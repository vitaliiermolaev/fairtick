// Headless load/soak bot runner.
//
// Spawns N WebSocket bots that go through the full handshake (Hello → Welcome →
// SignInWithApple → SetNickname → JoinGame) and then drive a REALISTIC client
// profile so a soak actually stresses what production will:
//   * MoveCommands at 4 Hz (random direction).
//   * Ping at 4 Hz — DECOUPLED from moves (a real client pings ~every 250ms;
//     the old runner piggybacked one ping per 4 moves ≈ 1 Hz, 4× too slow).
//   * ClientLogBatch every ~2s — exercises the telemetry writer + disk path
//     (the old runner sent none, so the soak never touched it).
//   * Match cycling: on GameEnded (the room hits game_duration_sec) the bot
//     ReturnToMenu + JoinGame again, so load continues past the FIRST match
//     instead of every bot idling in a dead room.
//   * Reconnect/resume churn: a `--churn-frac` fraction of bots periodically
//     drop the socket and reconnect with Resume{token} (falling back to a fresh
//     JoinGame on ResumeRejected) — exercising disconnect_hold, the resume
//     registry, and the grace sweep under load.
//
// Each bot records its own metrics; the runner writes a per-bot list plus an
// aggregate go/no-go summary to `--out/bot_run_<id>.json`.
//
// Usage:
//   cargo run --release --bin bot_runner -- --count 100 --duration 300 \
//       --server ws://127.0.0.1:9100/ws --out ./metrics --churn-frac 0.3
//
// `--strict` turns the run into a GO/NO-GO GATE (exit 1) instead of an exploratory soak:
// errors, never_joined, or UNPLANNED disconnects fail the run. Use it for the beta-gate
// soak on a quiet box with no planned restarts; leave it off for chaos/overload runs where
// disconnect-recovery is the thing being exercised. resumes_rejected stays report-only in
// both modes (a churn drop whose room ended during the absence is expected).

use fairtick::protocol::{
    ClientLogBatch, ClientLogEntry, ClientMessage, Direction, MoveCommand, ServerMessage,
};
use futures_util::{SinkExt, StreamExt};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default, Serialize)]
struct BotMetrics {
    bot_id: u64,
    nickname: String,
    snapshots_received: u64,
    events_received: u64,
    pongs_received: u64,
    move_commands_sent: u64,
    pings_sent: u64,
    log_batches_sent: u64,
    matches_completed: u64,
    reconnects: u64,
    /// UNPLANNED connection losses (server close / stream end / WS error) the bot
    /// recovered from by reconnecting. Separate from `reconnects` (planned churn)
    /// and from `errors` (server-sent Error messages).
    disconnects: u64,
    resumes_rejected: u64,
    errors: u64,
    welcomed: bool,
    /// Currently in a match (cleared on GameEnded and on every reconnect).
    joined: bool,
    /// Joined a match at least once this run — never cleared. The summary's
    /// `never_joined` keys off THIS, not `joined`, so a healthy bot that exits
    /// between matches isn't miscounted as never having joined.
    ever_joined: bool,
    duration_sec: u64,
    last_rtt_ms: u64,
    max_rtt_ms: u64,
    server_tick_seen: u64,
}

/// Mutable per-CONNECTION session state, reset on each (re)connect. Carries the
/// resume token across reconnects; the auth handler Resumes when a token is
/// present and fresh-JoinGames otherwise.
struct Session {
    resume_token: Option<String>,
    session_id: String,
    /// Monotonic ClientLogEntry seq across flushes — mirrors the Unity logger so
    /// downstream gap/reorder analysis sees realistic sequences, not 0,1,2 forever.
    log_seq: u64,
    /// Client log generation, bumped on each GameJoined (Unity bumps per join) so
    /// per-match correlation sees realistic gen values, not a constant 0.
    log_gen: u32,
    /// `--client-ready`: walk the real client's readiness handshake so fillers can
    /// legally eat the bots (filler→human eats require claim_ready). Off = the
    /// SAFETY profile (never claim_ready; every filler→bot eat must be rejected).
    client_ready_mode: bool,
    /// Readiness progress per admission: 0 idle/off, 1 = GameJoined seen (awaiting
    /// the first snapshot), 2 = ClientWorldReady sent (awaiting a pong), 3 = done.
    /// Reset on every GameJoined — per-admission, exactly like the server's state.
    ready_stage: u8,
}

