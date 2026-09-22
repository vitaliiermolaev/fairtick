//! Filler internal eats (lead review 2026-06-11, P0 #1).
//!
//! A filler is a server-driven fake player with NO client, so it can never send the
//! `EatClaim` a human attacker would — and without one, a strong filler that visibly runs
//! over a weak human does NOTHING. "He caught me but didn't eat me" is the loudest bot
//! tell there is. This policy is the server-side stand-in for the missing client claim:
//! it decides WHEN a filler's overlap may become an eat; the Room then applies the result
//! through the exact same `PlayerEaten` → respawn path an accepted human claim uses.
//!
//! TIMELINE (explicit, per the project's fairness rules): both positions are CURRENT
//! SERVER-TICK positions. A filler has no render/interpolation timeline of its own, so
//! there is nothing to reconstruct on the attacker side; the VICTIM, however, renders the
//! attacker slightly in the past. The compensation here is deliberate conservatism, not
//! reconstruction:
//!   * the overlap must persist `sustained_ticks` consecutive server ticks (a real glide-
//!     over, not a one-tick brush the victim may never see);
//!   * a HUMAN victim must be claim-ready (visual world trusted + clean pong + admission
//!     shield expired) and past the same post-admission age gate human claims obey —
//!     a filler must never kill what a freshly (re)joined client hasn't even drawn yet;
//!   * spawn protection, boost invincibility, resume shield, and safe zones block the eat
//!     outright, same as the human-claim policy;
//!   * each filler has an eat COOLDOWN, so an aggressive bot cannot chain-vacuum a lobby.
//!
//! The policy is pure bookkeeping + verdicts: no Room access, no telemetry, no side
//! effects (CLAUDE.md: policies decide, the room executes, telemetry observes).

use std::collections::HashMap;

/// Tunables. Constructed by the Room from `claim_fairness` (the overlap radius is the
/// SAME `visible_overlap_radius_px` human claims validate against — one source of truth)
/// plus the filler-specific conservatism knobs below.
#[derive(Debug, Clone)]
pub struct FillerEatRules {
    /// Max server-tick attacker↔victim distance that counts as an overlap.
    pub overlap_radius_px: f32,
    /// Consecutive overlap ticks required before the eat may fire (lead: 4–8).
    pub sustained_ticks: u32,
    /// Ticks after a successful eat during which this attacker cannot eat again.
    pub attacker_cooldown_ticks: u64,
    /// A HUMAN victim must be at least this many ticks past its admission (mirrors the
    /// human-claim `too_soon_after_admission` gate).
    pub min_victim_admission_age_ticks: u64,
}

/// Everything the policy needs to know about one attacker→victim overlap this tick.
/// Owned data (no borrows into Room state) so the call site stays borrow-trivial.
#[derive(Debug, Clone)]
pub struct FillerEatCandidate {
    pub victim_id: String,
    pub victim_life_id: u32,
    pub dist_px: f32,
    /// Victim drives a real client (gates claim-readiness/admission-age checks; filler
    /// victims have no client and skip those two).
    pub victim_is_human: bool,
    pub victim_alive: bool,
    pub victim_spawn_protected: bool,
    pub victim_invincible: bool,
    pub victim_resume_shielded: bool,
    pub victim_claim_ready: bool,
    pub victim_admission_age_ticks: u64,
    pub either_in_safe_zone: bool,
    pub attacker_score: u32,
    pub victim_score: u32,
}

/// One tick's verdict for one attacker.
#[derive(Debug, PartialEq, Eq)]
pub enum FillerEatVerdict {
    /// No overlap / run still accumulating / cooling down — nothing to report.
    NotYet,
    /// The run reached the sustained threshold but a gate blocks the eat. Emitted at most
    /// once per (run, reason) so telemetry isn't spammed every tick the block persists.
    Reject(&'static str),
    /// All gates passed over a sustained overlap: the Room applies the eat now.
    Eat,
}

/// A consecutive-overlap run: one attacker glued to one victim LIFE. Victim change,
/// life change (respawn), or a gap tick resets it — symmetric with how the human-claim
/// path life-scopes kills.
#[derive(Debug)]
struct Run {
    victim_id: String,
    victim_life_id: u32,
    ticks: u32,
    last_tick: u64,
    /// Last reject reason already surfaced for this run (dedup for telemetry).
    reported_reject: Option<&'static str>,
}

#[derive(Debug)]
pub struct FillerEatPolicy {
    rules: FillerEatRules,
    runs: HashMap<String, Run>,
    cooldown_until: HashMap<String, u64>,
}

impl FillerEatPolicy {
    pub fn new(rules: FillerEatRules) -> Self {
        Self { rules, runs: HashMap::new(), cooldown_until: HashMap::new() }
    }

