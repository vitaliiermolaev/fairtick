//! EnemyDeathClaimPolicy — the claim-based enemy-eats-player rule, the SYMMETRIC counterpart of
//! [`super::eat_claim::EatClaimPolicy`]. Instead of the server guessing what the victim's phone
//! drew (the v4/v5 projection guesswork), the victim's client reports the render ticks + visible
//! positions it actually drew; the Room reconstructs those from contact history and this policy
//! decides — purely — whether the claimed death is consistent and lethal.
//!
//! Two phases, like the eat policy:
//!   * [`shape`] — needs only the claim (visual radius + render-skew geometry).
//!   * [`validate`] — needs the gathered history frames (liveness, shield, safe zone, score,
//!     reconstructed overlap).
//!
//! The reject reasons ARE the contract — the client logs them and resyncs. (lead manifesto)

// The visual-overlap radius, position tolerance, reconstruct-overlap radius and visual-distance
// tolerance USED to be duplicated here as consts (18 / 12 / 26 / 1) — byte-identical to the eat
// policy's, which is exactly how the two could silently drift. They now come from the shared
// [`ClaimFairnessConfig`] (config single source of truth, folded into config_hash). (review #3)
use crate::config_shared::ClaimFairnessConfig;

/// Substantive inputs gathered from the victim's + enemy's reconstructed history frames.
pub struct EnemyDeathClaimRules {
    /// Victim is alive on the CURRENT server tick (can't kill a player who already died/respawned).
    pub victim_now_alive: bool,
    /// Victim is spawn-protected RIGHT NOW — a stale claim against a previous life must not kill
    /// the freshly-respawned (protected) one. Mirrors EatClaimPolicy::player_eat. (lead review v6)
    pub victim_now_spawn_protected: bool,
    /// Victim was alive at the claimed render tick.
    pub victim_hist_alive: bool,
    /// Victim was spawn-protected at the claimed render tick (can't be eaten).
    pub victim_hist_spawn_protected: bool,
    /// Victim was boost-invincible at the claimed render tick (can't be eaten).
    pub victim_hist_invincible: bool,
    /// Either entity was inside the safe zone at the claimed render tick.
    pub in_safe_zone: bool,
    /// Enemy / victim scores at the claimed render tick — the enemy must outscore the victim to eat it.
    pub enemy_score: u32,
    pub victim_score: u32,
    /// Reconstructed victim↔enemy distance at the claimed render ticks.
    pub reconstructed: f32,
}

pub struct EnemyDeathClaimPolicy;