/// Why one connection's drive loop ended.
enum SessionEnd {
    /// The bot's overall run deadline was reached — stop for good.
    Deadline,
    /// Planned churn drop — reconnect and Resume.
    ChurnDrop,
    /// Unplanned loss (server close / stream end / WS error) — reconnect so a
    /// transient server restart doesn't silently remove this bot's load for the
    /// rest of the soak.
    ConnectionLost,
}

/// Serialize a typed protocol message and send it; `false` = the send FAILED (socket
/// gone). Using `ClientMessage` (the same types the server parses) instead of hand-rolled
/// `json!` literals means a protocol change breaks the soak at COMPILE time instead of
/// silently desyncing what the bots send from what production clients send. The cadence
/// loop treats a failed send as ConnectionLost immediately, so the bot doesn't keep
/// "sending" into a dead socket until the read side notices (cleaner metrics).
async fn send_msg(ws: &mut Ws, msg: &ClientMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => ws.send(Message::Text(json)).await.is_ok(),
        Err(_) => false, // unreachable for our own wire types
    }
}

fn file_sha256(path: &str) -> String {
    let bytes = std::fs::read(path).expect("read");
    let mut h = Sha256::new();
    h.update(&bytes);
    format!("{:x}", h.finalize())
}

fn now_ms() -> u64 {
    // allow-wall-clock: telemetry / ping pairing only
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // wss:// support: rustls can't auto-pick a CryptoProvider (the dep tree enables more
    // than one provider feature), so install ring explicitly before any TLS connect.
    rustls::crypto::ring::default_provider().install_default().expect("install rustls ring CryptoProvider");

    let mut count: usize = 3;
    let mut duration_sec: u64 = 20;
    let mut server = "ws://127.0.0.1:8080/ws".to_string();
    let mut out = "./metrics".to_string();
    let mut seed: u64 = 0xBEEF_CAFE;
    // Fraction of bots that do reconnect/resume churn, and the mean seconds a
    // churn bot stays connected before dropping + resuming.
    let mut churn_frac: f64 = 0.3;
    let mut churn_sec: u64 = 30;
    // Gate mode: any error / never-joined bot / unplanned disconnect exits non-zero.
    let mut strict = false;
    // Realistic-client mode: bots walk the ClientWorldReady/ClientClaimReady handshake,
    // so fillers can legally eat them (the REALISTIC profile). Off = SAFETY profile.
    let mut client_ready = false;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--count" => {
                count = args[i + 1].parse().expect("bad count");
                i += 2;
            }
            "--duration" => {
                duration_sec = args[i + 1].parse().expect("bad duration");
                i += 2;
            }
            "--server" => {
                server = args[i + 1].clone();
                i += 2;
            }
            "--out" => {
                out = args[i + 1].clone();
                i += 2;
            }
            "--seed" => {
                seed = args[i + 1].parse().expect("bad seed");
                i += 2;
            }
            "--churn-frac" => {
                churn_frac = args[i + 1].parse().expect("bad churn-frac");
                i += 2;
            }
            "--churn-sec" => {
                churn_sec = args[i + 1].parse().expect("bad churn-sec");
                i += 2;
            }
            "--strict" => {
                strict = true;
                i += 1;
            }
            "--client-ready" => {
                client_ready = true;
                i += 1;
            }
            _ => {
                eprintln!("unknown arg: {}", args[i]);
                std::process::exit(2);
            }
        }
    }

    let config_hash = file_sha256("gameplay_config.toml");
    let maze_hash = file_sha256("maze.json");
    // Read the protocol_version from the SAME config the server loads, not a hardcoded literal —
    // a stale literal here just gets the bot rejected at the Hello handshake. (config SOT)
    // Fail FAST if the config can't be read/parsed: a load-test tool that can't read its own
    // config has no recoverable path (silently defaulting would just get every bot rejected).
    let protocol_version =
        fairtick::config_shared::load_config_from_str(&std::fs::read_to_string("gameplay_config.toml")?)?
            .network
            .protocol_version;
    let run_id = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    std::fs::create_dir_all(&out)?;
    // First `churn_count` bots churn; the rest hold a single session for the whole run.
    let churn_count = ((count as f64) * churn_frac).round() as usize;
    println!(
        "bot_runner count={} duration={}s server={} seed={} churn={}/{} churn_sec={} run_id={} out={} client_ready={}",
        count, duration_sec, server, seed, churn_count, count, churn_sec, run_id, out, client_ready
    );

    let mut handles = Vec::new();
    for i in 0..count {
        let server_url = server.clone();
        let cfg_hash = config_hash.clone();
        let m_hash = maze_hash.clone();
        let bot_seed = seed.wrapping_add(i as u64);
        let churn = if i < churn_count { Some(churn_sec) } else { None };
        let handle = tokio::spawn(async move {
            run_bot(
                i as u64,
                server_url,
                protocol_version,
                cfg_hash,
                m_hash,
                bot_seed,
                duration_sec,
                churn,
                client_ready,
            )
            .await
        });
        handles.push(handle);
    }

    let mut results: Vec<BotMetrics> = Vec::new();
    for h in handles {
        match h.await {
            Ok(m) => results.push(m),
            Err(e) => eprintln!("bot join error: {}", e),
        }
    }

    // Aggregate go/no-go view: an honest soak wants every bot welcomed+joined,
    // zero hard errors, and snapshots flowing. resumes_rejected is EXPECTED (a
    // drop whose room ended before reconnect), so it's reported, not failed-on.
    let total_snapshots: u64 = results.iter().map(|b| b.snapshots_received).sum();
    let total_moves: u64 = results.iter().map(|b| b.move_commands_sent).sum();
    let total_errors: u64 = results.iter().map(|b| b.errors).sum();
    let total_reconnects: u64 = results.iter().map(|b| b.reconnects).sum();
    let total_disconnects: u64 = results.iter().map(|b| b.disconnects).sum();
    let total_resumes_rejected: u64 = results.iter().map(|b| b.resumes_rejected).sum();
    let total_matches: u64 = results.iter().map(|b| b.matches_completed).sum();
    // ever_joined, not joined: `joined` is false between matches (GameEnded →
    // next GameJoined), so a bot whose deadline lands in that window would be
    // falsely counted as never having joined.
    let never_joined = results.iter().filter(|b| !b.ever_joined).count();
    let max_rtt: u64 = results.iter().map(|b| b.max_rtt_ms).max().unwrap_or(0);
    let summary = serde_json::json!({
        "run_id": run_id,
        "config_hash": config_hash,
        "maze_hash": maze_hash,
        "server": server,
        "count": count,
        "duration_sec": duration_sec,
        "seed": seed,
        "churn_count": churn_count,
        "agg": {
            "total_snapshots": total_snapshots,
            "total_moves": total_moves,
            "total_errors": total_errors,
            "total_reconnects": total_reconnects,
            "total_disconnects": total_disconnects,
            "total_resumes_rejected": total_resumes_rejected,
            "total_matches_completed": total_matches,
            "never_joined": never_joined,
            "max_rtt_ms": max_rtt,
        },
        "bots": results,
    });
    let out_path = PathBuf::from(&out).join(format!("bot_run_{}.json", run_id));
    std::fs::write(&out_path, serde_json::to_string_pretty(&summary)?)?;
    println!(
        "wrote {} — snapshots={} moves={} matches={} reconnects={} disconnects={} resume_rejected={} errors={} never_joined={} max_rtt={}ms",
        out_path.display(), total_snapshots, total_moves, total_matches,
        total_reconnects, total_disconnects, total_resumes_rejected, total_errors, never_joined, max_rtt
    );
    if total_errors > 0 || never_joined > 0 {
        eprintln!(
            "SOAK WARN: errors={} never_joined={} (see {})",
            total_errors,
            never_joined,
            out_path.display()
        );
    }
    // GATE verdict (--strict): for a beta go/no-go run, recovery paths must not be part of
    // normal operation — any protocol error, never-joined bot, or UNPLANNED disconnect
    // fails the run with a non-zero exit (the WARN above only prints). Planned churn
    // (reconnects) and resumes_rejected stay report-only: churn is the configured profile.
    if strict && (total_errors > 0 || never_joined > 0 || total_disconnects > 0) {
        eprintln!(
            "SOAK FAIL (strict): errors={} never_joined={} disconnects={} (see {})",
            total_errors,
            never_joined,
            total_disconnects,
            out_path.display()
        );
        std::process::exit(1);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)] // flat CLI plumbing, one call site
