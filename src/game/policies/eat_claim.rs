//! EatClaimValidationPolicy — is a player-initiated eat (an EatClaim) legitimate?
//!
//! Player-eats-X is *claim-based* (attacker-view, protocol v3 — see the `eat-claim-system`
//! note): the client says "at my render tick I saw my mouth overlap this target", and the
//! server re-validates that against the past it recorded (the contact-history ring). The
//! server no longer eats on its own collision.
//!
//! This module owns the **rules**: the thresholds, the eligibility ("can I even eat this?"),
//! the respawn grace, the reconstructed-overlap geometry, and — crucially — the **reject
//! reason strings**, which are a stable contract (they hit telemetry and the
//! `EatClaimRejected` wire message; clients and tests key off them, so they must stay
//! byte-identical).
//!
//! The Room keeps the **world query** (sampling the history ring, finding the enemy,
//! reading current availability) and the **side effects** (telemetry, reject send, score,
//! respawn, broadcast). The Room calls these functions in the SAME order the inline
//! validation used, so behavior is unchanged — this is a pure extraction.

use crate::config_shared::ClaimFairnessConfig;
use crate::game::timeline::{RenderTick, ServerTick};
use crate::protocol::EatClaim;

/// Per-player cap on buffered (pending) eat claims — a RESOURCE bound (anti-OOM), not a fairness
/// tolerance, so it stays a code constant rather than config (it doesn't affect any accept/reject
/// decision, only memory). The fairness tolerances live in [`ClaimFairnessConfig`] (review #3).
pub const EAT_CLAIM_PENDING_CAP: usize = 256;

/// Eat eligibility: an attacker may eat a target only if invincible or strictly heavier.
#[inline]
pub fn can_eat(attacker_invincible: bool, attacker_score: u32, target_score: u32) -> bool {
    attacker_invincible || attacker_score > target_score
}

/// Which arm of `can_eat` permitted the eat — for telemetry only, mirroring the
/// short-circuit order (invincible/boost first, then score). `is_invincible` is the boost
/// flag (spawn protection is tracked separately), so "boost" is accurate. "none" means
/// neither arm holds (i.e. `can_eat` was false); it should only ever appear on a REJECT,
/// so seeing it on an accept log is itself a bug signal.
#[inline]
pub fn accept_reason(attacker_invincible: bool, attacker_score: u32, target_score: u32) -> &'static str {
    if attacker_invincible {
        "boost"
    } else if attacker_score > target_score {
        "score"
    } else {
        "none"
    }
}

/// Inputs for the enemy-eat rules (everything after the Room has sampled the enemy's
/// history, confirmed the position match, and located the live enemy).
pub struct EnemyEatRules {
    /// Server tick this enemy's current life started.
    pub respawned_at: ServerTick,
    /// `floor(target_render_tick)`: the history frame (a server tick) the claim targets.
    pub target_floor: ServerTick,
    pub attacker_invincible: bool,
    pub attacker_score: u32,
    pub enemy_score: u32,
    /// Reconstructed attacker↔enemy distance at the claimed ticks (px).
    pub reconstructed: f32,
    /// Generation the client named in the claim (`EatClaim.target_generation`). `None` =
    /// legacy client (no generation tracking) → skip the explicit check and rely on the
    /// respawned_at/target_floor guard below.
    pub claimed_generation: Option<u32>,
    /// The live enemy's current generation. A `Some(claimed)` that differs is a claim
    /// against an enemy life that has already ended (id was reused).
    pub current_generation: u32,
}

/// Inputs for the player-eat rules (everything after the Room has sampled the victim's
/// history, confirmed the position match, and read current availability).
pub struct PlayerEatRules {
    pub victim_now_alive: bool,
    pub victim_now_spawn_protected: bool,
    pub victim_hist_alive: bool,
    pub victim_hist_spawn_protected: bool,
    pub victim_hist_invincible: bool,
    /// Either party in a safe zone (at the reconstructed positions).
    pub in_safe_zone: bool,
    pub attacker_invincible: bool,
    pub attacker_score: u32,
    pub victim_score: u32,
    /// Reconstructed attacker↔victim distance at the claimed ticks (px).
    pub reconstructed: f32,
}

pub struct EatClaimPolicy;