impl EnemyDeathClaimPolicy {
    /// Phase 1 — checks needing only the claim itself. `max_render_skew_ticks` is the largest
    /// HONEST `victim_render_tick - enemy_render_tick` the caller will accept: the victim renders its
    /// own avatar AHEAD (predicted) and the enemy BEHIND (interpolated), so the skew is roughly
    /// `client_prediction_lead + enemy_interp_delay`. It is caller-provided (config-derived in Room)
    /// rather than a const so it can't drift below the configured prediction lead + interp delay and
    /// wrongly reject an honest high-RTT claim. (round-3 follow-up)
    pub fn shape(
        cfg: &ClaimFairnessConfig,
        visual_distance: f32,
        victim_render_tick: f64,
        enemy_render_tick: f64,
        max_render_skew_ticks: f64,
    ) -> Result<(), &'static str> {
        if !visual_distance.is_finite() || !victim_render_tick.is_finite() || !enemy_render_tick.is_finite() {
            return Err("nan_claim");
        }
        if !(0.0..=cfg.visible_overlap_radius_px).contains(&visual_distance) {
            // The victim didn't actually see an overlap → not a visible death.
            return Err("visual_too_far");
        }
        // The victim is predicted ahead of the interpolated enemy; the reverse is implausible.
        if victim_render_tick < enemy_render_tick {
            return Err("victim_behind_enemy");
        }
        if victim_render_tick - enemy_render_tick > max_render_skew_ticks {
            return Err("skew_too_large");
        }
        Ok(())
    }

    /// Reported vs reconstructed position agreement (per entity).
    pub fn within_position_tolerance(cfg: &ClaimFairnessConfig, dist: f32) -> bool {
        dist <= cfg.position_tolerance_px
    }

    /// The claim's reported `visual_distance` must match the geometry of its reported positions,
    /// and those positions must themselves be a visible overlap — so a forged `visual_distance`
    /// can't sneak a far-apart kill past the shape gate. `reported_dist` =
    /// distance(victim_position, enemy_position).
    pub fn positions_consistent(
        cfg: &ClaimFairnessConfig,
        reported_dist: f32,
        visual_distance: f32,
    ) -> Result<(), &'static str> {
        if (reported_dist - visual_distance).abs() > cfg.visual_distance_tolerance_px {
            return Err("visual_distance_mismatch");
        }
        if reported_dist > cfg.visible_overlap_radius_px {
            return Err("reported_positions_too_far");
        }
        Ok(())
    }

    /// Phase 2 — substantive rules from the gathered history. First failing rule wins (order
    /// mirrors the gate: liveness → shield → safe zone → eligibility → reconstructed overlap).
    pub fn validate(cfg: &ClaimFairnessConfig, rules: &EnemyDeathClaimRules) -> Result<(), &'static str> {
        if !rules.victim_now_alive {
            return Err("victim_not_alive_now");
        }
        // A stale claim must not kill the victim's NEW (spawn-protected) life — the double-kill guard.
        if rules.victim_now_spawn_protected {
            return Err("victim_spawn_protected_now");
        }
        if !rules.victim_hist_alive {
            return Err("victim_not_alive_at_claim");
        }
        if rules.victim_hist_spawn_protected {
            return Err("victim_spawn_protected");
        }
        if rules.victim_hist_invincible {
            return Err("victim_invincible");
        }
        if rules.in_safe_zone {
            return Err("safe_zone");
        }
        if rules.enemy_score <= rules.victim_score {
            return Err("enemy_not_bigger");
        }
        if rules.reconstructed > cfg.reconstruct_overlap_radius_px() {
            return Err("reconstructed_too_far");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_rules() -> EnemyDeathClaimRules {
        EnemyDeathClaimRules {
            victim_now_alive: true,
            victim_now_spawn_protected: false,
            victim_hist_alive: true,
            victim_hist_spawn_protected: false,
            victim_hist_invincible: false,
            in_safe_zone: false,
            enemy_score: 100,
            victim_score: 10,
            reconstructed: 12.0,
        }
    }

    #[test]
    fn shape_accepts_visible_overlap_with_forward_skew() {
        let cfg = ClaimFairnessConfig::sample();
        // Accepts right up to the caller's max skew (here 44 = e.g. lead 36 + interp 8).
        assert_eq!(EnemyDeathClaimPolicy::shape(&cfg, 15.0, 1000.0, 992.0, 44.0), Ok(()));
        assert_eq!(
            EnemyDeathClaimPolicy::shape(&cfg, 15.0, 1000.0, 956.0, 44.0),
            Ok(()),
            "skew == max accepts"
        );
    }

    #[test]
    fn shape_rejects_non_overlap_and_bad_geometry() {
        let cfg = ClaimFairnessConfig::sample();
        assert_eq!(EnemyDeathClaimPolicy::shape(&cfg, 20.0, 1000.0, 992.0, 44.0), Err("visual_too_far"));
        assert_eq!(EnemyDeathClaimPolicy::shape(&cfg, 5.0, 990.0, 992.0, 44.0), Err("victim_behind_enemy"));
        // Skew beyond the caller's max → rejected (here 49 > 44).
        assert_eq!(EnemyDeathClaimPolicy::shape(&cfg, 5.0, 1041.0, 992.0, 44.0), Err("skew_too_large"));
        assert_eq!(EnemyDeathClaimPolicy::shape(&cfg, f32::NAN, 1000.0, 992.0, 44.0), Err("nan_claim"));
    }

    #[test]
    fn position_tolerance_band() {
        let cfg = ClaimFairnessConfig::sample();
        assert!(EnemyDeathClaimPolicy::within_position_tolerance(&cfg, 12.0));
        assert!(!EnemyDeathClaimPolicy::within_position_tolerance(&cfg, 12.1));
    }

    #[test]
    fn validate_accepts_clean_lethal_claim() {
        let cfg = ClaimFairnessConfig::sample();
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &ok_rules()), Ok(()));
    }

    #[test]
    fn positions_consistent_guards_forged_visual_distance() {
        let cfg = ClaimFairnessConfig::sample();
        // Reported visual_distance must match the positions' geometry.
        assert_eq!(EnemyDeathClaimPolicy::positions_consistent(&cfg, 15.0, 15.5), Ok(()));
        assert_eq!(
            EnemyDeathClaimPolicy::positions_consistent(&cfg, 15.0, 0.0),
            Err("visual_distance_mismatch")
        );
        // Positions agree with visual_distance but are not actually overlapping.
        assert_eq!(
            EnemyDeathClaimPolicy::positions_consistent(&cfg, 25.0, 25.0),
            Err("reported_positions_too_far")
        );
    }

    #[test]
    fn validate_rejects_each_rule() {
        let cfg = ClaimFairnessConfig::sample();
        let mut r = ok_rules();
        r.victim_now_alive = false;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("victim_not_alive_now"));

        // Stale claim against a since-respawned (now spawn-protected) life → rejected.
        let mut r = ok_rules();
        r.victim_now_spawn_protected = true;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("victim_spawn_protected_now"));

        let mut r = ok_rules();
        r.victim_hist_alive = false;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("victim_not_alive_at_claim"));

        let mut r = ok_rules();
        r.victim_hist_spawn_protected = true;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("victim_spawn_protected"));

        let mut r = ok_rules();
        r.victim_hist_invincible = true;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("victim_invincible"));

        let mut r = ok_rules();
        r.in_safe_zone = true;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("safe_zone"));

        // Enemy not bigger than the victim → it can't eat it.
        let mut r = ok_rules();
        r.enemy_score = 10;
        r.victim_score = 10;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("enemy_not_bigger"));

        let mut r = ok_rules();
        r.reconstructed = cfg.reconstruct_overlap_radius_px() + 0.1;
        assert_eq!(EnemyDeathClaimPolicy::validate(&cfg, &r), Err("reconstructed_too_far"));
    }
}
