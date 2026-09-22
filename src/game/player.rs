use crate::config_shared::GameplayConfig;
use crate::game::map::GameMap;
use crate::game::sim::{can_step, simulate_player_tick, MoveAxis, PlayerSimState};
use crate::protocol::{Direction, PlayerState, Position};
use std::collections::VecDeque;
use std::sync::Arc;
use uuid::Uuid;

/// Stage 6.5 turn-intent buffer. Pac-Man-style: a swipe that arrives just
/// before/after an intersection still results in the obvious turn, instead
/// of getting silently dropped because the wall ahead made it momentarily
/// "invalid". This is NOT lag compensation — we never rewind server time.
///
/// `recent_intents` keeps the last ~8 turns the player asked for; the room
/// loop scans them newest-first each tick and applies the most recent one
/// that's both ≤ `MAX_INTENT_AGE_TICKS` old AND walkable from the player's
/// current server position.
#[derive(Debug, Clone)]
pub struct TurnIntent {
    pub direction: Direction,
    pub client_tick: u64,
    pub received_at_server_tick: u64,
    pub seq: u32,
}

/// 200 ms @ 60 Hz. Beyond this the intent is stale enough that applying it
/// would feel "ghostly" — the player has moved on.
pub const MAX_INTENT_AGE_TICKS: u64 = 12;
/// Buffer cap. Bigger than needed for typical play (≤3 distinct intents per
/// 200ms window) but cheap enough that we don't bother shrinking it.
const INTENT_BUFFER_CAPACITY: usize = 8;

#[derive(Debug, Clone)]
pub struct Player {
    pub id: String,
    pub user_id: String,
    pub nickname: String,
    pub position: Position,
    pub direction: Direction,
    pub desired_direction: Direction,
    /// Axis-locked movement state. Set on spawn / after any 90° turn /
    /// after respawn or portal. The simulation NEVER lets perpendicular
    /// position drift off `lane_center`, so there's nothing to "smooth
    /// correct" sideways.
    pub axis: MoveAxis,
    pub lane_center: f32,
    pub score: u32,
    pub speed: f32,
    pub is_invincible: bool,
    /// Server tick at which invincibility expires. None = not invincible.
    /// Gameplay time is measured in ticks; never in wall-clock.
    pub invincibility_end_tick: Option<u64>,
    pub boosted_until_tick: Option<u64>,
    /// Spawn protection after a death-respawn: the player CANNOT be eaten until
    /// this tick (purely defensive — does not let them eat). None = unprotected.
    /// Client mirrors this with a blink for the same window — the duration is
    /// `config.gameplay.spawn_protection_sec`, forwarded to the client in Welcome.
    pub spawn_protected_until_tick: Option<u64>,
    /// Victim-favored death fairness: a player only dies once an enemy has stayed
    /// in kill range for several consecutive ticks (room.rs owns the threshold).
    /// Tracks the current contact enemy + how many ticks it has persisted, so a
    /// one-tick brush from a render-skewed enemy can't kill instantly.
    pub death_contact_enemy: Option<String>,
    /// Generation (life) of `death_contact_enemy`. Enemy ids are reused across respawns, so
    /// the contact run is keyed on (id, generation): if the enemy respawns mid-candidate the
    /// run resets rather than a deferred death surviving the respawn. (lead review)
    pub death_contact_generation: u32,
    pub death_contact_ticks: u32,
    /// Server distance (px) at the most recent contact tick of the current run. Captured so a
    /// suppression (shield engaged / contact broke) can log how close the held candidate
    /// actually got before it was dropped — not just that a run existed. (lead review)
    pub death_contact_last_dist: f32,
    pub is_alive: bool,
    /// Monotonic life counter, bumped on every respawn (the player analogue of enemy
    /// `generation`). A player id is stable across deaths, so the claim causal ledger keys a
    /// "this player was killed" fact on (id, life_id): a kill recorded against life N must not
    /// reject an eat claim made by the respawned life N+1. Recorded into each contact-history
    /// frame so a reconstruction knows WHICH life the player was at a past render tick. (round-3 #1)
    pub life_id: u32,
    /// Highest accepted MoveCommand.seq. Stale or duplicate seqs are dropped
    /// on the server so clients can safely retransmit.
    pub last_processed_input_seq: u32,
    /// Stage 6.5: sliding window of turn intents — see `TurnIntent` doc.
    pub recent_intents: VecDeque<TurnIntent>,
    /// OBSERVE-ONLY lead sample: the latest accepted input's `(target_tick,
    /// received_at_server_tick)`. The client schedules `target_tick` a few ticks AHEAD of
    /// server-now so the command arrives in time, so `target_tick − received` is a per-player
    /// proxy for the client's render lead (the wire `MoveCommand` carries no client tick). Fills
    /// the death-telemetry victim-forward projection; calibrated against the client's true
    /// `presentation_lead_ticks`. (lead manifesto 2026-06-06)
    pub last_input_lead_sample: Option<(u64, u64)>,
    /// Has THIS admission's connection declared its world trusted (ClientWorldReady)?
    /// Death claims are rejected until it has — the server-side safety net for the
    /// proven 2026-06-10 ghost death (a claim sent one frame after a resume bootstrap,
    /// before the client's clock/remote history existed). False on every fresh Player
    /// (join AND resume both construct one), so a previous life's declaration can't leak.
    pub world_ready: bool,
    /// Claim-readiness handshake (lead manifesto 2026-06-11, VisualReady/ClaimReady split):
    /// `world_ready` above means "visually assembled" (overlay can drop); THIS means the
    /// client also proved its clock (clean pong) and its admission shield expired — only
    /// then are its death/eat claims honoured. Reset on every admission like world_ready.
    pub claim_ready: bool,
    /// Tick this admission entered the room (set by admit_player) — claim/input telemetry
    /// reports decisions as an age since admission, so "claim N ticks after a resume" is a
    /// log fact instead of a cross-event join. (lead manifesto #5)
    pub admitted_at_tick: u64,
    /// Whether this admission came through the resume path (vs a fresh join).
    pub admitted_via_resume: bool,
    /// First accepted MoveCommand of this admission already logged
    /// (server_first_input_after_admission fires once).
    pub first_input_logged: bool,
    /// Human vs server-driven filler (see `game::filler_bot`). SERVER-ONLY fact — it never
    /// rides the wire (a filler is an ordinary `PlayerState` to every client). Gates the
    /// things only a real account may touch: rewards/persistence, the human-based room
    /// capacity check, and the filler rebalance logic.
    pub kind: ActorKind,
    config: Arc<GameplayConfig>,
}

