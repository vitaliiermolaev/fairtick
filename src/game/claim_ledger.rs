//! Causal claim ledger (lead review round-3 #1). Applying claims in render-tick order is NOT
//! sufficient on its own: the contact-history ring is immutable, so an enemy a player ATE at tick
//! 10 still has a history frame at tick 11 and could "kill" via a later death claim referencing that
//! frame. This records what an applied claim (at a STRICTLY earlier render tick) consumed/killed, so
//! `process_claims` can reject a claim whose precondition an earlier claim already invalidated —
//! resolving same-entity eat↔death conflicts causally, not just by sort.
//!
//! PERSISTENT, not per-`process_claims` (round-3 #1 follow-up): the conflicting claims need NOT
//! arrive in the same drain batch — `eat@10` may resolve at server tick 12 and the stale `death@11`
//! arrive at tick 13, by which point a per-batch ledger is empty but the gen-0 history frame still
//! exists. So the Room holds ONE ledger across batches; `prune` drops entries older than any claim
//! that could still reference them (their history frames have aged out of the ring).
//!
//! Keys carry the entity's LIFE id (enemy `generation` / player `life_id`), because ids are reused
//! across respawns: a kill recorded against life N must never reject a claim made by the respawned
//! life N+1. (Extracted from the Room god-class — review #1.)

use crate::game::timeline::RenderTick;
use std::collections::HashMap;

#[derive(Default, Debug)]
pub(crate) struct ClaimEffects {
    /// (enemy_id, generation) → earliest render tick at which an applied EAT claim consumed it.
    consumed_enemies: HashMap<(String, u32), RenderTick>,
    /// (player_id, life_id) → earliest render tick at which an applied claim killed that life.
    killed_player_lives: HashMap<(String, u32), RenderTick>,
}

impl ClaimEffects {
    pub(crate) fn consume_enemy(&mut self, enemy_id: &str, generation: u32, tick: RenderTick) {
        self.consumed_enemies
            .entry((enemy_id.to_string(), generation))
            .and_modify(|t| {
                if tick < *t {
                    *t = tick
                }
            })
            .or_insert(tick);
    }
    pub(crate) fn kill_player_life(&mut self, player_id: &str, life_id: u32, tick: RenderTick) {
        self.killed_player_lives
            .entry((player_id.to_string(), life_id))
            .and_modify(|t| {
                if tick < *t {
                    *t = tick
                }
            })
            .or_insert(tick);
    }
    /// An EAT claim consumed this enemy life STRICTLY before `before` (so a death by it can't stand).
    pub(crate) fn enemy_consumed_before(&self, enemy_id: &str, generation: u32, before: RenderTick) -> bool {
        self.consumed_enemies.get(&(enemy_id.to_string(), generation)).is_some_and(|t| *t < before)
    }
    /// A claim killed THIS life of the player STRICTLY before `before` (so an eat BY that same life
    /// can't stand). A later life (post-respawn `life_id`) has no entry, so its eats are unaffected.
    pub(crate) fn player_life_killed_before(
        &self,
        player_id: &str,
        life_id: u32,
        before: RenderTick,
    ) -> bool {
        self.killed_player_lives.get(&(player_id.to_string(), life_id)).is_some_and(|t| *t < before)
    }
    /// Drop entries older than `min_tick`. Safe once an entry's render tick has aged out of the
    /// contact-history ring: no still-acceptable claim can reconstruct that life any more, so the
    /// entry can never bound another claim again. Keeps the ledger bounded over a long match.
    pub(crate) fn prune(&mut self, min_tick: RenderTick) {
        self.consumed_enemies.retain(|_, t| *t >= min_tick);
        self.killed_player_lives.retain(|_, t| *t >= min_tick);
    }
}
