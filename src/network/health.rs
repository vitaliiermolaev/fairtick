//! HTTP liveness/readiness probes.
//!
//! Transport/diagnostics only — these OBSERVE server health, they make no
//! gameplay decision. Kept in their own module so the WebSocket handler doesn't
//! accumulate unrelated responsibilities.
//!
//!  * `/healthz` (liveness)  — is the PROCESS up and the HTTP server answering?
//!    Checks nothing else BY DESIGN: a failing liveness probe means "restart
//!    me", which is the wrong remedy for a transient DB blip or an idle loop.
//!  * `/readyz`  (readiness) — should this instance accept PLAYERS right now?
//!    Fails (503) if the DB is unreachable OR the simulation tick loop has
//!    stalled — so a deploy that is process-alive but can't actually run a
//!    match is pulled from rotation instead of silently swallowing joins.
//!
//! The decision (`evaluate`) is a pure function so it is unit-testable without
//! axum or I/O; the handler gathers the observed inputs and passes them in.

use axum::http::StatusCode;
use serde::Serialize;
use sqlx::SqlitePool;
use std::time::Duration;

/// Liveness body. A `&'static str` so the handler allocates nothing.
pub const LIVENESS_OK: &str = "ok";

/// A tick loop that hasn't advanced in this many ticks is STALLED (hung room
/// update, deadlocked lock, saturated runtime) — NOT merely idle, because an
/// empty server still ticks ~tick_rate×/s. ≈2s at 60Hz. Set well above any
/// plausible GC/scheduling hiccup so readiness only flips on a real stall.
const TICK_STALL_GRACE_TICKS: u64 = 120;

/// Bound the DB probe so a wedged pool can't hang the readiness handler itself
/// (acquiring a connection would otherwise wait indefinitely).
const DB_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Max heartbeat age (ms) before the tick loop counts as stalled, derived from
/// the configured tick rate so it tracks config instead of a hardcoded magic
/// number. `tick_rate.max(1)` guards a bogus 0 config against div-by-zero.
pub fn stall_threshold_ms(tick_rate: u64) -> u64 {
    (TICK_STALL_GRACE_TICKS * 1000) / tick_rate.max(1)
}

/// Prove the pool can hand out a working connection with a trivial `SELECT 1`,
/// bounded by `DB_PROBE_TIMEOUT`. The returned error/timeout text is intended for
/// SERVER-SIDE logging; the public `/readyz` handler sanitizes it to a generic
/// "unavailable" so the raw driver error (schema/paths) isn't exposed.
pub async fn probe_db(pool: &SqlitePool) -> Result<(), String> {
    match tokio::time::timeout(DB_PROBE_TIMEOUT, sqlx::query("SELECT 1").execute(pool)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("timeout after {}ms", DB_PROBE_TIMEOUT.as_millis())),
    }
}

/// Per-dependency verdicts, serialized into the `/readyz` body.
#[derive(Serialize)]
pub struct Checks {
    /// `"ok"` or `"error: <detail>"`.
    pub db: String,
    /// `"ok"` or `"stalled: <detail>"`.
    pub tick_loop: String,
}

/// `/readyz` response body + overall verdict.
#[derive(Serialize)]
pub struct Readiness {
    pub ready: bool,
    pub checks: Checks,
    pub tick_loop_age_ms: u64,
    pub rooms: usize,
    /// Live WebSocket connections (lock-free gauge ≈ players online). Observability
    /// only — it does NOT gate readiness; surfaced so ops/soak monitoring can watch
    /// load + degradation without a separate endpoint. (beta monitoring)
    pub active_conns: usize,
}

impl Readiness {
    /// 200 when ready, 503 otherwise — the standard contract a load balancer /
    /// orchestrator readiness probe expects.
    pub fn status_code(&self) -> StatusCode {
        if self.ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// Pure readiness decision: given the observed inputs, decide whether to accept
/// players. No I/O — the handler does the I/O (DB probe, heartbeat read) and
/// passes the results here so this stays unit-testable in isolation.
pub fn evaluate(
    db: Result<(), String>,
    tick_loop_age_ms: u64,
    tick_rate: u64,
    rooms: usize,
    active_conns: usize,
) -> Readiness {
    let threshold = stall_threshold_ms(tick_rate);
    let tick_ok = tick_loop_age_ms <= threshold;
    let db_ok = db.is_ok();
    Readiness {
        ready: db_ok && tick_ok,
        checks: Checks {
            db: match db {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("error: {e}"),
            },
            tick_loop: if tick_ok {
                "ok".to_string()
            } else {
                format!("stalled: no tick in {tick_loop_age_ms}ms (> {threshold}ms)")
            },
        },
        tick_loop_age_ms,
        rooms,
        active_conns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_threshold_is_two_seconds_at_60hz() {
        assert_eq!(stall_threshold_ms(60), 2000);
    }

    #[test]
    fn zero_tick_rate_does_not_divide_by_zero() {
        // Defensive: a bogus 0 config must not panic the readiness probe.
        assert_eq!(stall_threshold_ms(0), 120_000);
    }

    #[test]
    fn ready_when_db_ok_and_tick_fresh() {
        let r = evaluate(Ok(()), 16, 60, 3, 7);
        assert!(r.ready);
        assert_eq!(r.status_code(), StatusCode::OK);
        assert_eq!(r.checks.db, "ok");
        assert_eq!(r.checks.tick_loop, "ok");
        assert_eq!(r.rooms, 3);
        // active_conns is observability only — surfaced, never gates readiness.
        assert_eq!(r.active_conns, 7);
    }

    #[test]
    fn not_ready_when_db_unreachable() {
        let r = evaluate(Err("no such table".into()), 16, 60, 0, 0);
        assert!(!r.ready);
        assert_eq!(r.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(r.checks.db.contains("error"));
        // Only the DB failed; the tick loop check is still reported healthy.
        assert_eq!(r.checks.tick_loop, "ok");
    }

    #[test]
    fn not_ready_when_tick_loop_stalled() {
        // Heartbeat age well beyond the 2s threshold at 60Hz.
        let r = evaluate(Ok(()), 5_000, 60, 1, 0);
        assert!(!r.ready);
        assert_eq!(r.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(r.checks.db, "ok");
        assert!(r.checks.tick_loop.contains("stalled"));
    }

    #[test]
    fn tick_age_exactly_at_threshold_is_still_ok() {
        // Tick-boundary edge case: age == threshold is healthy (`<=`), age one
        // ms past it is not.
        let t = stall_threshold_ms(60);
        assert!(evaluate(Ok(()), t, 60, 0, 0).ready);
        assert!(!evaluate(Ok(()), t + 1, 60, 0, 0).ready);
    }

    #[tokio::test]
    async fn probe_db_ok_on_live_pool() {
        let pool = crate::db::init_db("sqlite::memory:").await.unwrap();
        assert!(probe_db(&pool).await.is_ok());
    }

    #[tokio::test]
    async fn probe_db_errors_on_closed_pool() {
        let pool = crate::db::init_db("sqlite::memory:").await.unwrap();
        pool.close().await;
        assert!(probe_db(&pool).await.is_err());
    }
}