    pub fn rules(&self) -> &FillerEatRules {
        &self.rules
    }

    /// Advance one attacker's run with this tick's best overlap candidate (None = no
    /// overlapping weaker player this tick) and return the verdict.
    pub fn evaluate(
        &mut self,
        tick: u64,
        attacker_id: &str,
        candidate: Option<&FillerEatCandidate>,
    ) -> FillerEatVerdict {
        // Cooling down: no accumulation at all — after the cooldown the attacker must
        // earn a FRESH sustained overlap (no banked ticks from during the cooldown).
        if self.cooldown_until.get(attacker_id).is_some_and(|until| tick < *until) {
            self.runs.remove(attacker_id);
            return FillerEatVerdict::NotYet;
        }

        let Some(c) = candidate else {
            self.runs.remove(attacker_id);
            return FillerEatVerdict::NotYet;
        };
        debug_assert!(c.dist_px <= self.rules.overlap_radius_px, "caller pre-filters overlap");

        // Continue or restart the run. A run is (victim, life)-scoped and must be
        // CONSECUTIVE: any gap tick (no candidate → removal above) restarts it.
        let run = self.runs.entry(attacker_id.to_string()).or_insert(Run {
            victim_id: c.victim_id.clone(),
            victim_life_id: c.victim_life_id,
            ticks: 0,
            last_tick: tick,
            reported_reject: None,
        });
        if run.victim_id != c.victim_id
            || run.victim_life_id != c.victim_life_id
            || tick.saturating_sub(run.last_tick) > 1
        {
            run.victim_id = c.victim_id.clone();
            run.victim_life_id = c.victim_life_id;
            run.ticks = 0;
            run.reported_reject = None;
        }
        run.last_tick = tick;
        run.ticks = run.ticks.saturating_add(1);

        if run.ticks < self.rules.sustained_ticks {
            return FillerEatVerdict::NotYet;
        }

        // Sustained overlap reached: gates, hardest-fact first. Order matters only for
        // which reason gets reported; all of them block.
        let blocked: Option<&'static str> = if !c.victim_alive {
            Some("victim_dead")
        } else if c.either_in_safe_zone {
            Some("safe_zone")
        } else if c.victim_spawn_protected {
            Some("victim_spawn_protected")
        } else if c.victim_invincible {
            Some("victim_invincible")
        } else if c.victim_resume_shielded {
            Some("victim_resume_shield")
        } else if c.attacker_score <= c.victim_score {
            Some("score_not_greater")
        } else if c.victim_is_human && !c.victim_claim_ready {
            Some("victim_not_claim_ready")
        } else if c.victim_is_human
            && c.victim_admission_age_ticks < self.rules.min_victim_admission_age_ticks
        {
            Some("victim_too_recently_admitted")
        } else {
            None
        };

        match blocked {
            Some(reason) => {
                if run.reported_reject == Some(reason) {
                    FillerEatVerdict::NotYet // already surfaced; stay quiet while it persists
                } else {
                    run.reported_reject = Some(reason);
                    FillerEatVerdict::Reject(reason)
                }
            }
            None => FillerEatVerdict::Eat,
        }
    }

    /// The Room applied an eat for this attacker: arm its cooldown and clear the run.
    pub fn note_eat_applied(&mut self, tick: u64, attacker_id: &str) {
        self.runs.remove(attacker_id);
        self.cooldown_until.insert(attacker_id.to_string(), tick + self.rules.attacker_cooldown_ticks);
    }

