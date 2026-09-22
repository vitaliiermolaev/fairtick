//! EnemyDeathCandidatePolicy — the timeline-EXPLICIT successor to [`EnemyContactPolicy`]
//! for the one server-side death that depends on what the victim *saw*: an enemy eating a
//! player.
//!
//! ## The rule
//!
//! Server contact is NECESSARY but NOT SUFFICIENT. A kill lands only when BOTH hold:
//!   * the SERVER timeline confirms contact (server_dist held for N ticks), AND
//!   * the victim's PRESENTATION timeline confirms contact — the killer enemy, as the victim
//!     was rendering it (reconstructed `interp_delay` ticks back, on the SAME enemy life),
//!     was within the VISIBLE-confirm radius.
//!
//! If the server confirms but the visible timeline does not — a gap, OR no reconstructable
//! history (early match / generation mismatch / pre-respawn tick) — the kill is HELD
//! (`Delay`), never forced through by a timer. As the enemy keeps closing the reconstructed
//! distance shrinks and confirms; if instead the server contact breaks first (a transient
//! brush) the Room resets the run and the candidate is suppressed.
//!
//! ## Config-driven
//!
//! The thresholds live in `config.death_fairness` (versioned, folded into `config_hash`), not
//! hardcoded consts: the visible-confirm radius is
//! `player_render_radius + enemy_render_radius + grace` (the sprite-overlap distance — NOT the
//! server kill radius), and the enemy interp delay is the SAME value the client renders at
//! (sent in `Welcome`). This policy is pure: it receives the resolved numbers in the context.
//!
//! ## Honest naming
//!
//! The reconstructed distance is the victim's CURRENT (predicted) position vs the enemy
//! reconstructed `interp_delay` back, so it's `reconstructed_enemy_visible_dist` — the
//! server's reconstruction of the *enemy-visible* distance, not a full player-presented model.

use crate::game::policies::enemy_contact::{ContactDecision, EnemyContactContext, EnemyContactPolicy};
use crate::game::timeline::ServerTick;

// Re-export so the Room and the death log key off ONE name for the kill reason.
pub use crate::game::policies::enemy_contact::KillReason;

/// Everything the death *rule* needs for one player vs. their closest eating enemy this
/// tick. Pure inputs. NOTE: there is no `shielded` field — the Room guarantees this is only
/// called for an UNSHIELDED player in server-contact range, and resets the run explicitly
/// when shielded (so the field can't go stale/misleading here).
#[derive(Clone, Copy, Debug)]
pub struct EnemyDeathContext {
    /// Authoritative server tick of this decision.
    pub server_tick: ServerTick,
    /// Tick the enemy was reconstructed at on the victim's presentation timeline
    /// (`server_tick − interp_delay_ticks`).
    pub reconstructed_enemy_tick: ServerTick,
    /// SERVER-truth distance to the closest eating enemy this tick (px).
    pub server_dist: f32,
    /// Reconstructed enemy-visible distance (current player vs the killer's SAME-LIFE position
    /// at `reconstructed_enemy_tick`). `None` when it couldn't be reconstructed (no/old
    /// history, generation mismatch, pre-respawn tick) → the armed gate HOLDS rather than
    /// trusting server-current.
    pub reconstructed_enemy_visible_dist: Option<f32>,
    /// Ticks the enemy is rendered behind on the client (config `enemy_interp_delay_ticks`).
    pub interp_delay_ticks: u64,
    /// The victim-favored SERVER kill radius (see [`EnemyContactPolicy::kill_radius`]).
    pub kill_radius: f32,
    /// Consecutive in-range ticks against the SAME enemy life on the SERVER timeline, incl. now.
    pub contact_ticks_server: u32,
    /// Threshold a contact run must reach to kill (`CONTACT_TICKS_REQUIRED`).
    pub contact_ticks_required: u32,
    /// `Some(r)` = visible gate ENABLED (kill only when the reconstructed enemy-visible
    /// distance ≤ r — the sprite-overlap radius, NOT the kill radius); `None` = observe-only.
    pub visible_confirm_radius_px: Option<f32>,
    /// Reconstructed distance from the victim PROJECTED FORWARD by the client lead (where the
    /// client was drawing the local player) to the same reconstructed enemy. policy v4 requires
    /// THIS within the confirm radius too: the gate's reconstructed-only check missed ghosts where
    /// the victim had predicted themselves past the enemy. `None` = no lead sample yet → the gate
    /// falls back to reconstructed-only (the Room logs `projected_visible_missing_on_death_candidate`).
    pub victim_projected_visible_dist: Option<f32>,
    /// Estimated client lead (ticks) used to build the projection — `None` when no input sample
    /// exists yet. Feeds the v5 prediction-uncertainty guard (a big lead makes the projection
    /// turn-sensitive). See [`DelayReason::PredictionUncertain`].
    pub victim_projected_lead_ticks: Option<f64>,
}

