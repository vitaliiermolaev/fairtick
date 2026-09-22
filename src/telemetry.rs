//! Match telemetry: one NDJSON file per server run holding a single, unified
//! event stream from BOTH the server and the connected clients.
//!
//! Design:
//!  - One file per process run: `logs/fairtick-run-<run_id>.ndjson`.
//!  - One JSON object per line (NDJSON) — grep/jq friendly, and a crash mid-write
//!    only ever loses the last partial line.
//!  - The SERVER stamps `server_ts` at write time. Client events carry their own
//!    `client_seq` / `client_mono_ms`; we never trust the phone's wall clock for
//!    ordering — intra-client order is `client_seq`, global order is `server_ts`.
//!  - `emit` is a non-blocking `try_send` into a bounded channel: telemetry MUST
//!    never block or slow the game loop. If the channel is full we DROP the event
//!    (same fail-fast policy as the per-player reliable/snapshot channels).
//!
//! A single writer task owns the file and drains the channel, so `Telemetry` is
//! cheap to `clone()` and share across the ws handlers and every Room.

// allow-wall-clock: telemetry is a logging/diagnostics sink — `server_ts` is
// deliberately wall-clock so a human can line events up against client logs.
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::fs::{create_dir_all, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Bounded event queue. Sized large so a burst (a flushed client batch + a wave
/// of server events on the same tick) doesn't drop, but still capped so a stuck
/// writer can't grow memory without bound.
const TELEMETRY_CHANNEL_CAPACITY: usize = 8192;

#[derive(Clone, Debug)]
pub struct Telemetry {
    tx: mpsc::Sender<Value>,
    run_id: Arc<str>,
    /// `FAIRTICK_TELEMETRY_VERBOSE=1` — enables the high-rate firehose events
    /// (server_snapshot_sent, input_applied: up to one per tick per player). OFF
    /// by default so normal runs stay readable and don't self-induce lag.
    verbose: bool,
}

impl Telemetry {
    /// Open the first telemetry segment and spawn the writer task. Returns a
    /// cheap-to-clone handle. Emits `server_started` immediately.
    ///
    /// Size-based ROTATION: a long-running beta server must not fill the disk with one
    /// unbounded NDJSON file (a 100-player run writes ~10MB/min). Each segment is capped at
    /// `FAIRTICK_LOG_MAX_MB` (default 64); on rollover the `FAIRTICK_LOG_RETAIN` (default 12)
    /// NEWEST `fairtick-run_*.ndjson` files are kept and older ones deleted — so total
    /// telemetry disk is bounded by retain × max_mb regardless of run length or restart count.
    /// `FAIRTICK_LOG_DIR` overrides the directory (default `logs`).
    pub async fn new() -> std::io::Result<Self> {
        let run_id: Arc<str> = Utc::now().format("%d-%m-%Y_%H-%M-%S").to_string().into();
        let dir = std::env::var("FAIRTICK_LOG_DIR").unwrap_or_else(|_| "logs".to_string());
        create_dir_all(&dir).await?;
        let max_bytes = env_u64("FAIRTICK_LOG_MAX_MB", 64).saturating_mul(1024 * 1024);
        let retain = env_u64("FAIRTICK_LOG_RETAIN", 12).max(1) as usize;

        let mut seq: u64 = 0;
        let path = segment_path(&dir, &run_id, seq);
        let file = OpenOptions::new().create(true).append(true).open(&path).await?;
        info!(
            "📒 Telemetry writing to {} (rotate at {}MB, retain {})",
            path,
            max_bytes / (1024 * 1024),
            retain
        );

        let (tx, mut rx) = mpsc::channel::<Value>(TELEMETRY_CHANNEL_CAPACITY);
        let task_run_id = Arc::clone(&run_id);
        tokio::spawn(async move {
            let mut writer = BufWriter::new(file);
            let mut bytes: u64 = 0;
            // Write each event, then drain anything already queued WITHOUT waiting,
            // and flush once the queue is momentarily empty. Under low load that's a
            // flush after (nearly) every event, so `tail -f` and post-crash reads see
            // the file current; under a burst it batches the writes and flushes once.
            'outer: while let Some(first) = rx.recv().await {
                let mut event = Some(first);
                while let Some(mut e) = event.take() {
                    // Stamp the write time. For SERVER events this IS the event time
                    // (`server_ts`). For CLIENT events the real event time is the
                    // client's own `client_event_unix_ms`/`client_mono_ms`; the
                    // server-side stamp is only when the batch was INGESTED, so name
                    // it `ingest_ts` to avoid sorting client events by it.
                    let ts = json!(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true));
                    let is_client = e.get("source").and_then(|v| v.as_str()) == Some("client");
                    e[if is_client { "ingest_ts" } else { "server_ts" }] = ts;
                    if let Ok(line) = serde_json::to_string(&e) {
                        if writer.write_all(line.as_bytes()).await.is_err()
                            || writer.write_all(b"\n").await.is_err()
                        {
                            warn!("telemetry write failed — stopping writer");
                            break 'outer;
                        }
                        bytes += line.len() as u64 + 1;
                    }
                    event = rx.try_recv().ok();
                }
                let _ = writer.flush().await;
                // Rotate on a whole-line boundary (after the flush). On any IO error keep
                // writing to the current segment — telemetry must never take the server down.
                if bytes >= max_bytes {
                    match rotate(&dir, &task_run_id, &mut seq, retain).await {
                        Ok(next) => {
                            writer = BufWriter::new(next);
                            bytes = 0;
                        }
                        Err(e) => warn!("telemetry rotate failed, staying on current segment: {e}"),
                    }
                }
            }
            let _ = writer.flush().await;
        });

        let verbose = std::env::var("FAIRTICK_TELEMETRY_VERBOSE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let telemetry = Self { tx, run_id, verbose };
        telemetry.server_info("server_started", json!({ "pid": std::process::id(), "verbose": verbose }));
        Ok(telemetry)
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Whether the high-rate firehose events should be emitted.
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }

    /// Non-blocking. Drops the event if the queue is full or the writer is gone —
    /// telemetry never back-pressures the caller.
    pub fn emit(&self, event: Value) {
        let _ = self.tx.try_send(event);
    }

    /// A server-sourced event. `fields` is the event-specific payload object.
    pub fn server_info(&self, event: &str, fields: Value) {
        self.emit_server("info", event, fields);
    }

    pub fn server_warn(&self, event: &str, fields: Value) {
        self.emit_server("warn", event, fields);
    }

    fn emit_server(&self, level: &str, event: &str, fields: Value) {
        self.emit(json!({
            "source": "server",
            "level": level,
            "event": event,
            "run_id": &*self.run_id,
            "fields": fields,
        }));
    }
}

/// Parse a u64 env var, falling back to `default` on missing/garbage.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Telemetry file name for one rotation segment. Keeps the `fairtick-run_*.ndjson`
/// prefix so existing tooling (bot_runner/monitor globs, jq pipelines) still matches.
fn segment_path(dir: &str, run_id: &str, seq: u64) -> String {
    format!("{dir}/fairtick-run_{run_id}_{seq:03}.ndjson")
}

/// Open the NEXT segment and prune old telemetry files to the retention cap.
async fn rotate(dir: &str, run_id: &str, seq: &mut u64, retain: usize) -> std::io::Result<tokio::fs::File> {
    *seq += 1;
    let path = segment_path(dir, run_id, *seq);
    let file = OpenOptions::new().create(true).append(true).open(&path).await?;
    info!("📒 Telemetry rotated to {}", path);
    if let Err(e) = prune_old_logs(dir, retain).await {
        warn!("telemetry prune failed: {e}");
    }
    Ok(file)
}

/// Keep the `retain` newest `fairtick-run_*.ndjson` files (by mtime); delete the rest.
/// Bounds total telemetry disk across runs/restarts. Prunes a GLOBAL glob — correct for the
/// normal one-server-per-logs-dir deploy (container volume); a second server sharing the SAME
/// dir locally could see ITS older files pruned too, which is acceptable for dev. Non-telemetry
/// files (anything not matching the prefix) are never touched.
async fn prune_old_logs(dir: &str, retain: usize) -> std::io::Result<()> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    while let Some(ent) = rd.next_entry().await? {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("fairtick-run_") && name.ends_with(".ndjson") {
            if let Ok(meta) = ent.metadata().await {
                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                files.push((mtime, ent.path()));
            }
        }
    }
    if files.len() <= retain {
        return Ok(());
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0)); // newest first
    for (_, path) in files.into_iter().skip(retain) {
        let _ = tokio::fs::remove_file(&path).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prune_caps_telemetry_files_and_spares_others() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        // 7 telemetry segments + one unrelated file.
        for i in 0..7u64 {
            tokio::fs::write(segment_path(d, "run", i), b"x\n").await.unwrap();
        }
        tokio::fs::write(format!("{d}/unrelated.txt"), b"keep").await.unwrap();

        prune_old_logs(d, 3).await.unwrap();

        let mut telem = 0;
        let mut other_kept = false;
        let mut rd = tokio::fs::read_dir(d).await.unwrap();
        while let Some(e) = rd.next_entry().await.unwrap() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("fairtick-run_") && name.ends_with(".ndjson") {
                telem += 1;
            }
            if name == "unrelated.txt" {
                other_kept = true;
            }
        }
        assert_eq!(telem, 3, "retention keeps exactly the cap");
        assert!(other_kept, "non-telemetry files are never pruned");
    }

    #[tokio::test]
    async fn prune_noop_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        for i in 0..2u64 {
            tokio::fs::write(segment_path(d, "run", i), b"x\n").await.unwrap();
        }
        prune_old_logs(d, 12).await.unwrap();
        let mut n = 0;
        let mut rd = tokio::fs::read_dir(d).await.unwrap();
        while let Some(_e) = rd.next_entry().await.unwrap() {
            n += 1;
        }
        assert_eq!(n, 2, "nothing pruned when under the cap");
    }
}
