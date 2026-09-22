//! Domain events — the gameplay *facts* the simulation produces.
//!
//! A `DomainEvent` is "this happened in the match" (a player respawned, an enemy was
//! eaten, a booster spawned). It is deliberately NOT the wire format: the simulation
//! emits `DomainEvent`s and a thin mapping layer turns them into `protocol::ServerEvent`
//! for the client. That keeps the gameplay code from naming `ServerMessage` /
//! `ServerEvent` at every emit site, so the wire protocol can change without touching the
//! rules, and the same fact can later feed telemetry / replay / tests (see Plan.md §3).
//!
//! `event_id` and `server_tick` are NOT part of a `DomainEvent` — they are transport
//! bookkeeping stamped by the Room when it emits (the gap-free id counter and the current
//! tick). The Room's `emit_event` funnel is the single place that does the stamping +
//! conversion + broadcast.

use crate::protocol::{Booster, DeathDecisionCode, Direction, PointItem, Position, ServerEvent, StatePatch};

/// A gameplay fact. One variant per `protocol::ServerEvent`, minus the transport-stamped
/// `event_id` / `server_tick`.
#[derive(Debug, Clone)]
pub enum DomainEvent {
    /// A player was eaten (always via an accepted EatClaim in the current model).
    PlayerEaten {
        eater_id: String,
        eaten_id: String,
        eaten_position: Position,
    },
    /// An enemy killed a player — the authoritative death fact with the full
    /// timeline-explicit decision context. Emitted (reliably) BEFORE the victim's
    /// `PlayerRespawned`; death and respawn are separate facts.
    PlayerKilledByEnemy {
        victim_id: String,
        killer_enemy_id: String,
        killer_enemy_generation: u32,
        killer_enemy_respawned_at_tick: u64,
        server_dist: f32,
        reconstructed_enemy_tick: u64,
        reconstructed_enemy_visible_dist: Option<f32>,
        server_kill_radius: f32,
        /// Whether the visible gate was armed (false = observe-only, server-truth kill).
        visible_gate_enabled: bool,
        /// Visible-confirm radius when armed; `None` in observe-only mode (no `Infinity`).
        visible_confirm_radius_px: Option<f32>,
        interp_delay_ticks: u64,
        contact_ticks: u32,
        decision: DeathDecisionCode,
        policy_version: u32,
        killer_position: Position,
        victim_position: Position,
    },
    /// A player respawned. Hard event — clears client prediction history. Carries the
    /// death-fairness diagnostics (None for a player-vs-player death).
    PlayerRespawned {
        player_id: String,
        position: Position,
        state_patch: StatePatch,
        killer_player_id: Option<String>,
        killer_enemy_id: Option<String>,
        server_dist: Option<f32>,
        contact_ticks: Option<u32>,
        killer_position: Option<Position>,
        victim_position: Option<Position>,
    },
    /// A player stepped through the portal and was teleported. Hard event.
    PortalTeleport {
        player_id: String,
        from: Position,
        to: Position,
        state_patch: StatePatch,
    },
    /// A player's move speed changed (boost start/expire). Soft event scheduled at
    /// `effective_tick` so client prediction uses the authoritative speed.
    PlayerSpeedChanged {
        effective_tick: u64,
        player_id: String,
        speed: f32,
        is_invincible: bool,
        reason: String,
    },
    /// An enemy relocated (respawn). Hard event — clears the entity's interp history.
    EnemyRespawned {
        enemy_id: String,
        position: Position,
        direction: Direction,
        speed: f32,
        score: u32,
        reason: String,
        caused_by_player_id: Option<String>,
        /// New life id (incremented every respawn) so the client can pin eat claims to it.
        generation: u32,
    },
    PointCollected {
        point_id: String,
        player_id: String,
    },
    BoosterCollected {
        booster_id: String,
        player_id: String,
    },
    PointSpawned {
        point: PointItem,
    },
    BoosterSpawned {
        booster: Booster,
    },
}

