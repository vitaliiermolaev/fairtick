//! EnemyContactPolicy — does an enemy eat a player this tick?
//!
//! This is the most fairness-sensitive rule in the game (see the
//! `deaths-are-presentation-skew` note): a render-skewed enemy is drawn behind its true
//! position, so a naive "centres within radius → die" kills from across the screen. The
//! rule is therefore *victim-favored*: the kill radius is shrunk by `VICTIM_GRACE_PX`,
//! and the contact must persist for `CONTACT_TICKS_REQUIRED` consecutive ticks against
//! the SAME enemy, so a one-tick brush can't kill instantly.
//!
//! Pulled out of `Room::check_enemy_collisions` so the rule is one pure, testable
//! function. The Room still owns the world query (which enemy is closest, is the player
//! shielded) and the side effects (telemetry, respawn); this module owns the decision
//! plus the two tunable constants.

use crate::game::timeline::ServerTick;

/// Victim-favored kill-radius shrink (px). HARDCODED (not config) to keep `config_hash`
/// stable — no StreamingAssets re-sync.
///
/// Tuned 2026-06-03 from device telemetry (see the `match-telemetry` note): deaths
/// landed at server_dist 10–13px while the player saw the enemy 26–28px away
/// (visual_minus_server up to ~17px), reading as "sudden death". With
/// `collision_distance_px = 20` a grace of 9 puts the server kill at ~11px while the
/// sprite stays 18px wide (hitbox < sprite, arcade-style forgiveness).
pub const VICTIM_GRACE_PX: f32 = 9.0;

/// Consecutive same-enemy contact ticks required for a kill (~50ms at 60Hz).
///
/// A first pass tried 6 ticks (~100ms) but it was NON-LETHAL in play — the contact run
/// is keyed to a single enemy_id and resets when the closest enemy changes, so running
/// THROUGH a cluster never accumulated 6 ticks (the player became invincible to groups).
/// Back to 3: a real pass-through (centres overlap → dist ~0) still kills; the smaller
/// ~11px radius already mitigates the render-skewed one-tick brush 6 ticks was meant to
/// filter.
pub const CONTACT_TICKS_REQUIRED: u32 = 3;

// Invariant guard (fails the build, not at runtime): a 0-tick requirement would make a
// kill fire on the very first frame of contact, defeating the victim-favored rule.
const _: () = assert!(CONTACT_TICKS_REQUIRED > 0, "CONTACT_TICKS_REQUIRED must be > 0");

/// What killed a player. Only enemy contact is server-side; PvP deaths arrive via an
/// accepted EatClaim and never reach this policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillReason {
    EnemyContact,
}

/// Everything the death *rule* needs for one player vs. their closest eating enemy this
/// tick. Pure inputs — no world handles, no channels. `server_tick` is carried for
/// explainability / telemetry; the policy does not branch on it (yet).
#[derive(Clone, Copy, Debug)]
pub struct EnemyContactContext {
    pub server_tick: ServerTick,
    /// Distance to the closest enemy that *can* eat this player (px). `f32::INFINITY`
    /// when there is no such enemy.
    pub server_dist: f32,
    /// The shrunk kill radius — see [`EnemyContactPolicy::kill_radius`].
    pub kill_radius: f32,
    /// Consecutive ticks of contact with the SAME enemy, INCLUDING this tick.
    pub contact_ticks: u32,
    /// Threshold a contact run must reach to kill (`CONTACT_TICKS_REQUIRED`).
    pub contact_ticks_required: u32,
    /// Player is in a safe zone or spawn-protected — contact cannot accumulate.
    pub shielded: bool,
}

/// The death decision for one player this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContactDecision {
    /// No eating enemy inside the shrunk radius (or the player is shielded). The Room
    /// resets this player's contact run.
    NoContact,
    /// An enemy is in range but the contact run has not reached the kill threshold yet.
    TrackContact,
    /// Same enemy held in range for `contact_ticks_required` ticks — the player dies.
    KillPlayer { reason: KillReason },
}

pub struct EnemyContactPolicy;

impl EnemyContactPolicy {
    /// The victim-favored kill radius: an enemy must get *closer* than the raw collision
    /// distance to kill. Single source of truth for `VICTIM_GRACE_PX` — used by the Room
    /// world query and by the `Welcome` handshake (`enemy_kill_radius_px`).
    #[inline]
    pub fn kill_radius(collision_distance_px: f32) -> f32 {
        collision_distance_px - VICTIM_GRACE_PX
    }

    /// Pure death decision. Order of checks mirrors the original
    /// `check_enemy_collisions`: shielded or out-of-range ⇒ no accumulating threat;
    /// otherwise kill once the run reaches the threshold.
    pub fn evaluate(ctx: &EnemyContactContext) -> ContactDecision {
        let in_range = ctx.server_dist < ctx.kill_radius;
        if ctx.shielded || !in_range {
            return ContactDecision::NoContact;
        }
        if ctx.contact_ticks >= ctx.contact_ticks_required {
            ContactDecision::KillPlayer { reason: KillReason::EnemyContact }
        } else {
            ContactDecision::TrackContact
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(server_dist: f32, contact_ticks: u32, shielded: bool) -> EnemyContactContext {
        EnemyContactContext {
            server_tick: ServerTick::new(100),
            server_dist,
            kill_radius: EnemyContactPolicy::kill_radius(20.0), // == 11.0
            contact_ticks,
            contact_ticks_required: CONTACT_TICKS_REQUIRED,
            shielded,
        }
    }

    #[test]
    fn kill_radius_shrinks_by_grace() {
        assert_eq!(EnemyContactPolicy::kill_radius(20.0), 11.0);
    }

    #[test]
    fn out_of_range_is_no_contact() {
        // dist == radius is NOT a contact (strict `<`, mirrors the original `>=` skip).
        assert_eq!(EnemyContactPolicy::evaluate(&ctx(11.0, 99, false)), ContactDecision::NoContact);
        assert_eq!(EnemyContactPolicy::evaluate(&ctx(11.5, 99, false)), ContactDecision::NoContact);
    }

    #[test]
    fn shielded_never_kills_even_in_range_past_threshold() {
        assert_eq!(
            EnemyContactPolicy::evaluate(&ctx(0.0, CONTACT_TICKS_REQUIRED + 5, true)),
            ContactDecision::NoContact
        );
    }

    #[test]
    fn in_range_below_threshold_tracks() {
        for ticks in 0..CONTACT_TICKS_REQUIRED {
            assert_eq!(
                EnemyContactPolicy::evaluate(&ctx(5.0, ticks, false)),
                ContactDecision::TrackContact,
                "ticks={ticks} should still be building"
            );
        }
    }

    #[test]
    fn in_range_at_or_past_threshold_kills() {
        assert_eq!(
            EnemyContactPolicy::evaluate(&ctx(5.0, CONTACT_TICKS_REQUIRED, false)),
            ContactDecision::KillPlayer { reason: KillReason::EnemyContact }
        );
        assert_eq!(
            EnemyContactPolicy::evaluate(&ctx(0.0, CONTACT_TICKS_REQUIRED + 10, false)),
            ContactDecision::KillPlayer { reason: KillReason::EnemyContact }
        );
    }
}
