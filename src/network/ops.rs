//! Ops dashboard — `/ops/statusz` (JSON) + `/ops` (auto-refreshing HTML).
//!
//! Observability ONLY (same contract as `health`): this module reads counters and
//! population snapshots and renders them; it makes no gameplay or admission decision.
//! Exposed to the internet exclusively through Caddy's `/ops/*` basic_auth block —
//! the backend itself does no auth, the edge does (one place, one credential).
//!
//! Deliberately NO nicknames / user ids / tokens in the payload: an ops page must
//! not become a roster or credential leak if the password ever escapes.

use crate::game::room_manager::{RoomOverview, TickPerfSnapshot};
use serde::Serialize;

/// The `/ops/statusz` body. Everything the "is the server okay / who's online"
/// glance needs, assembled by the handler from lock-free gauges + read locks.
#[derive(Serialize)]
pub struct StatusSnapshot {
    pub build: String,
    pub uptime_sec: u64,
    pub conns: ConnStatus,
    pub players: PlayerTotals,
    pub rooms_count: usize,
    pub rooms: Vec<RoomOverview>,
    pub resume_holds: usize,
    pub tick: TickStatus,
    pub matchmaking_enabled: bool,
}

#[derive(Serialize)]
pub struct ConnStatus {
    pub active: usize,
    pub max: usize,
}

#[derive(Serialize)]
pub struct PlayerTotals {
    /// Real connected players (the number that means "online").
    pub humans: usize,
    /// Server-driven fillers currently riding rooms (kept SEPARATE — folding them
    /// into "online" would make the dashboard lie about traction).
    pub fillers: usize,
}

#[derive(Serialize)]
pub struct TickStatus {
    #[serde(flatten)]
    pub perf: TickPerfSnapshot,
    /// ms since the last completed tick (readiness-style staleness signal).
    pub heartbeat_age_ms: u64,
}

/// Raw inputs the handler gathers before assembly (one struct, not nine args).
pub struct StatusInputs {
    pub build: String,
    pub uptime_sec: u64,
    pub active_conns: usize,
    pub max_conns: usize,
    pub rooms: Vec<RoomOverview>,
    pub resume_holds: usize,
    pub perf: TickPerfSnapshot,
    pub heartbeat_age_ms: u64,
    pub matchmaking_enabled: bool,
}

/// Assemble the status body from already-gathered inputs. Pure (no I/O, no locks)
/// so the shape is unit-testable without axum or a live RoomManager.
pub fn build_status(i: StatusInputs) -> StatusSnapshot {
    let humans = i.rooms.iter().map(|r| r.humans).sum();
    let fillers = i.rooms.iter().map(|r| r.fillers).sum();
    StatusSnapshot {
        build: i.build,
        uptime_sec: i.uptime_sec,
        conns: ConnStatus { active: i.active_conns, max: i.max_conns },
        players: PlayerTotals { humans, fillers },
        rooms_count: i.rooms.len(),
        rooms: i.rooms,
        resume_holds: i.resume_holds,
        tick: TickStatus { perf: i.perf, heartbeat_age_ms: i.heartbeat_age_ms },
        matchmaking_enabled: i.matchmaking_enabled,
    }
}

/// The dashboard page: a single static HTML that polls `/ops/statusz` every 3 s.
/// Served by the backend so deploys can't desync page ↔ payload.
pub const DASHBOARD_HTML: &str = include_str!("ops_dashboard.html");

#[cfg(test)]
mod tests {
    use super::*;

    fn room(humans: usize, fillers: usize) -> RoomOverview {
        RoomOverview {
            id_short: "abcd1234".into(),
            humans,
            fillers,
            reserved: 0,
            total: humans + fillers,
            tick: 100,
            remaining_sec: 30,
            is_active: true,
            joinable: true,
        }
    }

    #[test]
    fn totals_split_humans_from_fillers_and_payload_has_no_identity_fields() {
        let s = build_status(StatusInputs {
            build: "abc123".into(),
            uptime_sec: 60,
            active_conns: 2,
            max_conns: 150,
            rooms: vec![room(2, 6), room(0, 8)],
            resume_holds: 1,
            perf: TickPerfSnapshot::default(),
            heartbeat_age_ms: 17,
            matchmaking_enabled: true,
        });
        assert_eq!(s.players.humans, 2, "online = HUMANS only");
        assert_eq!(s.players.fillers, 14, "fillers counted separately, never folded in");
        assert_eq!(s.rooms_count, 2);
        // The wire payload must stay identity-free: no nickname/user/token keys anywhere.
        let json = serde_json::to_string(&s).unwrap();
        for needle in ["nickname", "user_id", "token"] {
            assert!(!json.contains(needle), "statusz must not leak '{needle}'");
        }
    }
}