/// Who is driving this Player entity. Deliberately NOT serialized anywhere near the
/// protocol: exposing it would defeat the point of fillers (manifesto: no `is_bot` on
/// the wire, no separate list, no special color).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    Human,
    FillerBot,
}

/// The slice of a player's state worth restoring on RESUME after a mid-match drop: where
/// they were and their score. Lane/axis are NOT stored — they're recomputed from
/// position+direction exactly as a spawn does. Transient buffs (boost / invincibility /
/// spawn-protection) are intentionally NOT restored: a returning player reappears where they
/// dropped (no free random-respawn teleport / escape), but does not keep or gain protection
/// by reconnecting. (beta resume rule)
#[derive(Debug, Clone)]
pub struct PlayerResumeState {
    pub position: Position,
    pub direction: Direction,
    pub score: u32,
}

impl Player {
    pub fn new(user_id: String, nickname: String, position: Position, config: Arc<GameplayConfig>) -> Self {
        let (axis, lane_center) = PlayerSimState::lane_for_spawn(position, Direction::Right);
        Self {
            // Short opaque id (like conn_id), NOT a full uuid: this id rides in EVERY
            // 30/s snapshot delta to every client, so 36-char uuids were pure egress
            // weight (~0.7KB/delta of ids alone). STATISTICALLY unique (32 random bits,
            // ~10 ids alive per room) — not enforced; nothing parses id structure.
            id: format!("p_{}", &Uuid::new_v4().simple().to_string()[..8]),
            user_id,
            nickname,
            position,
            direction: Direction::Right,
            desired_direction: Direction::Right,
            axis,
            lane_center,
            score: config.gameplay.initial_score,
            speed: config.gameplay.base_speed,
            is_invincible: false,
            invincibility_end_tick: None,
            boosted_until_tick: None,
            spawn_protected_until_tick: None,
            death_contact_enemy: None,
            death_contact_generation: 0,
            death_contact_ticks: 0,
            death_contact_last_dist: 0.0,
            is_alive: true,
            life_id: 0,
            last_processed_input_seq: 0,
            recent_intents: VecDeque::with_capacity(INTENT_BUFFER_CAPACITY),
            last_input_lead_sample: None,
            world_ready: false,
            kind: ActorKind::Human,
            claim_ready: false,
            admitted_at_tick: 0,
            admitted_via_resume: false,
            first_input_logged: false,
            config,
        }
    }