    /// Drop all bookkeeping that references a removed player (either role).
    pub fn forget_player(&mut self, player_id: &str) {
        self.runs.remove(player_id);
        self.cooldown_until.remove(player_id);
        self.runs.retain(|_, run| run.victim_id != player_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> FillerEatRules {
        FillerEatRules {
            overlap_radius_px: 18.0,
            sustained_ticks: 6,
            attacker_cooldown_ticks: 240,
            min_victim_admission_age_ticks: 60,
        }
    }

    fn clean_candidate() -> FillerEatCandidate {
        FillerEatCandidate {
            victim_id: "p_victim".into(),
            victim_life_id: 0,
            dist_px: 5.0,
            victim_is_human: true,
            victim_alive: true,
            victim_spawn_protected: false,
            victim_invincible: false,
            victim_resume_shielded: false,
            victim_claim_ready: true,
            victim_admission_age_ticks: 600,
            either_in_safe_zone: false,
            attacker_score: 100,
            victim_score: 10,
        }
    }

    #[test]
    fn sustained_overlap_required_before_eat() {
        let mut p = FillerEatPolicy::new(rules());
        let c = clean_candidate();
        for t in 1..6 {
            assert_eq!(p.evaluate(t, "f1", Some(&c)), FillerEatVerdict::NotYet, "tick {t}");
        }
        assert_eq!(p.evaluate(6, "f1", Some(&c)), FillerEatVerdict::Eat);
    }

    #[test]
    fn gap_tick_resets_the_run() {
        let mut p = FillerEatPolicy::new(rules());
        let c = clean_candidate();
        for t in 1..6 {
            p.evaluate(t, "f1", Some(&c));
        }
        assert_eq!(p.evaluate(7, "f1", None), FillerEatVerdict::NotYet); // contact broke
        for t in 8..13 {
            assert_eq!(p.evaluate(t, "f1", Some(&c)), FillerEatVerdict::NotYet, "tick {t}");
        }
        assert_eq!(p.evaluate(13, "f1", Some(&c)), FillerEatVerdict::Eat);
    }

    #[test]
    fn victim_respawn_mid_run_resets_via_life_id() {
        let mut p = FillerEatPolicy::new(rules());
        let mut c = clean_candidate();
        for t in 1..6 {
            p.evaluate(t, "f1", Some(&c));
        }
        c.victim_life_id = 1; // the victim died to someone else and respawned in place
        assert_eq!(p.evaluate(6, "f1", Some(&c)), FillerEatVerdict::NotYet, "run must restart");
    }

    #[test]
    fn cooldown_blocks_and_then_requires_fresh_overlap() {
        let mut p = FillerEatPolicy::new(rules());
        let c = clean_candidate();
        for t in 1..=6 {
            p.evaluate(t, "f1", Some(&c));
        }
        p.note_eat_applied(6, "f1");
        // Overlapping the whole cooldown accumulates NOTHING.
        for t in 7..246 {
            assert_eq!(p.evaluate(t, "f1", Some(&c)), FillerEatVerdict::NotYet, "tick {t}");
        }
        // Cooldown over (6+240=246): a fresh sustained run is still required.
        for t in 246..251 {
            assert_eq!(p.evaluate(t, "f1", Some(&c)), FillerEatVerdict::NotYet, "tick {t}");
        }
        assert_eq!(p.evaluate(251, "f1", Some(&c)), FillerEatVerdict::Eat);
    }

    #[test]
    fn human_victim_gates_block_and_report_once() {
        let mut p = FillerEatPolicy::new(rules());
        let mut c = clean_candidate();
        c.victim_claim_ready = false;
        for t in 1..6 {
            assert_eq!(p.evaluate(t, "f1", Some(&c)), FillerEatVerdict::NotYet);
        }
        assert_eq!(p.evaluate(6, "f1", Some(&c)), FillerEatVerdict::Reject("victim_not_claim_ready"));
        // Same persisting block: silent (no telemetry spam).
        assert_eq!(p.evaluate(7, "f1", Some(&c)), FillerEatVerdict::NotYet);
        // Gate clears while the overlap persists: the eat fires without a new run.
        c.victim_claim_ready = true;
        assert_eq!(p.evaluate(8, "f1", Some(&c)), FillerEatVerdict::Eat);
    }

    #[test]
    fn filler_victim_skips_client_readiness_gates() {
        let mut p = FillerEatPolicy::new(rules());
        let mut c = clean_candidate();
        c.victim_is_human = false;
        c.victim_claim_ready = false; // fillers never have a client to be "ready"
        c.victim_admission_age_ticks = 0;
        for t in 1..6 {
            p.evaluate(t, "f1", Some(&c));
        }
        assert_eq!(p.evaluate(6, "f1", Some(&c)), FillerEatVerdict::Eat);
    }

    type GateCase = (fn(&mut FillerEatCandidate), &'static str);

    #[test]
    fn hard_gates_block_regardless_of_victim_kind() {
        let cases: Vec<GateCase> = vec![
            (|c| c.victim_spawn_protected = true, "victim_spawn_protected"),
            (|c| c.victim_invincible = true, "victim_invincible"),
            (|c| c.victim_resume_shielded = true, "victim_resume_shield"),
            (|c| c.either_in_safe_zone = true, "safe_zone"),
            (|c| c.victim_score = 100, "score_not_greater"),
        ];
        for (mutate, want) in cases {
            let mut p = FillerEatPolicy::new(rules());
            let mut c = clean_candidate();
            mutate(&mut c);
            for t in 1..6 {
                p.evaluate(t, "f1", Some(&c));
            }
            assert_eq!(p.evaluate(6, "f1", Some(&c)), FillerEatVerdict::Reject(want));
        }
    }
}