async fn run_bot(
    id: u64,
    server: String,
    protocol_version: u32,
    config_hash: String,
    maze_hash: String,
    seed: u64,
    duration_sec: u64,
    churn_sec: Option<u64>,
    client_ready: bool,
) -> BotMetrics {
    let nick = format!("bot_{}_{}", id, &Uuid::new_v4().to_string()[..6]);
    // STABLE per-bot identity so a reconnect re-auths as the SAME user (resume
    // is keyed by token, but the user must match for the slot to be handed back).
    let apple_token = format!("test_token_botuser_{}_{}", seed, id);
    let mut metrics = BotMetrics { bot_id: id, nickname: nick.clone(), ..Default::default() };
    let mut rng = ChaCha8Rng::seed_from_u64(seed);

    let started = Instant::now(); // allow-wall-clock: bot duration metric
    let deadline = started + Duration::from_secs(duration_sec); // allow-wall-clock: bot lifetime
    let mut resume_token: Option<String> = None;

    // Outer session loop: reconnect (resuming) until the overall deadline. A
    // non-churn bot normally runs one session but also reconnects after an
    // unplanned drop; a churn bot additionally loops every churn_sec.
    while Instant::now() < deadline {
        let mut session = Session {
            resume_token: resume_token.clone(),
            session_id: Uuid::new_v4().to_string(),
            log_seq: 0, // fresh session_id → fresh seq/gen domain, like a client relaunch
            log_gen: 0,
            client_ready_mode: client_ready,
            ready_stage: 0,
        };
        // Per-CONNECTION gates: the fresh socket must redo Hello→Welcome and
        // Auth→(Resume|JoinGame) before the bot may send gameplay traffic.
        // Stale true values from the previous session would make the bot send
        // MoveCommand/Ping before the handshake completes — the server answers
        // Error("Not in a game"), polluting metrics.errors with false positives.
        // A successful Resume re-delivers GameJoined, so `joined` comes back.
        metrics.welcomed = false;
        metrics.joined = false;

        let mut ws = match tokio_tungstenite::connect_async(&server).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                eprintln!("bot {} connect error: {}", id, e);
                metrics.errors += 1;
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };

        // Handshake: Hello → (reactive) Welcome, then SignInWithApple. The
        // AuthFailed→SetNickname / AuthSuccess→(Resume|JoinGame) handshake is
        // driven reactively in handle_text. A failed handshake send = the socket
        // died right after connect — count it as an unplanned disconnect and
        // retry, same as a mid-session loss (consistent send_msg semantics).
        let hello_ok = send_msg(
            &mut ws,
            &ClientMessage::Hello {
                protocol_version,
                client_build: "bot".to_string(),
                platform: "bot".to_string(),
                config_hash: config_hash.clone(),
                maze_hash: maze_hash.clone(),
                connect_attempt_id: 0,
                client_session_id: String::new(),
                resume_intent: false,
            },
        )
        .await;
        let auth_ok = hello_ok
            && send_msg(
                &mut ws,
                &ClientMessage::SignInWithApple {
                    apple_token: apple_token.clone(),
                    nonce: "bot".to_string(),
                },
            )
            .await;
        if !auth_ok {
            let _ = ws.close(None).await;
            metrics.disconnects += 1;
            tokio::time::sleep(Duration::from_millis(rng.random_range(500..1000))).await;
            continue;
        }

        // Per-session churn deadline (jittered ±25% so 100 bots don't all drop
        // on the same tick). None = hold the session to the overall deadline.
        let session_deadline = churn_sec.map(|s| {
            let jitter = rng.random_range(0.75..1.25);
            Instant::now() + Duration::from_secs_f64(s as f64 * jitter)
        });

        let end =
            drive_session(&mut ws, &mut metrics, &mut session, &nick, &mut rng, deadline, session_deadline)
                .await;

        let _ = ws.close(None).await;
        // Carry the freshest resume token to the next reconnect.
        resume_token = session.resume_token.clone();

        match end {
            SessionEnd::Deadline => break,
            SessionEnd::ChurnDrop => {
                metrics.reconnects += 1;
                // Brief gap to look like a real network drop (and to let the server
                // record the disconnect_hold before we race back in).
                tokio::time::sleep(Duration::from_millis(rng.random_range(150..400))).await;
            }
            SessionEnd::ConnectionLost => {
                // Server close / stream end / WS error mid-soak (e.g. a server
                // restart). Reconnect-and-resume instead of exiting, so the soak
                // keeps its load and actually exercises recovery; the count is
                // surfaced in the summary.
                metrics.disconnects += 1;
                tokio::time::sleep(Duration::from_millis(rng.random_range(500..1000))).await;
            }
        }
    }

    metrics.duration_sec = started.elapsed().as_secs(); // allow-wall-clock: bot duration metric
    metrics
}

