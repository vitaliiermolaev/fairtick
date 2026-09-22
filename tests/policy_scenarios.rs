//! Replays tests/fixtures/policy_scenarios.json against the pure gameplay policies
//! (src/game/policies). These are named given/when/then regressions for the most
//! expensive bug classes — unfair death and bad eat-claim accept/reject — kept in a
//! language-neutral fixture so the same cases can later be shared with the Unity client.
//!
//! If a policy's rule or a reject-reason string changes intentionally, update the
//! fixture in the same commit.

use fairtick::game::policies::eat_claim::{EatClaimPolicy, EnemyEatRules, PlayerEatRules};
use fairtick::game::policies::enemy_contact::{ContactDecision, EnemyContactContext, EnemyContactPolicy};
use fairtick::game::policies::enemy_death_candidate::{
    DeathDecision, DelayReason, EnemyDeathCandidatePolicy, EnemyDeathContext,
};
use fairtick::game::timeline::ServerTick;
use fairtick::protocol::{EatClaim, EatTargetKind, Position};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
#[serde(tag = "policy")]
enum Scenario {
    #[serde(rename = "enemy_contact")]
    EnemyContact {
        name: String,
        server_dist: f32,
        kill_radius: f32,
        contact_ticks: u32,
        contact_ticks_required: u32,
        shielded: bool,
        expect: String,
    },
    #[serde(rename = "enemy_death")]
    EnemyDeath {
        name: String,
        server_dist: f32,
        /// Reconstructed enemy-visible distance; `null` = no history to reconstruct.
        reconstructed_enemy_visible_dist: Option<f32>,
        interp_delay_ticks: u64,
        kill_radius: f32,
        contact_ticks_server: u32,
        contact_ticks_required: u32,
        /// `null` = observe-only gate; a number = the armed visible-confirm RADIUS (NOT the
        /// kill radius — the sprite-overlap distance + grace).
        visible_confirm_radius_px: Option<f32>,
        /// policy v4: victim projected forward by the client lead vs the reconstructed enemy.
        /// Absent/`null` = no lead sample → the gate falls back to reconstructed-only.
        #[serde(default)]
        victim_projected_visible_dist: Option<f32>,
        /// policy v5: the client lead used for the projection. Absent/`null` → the v5
        /// prediction-uncertainty guard never fires.
        #[serde(default)]
        victim_projected_lead_ticks: Option<f64>,
        expect: String,
    },
    #[serde(rename = "enemy_eat")]
    EnemyEat {
        name: String,
        respawned_at: u64,
        target_floor: u64,
        attacker_invincible: bool,
        attacker_score: u32,
        enemy_score: u32,
        reconstructed: f32,
        /// Optional in the fixture (older cases omit them) → legacy None / 0 generation.
        #[serde(default)]
        claimed_generation: Option<u32>,
        #[serde(default)]
        current_generation: u32,
        expect: String,
    },
    #[serde(rename = "player_eat")]
    PlayerEat {
        name: String,
        victim_now_alive: bool,
        victim_now_spawn_protected: bool,
        victim_hist_alive: bool,
        victim_hist_spawn_protected: bool,
        victim_hist_invincible: bool,
        in_safe_zone: bool,
        attacker_invincible: bool,
        attacker_score: u32,
        victim_score: u32,
        reconstructed: f32,
        expect: String,
    },
    #[serde(rename = "shape")]
    Shape {
        name: String,
        attacker_id: String,
        target_id: String,
        attacker_render_tick: f64,
        target_render_tick: f64,
        visual_distance: f32,
        expect: String,
    },
}

fn check_result(result: Result<(), &'static str>) -> String {
    match result {
        Ok(()) => "ok".to_string(),
        Err(reason) => reason.to_string(),
    }
}