impl DomainEvent {
    /// Map a gameplay fact to the wire event, stamping the transport bookkeeping. This is
    /// the ONLY place the simulation's events become `protocol::ServerEvent`.
    pub fn into_server_event(self, event_id: u64, server_tick: u64) -> ServerEvent {
        match self {
            DomainEvent::PlayerEaten { eater_id, eaten_id, eaten_position } => {
                ServerEvent::PlayerEaten { event_id, server_tick, eater_id, eaten_id, eaten_position }
            }
            DomainEvent::PlayerKilledByEnemy {
                victim_id,
                killer_enemy_id,
                killer_enemy_generation,
                killer_enemy_respawned_at_tick,
                server_dist,
                reconstructed_enemy_tick,
                reconstructed_enemy_visible_dist,
                server_kill_radius,
                visible_gate_enabled,
                visible_confirm_radius_px,
                interp_delay_ticks,
                contact_ticks,
                decision,
                policy_version,
                killer_position,
                victim_position,
            } => ServerEvent::PlayerKilledByEnemy {
                event_id,
                server_tick,
                victim_id,
                killer_enemy_id,
                killer_enemy_generation,
                killer_enemy_respawned_at_tick,
                server_dist,
                reconstructed_enemy_tick,
                reconstructed_enemy_visible_dist,
                server_kill_radius,
                visible_gate_enabled,
                visible_confirm_radius_px,
                interp_delay_ticks,
                contact_ticks,
                decision,
                policy_version,
                killer_position,
                victim_position,
            },
            DomainEvent::PlayerRespawned {
                player_id,
                position,
                state_patch,
                killer_player_id,
                killer_enemy_id,
                server_dist,
                contact_ticks,
                killer_position,
                victim_position,
            } => ServerEvent::PlayerRespawned {
                event_id,
                server_tick,
                player_id,
                position,
                state_patch,
                killer_player_id,
                killer_enemy_id,
                server_dist,
                contact_ticks,
                killer_position,
                victim_position,
            },
            DomainEvent::PortalTeleport { player_id, from, to, state_patch } => {
                ServerEvent::PortalTeleport { event_id, server_tick, player_id, from, to, state_patch }
            }
            DomainEvent::PlayerSpeedChanged { effective_tick, player_id, speed, is_invincible, reason } => {
                ServerEvent::PlayerSpeedChanged {
                    event_id,
                    server_tick,
                    effective_tick,
                    player_id,
                    speed,
                    is_invincible,
                    reason,
                }
            }
            DomainEvent::EnemyRespawned {
                enemy_id,
                position,
                direction,
                speed,
                score,
                reason,
                caused_by_player_id,
                generation,
            } => ServerEvent::EnemyRespawned {
                event_id,
                server_tick,
                enemy_id,
                position,
                direction,
                speed,
                score,
                reason,
                caused_by_player_id,
                generation,
            },
            DomainEvent::PointCollected { point_id, player_id } => {
                ServerEvent::PointCollected { event_id, server_tick, point_id, player_id }
            }
            DomainEvent::BoosterCollected { booster_id, player_id } => {
                ServerEvent::BoosterCollected { event_id, server_tick, booster_id, player_id }
            }
            DomainEvent::PointSpawned { point } => ServerEvent::PointSpawned { event_id, server_tick, point },
            DomainEvent::BoosterSpawned { booster } => {
                ServerEvent::BoosterSpawned { event_id, server_tick, booster }
            }
        }
    }