/// Why a server-confirmed kill is being held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelayReason {
    /// The reconstructed enemy-visible distance is beyond the confirm radius.
    VisibleGap,
    /// The visible timeline couldn't be reconstructed (no/old history, generation mismatch).
    NoVisibleHistory,
    /// v5: both visible legs nominally confirm, but the kill is leaning HARD on the
    /// constant-velocity victim projection at a meaningful lead — which goes stale at a turn
    /// boundary and can manufacture a "visible" kill the client drew well outside the radius
    /// (the confirmed ghost: server projected the victim right while the client had it going
    /// down). Hold this tick; a real contact re-confirms once the raw reconstructed distance is
    /// itself small, a turn artifact breaks contact and is suppressed.
    PredictionUncertain,
}

// v5 prediction-uncertainty guard thresholds (lead manifesto 2026-06-06). Trigger only when the
// projection is the DECIDING factor: the raw reconstructed distance is still wide, the projection
// pulled it inside by a lot, and the lead is big enough to be turn-sensitive. Tuned to the
// confirmed ghost (raw 15.81, proj 9.72, help 6.09, lead 4) → delay, while a genuinely close
// contact (raw ≤ 14.5) still kills immediately.
const PREDICTION_UNCERTAIN_RAW_MIN: f32 = 14.5;
const PREDICTION_UNCERTAIN_HELP_MIN: f32 = 3.0;
const PREDICTION_UNCERTAIN_LEAD_MIN: f64 = 3.0;

/// The death decision for one player this tick. Control flow for the Room; the Room maps it
/// (plus suppress/shield context it owns) to a stable `protocol::DeathDecisionCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeathDecision {
    /// Server says no contact. (Defensive — the Room only calls this for in-range unshielded
    /// players; it resets the run on its own None/shielded paths.)
    NoServerContact,
    /// Server contact still building toward the threshold.
    Track,
    /// Server-confirmed kill HELD: the presentation timeline doesn't confirm contact.
    Delay { reason: DelayReason },
    /// Kill: server contact held AND the victim-visible timeline confirms (or observe-only).
    Kill { reason: KillReason },
}

pub struct EnemyDeathCandidatePolicy;