#[test]
fn policy_scenarios_match_fixture() {
    let raw = include_str!("fixtures/policy_scenarios.json");
    let fixture: Fixture = serde_json::from_str(raw).expect("policy_scenarios.json parses");
    assert!(!fixture.scenarios.is_empty(), "fixture has scenarios");

    // Claim-validation tolerances come from the shipped config now (review #3 — single source),
    // so the eat-claim scenarios validate against the same numbers the live server uses.
    let cf = fairtick::config_shared::load_config_from_str(include_str!("../gameplay_config.toml"))
        .expect("gameplay_config.toml parses")
        .claim_fairness;

    for scenario in &fixture.scenarios {
        match scenario {
            Scenario::EnemyContact {
                name,
                server_dist,
                kill_radius,
                contact_ticks,
                contact_ticks_required,
                shielded,
                expect,
            } => {
                let decision = EnemyContactPolicy::evaluate(&EnemyContactContext {
                    server_tick: ServerTick::new(0),
                    server_dist: *server_dist,
                    kill_radius: *kill_radius,
                    contact_ticks: *contact_ticks,
                    contact_ticks_required: *contact_ticks_required,
                    shielded: *shielded,
                });
                let got = match decision {
                    ContactDecision::NoContact => "NoContact",
                    ContactDecision::TrackContact => "TrackContact",
                    ContactDecision::KillPlayer { .. } => "KillPlayer",
                };
                assert_eq!(got, expect, "enemy_contact scenario '{name}'");
            }
            Scenario::EnemyDeath {
                name,
                server_dist,
                reconstructed_enemy_visible_dist,
                interp_delay_ticks,
                kill_radius,
                contact_ticks_server,
                contact_ticks_required,
                visible_confirm_radius_px,
                victim_projected_visible_dist,
                victim_projected_lead_ticks,
                expect,
            } => {
                let decision = EnemyDeathCandidatePolicy::evaluate(&EnemyDeathContext {
                    server_tick: ServerTick::new(0),
                    reconstructed_enemy_tick: ServerTick::new(0),
                    server_dist: *server_dist,
                    reconstructed_enemy_visible_dist: *reconstructed_enemy_visible_dist,
                    interp_delay_ticks: *interp_delay_ticks,
                    kill_radius: *kill_radius,
                    contact_ticks_server: *contact_ticks_server,
                    contact_ticks_required: *contact_ticks_required,
                    visible_confirm_radius_px: *visible_confirm_radius_px,
                    victim_projected_visible_dist: *victim_projected_visible_dist,
                    victim_projected_lead_ticks: *victim_projected_lead_ticks,
                });
                let got = match decision {
                    DeathDecision::NoServerContact => "NoServerContact",
                    DeathDecision::Track => "Track",
                    DeathDecision::Delay { reason: DelayReason::VisibleGap } => "DelayVisibleGap",
                    DeathDecision::Delay { reason: DelayReason::NoVisibleHistory } => "DelayNoVisibleHistory",
                    DeathDecision::Delay { reason: DelayReason::PredictionUncertain } => {
                        "DelayPredictionUncertain"
                    }
                    DeathDecision::Kill { .. } => "Kill",
                };
                assert_eq!(got, expect, "enemy_death scenario '{name}'");
            }
            Scenario::EnemyEat {
                name,
                respawned_at,
                target_floor,
                attacker_invincible,
                attacker_score,
                enemy_score,
                reconstructed,
                claimed_generation,
                current_generation,
                expect,
            } => {
                let got = check_result(EatClaimPolicy::enemy_eat(
                    &cf,
                    &EnemyEatRules {
                        respawned_at: ServerTick::new(*respawned_at),
                        target_floor: ServerTick::new(*target_floor),
                        attacker_invincible: *attacker_invincible,
                        attacker_score: *attacker_score,
                        enemy_score: *enemy_score,
                        reconstructed: *reconstructed,
                        claimed_generation: *claimed_generation,
                        current_generation: *current_generation,
                    },
                ));
                assert_eq!(&got, expect, "enemy_eat scenario '{name}'");
            }
            Scenario::PlayerEat {
                name,
                victim_now_alive,
                victim_now_spawn_protected,
                victim_hist_alive,
                victim_hist_spawn_protected,
                victim_hist_invincible,
                in_safe_zone,
                attacker_invincible,
                attacker_score,
                victim_score,
                reconstructed,
                expect,
            } => {
                let got = check_result(EatClaimPolicy::player_eat(
                    &cf,
                    &PlayerEatRules {
                        victim_now_alive: *victim_now_alive,
                        victim_now_spawn_protected: *victim_now_spawn_protected,
                        victim_hist_alive: *victim_hist_alive,
                        victim_hist_spawn_protected: *victim_hist_spawn_protected,
                        victim_hist_invincible: *victim_hist_invincible,
                        in_safe_zone: *in_safe_zone,
                        attacker_invincible: *attacker_invincible,
                        attacker_score: *attacker_score,
                        victim_score: *victim_score,
                        reconstructed: *reconstructed,
                    },
                ));
                assert_eq!(&got, expect, "player_eat scenario '{name}'");
            }
            Scenario::Shape {
                name,
                attacker_id,
                target_id,
                attacker_render_tick,
                target_render_tick,
                visual_distance,
                expect,
            } => {
                let claim = EatClaim {
                    claim_id: 1,
                    target_kind: EatTargetKind::Enemy,
                    target_id: target_id.clone(),
                    attacker_render_tick: *attacker_render_tick,
                    target_render_tick: *target_render_tick,
                    attacker_position: Position { x: 0.0, y: 0.0 },
                    target_position: Position { x: 0.0, y: 0.0 },
                    visual_distance: *visual_distance,
                    target_generation: None,
                };
                // Same order the Room uses: pre-dedup shape, then post-dedup shape.
                let got = check_result(
                    EatClaimPolicy::shape_pre_dedup(&claim, attacker_id)
                        .and_then(|_| EatClaimPolicy::shape_post_dedup(&cf, &claim)),
                );
                assert_eq!(&got, expect, "shape scenario '{name}'");
            }
        }
    }
}