    /// Capture the resume-relevant state (position/direction/score) before a drop removes
    /// the entity, so [`apply_resume`](Self::apply_resume) can put the player back where they
    /// were instead of at a random spawn.
    pub fn resume_snapshot(&self) -> PlayerResumeState {
        PlayerResumeState { position: self.position, direction: self.direction, score: self.score }
    }

    /// Re-apply a [`PlayerResumeState`] onto a freshly-added entity so a resumed player
    /// reappears where they dropped (not at a random spawn — which would be a free escape).
    /// Recomputes lane/axis from position+direction so movement stays consistent, same as a
    /// spawn. Transient buffs are deliberately left at their fresh-spawn defaults.
    pub fn apply_resume(&mut self, st: PlayerResumeState) {
        let (axis, lane_center) = PlayerSimState::lane_for_spawn(st.position, st.direction);
        self.position = st.position;
        self.direction = st.direction;
        self.desired_direction = st.direction;
        self.axis = axis;
        self.lane_center = lane_center;
        self.score = st.score;
    }

    /// Resume admission shield (lead manifesto 2026-06-11, P0 #1): true while this
    /// RESUME admission is inside its grace window — the player can neither die nor eat.
    /// Computed from the admission stamp (no extra mutable state to drift); fresh joins
    /// (`admitted_via_resume == false`) are never shielded by this.
    pub fn resume_shield_active(&self, tick: u64, shield_ticks: u64) -> bool {
        self.admitted_via_resume && tick < self.admitted_at_tick.saturating_add(shield_ticks)
    }

    #[inline]
    pub fn is_filler(&self) -> bool {
        self.kind == ActorKind::FillerBot
    }

    /// Stage 6.5: record an accepted MoveCommand into the intent buffer.
    /// Old entries beyond cap are dropped (FIFO) to keep memory tight.
    pub fn push_intent(
        &mut self,
        direction: Direction,
        client_tick: u64,
        received_at_server_tick: u64,
        seq: u32,
    ) {
        if self.recent_intents.len() == INTENT_BUFFER_CAPACITY {
            self.recent_intents.pop_front();
        }
        self.recent_intents.push_back(TurnIntent { direction, client_tick, received_at_server_tick, seq });
    }

    /// Stage 6.5: return the most recent buffered intent that's still fresh
    /// (≤ 12 ticks old) AND walkable from the player's current position.
    /// Returns the full intent so the caller can decide whether to count the
    /// application as "grace" (intent applied later than received).
    pub fn find_applicable_turn_intent(&self, map: &GameMap, current_tick: u64) -> Option<TurnIntent> {
        for intent in self.recent_intents.iter().rev() {
            if current_tick.saturating_sub(intent.received_at_server_tick) > MAX_INTENT_AGE_TICKS {
                continue;
            }
            if can_step(&self.position, &intent.direction, map) {
                return Some(intent.clone());
            }
        }
        None
    }

    /// Current world velocity (px/sec) from the live direction + speed. Axis-locked movement
    /// ⇒ exactly one component is non-zero; the sign matches `simulate_player_tick`'s position
    /// deltas (Up decreases y). Observe-only: used by death-fairness telemetry to project where
    /// the victim's CLIENT was rendering them (predicted forward by the client lead).
    pub fn velocity(&self) -> (f32, f32) {
        match self.direction {
            Direction::Up => (0.0, -self.speed),
            Direction::Down => (0.0, self.speed),
            Direction::Left => (-self.speed, 0.0),
            Direction::Right => (self.speed, 0.0),
        }
    }

    /// Record the lead sample from an accepted input. `target_tick` is the tick the client
    /// scheduled the input for (a few ticks ahead of server-now); `received_at_server_tick` is
    /// the server tick when it arrived. See [`Self::last_input_lead_sample`].
    pub fn note_input_lead(&mut self, target_tick: u64, received_at_server_tick: u64) {
        self.last_input_lead_sample = Some((target_tick, received_at_server_tick));
    }