impl EnemyDeathCandidatePolicy {
    /// Pure death decision. The SERVER-timeline arm is delegated to [`EnemyContactPolicy`]
    /// (single source of the honest rule, called with `shielded:false` — the Room guarantees
    /// it); this policy adds the visible-confirmation requirement on top of a confirmed kill.
    pub fn evaluate(ctx: &EnemyDeathContext) -> DeathDecision {
        let server_decision = EnemyContactPolicy::evaluate(&EnemyContactContext {
            server_tick: ctx.server_tick,
            server_dist: ctx.server_dist,
            kill_radius: ctx.kill_radius,
            contact_ticks: ctx.contact_ticks_server,
            contact_ticks_required: ctx.contact_ticks_required,
            shielded: false, // Room handles shielding (explicit reset) before calling.
        });

        match server_decision {
            ContactDecision::NoContact => DeathDecision::NoServerContact,
            ContactDecision::TrackContact => DeathDecision::Track,
            ContactDecision::KillPlayer { reason } => match ctx.visible_confirm_radius_px {
                // Observe-only: gate disabled → trust server truth.
                None => DeathDecision::Kill { reason },
                Some(confirm_radius) => match ctx.reconstructed_enemy_visible_dist {
                    // Armed but the visible timeline couldn't be reconstructed → HOLD, do NOT
                    // fall back to a server-current kill (player-visible timeline is authoritative).
                    None => DeathDecision::Delay { reason: DelayReason::NoVisibleHistory },
                    Some(visible_dist) if visible_dist > confirm_radius => {
                        DeathDecision::Delay { reason: DelayReason::VisibleGap }
                    }
                    // policy v4: the enemy-reconstructed leg confirms, but a kill ALSO needs the
                    // victim PROJECTED FORWARD by the client lead to be within the radius — the leg
                    // that catches a victim who has predicted themselves past the enemy (the
                    // confirmed ghost: reconstructed 17 ≤ 18 but projected 20.4 > 18). When the
                    // projection is unavailable (no lead sample) we DON'T regress to a ghost-prone
                    // kill — fall back to reconstructed-only and let the Room log the gap.
                    Some(raw) => match ctx.victim_projected_visible_dist {
                        Some(projected) if projected > confirm_radius => {
                            DeathDecision::Delay { reason: DelayReason::VisibleGap }
                        }
                        // v5: both legs nominally confirm, but if the kill leans HARD on the
                        // projection (raw still wide, projection pulled it in a lot) at a
                        // turn-sensitive lead, the victim's render direction may be stale → HOLD.
                        Some(projected)
                            if raw > PREDICTION_UNCERTAIN_RAW_MIN
                                && (raw - projected) > PREDICTION_UNCERTAIN_HELP_MIN
                                && ctx
                                    .victim_projected_lead_ticks
                                    .is_some_and(|l| l >= PREDICTION_UNCERTAIN_LEAD_MIN) =>
                        {
                            DeathDecision::Delay { reason: DelayReason::PredictionUncertain }
                        }
                        // Both legs confirm (and not projection-uncertain), or no projection
                        // available (no lead sample → reconstructed-only fallback) → kill.
                        _ => DeathDecision::Kill { reason },
                    },
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::policies::enemy_contact::CONTACT_TICKS_REQUIRED;

    const KILL_RADIUS: f32 = 11.0;
    const CONFIRM_RADIUS: f32 = 20.0; // arbitrary test radius for the gate mechanism

    // Default ctx: the projected leg MIRRORS the reconstructed leg (the common case where the
    // victim isn't predicting itself away), so the pre-v4 tests still exercise the same paths.
    fn ctx(
        server_dist: f32,
        contact_ticks_server: u32,
        visible_dist: Option<f32>,
        confirm_radius: Option<f32>,
    ) -> EnemyDeathContext {
        ctx_proj(server_dist, contact_ticks_server, visible_dist, visible_dist, confirm_radius)
    }

    // Full control over the two reconstructed legs (enemy-back vs victim-projected-forward).
    // lead = None → the v5 prediction-uncertainty guard never fires (pre-v5 behaviour).
    fn ctx_proj(
        server_dist: f32,
        contact_ticks_server: u32,
        visible_dist: Option<f32>,
        projected_dist: Option<f32>,
        confirm_radius: Option<f32>,
    ) -> EnemyDeathContext {
        ctx_lead(server_dist, contact_ticks_server, visible_dist, projected_dist, None, confirm_radius)
    }

    // Adds the client lead (drives the v5 prediction-uncertainty guard).
    fn ctx_lead(
        server_dist: f32,
        contact_ticks_server: u32,
        visible_dist: Option<f32>,
        projected_dist: Option<f32>,
        lead: Option<f64>,
        confirm_radius: Option<f32>,
    ) -> EnemyDeathContext {
        EnemyDeathContext {
            server_tick: ServerTick::new(1000),
            reconstructed_enemy_tick: ServerTick::new(992),
            server_dist,
            reconstructed_enemy_visible_dist: visible_dist,
            interp_delay_ticks: 8,
            kill_radius: KILL_RADIUS,
            contact_ticks_server,
            contact_ticks_required: CONTACT_TICKS_REQUIRED,
            visible_confirm_radius_px: confirm_radius,
            victim_projected_visible_dist: projected_dist,
            victim_projected_lead_ticks: lead,
        }
    }

    #[test]
    fn out_of_range_is_no_server_contact() {
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(KILL_RADIUS, 99, None, None)),
            DeathDecision::NoServerContact
        );
    }

    #[test]
    fn below_threshold_tracks() {
        for ticks in 0..CONTACT_TICKS_REQUIRED {
            assert_eq!(
                EnemyDeathCandidatePolicy::evaluate(&ctx(2.0, ticks, Some(0.0), Some(CONFIRM_RADIUS))),
                DeathDecision::Track
            );
        }
    }

    #[test]
    fn observe_only_preserves_server_kill() {
        // gate disabled → kills regardless of (even absent) visible distance.
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(2.0, CONTACT_TICKS_REQUIRED, Some(35.0), None)),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(2.0, CONTACT_TICKS_REQUIRED, None, None)),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
    }

