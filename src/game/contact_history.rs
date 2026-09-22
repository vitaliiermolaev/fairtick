//! Contact history — the immutable per-tick ring of who-was-where, used to reconstruct what a
//! client could plausibly have SEEN when validating an eat/death claim against the past it drew.
//!
//! Extracted from the Room god-class (review #1). It owns the ring and the record/sample mechanics;
//! it makes NO gameplay decision (the claim policies decide on the sampled frames). Generation /
//! life ids ride in the frames so a reconstruction for one entity life can never blend in a position
//! from a different life (ids are reused across respawns).

use crate::game::ai::Enemy;
use crate::game::player::Player;
use crate::protocol::{Direction, Position};
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Debug)]
pub(crate) struct PlayerHistoryState {
    pub(crate) position: Position,
    #[allow(dead_code)]
    pub(crate) direction: Direction,
    /// Logged on eat accept/reject (corroborates accept_reason=boost via boosted_speed).
    pub(crate) speed: f32,
    pub(crate) score: u32,
    pub(crate) is_invincible: bool,
    pub(crate) spawn_protected: bool,
    pub(crate) is_alive: bool,
    /// Which life of this player the frame belongs to (bumped on respawn). The claim causal
    /// ledger keys a kill on (id, life_id) so a kill of life N can't reject life N+1's eat. (#1)
    pub(crate) life_id: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct EnemyHistoryState {
    pub(crate) position: Position,
    #[allow(dead_code)]
    pub(crate) direction: Direction,
    #[allow(dead_code)]
    pub(crate) speed: f32,
    pub(crate) score: u32,
    pub(crate) respawned_at_tick: u64,
    /// Life id of this enemy AT this frame. The death reconstruction rejects a frame whose
    /// generation differs from the live enemy — so a generation-1 death can't be confirmed
    /// by a generation-0 position (the player was looking at a different life). (lead review)
    pub(crate) generation: u32,
}

/// One per-tick frame of who-was-where.
#[derive(Clone, Debug)]
struct ContactHistoryFrame {
    tick: u64,
    players: HashMap<String, PlayerHistoryState>,
    enemies: HashMap<String, EnemyHistoryState>,
}

/// The bounded ring of recent frames + the sampler. `max_frames` IS the retention policy (trim
/// target), not just a Vec pre-allocation hint — so the window is one explicit number, not a global
/// const buried in `record`. (review #7)
#[derive(Debug)]
pub(crate) struct ContactHistory {
    ring: VecDeque<ContactHistoryFrame>,
    max_frames: usize,
}

impl ContactHistory {
    pub(crate) fn with_capacity(max_frames: usize) -> Self {
        Self { ring: VecDeque::with_capacity(max_frames), max_frames }
    }