impl EatClaimPolicy {
    /// `true` if a sampled-history position is close enough to the position the client
    /// claimed (the same tolerance for attacker / enemy / victim). The Room owns the
    /// reason string per call site (`attacker_/enemy_/victim_position_mismatch`).
    #[inline]
    pub fn within_position_tolerance(cfg: &ClaimFairnessConfig, history_vs_claim_dist: f32) -> bool {
        history_vs_claim_dist <= cfg.position_tolerance_px
    }

    /// Shape checks that run BEFORE the per-attacker claim-id high-water mark is bumped,
    /// so a NaN/self-target claim never advances the mark. (Order preserved from room.)
    pub fn shape_pre_dedup(claim: &EatClaim, attacker_id: &str) -> Result<(), &'static str> {
        // Reject NaN/Inf before they can poison sampling/dedup/distance checks.
        if !claim.attacker_render_tick.is_finite()
            || !claim.target_render_tick.is_finite()
            || !claim.visual_distance.is_finite()
            || !claim.attacker_position.x.is_finite()
            || !claim.attacker_position.y.is_finite()
            || !claim.target_position.x.is_finite()
            || !claim.target_position.y.is_finite()
        {
            return Err("bad_claim_numbers");
        }
        if claim.target_id == attacker_id {
            return Err("self_target");
        }
        Ok(())
    }

    /// Shape checks that run AFTER the high-water mark is bumped (so a claim rejected here
    /// still advances the mark, exactly as before): visual radius + view-skew geometry.
    pub fn shape_post_dedup(cfg: &ClaimFairnessConfig, claim: &EatClaim) -> Result<(), &'static str> {
        if claim.visual_distance > cfg.visible_overlap_radius_px {
            return Err("visual_distance_too_large");
        }
        // Both ticks are on the attacker's RENDER timeline (the wire carries bare f64s).
        let attacker = RenderTick::new(claim.attacker_render_tick);
        let target = RenderTick::new(claim.target_render_tick);
        if attacker < target {
            return Err("target_tick_after_attacker_tick");
        }
        if attacker.ticks_after(target) > cfg.max_view_skew_ticks {
            return Err("view_skew_too_large");
        }
        Ok(())
    }

    /// The attacker must have been alive at its claimed render tick and standing within
    /// tolerance of where it said it was. `attacker_pos_dist` = |history − claim| (px).
    pub fn attacker_eligible(
        cfg: &ClaimFairnessConfig,
        attacker_alive: bool,
        attacker_pos_dist: f32,
    ) -> Result<(), &'static str> {
        if !attacker_alive {
            return Err("attacker_not_alive_at_claim");
        }
        if !Self::within_position_tolerance(cfg, attacker_pos_dist) {
            return Err("attacker_position_mismatch");
        }
        Ok(())
    }

    /// Enemy-eat rules, in the original order: not-already-respawned, respawn grace,
    /// eat eligibility, reconstructed overlap. (`no_enemy_history`, `enemy_not_found`,
    /// and `enemy_position_mismatch` are Room-side gather failures checked before this.)
    pub fn enemy_eat(cfg: &ClaimFairnessConfig, rules: &EnemyEatRules) -> Result<(), &'static str> {
        // Explicit reusable-entity guard: if the client named the life it saw, it must be
        // the life that's live now. A mismatch means the enemy respawned (id reused) since
        // the client's render tick — a stale claim, rejected before the tick heuristics.
        if let Some(claimed) = rules.claimed_generation {
            if claimed != rules.current_generation {
                return Err("enemy_generation_mismatch");
            }
        }
        if rules.respawned_at > rules.target_floor {
            return Err("enemy_already_respawned");
        }
        if rules.target_floor.get() <= rules.respawned_at.get() + cfg.enemy_respawn_eat_grace_ticks {
            return Err("enemy_respawn_grace");
        }
        if !can_eat(rules.attacker_invincible, rules.attacker_score, rules.enemy_score) {
            return Err("attacker_cannot_eat_enemy");
        }
        if rules.reconstructed > cfg.reconstruct_overlap_radius_px() {
            return Err("server_reconstruct_no_overlap");
        }
        Ok(())
    }

    /// Player-eat rules, in the original order: victim available now, victim unprotected
    /// at the target tick, neither party in a safe zone, eat eligibility, reconstructed
    /// overlap. (`no_victim_history`, `attacker_not_found`, `victim_not_found`, and
    /// `victim_position_mismatch` are Room-side gather failures checked before this.)
    pub fn player_eat(cfg: &ClaimFairnessConfig, rules: &PlayerEatRules) -> Result<(), &'static str> {
        if !rules.victim_now_alive || rules.victim_now_spawn_protected {
            return Err("victim_not_available_now");
        }
        if !rules.victim_hist_alive || rules.victim_hist_spawn_protected || rules.victim_hist_invincible {
            return Err("victim_protected_at_target_tick");
        }
        if rules.in_safe_zone {
            return Err("safe_zone");
        }
        if !can_eat(rules.attacker_invincible, rules.attacker_score, rules.victim_score) {
            return Err("attacker_cannot_eat_player");
        }
        if rules.reconstructed > cfg.reconstruct_overlap_radius_px() {
            return Err("server_reconstruct_no_overlap");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{EatTargetKind, Position};

    fn claim(attacker_tick: f64, target_tick: f64, visual: f32) -> EatClaim {
        EatClaim {
            claim_id: 1,
            target_id: "enemy-1".to_string(),
            target_kind: EatTargetKind::Enemy,
            attacker_render_tick: attacker_tick,
            target_render_tick: target_tick,
            attacker_position: Position { x: 0.0, y: 0.0 },
            target_position: Position { x: 0.0, y: 0.0 },
            visual_distance: visual,
            target_generation: None,
        }
    }

    #[test]
    fn shape_pre_dedup_rejects_nan_and_self_target() {
        let mut c = claim(100.0, 95.0, 5.0);
        c.visual_distance = f32::NAN;
        assert_eq!(EatClaimPolicy::shape_pre_dedup(&c, "att"), Err("bad_claim_numbers"));
        let mut c2 = claim(100.0, 95.0, 5.0);
        c2.target_id = "att".to_string();
        assert_eq!(EatClaimPolicy::shape_pre_dedup(&c2, "att"), Err("self_target"));
        assert_eq!(EatClaimPolicy::shape_pre_dedup(&claim(100.0, 95.0, 5.0), "att"), Ok(()));
    }

    #[test]
    fn shape_post_dedup_enforces_radius_and_skew() {
        let cfg = ClaimFairnessConfig::sample();
        assert_eq!(
            EatClaimPolicy::shape_post_dedup(&cfg, &claim(100.0, 95.0, cfg.visible_overlap_radius_px + 0.1)),
            Err("visual_distance_too_large")
        );
        // target tick after attacker tick (negative skew).
        assert_eq!(
            EatClaimPolicy::shape_post_dedup(&cfg, &claim(95.0, 100.0, 5.0)),
            Err("target_tick_after_attacker_tick")
        );
        // skew just over the cap.
        assert_eq!(
            EatClaimPolicy::shape_post_dedup(&cfg, &claim(100.0 + cfg.max_view_skew_ticks + 0.1, 100.0, 5.0)),
            Err("view_skew_too_large")
        );
        // skew exactly at the cap is allowed.
        assert_eq!(
            EatClaimPolicy::shape_post_dedup(&cfg, &claim(100.0 + cfg.max_view_skew_ticks, 100.0, 5.0)),
            Ok(())
        );
    }

    #[test]
    fn accept_reason_mirrors_can_eat_short_circuit() {
        // Boost wins even when also heavier (invincible is the first arm).
        assert_eq!(accept_reason(true, 10, 5), "boost");
        assert_eq!(accept_reason(true, 1, 5), "boost");
        // Not invincible but strictly heavier → score.
        assert_eq!(accept_reason(false, 10, 5), "score");
        // Neither arm → "none" (would only appear on a reject; can_eat is false here).
        assert_eq!(accept_reason(false, 5, 5), "none");
        assert_eq!(accept_reason(false, 4, 5), "none");
        assert!(!can_eat(false, 5, 5) && accept_reason(false, 5, 5) == "none");
    }

    #[test]
    fn attacker_eligibility_checks_alive_then_position() {
        let cfg = ClaimFairnessConfig::sample();
        assert_eq!(EatClaimPolicy::attacker_eligible(&cfg, false, 0.0), Err("attacker_not_alive_at_claim"));
        assert_eq!(
            EatClaimPolicy::attacker_eligible(&cfg, true, cfg.position_tolerance_px + 0.1),
            Err("attacker_position_mismatch")
        );
        assert_eq!(EatClaimPolicy::attacker_eligible(&cfg, true, cfg.position_tolerance_px), Ok(()));
    }

    fn enemy_rules() -> EnemyEatRules {
        EnemyEatRules {
            respawned_at: ServerTick::new(0),
            target_floor: ServerTick::new(100),
            attacker_invincible: false,
            attacker_score: 10,
            enemy_score: 5,
            reconstructed: 4.0,
            claimed_generation: None,
            current_generation: 0,
        }
    }

    #[test]
    fn enemy_eat_full_ladder() {
        let cfg = ClaimFairnessConfig::sample();
        // Happy path.
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &enemy_rules()), Ok(()));
        // Already respawned past the targeted frame.
        let mut r = enemy_rules();
        r.respawned_at = ServerTick::new(101);
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Err("enemy_already_respawned"));
        // Inside the post-respawn grace window.
        let mut r = enemy_rules();
        r.respawned_at = ServerTick::new(98); // target_floor 100 <= 98 + 3
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Err("enemy_respawn_grace"));
        // Too light and not invincible.
        let mut r = enemy_rules();
        r.attacker_score = 5;
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Err("attacker_cannot_eat_enemy"));
        // Invincible can eat an equal/heavier enemy.
        let mut r = enemy_rules();
        r.attacker_score = 5;
        r.attacker_invincible = true;
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Ok(()));
        // Reconstructed overlap just out of range.
        let mut r = enemy_rules();
        r.reconstructed = cfg.reconstruct_overlap_radius_px() + 0.1;
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Err("server_reconstruct_no_overlap"));
    }

    #[test]
    fn enemy_eat_generation_guard() {
        let cfg = ClaimFairnessConfig::sample();
        // Client named a generation that matches the live enemy → proceeds (here: ok).
        let mut r = enemy_rules();
        r.claimed_generation = Some(3);
        r.current_generation = 3;
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Ok(()));
        // Named a STALE generation (enemy respawned since) → rejected before any tick
        // heuristic, even though the rest of the claim is otherwise valid.
        let mut r = enemy_rules();
        r.claimed_generation = Some(2);
        r.current_generation = 3;
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Err("enemy_generation_mismatch"));
        // Legacy client (None) → no explicit check, falls through to the tick guards.
        let mut r = enemy_rules();
        r.claimed_generation = None;
        r.current_generation = 9;
        assert_eq!(EatClaimPolicy::enemy_eat(&cfg, &r), Ok(()));
    }

    fn player_rules() -> PlayerEatRules {
        PlayerEatRules {
            victim_now_alive: true,
            victim_now_spawn_protected: false,
            victim_hist_alive: true,
            victim_hist_spawn_protected: false,
            victim_hist_invincible: false,
            in_safe_zone: false,
            attacker_invincible: false,
            attacker_score: 10,
            victim_score: 5,
            reconstructed: 4.0,
        }
    }

    #[test]
    fn player_eat_full_ladder() {
        let cfg = ClaimFairnessConfig::sample();
        assert_eq!(EatClaimPolicy::player_eat(&cfg, &player_rules()), Ok(()));
        let mut r = player_rules();
        r.victim_now_spawn_protected = true;
        assert_eq!(EatClaimPolicy::player_eat(&cfg, &r), Err("victim_not_available_now"));
        let mut r = player_rules();
        r.victim_hist_invincible = true;
        assert_eq!(EatClaimPolicy::player_eat(&cfg, &r), Err("victim_protected_at_target_tick"));
        let mut r = player_rules();
        r.in_safe_zone = true;
        assert_eq!(EatClaimPolicy::player_eat(&cfg, &r), Err("safe_zone"));
        let mut r = player_rules();
        r.attacker_score = 5;
        assert_eq!(EatClaimPolicy::player_eat(&cfg, &r), Err("attacker_cannot_eat_player"));
        let mut r = player_rules();
        r.reconstructed = cfg.reconstruct_overlap_radius_px() + 0.1;
        assert_eq!(EatClaimPolicy::player_eat(&cfg, &r), Err("server_reconstruct_no_overlap"));
    }
}