    /// Stable variant name for telemetry/diagnostics (e.g. the reliable-before-snapshot
    /// ordering log) — borrows, does not consume the event.
    pub fn kind(&self) -> &'static str {
        match self {
            DomainEvent::PlayerEaten { .. } => "PlayerEaten",
            DomainEvent::PlayerKilledByEnemy { .. } => "PlayerKilledByEnemy",
            DomainEvent::PlayerRespawned { .. } => "PlayerRespawned",
            DomainEvent::PortalTeleport { .. } => "PortalTeleport",
            DomainEvent::PlayerSpeedChanged { .. } => "PlayerSpeedChanged",
            DomainEvent::EnemyRespawned { .. } => "EnemyRespawned",
            DomainEvent::PointCollected { .. } => "PointCollected",
            DomainEvent::BoosterCollected { .. } => "BoosterCollected",
            DomainEvent::PointSpawned { .. } => "PointSpawned",
            DomainEvent::BoosterSpawned { .. } => "BoosterSpawned",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DomainEvent;
    use crate::protocol::{Direction, Position, ServerEvent, StatePatch};

    fn patch() -> StatePatch {
        StatePatch {
            position: Position { x: 1.0, y: 2.0 },
            direction: Direction::Up,
            speed: 100.0,
            is_invincible: false,
        }
    }

    // The map is the central seam between gameplay facts and the wire. These lock that
    // EVERY hard/important event stamps event_id + server_tick and threads its payload
    // through — so adding a protocol field without wiring DomainEvent fails a test.

    #[test]
    fn player_respawned_stamps_id_tick_and_preserves_fairness_fields() {
        let ev = DomainEvent::PlayerRespawned {
            player_id: "p1".into(),
            position: Position { x: 3.0, y: 4.0 },
            state_patch: patch(),
            killer_player_id: None,
            killer_enemy_id: Some("e9".into()),
            server_dist: Some(7.5),
            contact_ticks: Some(3),
            killer_position: Some(Position { x: 5.0, y: 6.0 }),
            victim_position: Some(Position { x: 3.0, y: 4.0 }),
        };
        match ev.into_server_event(42, 777) {
            ServerEvent::PlayerRespawned {
                event_id,
                server_tick,
                player_id,
                killer_enemy_id,
                server_dist,
                contact_ticks,
                killer_position,
                ..
            } => {
                assert_eq!((event_id, server_tick), (42, 777));
                assert_eq!(player_id, "p1");
                assert_eq!(killer_enemy_id.as_deref(), Some("e9"));
                assert_eq!(server_dist, Some(7.5));
                assert_eq!(contact_ticks, Some(3));
                assert_eq!(killer_position.map(|p| (p.x, p.y)), Some((5.0, 6.0)));
            }
            _ => panic!("expected ServerEvent::PlayerRespawned"),
        }
    }

    #[test]
    fn portal_teleport_stamps_and_maps() {
        let ev = DomainEvent::PortalTeleport {
            player_id: "p1".into(),
            from: Position { x: 1.0, y: 1.0 },
            to: Position { x: 9.0, y: 9.0 },
            state_patch: patch(),
        };
        match ev.into_server_event(1, 2) {
            ServerEvent::PortalTeleport { event_id, server_tick, to, .. } => {
                assert_eq!((event_id, server_tick), (1, 2));
                assert_eq!((to.x, to.y), (9.0, 9.0));
            }
            _ => panic!("expected ServerEvent::PortalTeleport"),
        }
    }

    #[test]
    fn enemy_respawned_stamps_and_maps() {
        let ev = DomainEvent::EnemyRespawned {
            enemy_id: "e1".into(),
            position: Position { x: 0.0, y: 0.0 },
            direction: Direction::Left,
            speed: 100.0,
            score: 25,
            reason: "player_ate_enemy".into(),
            caused_by_player_id: Some("p1".into()),
            generation: 3,
        };
        match ev.into_server_event(5, 6) {
            ServerEvent::EnemyRespawned {
                event_id,
                server_tick,
                enemy_id,
                score,
                caused_by_player_id,
                generation,
                ..
            } => {
                assert_eq!((event_id, server_tick), (5, 6));
                assert_eq!(enemy_id, "e1");
                assert_eq!(score, 25);
                assert_eq!(caused_by_player_id.as_deref(), Some("p1"));
                assert_eq!(generation, 3, "generation is threaded through the wire mapping");
            }
            _ => panic!("expected ServerEvent::EnemyRespawned"),
        }
    }

    #[test]
    fn player_speed_changed_stamps_effective_tick_separately() {
        let ev = DomainEvent::PlayerSpeedChanged {
            effective_tick: 100,
            player_id: "p1".into(),
            speed: 160.0,
            is_invincible: true,
            reason: "boost".into(),
        };
        match ev.into_server_event(8, 9) {
            ServerEvent::PlayerSpeedChanged {
                event_id,
                server_tick,
                effective_tick,
                speed,
                is_invincible,
                ..
            } => {
                // server_tick (when emitted) and effective_tick (when it applies) are
                // distinct timelines — both must survive the map.
                assert_eq!((event_id, server_tick), (8, 9));
                assert_eq!(effective_tick, 100);
                assert_eq!(speed, 160.0);
                assert!(is_invincible);
            }
            _ => panic!("expected ServerEvent::PlayerSpeedChanged"),
        }
    }

    #[test]
    fn point_collected_stamps_and_maps() {
        let ev = DomainEvent::PointCollected { point_id: "pt1".into(), player_id: "p1".into() };
        match ev.into_server_event(11, 12) {
            ServerEvent::PointCollected { event_id, server_tick, point_id, player_id } => {
                assert_eq!((event_id, server_tick), (11, 12));
                assert_eq!(point_id, "pt1");
                assert_eq!(player_id, "p1");
            }
            _ => panic!("expected ServerEvent::PointCollected"),
        }
    }

    #[test]
    fn kind_is_stable_per_variant() {
        assert_eq!(
            DomainEvent::PortalTeleport {
                player_id: "p".into(),
                from: Position { x: 0.0, y: 0.0 },
                to: Position { x: 0.0, y: 0.0 },
                state_patch: patch(),
            }
            .kind(),
            "PortalTeleport"
        );
    }
}