    /// OBSERVE-ONLY estimate of how many ticks AHEAD the victim's client renders the local
    /// player: `target_tick − received_at_server_tick` of the latest accepted input (both
    /// captured together at receipt, so the difference is the steady-state lead regardless of how
    /// stale the sample is). The wire `MoveCommand` carries no client tick, so this scheduling
    /// gap is the only server-side lead signal. Logged, NOT yet used for kills, so the visible
    /// gate's missing victim-forward-projection can be calibrated against the client's true
    /// `presentation_lead_ticks`. Clamped at 0 (a client never renders the local player behind
    /// server-now); `None` until the first input. (lead manifesto 2026-06-06)
    pub fn estimated_client_lead_ticks(&self) -> Option<f64> {
        self.last_input_lead_sample.map(|(target, received)| (target as f64 - received as f64).max(0.0))
    }

    /// Update one tick of player simulation.
    ///
    /// `current_tick` is the server tick AFTER this update completes.
    /// Movement is delegated to the pure `simulate_player_tick` so server and
    /// client share identical math — see `game::sim`.
    pub fn update(&mut self, current_tick: u64, _dt: f32, map: &GameMap) -> (Position, Position) {
        let old_pos = self.position;

        self.update_timers(current_tick);

        let sim = PlayerSimState {
            position: self.position,
            direction: self.direction,
            desired_direction: self.desired_direction,
            speed: self.speed,
            axis: self.axis,
            lane_center: self.lane_center,
        };
        let next = simulate_player_tick(sim, map, &self.config);
        self.position = next.position;
        self.direction = next.direction;
        self.axis = next.axis;
        self.lane_center = next.lane_center;

        (old_pos, self.position)
    }

    fn update_timers(&mut self, current_tick: u64) {
        if let Some(end) = self.invincibility_end_tick {
            if current_tick >= end {
                self.is_invincible = false;
                self.invincibility_end_tick = None;
            }
        }
        if let Some(end) = self.boosted_until_tick {
            if current_tick >= end {
                self.speed = self.config.gameplay.base_speed;
                self.boosted_until_tick = None;
            }
        }
        if let Some(end) = self.spawn_protected_until_tick {
            if current_tick >= end {
                self.spawn_protected_until_tick = None;
            }
        }
    }

    /// True while spawn protection is active — the player cannot be eaten.
    pub fn is_spawn_protected(&self, current_tick: u64) -> bool {
        self.spawn_protected_until_tick.is_some_and(|end| current_tick < end)
    }

    /// Register that `enemy_id` (life `generation`) is in kill range this tick at `dist` px;
    /// returns the new run length of consecutive in-range ticks with the SAME enemy life. A
    /// different id OR a different generation (the enemy respawned) resets the run — a deferred
    /// death candidate must not survive the enemy's respawn. `dist` is remembered so a later
    /// suppression can report how close the run got.
    pub fn register_death_contact(&mut self, enemy_id: &str, generation: u32, dist: f32) -> u32 {
        let same = self.death_contact_enemy.as_deref() == Some(enemy_id)
            && self.death_contact_generation == generation;
        if same {
            self.death_contact_ticks += 1;
        } else {
            self.death_contact_enemy = Some(enemy_id.to_string());
            self.death_contact_generation = generation;
            self.death_contact_ticks = 1;
        }
        self.death_contact_last_dist = dist;
        self.death_contact_ticks
    }

    /// Contact broken (no eating enemy in kill range this tick, or shielded) —
    /// reset the run so the next death needs a fresh persistent contact.
    pub fn clear_death_contact(&mut self) {
        self.death_contact_enemy = None;
        self.death_contact_generation = 0;
        self.death_contact_ticks = 0;
        self.death_contact_last_dist = 0.0;
    }

    pub fn set_direction(&mut self, direction: Direction) {
        self.desired_direction = direction;
    }

    pub fn can_eat(&self, other: &Player) -> bool {
        if !self.is_alive || !other.is_alive {
            return false;
        }
        if other.is_invincible {
            return false;
        }
        self.is_invincible || self.score > other.score
    }