    #[test]
    fn armed_holds_on_visible_gap_with_no_timer() {
        let r = Some(CONFIRM_RADIUS);
        // Far visible, even after a huge contact run → still held (no liveness timer).
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(2.0, CONTACT_TICKS_REQUIRED + 100, Some(30.0), r)),
            DeathDecision::Delay { reason: DelayReason::VisibleGap }
        );
    }

    #[test]
    fn armed_holds_on_no_visible_history() {
        // Gate armed + no reconstruction → HOLD (NOT a server-current kill). This is the
        // round-3 fix: player-visible timeline is authoritative even when we can't see it.
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(
                2.0,
                CONTACT_TICKS_REQUIRED,
                None,
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Delay { reason: DelayReason::NoVisibleHistory }
        );
    }

    #[test]
    fn armed_kills_when_visible_confirms() {
        let r = Some(CONFIRM_RADIUS);
        // Within the confirm radius → kill.
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(2.0, CONTACT_TICKS_REQUIRED, Some(18.0), r)),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
        // Exactly at the radius → kill (strict `>` to hold).
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(2.0, CONTACT_TICKS_REQUIRED, Some(CONFIRM_RADIUS), r)),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
    }

    #[test]
    fn gate_never_adds_a_kill_below_server_threshold() {
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx(
                2.0,
                CONTACT_TICKS_REQUIRED - 1,
                Some(0.0),
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Track
        );
    }

    // policy v4: the confirmed ghost — enemy-reconstructed leg confirms (≤ radius) but the
    // victim-projected-forward leg is beyond it (the device case: reconstructed 17, projected 20.4,
    // radius 18). Must HOLD, not kill.
    #[test]
    fn armed_holds_when_projected_exceeds_radius() {
        let r = Some(CONFIRM_RADIUS);
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_proj(
                2.0,
                CONTACT_TICKS_REQUIRED,
                Some(17.0),
                Some(25.0),
                r
            )),
            DeathDecision::Delay { reason: DelayReason::VisibleGap }
        );
        // Exactly at the radius on both legs → kill (strict `>` to hold).
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_proj(
                2.0,
                CONTACT_TICKS_REQUIRED,
                Some(CONFIRM_RADIUS),
                Some(CONFIRM_RADIUS),
                r
            )),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
    }

    // Both legs within the radius → kill (the honest device deaths: reconstructed ~16, projected ~8).
    #[test]
    fn armed_kills_when_both_legs_confirm() {
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_proj(
                4.0,
                CONTACT_TICKS_REQUIRED,
                Some(16.4),
                Some(8.3),
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
    }

    // No lead sample (projected None) → fall back to the reconstructed-only rule rather than
    // regressing to a ghost-prone kill block; the Room logs the gap separately. Reconstructed
    // confirms → kill; reconstructed beyond radius → still held.
    #[test]
    fn projected_none_falls_back_to_reconstructed_only() {
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_proj(
                2.0,
                CONTACT_TICKS_REQUIRED,
                Some(10.0),
                None,
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_proj(
                2.0,
                CONTACT_TICKS_REQUIRED,
                Some(30.0),
                None,
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Delay { reason: DelayReason::VisibleGap }
        );
    }

    // v5: both legs nominally confirm, but the kill leans hard on the projection at a big lead —
    // the confirmed turn-boundary ghost (raw 15.81, proj 9.72, help 6.09, lead 4). HOLD.
    #[test]
    fn v5_prediction_uncertain_holds_turn_boundary_ghost() {
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_lead(
                5.27,
                CONTACT_TICKS_REQUIRED,
                Some(15.81),
                Some(9.72),
                Some(4.0),
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Delay { reason: DelayReason::PredictionUncertain }
        );
    }

    // v5 guard is OFF when the contact is genuinely close (raw ≤ 14.5) even with a big lead —
    // the projection isn't doing the heavy lifting, so the kill is trustworthy.
    #[test]
    fn v5_close_contact_still_kills_despite_lead() {
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_lead(
                4.0,
                CONTACT_TICKS_REQUIRED,
                Some(14.0),
                Some(8.0),
                Some(6.0),
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
    }

    // v5 guard is OFF at low lead (not turn-sensitive) and OFF when the projection barely helped.
    #[test]
    fn v5_guard_off_for_low_lead_or_small_projection_help() {
        // raw wide, big help, but lead < 3 → kill.
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_lead(
                4.0,
                CONTACT_TICKS_REQUIRED,
                Some(16.0),
                Some(8.0),
                Some(2.0),
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
        // raw wide, big lead, but projection help ≤ 3 (raw 16 → proj 14) → kill.
        assert_eq!(
            EnemyDeathCandidatePolicy::evaluate(&ctx_lead(
                4.0,
                CONTACT_TICKS_REQUIRED,
                Some(16.0),
                Some(14.0),
                Some(6.0),
                Some(CONFIRM_RADIUS)
            )),
            DeathDecision::Kill { reason: KillReason::EnemyContact }
        );
    }
}