/// Drive one connection until the overall `deadline`, the `session_deadline`
/// churn point, or an unplanned connection loss (see [`SessionEnd`]). Sends
/// moves/pings/log-batches on cadence and drains inbound.
async fn drive_session(
    ws: &mut Ws,
    metrics: &mut BotMetrics,
    session: &mut Session,
    nickname: &str,
    rng: &mut ChaCha8Rng,
    deadline: Instant,
    session_deadline: Option<Instant>,
) -> SessionEnd {
    let move_interval = Duration::from_millis(250); // 4 Hz
    let ping_interval = Duration::from_millis(250); // 4 Hz, decoupled from moves
    let log_interval = Duration::from_millis(2000); // ~every 2s, like the real client
                                                    // First move/ping fire one interval in (avoids `Instant - Duration` underflow); a 250ms
                                                    // warm-up at session start is realistic anyway. allow-wall-clock: bot pacing.
    let mut last_move = Instant::now();
    let mut last_ping = Instant::now();
    let mut last_log = Instant::now();
    let mut next_seq: u32 = 1;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return SessionEnd::Deadline;
        }
        if let Some(sd) = session_deadline {
            if now >= sd && metrics.joined {
                return SessionEnd::ChurnDrop; // churn: drop and resume
            }
        }

        if metrics.welcomed && metrics.joined {
            if last_move.elapsed() >= move_interval {
                let direction = [Direction::Up, Direction::Down, Direction::Left, Direction::Right]
                    [rng.random_range(0..4)];
                let cmd = ClientMessage::MoveCommand(MoveCommand {
                    seq: next_seq,
                    target_tick: metrics.server_tick_seen + 2,
                    direction,
                });
                next_seq = next_seq.wrapping_add(1);
                if !send_msg(ws, &cmd).await {
                    return SessionEnd::ConnectionLost;
                }
                metrics.move_commands_sent += 1;
                last_move = now;
            }
            if last_ping.elapsed() >= ping_interval {
                if !send_msg(ws, &ClientMessage::Ping { client_send_time_ms: now_ms() }).await {
                    return SessionEnd::ConnectionLost;
                }
                metrics.pings_sent += 1;
                last_ping = now;
            }
            if last_log.elapsed() >= log_interval {
                let batch = make_log_batch(session, metrics);
                if !send_msg(ws, &batch).await {
                    return SessionEnd::ConnectionLost;
                }
                metrics.log_batches_sent += 1;
                last_log = now;
            }
        }

        // Drain incoming with a short timeout so we keep ticking outward.
        match tokio::time::timeout(Duration::from_millis(50), ws.next()).await {
            Ok(Some(Ok(Message::Text(s)))) => {
                metrics.events_received += handle_text(&s, metrics, session, nickname, ws).await;
            }
            // Protocol v6: snapshots arrive as bincode BINARY frames (same types, new
            // encoding) — decode them like the real client does, so snapshots_received
            // and server_tick_seen keep measuring the actual production wire.
            Ok(Some(Ok(Message::Binary(d)))) => {
                handle_binary(&d, metrics);
                // Readiness step 1 (--client-ready): the first snapshot after a join is
                // the bot's "world assembled" moment — declare it, like the real client.
                if session.ready_stage == 1 {
                    session.ready_stage = 2;
                    let msg = ClientMessage::ClientWorldReady {
                        world_sync_epoch: 1,
                        anchor_tick: metrics.server_tick_seen,
                    };
                    if !send_msg(ws, &msg).await {
                        return SessionEnd::ConnectionLost;
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) => {
                // Unplanned loss — the caller reconnects (counted in
                // `disconnects`, NOT `errors`: a transport drop during a soak —
                // e.g. a server restart — is a recovery scenario to exercise,
                // not a protocol error to fail the run on).
                return SessionEnd::ConnectionLost;
            }
            _ => {}
        }
    }
}