    /// Snapshot everyone's position + gameplay flags THIS tick into the ring (trimmed to the window).
    pub(crate) fn record(&mut self, players: &HashMap<String, Player>, enemies: &[Enemy], tick: u64) {
        let players = players
            .iter()
            .map(|(id, p)| {
                (
                    id.clone(),
                    PlayerHistoryState {
                        position: p.position,
                        direction: p.direction,
                        speed: p.speed,
                        score: p.score,
                        is_invincible: p.is_invincible,
                        spawn_protected: p.is_spawn_protected(tick),
                        is_alive: p.is_alive,
                        life_id: p.life_id,
                    },
                )
            })
            .collect();
        let enemies = enemies
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    EnemyHistoryState {
                        position: e.position,
                        direction: e.direction,
                        speed: e.speed,
                        score: e.score,
                        respawned_at_tick: e.respawned_at_tick,
                        generation: e.generation,
                    },
                )
            })
            .collect();
        self.push_frame(ContactHistoryFrame { tick, players, enemies });
    }

    /// Append a frame and trim to the retention window. `record` builds the frame from live state and
    /// ends here, so a test that calls `push_frame` exercises the EXACT production push+trim path —
    /// if this stops trimming, both `record` and the trim test break together. (review)
    fn push_frame(&mut self, frame: ContactHistoryFrame) {
        self.ring.push_back(frame);
        while self.ring.len() > self.max_frames {
            self.ring.pop_front();
        }
    }

    /// Player position at a (fractional) past tick: position is interpolated between the bracketing
    /// frames; gameplay flags come from the floor frame.
    pub(crate) fn sample_player(&self, player_id: &str, tick: f64) -> Option<PlayerHistoryState> {
        let lo_tick = tick.floor() as u64;
        let hi_tick = tick.ceil() as u64;
        let lo = self.ring.iter().find(|f| f.tick == lo_tick)?.players.get(player_id)?.clone();
        if hi_tick == lo_tick {
            return Some(lo);
        }
        // Ring-edge policy (review): for a FRACTIONAL tick the upper bracketing frame must actually
        // exist. If it doesn't (hi_tick is newer than anything recorded, or a gap), REFUSE rather
        // than fall back to the floor frame as if the entity never moved — a phantom no-move sample
        // is fairness-sensitive. In practice the claim resolver gates a fractional claim until the
        // sim reaches ceil(render_tick) (see process_claims), so for a real claim the hi frame is
        // already recorded; this `None` only fires for an out-of-window / gap sample, which should
        // reject (→ no_*_history) not validate against a guessed coordinate.
        let hi =
            self.ring.iter().find(|f| f.tick == hi_tick).and_then(|f| f.players.get(player_id)).cloned()?;
        // Never interpolate ACROSS a respawn: if the bracketing frames are different lives, the
        // lerped position would be a blend of a dead life and a fresh-spawned one — refuse rather
        // than produce a phantom coordinate that yields confusing position_mismatch / false rejects
        // around the respawn boundary. (review #4)
        if hi.life_id != lo.life_id {
            return None;
        }
        let t = (tick - lo_tick as f64) as f32;
        let mut out = lo.clone();
        out.position = Position {
            x: lo.position.x + (hi.position.x - lo.position.x) * t,
            y: lo.position.y + (hi.position.y - lo.position.y) * t,
        };
        Some(out)
    }

    /// Sample an enemy's position at a (fractional) past `tick`, interpolating between the two
    /// bracketing frames. GENERATION-AWARE: when `generation` is `Some(g)`, a bracketing frame whose
    /// `generation != g` is rejected (→ `None`) BEFORE interpolation, so a reconstruction for one
    /// enemy life can never blend in a position from a different life (ids are reused across
    /// respawns). The check has to live INSIDE the sampler — filtering the interpolated result
    /// afterwards is too late, the position may already mix gen-N and gen-(N+1) coordinates. `None`
    /// = no generation constraint (back-compat: an eat claim from an old client that didn't name a
    /// target generation). (lead review)
    pub(crate) fn sample_enemy(
        &self,
        enemy_id: &str,
        generation: Option<u32>,
        tick: f64,
    ) -> Option<EnemyHistoryState> {
        let lo_tick = tick.floor() as u64;
        let hi_tick = tick.ceil() as u64;
        let lo = self.ring.iter().find(|f| f.tick == lo_tick)?.enemies.get(enemy_id)?.clone();
        // Reject the low frame if it belongs to a different enemy life than asked for.
        if generation.is_some_and(|g| lo.generation != g) {
            return None;
        }
        if hi_tick == lo_tick {
            return Some(lo);
        }
        let hi = match self.ring.iter().find(|f| f.tick == hi_tick).and_then(|f| f.enemies.get(enemy_id)) {
            // A present high frame from a DIFFERENT life can't bracket this interpolation —
            // refuse rather than blend two lives' positions across the respawn.
            Some(h) if generation.is_some_and(|g| h.generation != g) => return None,
            Some(h) => h.clone(),
            // No high frame at all (ring edge / gap): REFUSE rather than fall back to the low frame
            // as if the enemy never moved — a phantom no-move sample is fairness-sensitive. The claim
            // resolver gates a fractional claim until the sim reaches ceil(render_tick), so a real
            // claim's hi frame exists; this `None` only fires for an out-of-window sample. (review)
            None => return None,
        };
        // Never blend across a respawn, even when no generation was requested (back-compat old
        // claims): a lerp between two enemy lives is a phantom position. (review #4)
        if hi.generation != lo.generation {
            return None;
        }
        let t = (tick - lo_tick as f64) as f32;
        let mut out = lo.clone();
        out.position = Position {
            x: lo.position.x + (hi.position.x - lo.position.x) * t,
            y: lo.position.y + (hi.position.y - lo.position.y) * t,
        };
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player_frame(tick: u64, id: &str, x: f32, life_id: u32) -> ContactHistoryFrame {
        let mut players = HashMap::new();
        players.insert(
            id.to_string(),
            PlayerHistoryState {
                position: Position { x, y: 0.0 },
                direction: Direction::Right,
                speed: 0.0,
                score: 0,
                is_invincible: false,
                spawn_protected: false,
                is_alive: true,
                life_id,
            },
        );
        ContactHistoryFrame { tick, players, enemies: HashMap::new() }
    }

    fn enemy_frame(tick: u64, id: &str, x: f32, generation: u32) -> ContactHistoryFrame {
        let mut enemies = HashMap::new();
        enemies.insert(
            id.to_string(),
            EnemyHistoryState {
                position: Position { x, y: 0.0 },
                direction: Direction::Right,
                speed: 0.0,
                score: 0,
                respawned_at_tick: 0,
                generation,
            },
        );
        ContactHistoryFrame { tick, players: HashMap::new(), enemies }
    }

    /// review #4: a fractional tick that brackets a respawn (different life_id frames) must NOT
    /// interpolate — a lerp between a dead life and the fresh spawn is a phantom coordinate.
    #[test]
    fn sample_player_does_not_blend_across_life_id() {
        let mut ch = ContactHistory::with_capacity(8);
        ch.push_frame(player_frame(10, "p", 0.0, 0)); // life 0
        ch.push_frame(player_frame(11, "p", 100.0, 1)); // life 1 (respawned)
        assert!(ch.sample_player("p", 10.0).is_some(), "integer tick within a life is fine");
        assert!(ch.sample_player("p", 11.0).is_some());
        assert!(ch.sample_player("p", 10.5).is_none(), "must not blend across the respawn boundary");
    }

    /// review #4: even with NO generation constraint, an enemy sample across a respawn (different
    /// generations in the bracketing frames) must refuse rather than blend two lives' positions.
    #[test]
    fn sample_enemy_does_not_blend_across_generation_even_unconstrained() {
        let mut ch = ContactHistory::with_capacity(8);
        ch.push_frame(enemy_frame(10, "e", 0.0, 0)); // gen 0
        ch.push_frame(enemy_frame(11, "e", 100.0, 1)); // gen 1
        assert!(ch.sample_enemy("e", None, 10.5).is_none(), "no blend across generation");

        let mut same = ContactHistory::with_capacity(8);
        same.push_frame(enemy_frame(10, "e", 0.0, 0));
        same.push_frame(enemy_frame(11, "e", 100.0, 0)); // same life → interpolates
        assert!(same.sample_enemy("e", None, 10.5).is_some());
    }

    /// review (ring-edge): a fractional sample whose UPPER frame doesn't exist (ring edge / gap)
    /// must REFUSE, not fall back to the floor frame as if the entity never moved (a phantom
    /// no-move coordinate is fairness-sensitive). The exact integer frame still samples.
    #[test]
    fn sample_refuses_fractional_without_high_frame() {
        let mut ch = ContactHistory::with_capacity(8);
        ch.push_frame(player_frame(10, "p", 0.0, 0)); // only frame 10 — no 11
        ch.push_frame(enemy_frame(20, "e", 0.0, 0)); // only frame 20 — no 21
        assert!(ch.sample_player("p", 10.0).is_some(), "the exact integer frame still samples");
        assert!(ch.sample_player("p", 10.5).is_none(), "no frame 11 → refuse, don't phantom no-move");
        assert!(ch.sample_enemy("e", Some(0), 20.0).is_some());
        assert!(ch.sample_enemy("e", Some(0), 20.5).is_none(), "no frame 21 → refuse");
    }

    /// review #7 + (test exercises production): the ring trims via `push_frame` — the EXACT path
    /// `record` ends in — keeping only the newest `max_frames`. If `record`/`push_frame` ever stops
    /// trimming, this fails.
    #[test]
    fn record_trims_to_max_frames() {
        let mut ch = ContactHistory::with_capacity(3);
        for t in 0..10 {
            ch.push_frame(enemy_frame(t, "e", t as f32, 0));
        }
        assert_eq!(ch.ring.len(), 3, "trimmed to max_frames by the production path");
        assert_eq!(ch.ring.front().unwrap().tick, 7, "kept the NEWEST frames, dropped the oldest");
        assert_eq!(ch.ring.back().unwrap().tick, 9);
    }
}
