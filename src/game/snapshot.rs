//! Snapshot projection — the pure, read-only mapping of authoritative Room state into the network
//! keyframe (`GameStateUpdate`) and movement delta (`GameStateDelta`) DTOs.
//!
//! Extracted from the `Room` shell (review #1 — keep the god-struct from growing). Projection holds
//! NO state, makes NO gameplay decision, and does NO I/O: it only reads a borrowed view of the world
//! and builds the wire shape. The Room owns the world and the decisions; this owns the DTO mapping
//! (CLAUDE.md rule 8 "snapshots are projections, not gameplay" / rule 19).

use crate::game::ai::Enemy;
use crate::game::player::Player;
use crate::protocol::{
    Booster, EnemyPositionUpdate, GameStateDelta, GameStateUpdate, PlayerPositionUpdate, PointItem,
    PortalState,
};
use std::collections::HashMap;

/// A borrowed, read-only view of exactly the Room state a snapshot needs. The Room (which alone can
/// read its private fields) constructs this and hands it here, so the projection logic lives outside
/// the god-struct without exposing Room's internals.
pub(crate) struct SnapshotProjector<'a> {
    pub players: &'a HashMap<String, Player>,
    pub enemies: &'a [Enemy],
    pub boosters: &'a [Booster],
    pub points: &'a [PointItem],
    pub portal: Option<PortalState>,
    pub tick: u64,
    pub tick_rate: u64,
    pub time_remaining: u64,
    pub last_event_id: Option<u64>,
}

impl SnapshotProjector<'_> {
    /// Players in a STABLE by-id order. HashMap iteration order isn't deterministic, so the wire
    /// arrays (and any golden/replay diff) would otherwise be noisy run-to-run. Order is cosmetic to
    /// the client (it keys entities by id), but determinism matters for fixtures/replay. (review #8)
    fn players_by_id(players: &HashMap<String, Player>) -> impl Iterator<Item = &Player> {
        let mut v: Vec<&Player> = players.values().collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v.into_iter()
    }

    /// Full keyframe (everything). `snapshot_seq` is 0 for an out-of-band keyframe (join /
    /// RequestFullState) and the per-tick sequence otherwise.
    pub fn full(
        &self,
        last_processed_input_seq: Option<u32>,
        snapshot_seq: u64,
        server_time_ms: u64,
    ) -> GameStateUpdate {
        GameStateUpdate {
            players: Self::players_by_id(self.players)
                .map(|p| p.to_state(self.tick, self.tick_rate))
                .collect(),
            enemies: self.enemies.iter().map(|e| e.to_state()).collect(),
            boosters: self.boosters.to_vec(),
            points: self.points.to_vec(),
            portal: self.portal.clone(),
            time_remaining: self.time_remaining,
            tick: self.tick,
            server_time_ms,
            last_processed_input_seq,
            last_event_id: self.last_event_id,
            snapshot_seq,
            // Projection never knows about requests; the Room's RequestFullState reply
            // path stamps the echo (build_full_state_reply).
            full_state_request_id: None,
        }
    }

    /// Movement delta (positions / directions / speeds / score only).
    pub fn delta(
        &self,
        last_processed_input_seq: Option<u32>,
        snapshot_seq: u64,
        server_time_ms: u64,
    ) -> GameStateDelta {
        GameStateDelta {
            players: Self::players_by_id(self.players)
                .map(|p| PlayerPositionUpdate {
                    id: p.id.clone(),
                    position: p.position,
                    direction: p.direction,
                    speed: p.speed,
                    score: p.score,
                })
                .collect(),
            enemies: self
                .enemies
                .iter()
                .map(|e| EnemyPositionUpdate {
                    id: e.id.clone(),
                    position: e.position,
                    direction: e.direction,
                    speed: e.speed,
                    generation: e.generation,
                })
                .collect(),
            tick: self.tick,
            time_remaining: self.time_remaining,
            server_time_ms,
            last_processed_input_seq,
            last_event_id: self.last_event_id,
            snapshot_seq,
        }
    }
}