/// A small but well-formed ClientLogBatch (3 entries) — enough to exercise the
/// server's telemetry parse + NDJSON write + disk path under soak load. `seq` is
/// monotonic across flushes and `gen` tracks the current match (bumped on each
/// GameJoined), mirroring the Unity logger so downstream gap/reorder/correlation
/// analysis sees realistic values from bot traffic too.
fn make_log_batch(session: &mut Session, metrics: &BotMetrics) -> ClientMessage {
    let base_seq = session.log_seq;
    session.log_seq += 3;
    let entry = |seq: u64, name: &str| ClientLogEntry {
        seq: base_seq + seq,
        client_unix_ms: now_ms() as i64,
        client_mono_ms: 0,
        level: "info".to_string(),
        event_name: name.to_string(),
        message: "soak".to_string(),
        room_id: String::new(),
        player_id: String::new(),
        server_tick: metrics.server_tick_seen as i64,
        client_tick: 0,
        render_tick: 0.0,
        fields_json: "{\"soak\":true}".to_string(),
    };
    ClientMessage::ClientLogBatch(ClientLogBatch {
        session_id: session.session_id.clone(),
        flush_reason: "interval".to_string(),
        dropped: 0,
        gen: session.log_gen,
        entries: vec![
            entry(0, "client_diag_sample"),
            entry(1, "client_net_stats"),
            entry(2, "client_ws_recv"),
        ],
    })
}