    pub fn respawn(&mut self, position: Position, current_tick: u64, tick_rate: u64) {
        let (axis, lane_center) = PlayerSimState::lane_for_spawn(position, Direction::Right);
        self.position = position;
        self.direction = Direction::Right;
        self.desired_direction = Direction::Right;
        self.axis = axis;
        self.lane_center = lane_center;
        self.score = self.config.gameplay.initial_score;
        self.is_alive = true;
        // New life: a kill recorded against the previous life must not reject this life's claims.
        self.life_id = self.life_id.saturating_add(1);
        self.is_invincible = false;
        self.invincibility_end_tick = None;
        self.boosted_until_tick = None;
        // Spawn protection: can't be eaten for the next few seconds (you respawn
        // into a field full of bots — without this you're eaten instantly). Config-sourced
        // (mirrored by the client via Welcome), not a duplicated const. (review #3)
        self.spawn_protected_until_tick =
            Some(current_tick + (self.config.gameplay.spawn_protection_sec * tick_rate as f32) as u64);
        self.clear_death_contact(); // stale contact run from the previous life
        self.speed = self.config.gameplay.base_speed;
        // Stage 6.5: turn intents from before death don't apply to the
        // post-respawn cell; drop them so we don't immediately steer the
        // player into a wall at the new spawn position.
        self.recent_intents.clear();
    }

    pub fn collect_point(&mut self) {
        self.score += 2;
    }

    pub fn collect_booster(&mut self, current_tick: u64, tick_rate: u64) {
        self.is_invincible = true;
        self.speed = self.config.gameplay.boosted_speed;
        let ticks = (self.config.gameplay.invincibility_duration_sec * tick_rate as f32) as u64;
        self.invincibility_end_tick = Some(current_tick + ticks);
        self.boosted_until_tick = Some(current_tick + ticks);
    }

    pub fn get_invincibility_remaining(&self, current_tick: u64, tick_rate: u64) -> f32 {
        match self.invincibility_end_tick {
            Some(end) if end > current_tick => (end - current_tick) as f32 / tick_rate as f32,
            _ => 0.0,
        }
    }

    pub fn to_state(&self, current_tick: u64, tick_rate: u64) -> PlayerState {
        PlayerState {
            id: self.id.clone(),
            nickname: self.nickname.clone(),
            position: self.position,
            direction: self.direction,
            score: self.score,
            speed: self.speed,
            is_invincible: self.is_invincible,
            invincibility_remaining: self.get_invincibility_remaining(current_tick, tick_rate),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_shared::load_config_from_str;

    fn make_config() -> Arc<GameplayConfig> {
        Arc::new(load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap())
    }

    #[test]
    fn collect_booster_sets_invincibility_end_tick() {
        let config = make_config();
        let mut p = Player::new("u".into(), "n".into(), Position { x: 0.0, y: 0.0 }, config.clone());
        let start_tick = 100;
        p.collect_booster(start_tick, config.room.tick_rate);

        // 10 sec * 60 tick_rate = 600 ticks
        assert!(p.is_invincible);
        assert_eq!(
            p.invincibility_end_tick,
            Some(
                start_tick
                    + (config.gameplay.invincibility_duration_sec * config.room.tick_rate as f32) as u64
            )
        );
        assert_eq!(p.speed, config.gameplay.boosted_speed);
    }

    #[test]
    fn invincibility_expires_exactly_at_end_tick() {
        let config = make_config();
        let mut p = Player::new("u".into(), "n".into(), Position { x: 0.0, y: 0.0 }, config.clone());
        p.collect_booster(100, config.room.tick_rate);
        let end = p.invincibility_end_tick.unwrap();

        p.update_timers(end - 1);
        assert!(p.is_invincible);

        p.update_timers(end);
        assert!(!p.is_invincible);
        assert_eq!(p.invincibility_end_tick, None);
        assert_eq!(p.speed, config.gameplay.base_speed);
    }

    #[test]
    fn invincibility_remaining_decreases_with_ticks() {
        let config = make_config();
        let mut p = Player::new("u".into(), "n".into(), Position { x: 0.0, y: 0.0 }, config.clone());
        p.collect_booster(0, config.room.tick_rate);
        let r0 = p.get_invincibility_remaining(0, config.room.tick_rate);
        let r60 = p.get_invincibility_remaining(60, config.room.tick_rate);
        assert!((r0 - config.gameplay.invincibility_duration_sec).abs() < 0.01);
        assert!((r60 - (config.gameplay.invincibility_duration_sec - 1.0)).abs() < 0.01);
    }
}