/// Binary inbound = bincode snapshots (protocol v6). Anything else is unexpected and counted
/// as a protocol error — the soak must notice drift, not skip it.
fn handle_binary(data: &[u8], metrics: &mut BotMetrics) {
    match ServerMessage::deserialize(data) {
        Ok(ServerMessage::GameState(s)) => {
            metrics.snapshots_received += 1;
            metrics.server_tick_seen = s.tick;
        }
        Ok(ServerMessage::GameStateDelta(d)) => {
            metrics.snapshots_received += 1;
            metrics.server_tick_seen = d.tick;
        }
        Ok(_) | Err(_) => metrics.errors += 1,
    }
}

async fn handle_text(
    raw: &str,
    metrics: &mut BotMetrics,
    session: &mut Session,
    nickname: &str,
    ws: &mut Ws,
) -> u64 {
    let Ok(v) = serde_json::from_str::<Value>(raw) else { return 0 };
    if v.get("Welcome").is_some() {
        metrics.welcomed = true;
        return 0;
    }
    if v.get("AuthFailed").is_some() {
        // New user → set the bot's nickname (only happens on the very first
        // session; a reconnect re-auths an existing user → AuthSuccess directly).
        send_msg(ws, &ClientMessage::SetNickname { nickname: nickname.to_string() }).await;
        return 0;
    }
    if v.get("AuthSuccess").is_some() {
        // Resume into the SAME match if we're reconnecting with a token; else
        // fresh join. ResumeRejected falls back to JoinGame below.
        if let Some(tok) = &session.resume_token {
            send_msg(ws, &ClientMessage::Resume { resume_token: tok.clone() }).await;
        } else {
            send_msg(ws, &ClientMessage::JoinGame).await;
        }
        return 0;
    }
    if v.get("ResumeRejected").is_some() {
        // The dropped match is gone (its room ended during our absence) — fall
        // back to a fresh join, exactly like the real client.
        metrics.resumes_rejected += 1;
        session.resume_token = None;
        send_msg(ws, &ClientMessage::JoinGame).await;
        return 0;
    }
    if let Some(gj) = v.get("GameJoined") {
        metrics.joined = true;
        metrics.ever_joined = true;
        session.log_gen += 1; // new match → new client-log generation (Unity logger semantics)
        if let Some(t) = gj.get("server_tick").and_then(|x| x.as_u64()) {
            metrics.server_tick_seen = t;
        }
        // Stash THIS match's resume token so a churn drop can resume into it.
        if let Some(tok) = gj.get("resume_token").and_then(|x| x.as_str()) {
            session.resume_token = Some(tok.to_string());
        }
        // Readiness is PER-ADMISSION on the server; restart the walk on every join/resume.
        session.ready_stage = if session.client_ready_mode { 1 } else { 0 };
        return 0;
    }
    if v.get("GameEnded").is_some() {
        // The room hit game_duration_sec. Cycle into a fresh match so the soak
        // keeps load up instead of idling in a dead room (ReturnToMenu frees the
        // slot, then JoinGame). The old match's resume token is now stale.
        metrics.matches_completed += 1;
        metrics.joined = false;
        session.resume_token = None;
        send_msg(ws, &ClientMessage::ReturnToMenu).await;
        send_msg(ws, &ClientMessage::JoinGame).await;
        return 0;
    }
    if let Some(p) = v.get("Pong") {
        metrics.pongs_received += 1;
        if let Some(c) = p.get("client_send_time_ms").and_then(|x| x.as_u64()) {
            let rtt = now_ms().saturating_sub(c);
            metrics.last_rtt_ms = rtt;
            metrics.max_rtt_ms = metrics.max_rtt_ms.max(rtt);
        }
        // Readiness step 2: world declared + a clean pong proves the clock — the same
        // order the real client earns claim trust in.
        if session.ready_stage == 2 {
            session.ready_stage = 3;
            send_msg(
                ws,
                &ClientMessage::ClientClaimReady {
                    world_sync_epoch: 1,
                    anchor_tick: metrics.server_tick_seen,
                },
            )
            .await;
        }
        return 0;
    }
    if let Some(d) = v.get("GameStateDelta").or_else(|| v.get("GameState")) {
        metrics.snapshots_received += 1;
        if let Some(t) = d.get("tick").and_then(|x| x.as_u64()) {
            metrics.server_tick_seen = t;
        }
        return 0;
    }
    if v.get("Error").is_some() {
        metrics.errors += 1;
        return 0;
    }
    if v.get("Event").is_some() {
        return 1; // counted toward metrics.events_received
    }
    0
}
