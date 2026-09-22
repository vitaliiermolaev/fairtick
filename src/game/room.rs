use crate::clock::now_unix_ms;
use crate::config_shared::{GameplayConfig, MazeData};
use crate::game::ai::Enemy;
use crate::game::claim_ledger::ClaimEffects;
use crate::game::contact_history::{ContactHistory, PlayerHistoryState};
use crate::game::events::DomainEvent;
use crate::game::filler_bot::{BotController, BotView, SeenActor, FILLER_NICKNAMES};
use crate::game::map::GameMap;
use crate::game::outbox::{PlayerOutbound, PlayerOutbox};
use crate::game::player::{ActorKind, Player, PlayerResumeState};
use crate::game::policies::eat_claim::{
    accept_reason, EatClaimPolicy, EnemyEatRules, PlayerEatRules, EAT_CLAIM_PENDING_CAP,
};
use crate::game::policies::enemy_contact::EnemyContactPolicy;
use crate::game::policies::enemy_death_candidate::{
    DeathDecision, DelayReason, EnemyDeathCandidatePolicy, EnemyDeathContext,
};
use crate::game::policies::enemy_death_claim::{EnemyDeathClaimPolicy, EnemyDeathClaimRules};
use crate::game::policies::filler_eat::{
    FillerEatCandidate, FillerEatPolicy, FillerEatRules, FillerEatVerdict,
};
use crate::game::rng::RoomRng;
use crate::game::snapshot::SnapshotProjector;
use crate::game::timeline::{RenderTick, ServerTick};
use crate::protocol::{
    Booster, BoosterType, DeathDecisionCode, Direction, EatClaim, EatTargetKind, EnemyDeathClaim,
    GameStateDelta, GameStateUpdate, GameWinner, PlayerReward, PointItem, PortalState, Position,
    ServerMessage, StatePatch,
};
use crate::telemetry::Telemetry;
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

// Victim-favored enemy-eats-player fairness now lives in EnemyContactPolicy
// (src/game/policies/enemy_contact.rs): the rule, the tuning history, and the
// VICTIM_GRACE_PX / CONTACT_TICKS_REQUIRED constants. Re-exported here so existing
// references (`crate::game::room::*` in the Welcome handshake, the in-file tests) keep
// resolving against a single source of truth.
pub use crate::game::policies::enemy_contact::{CONTACT_TICKS_REQUIRED, VICTIM_GRACE_PX};

/// A just-respawned bot is spawned at least this far from every player (paired with
/// `claim_fairness.enemy_respawn_eat_grace_ticks`). Together they kill the
/// double-eat: an eaten bot used to respawn inside the eater's mouth and be eaten again
/// the very next tick (two EnemyRespawned for the same enemy one tick apart).
const ENEMY_RESPAWN_CLEARANCE_PX: f32 = 40.0;
const ENEMY_RESPAWN_SPAWN_TRIES: usize = 8;

/// Filler lifecycle pacing (server-only knobs; the player-visible contract — target count
/// and exit delays — lives in `config.filler`). The rebalance check is throttled so the
/// population drifts toward target organically instead of snapping every tick; replacement
/// spawns are extra-delayed so a leaver's "slot" refills like a real join, not a swap.
const FILLER_REBALANCE_INTERVAL_TICKS: u64 = 30;
const FILLER_SPAWN_DELAY_MIN_TICKS: usize = 60;
const FILLER_SPAWN_DELAY_MAX_TICKS: usize = 240;

/// Filler internal-eat conservatism (policy rationale in `policies::filler_eat`): the
/// overlap must persist this many consecutive server ticks (100 ms @60tps — a real
/// glide-over, inside the lead's 4–8 band), and a filler that ate waits out a cooldown
/// before it may eat again (no chain-vacuuming a lobby).
const FILLER_EAT_SUSTAINED_TICKS: u32 = 6;
const FILLER_EAT_COOLDOWN_TICKS: u64 = 240;
/// Claim-id marker for filler INTERNAL eats in events/logs/death causes. u32::MAX, not
/// 0: client claim ids count up from 1, but a magic 0 would be indistinguishable from a
/// default-initialized field the day some client bug sends one. (lead review)
const INTERNAL_FILLER_CLAIM_ID: u32 = u32::MAX;

/// A due polite exit re-checks visibility: a leaver within this range of a human gets
/// POSTPONED (rng 30..90 ticks, up to the postpone cap) — people shouldn't watch a
/// "player" evaporate next to them. Expedited (burst-overflow) exits skip the recheck:
/// shrinking an overfull room beats hiding one despawn.
const FILLER_EXIT_NEAR_HUMAN_PX: f32 = 260.0;
const FILLER_EXIT_POSTPONE_MIN_TICKS: usize = 30;
const FILLER_EXIT_POSTPONE_MAX_TICKS: usize = 90;
const FILLER_EXIT_MAX_POSTPONES: u8 = 3;
/// Expedited exit delay band — still never same-tick, but fast (0.1–0.4 s @60tps).
const FILLER_EXPEDITED_EXIT_MIN_TICKS: usize = 6;
const FILLER_EXPEDITED_EXIT_MAX_TICKS: usize = 24;
/// Runtime replacement fillers spawn at least this far from every human ("a player
/// materialized in front of me" tell); first-admission top-ups ride the keyframe and
/// don't need it. No clear spot in the tries budget ⇒ the spawn is POSTPONED.
const FILLER_SPAWN_MIN_HUMAN_DIST_PX: f32 = 400.0;
const FILLER_SPAWN_CLEAR_TRIES: usize = 12;
const FILLER_SPAWN_POSTPONE_MIN_TICKS: usize = 60;
const FILLER_SPAWN_POSTPONE_MAX_TICKS: usize = 120;
/// The manager won't seat a NEW join into a room with less than this left on the clock
/// (10 s @60tps) — joining a filler lobby seconds before GameEnded feels broken. Resumes
/// are exempt: returning to your own match is always right.
pub const MIN_JOINABLE_REMAINING_TICKS: u64 = 600;

/// One filler's pending departure.
#[derive(Debug, Clone)]
struct ScheduledFillerExit {
    player_id: String,
    at_tick: u64,
    /// Burst-overflow exit: short delay, skips the near-human postpone.
    expedited: bool,
    /// Times the due exit was already deferred for being in a human's view.
    postpones: u8,
}

/// Death-fairness payload captured at the killing tick, threaded into the
/// PlayerRespawned event so the client can compare server truth vs what it drew.
struct EnemyDeath {
    killer_enemy_id: String,
    server_dist: f32,
    contact_ticks: u32,
    killer_position: Position,
    victim_position: Position,
}

/// What killed a player — threaded into respawn_player so the PlayerRespawned event
/// carries the right killer field. Enemy kills are server-side (current model);
/// Player kills only ever come from an accepted EatClaim.
enum DeathCause {
    Enemy(EnemyDeath),
    Player {
        killer_player_id: String,
        server_dist: f32,
        killer_position: Position,
        victim_position: Position,
        #[allow(dead_code)]
        claim_id: u32,
    },
}

// --- player-initiated eating (EatClaim) -----------------------------------
// The EatClaim validation RULES live in EatClaimPolicy (src/game/policies/eat_claim.rs); the
// fairness TOLERANCES (visual radius, position tolerance, view skew, process delay, history span,
// respawn grace) now come from `config.claim_fairness` (single source — review #3). The only thing
// still a code constant here is EAT_CLAIM_PENDING_CAP (a memory bound, not a fairness value).

/// A claim handed to the room from the websocket task.
#[derive(Debug)]
pub struct EatClaimInput {
    pub player_id: String,
    pub claim: EatClaim,
}

/// A victim's claim that an enemy ate them (claim-based death path). Routed to the room loop
/// and validated against contact history, symmetric with [`EatClaimInput`].
#[derive(Debug)]
pub struct EnemyDeathClaimInput {
    pub player_id: String,
    pub claim: EnemyDeathClaim,
}

/// OBSERVE-ONLY "drove through it and lived" probe routed to the room loop. The client sends it
/// when it logged a `visual_overlap_without_death`; the room replies (in the log) with its own
/// view of that moment. Diagnostics only — never mutates game state. (lead manifesto 2026-06-06)
#[derive(Debug)]
pub struct VisualOverlapProbeInput {
    pub player_id: String,
    pub enemy_id: String,
    pub enemy_generation: u32,
    pub known_server_tick: u64,
}

/// An async, fire-and-forget command handed to the room from the transport tasks. The
/// room is effectively a single-writer actor — its `update` runs under a per-room write
/// lock — and `RoomCommand` makes that mailbox explicit: every async input funnels
/// through one handler (`apply_command`), which also lets tests drive the room without a
/// channel (feed a command list, run `update`, inspect the emitted events).
///
/// Join/Leave/RequestFullState are still served synchronously under the lock (they return
/// a value to the caller), so they are not part of this enum yet — promoting them would
/// mean the full actor model (a dedicated room task + oneshot replies), a deliberate
/// later step (see Plan.md §2).
#[derive(Debug)]
pub enum RoomCommand {
    PlayerInput(PlayerInput),
    EatClaim(EatClaimInput),
    /// Claim-based enemy-eats-player death — see [`EnemyDeathClaimInput`].
    EnemyDeathClaim(EnemyDeathClaimInput),
    /// OBSERVE-ONLY diagnostic correlation — see [`VisualOverlapProbeInput`].
    VisualOverlapProbe(VisualOverlapProbeInput),
}

/// Per-player input buffer. Inputs are applied at their `target_tick` (not on
/// receive), so client prediction and server authority turn on the same tick.
/// Bounded so a flood can't grow memory.
#[derive(Debug, Default)]
pub struct PlayerInputSlot {
    pub pending: Vec<PlayerInput>,
    pub dropped_count: u64,
}

const INPUT_BUFFER_CAP: usize = 64;
/// Reject inputs targeting more than this far in the future (covers the client's
/// max lead with headroom; anything beyond is a misbehaving/malicious client).
const MAX_FUTURE_INPUT_TICKS: u64 = 60;
/// Inputs older than this (target_tick already long past) are rejected; the
/// client's resend + reconciliation recover. (Within the window, a late input
/// applies on the next tick — and stale seqs are dropped by the seq guard.)
const LATE_GRACE_TICKS: u64 = 60;
/// v6 anti-cheat safety net: at policy_version >= 6 the server doesn't server-kill (death is
/// claim-based), but if a LETHAL enemy contact persists this many ticks with NO client death
/// claim arriving (which would respawn the victim and reset the run), the client is
/// withholding/broken/cheating and the server kills anyway. 30 ticks = 0.5s — far beyond the few
/// ticks a healthy client needs to see the overlap and claim. (lead review v6 #1)
const SERVER_FALLBACK_KILL_TICKS: u32 = 30;
/// Resume admission shield (lead manifesto 2026-06-11, P0 #1): for this many ticks after a
/// RESUME admission the player can neither die nor eat. The 07-12-37 run shows accepted
/// kills 20 and 48 ticks after re-entry — technically clean claims, but UX-wise "вернулся
/// и сразу хлопнуло". 90 ticks = 1.5s @60Hz. SYMMETRIC (no dying, no eating, no being
/// eaten by players) so the window can't be farmed as free invulnerability. Sent to the
/// client in Welcome (`resume_shield_ticks`) so its claim gates mirror the same window
/// from one source.
pub const RESUME_SHIELD_TICKS: u64 = 90;
/// Minimum age of an admission INTO A RUNNING room before its claims are honoured (lead
/// P0 #5) — a second, coarser net under the claim-ready handshake. `admitted_at_tick == 0`
/// (an original member of a fresh room) is exempt: the room starts WITH them, and spawn
/// protection already covers a fresh room's first seconds.
const MIN_CLAIM_AFTER_ADMISSION_TICKS: u64 = 60;
/// STRICT visible bar for the no-claim fallback kill (lead P0 #2): `kill_radius + this`,
/// required on BOTH visible legs (enemy reconstructed back AND victim projected forward),
/// and the projection must EXIST. The 07-12-37 fallback ghost (reconstructed 13.7 /
/// projected 17.7 / client actually drew 23.6 — no on-screen overlap, hence honestly no
/// claim) stays deferred at 11+2=13px, while a true mutual on-screen kill (~3-9px) passes.
const FALLBACK_STRICT_VISIBLE_GRACE_PX: f32 = 2.0;
/// Fallback-candidate telemetry cadence (lead P2 #7): the first lethal tick, then every
/// Nth — instead of one warn per tick for the whole sustained contact.
const FALLBACK_CANDIDATE_LOG_EVERY_TICKS: u32 = 10;
/// A pending, PLAUSIBLE death claim (right victim/enemy/gen, near-future, shape+positions valid)
/// buys the in-flight claim a BOUNDED extra grace before the anti-cheat fallback pre-empts it —
/// it must NOT hold forever. A cheater can keep a fresh future death claim "pending" every tick
/// (each held until the sim reaches its render tick), so without a cap the fallback would never
/// fire and the player would be immortal. Past SERVER_FALLBACK_KILL_TICKS + this, the server kills
/// regardless of any pending claim (15 ticks = 0.25s, far longer than an honest claim's flight).
/// (round-3 #2)
const DEATH_CLAIM_PENDING_GRACE_TICKS: u32 = 15;
/// Per-player cap on buffered (pending) death claims — anti-spam on the now-RELIABLE death-claim
/// channel. An honest client has ≤1 death claim in flight at a time (its controller gates a single
/// prediction), so a handful is ample headroom for retransmit/timing races; beyond it a client is
/// flooding and further claims are rejected at ingest. The global `EAT_CLAIM_PENDING_CAP` still
/// bounds total memory; this stops ONE player from monopolising the shared ring. (round-3 follow-up)
const MAX_PENDING_DEATH_CLAIMS_PER_PLAYER: usize = 3;
/// Clock-jitter headroom added to the client's prediction-lead cap to get the death-claim future
/// SCHEDULING cap (see `death_claim_max_future_ticks`). Keeps the scheduling cap strictly above the
/// fallback HOLD lead so a claim the fallback would treat as plausible is never rejected before it
/// can resolve. Small + fixed (not config) — pure clock-skew slack, not a gameplay rule. (round-3 #2)
/// INVARIANT (lead 2026-06-11 remote-interp patch): this slack also bounds the TOP of the
/// client's ADAPTIVE enemy interp delay — the death-claim skew cap is `lead + config interp
/// (8) + this`, so the client clamps its adaptive delay to 8 + 10 = 18 ticks
/// (MaxAdaptiveEnemyDelayTicks). Shrinking this without lowering the client clamp makes
/// honest high-jitter death claims die as skew_too_large.
const DEATH_CLAIM_FUTURE_SLACK_TICKS: u64 = 10;

/// The history frame a claim at `render_tick` NEEDS before it can be resolved: `ceil(render_tick)`,
/// or `None` if the render tick is not a valid client timeline value (non-finite or negative).
///
/// This is the SINGLE definition tying the claim-readiness gate to the ContactHistory sampler:
/// `sample_player`/`sample_enemy` REFUSE a fractional tick whose UPPER bracketing frame isn't
/// recorded yet (no phantom no-move coordinate). So a fractional claim must be HELD until the sim
/// has recorded `ceil(render_tick)` — resolving it at `floor` would mark it ready a tick early and
/// the sampler would (correctly) reject the honest claim as `no_*_history`. Using one named helper
/// (instead of an inline `tick_f.ceil()`) keeps the gate from silently drifting back to floor and
/// re-opening that false-reject. `None` (invalid timeline) is rejected explicitly by the caller as
/// `invalid_render_tick` rather than coerced to tick 0 — explicit timeline validation. (lead review)
#[inline]
fn required_history_tick(render_tick: f64) -> Option<u64> {
    if !render_tick.is_finite() || render_tick < 0.0 {
        return None;
    }
    Some(render_tick.ceil() as u64)
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubmitResult {
    Accepted,
    /// Accepted, but its target_tick had already passed (applies next tick). A
    /// burst of these means the client's target_tick scheduling runs slightly
    /// behind the server — a likely source of small reconciliation corrections.
    AcceptedLate {
        age_ticks: u64,
    },
    ReplacedRetransmit,
    RejectedFuture,
    RejectedTooLate,
    Full,
}

impl PlayerInputSlot {
    /// Validate + buffer an input, deduping retransmits by seq and bounding the
    /// buffer. `current_tick` is the server's current tick (for the target_tick
    /// validation window). Returns the outcome for metrics.
    pub fn submit(&mut self, input: PlayerInput, current_tick: u64) -> SubmitResult {
        if input.target_tick > current_tick + MAX_FUTURE_INPUT_TICKS {
            return SubmitResult::RejectedFuture;
        }
        if input.target_tick + LATE_GRACE_TICKS < current_tick {
            return SubmitResult::RejectedTooLate;
        }
        // Retransmit of a seq we already hold → replace in place (never grow).
        if let Some(existing) = self.pending.iter_mut().find(|x| x.seq == input.seq) {
            *existing = input;
            return SubmitResult::ReplacedRetransmit;
        }
        if self.pending.len() >= INPUT_BUFFER_CAP {
            self.dropped_count = self.dropped_count.saturating_add(1);
            return SubmitResult::Full;
        }
        let target_tick = input.target_tick;
        self.pending.push(input);
        if target_tick <= current_tick {
            SubmitResult::AcceptedLate { age_ticks: current_tick - target_tick }
        } else {
            SubmitResult::Accepted
        }
    }

    /// Remove and return inputs due at `current_tick` (`target_tick <= current`),
    /// ordered by (target_tick, seq). A command that arrived late (target_tick
    /// already passed) is due immediately.
    pub fn take_due(&mut self, current_tick: u64) -> Vec<PlayerInput> {
        let mut due: Vec<PlayerInput> = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].target_tick <= current_tick {
                due.push(self.pending.remove(i));
            } else {
                i += 1;
            }
        }
        due.sort_by(|a, b| a.target_tick.cmp(&b.target_tick).then(a.seq.cmp(&b.seq)));
        due
    }
}

/// How a player is being added to the room. Drives the spawn position AND the telemetry the
/// add emits, so the FRESH-join and RESUME paths share one insertion routine without a resume
/// masquerading as a random-spawn join. (review Medium: telemetry honesty on resume.)
enum PlayerAdmission {
    /// New entry: random spawn, `player_joined` telemetry.
    Fresh,
    /// Reconnect after a drop: restore the dropped position/direction/score, `player_resumed`
    /// telemetry with the RESTORED values.
    Resume(PlayerResumeState),
}

/// One inbound MoveCommand routed to the room loop.
///
/// `seq` is monotonic per player; the server drops anything `<= last_processed`
/// to ignore retransmits. `target_tick` is the server tick at which the client
/// wants this input applied — the buffer holds it until the room reaches it.
#[derive(Debug)]
pub struct PlayerInput {
    pub player_id: String,
    pub seq: u32,
    pub target_tick: u64,
    pub direction: Direction,
}

#[derive(Debug)]
pub struct Room {
    pub id: String,
    pub map: GameMap,
    pub players: HashMap<String, Player>,
    pub enemies: Vec<Enemy>,
    pub points: Vec<PointItem>,
    pub boosters: Vec<Booster>,
    /// Current server tick (incremented once per Room::update call).
    pub tick: u64,
    /// Tick at which the match ends. Wall-clock is NOT used.
    pub game_end_tick: u64,
    pub next_point_spawn_tick: u64,
    pub next_booster_spawn_tick: u64,
    /// Some(tick) while a portal is in cooldown; None when portal is active or never used.
    pub portal_respawn_tick: Option<u64>,
    pub is_active: bool,
    /// Slots held for players who dropped and may RESUME within `reconnect_grace_sec`. A
    /// disconnect removes the player's entity but reserves its capacity here so a third
    /// player can't take the slot before the original reconnects; `is_full` counts it.
    /// Incremented by `hold_player_for_resume`, decremented by `resume_player` (the player
    /// came back) or `release_reservation` (grace expired). Pure capacity bookkeeping — it
    /// touches NO gameplay (death/eat/collision) state.
    reserved_slots: usize,
    /// Filler-bot brains keyed by player_id. The ENTITY rides `players` as an ordinary
    /// Player (kind = FillerBot, no outbox/input slot); only the brain lives here. See
    /// `game::filler_bot` for the design (perception/goals/pathfinding/motor).
    filler_controllers: HashMap<String, BotController>,
    /// Fillers scheduled to leave. Exits are DELAYED (`config.filler.exit_delay_*`, or
    /// the expedited band for burst overflow) so a human join never triggers a same-tick
    /// quit — the classic "bot freed my slot" tell.
    scheduled_filler_exits: Vec<ScheduledFillerExit>,
    /// Ticks at which queued replacement fillers enter (delayed top-up after the
    /// population dropped below target; re-checked against target when due).
    pending_filler_spawns: Vec<u64>,
    /// Next tick the throttled filler rebalance runs.
    next_filler_rebalance_tick: u64,
    /// First tick the room had zero humans AND zero reserved resume slots (None while
    /// inhabited). Past `config.filler.empty_room_grace_ticks` the fillers are swept and
    /// the room deactivates — a filler-only room must not simulate until game_end.
    humans_absent_since_tick: Option<u64>,
    /// Internal-eat bookkeeping (sustained-overlap runs + per-attacker cooldowns); the
    /// rules and rationale live in [`policies::filler_eat`](crate::game::policies::filler_eat).
    filler_eat_policy: FillerEatPolicy,
    /// Stage 6: per-player outbound transport (reliable mpsc + latest-only snapshot watch) and its
    /// delivery counters. Extracted to [`PlayerOutbox`] (review #1).
    pub outbox: PlayerOutbox,
    /// Stage 6: per-player input coalescer. Drained once per tick.
    pub input_slots: HashMap<String, PlayerInputSlot>,
    pub config: Arc<GameplayConfig>,
    pub rng: RoomRng,
    /// Next event_id to assign. Starts at 1 so `last_event_id` Option uses 0 for "none yet".
    next_event_id: u64,
    input_rx: mpsc::Receiver<PlayerInput>,
    // Player-initiated eating: incoming claims, the per-player de-dup high-water
    // mark, and a ring of recent contact frames to validate claims against.
    claim_rx: mpsc::Receiver<EatClaimInput>,
    // OBSERVE-ONLY "drove through it and lived" probes. Both ends live on the room so the sender
    // is handed out via `probe_sender()` (no extra tuple element on the constructor).
    probe_rx: mpsc::Receiver<VisualOverlapProbeInput>,
    probe_tx: mpsc::Sender<VisualOverlapProbeInput>,
    // Claim-based death: same both-ends-on-room pattern as the probe channel (sender handed out
    // via `enemy_death_claim_sender()`), plus the pending ring + per-player de-dup mark.
    enemy_death_claim_rx: mpsc::Receiver<EnemyDeathClaimInput>,
    enemy_death_claim_tx: mpsc::Sender<EnemyDeathClaimInput>,
    pending_enemy_death_claims: Vec<EnemyDeathClaimInput>,
    last_enemy_death_claim_id: HashMap<String, u32>,
    pending_eat_claims: Vec<EatClaimInput>,
    last_eat_claim_id: HashMap<String, u32>,
    contact_history: ContactHistory,
    /// Causal claim ledger, PERSISTENT across `process_claims` batches (round-3 #1). Pruned to the
    /// history window in `record_contact_history`. See [`ClaimEffects`].
    claim_ledger: ClaimEffects,
    // Stage 6 telemetry — surfaced via `take_telemetry()` for the HUD/audit.
    pub input_coalesced_count: u64,
    // target_tick validation metrics.
    pub input_future_rejected: u64,
    pub input_late_rejected: u64,
    pub input_duplicate: u64,
    // Inputs accepted but already past their target_tick (client running behind).
    pub input_late_accepted: u64,
    pub input_late_accepted_max_age: u64,
    /// Stage 6.5: number of times a buffered intent was applied a tick or
    /// more after it arrived. On Good network this stays ~0; on LossyTCP
    /// it grows whenever the intent's wall-validity opens up post-arrival.
    pub turn_grace_applied_count: u64,
    /// Cumulative count of SERVER-FORCED resyncs in THIS room: a player's reliable lane overflowed,
    /// so we dropped their outbound to force a reconnect=resync rather than ship snapshots that
    /// assume a hard event they never got (review #2). Should sit at 0 on a healthy room — a
    /// climbing per-room total points at a specific room/client that is chronically behind, which
    /// the per-event `reliable_overflow_forced_resync` records can't show at a glance. Observe-only.
    /// (review #10 — per-room resync telemetry counter.)
    pub forced_resync_count: u64,
    /// Match-telemetry sink. `None` in unit tests (Room::new leaves it unset);
    /// RoomManager attaches it via `attach_telemetry` for live rooms.
    telemetry: Option<Telemetry>,
    /// Monotonic per-tick snapshot sequence stamped into every snapshot. Starts at
    /// 1; 0 is reserved for out-of-band keyframes (join / RequestFullState) so the
    /// client can tell a periodic snapshot (drop-trackable) from a keyframe.
    next_snapshot_seq: u64,
}

/// Why [`Room::try_add_player`] refused a join. The caller (RoomManager::join_room)
/// retries against another (or a fresh) room for BOTH variants. This exists so the
/// join validation happens under the SAME write lock that inserts the player:
/// - `Full` — two joiners racing on a 9/10 room can't both pass a stale `has_space()`
///   snapshot and overflow it to 11/10.
/// - `Inactive` — the room ended between `find_or_create_room()` (read lock) and our
///   write lock, so we must not add a player into a dead room.
/// - `TooLateInMatch` — the match crossed the min-remaining threshold between the
///   manager's read-lock pick and our write lock; a fresh joiner must not land in a
///   match that GameEnds seconds later. The manager retries into another/fresh room.
///   RESUMES are unaffected (they come through `admit_player` via the resume path,
///   not this entry point) — returning to your own match is always right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomJoinRejected {
    Full,
    Inactive,
    TooLateInMatch,
}

impl Room {
    pub fn new(
        config: Arc<GameplayConfig>,
        maze: Arc<MazeData>,
        seed: Option<u64>,
    ) -> (Self, mpsc::Sender<PlayerInput>, mpsc::Sender<EatClaimInput>) {
        // Stage 6: bounded input channel. Capacity 256 is comfortably larger
        // than rate_limit_per_sec * tick_period * max_players, so the channel
        // shouldn't fill under healthy load; if it does fill, sends fail fast
        // and the player_move helper records a drop instead of growing memory.
        let (input_tx, input_rx) = mpsc::channel(256);
        let (claim_tx, claim_rx) = mpsc::channel(256);
        // Low-rate diagnostic channel (a few survived-overlap probes per match).
        let (probe_tx, probe_rx) = mpsc::channel(64);
        // Claim-based death channel (a few deaths per player per match).
        let (enemy_death_claim_tx, enemy_death_claim_rx) = mpsc::channel(256);
        let mut rng = match seed {
            Some(s) => RoomRng::from_seed(s),
            None => RoomRng::from_entropy(),
        };
        let map = GameMap::new(&config, &maze, &mut rng);

        let tick_rate = config.room.tick_rate;
        let game_end_tick = config.room.game_duration_sec * tick_rate;
        let next_point_spawn_tick = (config.spawn.point_interval_sec * tick_rate as f32) as u64;
        let next_booster_spawn_tick = (config.spawn.booster_interval_sec * tick_rate as f32) as u64;

        // Spawn config.ai.count bots ("bot1"…), each with a DISTINCT starting score
        // spread across [min..=max] so they read as individuals (and only some are
        // eatable at a given player score).
        let bot_count = config.ai.count; // 0 = bot-free live mode (no forced spawn)
        let (min_s, max_s) = (config.ai.min_start_score, config.ai.max_start_score);
        let enemies: Vec<Enemy> = (0..bot_count)
            .map(|i| {
                let nickname = format!("bot{}", i + 1);
                let start_score = if bot_count == 1 {
                    min_s
                } else {
                    min_s + ((max_s - min_s) as u64 * i as u64 / (bot_count - 1) as u64) as u32
                };
                Enemy::new(map.get_random_spawn_position(&mut rng), config.clone(), 0, nickname, start_score)
            })
            .collect();

        // Read before `config` is moved into the struct below.
        let history_ticks = config.claim_fairness.history_ticks;
        let room = Self {
            id: Uuid::new_v4().to_string(),
            map,
            players: HashMap::new(),
            enemies,
            points: Vec::new(),
            boosters: Vec::new(),
            tick: 0,
            game_end_tick,
            next_point_spawn_tick,
            next_booster_spawn_tick,
            portal_respawn_tick: None,
            is_active: true,
            reserved_slots: 0,
            filler_controllers: HashMap::new(),
            scheduled_filler_exits: Vec::new(),
            pending_filler_spawns: Vec::new(),
            next_filler_rebalance_tick: FILLER_REBALANCE_INTERVAL_TICKS,
            humans_absent_since_tick: None,
            filler_eat_policy: FillerEatPolicy::new(FillerEatRules {
                // SAME overlap radius human eat claims validate against — one source of truth.
                overlap_radius_px: config.claim_fairness.visible_overlap_radius_px,
                sustained_ticks: FILLER_EAT_SUSTAINED_TICKS,
                attacker_cooldown_ticks: FILLER_EAT_COOLDOWN_TICKS,
                min_victim_admission_age_ticks: MIN_CLAIM_AFTER_ADMISSION_TICKS,
            }),
            outbox: PlayerOutbox::default(),
            input_slots: HashMap::new(),
            config,
            rng,
            next_event_id: 1,
            input_rx,
            claim_rx,
            probe_rx,
            probe_tx,
            enemy_death_claim_rx,
            enemy_death_claim_tx,
            pending_enemy_death_claims: Vec::new(),
            last_enemy_death_claim_id: HashMap::new(),
            pending_eat_claims: Vec::new(),
            claim_ledger: ClaimEffects::default(),
            last_eat_claim_id: HashMap::new(),
            contact_history: ContactHistory::with_capacity(history_ticks),
            input_coalesced_count: 0,
            input_future_rejected: 0,
            input_late_rejected: 0,
            input_duplicate: 0,
            input_late_accepted: 0,
            input_late_accepted_max_age: 0,
            turn_grace_applied_count: 0,
            forced_resync_count: 0,
            telemetry: None,
            next_snapshot_seq: 1,
        };

        // Deliberately NO filler spawn here (lead review P0 #2): a room fills to the
        // visible-population target at the FIRST HUMAN ADMISSION (admit_player → top-up),
        // so the joiner's keyframe shows exactly `target` players — target-1 fillers plus
        // themselves — instead of target+1 (fillers pre-spawned, then the human on top).

        (room, input_tx, claim_tx)
    }

    /// Sender for OBSERVE-ONLY visual-overlap probes (the receiver lives on the room and is
    /// drained each `update`). Handed to RoomManager after construction so the diagnostic stays
    /// off the constructor tuple. Clone-per-call (the channel is cheap and low-rate).
    pub fn probe_sender(&self) -> mpsc::Sender<VisualOverlapProbeInput> {
        self.probe_tx.clone()
    }

    /// Sender for claim-based death claims (receiver drained each tick in
    /// `process_claims`). Handed to RoomManager after construction, like the probe
    /// sender, to keep the constructor tuple unchanged.
    pub fn enemy_death_claim_sender(&self) -> mpsc::Sender<EnemyDeathClaimInput> {
        self.enemy_death_claim_tx.clone()
    }

    /// Attach the match-telemetry sink. Called once by RoomManager right after
    /// construction; tests leave it unset (events become no-ops).
    pub fn attach_telemetry(&mut self, telemetry: Telemetry) {
        self.telemetry = Some(telemetry);
    }

    /// Emit one server-sourced telemetry event for this room, if a sink is attached.
    /// `room_id` is added automatically so call sites only pass the event-specific
    /// fields. No-op (and `fields` is dropped) when telemetry is unset.
    fn telemetry_event(&self, level: &str, event: &str, mut fields: serde_json::Value) {
        let Some(telemetry) = &self.telemetry else { return };
        if let Some(obj) = fields.as_object_mut() {
            obj.insert("room_id".to_string(), json!(self.id));
            obj.insert("server_tick".to_string(), json!(self.tick));
        }
        match level {
            "warn" => telemetry.server_warn(event, fields),
            _ => telemetry.server_info(event, fields),
        }
    }

    /// True only when telemetry is attached AND `FAIRTICK_TELEMETRY_VERBOSE=1`. Gate
    /// the per-tick/per-input firehose events on this.
    fn telemetry_verbose(&self) -> bool {
        self.telemetry.as_ref().map(|t| t.is_verbose()).unwrap_or(false)
    }

    pub fn tick_rate(&self) -> u64 {
        self.config.room.tick_rate
    }

    /// Allocate the next room-scoped event id. Monotonic, gap-free.
    fn alloc_event_id(&mut self) -> u64 {
        let id = self.next_event_id;
        self.next_event_id = self.next_event_id.checked_add(1).expect("event_id overflow");
        id
    }

    /// Highest event_id emitted so far. None until the first event fires.
    fn last_event_id(&self) -> Option<u64> {
        if self.next_event_id <= 1 {
            None
        } else {
            Some(self.next_event_id - 1)
        }
    }

    /// Builder for hard-event state patches — keeps Stage 5.5 emission tidy.
    fn state_patch_for(&self, player_id: &str) -> Option<StatePatch> {
        self.players.get(player_id).map(|p| StatePatch {
            position: p.position,
            direction: p.direction,
            speed: p.speed,
            is_invincible: p.is_invincible,
        })
    }

    /// Capacity-checked entry point for joining. The check and the insert happen
    /// under the caller's single `&mut self` turn (held under `room.write()`),
    /// so capacity can't be raced: a second joiner can't observe the same
    /// pre-insert count after the first has already inserted. On `Err(..)` (Full or
    /// Inactive) the caller must retry against another (or a fresh) room.
    ///
    /// `add_player` itself stays capacity-unchecked and module-private — this is
    /// the only way production code adds a player, which structurally enforces
    /// backend rule 5 ("capacity checks must happen under the same lock/actor
    /// turn that adds the player").
    pub fn try_add_player(
        &mut self,
        user_id: String,
        nickname: String,
        outbound: PlayerOutbound,
    ) -> Result<(String, ServerMessage), RoomJoinRejected> {
        // Both checks run in the SAME &mut turn as the insert below. `is_active`
        // first: a room that ended after find_or_create_room()'s read-lock snapshot
        // must not receive a player (the manager retries into a live/fresh room).
        if !self.is_active {
            tracing::warn!(
                "🚫 [ROOM {}] rejecting join for {}: room inactive (tick {}/{})",
                self.id,
                nickname,
                self.tick,
                self.game_end_tick
            );
            return Err(RoomJoinRejected::Inactive);
        }
        if self.is_full() {
            // Log HUMANS vs the human cap (what is_full actually checks) — total
            // entities would read "17/10" in a filler-populated room and send an
            // on-call straight down the wrong path.
            tracing::warn!(
                "🚫 [ROOM {}] rejecting join for {}: room full (humans {}+{} reserved / {}, {} entities)",
                self.id,
                nickname,
                self.human_count(),
                self.reserved_slots,
                self.config.room.max_players,
                self.players.len()
            );
            return Err(RoomJoinRejected::Full);
        }
        // Re-checked HERE under the same write lock that inserts (lead review): the
        // manager's read-lock pick already filters on accepting_new_joins(), but the
        // match can cross the min-remaining threshold between the two locks — the
        // race that seats someone four seconds before GameEnded.
        if !self.accepting_new_joins() {
            tracing::warn!(
                "🚫 [ROOM {}] rejecting join for {}: match too close to its end (tick {}/{})",
                self.id,
                nickname,
                self.tick,
                self.game_end_tick
            );
            return Err(RoomJoinRejected::TooLateInMatch);
        }
        Ok(self.add_player(user_id, nickname, outbound))
    }

    fn add_player(
        &mut self,
        user_id: String,
        nickname: String,
        outbound: PlayerOutbound,
    ) -> (String, ServerMessage) {
        self.admit_player(user_id, nickname, outbound, PlayerAdmission::Fresh)
    }

    /// Insert a player into the room — shared by a FRESH join and a RESUME. The admission kind
    /// drives both the spawn position (random for a fresh join, the dropped position for a
    /// resume) AND which telemetry the add emits, so a resume no longer lies: previously it
    /// reused `add_player`, logging a `player_joined` at a RANDOM spawn before apply_resume
    /// moved the entity to its real position, so telemetry showed a position the player never
    /// occupied. (review Medium: telemetry honesty on resume.)
    fn admit_player(
        &mut self,
        user_id: String,
        nickname: String,
        outbound: PlayerOutbound,
        admission: PlayerAdmission,
    ) -> (String, ServerMessage) {
        let (position, resume) = match admission {
            PlayerAdmission::Fresh => (self.map.get_random_spawn_position(&mut self.rng), None),
            // Resume: place the entity AT the dropped position up front so the random spawn is
            // never even computed (no phantom intermediate position to mis-log).
            PlayerAdmission::Resume(st) => (st.position, Some(st)),
        };
        let mut player = Player::new(user_id.clone(), nickname.clone(), position, self.config.clone());
        // Admission stamp: claim/input telemetry reports decisions as an age since THIS
        // admission ("claim 16 ticks after resume" is the ghost-death signature).
        player.admitted_at_tick = self.tick;
        player.admitted_via_resume = resume.is_some();
        if let Some(st) = resume.clone() {
            // Restore direction/score (position already set above) before anything observes the
            // entity, so the keyframe + telemetry below reflect the resumed state, not a spawn.
            player.apply_resume(st);
        }
        let player_id = player.id.clone();
        let score = player.score;

        tracing::info!(
            "🎮 [ROOM {}] {} player: {} ({}) at ({:.1}, {:.1}). Total before: {}",
            self.id,
            if resume.is_some() { "Resuming" } else { "Adding" },
            nickname,
            player_id,
            position.x,
            position.y,
            self.players.len()
        );

        self.players.insert(player_id.clone(), player);

        // Top up to the visible-population target in the SAME &mut turn as the human's
        // insert and BEFORE their outbox registers (lead review P0 #2): the first human's
        // keyframe then carries exactly `target` players (themselves + target-1 fillers),
        // and the fillers' PlayerJoined broadcasts cannot precede GameJoined on the
        // joiner's reliable lane — to them the fillers simply "were already here".
        if self.config.filler.enabled {
            self.top_up_fillers_now();
        }

        self.outbox.insert(player_id.clone(), outbound);
        self.input_slots.insert(player_id.clone(), PlayerInputSlot::default());

        // PlayerJoined carries no position, so it's correct for both paths (a resumed player
        // reappears to others as a join).
        self.outbox.broadcast_reliable(ServerMessage::PlayerJoined {
            player_id: player_id.clone(),
            nickname: self.players[&player_id].nickname.clone(),
        });

        if resume.is_some() {
            self.telemetry_event(
                "info",
                "player_resumed",
                json!({
                    "player_id": player_id,
                    "user_id": user_id,
                    "nickname": nickname,
                    // The RESTORED position/score (what the keyframe actually carries), not a spawn.
                    "x": position.x,
                    "y": position.y,
                    "score": score,
                    "reserved_slots_after": self.reserved_slots,
                    "player_count": self.players.len(),
                }),
            );
        } else {
            self.telemetry_event(
                "info",
                "player_joined",
                json!({
                    "player_id": player_id,
                    "user_id": user_id,
                    "nickname": nickname,
                    "x": position.x,
                    "y": position.y,
                    "player_count": self.players.len(),
                }),
            );
        }

        // A human just took a seat: run the rebalance in the SAME &mut turn as the insert
        // (can't race another join). Joining an ALREADY-AT-TARGET room puts it one over ⇒
        // ONE filler schedules a DELAYED exit — by construction never this tick
        // (exit_delay_min_ticks ≥ 1). A first human into a fresh room was topped up to
        // exactly target above, so no exit fires for them. A join BURST additionally
        // trips the transient cap, which expedites enough exits to shrink the overflow.
        if self.config.filler.enabled {
            self.rebalance_fillers();
            self.enforce_transient_cap();
        }

        // Build (do NOT send) an immediate keyframe. do_join_game sends it right
        // AFTER GameJoined on the same ordered channel, so the client is
        // guaranteed the order GameJoined → GameState full → subsequent events.
        // Sending it here (before GameJoined exists) could let it overtake
        // GameJoined and init the client before it knows its own player_id.
        let initial_full = self.build_initial_full();
        (player_id, initial_full)
    }

    pub fn remove_player(&mut self, player_id: &str) {
        let nickname = self.players.get(player_id).map(|p| p.nickname.clone());
        tracing::info!(
            "🚪 [ROOM {}] Removing player: {:?} ({}). Total before: {}",
            self.id,
            nickname,
            player_id,
            self.players.len()
        );

        self.players.remove(player_id);
        self.outbox.remove(player_id);
        self.input_slots.remove(player_id);

        // Drop this player_id's claim bookkeeping too. player_id is per-connection and never
        // reused (a resume mints a fresh one), so these entries can only be dead weight after a
        // remove — and a reconnect-heavy session would otherwise accumulate them until the
        // history window happened to prune. A disconnected player's in-flight claims must also
        // not resolve against the freshly-removed entity. (review Medium: per-player state leak.)
        self.last_eat_claim_id.remove(player_id);
        self.last_enemy_death_claim_id.remove(player_id);
        self.pending_eat_claims.retain(|c| c.player_id != player_id);
        self.pending_enemy_death_claims.retain(|c| c.player_id != player_id);

        // Filler bookkeeping: drop the brain, any scheduled exit, and any internal-eat
        // run/cooldown that references the entity in either role (no-ops for humans in
        // the attacker maps — they simply aren't there).
        self.filler_controllers.remove(player_id);
        self.scheduled_filler_exits.retain(|e| e.player_id != player_id);
        self.filler_eat_policy.forget_player(player_id);

        self.outbox.broadcast_reliable(ServerMessage::PlayerLeft { player_id: player_id.to_string() });

        self.telemetry_event(
            "info",
            "player_left",
            json!({
                "player_id": player_id,
                "nickname": nickname,
                "player_count": self.players.len(),
            }),
        );
    }

    // ---- Filler bots (fake players; brains in game::filler_bot) -------------------------
    //
    // The contract (lead manifesto 2026-06-11): a filler IS a Player — same entity map, same
    // simulation, same wire PlayerState. No is_bot on the wire, no separate list, no "bot1"
    // names, no special color. The room owns only the LIFECYCLE (spawn / delayed exit /
    // rebalance) and the per-tick drive that turns brain output into set_direction — the
    // exact mutation an applied human MoveCommand performs. Gameplay code past this point
    // never branches on ActorKind; only rewards/capacity/persistence do.

    /// Population target: `config.filler.target_visible_players`, capped by room capacity.
    fn filler_target(&self) -> usize {
        self.config.filler.target_visible_players.min(self.config.room.max_players)
    }

    fn dist_px(a: &Position, b: &Position) -> f32 {
        ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
    }

    fn dist_to_nearest_human(&self, pos: &Position) -> Option<f32> {
        self.players
            .values()
            .filter(|p| !p.is_filler())
            .map(|p| Self::dist_px(pos, &p.position))
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Synchronous fill to target at a human admission: spawns `target - population`
    /// fillers in the caller's &mut turn, so the admission keyframe carries exactly
    /// `target` players. Population counts the just-inserted human, reserved resume
    /// slots, and existing fillers. No exits are scheduled here — over-target is the
    /// rebalance's business.
    fn top_up_fillers_now(&mut self) {
        let target = self.filler_target();
        let population = self.human_count() + self.reserved_slots + self.filler_count();
        for _ in population..target {
            self.spawn_filler();
        }
    }

    /// A pool nickname not currently worn by anyone in the room (humans included — a
    /// filler shadowing the human's own name is a tell). Pool exhausted ⇒ short numeric
    /// suffix, still uniqueness-checked. All draws via RoomRng (replay-stable).
    fn pick_filler_nickname(&mut self) -> String {
        let used: HashSet<&str> = self.players.values().map(|p| p.nickname.as_str()).collect();
        let free: Vec<&&str> = FILLER_NICKNAMES.iter().filter(|n| !used.contains(*n)).collect();
        if !free.is_empty() {
            return free[self.rng.range_usize(0..free.len())].to_string();
        }
        // 180-name pool exhausted (only plausible in stress tests): suffix a base name.
        loop {
            let base = FILLER_NICKNAMES[self.rng.range_usize(0..FILLER_NICKNAMES.len())];
            let cand = format!("{}{}", base, self.rng.range_usize(2..100));
            if !self.players.values().any(|p| p.nickname == cand) {
                return cand;
            }
        }
    }

    /// A spawn position at least `FILLER_SPAWN_MIN_HUMAN_DIST_PX` from every human, or
    /// None if the tries budget found nothing (caller postpones the spawn). Used for
    /// RUNTIME replacements only — admission top-ups ride the join keyframe, where
    /// "already standing there" is exactly what the joiner expects.
    fn spawn_position_clear_of_humans(&mut self) -> Option<Position> {
        for _ in 0..FILLER_SPAWN_CLEAR_TRIES {
            let pos = self.map.get_random_spawn_position(&mut self.rng);
            let clear =
                self.dist_to_nearest_human(&pos).map(|d| d >= FILLER_SPAWN_MIN_HUMAN_DIST_PX).unwrap_or(true);
            if clear {
                return Some(pos);
            }
        }
        None
    }

    /// Insert one filler: an ordinary Player (kind = FillerBot) + its brain. NO outbox
    /// entry and NO input slot — there is no transport to fake; the brain drives
    /// `set_direction` directly in `drive_filler_bots`.
    fn spawn_filler(&mut self) {
        let position = self.map.get_random_spawn_position(&mut self.rng);
        self.spawn_filler_at(position);
    }

    fn spawn_filler_at(&mut self, position: Position) {
        let nickname = self.pick_filler_nickname();
        // "filler:" prefix can never collide with a real account id — a structural second
        // line of defense (beyond ActorKind) for the reward/persistence filter.
        let user_id = format!("filler:{}", Uuid::new_v4().simple());
        let mut player = Player::new(user_id, nickname.clone(), position, self.config.clone());
        player.kind = ActorKind::FillerBot;
        player.admitted_at_tick = self.tick;
        let player_id = player.id.clone();
        self.players.insert(player_id.clone(), player);
        self.filler_controllers.insert(player_id.clone(), BotController::new(&mut self.rng, self.tick));

        // To everyone already connected this is indistinguishable from a real join.
        self.outbox.broadcast_reliable(ServerMessage::PlayerJoined {
            player_id: player_id.clone(),
            nickname: nickname.clone(),
        });
        self.telemetry_event(
            "info",
            "filler_spawned",
            json!({
                "player_id": player_id,
                "nickname": nickname,
                "filler_count": self.filler_count(),
                "human_count": self.human_count(),
            }),
        );
    }

    /// Pick ONE filler and schedule its exit. Candidate scoring (manifesto): prefer
    /// far-from-every-human and low in-room score — the human should rarely SEE the
    /// leaver go, and never lose the leaderboard head they were watching. Never removes
    /// immediately; `expedited` only narrows the delay band (burst overflow). Returns
    /// false when every filler is already scheduled (nothing to pick).
    fn schedule_filler_exit(&mut self, expedited: bool) -> bool {
        let already: HashSet<&String> = self.scheduled_filler_exits.iter().map(|e| &e.player_id).collect();
        let mut best: Option<(f32, String, f32)> = None; // (score, id, dist_to_human)
        for p in self.players.values() {
            if !p.is_filler() || already.contains(&p.id) {
                continue;
            }
            let dist = self.dist_to_nearest_human(&p.position).unwrap_or(f32::MAX);
            // Distance dominates; low score breaks ties (don't evaporate a leader).
            let score = dist - 0.5 * p.score as f32;
            if best.as_ref().map(|(b, _, _)| score > *b).unwrap_or(true) {
                best = Some((score, p.id.clone(), dist));
            }
        }
        let Some((_, id, dist)) = best else { return false };
        let (min, max) = if expedited {
            (FILLER_EXPEDITED_EXIT_MIN_TICKS, FILLER_EXPEDITED_EXIT_MAX_TICKS)
        } else {
            let min = self.config.filler.exit_delay_min_ticks.max(1) as usize;
            (min, (self.config.filler.exit_delay_max_ticks as usize).max(min + 1))
        };
        let delay = self.rng.range_usize(min..max) as u64;
        self.scheduled_filler_exits.push(ScheduledFillerExit {
            player_id: id.clone(),
            at_tick: self.tick + delay,
            expedited,
            postpones: 0,
        });
        self.telemetry_event(
            "info",
            "filler_exit_scheduled",
            json!({
                "player_id": id,
                "delay_ticks": delay,
                "expedited": expedited,
                "dist_to_nearest_human": if dist.is_finite() { Some(dist) } else { None },
            }),
        );
        true
    }

    /// Human-burst ceiling (lead review P1): humans always seat, but TOTAL entities past
    /// `max_transient_visible_players` must shrink FAST — enough of the scheduled exits
    /// get upgraded to expedited (short delay, no postpone recheck), and new expedited
    /// exits are scheduled for any remainder. Called in the admission turn.
    fn enforce_transient_cap(&mut self) {
        let cap = self.config.filler.max_transient_visible_players.max(1);
        let total = self.player_count();
        if total <= cap {
            return;
        }
        let already_fast = self.scheduled_filler_exits.iter().filter(|e| e.expedited).count();
        let mut need = (total - cap).saturating_sub(already_fast);
        if need == 0 {
            return;
        }
        // Upgrade polite exits first — they were going anyway, just not fast enough.
        for i in 0..self.scheduled_filler_exits.len() {
            if need == 0 {
                break;
            }
            if self.scheduled_filler_exits[i].expedited {
                continue;
            }
            let short = self.tick
                + self.rng.range_usize(FILLER_EXPEDITED_EXIT_MIN_TICKS..FILLER_EXPEDITED_EXIT_MAX_TICKS)
                    as u64;
            let e = &mut self.scheduled_filler_exits[i];
            e.expedited = true;
            e.at_tick = e.at_tick.min(short);
            need -= 1;
        }
        // Schedule fresh expedited exits for the rest.
        while need > 0 && self.schedule_filler_exit(true) {
            need -= 1;
        }
    }

    /// Throttled population check: drift the (humans + reserved + fillers) total toward
    /// `filler_target`, ONE step per round — exits and replacements trickle like real
    /// players, never in a batch. Reserved resume slots count as humans so a reconnect
    /// window doesn't churn a filler out and back.
    fn rebalance_fillers(&mut self) {
        if !self.config.filler.enabled || !self.is_active {
            return;
        }
        let humans = self.human_count() + self.reserved_slots;
        if humans == 0 {
            // An uninhabited room never tops up — the empty-room sweep owns it.
            return;
        }
        let fillers = self.filler_count();
        let exiting = self.scheduled_filler_exits.len().min(fillers);
        let arriving = self.pending_filler_spawns.len();
        let projected = humans + fillers + arriving - exiting;
        let target = self.filler_target();
        if projected > target && fillers > exiting {
            self.schedule_filler_exit(false);
        } else if projected < target {
            let delay =
                self.rng.range_usize(FILLER_SPAWN_DELAY_MIN_TICKS..FILLER_SPAWN_DELAY_MAX_TICKS) as u64;
            self.pending_filler_spawns.push(self.tick + delay);
        }
    }

    /// Per-tick lifecycle: the empty-room sweep, due exits (with the near-human
    /// postpone recheck), due spawns (re-checked against target, spawned clear of
    /// humans or re-queued), and the throttled rebalance.
    fn process_filler_lifecycle(&mut self, current_tick: u64) {
        if !self.config.filler.enabled {
            return;
        }

        // Empty-room sweep (lead review P1): no humans, no resume holds — keep the
        // fillers warm through a short grace (someone may be matchmaking in right now),
        // then sweep and deactivate so a filler-only room can't simulate until game_end.
        if self.human_count() == 0 && self.reserved_slots == 0 && self.filler_count() > 0 {
            let since = *self.humans_absent_since_tick.get_or_insert(current_tick);
            if current_tick.saturating_sub(since) >= self.config.filler.empty_room_grace_ticks {
                let swept = self.filler_count();
                self.players.retain(|_, p| !p.is_filler());
                self.filler_controllers.clear();
                self.scheduled_filler_exits.clear();
                self.pending_filler_spawns.clear();
                self.is_active = false;
                tracing::info!(
                    "🧹 [ROOM {}] swept {} fillers from an uninhabited room and deactivated (tick {})",
                    self.id,
                    swept,
                    current_tick
                );
                self.telemetry_event(
                    "info",
                    "filler_room_swept",
                    json!({ "swept_fillers": swept, "grace_ticks": self.config.filler.empty_room_grace_ticks }),
                );
                return;
            }
        } else {
            self.humans_absent_since_tick = None;
        }

        // Due exits — each polite one re-checks visibility AT THE EXIT TICK (lead review
        // P1): the leaver may have wandered next to a human since scheduling, and a
        // "player" must not evaporate in front of someone. Postpone up to the cap, then
        // leave regardless (the room stays over target forever otherwise).
        if self.scheduled_filler_exits.iter().any(|e| e.at_tick <= current_tick) {
            let mut due: Vec<ScheduledFillerExit> = Vec::new();
            self.scheduled_filler_exits.retain(|e| {
                if e.at_tick <= current_tick {
                    due.push(e.clone());
                    false
                } else {
                    true
                }
            });
            for e in due {
                let near_human = self
                    .players
                    .get(&e.player_id)
                    .and_then(|p| self.dist_to_nearest_human(&p.position))
                    .map(|d| d < FILLER_EXIT_NEAR_HUMAN_PX)
                    .unwrap_or(false);
                if !e.expedited && near_human && e.postpones < FILLER_EXIT_MAX_POSTPONES {
                    let delay =
                        self.rng.range_usize(FILLER_EXIT_POSTPONE_MIN_TICKS..FILLER_EXIT_POSTPONE_MAX_TICKS)
                            as u64;
                    self.telemetry_event(
                        "info",
                        "filler_exit_postponed_near_human",
                        json!({
                            "player_id": e.player_id,
                            "postpones": e.postpones + 1,
                            "retry_in_ticks": delay,
                        }),
                    );
                    self.scheduled_filler_exits.push(ScheduledFillerExit {
                        at_tick: current_tick + delay,
                        postpones: e.postpones + 1,
                        ..e
                    });
                    continue;
                }
                self.filler_controllers.remove(&e.player_id);
                if self.players.contains_key(&e.player_id) {
                    self.remove_player(&e.player_id);
                    self.telemetry_event(
                        "info",
                        "filler_removed",
                        json!({ "player_id": e.player_id, "expedited": e.expedited }),
                    );
                }
            }
        }

        // Due spawns — re-check the deficit (a human may have arrived while queued) and
        // spawn CLEAR of humans; no clear spot ⇒ postpone, never materialize on-screen.
        if self.pending_filler_spawns.iter().any(|at| *at <= current_tick) {
            let due = self.pending_filler_spawns.iter().filter(|at| **at <= current_tick).count();
            self.pending_filler_spawns.retain(|at| *at > current_tick);
            for _ in 0..due {
                let population = self.human_count() + self.reserved_slots + self.filler_count();
                if population >= self.filler_target() {
                    continue;
                }
                match self.spawn_position_clear_of_humans() {
                    Some(pos) => self.spawn_filler_at(pos),
                    None => {
                        let delay = self
                            .rng
                            .range_usize(FILLER_SPAWN_POSTPONE_MIN_TICKS..FILLER_SPAWN_POSTPONE_MAX_TICKS)
                            as u64;
                        self.telemetry_event(
                            "info",
                            "filler_spawn_postponed_no_safe_position",
                            json!({ "retry_in_ticks": delay }),
                        );
                        self.pending_filler_spawns.push(current_tick + delay);
                    }
                }
            }
        }

        if current_tick >= self.next_filler_rebalance_tick {
            self.next_filler_rebalance_tick = current_tick + FILLER_REBALANCE_INTERVAL_TICKS;
            self.rebalance_fillers();
        }
    }

    /// Give each filler brain its bounded perception view and apply any emitted turn via
    /// `Player::set_direction` — at the same stage of the tick where human inputs apply,
    /// so brains and humans steer under identical rules.
    fn drive_filler_bots(&mut self, current_tick: u64) {
        if self.filler_controllers.is_empty() {
            return;
        }
        // mem::take splits the borrows: controllers (mut) vs world (shared) vs rng (mut).
        let mut controllers = std::mem::take(&mut self.filler_controllers);
        // Deterministic order: brains draw from RoomRng, so iteration order is replay-relevant.
        let mut ids: Vec<String> = controllers.keys().cloned().collect();
        ids.sort();
        // Goal transitions surfaced this tick (verbose-only telemetry, emitted post-loop).
        let verbose = self.telemetry_verbose();
        let mut goal_changes: Vec<(String, &'static str, &'static str)> = Vec::new();
        for id in &ids {
            let Some(ctrl) = controllers.get_mut(id) else { continue };
            let Some(me) = self.players.get(id) else { continue };
            let me_pos = me.position;
            let me_score = me.score;
            let radius = ctrl.personality.perception_radius_px;

            // Bounded perception (manifesto): the bot sees ONLY what's inside its radius —
            // other players (human or filler — it cannot tell), PvE enemies (as threats),
            // and nearby pickups. No map-wide omniscience.
            let mut actors: Vec<SeenActor> = Vec::new();
            for p in self.players.values() {
                if p.id != *id && Self::dist_px(&me_pos, &p.position) <= radius {
                    actors.push(SeenActor {
                        id: p.id.clone(),
                        pos: p.position,
                        score: p.score,
                        is_pve_enemy: false,
                    });
                }
            }
            for e in &self.enemies {
                if Self::dist_px(&me_pos, &e.position) <= radius {
                    actors.push(SeenActor {
                        id: e.id.clone(),
                        pos: e.position,
                        score: e.score,
                        is_pve_enemy: true,
                    });
                }
            }
            let points: Vec<(String, Position)> = self
                .points
                .iter()
                .filter(|pt| Self::dist_px(&me_pos, &pt.position) <= radius)
                .map(|pt| (pt.id.clone(), pt.position))
                .collect();
            let boosters: Vec<(String, Position)> = self
                .boosters
                .iter()
                .filter(|b| Self::dist_px(&me_pos, &b.position) <= radius)
                .map(|b| (b.id.clone(), b.position))
                .collect();

            let view = BotView {
                tick: current_tick,
                me_pos,
                me_score,
                actors: &actors,
                points: &points,
                boosters: &boosters,
                map: &self.map,
            };
            if let Some(dir) = ctrl.tick(&view, &mut self.rng) {
                if let Some(p) = self.players.get_mut(id) {
                    p.set_direction(dir);
                }
            }
            if verbose {
                if let Some((from, to)) = ctrl.take_goal_change() {
                    goal_changes.push((id.clone(), from, to));
                }
            }
        }
        // Drop brains whose entity vanished mid-take (defensive; exits remove both together).
        controllers.retain(|id, _| self.players.contains_key(id));
        self.filler_controllers = controllers;
        for (id, from, to) in goal_changes {
            self.telemetry_event(
                "info",
                "filler_goal_changed",
                json!({ "player_id": id, "from": from, "to": to }),
            );
        }
    }

    /// Filler internal eats (lead review P0 #1): the server-side stand-in for the EatClaim
    /// a filler has no client to send. Per tick, each filler's nearest WEAKER overlapping
    /// player feeds [`FillerEatPolicy`]; on a sustained, fully-gated overlap the eat goes
    /// through the SAME `handle_player_eaten_by_claim` path (score transfer, PlayerEaten
    /// event, respawn, claim-ledger kill record) an accepted human claim uses.
    ///
    /// TIMELINE: both positions are CURRENT SERVER-TICK (a filler has no render timeline);
    /// the victim-side fairness comes from the policy's conservatism gates, not from
    /// reconstruction — see the policy module doc.
    fn process_filler_internal_eats(&mut self) {
        if !self.config.filler.enabled {
            return;
        }
        // Deterministic attacker order (replay): eats mutate scores and draw event ids.
        // The LIFE each attacker held at list build is captured too — an attacker that
        // gets eaten by an earlier attacker in this same pass respawns with a new life,
        // and the respawned life must NOT inherit this pass's attack slot. (lead review)
        let mut filler_ids: Vec<(String, u32)> = self
            .players
            .values()
            .filter(|p| p.is_filler() && p.is_alive)
            .map(|p| (p.id.clone(), p.life_id))
            .collect();
        if filler_ids.is_empty() {
            return;
        }
        filler_ids.sort();
        let radius = self.filler_eat_policy.rules().overlap_radius_px;
        let tick = self.tick;

        for (attacker_id, attacker_life_at_start) in &filler_ids {
            let Some(att) = self.players.get(attacker_id) else { continue };
            // Attacker-side gates, re-checked at ATTACK time (state may have changed
            // earlier in this very pass): still alive, still the SAME life (not eaten +
            // respawned mid-pass), and not spawn-protected (a fresh respawn shouldn't
            // kill on its first tick back any more than it can be killed).
            if !att.is_alive || att.life_id != *attacker_life_at_start || att.is_spawn_protected(tick) {
                continue;
            }
            let (att_pos, att_score) = (att.position, att.score);

            // Nearest WEAKER player inside the overlap radius (humans and fillers alike —
            // strictly lower score; equals can't eat each other, same as the claim rule).
            let mut best: Option<(f32, &Player)> = None;
            for v in self.players.values() {
                if v.id == *attacker_id || v.score >= att_score {
                    continue;
                }
                let d = Self::dist_px(&att_pos, &v.position);
                if d <= radius && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best = Some((d, v));
                }
            }
            let candidate = best.map(|(d, v)| FillerEatCandidate {
                victim_id: v.id.clone(),
                victim_life_id: v.life_id,
                dist_px: d,
                victim_is_human: !v.is_filler(),
                victim_alive: v.is_alive,
                victim_spawn_protected: v.is_spawn_protected(tick),
                victim_invincible: v.is_invincible,
                victim_resume_shielded: v.resume_shield_active(tick, RESUME_SHIELD_TICKS),
                victim_claim_ready: v.claim_ready,
                victim_admission_age_ticks: tick.saturating_sub(v.admitted_at_tick),
                either_in_safe_zone: self.map.is_in_safe_zone(&att_pos)
                    || self.map.is_in_safe_zone(&v.position),
                attacker_score: att_score,
                victim_score: v.score,
            });

            match self.filler_eat_policy.evaluate(tick, attacker_id, candidate.as_ref()) {
                FillerEatVerdict::NotYet => {}
                FillerEatVerdict::Reject(reason) => {
                    let c = candidate.as_ref().expect("reject implies a candidate");
                    self.telemetry_event(
                        "info",
                        "filler_internal_eat_reject",
                        json!({
                            "attacker_id": attacker_id,
                            "victim_id": c.victim_id,
                            "reason": reason,
                            "dist": c.dist_px,
                            "attacker_score": c.attacker_score,
                            "victim_score": c.victim_score,
                            "victim_is_human": c.victim_is_human,
                        }),
                    );
                }
                FillerEatVerdict::Eat => {
                    let c = candidate.as_ref().expect("eat implies a candidate");
                    let victim_id = c.victim_id.clone();
                    let victim_life = c.victim_life_id;
                    let victim_pos = self.players.get(&victim_id).map(|v| v.position).unwrap_or(att_pos);
                    tracing::info!(
                        "💀 FillerInternalEat ACCEPT room={} eater={} victim={} tick={} dist={:.1} sustained={} (server-tick timeline)",
                        self.id, attacker_id, victim_id, tick, c.dist_px,
                        FILLER_EAT_SUSTAINED_TICKS
                    );
                    self.telemetry_event(
                        "info",
                        "filler_internal_eat_accept",
                        json!({
                            "attacker_id": attacker_id,
                            "victim_id": victim_id,
                            "dist": c.dist_px,
                            "sustained_ticks": FILLER_EAT_SUSTAINED_TICKS,
                            "attacker_score": c.attacker_score,
                            "victim_score": c.victim_score,
                            "victim_is_human": c.victim_is_human,
                            "timeline": "server_tick",
                        }),
                    );
                    // INTERNAL_FILLER_CLAIM_ID marks the missing client claim; the rest
                    // of the path — score transfer, PlayerEaten, respawn — is the claim path.
                    self.handle_player_eaten_by_claim(
                        attacker_id,
                        &victim_id,
                        INTERNAL_FILLER_CLAIM_ID,
                        c.dist_px,
                        att_pos,
                        victim_pos,
                    );
                    // Ledger parity with accepted claims: this life is dead — a later
                    // stale human claim against it must reject as kill-by-corpse.
                    self.claim_ledger.kill_player_life(&victim_id, victim_life, tick as f64);
                    self.filler_eat_policy.note_eat_applied(tick, attacker_id);
                }
            }
        }
    }

    // ---- end filler bots ------------------------------------------------------------------

    /// Route one async command into the room's buffers. The SINGLE handler the inbound
    /// channels (and channel-free replay tests) funnel through.
    pub fn apply_command(&mut self, command: RoomCommand) {
        match command {
            RoomCommand::PlayerInput(input) => self.ingest_player_input(input),
            RoomCommand::EatClaim(input) => self.ingest_eat_claim(input),
            RoomCommand::EnemyDeathClaim(input) => self.ingest_enemy_death_claim(input),
            RoomCommand::VisualOverlapProbe(probe) => self.handle_visual_overlap_probe(probe),
        }
    }

    /// Buffer a player input into its per-player slot (applied later at its target_tick,
    /// not on receive) and update the input audit counters.
    fn ingest_player_input(&mut self, input: PlayerInput) {
        let current = self.tick;
        // A retransmit of a seq we ALREADY processed (and removed from pending) is just a
        // duplicate — count it as such, not as a late input. Otherwise accepted_late is
        // inflated by harmless resends of old, applied commands.
        if let Some(player) = self.players.get(&input.player_id) {
            if input.seq <= player.last_processed_input_seq {
                self.input_duplicate = self.input_duplicate.saturating_add(1);
                return;
            }
        }
        // Captured before `input` is moved into submit, so an accepted input can record its
        // OBSERVE-ONLY lead sample (target_tick scheduled ahead of `current` ≈ the client lead).
        let player_id = input.player_id.clone();
        let target_tick = input.target_tick;
        let result = match self.input_slots.get_mut(&input.player_id) {
            Some(slot) => slot.submit(input, current),
            None => return, // unknown player (already removed)
        };
        match result {
            SubmitResult::RejectedFuture => {
                self.input_future_rejected = self.input_future_rejected.saturating_add(1)
            }
            SubmitResult::RejectedTooLate => {
                self.input_late_rejected = self.input_late_rejected.saturating_add(1)
            }
            SubmitResult::ReplacedRetransmit => self.input_duplicate = self.input_duplicate.saturating_add(1),
            SubmitResult::Full => self.input_coalesced_count = self.input_coalesced_count.saturating_add(1),
            SubmitResult::AcceptedLate { age_ticks } => {
                self.input_late_accepted = self.input_late_accepted.saturating_add(1);
                self.input_late_accepted_max_age = self.input_late_accepted_max_age.max(age_ticks);
                if let Some(p) = self.players.get_mut(&player_id) {
                    p.note_input_lead(target_tick, current);
                }
            }
            SubmitResult::Accepted => {
                if let Some(p) = self.players.get_mut(&player_id) {
                    p.note_input_lead(target_tick, current);
                }
            }
        }
    }

    /// Buffer an eat claim into the pending ring (validated later in process_claims).
    fn ingest_eat_claim(&mut self, input: EatClaimInput) {
        if self.pending_eat_claims.len() >= EAT_CLAIM_PENDING_CAP {
            // Reject the INCOMING claim — never drop another player's queued one.
            self.reject_eat_claim(&input.player_id, input.claim.claim_id, "claim_pending_full");
            return;
        }
        self.pending_eat_claims.push(input);
    }

    /// Buffer a claim-based death into the pending ring (validated later in
    /// process_claims). Bounded globally (memory) AND per-player (anti-spam).
    fn ingest_enemy_death_claim(&mut self, input: EnemyDeathClaimInput) {
        if self.pending_enemy_death_claims.len() >= EAT_CLAIM_PENDING_CAP {
            self.reject_enemy_death_claim(&input.player_id, input.claim.claim_id, "claim_pending_full");
            return;
        }
        // Per-player anti-spam (round-3 follow-up): the death-claim channel is RELIABLE, so a
        // malicious client could flood EnemyDeathClaim and crowd the shared pending ring. An honest
        // client predicts ONE death at a time (its EnemyDeathClaimController gates a single in-flight
        // claim), so a tiny per-player budget is generous; over it we reject the INCOMING claim (tell
        // the client to back off) and never drop another player's queued one.
        let mine = self.pending_enemy_death_claims.iter().filter(|c| c.player_id == input.player_id).count();
        if mine >= MAX_PENDING_DEATH_CLAIMS_PER_PLAYER {
            self.reject_enemy_death_claim(
                &input.player_id,
                input.claim.claim_id,
                "too_many_pending_death_claims",
            );
            return;
        }
        // RECEIPT is its own fact (lead manifesto #5): pairs the client's claim_sent with
        // the server's intake — with the admission-age/readiness facts the later decision
        // is judged against. Claims are rare (one per overlap episode), so unthrottled.
        let (world_ready, age_since_admit, via_resume) = self
            .players
            .get(&input.player_id)
            .map(|p| (p.world_ready, self.tick.saturating_sub(p.admitted_at_tick), p.admitted_via_resume))
            .unwrap_or((false, 0, false));
        self.telemetry_event(
            "info",
            "server_claim_received",
            json!({
                "claim_kind": "enemy_ate_player",
                "claim_id": input.claim.claim_id,
                "player_id": input.player_id,
                "enemy_id": input.claim.enemy_id,
                "server_tick": self.tick,
                "victim_render_tick": input.claim.victim_render_tick,
                "world_ready_seen": world_ready,
                "age_since_admit_ticks": age_since_admit,
                "admitted_via_resume": via_resume,
            }),
        );
        self.pending_enemy_death_claims.push(input);
    }

    /// Is a death claim for this exact (victim, killer enemy, generation) still waiting to resolve?
    /// Existence-only; the fallback uses [`has_plausible_pending_enemy_death_claim`] instead. Used by
    /// tests to distinguish "in the ring" from "plausible enough to hold the fallback".
    #[cfg(test)]
    fn has_pending_enemy_death_claim(&self, player_id: &str, enemy_id: &str, generation: u32) -> bool {
        self.pending_enemy_death_claims.iter().any(|c| {
            c.player_id == player_id && c.claim.enemy_id == enemy_id && c.claim.enemy_generation == generation
        })
    }

    /// How far AHEAD of the current sim tick a pending death claim's `victim_render_tick` may sit and
    /// still count as a fallback blocker. MATCHES the scheduling window (`death_claim_max_future_ticks`
    /// = lead + slack), NOT just `lead`: the resolver HOLDS any claim within the future cap, so the
    /// fallback must treat that same band as plausible — otherwise a late-arriving honest claim in the
    /// (lead, lead+slack] window could be pre-empted by a server kill. Widening it can't reintroduce
    /// the immortality stall, because the hold is still time-boxed by `DEATH_CLAIM_PENDING_GRACE_TICKS`.
    /// (round-3 follow-up)
    fn death_claim_hold_lead_ticks(&self) -> f64 {
        self.death_claim_max_future_ticks() as f64
    }

    /// Reject (don't even schedule) a death claim whose `victim_render_tick` is more than this far in
    /// the future = the client's prediction-lead cap + a little clock-jitter slack. TIGHTER than the
    /// generic `MAX_FUTURE_INPUT_TICKS` (60); strictly above the fallback hold lead so a still-holdable
    /// claim is never rejected first. (round-3 #2 / follow-up)
    fn death_claim_max_future_ticks(&self) -> u64 {
        self.config.network.max_client_prediction_lead_ticks + DEATH_CLAIM_FUTURE_SLACK_TICKS
    }

    /// Largest honest `victim_render_tick - enemy_render_tick` (render skew) a death claim may carry.
    /// The victim is predicted AHEAD by up to the client's prediction-lead cap, and the killer enemy
    /// is interpolated BEHIND by the enemy interp delay, so the honest skew is ~lead + interp; the
    /// jitter slack mirrors the future cap. CONFIG-DERIVED, not a 30-tick const, so it can't drift
    /// below lead + interp and reject an honest high-RTT claim (which would also let the fallback
    /// kill a player whose real claim was in flight). (round-3 follow-up — Blocker)
    fn death_claim_max_skew_ticks(&self) -> f64 {
        self.config.network.max_client_prediction_lead_ticks as f64
            + self.config.death_fairness.enemy_interp_delay_ticks as f64
            + DEATH_CLAIM_FUTURE_SLACK_TICKS as f64
    }

    /// Contact-tick ceiling on the fallback HOLD: past this many ticks of sustained lethal contact the
    /// anti-cheat fallback fires even with a plausible claim pending (so a never-resolving / refresh-
    /// spammed claim can't grant immortality). It must be at least `death_claim_max_future_ticks + 1`,
    /// because a claim accepted into pending as plausible can sit up to that far ahead before the sim
    /// reaches its render tick and `process_claims` can resolve it — pre-empting it earlier would kill
    /// a player whose honest claim simply hadn't become READY yet. The usual bound is the grace sum
    /// (`SERVER_FALLBACK_KILL_TICKS` + `DEATH_CLAIM_PENDING_GRACE_TICKS`); the `.max` keeps the two in
    /// lockstep if the config lead grows. (round-3 follow-up — Blocker)
    fn death_claim_fallback_hold_cap_ticks(&self) -> u32 {
        let pending_ready_cap = self.death_claim_max_future_ticks().saturating_add(1) as u32;
        (SERVER_FALLBACK_KILL_TICKS + DEATH_CLAIM_PENDING_GRACE_TICKS).max(pending_ready_cap)
    }

    /// How far behind the current sim tick a causal-ledger entry stays live before pruning: the
    /// history ring span plus the largest future a still-acceptable claim can name. The future term
    /// is `max(MAX_FUTURE_INPUT_TICKS, death_claim_max_future_ticks())` — config-derived, NOT a const,
    /// so if the prediction lead grows the ledger can't start pruning entries a late death/eat claim
    /// could still legitimately reference. (round-3 #1 follow-up)
    fn claim_ledger_retain_ticks(&self) -> f64 {
        self.config.claim_fairness.history_ticks as f64
            + MAX_FUTURE_INPUT_TICKS.max(self.death_claim_max_future_ticks()) as f64
    }

    /// Does a PLAUSIBLE pending death claim for this exact (victim, killer enemy, generation) sit in
    /// the ring? `process_claims` (run earlier in the tick) holds claims whose victim_render_tick the
    /// sim hasn't reached yet, so an in-flight claim waits here — and the anti-cheat fallback gives it
    /// a bounded grace to resolve instead of pre-empting it with a duplicate server kill.
    ///
    /// "Plausible" is the guard against an immortality stall (round-3 #2): a bare existence check let
    /// a cheater hold the fallback forever by spamming junk/far-future claims. To count, a pending
    /// claim must be near-future (`victim_render_tick` within `death_claim_hold_lead_ticks` — which now
    /// equals the scheduling window, lead + slack — of now) AND internally consistent (`shape` +
    /// `positions_consistent` pass, the same gates the resolver applies first). Even a plausible claim
    /// only delays the fallback up to `death_claim_fallback_hold_cap_ticks` (see the caller); this just
    /// stops an *implausible* one from getting even that grace.
    fn has_plausible_pending_enemy_death_claim(
        &self,
        player_id: &str,
        enemy_id: &str,
        generation: u32,
    ) -> bool {
        let max_lead_tick = self.tick as f64 + self.death_claim_hold_lead_ticks();
        let max_skew = self.death_claim_max_skew_ticks();
        let cf = &self.config.claim_fairness;
        self.pending_enemy_death_claims.iter().any(|c| {
            c.player_id == player_id
                && c.claim.enemy_id == enemy_id
                && c.claim.enemy_generation == generation
                && c.claim.victim_render_tick <= max_lead_tick
                && EnemyDeathClaimPolicy::shape(
                    cf,
                    c.claim.visual_distance,
                    c.claim.victim_render_tick,
                    c.claim.enemy_render_tick,
                    max_skew,
                )
                .is_ok()
                && EnemyDeathClaimPolicy::positions_consistent(
                    cf,
                    Self::distance(&c.claim.victim_position, &c.claim.enemy_position),
                    c.claim.visual_distance,
                )
                .is_ok()
        })
    }

    /// OBSERVE-ONLY: log the server's view of a client-reported survived overlap
    /// (`visual_overlap_without_death`) so "drove through it and lived" can be correlated
    /// server↔client. Reads live state + history; decides and mutates NOTHING. (lead manifesto)
    fn handle_visual_overlap_probe(&mut self, probe: VisualOverlapProbeInput) {
        let VisualOverlapProbeInput { player_id, enemy_id, enemy_generation, known_server_tick } = probe;
        let Some(player) = self.players.get(&player_id) else { return };

        let player_now = player.position;
        let player_invincible = player.is_invincible;
        let player_spawn_protected = player.is_spawn_protected(self.tick);
        let player_invincibility_end_tick = player.invincibility_end_tick;
        let player_spawn_protected_until_tick = player.spawn_protected_until_tick;
        let in_safe_zone = self.map.is_in_safe_zone(&player_now);
        let (player_vel_x, player_vel_y) = player.velocity();
        let lead = player.estimated_client_lead_ticks();
        // The server-timeline contact run is keyed on (enemy id, generation) — only counts if it's
        // THIS enemy life the client probed.
        let contact_on_enemy = player.death_contact_enemy.as_deref() == Some(enemy_id.as_str())
            && player.death_contact_generation == enemy_generation;
        let contact_ticks = if contact_on_enemy { player.death_contact_ticks } else { 0 };

        // Current enemy. "alive" here = present AT the probed generation (a newer generation means
        // the bot the client saw has since respawned into a different life).
        let enemy = self.enemies.iter().find(|e| e.id == enemy_id);
        let enemy_present_same_gen = enemy.is_some_and(|e| e.generation == enemy_generation);
        let enemy_can_eat = enemy.is_some_and(|e| e.can_eat(player));
        let server_dist_now = enemy.map(|e| Self::distance(&player_now, &e.position));

        // Server distance at the tick the client thought it saw the overlap (both from history).
        let server_dist_at_known_tick = match (
            self.contact_history.sample_player(&player_id, known_server_tick as f64),
            self.contact_history.sample_enemy(&enemy_id, Some(enemy_generation), known_server_tick as f64),
        ) {
            (Some(p), Some(e)) => Some(Self::distance(&p.position, &e.position)),
            _ => None,
        };

        // The two death-gate legs reconstructed at NOW (same math as check_enemy_collisions).
        let interp = self.config.death_fairness.enemy_interp_delay_ticks;
        let visible_tick = self.tick.saturating_sub(interp);
        let enemy_visible_pos = self
            .contact_history
            .sample_enemy(&enemy_id, Some(enemy_generation), visible_tick as f64)
            .map(|h| h.position);
        let reconstructed = enemy_visible_pos.map(|ep| Self::distance(&player_now, &ep));
        let tick_rate = self.config.room.tick_rate as f32;
        let projected = match (lead, enemy_visible_pos) {
            (Some(l), Some(ep)) => {
                let dt = l as f32 / tick_rate;
                Some(Self::distance(
                    &Position { x: player_now.x + player_vel_x * dt, y: player_now.y + player_vel_y * dt },
                    &ep,
                ))
            }
            _ => None,
        };

        let confirm_radius = self.config.death_fairness.visible_confirm_radius_px();
        let kill_radius = EnemyContactPolicy::kill_radius(self.config.collision.collision_distance_px);

        // Shield breakdown — split the bare "shielded" into the SPECIFIC protection so a reader
        // (and the client's UX) can act on it rather than guess. The player is the shielded entity
        // (these stop the enemy eating US). enemy_spawn_shield is the bot's just-respawned eat-grace
        // (informational; it gates player-eats-enemy, not this direction).
        let player_boost_invincible = player_invincible;
        let player_respawn_shield = player_spawn_protected;
        let enemy_spawn_shield = enemy.is_some_and(|e| {
            self.tick < e.respawned_at_tick + self.config.claim_fairness.enemy_respawn_eat_grace_ticks
        });
        let shielded = player_boost_invincible || player_respawn_shield || in_safe_zone;
        let (shield_reason, shield_until_tick) = if player_boost_invincible {
            ("boost_invincible", player_invincibility_end_tick)
        } else if player_respawn_shield {
            ("spawn_protection", player_spawn_protected_until_tick)
        } else if in_safe_zone {
            ("safe_zone", None) // positional, not tick-bounded
        } else {
            ("none", None)
        };
        let shield_remaining_ticks = shield_until_tick.map(|t| t.saturating_sub(self.tick));

        // The v4 visible legs: a kill needs BOTH reconstructed AND projected within the confirm
        // radius — either missing/over means the gate would block.
        let visible_legs_block =
            reconstructed.is_none_or(|r| r > confirm_radius) || projected.is_some_and(|p| p > confirm_radius);
        // Fine-grained reason (kept on every event for continuity / drill-down).
        let why = if shielded {
            "shielded"
        } else if !enemy_present_same_gen {
            "enemy_respawned_or_missing"
        } else if !enemy_can_eat {
            "enemy_cannot_eat"
        } else if server_dist_at_known_tick.is_none_or(|d| d > kill_radius) {
            "no_server_contact"
        } else if contact_ticks < CONTACT_TICKS_REQUIRED {
            "below_contact_ticks"
        } else if reconstructed.is_none_or(|r| r > confirm_radius) {
            "reconstructed_visible_gap"
        } else if projected.is_some_and(|p| p > confirm_radius) {
            "projected_visible_gap"
        } else {
            "would_kill"
        };

        // Final diagnosis → the event NAME (the split). `visual_overlap_without_death` is RESERVED
        // for the genuinely-bad residue: server had a lethal, long-enough, visibly-confirmed contact
        // at the seen tick yet no death — everything else gets a self-explaining name.
        let event = Self::classify_overlap_diagnosis(
            shielded,
            enemy_present_same_gen && enemy_can_eat,
            server_dist_at_known_tick.is_some_and(|d| d <= kill_radius),
            contact_ticks >= CONTACT_TICKS_REQUIRED,
            visible_legs_block,
        );

        self.telemetry_event(
            "info",
            event,
            json!({
                "player_id": player_id,
                "enemy_id": enemy_id,
                "enemy_generation": enemy_generation,
                "client_known_server_tick": known_server_tick,
                "server_tick": self.tick,
                "server_dist_at_known_tick": server_dist_at_known_tick,
                "server_dist_now": server_dist_now,
                "contact_ticks": contact_ticks,
                "reconstructed_enemy_visible_dist": reconstructed,
                "victim_projected_visible_dist": projected,
                "victim_projected_lead_ticks": lead,
                "victim_velocity_x": player_vel_x,
                "victim_velocity_y": player_vel_y,
                "player_invincible": player_invincible,
                "enemy_alive": enemy_present_same_gen,
                "enemy_can_eat": enemy_can_eat,
                "server_kill_radius": kill_radius,
                "visible_confirm_radius": confirm_radius,
                "decision_why_no_kill": why,
                // Shield breakdown (lead manifesto 2026-06-06).
                "shielded_entity": if shielded { "player" } else { "none" },
                "shield_reason": shield_reason,
                "shield_until_tick": shield_until_tick,
                "shield_remaining_ticks": shield_remaining_ticks,
                "player_respawn_shield": player_respawn_shield,
                "enemy_spawn_shield": enemy_spawn_shield,
                "player_boost_invincible": player_boost_invincible,
            }),
        );
    }

    /// Classify a client-reported survived overlap into one final-diagnosis event name
    /// (OBSERVE-ONLY). Order mirrors the death gate: shield → valid lethal target → server
    /// contact at the seen tick → contact-run length → the v4 visible legs. The generic
    /// `visual_overlap_without_death` is RESERVED for the genuinely-bad residue (the server had a
    /// lethal, long-enough, visibly-confirmed contact and STILL didn't kill).
    fn classify_overlap_diagnosis(
        shielded: bool,
        valid_lethal_target: bool,
        server_contact_at_seen: bool,
        contact_long_enough: bool,
        visible_legs_block: bool,
    ) -> &'static str {
        if shielded {
            "visual_overlap_shielded"
        } else if !valid_lethal_target || !server_contact_at_seen {
            "visual_overlap_no_server_contact"
        } else if !contact_long_enough {
            "visual_overlap_contact_too_short"
        } else if visible_legs_block {
            "visual_overlap_projected_blocked"
        } else {
            "visual_overlap_without_death"
        }
    }

    pub fn update(&mut self) -> Option<Vec<PlayerReward>> {
        // Route queued inputs into per-player buffers via the command mailbox. They are
        // applied below at their target_tick (not on receive).
        while let Ok(input) = self.input_rx.try_recv() {
            self.apply_command(RoomCommand::PlayerInput(input));
        }
        // OBSERVE-ONLY: correlate any survived-overlap probes against this room's state/history.
        while let Ok(probe) = self.probe_rx.try_recv() {
            self.apply_command(RoomCommand::VisualOverlapProbe(probe));
        }

        self.tick += 1;
        // Pure simulation derives its own dt from tick_rate. AI also uses
        // ticks for its decision schedule but still needs dt for sub-cell
        // movement; pass it explicitly there.
        let dt = 1.0_f32 / self.config.room.tick_rate as f32;

        if self.tick.is_multiple_of(60) {
            tracing::debug!(
                "⏱️ [ROOM {}] Tick {}/{}, {} players",
                self.id,
                self.tick,
                self.game_end_tick,
                self.players.len()
            );
            // PER-WINDOW metrics (reset each log) so the line shows the RATE this
            // second, not a match-long running total. future>0 after a new round =
            // a client still in the old tick-domain; accepted_late>0 = inputs
            // landing after their target_tick (likely small client-side corrections).
            if self.input_future_rejected > 0
                || self.input_late_rejected > 0
                || self.input_duplicate > 0
                || self.input_late_accepted > 0
                || self.input_coalesced_count > 0
            {
                tracing::warn!(
                    "⚠️ [ROOM {}] input rejects/late window: future={} rejected_late={} accepted_late={} accepted_late_max_age={} duplicate={} full={} tick={}",
                    self.id,
                    self.input_future_rejected,
                    self.input_late_rejected,
                    self.input_late_accepted,
                    self.input_late_accepted_max_age,
                    self.input_duplicate,
                    self.input_coalesced_count,
                    self.tick
                );
            }
            self.input_future_rejected = 0;
            self.input_late_rejected = 0;
            self.input_late_accepted = 0;
            self.input_late_accepted_max_age = 0;
            self.input_duplicate = 0;
            self.input_coalesced_count = 0;
        }

        if !self.is_active {
            return None;
        }

        if self.tick >= self.game_end_tick {
            return Some(self.end_game());
        }

        let current_tick = self.tick;

        // Filler lifecycle FIRST (due exits/spawns + throttled rebalance), so a leaver is
        // gone before this tick's movement/collision and its PlayerLeft precedes the
        // snapshot that no longer carries it (reliable-before-snapshot invariant).
        self.process_filler_lifecycle(current_tick);

        // Apply inputs whose target_tick has arrived (<= current_tick), in
        // (target_tick, seq) order, with seq replay protection. Applying at
        // target_tick — not at receive time — keeps client prediction and
        // server authority turning on the same tick (no "snake").
        let verbose = self.telemetry_verbose();
        // Verbose firehose: (player, input_seq, target_tick) of each input applied
        // this tick. Collected here (borrow of input_slots/players is live) and
        // emitted after the loop. Only allocated when verbose.
        let mut applied_log: Vec<(String, u32, u64)> = Vec::new();
        // First accepted input of an admission (always-on, once per admission): the lead's
        // start-jerk forensics anchor — was early movement input-driven, and how long after
        // the join/resume the first input actually reached the sim.
        let mut first_input_log: Vec<(String, u32, u64, u64, bool)> = Vec::new();
        for (pid, slot) in self.input_slots.iter_mut() {
            let due = slot.take_due(current_tick);
            if let Some(player) = self.players.get_mut(pid) {
                for input in due {
                    if input.seq > player.last_processed_input_seq {
                        player.set_direction(input.direction);
                        player.last_processed_input_seq = input.seq;
                        if !player.first_input_logged {
                            player.first_input_logged = true;
                            first_input_log.push((
                                pid.clone(),
                                input.seq,
                                input.target_tick,
                                current_tick.saturating_sub(player.admitted_at_tick),
                                player.admitted_via_resume,
                            ));
                        }
                        if verbose {
                            applied_log.push((pid.clone(), input.seq, input.target_tick));
                        }
                    }
                }
            }
        }
        for (pid, seq, target_tick, age_since_admit, via_resume) in first_input_log {
            self.telemetry_event(
                "info",
                "server_first_input_after_admission",
                json!({
                    "player_id": pid,
                    "input_seq": seq,
                    "target_tick": target_tick,
                    "applied_tick": current_tick,
                    "age_since_admit_ticks": age_since_admit,
                    "admitted_via_resume": via_resume,
                }),
            );
        }
        for (pid, seq, target_tick) in applied_log {
            self.telemetry_event(
                "info",
                "input_applied",
                json!({
                    "player_id": pid,
                    "input_seq": seq,
                    "target_tick": target_tick,
                    "applied_tick": current_tick,
                    "applied_late_ticks": current_tick.saturating_sub(target_tick),
                }),
            );
        }

        // Filler brains "press their keys" at the same stage human inputs just applied —
        // set_direction before movement, identical steering rules for both kinds.
        self.drive_filler_bots(current_tick);

        // Update players. STABLE ORDER (determinism): this loop makes gameplay decisions — a
        // contested portal pickup consumes the portal AND advances RoomRng (get_random_spawn_position),
        // so whichever player iterates first changes the RNG sequence; and boost-expiry events are
        // allocated event_ids in this order. HashMap iteration order isn't stable, so sort by id —
        // the lowest id wins a contested portal and the event order is replay-stable. (review #1)
        let mut player_ids: Vec<String> = self.players.keys().cloned().collect();
        player_ids.sort();
        // Boost speed can expire inside player.update (update_timers). Capture the
        // change so we can emit a PlayerSpeedChanged event at THIS tick — the client
        // schedules the speed there instead of drifting until the next snapshot.
        let mut speed_changes: Vec<(String, f32, bool)> = Vec::new();
        for pid in &player_ids {
            // Step 1: simulate movement
            let old_speed = self.players.get(pid).map(|p| p.speed);
            if let Some(player) = self.players.get_mut(pid) {
                player.update(current_tick, dt, &self.map);
            }
            if let (Some(old), Some(p)) = (old_speed, self.players.get(pid)) {
                if (p.speed - old).abs() > 0.01 {
                    speed_changes.push((pid.clone(), p.speed, p.is_invincible));
                }
            }

            // Step 2: portal check requires &mut self.map + &mut self.rng,
            // so we re-borrow player position out and operate on map separately.
            let pos = self.players.get(pid).map(|p| p.position);
            if let Some(pos) = pos {
                if self.map.check_portal(&pos, self.config.portal.pickup_radius_px) {
                    let teleport_pos = self.map.get_random_spawn_position(&mut self.rng);
                    let from_pos = pos;
                    if let Some(player) = self.players.get_mut(pid) {
                        player.position = teleport_pos;
                    }
                    self.map.remove_portal();
                    let respawn_ticks =
                        (self.config.portal.respawn_interval_sec * self.tick_rate() as f32) as u64;
                    self.portal_respawn_tick = Some(current_tick + respawn_ticks);

                    // Stage 5.5: PortalTeleport is a hard event — client must
                    // drop its prediction/interpolation history for this
                    // entity so the visual doesn't lerp across the teleport.
                    let patch = self.state_patch_for(pid).expect("player exists post-teleport");
                    let event_id = self.alloc_event_id();
                    self.emit_event(
                        event_id,
                        current_tick,
                        DomainEvent::PortalTeleport {
                            player_id: pid.clone(),
                            from: from_pos,
                            to: teleport_pos,
                            state_patch: patch,
                        },
                    );
                }
            }
        }

        // Emit boost-expired speed changes detected above.
        for (pid, speed, inv) in speed_changes {
            let event_id = self.alloc_event_id();
            // Expiry runs in update_timers() BEFORE movement (player.update), so the
            // movement at this tick already used base speed → effective NOW. Use
            // current_tick (== self.tick here) so the two tick vars don't drift apart
            // under a future refactor.
            let effective_tick = current_tick;
            tracing::info!(
                "⚡ PlayerSpeedChanged room={} player={} event={} tick={} eff={} reason=boost_expired speed={:.1} inv={}",
                self.id, pid, event_id, self.tick, effective_tick, speed, inv
            );
            self.telemetry_event(
                "info",
                "player_speed_changed",
                json!({
                    "player_id": pid,
                    "event_id": event_id,
                    "effective_tick": effective_tick,
                    "speed": speed,
                    "is_invincible": inv,
                    "reason": "boost_expired",
                }),
            );
            self.emit_event(
                event_id,
                self.tick,
                DomainEvent::PlayerSpeedChanged {
                    effective_tick,
                    player_id: pid,
                    speed,
                    is_invincible: inv,
                    reason: "boost_expired".to_string(),
                },
            );
        }

        // Update enemies. Determinism: the AI picks the nearest eatable player, and a TIE (two
        // players equidistant) must resolve the same way every run — for replay/repro and to avoid
        // arbitrary unfairness. HashMap iteration order is not stable, so feed a by-id-sorted slice.
        let mut players_vec: Vec<Player> = self.players.values().cloned().collect();
        players_vec.sort_by(|a, b| a.id.cmp(&b.id));
        for enemy in &mut self.enemies {
            enemy.update(current_tick, dt, &players_vec, &self.map, &mut self.rng);
        }

        // Record where everyone is THIS tick (after movement), then resolve any
        // player-initiated eats from claims against that history. PvP and
        // player-eats-enemy are NO LONGER decided by the server's current-tick
        // collision — only by EatClaim. Enemy-eats-player stays server-side.
        self.record_contact_history();
        // Unified chronological resolver (v6 review #3): eat + enemy-death claims applied in one
        // timeline order (oldest effective render tick first), then the server-side fallback (now
        // observe-only at v6, with the sustained-no-claim anti-cheat kill).
        self.process_claims();
        // Filler internal eats AFTER human claims: a human claim that just killed/ate this
        // tick respawns its victim with protection, which the filler gates then observe.
        self.process_filler_internal_eats();
        self.check_enemy_collisions();
        self.check_item_collection();

        // Spawn points (tick-based)
        if current_tick >= self.next_point_spawn_tick {
            self.spawn_point();
            let interval = (self.config.spawn.point_interval_sec * self.tick_rate() as f32) as u64;
            self.next_point_spawn_tick = current_tick + interval;
        }

        // Spawn boosters (tick-based)
        if current_tick >= self.next_booster_spawn_tick && self.boosters.is_empty() {
            self.spawn_booster();
            let interval = (self.config.spawn.booster_interval_sec * self.tick_rate() as f32) as u64;
            self.next_booster_spawn_tick = current_tick + interval;
        }

        // Respawn portal (tick-based)
        if let Some(respawn_at) = self.portal_respawn_tick {
            if current_tick >= respawn_at {
                self.map.spawn_portal(&mut self.rng);
                self.portal_respawn_tick = None;
            }
        }

        // Send game state on send_interval_ticks cadence
        if self.tick.is_multiple_of(self.config.room.send_interval_ticks) {
            self.broadcast_game_state();
        }

        // Population heartbeat (lead review: beta dashboards must answer "are the bots
        // alive / leaking / crowding?" without a replay). 10 s cadence, occupied rooms only.
        if self.tick.is_multiple_of(600) && !self.players.is_empty() {
            let mut goals: BTreeMap<&'static str, usize> = BTreeMap::new();
            for ctrl in self.filler_controllers.values() {
                *goals.entry(ctrl.goal_kind()).or_insert(0) += 1;
            }
            self.telemetry_event(
                "info",
                "room_population",
                json!({
                    "human_count": self.human_count(),
                    "filler_count": self.filler_count(),
                    "reserved_slots": self.reserved_slots,
                    "scheduled_filler_exits": self.scheduled_filler_exits.len(),
                    "pending_filler_spawns": self.pending_filler_spawns.len(),
                    "total_players": self.players.len(),
                    "filler_goals": goals,
                }),
            );
        }

        // Reliable-overflow bookkeeping. The lanes were ALREADY closed at the moment of overflow
        // (PlayerOutbox::note_reliable_overflow drops the outbound immediately, so no snapshot
        // reflecting the dropped hard event could be queued after it — the structural guard). This
        // loop is just the forced-resync log + telemetry + counter; `outbox.remove` here is a no-op
        // for an already-removed channel. (review — reliable-overflow race closed at the source.)
        for pid in self.outbox.take_disconnects() {
            self.forced_resync_count = self.forced_resync_count.saturating_add(1);
            tracing::error!(
                "🔌 reliable overflow for player={} room={} tick={} forced_resync_total={} — dropping outbound to force resync",
                pid, self.id, self.tick, self.forced_resync_count
            );
            self.telemetry_event(
                "warn",
                "reliable_overflow_forced_resync",
                // `forced_resync_total` is this room's running count (review #10) — a per-room gauge
                // alongside the per-event record, so a chronically-behind room stands out at a glance.
                json!({ "player_id": pid, "forced_resync_total": self.forced_resync_count }),
            );
            self.outbox.remove(&pid);
        }

        None
    }

    /// DISABLED: server-current-tick PvP collision is no longer a gameplay path —
    /// player-vs-player eating goes through EatClaim (attacker-view validation).
    /// Kept (unused) only so old tests can reference the legacy behaviour.
    #[allow(dead_code)]
    fn check_player_collisions_server_fallback_disabled(&mut self) {
        // Stable order (determinism) — legacy/disabled path, sorted for consistency with the live loops.
        let mut player_ids: Vec<String> = self.players.keys().cloned().collect();
        player_ids.sort();
        let collision_distance = self.config.collision.collision_distance_px;

        for i in 0..player_ids.len() {
            for j in (i + 1)..player_ids.len() {
                let id1 = &player_ids[i];
                let id2 = &player_ids[j];

                let (p1, p2) = {
                    let p1 = self.players.get(id1).unwrap().clone();
                    let p2 = self.players.get(id2).unwrap().clone();
                    (p1, p2)
                };

                if Self::is_collision(&p1.position, &p2.position, collision_distance) {
                    if self.map.is_in_safe_zone(&p1.position) || self.map.is_in_safe_zone(&p2.position) {
                        continue;
                    }

                    // A spawn-protected victim can't be eaten (offense still allowed,
                    // mirroring the enemy-collision shield above). The resume shield is
                    // SYMMETRIC by contrast (lead 2026-06-11): a just-resumed player can
                    // neither be eaten NOR eat — otherwise the grace window is a free
                    // offense buff.
                    let p1_shield = p1.resume_shield_active(self.tick, RESUME_SHIELD_TICKS);
                    let p2_shield = p2.resume_shield_active(self.tick, RESUME_SHIELD_TICKS);
                    if p1.can_eat(&p2) && !p2.is_spawn_protected(self.tick) && !p1_shield && !p2_shield {
                        self.handle_player_eaten(id1, id2);
                    } else if p2.can_eat(&p1) && !p1.is_spawn_protected(self.tick) && !p1_shield && !p2_shield
                    {
                        self.handle_player_eaten(id2, id1);
                    }
                }
            }
        }
    }

    fn check_enemy_collisions(&mut self) {
        // Stable order so emitted death/respawn events get replay-stable event_ids (each player's
        // death is independent, so order only affects event ordering — but that must be deterministic). (review #1)
        let mut player_ids: Vec<String> = self.players.keys().cloned().collect();
        player_ids.sort();
        let collision_distance = self.config.collision.collision_distance_px;
        // Victim-favored SERVER kill radius. EnemyContactPolicy owns it + the server-timeline
        // rule; EnemyDeathCandidatePolicy (Pass B) wraps it with the visible-confirm gate.
        let enemy_eats_player_distance = EnemyContactPolicy::kill_radius(collision_distance);
        // Config-driven death-fairness model (versioned, folded into config_hash).
        let df = &self.config.death_fairness;
        let interp_delay = df.enemy_interp_delay_ticks;
        let policy_version = df.policy_version;
        // Some(radius) = visible gate ENABLED; None = observe-only.
        let visible_confirm_radius =
            if df.visible_gate_enabled { Some(df.visible_confirm_radius_px()) } else { None };

        // player_id -> the death payload. Only enemy-eats-player stays server-side;
        // player-eats-enemy is an EatClaim now (process_claims, run before this).
        let mut players_to_respawn: BTreeMap<String, EnemyDeath> = BTreeMap::new();

        // Pass A: classify each player's threat. Shielded is EXPLICIT (a distinct outcome,
        // not "happened to find no enemy"), so Pass B can reset the contact run on shield.
        enum Threat {
            Shielded,
            NoEnemy,
            Enemy { idx: usize, id: String, dist: f32, pos: Position },
        }
        let mut threats: Vec<(String, Threat)> = Vec::new();
        for player_id in &player_ids {
            let player = self.players.get(player_id).unwrap();
            // Shielded = safe zone OR spawn-protected OR invincible (boost). An invincible
            // player can't be eaten; treating it as a shield (explicitly) means a deferred
            // candidate is RESET the moment boost starts, so an old contact can't fire on
            // boost expiry. (lead review — explicit, not implicit.)
            let shielded = self.map.is_in_safe_zone(&player.position)
                || player.is_spawn_protected(self.tick)
                || player.is_invincible
                // Resume admission grace (lead 2026-06-11 P0 #1): the player just re-entered a
                // live world — no deaths (and the run resets, like every shield) until it ends.
                || player.resume_shield_active(self.tick, RESUME_SHIELD_TICKS);
            if shielded {
                threats.push((player_id.clone(), Threat::Shielded));
                continue;
            }
            let mut best: Option<(usize, String, f32, Position)> = None;
            for (enemy_idx, enemy) in self.enemies.iter().enumerate() {
                if !enemy.can_eat(player) {
                    continue;
                }
                let dist = Self::distance(&player.position, &enemy.position);
                if dist >= enemy_eats_player_distance {
                    continue;
                }
                match &best {
                    Some((_, _, bd, _)) if *bd <= dist => {}
                    _ => best = Some((enemy_idx, enemy.id.clone(), dist, enemy.position)),
                }
            }
            threats.push((
                player_id.clone(),
                match best {
                    Some((idx, id, dist, pos)) => Threat::Enemy { idx, id, dist, pos },
                    None => Threat::NoEnemy,
                },
            ));
        }

        // Pass B: a kill needs the SAME enemy life in range for CONTACT_TICKS_REQUIRED ticks
        // on the SERVER timeline AND visible confirmation. Server contact is necessary, not
        // sufficient. Shield/contact-broken explicitly RESET the run (suppress).
        for (player_id, threat) in threats {
            let (enemy_idx, enemy_id, dist, killer_pos) = match threat {
                Threat::Shielded => {
                    self.suppress_death_candidate(&player_id, DeathDecisionCode::SuppressShielded);
                    continue;
                }
                Threat::NoEnemy => {
                    self.suppress_death_candidate(&player_id, DeathDecisionCode::SuppressContactBroken);
                    continue;
                }
                Threat::Enemy { idx, id, dist, pos } => (idx, id, dist, pos),
            };

            // Key the contact run on the live (id, generation) so a respawn mid-candidate resets it.
            let enemy_generation = self.enemies[enemy_idx].generation;
            let enemy_respawned_at = self.enemies[enemy_idx].respawned_at_tick;
            let (ticks, victim_pos, player_speed) = {
                let p = self.players.get_mut(&player_id).unwrap();
                (p.register_death_contact(&enemy_id, enemy_generation, dist), p.position, p.speed)
            };

            // Reconstruct the killer on the victim's PRESENTATION timeline (the enemy rendered
            // `interp_delay` ticks back) vs the victim's current predicted position. GENERATION-
            // AWARE: `ContactHistory::sample_enemy` is told the killer's life and rejects a frame from a
            // DIFFERENT enemy life BEFORE interpolating (so a gen-1 death can't be "confirmed" by
            // a blended gen-0 position); the explicit `visible_tick < respawned_at` guard short-
            // circuits the pre-life window. `None` (no/old/wrong-life history) → the armed gate
            // HOLDS (does not fall back to a server-current kill).
            let visible_tick = self.tick.saturating_sub(interp_delay);
            // The killer reconstructed on the victim's PRESENTATION timeline (interp_delay back,
            // SAME enemy life). Kept as a position so the projected-victim leg below and the
            // telemetry can both use it.
            let enemy_visible_pos = if visible_tick < enemy_respawned_at {
                None
            } else {
                self.contact_history
                    .sample_enemy(&enemy_id, Some(enemy_generation), visible_tick as f64)
                    .map(|h| h.position)
            };
            let reconstructed_enemy_visible_dist =
                enemy_visible_pos.map(|ep| Self::distance(&victim_pos, &ep));

            // OBSERVE-ONLY (lead manifesto 2026-06-06): the visible gate measures the enemy
            // reconstructed BACK vs the victim at server-now, but the client also renders the
            // LOCAL player predicted FORWARD by its lead — so a death can be server-close yet the
            // player already drew themselves past the enemy (the confirmed ghost case). Reconstruct
            // that second leg and log it; the gate does NOT yet use it. Lead is the per-player
            // ESTIMATE (logged, not hardcoded) — a kill condition on victim_projected_visible_dist
            // is deferred until these logs calibrate the lead.
            let (victim_vel_x, victim_vel_y, victim_lead_ticks) = {
                let p = self.players.get(&player_id).unwrap();
                let (vx, vy) = p.velocity();
                (vx, vy, p.estimated_client_lead_ticks())
            };
            let tick_rate = self.config.room.tick_rate as f32;
            let victim_projected_pos = victim_lead_ticks.map(|lead| {
                let dt = lead as f32 / tick_rate;
                Position { x: victim_pos.x + victim_vel_x * dt, y: victim_pos.y + victim_vel_y * dt }
            });
            let victim_projected_visible_dist = match (victim_projected_pos, enemy_visible_pos) {
                (Some(pp), Some(ep)) => Some(Self::distance(&pp, &ep)),
                _ => None,
            };

            let decision = EnemyDeathCandidatePolicy::evaluate(&EnemyDeathContext {
                server_tick: ServerTick::new(self.tick),
                reconstructed_enemy_tick: ServerTick::new(visible_tick),
                server_dist: dist,
                reconstructed_enemy_visible_dist,
                interp_delay_ticks: interp_delay,
                kill_radius: enemy_eats_player_distance,
                contact_ticks_server: ticks,
                contact_ticks_required: CONTACT_TICKS_REQUIRED,
                visible_confirm_radius_px: visible_confirm_radius,
                victim_projected_visible_dist,
                victim_projected_lead_ticks: victim_lead_ticks,
            });

            match decision {
                DeathDecision::Kill { .. } => {
                    // v6: death is CLAIM-BASED. The server no longer KILLS here — a v6 client sends
                    // an EnemyDeathClaim for what it actually saw. This server-current path would
                    // reintroduce the guessed (ghost-prone) death, so at v6 it only RECORDS that it
                    // would have killed (so a missing/lost client claim is visible) and never emits
                    // a death. (config_hash + protocol_version gate old clients out, so every live
                    // client is claim-capable.) (lead review v6)
                    if policy_version >= 6 {
                        // Candidate telemetry CADENCE (lead P2 #7): the first lethal tick, then
                        // every Nth — one warn per tick for a multi-second contact was noise.
                        if ticks == CONTACT_TICKS_REQUIRED || ticks % FALLBACK_CANDIDATE_LOG_EVERY_TICKS == 0
                        {
                            self.telemetry_event(
                                "warn",
                                "enemy_death_server_fallback_candidate",
                                json!({
                                    "player_id": player_id,
                                    "enemy_id": enemy_id,
                                    "enemy_generation": enemy_generation,
                                    "server_tick": self.tick,
                                    "server_dist": dist,
                                    "reconstructed_enemy_visible_dist": reconstructed_enemy_visible_dist,
                                    "victim_projected_visible_dist": victim_projected_visible_dist,
                                    "victim_projected_lead_ticks": victim_lead_ticks,
                                    "contact_ticks": ticks,
                                    "policy_version": policy_version,
                                }),
                            );
                        }
                        // ANTI-CHEAT SAFETY NET (v6 review #1): normally a v6 client claims its own
                        // death within a few ticks of seeing the overlap, and the accepted claim
                        // respawns the victim (resetting this contact run). If a LETHAL server
                        // contact persists SERVER_FALLBACK_KILL_TICKS with NO claim arriving, the
                        // client is withholding / broken / cheating — so the server kills anyway
                        // (a player must not be immortal vs bots by not sending claims). Below the
                        // threshold we keep waiting for the claim (observe-only).
                        if ticks < SERVER_FALLBACK_KILL_TICKS {
                            continue;
                        }
                        // A PLAUSIBLE death claim is in flight (waiting for its render tick) — give it
                        // a BOUNDED grace to resolve instead of pre-empting it with a duplicate server
                        // kill. The cap is `death_claim_fallback_hold_cap_ticks` (≥ the scheduling
                        // window + 1) so a claim accepted into pending is never killed before it can
                        // become READY; past it the fallback fires regardless, so a never-resolving /
                        // refresh-spammed claim can't grant immortality. The contact-tick counter only
                        // advances while the enemy stays lethally close, so the player can't reset the
                        // clock without actually escaping the overlap. (round-3 #2 / follow-up)
                        if ticks < self.death_claim_fallback_hold_cap_ticks()
                            && self.has_plausible_pending_enemy_death_claim(
                                &player_id,
                                &enemy_id,
                                enemy_generation,
                            )
                        {
                            continue;
                        }
                        // STRICT visible bar for a kill with NO claim (lead 2026-06-11 P0 #2):
                        // the net itself produced a ghost (run 07-12-37 — server_dist 3.3,
                        // reconstructed 13.7, projected 17.7, while the CLIENT drew the killer
                        // at 23.6px: no on-screen overlap, hence honestly no claim). Require
                        // BOTH visible legs within kill_radius + grace, and NEVER kill when the
                        // projection is unavailable (no lead sample ⇒ no argument the victim
                        // could have seen it). Anything else stays deferred — the contact run
                        // keeps counting, so the moment both legs close in, the net fires.
                        let strict_radius = enemy_eats_player_distance + FALLBACK_STRICT_VISIBLE_GRACE_PX;
                        let strict_visible = matches!(reconstructed_enemy_visible_dist, Some(d) if d <= strict_radius)
                            && matches!(victim_projected_visible_dist, Some(d) if d <= strict_radius);
                        if !strict_visible {
                            if ticks % FALLBACK_CANDIDATE_LOG_EVERY_TICKS == 0 {
                                self.telemetry_event(
                                    "warn",
                                    "enemy_death_fallback_deferred_strict",
                                    json!({
                                        "player_id": player_id,
                                        "enemy_id": enemy_id,
                                        "enemy_generation": enemy_generation,
                                        "server_tick": self.tick,
                                        "server_dist": dist,
                                        "contact_ticks": ticks,
                                        "strict_visible_radius": strict_radius,
                                        "reconstructed_enemy_visible_dist": reconstructed_enemy_visible_dist,
                                        "victim_projected_visible_dist": victim_projected_visible_dist,
                                        "victim_projected_lead_ticks": victim_lead_ticks,
                                    }),
                                );
                            }
                            continue;
                        }
                        self.telemetry_event(
                            "warn",
                            "enemy_death_claim_missing_sustained_kill",
                            json!({
                                "player_id": player_id,
                                "enemy_id": enemy_id,
                                "enemy_generation": enemy_generation,
                                "server_tick": self.tick,
                                "server_dist": dist,
                                "contact_ticks": ticks,
                                "threshold_ticks": SERVER_FALLBACK_KILL_TICKS,
                                "strict_visible_radius": strict_radius,
                                "reconstructed_enemy_visible_dist": reconstructed_enemy_visible_dist,
                                "victim_projected_visible_dist": victim_projected_visible_dist,
                            }),
                        );
                        // fall through to the server-side kill below.
                    }
                    // policy v4 fallback audit: an armed-gate kill landed via the reconstructed-only
                    // path because there was no client lead sample to project the victim forward
                    // (so the second leg couldn't be checked). Surface it rather than silently
                    // regressing to the pre-v4 ghost-prone rule for this death.
                    if visible_confirm_radius.is_some() && victim_projected_visible_dist.is_none() {
                        self.telemetry_event(
                            "info",
                            "projected_visible_missing_on_death_candidate",
                            json!({
                                "player_id": player_id,
                                "enemy_id": enemy_id,
                                "enemy_generation": enemy_generation,
                                "server_tick": self.tick,
                                "reconstructed_enemy_visible_dist": reconstructed_enemy_visible_dist,
                                "server_dist": dist,
                                "victim_velocity_x": victim_vel_x,
                                "victim_velocity_y": victim_vel_y,
                                "interp_delay_ticks": interp_delay,
                            }),
                        );
                    }
                    // Gate armed ⇒ the visible timeline CONFIRMED the kill; observe-only (gate
                    // disabled) ⇒ killed on server-truth alone. Distinct codes so a log/replay
                    // reader never reads an observe-only death as visibly-confirmed. The radius
                    // rides as an Option (no `Infinity` sentinel on the wire/log path).
                    let code = if policy_version >= 6 {
                        // At v6 this line is reachable ONLY via the sustained-missing-claim anti-cheat
                        // fallback above (the normal v6 path `continue`s and waits for the claim). Tag
                        // it distinctly so logs/replays never read it as an honest claim-confirmed kill
                        // or a legacy pre-v6 visible kill. (round-3 #3)
                        DeathDecisionCode::KillNowClaimMissingSustained
                    } else if visible_confirm_radius.is_some() {
                        DeathDecisionCode::KillNowVisibleConfirmed
                    } else {
                        DeathDecisionCode::KillNowServerOnlyObserveOnly
                    };
                    tracing::info!(
                        "💀 EnemyAtePlayer room={} player={} enemy={} gen={} tick={} server_dist={:.1} visible_dist={:?} confirm_radius={:?} visible_tick={} interp_delay={} contact_ticks={} decision={} player_speed={:.1}",
                        self.id, player_id, enemy_id, enemy_generation, self.tick, dist,
                        reconstructed_enemy_visible_dist, visible_confirm_radius, visible_tick,
                        interp_delay, ticks, code.as_str(), player_speed
                    );
                    self.telemetry_event(
                        "info",
                        "enemy_ate_player",
                        json!({
                            "player_id": player_id,
                            "enemy_id": enemy_id,
                            "enemy_generation": enemy_generation,
                            "decision": code.as_str(),
                            "policy_version": policy_version,
                            "server_dist": dist,
                            "reconstructed_enemy_visible_dist": reconstructed_enemy_visible_dist,
                            "reconstructed_enemy_tick": visible_tick,
                            "interp_delay_ticks": interp_delay,
                            "contact_ticks": ticks,
                            "contact_ticks_required": CONTACT_TICKS_REQUIRED,
                            "server_kill_radius": enemy_eats_player_distance,
                            "visible_gate_enabled": visible_confirm_radius.is_some(),
                            "visible_confirm_radius": visible_confirm_radius,
                            "player_speed": player_speed,
                            // The victim-forward-projection leg the v4 gate confirms (and v5 guards
                            // against over-trusting): victim predicted forward by the estimated
                            // client lead vs the reconstructed enemy. On a KILL both legs were within
                            // the confirm radius (and the v5 uncertainty guard didn't trip).
                            "victim_projected_visible_dist": victim_projected_visible_dist,
                            "victim_projected_lead_ticks": victim_lead_ticks,
                            "victim_velocity_x": victim_vel_x,
                            "victim_velocity_y": victim_vel_y,
                            "victim_projected_x": victim_projected_pos.map(|p| p.x),
                            "victim_projected_y": victim_projected_pos.map(|p| p.y),
                            "enemy_visible_x": enemy_visible_pos.map(|p| p.x),
                            "enemy_visible_y": enemy_visible_pos.map(|p| p.y),
                        }),
                    );
                    // Authoritative death fact: FULL timeline context in ONE reliable event,
                    // emitted BEFORE the victim's PlayerRespawned (separate facts).
                    let kill_event_id = self.alloc_event_id();
                    self.emit_event(
                        kill_event_id,
                        self.tick,
                        DomainEvent::PlayerKilledByEnemy {
                            victim_id: player_id.clone(),
                            killer_enemy_id: enemy_id.clone(),
                            killer_enemy_generation: enemy_generation,
                            killer_enemy_respawned_at_tick: enemy_respawned_at,
                            server_dist: dist,
                            reconstructed_enemy_tick: visible_tick,
                            reconstructed_enemy_visible_dist,
                            server_kill_radius: enemy_eats_player_distance,
                            visible_gate_enabled: visible_confirm_radius.is_some(),
                            visible_confirm_radius_px: visible_confirm_radius,
                            interp_delay_ticks: interp_delay,
                            contact_ticks: ticks,
                            decision: code,
                            policy_version,
                            killer_position: killer_pos,
                            victim_position: victim_pos,
                        },
                    );
                    players_to_respawn.insert(
                        player_id,
                        EnemyDeath {
                            killer_enemy_id: enemy_id,
                            server_dist: dist,
                            contact_ticks: ticks,
                            killer_position: killer_pos,
                            victim_position: victim_pos,
                        },
                    );
                }
                // Server confirmed but the PRESENTATION timeline does not (a visible gap, or
                // no reconstructable history) → HOLD. The run keeps building; it lands once the
                // visible distance confirms, or is suppressed when server contact breaks. NEVER
                // forced through by a timer.
                DeathDecision::Delay { reason } => {
                    let code = match reason {
                        DelayReason::VisibleGap => DeathDecisionCode::DelayVisibleGap,
                        DelayReason::NoVisibleHistory => DeathDecisionCode::DelayNoVisibleHistory,
                        DelayReason::PredictionUncertain => DeathDecisionCode::DelayPredictionUncertain,
                    };
                    // Throttle the per-tick deferral spam (a held parallel-chase can defer for 80+
                    // ticks): log on defer START (first tick a hold is possible) and every 10th
                    // tick while held. The terminal suppress/kill log once on their own paths.
                    if ticks == CONTACT_TICKS_REQUIRED || ticks.is_multiple_of(10) {
                        tracing::info!(
                            "⏸️ EnemyDeathDeferred room={} player={} enemy={} gen={} tick={} server_dist={:.1} visible_dist={:?} confirm_radius={:?} contact_ticks={} decision={}",
                            self.id, player_id, enemy_id, enemy_generation, self.tick, dist,
                            reconstructed_enemy_visible_dist, visible_confirm_radius, ticks, code.as_str()
                        );
                        self.telemetry_event(
                            "info",
                            "enemy_death_deferred",
                            json!({
                                "player_id": player_id,
                                "enemy_id": enemy_id,
                                "enemy_generation": enemy_generation,
                                "decision": code.as_str(),
                                "policy_version": policy_version,
                                "server_dist": dist,
                                "reconstructed_enemy_visible_dist": reconstructed_enemy_visible_dist,
                                "reconstructed_enemy_tick": visible_tick,
                                "visible_confirm_radius": visible_confirm_radius,
                                "interp_delay_ticks": interp_delay,
                                "contact_ticks": ticks,
                                // The victim-forward-projection leg (now an ARMED gate input): on a
                                // HELD death this is often what explains the hold — server-close but
                                // the victim drew themselves past the reconstructed enemy, or (v5)
                                // the projection leaned too hard at a turn-sensitive lead.
                                "victim_projected_visible_dist": victim_projected_visible_dist,
                                "victim_projected_lead_ticks": victim_lead_ticks,
                                "victim_velocity_x": victim_vel_x,
                                "victim_velocity_y": victim_vel_y,
                                "victim_projected_x": victim_projected_pos.map(|p| p.x),
                                "victim_projected_y": victim_projected_pos.map(|p| p.y),
                                "enemy_visible_x": enemy_visible_pos.map(|p| p.x),
                                "enemy_visible_y": enemy_visible_pos.map(|p| p.y),
                            }),
                        );
                    }
                }
                // In range but the server-timeline run is still building.
                DeathDecision::Track | DeathDecision::NoServerContact => {
                    tracing::debug!(
                        "⏳ EnemyContactBuilding room={} player={} enemy={} tick={} server_dist={:.1} visible_dist={:?} contact_ticks={}/{}",
                        self.id, player_id, enemy_idx, self.tick, dist, reconstructed_enemy_visible_dist,
                        ticks, CONTACT_TICKS_REQUIRED
                    );
                }
            }
        }

        for (player_id, death) in players_to_respawn {
            self.respawn_player(&player_id, Some(DeathCause::Enemy(death)));
        }
    }

    /// Explicitly drop a player's enemy-contact run (shield engaged, or the enemy left). Logs
    /// the suppression code ONLY when a candidate was actually building, so it's the
    /// building→suppressed transition, not idle spam. Keeps the shield/break reset in the Room
    /// (the death policy has no `shielded` field — it can't go stale there). (lead review #6)
    fn suppress_death_candidate(&mut self, player_id: &str, code: DeathDecisionCode) {
        // Snapshot the contact run's context BEFORE clearing it — a suppression is only
        // investigable if it names WHICH enemy life was being tracked, how long, and how close
        // it got. Reading post-clear would lose all of it. (lead review)
        let ctx = self.players.get(player_id).map(|p| {
            (
                p.death_contact_enemy.clone().unwrap_or_default(),
                p.death_contact_generation,
                p.death_contact_ticks,
                p.death_contact_last_dist,
            )
        });
        if let Some(p) = self.players.get_mut(player_id) {
            p.clear_death_contact();
        }
        // Log only the building→suppressed transition (a run was actually accumulating), not
        // idle ticks — so suppression telemetry stays signal, not spam.
        if let Some((enemy_id, enemy_generation, contact_ticks, last_server_dist)) = ctx {
            if contact_ticks > 0 {
                tracing::debug!(
                    "🚫 EnemyContactSuppressed room={} player={} enemy={} gen={} tick={} contact_ticks={} last_server_dist={:.1} decision={}",
                    self.id, player_id, enemy_id, enemy_generation, self.tick,
                    contact_ticks, last_server_dist, code.as_str()
                );
                self.telemetry_event(
                    "info",
                    "enemy_death_suppressed",
                    json!({
                        "player_id": player_id,
                        "enemy_id": enemy_id,
                        "enemy_generation": enemy_generation,
                        "contact_ticks": contact_ticks,
                        "server_tick": self.tick,
                        "last_server_dist": last_server_dist,
                        "decision": code.as_str(),
                    }),
                );
            }
        }
    }

    /// Respawn an enemy a player ate (via EatClaim): pick a position clear of every
    /// player (so it doesn't land back in the eater's mouth — the double-eat fix)
    /// and broadcast the hard EnemyRespawned event.
    fn respawn_enemy_eaten_by(&mut self, enemy_idx: usize, eaten_by: &str) {
        let mut new_pos = self.map.get_random_spawn_position(&mut self.rng);
        for _ in 0..ENEMY_RESPAWN_SPAWN_TRIES {
            let clear = self
                .players
                .values()
                .all(|p| Self::distance(&p.position, &new_pos) >= ENEMY_RESPAWN_CLEARANCE_PX);
            if clear {
                break;
            }
            new_pos = self.map.get_random_spawn_position(&mut self.rng);
        }
        let dir = self.choose_enemy_spawn_direction(&new_pos);
        self.enemies[enemy_idx].respawn(new_pos, dir, self.tick);

        let event_id = self.alloc_event_id();
        let (enemy_id, e_dir, e_speed, e_score, e_generation) = {
            let e = &self.enemies[enemy_idx];
            // generation was just bumped by respawn() above → this is the NEW life's id.
            (e.id.clone(), e.direction, e.speed, e.score, e.generation)
        };
        tracing::info!(
            "🤖 EnemyRespawned room={} enemy={} gen={} event={} tick={} pos=({:.1},{:.1}) reason=player_ate_enemy by={}",
            self.id, enemy_id, e_generation, event_id, self.tick, new_pos.x, new_pos.y, eaten_by
        );
        self.emit_event(
            event_id,
            self.tick,
            DomainEvent::EnemyRespawned {
                enemy_id,
                position: new_pos,
                direction: e_dir,
                speed: e_speed,
                score: e_score,
                reason: "player_ate_enemy".to_string(),
                caused_by_player_id: Some(eaten_by.to_string()),
                generation: e_generation,
            },
        );
    }

    // --- player-initiated eating (EatClaim) ---------------------------------

    /// Snapshot who-was-where THIS tick (after movement) into the contact-history
    /// ring, so a later EatClaim can be validated against the past the attacker saw.
    /// Record everyone's position this tick into the contact-history ring (delegated to
    /// [`ContactHistory`]), then prune the causal ledger of entries whose life has aged out of the
    /// ring — they can no longer bind any acceptable claim, so keeping them would only grow memory.
    fn record_contact_history(&mut self) {
        let tick = self.tick;
        self.contact_history.record(&self.players, &self.enemies, tick);
        let retain = self.claim_ledger_retain_ticks();
        self.claim_ledger.prune(tick as f64 - retain);
    }

    /// Drain the claim channel, hold claims whose attacker tick is still ahead of
    /// the sim, reject ones that arrived too late, and apply the rest in a
    /// deterministic order.
    /// Unified claim resolver (v6 review #3): drain BOTH the eat and the enemy-death claim
    /// channels, schedule each (a claim whose effective render tick the sim hasn't reached yet
    /// WAITS; absurd-future / too-old are rejected), then apply ALL ready claims in ONE
    /// chronological order — oldest effective render tick first. This is the correct conflict
    /// resolution: an eat and a death that reference the same enemy/tick resolve by what actually
    /// happened first on the timeline, instead of a fixed eat-vs-death precedence.
    fn process_claims(&mut self) {
        while let Ok(input) = self.claim_rx.try_recv() {
            self.apply_command(RoomCommand::EatClaim(input));
        }
        while let Ok(input) = self.enemy_death_claim_rx.try_recv() {
            self.apply_command(RoomCommand::EnemyDeathClaim(input));
        }
        let current_tick = self.tick;
        let max_process_delay = self.config.claim_fairness.max_process_delay_ticks;

        // A ready claim of either kind, tagged for the chronological merge + explicit tie-break.
        enum ReadyClaim {
            Eat(EatClaimInput),
            Death(EnemyDeathClaimInput),
        }
        // Sort key fields: effective render tick, then kind rank, then claim id, then player id.
        // kind_rank (Eat=0, Death=1) + player_id make a same-(tick, claim_id) order EXPLICIT instead
        // of drain-order (two players both number their first claim `1`). The causal ledger below
        // uses a STRICT `<` on tick, so two SAME-tick claims never cancel each other — this ordering
        // only fixes event/log emission order deterministically. (lead review round-3 #4)
        struct Ready {
            tick: f64,
            kind_rank: u8,
            claim_id: u32,
            player_id: String,
            claim: ReadyClaim,
        }
        let mut ready: Vec<Ready> = Vec::new();

        // Eat: schedule by attacker_render_tick.
        let mut keep_eat = Vec::new();
        for input in self.pending_eat_claims.drain(..).collect::<Vec<_>>() {
            let tick_f = input.claim.attacker_render_tick;
            // ceil, NOT floor — the sampler needs the upper frame recorded (see required_history_tick).
            let Some(required_tick) = required_history_tick(tick_f) else {
                self.reject_eat_claim(&input.player_id, input.claim.claim_id, "invalid_render_tick");
                continue;
            };
            if required_tick > current_tick + MAX_FUTURE_INPUT_TICKS {
                self.reject_eat_claim(&input.player_id, input.claim.claim_id, "claim_too_far_future");
            } else if required_tick > current_tick {
                keep_eat.push(input); // sim hasn't recorded ceil(render_tick) yet — HOLD, don't reject
            } else if current_tick as f64 - tick_f > max_process_delay as f64 {
                // Age from the ACTUAL render tick (not its ceil) so the process-delay window is exact.
                self.reject_eat_claim(&input.player_id, input.claim.claim_id, "too_old_after_attacker_tick");
            } else {
                ready.push(Ready {
                    tick: tick_f,
                    kind_rank: 0,
                    claim_id: input.claim.claim_id,
                    player_id: input.player_id.clone(),
                    claim: ReadyClaim::Eat(input),
                });
            }
        }
        self.pending_eat_claims = keep_eat;

        // Death: schedule by victim_render_tick (same rules).
        let mut keep_death = Vec::new();
        let death_max_future = self.death_claim_max_future_ticks();
        for input in self.pending_enemy_death_claims.drain(..).collect::<Vec<_>>() {
            let tick_f = input.claim.victim_render_tick;
            // ceil(victim_render_tick): the victim is the LATER of the two render ticks (shape requires
            // victim >= enemy), so once the victim's ceil frame exists the enemy's does too — gating on
            // it is sufficient for both legs of the reconstruction. (review — fractional claim gating)
            let Some(required_tick) = required_history_tick(tick_f) else {
                self.reject_enemy_death_claim(&input.player_id, input.claim.claim_id, "invalid_render_tick");
                continue;
            };
            // Tighter future cap than eat/inputs (round-3 #2): an honest death claim leads by at most
            // the client's prediction-lead cap, so a further-future one is only a stall attempt —
            // reject, never hold it.
            if required_tick > current_tick + death_max_future {
                self.reject_enemy_death_claim(&input.player_id, input.claim.claim_id, "claim_too_far_future");
            } else if required_tick > current_tick {
                keep_death.push(input); // sim hasn't recorded ceil(victim_render_tick) yet — HOLD
            } else if current_tick as f64 - tick_f > max_process_delay as f64 {
                // Age from the ACTUAL render tick (not its ceil) so the process-delay window is exact.
                self.reject_enemy_death_claim(
                    &input.player_id,
                    input.claim.claim_id,
                    "too_old_after_victim_tick",
                );
            } else {
                ready.push(Ready {
                    tick: tick_f,
                    kind_rank: 1,
                    claim_id: input.claim.claim_id,
                    player_id: input.player_id.clone(),
                    claim: ReadyClaim::Death(input),
                });
            }
        }
        self.pending_enemy_death_claims = keep_death;

        // Chronological: oldest effective render tick first; fully-explicit tie-break (#4).
        ready.sort_by(|a, b| {
            a.tick
                .partial_cmp(&b.tick)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.kind_rank.cmp(&b.kind_rank))
                .then(a.claim_id.cmp(&b.claim_id))
                .then_with(|| a.player_id.cmp(&b.player_id))
        });

        // Apply oldest-first, threading the PERSISTENT causal ledger so a same-entity conflict
        // resolved by an earlier (strictly-earlier-tick) claim invalidates the later one — even if
        // the two claims land in DIFFERENT batches (#1). Moved out of `self` for the loop (the
        // apply fns take `&mut self`), then put back; `prune` in record_contact_history bounds it.
        let mut effects = std::mem::take(&mut self.claim_ledger);
        for r in ready {
            match r.claim {
                ReadyClaim::Eat(input) => self.try_apply_eat_claim(input, &mut effects),
                ReadyClaim::Death(input) => self.try_apply_enemy_death_claim(input, &mut effects),
            }
        }
        self.claim_ledger = effects;
    }

    fn try_apply_enemy_death_claim(&mut self, input: EnemyDeathClaimInput, effects: &mut ClaimEffects) {
        let victim_id = input.player_id;
        let claim = input.claim;
        // ADMISSION SAFETY NET (lead manifesto 2026-06-11, VisualReady/ClaimReady split): a
        // death claim is honoured only from an admission that (a) declared CLAIM readiness —
        // visual world assembled AND clock re-proven by a clean pong AND its shield expired
        // (ClientClaimReady; the old ClientWorldReady is the weaker visual-only fact), (b) is
        // past its resume shield, and (c) is at least MIN_CLAIM_AFTER_ADMISSION_TICKS into a
        // live-room admission. FIRST checks (like the shape rejects): the claim id is NOT
        // burned — the honest client just waits/declares and re-claims. The anti-cheat
        // sustained-missing-claim path is unaffected: a withholder still dies to it.
        if !self.players.get(&victim_id).map(|p| p.claim_ready).unwrap_or(false) {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "claim_ready_not_seen");
            return;
        }
        if self
            .players
            .get(&victim_id)
            .map(|p| p.resume_shield_active(self.tick, RESUME_SHIELD_TICKS))
            .unwrap_or(false)
        {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "resume_admission_grace");
            return;
        }
        if self
            .players
            .get(&victim_id)
            .map(|p| {
                p.admitted_at_tick > 0
                    && self.tick.saturating_sub(p.admitted_at_tick) < MIN_CLAIM_AFTER_ADMISSION_TICKS
            })
            .unwrap_or(false)
        {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "too_soon_after_admission");
            return;
        }
        // Claim-validation tolerances (single config source — review #3). Cheap Arc clone so the
        // policy calls below don't hold an immutable borrow of `self` across the `self.reject_*` calls.
        let cf = self.config.clone();
        // Phase 1: shape (visual overlap + render-skew geometry) — needs only the claim. The skew cap
        // is config-derived (lead + interp + slack), not a const, so an honest high-RTT claim isn't
        // rejected for a large-but-legitimate victim-ahead/enemy-behind skew. (round-3 follow-up)
        if let Err(reason) = EnemyDeathClaimPolicy::shape(
            &cf.claim_fairness,
            claim.visual_distance,
            claim.victim_render_tick,
            claim.enemy_render_tick,
            self.death_claim_max_skew_ticks(),
        ) {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, reason);
            return;
        }
        // The reported visual_distance must match the reported positions' geometry (and those must
        // actually overlap) — a forged visual_distance can't sneak a far-apart kill through. (v6)
        let reported_dist = Self::distance(&claim.victim_position, &claim.enemy_position);
        if let Err(reason) = EnemyDeathClaimPolicy::positions_consistent(
            &cf.claim_fairness,
            reported_dist,
            claim.visual_distance,
        ) {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, reason);
            return;
        }
        // Per-victim monotonic claim id: drop dup/old + bump the high-water mark (replay guard). This
        // runs BEFORE the causal/state checks below so a SYNTACTICALLY VALID claim (passed shape +
        // positions) burns its claim_id even when rejected for a causal/state reason — otherwise a
        // stale claim rejected as `enemy_already_consumed_by_earlier_claim` keeps its id and could be
        // replayed forever for repeated rejects. (round-3 follow-up — dedup before causal)
        if let Some(last) = self.last_enemy_death_claim_id.get(&victim_id).copied() {
            if claim.claim_id <= last {
                self.reject_enemy_death_claim(&victim_id, claim.claim_id, "duplicate_or_old_claim_id");
                return;
            }
        }
        self.last_enemy_death_claim_id.insert(victim_id.clone(), claim.claim_id);
        // Causal (round-3 #1): this enemy LIFE was eaten by an earlier-tick eat claim, so it no longer
        // existed to kill the victim later — reject rather than let a since-eaten bot kill via its
        // still-present (immutable) history frame. Strict `<`, so a same-tick eat+death mutually stand
        // (each player saw their own outcome).
        if effects.enemy_consumed_before(&claim.enemy_id, claim.enemy_generation, claim.victim_render_tick) {
            self.reject_enemy_death_claim(
                &victim_id,
                claim.claim_id,
                "enemy_already_consumed_by_earlier_claim",
            );
            return;
        }

        // Gather: victim at its render tick, killer enemy at its render tick (generation-aware).
        let Some(victim_hist) = self.contact_history.sample_player(&victim_id, claim.victim_render_tick)
        else {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "no_victim_history");
            return;
        };
        let Some(enemy_hist) = self.contact_history.sample_enemy(
            &claim.enemy_id,
            Some(claim.enemy_generation),
            claim.enemy_render_tick,
        ) else {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "no_enemy_history");
            return;
        };
        // Reported vs reconstructed position agreement (anti-cheat / desync).
        if !EnemyDeathClaimPolicy::within_position_tolerance(
            &cf.claim_fairness,
            Self::distance(&victim_hist.position, &claim.victim_position),
        ) {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "victim_position_mismatch");
            return;
        }
        if !EnemyDeathClaimPolicy::within_position_tolerance(
            &cf.claim_fairness,
            Self::distance(&enemy_hist.position, &claim.enemy_position),
        ) {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "enemy_position_mismatch");
            return;
        }
        // Generation is validated against HISTORY, not live state: ContactHistory::sample_enemy above was
        // told Some(claim.enemy_generation) and returns None (→ "no_enemy_history") unless that
        // exact enemy life existed at enemy_render_tick. So a kill that was valid AT the claim tick
        // stands even if the enemy has since been eaten/respawned into a new generation — checking
        // the LIVE generation here would wrongly reject a legitimate rewound death. (v6 review #2)
        let enemy_respawned_at = enemy_hist.respawned_at_tick;

        // Victim availability NOW — guards a stale claim from killing a freshly-respawned life.
        let (victim_now_alive, victim_now_spawn_protected, victim_now_life_id) =
            match self.players.get(&victim_id) {
                Some(v) => (v.is_alive, v.is_spawn_protected(self.tick), v.life_id),
                None => {
                    self.reject_enemy_death_claim(&victim_id, claim.claim_id, "victim_not_found");
                    return;
                }
            };
        // Life-scope the kill to the EXACT life the claim is about — the victim analogue of the enemy
        // generation guard above. `victim_now_spawn_protected` only blocks a stale claim DURING the
        // new life's protection window; once that expires a claim against a PAST life would otherwise
        // pass the "alive now / alive at claim" checks and kill the current life. Comparing the live
        // life_id to the history frame's closes that gap: the life that SAW the overlap must be the
        // one alive now, else the victim already died and respawned. (round-3 follow-up — Blocker 1)
        if victim_now_life_id != victim_hist.life_id {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, "victim_life_mismatch");
            return;
        }
        let in_safe_zone =
            self.map.is_in_safe_zone(&victim_hist.position) || self.map.is_in_safe_zone(&enemy_hist.position);
        let reconstructed = Self::distance(&victim_hist.position, &enemy_hist.position);
        if let Err(reason) = EnemyDeathClaimPolicy::validate(
            &cf.claim_fairness,
            &EnemyDeathClaimRules {
                victim_now_alive,
                victim_now_spawn_protected,
                victim_hist_alive: victim_hist.is_alive,
                victim_hist_spawn_protected: victim_hist.spawn_protected,
                victim_hist_invincible: victim_hist.is_invincible,
                in_safe_zone,
                enemy_score: enemy_hist.score,
                victim_score: victim_hist.score,
                reconstructed,
            },
        ) {
            self.reject_enemy_death_claim(&victim_id, claim.claim_id, reason);
            return;
        }

        // ACCEPT. Positions come from the claim-validation ticks (what the victim SAW), not
        // server-now — the whole point of the claim path.
        tracing::info!(
            "💀 EnemyAtePlayerClaim ACCEPT room={} victim={} enemy={} gen={} claim={} now_tick={} victim_tick={:.1} enemy_tick={:.1} visual={:.1} reconstructed={:.1}",
            self.id, victim_id, claim.enemy_id, claim.enemy_generation, claim.claim_id, self.tick,
            claim.victim_render_tick, claim.enemy_render_tick, claim.visual_distance, reconstructed
        );
        self.telemetry_event(
            "info",
            "enemy_ate_player_claim_accept",
            json!({
                "player_id": victim_id,
                "enemy_id": claim.enemy_id,
                "enemy_generation": claim.enemy_generation,
                "claim": claim.claim_id,
                "decision": DeathDecisionCode::KillNowClaimConfirmed.as_str(),
                "policy_version": self.config.death_fairness.policy_version,
                "victim_tick": claim.victim_render_tick,
                "enemy_tick": claim.enemy_render_tick,
                "visual": claim.visual_distance,
                "reconstructed": reconstructed,
                "enemy_score": enemy_hist.score,
                "victim_score": victim_hist.score,
                // Admission context (lead manifesto #5): "accepted N ticks after a resume,
                // world declared ready" is the exact fact the ghost-death proof needed.
                "world_ready_seen": self.players.get(&victim_id).map(|p| p.world_ready).unwrap_or(false),
                "age_since_admit_ticks": self.players.get(&victim_id)
                    .map(|p| self.tick.saturating_sub(p.admitted_at_tick)).unwrap_or(0),
                "admitted_via_resume": self.players.get(&victim_id)
                    .map(|p| p.admitted_via_resume).unwrap_or(false),
            }),
        );
        // Authoritative death fact FIRST (full timeline context, positions from the claim), then
        // respawn — same ordering as the server-side kill path.
        let kill_event_id = self.alloc_event_id();
        self.emit_event(
            kill_event_id,
            self.tick,
            DomainEvent::PlayerKilledByEnemy {
                victim_id: victim_id.clone(),
                killer_enemy_id: claim.enemy_id.clone(),
                killer_enemy_generation: claim.enemy_generation,
                killer_enemy_respawned_at_tick: enemy_respawned_at,
                server_dist: reconstructed,
                reconstructed_enemy_tick: claim.enemy_render_tick.max(0.0) as u64,
                reconstructed_enemy_visible_dist: Some(reconstructed),
                server_kill_radius: EnemyContactPolicy::kill_radius(
                    self.config.collision.collision_distance_px,
                ),
                visible_gate_enabled: true,
                visible_confirm_radius_px: Some(self.config.death_fairness.visible_confirm_radius_px()),
                interp_delay_ticks: self.config.death_fairness.enemy_interp_delay_ticks,
                contact_ticks: 0,
                decision: DeathDecisionCode::KillNowClaimConfirmed,
                policy_version: self.config.death_fairness.policy_version,
                killer_position: enemy_hist.position,
                victim_position: victim_hist.position,
            },
        );
        self.respawn_player(
            &victim_id,
            Some(DeathCause::Enemy(EnemyDeath {
                killer_enemy_id: claim.enemy_id,
                server_dist: reconstructed,
                contact_ticks: 0,
                killer_position: enemy_hist.position,
                victim_position: victim_hist.position,
            })),
        );
        // Ledger (round-3 #1): the victim's THIS-life died at its render tick — a later eat by the
        // same life is now rejected as a kill-by-corpse, but the respawned life (new life_id) is
        // unaffected. The tick is victim_render_tick, the key the death resolver sorts on (#5).
        effects.kill_player_life(&victim_id, victim_hist.life_id, claim.victim_render_tick);
    }

    fn reject_enemy_death_claim(&mut self, player_id: &str, claim_id: u32, reason: &str) {
        tracing::warn!(
            "❌ EnemyDeathClaimRejected room={} player={} claim={} tick={} reason={}",
            self.id,
            player_id,
            claim_id,
            self.tick,
            reason
        );
        // Admission context (lead manifesto #5): a reject right after a resume reads very
        // differently from one mid-match — make that an in-event fact, not a log join.
        let (world_ready, age_since_admit, via_resume) = self
            .players
            .get(player_id)
            .map(|p| (p.world_ready, self.tick.saturating_sub(p.admitted_at_tick), p.admitted_via_resume))
            .unwrap_or((false, 0, false));
        self.telemetry_event(
            "warn",
            "enemy_death_claim_rejected",
            json!({
                "player_id": player_id,
                "claim": claim_id,
                "reason": reason,
                "server_tick": self.tick,
                "world_ready_seen": world_ready,
                "age_since_admit_ticks": age_since_admit,
                "admitted_via_resume": via_resume,
            }),
        );
        self.outbox.send_reliable_to(
            player_id,
            ServerMessage::EnemyDeathClaimRejected {
                claim_id,
                server_tick: self.tick,
                reason: reason.to_string(),
            },
        );
    }

    /// Shared eat-decision fields (CLAUDE.md #15: logs explain WHY). The reason is computed
    /// by the policy (`accept_reason`), so it can never drift from `can_eat`. Spliced into
    /// both accept logs and the substantive (eligibility) reject, so a log reader can tell a
    /// fair score-eat from a post-boost eat without re-deriving the rule. `target_score` /
    /// `*_invincible` / `*_speed` are read from the attacker/target's reconstructed history
    /// frame — the SAME values the policy decided on.
    fn eat_decision_fields(
        attacker_score: u32,
        target_score: u32,
        attacker_invincible: bool,
        attacker_speed: f32,
    ) -> serde_json::Value {
        json!({
            "attacker_score": attacker_score,
            "target_score": target_score,
            "attacker_invincible": attacker_invincible,
            "attacker_speed": attacker_speed,
            "accept_reason": accept_reason(attacker_invincible, attacker_score, target_score),
        })
    }

    /// Merge the keys of `extra` (an object) into `base` (an object). No-op if either is not
    /// an object. Used to splice `eat_decision_fields` into accept/reject telemetry.
    fn merge_fields(base: &mut serde_json::Value, extra: serde_json::Value) {
        if let (Some(obj), serde_json::Value::Object(extra)) = (base.as_object_mut(), extra) {
            for (k, v) in extra {
                obj.insert(k, v);
            }
        }
    }

    /// Reject a claim with no score context — used by the shape/gather rejects (NaN,
    /// no-history, position-mismatch, not-found) where attacker/target scores aren't known
    /// or aren't the reason for the reject.
    fn reject_eat_claim(&mut self, player_id: &str, claim_id: u32, reason: &str) {
        self.emit_eat_reject(player_id, claim_id, reason, None);
    }

    /// Reject from the eligibility ladder, where the attacker's history frame and the
    /// target's score ARE known — carries `eat_decision_fields` so a reject like
    /// `attacker_cannot_eat_enemy` shows the scores/boost that made it ineligible
    /// (accept_reason="none" on these by construction).
    fn reject_eat_claim_scored(
        &mut self,
        player_id: &str,
        claim_id: u32,
        reason: &str,
        decision: serde_json::Value,
    ) {
        self.emit_eat_reject(player_id, claim_id, reason, Some(decision));
    }

    fn emit_eat_reject(
        &mut self,
        player_id: &str,
        claim_id: u32,
        reason: &str,
        decision: Option<serde_json::Value>,
    ) {
        tracing::warn!(
            "❌ EatClaimRejected room={} player={} claim={} tick={} reason={}",
            self.id,
            player_id,
            claim_id,
            self.tick,
            reason
        );
        let mut fields = json!({
            "player_id": player_id,
            "claim": claim_id,
            "reason": reason,
        });
        if let Some(decision) = decision {
            Self::merge_fields(&mut fields, decision);
        }
        self.telemetry_event("warn", "eat_claim_rejected", fields);
        self.outbox.send_reliable_to(
            player_id,
            ServerMessage::EatClaimRejected { claim_id, server_tick: self.tick, reason: reason.to_string() },
        );
    }

    fn try_apply_eat_claim(&mut self, input: EatClaimInput, effects: &mut ClaimEffects) {
        let attacker_id = input.player_id;
        let claim = input.claim;
        // Claim-validation tolerances (single config source — review #3). Cheap Arc clone so the
        // policy calls don't hold an immutable borrow of `self` across the `self.reject_*` calls.
        let cf = self.config.clone();
        // Shape pre-checks (NaN/self-target) — run BEFORE the dedup mark is bumped.
        if let Err(reason) = EatClaimPolicy::shape_pre_dedup(&claim, &attacker_id) {
            self.reject_eat_claim(&attacker_id, claim.claim_id, reason);
            return;
        }
        // Same admission gates as the death-claim path (lead 2026-06-11: "death/eat claims
        // только после ClaimReady"), pre-dedup so the claim id isn't burned: claim-ready
        // declared, resume shield expired, live-room admission old enough.
        if !self.players.get(&attacker_id).map(|p| p.claim_ready).unwrap_or(false) {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "claim_ready_not_seen");
            return;
        }
        if self
            .players
            .get(&attacker_id)
            .map(|p| p.resume_shield_active(self.tick, RESUME_SHIELD_TICKS))
            .unwrap_or(false)
        {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "resume_shield");
            return;
        }
        if self
            .players
            .get(&attacker_id)
            .map(|p| {
                p.admitted_at_tick > 0
                    && self.tick.saturating_sub(p.admitted_at_tick) < MIN_CLAIM_AFTER_ADMISSION_TICKS
            })
            .unwrap_or(false)
        {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "too_soon_after_admission");
            return;
        }
        // Monotonic per-attacker claim id: drop dup/old (also bumps the high-water
        // mark so a replay can't re-trigger an accepted eat). Stateful — stays here.
        let last = self.last_eat_claim_id.get(&attacker_id).copied();
        if let Some(last) = last {
            if claim.claim_id <= last {
                self.reject_eat_claim(&attacker_id, claim.claim_id, "duplicate_or_old_claim_id");
                return;
            }
        }
        self.last_eat_claim_id.insert(attacker_id.clone(), claim.claim_id);
        // Shape post-checks (visual radius + view-skew geometry) — after the mark bump,
        // so a claim rejected here still advances the mark exactly as before.
        if let Err(reason) = EatClaimPolicy::shape_post_dedup(&cf.claim_fairness, &claim) {
            self.reject_eat_claim(&attacker_id, claim.claim_id, reason);
            return;
        }
        // Gather: the attacker's own view at its claimed render tick (Room owns the ring).
        let Some(attacker_hist) =
            self.contact_history.sample_player(&attacker_id, claim.attacker_render_tick)
        else {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "no_attacker_history");
            return;
        };
        // Causal (round-3 #1): this LIFE of the attacker was killed by an earlier-tick claim (in this
        // or a prior batch — the ledger is persistent), so it wasn't alive to make THIS eat — reject
        // rather than reconstruct a kill by a corpse. Keyed on the life the attacker was at its render
        // tick, so a respawned life's eats are unaffected. Checked here (not pre-dedup) because the
        // life_id comes from the gathered history frame.
        if effects.player_life_killed_before(&attacker_id, attacker_hist.life_id, claim.attacker_render_tick)
        {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "attacker_dead_by_earlier_claim");
            return;
        }
        if let Err(reason) = EatClaimPolicy::attacker_eligible(
            &cf.claim_fairness,
            attacker_hist.is_alive,
            Self::distance(&attacker_hist.position, &claim.attacker_position),
        ) {
            self.reject_eat_claim(&attacker_id, claim.claim_id, reason);
            return;
        }
        match claim.target_kind {
            EatTargetKind::Enemy => {
                self.try_apply_enemy_eat_claim(attacker_id, claim, attacker_hist, effects)
            }
            EatTargetKind::Player => {
                self.try_apply_player_eat_claim(attacker_id, claim, attacker_hist, effects)
            }
        }
    }

    fn try_apply_enemy_eat_claim(
        &mut self,
        attacker_id: String,
        claim: EatClaim,
        attacker_hist: PlayerHistoryState,
        effects: &mut ClaimEffects,
    ) {
        let cf = self.config.clone();
        let Some(target_hist) = self.contact_history.sample_enemy(
            &claim.target_id,
            claim.target_generation,
            claim.target_render_tick,
        ) else {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "no_enemy_history");
            return;
        };
        if !EatClaimPolicy::within_position_tolerance(
            &cf.claim_fairness,
            Self::distance(&target_hist.position, &claim.target_position),
        ) {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "enemy_position_mismatch");
            return;
        }
        let Some(enemy_idx) = self.enemies.iter().position(|e| e.id == claim.target_id) else {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "enemy_not_found");
            return;
        };
        let respawned_at = self.enemies[enemy_idx].respawned_at_tick;
        let current_generation = self.enemies[enemy_idx].generation;
        let reconstructed_dist = Self::distance(&attacker_hist.position, &target_hist.position);
        // Substantive enemy-eat rules (generation/respawn window, eat eligibility,
        // reconstructed overlap) live in the policy; the reject reasons are its contract.
        if let Err(reason) = EatClaimPolicy::enemy_eat(
            &cf.claim_fairness,
            &EnemyEatRules {
                respawned_at,
                target_floor: RenderTick::new(claim.target_render_tick).floor_tick(),
                attacker_invincible: attacker_hist.is_invincible,
                attacker_score: attacker_hist.score,
                enemy_score: target_hist.score,
                reconstructed: reconstructed_dist,
                claimed_generation: claim.target_generation,
                current_generation,
            },
        ) {
            let decision = Self::eat_decision_fields(
                attacker_hist.score,
                target_hist.score,
                attacker_hist.is_invincible,
                attacker_hist.speed,
            );
            self.reject_eat_claim_scored(&attacker_id, claim.claim_id, reason, decision);
            return;
        }
        tracing::info!(
            "🎯 PlayerAteEnemyClaim ACCEPT room={} player={} enemy={} claim={} now_tick={} attacker_tick={:.1} target_tick={:.1} visual={:.1} reconstructed={:.1}",
            self.id, attacker_id, claim.target_id, claim.claim_id, self.tick,
            claim.attacker_render_tick, claim.target_render_tick, claim.visual_distance, reconstructed_dist
        );
        let mut accept_fields = json!({
            "player_id": attacker_id,
            "enemy_id": claim.target_id,
            // Same enemy_id at different generations = different lives of one bot,
            // not a double-eat. respawned_at_tick marks when this life started.
            "enemy_generation": self.enemies[enemy_idx].generation,
            "enemy_respawned_at_tick": self.enemies[enemy_idx].respawned_at_tick,
            "claim": claim.claim_id,
            "attacker_tick": claim.attacker_render_tick,
            "target_tick": claim.target_render_tick,
            "visual": claim.visual_distance,
            "reconstructed": reconstructed_dist,
            // The max reconstructed attacker↔target distance that still counts as an
            // overlap. Logged so a disputed accept can be checked: reconstructed <= this.
            "allowed_radius": cf.claim_fairness.reconstruct_overlap_radius_px(),
        });
        // Why the eat was allowed (boost vs score) + the scores/speed it was decided on, so
        // an accept after boost_expired is provably a fair score-eat, not a stale-boost bug.
        Self::merge_fields(
            &mut accept_fields,
            Self::eat_decision_fields(
                attacker_hist.score,
                target_hist.score,
                attacker_hist.is_invincible,
                attacker_hist.speed,
            ),
        );
        self.telemetry_event("info", "player_ate_enemy_claim_accept", accept_fields);
        let enemy_score = self.enemies[enemy_idx].score;
        if let Some(p) = self.players.get_mut(&attacker_id) {
            p.score += enemy_score;
        }
        self.respawn_enemy_eaten_by(enemy_idx, &attacker_id);
        // Ledger (round-3 #1): this enemy LIFE is consumed — a later death claim "this enemy ate me"
        // at a strictly-later tick is now rejected as stale. Keyed on the eaten life's generation
        // (target_hist), which matches a death claim's enemy_generation. The tick is the EAT's
        // resolver key — attacker_render_tick (#5) — so the causal ordering matches the sort order
        // (eat claims are scheduled/sorted by attacker_render_tick, not target_render_tick).
        effects.consume_enemy(&claim.target_id, target_hist.generation, claim.attacker_render_tick);
    }

    fn try_apply_player_eat_claim(
        &mut self,
        attacker_id: String,
        claim: EatClaim,
        attacker_hist: PlayerHistoryState,
        effects: &mut ClaimEffects,
    ) {
        let cf = self.config.clone();
        let victim_id = claim.target_id.clone();
        let Some(victim_hist) = self.contact_history.sample_player(&victim_id, claim.target_render_tick)
        else {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "no_victim_history");
            return;
        };
        if !EatClaimPolicy::within_position_tolerance(
            &cf.claim_fairness,
            Self::distance(&victim_hist.position, &claim.target_position),
        ) {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "victim_position_mismatch");
            return;
        }
        if !self.players.contains_key(&attacker_id) {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "attacker_not_found");
            return;
        }
        // Copy out the victim's current availability so the borrow ends before reject.
        let (victim_now_alive, victim_now_spawn_protected, victim_now_life_id, victim_resume_shield) =
            match self.players.get(&victim_id) {
                Some(v) => (
                    v.is_alive,
                    v.is_spawn_protected(self.tick),
                    v.life_id,
                    v.resume_shield_active(self.tick, RESUME_SHIELD_TICKS),
                ),
                None => {
                    self.reject_eat_claim(&attacker_id, claim.claim_id, "victim_not_found");
                    return;
                }
            };
        // A just-resumed player can't be eaten either (the shield is symmetric — lead
        // 2026-06-11 P0 #1); same standing as the spawn-protection check in the policy.
        if victim_resume_shield {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "target_resume_shield");
            return;
        }
        // Life-scope the eat-kill to the life the claim saw (symmetric with the enemy-death path's
        // victim_life_mismatch guard): once the victim respawned and its protection expired, a stale
        // eat claim against the past life would otherwise pass the now-alive/now-protected checks and
        // kill the current life. (round-3 follow-up — Blocker 1, player-eat side)
        if victim_now_life_id != victim_hist.life_id {
            self.reject_eat_claim(&attacker_id, claim.claim_id, "victim_life_mismatch");
            return;
        }
        let in_safe_zone = self.map.is_in_safe_zone(&attacker_hist.position)
            || self.map.is_in_safe_zone(&victim_hist.position);
        let reconstructed_dist = Self::distance(&attacker_hist.position, &victim_hist.position);
        // Substantive player-eat rules (current availability, target-tick protection,
        // safe zone, eat eligibility, reconstructed overlap) live in the policy.
        if let Err(reason) = EatClaimPolicy::player_eat(
            &cf.claim_fairness,
            &PlayerEatRules {
                victim_now_alive,
                victim_now_spawn_protected,
                victim_hist_alive: victim_hist.is_alive,
                victim_hist_spawn_protected: victim_hist.spawn_protected,
                victim_hist_invincible: victim_hist.is_invincible,
                in_safe_zone,
                attacker_invincible: attacker_hist.is_invincible,
                attacker_score: attacker_hist.score,
                victim_score: victim_hist.score,
                reconstructed: reconstructed_dist,
            },
        ) {
            let decision = Self::eat_decision_fields(
                attacker_hist.score,
                victim_hist.score,
                attacker_hist.is_invincible,
                attacker_hist.speed,
            );
            self.reject_eat_claim_scored(&attacker_id, claim.claim_id, reason, decision);
            return;
        }
        tracing::info!(
            "💀 PlayerAtePlayerClaim ACCEPT room={} eater={} victim={} claim={} now_tick={} attacker_tick={:.1} target_tick={:.1} visual={:.1} reconstructed={:.1}",
            self.id, attacker_id, victim_id, claim.claim_id, self.tick,
            claim.attacker_render_tick, claim.target_render_tick, claim.visual_distance, reconstructed_dist
        );
        let mut accept_fields = json!({
            "eater": attacker_id,
            "victim": victim_id,
            "claim": claim.claim_id,
            "attacker_tick": claim.attacker_render_tick,
            "target_tick": claim.target_render_tick,
            "visual": claim.visual_distance,
            "reconstructed": reconstructed_dist,
            // The max reconstructed attacker↔target distance that still counts as an
            // overlap. Logged so a disputed accept can be checked: reconstructed <= this.
            "allowed_radius": cf.claim_fairness.reconstruct_overlap_radius_px(),
        });
        // Why the eat was allowed (boost vs score) + the scores/speed it was decided on.
        Self::merge_fields(
            &mut accept_fields,
            Self::eat_decision_fields(
                attacker_hist.score,
                victim_hist.score,
                attacker_hist.is_invincible,
                attacker_hist.speed,
            ),
        );
        self.telemetry_event("info", "player_ate_player_claim_accept", accept_fields);
        self.handle_player_eaten_by_claim(
            &attacker_id,
            &victim_id,
            claim.claim_id,
            reconstructed_dist,
            attacker_hist.position,
            victim_hist.position,
        );
        // Ledger (round-3 #1): the victim's THIS-life died — a later eat by that same life is now
        // rejected as a kill-by-corpse (the respawned life is unaffected). The tick is the eat's
        // resolver key, attacker_render_tick (#5), to match the sort order; victim_hist.life_id
        // identifies which life was eaten.
        effects.kill_player_life(&victim_id, victim_hist.life_id, claim.attacker_render_tick);
    }

    fn handle_player_eaten_by_claim(
        &mut self,
        eater_id: &str,
        eaten_id: &str,
        claim_id: u32,
        server_dist: f32,
        killer_position: Position,
        victim_position: Position,
    ) {
        let eaten_score = self.players.get(eaten_id).map(|p| p.score).unwrap_or(0);
        if let Some(eater) = self.players.get_mut(eater_id) {
            eater.score += eaten_score;
        }
        let event_id = self.alloc_event_id();
        self.emit_event(
            event_id,
            self.tick,
            DomainEvent::PlayerEaten {
                eater_id: eater_id.to_string(),
                eaten_id: eaten_id.to_string(),
                eaten_position: victim_position,
            },
        );
        self.respawn_player(
            eaten_id,
            Some(DeathCause::Player {
                killer_player_id: eater_id.to_string(),
                server_dist,
                killer_position,
                victim_position,
                claim_id,
            }),
        );
    }

    /// Pick a walkable heading for a freshly spawned enemy so the AI doesn't
    /// immediately grid-snap toward a wall on the next tick.
    fn choose_enemy_spawn_direction(&self, pos: &Position) -> Direction {
        let (gx, gy) = self.map.pos_to_grid(pos);
        for dir in [Direction::Right, Direction::Down, Direction::Left, Direction::Up] {
            let (nx, ny) = match dir {
                Direction::Up => (gx, gy.saturating_sub(1)),
                Direction::Down => (gx, gy + 1),
                Direction::Left => (gx.saturating_sub(1), gy),
                Direction::Right => (gx + 1, gy),
            };
            if !self.map.is_wall(nx, ny) {
                return dir;
            }
        }
        Direction::Right
    }

    fn check_item_collection(&mut self) {
        // Stable order so a CONTESTED pickup (two players overlapping the same point/booster this
        // tick) resolves deterministically — the lowest id wins, not whoever HashMap happened to
        // iterate first. (determinism)
        let mut player_ids: Vec<String> = self.players.keys().cloned().collect();
        player_ids.sort();
        let collision_distance = self.config.collision.collision_distance_px;
        let mut claimed_points: HashSet<usize> = HashSet::new();
        let mut claimed_boosters: HashSet<usize> = HashSet::new();
        let mut point_collectors: Vec<(String, usize)> = Vec::new();
        let mut booster_collectors: Vec<(String, usize)> = Vec::new();

        for player_id in &player_ids {
            let player = self.players.get(player_id).unwrap();
            let player_pos = player.position;

            for (idx, point) in self.points.iter().enumerate() {
                if !claimed_points.contains(&idx)
                    && Self::is_collision(&player_pos, &point.position, collision_distance)
                {
                    claimed_points.insert(idx);
                    point_collectors.push((player_id.clone(), idx));
                }
            }
            for (idx, booster) in self.boosters.iter().enumerate() {
                if !claimed_boosters.contains(&idx)
                    && Self::is_collision(&player_pos, &booster.position, collision_distance)
                {
                    claimed_boosters.insert(idx);
                    booster_collectors.push((player_id.clone(), idx));
                }
            }
        }

        // Snapshot the ids BEFORE we drain the vecs so we can name the
        // collected items in the outgoing events.
        let point_ids_by_idx: HashMap<usize, String> =
            self.points.iter().enumerate().map(|(i, p)| (i, p.id.clone())).collect();
        let booster_ids_by_idx: HashMap<usize, String> =
            self.boosters.iter().enumerate().map(|(i, b)| (i, b.id.clone())).collect();

        let mut point_indices: Vec<usize> = claimed_points.into_iter().collect();
        point_indices.sort_unstable();
        for &idx in point_indices.iter().rev() {
            self.points.remove(idx);
        }

        let mut booster_indices: Vec<usize> = claimed_boosters.into_iter().collect();
        booster_indices.sort_unstable();
        for &idx in booster_indices.iter().rev() {
            self.boosters.remove(idx);
        }

        let current_tick = self.tick;
        let tick_rate = self.tick_rate();

        for (player_id, idx) in point_collectors {
            if let Some(p) = self.players.get_mut(&player_id) {
                p.collect_point();
            }
            if let Some(point_id) = point_ids_by_idx.get(&idx).cloned() {
                let event_id = self.alloc_event_id();
                self.emit_event(
                    event_id,
                    current_tick,
                    DomainEvent::PointCollected { point_id, player_id: player_id.clone() },
                );
            }
        }
        for (player_id, idx) in booster_collectors {
            let mut speed_after = None;
            if let Some(p) = self.players.get_mut(&player_id) {
                p.collect_booster(current_tick, tick_rate);
                speed_after = Some((p.speed, p.is_invincible));
            }
            if let Some(booster_id) = booster_ids_by_idx.get(&idx).cloned() {
                let event_id = self.alloc_event_id();
                self.emit_event(
                    event_id,
                    current_tick,
                    DomainEvent::BoosterCollected { booster_id, player_id: player_id.clone() },
                );
            }
            // The boost raised speed → tell the client so prediction uses the
            // boosted speed. Collection happens AFTER movement this tick (player.update
            // already ran), so the boost is effective from the NEXT tick — otherwise
            // the client would re-simulate the current tick on boosted speed and the
            // server would not, leaving a 1-tick overshoot.
            if let Some((speed, inv)) = speed_after {
                let event_id = self.alloc_event_id();
                let effective_tick = current_tick + 1;
                tracing::info!(
                    "⚡ PlayerSpeedChanged room={} player={} event={} tick={} eff={} reason=boost_started speed={:.1} inv={}",
                    self.id, player_id, event_id, current_tick, effective_tick, speed, inv
                );
                self.telemetry_event(
                    "info",
                    "player_speed_changed",
                    json!({
                        "player_id": player_id,
                        "event_id": event_id,
                        "effective_tick": effective_tick,
                        "speed": speed,
                        "is_invincible": inv,
                        "reason": "boost_started",
                    }),
                );
                self.emit_event(
                    event_id,
                    current_tick,
                    DomainEvent::PlayerSpeedChanged {
                        effective_tick,
                        player_id,
                        speed,
                        is_invincible: inv,
                        reason: "boost_started".to_string(),
                    },
                );
            }
        }
    }

    #[allow(dead_code)]
    fn handle_player_eaten(&mut self, eater_id: &str, eaten_id: &str) {
        let eaten_score = self.players.get(eaten_id).unwrap().score;
        let eaten_pos = self.players.get(eaten_id).unwrap().position;
        if let Some(eater) = self.players.get_mut(eater_id) {
            eater.score += eaten_score;
        }
        let event_id = self.alloc_event_id();
        self.emit_event(
            event_id,
            self.tick,
            DomainEvent::PlayerEaten {
                eater_id: eater_id.to_string(),
                eaten_id: eaten_id.to_string(),
                eaten_position: eaten_pos,
            },
        );
        // Player-vs-player death — no enemy killer.
        self.respawn_player(eaten_id, None);
    }

    fn respawn_player(&mut self, player_id: &str, death: Option<DeathCause>) {
        let new_pos = self.map.get_random_spawn_position(&mut self.rng);
        let current_tick = self.tick;
        let tick_rate = self.tick_rate();
        if let Some(player) = self.players.get_mut(player_id) {
            player.respawn(new_pos, current_tick, tick_rate);
        }
        let patch = self.state_patch_for(player_id).expect("player exists after respawn");
        let event_id = self.alloc_event_id();
        // Split the death cause into the event's Optional fields. killer_player_id
        // and killer_enemy_id are mutually exclusive; both None = unknown/system.
        let (killer_player_id, killer_enemy_id, server_dist, contact_ticks, killer_position, victim_position) =
            match death {
                Some(DeathCause::Enemy(d)) => (
                    None,
                    Some(d.killer_enemy_id),
                    Some(d.server_dist),
                    Some(d.contact_ticks),
                    Some(d.killer_position),
                    Some(d.victim_position),
                ),
                Some(DeathCause::Player {
                    killer_player_id,
                    server_dist,
                    killer_position,
                    victim_position,
                    ..
                }) => (
                    Some(killer_player_id),
                    None,
                    Some(server_dist),
                    None,
                    Some(killer_position),
                    Some(victim_position),
                ),
                None => (None, None, None, None, None, None),
            };
        tracing::info!(
            "🧍 PlayerRespawned room={} player={} event={} tick={} pos=({:.1},{:.1}) killer_player={} killer_enemy={} server_dist={:?} contact_ticks={:?}",
            self.id, player_id, event_id, self.tick, new_pos.x, new_pos.y,
            killer_player_id.as_deref().unwrap_or("none"),
            killer_enemy_id.as_deref().unwrap_or("none"), server_dist, contact_ticks
        );
        self.telemetry_event(
            "info",
            "player_respawned",
            json!({
                "player_id": player_id,
                "event_id": event_id,
                "x": new_pos.x,
                "y": new_pos.y,
                "killer_player_id": killer_player_id.as_deref(),
                "killer_enemy_id": killer_enemy_id.as_deref(),
                "server_dist": server_dist,
                "contact_ticks": contact_ticks,
            }),
        );
        self.emit_event(
            event_id,
            self.tick,
            DomainEvent::PlayerRespawned {
                player_id: player_id.to_string(),
                position: new_pos,
                state_patch: patch,
                killer_player_id,
                killer_enemy_id,
                server_dist,
                contact_ticks,
                killer_position,
                victim_position,
            },
        );
    }

    fn spawn_point(&mut self) {
        let position = self.map.get_random_spawn_position(&mut self.rng);
        let point = PointItem { id: Uuid::new_v4().to_string(), position };
        let event_id = self.alloc_event_id();
        self.emit_event(event_id, self.tick, DomainEvent::PointSpawned { point: point.clone() });
        self.points.push(point);
    }

    fn spawn_booster(&mut self) {
        let position = self.map.get_random_spawn_position(&mut self.rng);
        let booster =
            Booster { id: Uuid::new_v4().to_string(), position, booster_type: BoosterType::Mushroom };
        let event_id = self.alloc_event_id();
        self.emit_event(event_id, self.tick, DomainEvent::BoosterSpawned { booster: booster.clone() });
        self.boosters.push(booster);
    }

    fn distance(a: &Position, b: &Position) -> f32 {
        let dx = a.x - b.x;
        let dy = a.y - b.y;
        (dx * dx + dy * dy).sqrt()
    }

    fn is_collision(pos1: &Position, pos2: &Position, distance: f32) -> bool {
        Self::distance(pos1, pos2) < distance
    }

    /// Seconds remaining, computed from ticks. Never reads wall-clock.
    fn time_remaining_secs(&self) -> u64 {
        let tick_rate = self.tick_rate();
        self.game_end_tick.saturating_sub(self.tick) / tick_rate
    }

    fn broadcast_game_state(&mut self) {
        // Single wall-clock stamp per broadcast — every recipient sees the same
        // server_time_ms so client TimeSync stays consistent across players.
        let server_time_ms = now_unix_ms();

        // One snapshot per tick (full-sync keyframe OR delta) → one sequence number,
        // shared by every recipient's copy this tick.
        let snapshot_seq = self.next_snapshot_seq;
        self.next_snapshot_seq += 1;

        // Per-recipient — last_processed_input_seq is the recipient's OWN seq, not the
        // shared one. The body of each snapshot is a PROJECTION of state (project_*),
        // kept separate from the delivery/sequencing here.
        let recipients: Vec<(String, Option<u32>)> = self
            .outbox
            .player_ids()
            .map(|pid| (pid.clone(), self.players.get(pid).map(|p| p.last_processed_input_seq)))
            .collect();
        if self.tick.is_multiple_of(self.config.room.full_sync_interval_ticks) {
            for (pid, last_seq) in recipients {
                let state =
                    self.project_full_keyframe(last_seq.filter(|&s| s > 0), snapshot_seq, server_time_ms);
                // Keyframe → reliable channel (never dropped behind a delta).
                self.outbox.send_reliable_to(&pid, ServerMessage::GameState(state));
            }
        } else {
            for (pid, last_seq) in recipients {
                let delta = self.project_delta(last_seq.filter(|&s| s > 0), snapshot_seq, server_time_ms);
                self.outbox.send_snapshot_to(&pid, ServerMessage::GameStateDelta(delta));
            }
        }

        // Verbose firehose: one row per tick's snapshot send (OFF by default).
        if self.telemetry_verbose() {
            let is_full = self.tick.is_multiple_of(self.config.room.full_sync_interval_ticks);
            self.telemetry_event(
                "info",
                "server_snapshot_sent",
                json!({
                    "snapshot_seq": snapshot_seq,
                    "is_full": is_full,
                    "entities": self.players.len() + self.enemies.len(),
                    "recipients": self.outbox.len(),
                }),
            );
        }
    }

    // --- snapshot projection -------------------------------------------------
    // A snapshot is a PROJECTION of authoritative state, not part of the simulation:
    // these read-only builders are the single source of truth for what a full keyframe
    // / delta contains, shared by the per-tick broadcast and the out-of-band
    // build_full_state. (They live on Room rather than a separate module because they
    // read Room's private fields; a future room/ package split could relocate them to
    // room/projection.rs.)

    /// Borrowed read-only view of the world for the snapshot projector — the one place Room hands
    /// its fields to the (extracted) projection logic. (review #1)
    fn snapshot_projector(&self) -> SnapshotProjector<'_> {
        SnapshotProjector {
            players: &self.players,
            enemies: &self.enemies,
            boosters: &self.boosters,
            points: &self.points,
            portal: self.map.portals.first().map(|p| PortalState { position: p.position }),
            tick: self.tick,
            tick_rate: self.tick_rate(),
            time_remaining: self.time_remaining_secs(),
            last_event_id: self.last_event_id(),
        }
    }

    /// Project authoritative state into a full keyframe DTO. `snapshot_seq` is 0 for an
    /// out-of-band keyframe (join / RequestFullState) and the per-tick sequence otherwise.
    fn project_full_keyframe(
        &self,
        last_processed_input_seq: Option<u32>,
        snapshot_seq: u64,
        server_time_ms: u64,
    ) -> GameStateUpdate {
        self.snapshot_projector().full(last_processed_input_seq, snapshot_seq, server_time_ms)
    }

    /// Project authoritative state into a movement delta DTO (positions/speeds only).
    fn project_delta(
        &self,
        last_processed_input_seq: Option<u32>,
        snapshot_seq: u64,
        server_time_ms: u64,
    ) -> GameStateDelta {
        self.snapshot_projector().delta(last_processed_input_seq, snapshot_seq, server_time_ms)
    }

    fn end_game(&mut self) -> Vec<PlayerReward> {
        self.is_active = false;

        let winner = self
            .players
            .values()
            // HUMANS ONLY (lead review P1): fillers may sit high on the scoreboard all
            // match, but the official GameEnded winner — the result screen, the winner
            // reward — always names the best human. A hidden bot must never steal the
            // final outcome from the people actually playing.
            .filter(|p| !p.is_filler())
            // Deterministic tie-break: highest score, then lowest id — so an equal-score finish
            // names the same winner every run instead of by HashMap order. (determinism)
            .max_by(|a, b| a.score.cmp(&b.score).then_with(|| b.id.cmp(&a.id)))
            .map(|p| GameWinner { user_id: p.user_id.clone(), nickname: p.nickname.clone(), score: p.score });

        // Rewards: HUMANS ONLY. Fillers stay eligible for the visual winner above (someone
        // else winning is realistic), but no reward row may carry a "filler:" user_id —
        // rewards persist to real accounts and a filler must never touch the DB/economy.
        let mut rewards = Vec::new();
        for player in self.players.values().filter(|p| !p.is_filler()) {
            let is_winner = winner.as_ref().map(|w| w.user_id == player.user_id).unwrap_or(false);
            let crystals = if is_winner {
                self.config.economy.winner_crystals
            } else {
                self.config.economy.participant_crystals
            };
            rewards.push(PlayerReward { user_id: player.user_id.clone(), crystals, is_winner });
        }

        // GameWinner/PlayerReward carry the account user_id (the persistent id rewards credit).
        // Telemetry also wants the per-connection player_id (the map key), so re-derive the
        // winning HUMAN and log BOTH unambiguously. Same filter + deterministic tie-break as
        // the GameEnded winner above, so the two always agree.
        let winner_player = self
            .players
            .values()
            .filter(|p| !p.is_filler())
            .max_by(|a, b| a.score.cmp(&b.score).then_with(|| b.id.cmp(&a.id)));
        // Filler endgame ranks (lead review: "are the bots stealing wins?" must be a
        // dashboard read, not a replay session). Ranked over ALL players, as the
        // scoreboard the humans saw. Replaces the old winner_is_filler field (now
        // always false by construction) with strictly more information.
        let mut ranked: Vec<&Player> = self.players.values().collect();
        ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        let filler_top1 = ranked.first().map(|p| p.is_filler()).unwrap_or(false);
        let filler_top3_count = ranked.iter().take(3).filter(|p| p.is_filler()).count();
        let best_human_rank = ranked.iter().position(|p| !p.is_filler()).map(|i| i + 1);
        self.telemetry_event(
            "info",
            "room_ended",
            json!({
                "winner_player_id": winner_player.map(|p| &p.id),
                "winner_user_id": winner_player.map(|p| &p.user_id),
                "winner_nickname": winner_player.map(|p| &p.nickname),
                "winner_score": winner_player.map(|p| p.score),
                "player_count": self.players.len(),
                "human_count": self.human_count(),
                "filler_count": self.filler_count(),
                "reward_count": rewards.len(),
                "filler_top1": filler_top1,
                "filler_top3_count": filler_top3_count,
                "best_human_rank": best_human_rank,
                "human_lost_to_filler": filler_top1 && best_human_rank.is_some(),
            }),
        );

        if let Some(w) = winner {
            self.outbox.broadcast_reliable(ServerMessage::GameEnded { winner: w, rewards: rewards.clone() });
        }

        // The match is over: fillers evaporate so the dead room can GC — the manager only
        // removes inactive rooms at player_count() == 0, and a filler must never pin one.
        // Clients already hold GameEnded (broadcast above); the result screen renders from
        // that message, not from room state, so no PlayerLeft ceremony is needed.
        self.players.retain(|_, p| !p.is_filler());
        self.filler_controllers.clear();
        self.scheduled_filler_exits.clear();
        self.pending_filler_spawns.clear();

        rewards
    }

    /// The single funnel for gameplay events: the simulation hands a `DomainEvent`
    /// (a fact), and this stamps the transport bookkeeping (`event_id`, `server_tick`),
    /// maps it to the wire `ServerEvent`, and broadcasts it reliably. The caller still
    /// allocates `event_id` (so its logs/telemetry can reference it at the same point);
    /// the mapping to the wire format lives in `game::events`, not in the rules, and the
    /// delivery in `game::outbox`.
    fn emit_event(&mut self, event_id: u64, server_tick: u64, event: DomainEvent) {
        // Verbose-only: pairs with `server_snapshot_sent` so a reader can verify the
        // reliable-before-snapshot invariant for a given (event_id, server_tick) — the
        // hard event is emitted here, and the snapshot at the same/next tick carries
        // last_event_id >= event_id. Telemetry observes; it changes no gameplay.
        if self.telemetry_verbose() {
            self.telemetry_event(
                "info",
                "server_reliable_event_emitted",
                json!({
                    "event_id": event_id,
                    "server_tick": server_tick,
                    "event_kind": event.kind(),
                }),
            );
        }
        self.outbox.broadcast_reliable(ServerMessage::Event(event.into_server_event(event_id, server_tick)));
    }

    /// Build an immediate keyframe (full GameState). Used right after join (sent
    /// strictly after GameJoined, see do_join_game) and to answer a client
    /// RequestFullState (e.g. on app resume).
    /// Join-time entry point. Identical to `build_full_state`; kept as a named
    /// alias so the join path reads clearly.
    pub fn build_initial_full(&self) -> ServerMessage {
        self.build_full_state()
    }

    /// Build a full keyframe of the current room state (the authoritative answer
    /// to a client `RequestFullState`, and the join snapshot).
    /// Out-of-band full keyframe (join / RequestFullState): no recipient-specific input
    /// seq, and `snapshot_seq: 0` so it is never part of the per-tick sequence.
    pub fn build_full_state(&self) -> ServerMessage {
        self.build_full_state_reply(None)
    }

    /// Same out-of-band keyframe, stamped with the client's `RequestFullState`
    /// correlation id (echoed back as `full_state_request_id`). The echo is what lets
    /// the client PROVE a seq=0 keyframe answers its CURRENT request rather than being
    /// an expired reply replayed out of a thawed socket's backlog (lead manifesto
    /// 2026-06-10). `None` = join keyframe / legacy request.
    pub fn build_full_state_reply(&self, request_id: Option<u64>) -> ServerMessage {
        let mut full = self.project_full_keyframe(None, 0, now_unix_ms());
        full.full_state_request_id = request_id;
        ServerMessage::GameState(full)
    }

    pub fn player_count(&self) -> usize {
        self.players.len()
    }

    /// Humans only (excludes fillers). The capacity/matchmaking population.
    pub fn human_count(&self) -> usize {
        self.players.values().filter(|p| !p.is_filler()).count()
    }

    pub fn filler_count(&self) -> usize {
        self.players.values().filter(|p| p.is_filler()).count()
    }

    pub fn is_full(&self) -> bool {
        // HUMANS + reserved slots (held for resuming droppers) only. Fillers never cost a
        // human a seat: a room "full" of fillers still admits max_players humans — the
        // rebalance then walks fillers out via DELAYED exits (never same-tick).
        self.human_count() + self.reserved_slots >= self.config.room.max_players
    }

    pub fn has_space(&self) -> bool {
        !self.is_full()
    }

    /// May the matchmaker seat a NEW player here? Active, and enough match left that the
    /// join isn't "GameEnded in 5 seconds" (lead review P1 — especially bad in a
    /// filler-populated lobby). RESUMES bypass this: returning to your own match is
    /// always right, however little is left.
    pub fn accepting_new_joins(&self) -> bool {
        self.is_active && self.tick.saturating_add(MIN_JOINABLE_REMAINING_TICKS) <= self.game_end_tick
    }

    /// Disconnect path: capture the player's resume state (position/direction/score),
    /// RESERVE its capacity slot, and remove the entity — so the slot is held for a resume but
    /// the live sim no longer carries a dead connection. Returns the state to stash in the
    /// resume session, or None if the player wasn't here (already gone). Reusing
    /// `remove_player` keeps PlayerLeft/telemetry behaviour identical to a normal leave; only
    /// the reservation is new.
    pub fn hold_player_for_resume(&mut self, player_id: &str) -> Option<PlayerResumeState> {
        let snapshot = self.players.get(player_id)?.resume_snapshot();
        self.reserved_slots += 1;
        self.remove_player(player_id);
        Some(snapshot)
    }

    /// Resume path: consume the held slot and re-add the player to THIS room AT THE POSITION
    /// THEY DROPPED FROM with their score restored (apply_resume) — not a random spawn, which
    /// would be a free escape teleport. Mirrors `try_add_player`'s is_active/capacity gating.
    /// The reservation is consumed up front: this resume resolves the held slot whether it
    /// succeeds or fails (a rejected resume frees the slot, it doesn't leak).
    pub fn resume_player(
        &mut self,
        user_id: String,
        nickname: String,
        outbound: PlayerOutbound,
        resume: PlayerResumeState,
    ) -> Result<(String, ServerMessage), RoomJoinRejected> {
        // Surface a drifted reservation (lead review): consuming a slot that wasn't held means
        // the accounting is off. The is_full check below still prevents over-fill, so this is a
        // diagnostic, not a guard.
        if self.reserved_slots == 0 {
            tracing::warn!(
                "[ROOM {}] resume_player consumed a reservation that wasn't held — reserved_slots accounting drifted",
                self.id
            );
        }
        self.release_reservation();
        if !self.is_active {
            return Err(RoomJoinRejected::Inactive);
        }
        if self.is_full() {
            return Err(RoomJoinRejected::Full);
        }
        // admit_player(Resume) places the entity AT the dropped position, restores score, builds
        // the keyframe AFTER the restore, and emits `player_resumed` (not a phantom `player_joined`
        // at a random spawn). The returned keyframe already shows the correct position + score.
        Ok(self.admit_player(user_id, nickname, outbound, PlayerAdmission::Resume(resume)))
    }

    /// ClientWorldReady from this player's connection: its world-sync gate released, so death
    /// claims may now be honoured (see the `world_not_ready` gate in try_apply_enemy_death_claim).
    /// Per-admission: every join/resume constructs a fresh Player with `world_ready: false`, so a
    /// declaration can't outlive the connection that made it. Telemetry closes the proof chain
    /// client_world_sync_complete → client_world_ready_received.
    pub fn set_player_world_ready(&mut self, player_id: &str, world_sync_epoch: u32, anchor_tick: u64) {
        let Some(player) = self.players.get_mut(player_id) else {
            tracing::warn!("[ROOM {}] ClientWorldReady for unknown player {}", self.id, player_id);
            return;
        };
        let was_ready = player.world_ready;
        player.world_ready = true;
        self.telemetry_event(
            "info",
            "client_world_ready_received",
            json!({
                "player_id": player_id,
                "world_sync_epoch": world_sync_epoch,
                "anchor_tick": anchor_tick,
                "server_tick": self.tick,
                "was_already_ready": was_ready,
            }),
        );
    }

    /// ClientClaimReady — the STRICTER second readiness stage (lead 2026-06-11): visual
    /// world + clean-pong clock + expired admission shield. Death/eat claims are honoured
    /// only after this; see the `claim_ready_not_seen` gates.
    pub fn set_player_claim_ready(&mut self, player_id: &str, world_sync_epoch: u32, anchor_tick: u64) {
        let Some(player) = self.players.get_mut(player_id) else {
            tracing::warn!("[ROOM {}] ClientClaimReady for unknown player {}", self.id, player_id);
            return;
        };
        let was_ready = player.claim_ready;
        player.claim_ready = true;
        self.telemetry_event(
            "info",
            "client_claim_ready_received",
            json!({
                "player_id": player_id,
                "world_sync_epoch": world_sync_epoch,
                "anchor_tick": anchor_tick,
                "server_tick": self.tick,
                "was_already_ready": was_ready,
            }),
        );
    }

    /// Grace expired (or a resume resolved the slot): give back a held slot. Saturating so a
    /// double-release (e.g. a racing sweep) can't underflow the counter.
    pub fn release_reservation(&mut self) {
        self.reserved_slots = self.reserved_slots.saturating_sub(1);
    }

    /// Held-slot count, for resume telemetry/diagnostics ("why is the room full with
    /// players.len() < max?").
    pub fn reserved_slots(&self) -> usize {
        self.reserved_slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_shared::{load_config_from_str, load_maze_from_str};
    use tokio::sync::watch; // PlayerOutbound's snapshot lane lives in game::outbox now.
                            // Tests assert on the wire events read back off the reliable channel.
    use crate::protocol::ServerEvent;

    fn make_room(seed: u64) -> Room {
        // Filler-free baseline: these scenarios pin gameplay rules for human players and
        // PvE enemies; wandering fillers would perturb RNG draws and enemy targeting.
        // Filler behavior has its own scenario suite (filler_tests) that opts in.
        let mut config = load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap();
        config.filler.enabled = false;
        // The shipping toml runs mob-free (count = 0); the enemy scenarios here need the
        // mobs back. 12 keeps every seed-pinned RNG draw identical to the historical runs.
        config.ai.count = 12;
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let (room, _input_tx, _claim_tx) = Room::new(Arc::new(config), maze, Some(seed));
        room
    }

    #[test]
    fn full_keyframe_projection_reflects_state_with_out_of_band_seq() {
        let room = make_room(7);
        // build_full_state is the out-of-band keyframe (join / RequestFullState), built
        // by the shared projector. It must project the room's authoritative entity set…
        let ServerMessage::GameState(full) = room.build_full_state() else {
            panic!("build_full_state must be a GameState keyframe");
        };
        assert_eq!(full.enemies.len(), room.enemies.len());
        assert_eq!(full.points.len(), room.points.len());
        assert_eq!(full.tick, room.tick);
        // …with the out-of-band invariants the dedup must preserve.
        assert_eq!(full.snapshot_seq, 0, "out-of-band keyframe is seq 0");
        assert_eq!(full.last_processed_input_seq, None);
    }

    #[test]
    fn game_end_tick_computed_from_config_not_wallclock() {
        let room = make_room(1);
        // game_duration_sec=40, tick_rate=60 → 2400 ticks.
        assert_eq!(room.game_end_tick, 40 * 60);
        assert_eq!(room.tick, 0);
    }

    #[test]
    fn next_spawn_ticks_initialised_from_intervals() {
        let room = make_room(1);
        // Init formula = interval_sec * tick_rate, independent of the config values.
        let tr = room.config.room.tick_rate as f32;
        assert_eq!(room.next_point_spawn_tick, (room.config.spawn.point_interval_sec * tr) as u64);
        assert_eq!(room.next_booster_spawn_tick, (room.config.spawn.booster_interval_sec * tr) as u64);
    }

    #[test]
    fn room_advances_one_tick_per_update() {
        let mut room = make_room(1);
        let before = room.tick;
        room.update();
        assert_eq!(room.tick, before + 1);
    }

    #[test]
    fn game_ends_when_tick_reaches_game_end_tick() {
        let mut room = make_room(1);
        // Fast-forward by setting tick directly. update() increments first, so set to end-1.
        room.tick = room.game_end_tick - 1;
        let rewards = room.update();
        assert!(rewards.is_some(), "game should end exactly at game_end_tick");
        assert!(!room.is_active);
    }

    #[test]
    fn deterministic_spawn_positions_for_same_seed() {
        let r1 = make_room(12345);
        let r2 = make_room(12345);
        // Same seed → same enemy spawn positions.
        assert_eq!(r1.enemies[0].position.x, r2.enemies[0].position.x);
        assert_eq!(r1.enemies[0].position.y, r2.enemies[0].position.y);
        assert_eq!(r1.enemies[1].position.x, r2.enemies[1].position.x);
        // Portal too.
        assert_eq!(r1.map.portals[0].position.x, r2.map.portals[0].position.x);
    }

    #[test]
    fn divergent_spawn_positions_for_different_seeds() {
        let r1 = make_room(1);
        let r2 = make_room(2);
        let any_diff = r1.enemies[0].position.x != r2.enemies[0].position.x
            || r1.enemies[1].position.x != r2.enemies[1].position.x
            || r1.map.portals[0].position.x != r2.map.portals[0].position.x;
        assert!(any_diff, "different seeds must diverge at least once");
    }

    /// Drop a STATIONARY player with a CONTROLLED id (overrides the random UUID) at `pos`, so a
    /// determinism test can pin which id is "lowest". Returns the reliable receiver to read the
    /// events the room broadcasts (kept alive so the bounded channel doesn't report closed).
    fn put_controlled_player(room: &mut Room, id: &str, pos: Position) -> mpsc::Receiver<ServerMessage> {
        let mut p = Player::new(format!("user-{id}"), format!("nick-{id}"), pos, room.config.clone());
        p.id = id.to_string();
        p.position = pos;
        p.speed = 0.0; // stationary — the only way it moves off `pos` is a portal teleport
        let (outbound, rrx, _srx) = test_outbound();
        std::mem::forget(_srx); // snapshot watch receiver: unused here, keep the sender valid
        room.players.insert(id.to_string(), p);
        room.outbox.insert(id.to_string(), outbound);
        room.input_slots.insert(id.to_string(), PlayerInputSlot::default());
        rrx
    }

    fn portal_teleports(rx: &mut mpsc::Receiver<ServerMessage>) -> Vec<(String, Position)> {
        drain_events(rx)
            .into_iter()
            .filter_map(|ev| match ev {
                ServerEvent::PortalTeleport { player_id, to, .. } => Some((player_id, to)),
                _ => None,
            })
            .collect()
    }

    /// review #1 (determinism): two players sitting on the SAME portal is a contested pickup — it
    /// consumes the single portal AND advances RoomRng (the teleport destination). Iterating players
    /// in HashMap order made the winner (and the RNG sequence after) depend on a per-instance random
    /// hash seed. The loop now sorts by id, so the LOWEST id deterministically wins.
    #[test]
    fn contested_portal_pickup_lowest_id_wins() {
        let mut room = make_room(99);
        let portal_pos = room.map.portals[0].position;
        let mut lo = put_controlled_player(&mut room, "p-aaaa", portal_pos);
        let _hi = put_controlled_player(&mut room, "p-bbbb", portal_pos);

        room.update();

        let teleports = portal_teleports(&mut lo);
        assert_eq!(teleports.len(), 1, "a contested portal is consumed exactly once");
        assert_eq!(teleports[0].0, "p-aaaa", "the lowest id wins the contested pickup");
        assert!(room.map.portals.is_empty(), "the single portal is consumed");
        assert!(room.portal_respawn_tick.is_some(), "consumed portal goes into respawn cooldown");
    }

    /// review #1 (determinism): the SAME contested setup on two identically-seeded rooms must pick
    /// the SAME winner AND teleport it to the SAME destination — i.e. the RNG advances identically
    /// regardless of the players HashMap's per-instance iteration order (which the sort neutralises).
    #[test]
    fn contested_portal_pickup_rng_is_replay_stable() {
        let run = |seed: u64| {
            let mut room = make_room(seed);
            let portal_pos = room.map.portals[0].position;
            let mut lo = put_controlled_player(&mut room, "p-aaaa", portal_pos);
            let _hi = put_controlled_player(&mut room, "p-bbbb", portal_pos);
            room.update();
            portal_teleports(&mut lo)
        };
        let a = run(99);
        let b = run(99);
        assert_eq!(a.len(), 1, "exactly one pickup");
        assert_eq!(a, b, "same seed + same ids -> same winner and same teleport destination");
    }

    /// review #1 (determinism): when two players' boosts expire on the SAME tick, both emit a
    /// PlayerSpeedChanged whose event_id is allocated in player-iteration order. HashMap order would
    /// make that allocation non-deterministic; the sorted loop guarantees the LOWEST id gets the
    /// lower event_id every run, so the reliable-event sequence is replay-stable.
    #[test]
    fn simultaneous_boost_expiry_event_ids_ordered_by_id() {
        let mut room = make_room(123);
        room.map.remove_portal(); // isolate: no portal pickups, only boost-expiry speed changes
        let boosted = room.config.gameplay.boosted_speed;
        let base = room.config.gameplay.base_speed;
        assert!(boosted > base, "test premise: boosted speed differs from base");

        let pos = Position { x: 100.0, y: 100.0 };
        let mut lo = put_controlled_player(&mut room, "p-aaaa", pos);
        let _hi = put_controlled_player(&mut room, "p-bbbb", pos);
        // Both currently boosted, expiring on the tick this update() will reach.
        let expire_tick = room.tick + 1;
        for id in ["p-aaaa", "p-bbbb"] {
            let p = room.players.get_mut(id).unwrap();
            p.speed = boosted;
            p.boosted_until_tick = Some(expire_tick);
        }

        room.update();

        let speed_changes: Vec<(String, u64)> = drain_events(&mut lo)
            .into_iter()
            .filter_map(|ev| match ev {
                ServerEvent::PlayerSpeedChanged { player_id, event_id, reason, .. }
                    if reason == "boost_expired" =>
                {
                    Some((player_id, event_id))
                }
                _ => None,
            })
            .collect();
        assert_eq!(speed_changes.len(), 2, "both boosts expire this tick");
        let id_a = speed_changes.iter().find(|(p, _)| p == "p-aaaa").unwrap().1;
        let id_b = speed_changes.iter().find(|(p, _)| p == "p-bbbb").unwrap().1;
        assert!(id_a < id_b, "lowest id gets the lower event_id (stable ordering), got a={id_a} b={id_b}");
    }

    /// review #10: a reliable overflow forces a resync (#2) AND bumps the per-room counter, so a
    /// chronically-behind room is visible as a running total — not just a stream of per-event logs.
    #[test]
    fn forced_resync_count_increments_on_reliable_overflow() {
        let mut room = make_room(7);
        room.map.remove_portal(); // isolate: the only reliable event will be the boost-expiry
        let boosted = room.config.gameplay.boosted_speed;
        {
            // Drop the reliable receiver → the player's reliable channel is CLOSED, so the next
            // reliable broadcast to them fails and flags a forced resync.
            let _closed = put_controlled_player(&mut room, "p-aaaa", Position { x: 100.0, y: 100.0 });
        }
        // A boost expiring this tick forces a reliable PlayerSpeedChanged broadcast.
        let p = room.players.get_mut("p-aaaa").unwrap();
        p.speed = boosted;
        p.boosted_until_tick = Some(room.tick + 1);

        assert_eq!(room.forced_resync_count, 0, "no resyncs before the overflow");
        room.update();
        assert_eq!(room.forced_resync_count, 1, "a reliable overflow forces exactly one resync");
        assert!(
            !room.outbox.player_ids().any(|id| id == "p-aaaa"),
            "the overflowed player's outbound is dropped to force the reconnect=resync"
        );
    }

    #[test]
    fn six_hundred_ticks_eq_ten_seconds_at_60hz() {
        let mut room = make_room(1);
        for _ in 0..600 {
            room.update();
        }
        // 10 sec elapsed, 30 sec remaining (40s match).
        assert_eq!(room.tick, 600);
        let expected_remaining = (room.game_end_tick - room.tick) / 60;
        assert_eq!(expected_remaining, 30);
    }

    fn make_room_with_input() -> (Room, mpsc::Sender<PlayerInput>) {
        let (room, input_tx, _claim_tx) = make_room_with_claim();
        (room, input_tx)
    }

    /// Like `make_room_with_input` but pins the death policy BELOW v6, so check_enemy_collisions
    /// still SERVER-KILLS. At v6 death is claim-based and the server is observe-only; these tests
    /// exercise the anti-cheat / legacy FALLBACK kill path that stays for no-claim clients.
    fn make_room_with_input_server_death() -> (Room, mpsc::Sender<PlayerInput>) {
        let mut config = load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap();
        config.death_fairness.policy_version = 5;
        config.filler.enabled = false; // filler-free baseline (see make_room)
        config.ai.count = 12; // mobs back for enemy scenarios (see make_room)
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let (room, input_tx, _claim_tx) = Room::new(Arc::new(config), maze, Some(7));
        (room, input_tx)
    }

    fn make_room_with_claim() -> (Room, mpsc::Sender<PlayerInput>, mpsc::Sender<EatClaimInput>) {
        let mut config = load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap();
        config.filler.enabled = false; // filler-free baseline (see make_room)
        config.ai.count = 12; // mobs back for enemy scenarios (see make_room)
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        Room::new(Arc::new(config), maze, Some(7))
    }

    /// Stage 6 test helper: build a PlayerOutbound pair for tests that need to
    /// drive add_player without spinning up a real WebSocket.
    fn test_outbound(
    ) -> (PlayerOutbound, mpsc::Receiver<ServerMessage>, watch::Receiver<Option<ServerMessage>>) {
        let (reliable_tx, reliable_rx) = mpsc::channel(64);
        let (snapshot_tx, snapshot_rx) = watch::channel(None);
        (PlayerOutbound { reliable: reliable_tx, snapshot: snapshot_tx }, reliable_rx, snapshot_rx)
    }

    /// Scenario: "player join capacity cannot race." try_add_player is the only
    /// production path in, and it enforces capacity under the same &mut turn that
    /// inserts. Filling to max_players must succeed; the next attempt must be
    /// rejected with RoomFull and must NOT grow the room past capacity. (The
    /// RoomManager layer turns that Err into a retry against another room — here
    /// we pin the room-level invariant that makes the race impossible.)
    #[test]
    fn try_add_player_enforces_capacity_under_one_turn() {
        let mut room = make_room(1);
        let max = room.config.room.max_players;

        // Keep the outbound receivers alive so the bounded reliable channels
        // don't report as closed while we fill the room.
        let mut keep_alive = Vec::new();
        for i in 0..max {
            let (outbound, r, s) = test_outbound();
            keep_alive.push((r, s));
            let res = room.try_add_player(format!("u{i}"), format!("p{i}"), outbound);
            assert!(res.is_ok(), "join {i} within capacity must succeed");
        }
        assert_eq!(room.player_count(), max);
        assert!(room.is_full());

        // The (max+1)-th joiner races in on a full room: rejected, count unchanged.
        let (outbound, _r, _s) = test_outbound();
        let res = room.try_add_player("overflow".into(), "overflow".into(), outbound);
        assert_eq!(res.err(), Some(RoomJoinRejected::Full), "join past capacity must be rejected");
        assert_eq!(room.player_count(), max, "a rejected join must not insert the player");
    }

    // --- Reconnect / resume slot-holding (imperative-shell capacity bookkeeping; the
    // gameplay core is untouched) ------------------------------------------------------

    #[test]
    fn dropped_slot_is_held_then_resume_restores_state() {
        let mut room = make_room(1);
        let (outbound, _r, _s) = test_outbound();
        let (pid, _full) = room.add_player("u0".into(), "p0".into(), outbound);
        // Stamp a recognisable state, then move them somewhere NOT a default spawn so we can
        // prove resume restores the drop position rather than a random respawn.
        room.players.get_mut(&pid).unwrap().score = 7;
        let dropped_pos = Position { x: 123.5, y: 456.0 };
        room.players.get_mut(&pid).unwrap().position = dropped_pos;

        // Drop: entity removed, slot reserved, state handed back for the resume session.
        let held = room.hold_player_for_resume(&pid).expect("drop returns the resume state to stash");
        assert_eq!(held.score, 7);
        assert_eq!(held.position, dropped_pos);
        assert_eq!(room.player_count(), 0, "dropped entity removed from the sim");

        // Resume within grace: same room, state restored onto the fresh entity, slot released.
        let (outbound, _r2, _s2) = test_outbound();
        let (new_pid, full) = room
            .resume_player("u0".into(), "p0".into(), outbound, held)
            .expect("resume into an active, slot-held room must succeed");
        assert_eq!(room.player_count(), 1);
        let resumed = room.players.get(&new_pid).unwrap();
        assert_eq!(resumed.score, 7, "score is restored on resume");
        assert_eq!(
            resumed.position, dropped_pos,
            "position is restored — resume is NOT a free random-spawn teleport"
        );
        // The keyframe rebuilt after the restore already reflects the restored state.
        let ServerMessage::GameState(ks) = full else {
            panic!("resume must hand back a GameState keyframe");
        };
        let me = ks.players.iter().find(|p| p.id == new_pid).unwrap();
        assert_eq!(me.score, 7, "resume keyframe shows the restored score, not the spawn default");
        assert_eq!(me.position, dropped_pos, "resume keyframe shows the restored position");
    }

    #[test]
    fn held_slot_counts_against_capacity_until_resumed() {
        let mut room = make_room(1);
        let max = room.config.room.max_players;
        let mut keep_alive = Vec::new();
        let mut pids = Vec::new();
        for i in 0..max {
            let (outbound, r, s) = test_outbound();
            keep_alive.push((r, s));
            let (pid, _) = room.add_player(format!("u{i}"), format!("p{i}"), outbound);
            pids.push(pid);
        }
        assert!(room.is_full());

        // One player drops: the entity leaves but the slot stays reserved, so the room is
        // STILL full — a third player can't steal the dropped player's place.
        let held = room.hold_player_for_resume(&pids[0]).unwrap();
        assert_eq!(room.player_count(), max - 1);
        assert!(room.is_full(), "held slot keeps the room full");
        let (outbound, _r, _s) = test_outbound();
        assert_eq!(
            room.try_add_player("intruder".into(), "intruder".into(), outbound).err(),
            Some(RoomJoinRejected::Full),
            "a held slot must not be taken by another player"
        );

        // The original returns: consumes the reservation, room back to exactly max.
        let (outbound, _r2, _s2) = test_outbound();
        room.resume_player("u0".into(), "p0".into(), outbound, held)
            .expect("resume must reclaim the held slot");
        assert_eq!(room.player_count(), max);
        assert!(room.is_full());
    }

    #[test]
    fn grace_expiry_release_frees_the_held_slot() {
        let mut room = make_room(1);
        let max = room.config.room.max_players;
        let mut keep_alive = Vec::new();
        let mut pids = Vec::new();
        for i in 0..max {
            let (outbound, r, s) = test_outbound();
            keep_alive.push((r, s));
            let (pid, _) = room.add_player(format!("u{i}"), format!("p{i}"), outbound);
            pids.push(pid);
        }
        room.hold_player_for_resume(&pids[0]).unwrap();
        assert!(room.is_full(), "slot held right after the drop");

        // Grace expired (the sweep calls this): the slot is given back and a NEW player can join.
        room.release_reservation();
        assert!(!room.is_full(), "released slot frees capacity");
        let (outbound, _r, _s) = test_outbound();
        assert!(
            room.try_add_player("newcomer".into(), "newcomer".into(), outbound).is_ok(),
            "after grace expiry the freed slot is joinable"
        );
    }

    #[test]
    fn resume_into_ended_room_is_rejected_and_releases_slot() {
        let mut room = make_room(1);
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u0".into(), "p0".into(), outbound);
        let held = room.hold_player_for_resume(&pid).unwrap();

        room.is_active = false; // the match ended while the player was away

        let (outbound, _r2, _s2) = test_outbound();
        assert_eq!(
            room.resume_player("u0".into(), "p0".into(), outbound, held).err(),
            Some(RoomJoinRejected::Inactive),
            "can't resume into a finished match"
        );
        // resume_player releases the reservation before the active check, so a rejected
        // resume doesn't leak the held slot.
        assert!(!room.is_full());
        assert_eq!(room.reserved_slots, 0, "rejected resume leaves no dangling reservation");
    }

    #[test]
    fn holding_an_absent_player_is_a_noop() {
        let mut room = make_room(1);
        assert!(room.hold_player_for_resume("ghost").is_none());
        assert_eq!(room.reserved_slots, 0, "no slot reserved for a player that wasn't here");
    }

    /// Scenario: a room that ended between find_or_create_room()'s read snapshot and
    /// the write lock must reject the join (Inactive) under the same turn — never add a
    /// player into a dead room. The RoomManager retries into a live/fresh room.
    #[test]
    fn try_add_player_rejects_inactive_room_under_one_turn() {
        let mut room = make_room(1);
        room.is_active = false; // room ended after it was picked
        assert!(!room.is_full(), "guard isn't masking a capacity reject");

        let (outbound, _r, _s) = test_outbound();
        let res = room.try_add_player("u".into(), "p".into(), outbound);
        assert_eq!(res.err(), Some(RoomJoinRejected::Inactive), "join into a dead room is rejected");
        assert_eq!(room.player_count(), 0, "no player added to an inactive room");
    }

    /// RoomCommand mailbox: feeding a command list via apply_command drives the room
    /// exactly like the inbound channel does (the channel just forwards into the same
    /// handler), so tests can replay a sequence without a channel.
    #[test]
    fn apply_command_replays_inputs_like_the_channel() {
        let (mut room, _tx) = make_room_with_input();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u1".into(), "Nick".into(), outbound);

        // seq=5 Up — accepted via the mailbox, last_processed becomes 5.
        room.apply_command(RoomCommand::PlayerInput(PlayerInput {
            player_id: pid.clone(),
            seq: 5,
            target_tick: 0,
            direction: Direction::Up,
        }));
        room.update();
        assert_eq!(room.players[&pid].last_processed_input_seq, 5);
        assert_eq!(room.players[&pid].desired_direction, Direction::Up);

        // Older seq=3 — ignored through the same path, direction unchanged.
        room.apply_command(RoomCommand::PlayerInput(PlayerInput {
            player_id: pid.clone(),
            seq: 3,
            target_tick: 0,
            direction: Direction::Left,
        }));
        room.update();
        assert_eq!(room.players[&pid].last_processed_input_seq, 5);
        assert_eq!(room.players[&pid].desired_direction, Direction::Up);
    }

    fn test_claim(claim_id: u32) -> EatClaim {
        EatClaim {
            claim_id,
            target_kind: EatTargetKind::Enemy,
            target_id: "e0".into(),
            attacker_render_tick: 0.0,
            target_render_tick: 0.0,
            attacker_position: Position { x: 0.0, y: 0.0 },
            target_position: Position { x: 0.0, y: 0.0 },
            visual_distance: 0.0,
            target_generation: None,
        }
    }

    /// Symmetric to the PlayerInput mailbox test: an EatClaim routed via apply_command is
    /// buffered into the pending ring exactly like the inbound claim channel does (both
    /// funnel through the same handler), so the mailbox path doesn't change behaviour.
    #[test]
    fn apply_command_buffers_eat_claim_like_the_channel() {
        let (mut room, _itx, _ctx) = make_room_with_claim();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u1".into(), "Nick".into(), outbound);

        assert_eq!(room.pending_eat_claims.len(), 0);
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: pid.clone(),
            claim: test_claim(1),
        }));
        assert_eq!(room.pending_eat_claims.len(), 1);
        assert_eq!(room.pending_eat_claims[0].claim.claim_id, 1);
        assert_eq!(room.pending_eat_claims[0].player_id, pid);
    }

    /// remove_player must drop the leaving player's claim bookkeeping (last_*_claim_id maps +
    /// pending claim queues) — those are keyed by the per-connection player_id, which is never
    /// reused, so they're pure dead weight after a remove and would otherwise accumulate across a
    /// reconnect-heavy session. Another player's bookkeeping must be left intact. (review Medium)
    #[test]
    fn remove_player_clears_only_its_own_claim_bookkeeping() {
        let (mut room, _itx, _ctx) = make_room_with_claim();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u1".into(), "Nick".into(), outbound);
        let (ob, _r2, _s2) = test_outbound();
        let (other, _) = room.add_player("u2".into(), "Other".into(), ob);

        // Seed per-player claim bookkeeping for BOTH players directly (deterministic — we're
        // testing the cleanup, not the validation paths that populate these).
        room.last_eat_claim_id.insert(pid.clone(), 7);
        room.last_eat_claim_id.insert(other.clone(), 3);
        room.last_enemy_death_claim_id.insert(pid.clone(), 9);
        room.pending_eat_claims.push(EatClaimInput { player_id: pid.clone(), claim: test_claim(1) });
        room.pending_eat_claims.push(EatClaimInput { player_id: other.clone(), claim: test_claim(2) });
        room.pending_enemy_death_claims.push(EnemyDeathClaimInput {
            player_id: pid.clone(),
            claim: death_claim(1, "e0", 0, 0.0, 0.0, OPEN_POS, OPEN_POS, 0.0),
        });

        room.remove_player(&pid);

        // pid's bookkeeping is gone…
        assert!(!room.last_eat_claim_id.contains_key(&pid));
        assert!(!room.last_enemy_death_claim_id.contains_key(&pid));
        assert!(!room.pending_eat_claims.iter().any(|c| c.player_id == pid));
        assert!(!room.pending_enemy_death_claims.iter().any(|c| c.player_id == pid));
        // …but the OTHER player's is untouched.
        assert_eq!(room.last_eat_claim_id.get(&other), Some(&3));
        assert!(room.pending_eat_claims.iter().any(|c| c.player_id == other));
    }

    /// The survived-overlap diagnosis classifier picks the right final-diagnosis event name,
    /// in the gate's order, reserving visual_overlap_without_death for the genuinely-bad residue.
    #[test]
    fn overlap_diagnosis_classification() {
        use Room as R;
        // shield wins even if everything else points to a kill.
        assert_eq!(R::classify_overlap_diagnosis(true, true, true, true, false), "visual_overlap_shielded");
        // not a valid lethal target (respawned/can't eat) → no server contact.
        assert_eq!(
            R::classify_overlap_diagnosis(false, false, true, true, false),
            "visual_overlap_no_server_contact"
        );
        // valid target but server wasn't in kill range at the seen tick.
        assert_eq!(
            R::classify_overlap_diagnosis(false, true, false, true, false),
            "visual_overlap_no_server_contact"
        );
        // server contact but the run was too short.
        assert_eq!(
            R::classify_overlap_diagnosis(false, true, true, false, false),
            "visual_overlap_contact_too_short"
        );
        // long-enough lethal contact but the v4 visible legs blocked it.
        assert_eq!(
            R::classify_overlap_diagnosis(false, true, true, true, true),
            "visual_overlap_projected_blocked"
        );
        // the residue: lethal, long enough, visibly confirmed, not shielded → the real bug.
        assert_eq!(
            R::classify_overlap_diagnosis(false, true, true, true, false),
            "visual_overlap_without_death"
        );
    }

    /// At the pending cap, the INCOMING claim is rejected (claim_pending_full) — the
    /// already-queued claims are never dropped to make room.
    #[test]
    fn eat_claim_pending_cap_rejects_incoming_not_queued() {
        let (mut room, _itx, _ctx) = make_room_with_claim();
        let (outbound, mut reliable_rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u1".into(), "Nick".into(), outbound);

        for i in 0..EAT_CLAIM_PENDING_CAP as u32 {
            room.apply_command(RoomCommand::EatClaim(EatClaimInput {
                player_id: pid.clone(),
                claim: test_claim(i),
            }));
        }
        assert_eq!(room.pending_eat_claims.len(), EAT_CLAIM_PENDING_CAP);
        let oldest_id = room.pending_eat_claims[0].claim.claim_id;

        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: pid.clone(),
            claim: test_claim(9999),
        }));
        assert_eq!(room.pending_eat_claims.len(), EAT_CLAIM_PENDING_CAP, "cap must not be exceeded");
        assert_eq!(room.pending_eat_claims[0].claim.claim_id, oldest_id, "oldest queued claim NOT dropped");
        assert!(
            !room.pending_eat_claims.iter().any(|c| c.claim.claim_id == 9999),
            "over-cap claim must not be queued"
        );

        // The attacker is told its claim was rejected, with the documented reason.
        let mut saw_reject = false;
        while let Ok(msg) = reliable_rx.try_recv() {
            if let ServerMessage::EatClaimRejected { claim_id, reason, .. } = msg {
                if claim_id == 9999 && reason == "claim_pending_full" {
                    saw_reject = true;
                }
            }
        }
        assert!(saw_reject, "over-cap claim must get EatClaimRejected claim_pending_full");
    }

    /// round-3 follow-up (anti-spam on the reliable death-claim channel): one player can't
    /// monopolise the shared pending ring. At the per-player cap, further death claims are rejected
    /// (too_many_pending_death_claims) without dropping the queued ones — and ANOTHER player's claim
    /// is unaffected.
    #[test]
    fn death_claim_per_player_pending_cap_rejects_spam() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (spammer, _) = room.add_player("us".into(), "S".into(), out);
        let (ob, mut rxb, _s2) = test_outbound();
        let (other, _) = room.add_player("uo".into(), "O".into(), ob);
        let ft = (room.tick + 5) as f64; // future render tick → claims STAY pending (we don't resolve)
        let spam = |id: u32| EnemyDeathClaimInput {
            player_id: spammer.clone(),
            claim: death_claim(id, "e0", 0, ft, ft, OPEN_POS, OPEN_POS, 0.0),
        };

        // Fill the spammer's per-player budget.
        for i in 0..MAX_PENDING_DEATH_CLAIMS_PER_PLAYER as u32 {
            room.apply_command(RoomCommand::EnemyDeathClaim(spam(i + 1)));
        }
        let mine =
            |room: &Room| room.pending_enemy_death_claims.iter().filter(|c| c.player_id == spammer).count();
        assert_eq!(mine(&room), MAX_PENDING_DEATH_CLAIMS_PER_PLAYER, "budget filled");
        let _ = drain_death_rejects(&mut rx);

        // One more from the spammer → rejected, queue unchanged (no queued claim dropped).
        room.apply_command(RoomCommand::EnemyDeathClaim(spam(9999)));
        assert!(
            drain_death_rejects(&mut rx)
                .iter()
                .any(|(id, r)| *id == 9999 && r == "too_many_pending_death_claims"),
            "over-cap claim rejected with the documented reason"
        );
        assert_eq!(mine(&room), MAX_PENDING_DEATH_CLAIMS_PER_PLAYER, "queue unchanged at the cap");
        assert!(
            !room.pending_enemy_death_claims.iter().any(|c| c.claim.claim_id == 9999),
            "over-cap claim not queued"
        );

        // A DIFFERENT player is unaffected by the spammer's cap.
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: other.clone(),
            claim: death_claim(1, "e0", 0, ft, ft, OPEN_POS, OPEN_POS, 0.0),
        }));
        assert_eq!(
            room.pending_enemy_death_claims.iter().filter(|c| c.player_id == other).count(),
            1,
            "another player's claim still queues"
        );
        assert!(drain_death_rejects(&mut rxb).is_empty(), "no reject for the unaffected player");
    }

    /// Projection invariant (no websocket): a periodic full and delta describe the SAME
    /// tick, both carry last_event_id, both carry the per-room snapshot_seq, and the
    /// last_processed_input_seq is whatever the CALLER passes for THAT recipient (the
    /// broadcast loop passes each recipient's own seq), not a shared value.
    #[test]
    fn full_and_delta_share_tick_event_id_and_recipient_seq() {
        let (mut room, _itx, _ctx) = make_room_with_claim();
        let (o1, _r1, _s1) = test_outbound();
        let (_p1, _) = room.add_player("u1".into(), "A".into(), o1);
        let (o2, _r2, _s2) = test_outbound();
        let (_p2, _) = room.add_player("u2".into(), "B".into(), o2);

        room.update(); // advance off tick 0
        let tick = room.tick;

        let full = room.project_full_keyframe(Some(7), 5, 1234);
        let delta = room.project_delta(Some(9), 6, 1234);

        assert_eq!(full.tick, tick);
        assert_eq!(delta.tick, tick);
        assert_eq!(full.last_event_id, room.last_event_id());
        assert_eq!(delta.last_event_id, room.last_event_id());
        assert_eq!(full.snapshot_seq, 5);
        assert_eq!(delta.snapshot_seq, 6);
        // Recipient-specific seq — distinct values prove the projector threads the
        // caller's per-recipient ack, not a room-global one.
        assert_eq!(full.last_processed_input_seq, Some(7));
        assert_eq!(delta.last_processed_input_seq, Some(9));
        assert_eq!(full.players.len(), 2);
        assert_eq!(delta.players.len(), 2);
    }

    /// Stage 5.5: event ids are gap-free and monotonic; last_event_id grows
    /// as the room broadcasts events. None until the first event fires.
    #[test]
    fn event_ids_are_monotonic_and_gapless() {
        let mut room = make_room(11);
        assert_eq!(room.last_event_id(), None);
        let a = room.alloc_event_id();
        let b = room.alloc_event_id();
        let c = room.alloc_event_id();
        assert_eq!((a, b, c), (1, 2, 3));
        assert_eq!(room.last_event_id(), Some(3));
    }

    /// Stage 5.5: state_patch_for returns the player's authoritative state at
    /// the moment a hard event fires, so the client can detect "already-applied"
    /// late events and skip redundant visual snaps.
    #[test]
    fn state_patch_reflects_current_player_state() {
        let (mut room, _input_tx) = make_room_with_input();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), outbound);
        let player_pos = room.players[&pid].position;
        let patch = room.state_patch_for(&pid).expect("patch exists");
        assert_eq!(patch.position.x, player_pos.x);
        assert_eq!(patch.position.y, player_pos.y);
    }

    /// Stage 2 acceptance: MoveCommand seq is monotonic; replays/older seqs are ignored.
    #[test]
    fn move_command_seq_monotonic_and_replay_ignored() {
        let (mut room, input_tx) = make_room_with_input();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u1".into(), "Nick".into(), outbound);

        // seq=5 with Up — accepted, last_processed becomes 5.
        input_tx
            .try_send(PlayerInput {
                player_id: pid.clone(),
                seq: 5,
                target_tick: 0,
                direction: Direction::Up,
            })
            .unwrap();
        room.update();
        assert_eq!(room.players[&pid].last_processed_input_seq, 5);
        assert_eq!(room.players[&pid].desired_direction, Direction::Up);

        // Replay of seq=5 (e.g. retransmit) — ignored, direction unchanged.
        input_tx
            .try_send(PlayerInput {
                player_id: pid.clone(),
                seq: 5,
                target_tick: 0,
                direction: Direction::Down,
            })
            .unwrap();
        // Older seq=3 — ignored.
        input_tx
            .try_send(PlayerInput {
                player_id: pid.clone(),
                seq: 3,
                target_tick: 0,
                direction: Direction::Left,
            })
            .unwrap();
        room.update();
        assert_eq!(room.players[&pid].last_processed_input_seq, 5);
        assert_eq!(room.players[&pid].desired_direction, Direction::Up);

        // Higher seq=6 — accepted.
        input_tx
            .try_send(PlayerInput {
                player_id: pid.clone(),
                seq: 6,
                target_tick: 0,
                direction: Direction::Right,
            })
            .unwrap();
        room.update();
        assert_eq!(room.players[&pid].last_processed_input_seq, 6);
        assert_eq!(room.players[&pid].desired_direction, Direction::Right);
    }

    /// Input buffer applies by target_tick: only inputs whose target_tick has
    /// arrived are returned, in (target_tick, seq) order; the rest stay pending.
    fn inp(seq: u32, target_tick: u64, direction: Direction) -> PlayerInput {
        PlayerInput { player_id: "p".into(), seq, target_tick, direction }
    }

    #[test]
    fn input_buffer_applies_by_target_tick() {
        let mut slot = PlayerInputSlot::default();
        slot.submit(inp(1, 10, Direction::Up), 0);
        slot.submit(inp(2, 5, Direction::Down), 0);
        slot.submit(inp(3, 12, Direction::Left), 0);

        // At tick 10: target_tick 5 and 10 are due (in target_tick order), 12 stays.
        let due = slot.take_due(10);
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].seq, 2); // target_tick 5 first
        assert_eq!(due[1].seq, 1); // then target_tick 10
        assert_eq!(slot.pending.len(), 1);

        // Future input survives take_due until its tick.
        let due = slot.take_due(12);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].seq, 3);
        assert!(slot.pending.is_empty());
    }

    #[test]
    fn retransmit_same_seq_does_not_duplicate() {
        let mut slot = PlayerInputSlot::default();
        assert_eq!(slot.submit(inp(7, 110, Direction::Up), 100), SubmitResult::Accepted);
        for _ in 0..9 {
            assert_eq!(slot.submit(inp(7, 110, Direction::Up), 100), SubmitResult::ReplacedRetransmit);
        }
        assert_eq!(slot.pending.len(), 1, "retransmits must not grow the buffer");
        assert_eq!(slot.pending[0].seq, 7);
    }

    #[test]
    fn rejects_absurd_future_and_too_late() {
        let mut slot = PlayerInputSlot::default();
        assert_eq!(slot.submit(inp(1, 100_000, Direction::Up), 100), SubmitResult::RejectedFuture);
        assert_eq!(slot.submit(inp(2, 5, Direction::Up), 100), SubmitResult::RejectedTooLate);
        assert!(slot.pending.is_empty(), "rejected inputs must not be buffered");
    }

    /// Input applied at exactly target_tick, not before.
    #[test]
    fn input_applies_exactly_at_target_tick() {
        let (mut room, input_tx) = make_room_with_input();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), outbound);
        let start = room.tick;
        input_tx
            .try_send(PlayerInput {
                player_id: pid.clone(),
                seq: 1,
                target_tick: start + 3,
                direction: Direction::Down,
            })
            .unwrap();
        room.update(); // tick start+1 — not yet
        assert_eq!(room.players[&pid].desired_direction, Direction::Right);
        room.update(); // tick start+2 — not yet
        assert_eq!(room.players[&pid].desired_direction, Direction::Right);
        room.update(); // tick start+3 — applied
        assert_eq!(room.players[&pid].desired_direction, Direction::Down);
    }

    /// Stage 6.5: intent older than 12 ticks is ignored even if walkable.
    #[test]
    fn intent_older_than_max_age_is_ignored() {
        let (mut room, _input_tx) = make_room_with_input();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), outbound);
        // Move the player to a known cell with an obvious walkable Right
        // (the corridor at y=437.5 is open).
        if let Some(p) = room.players.get_mut(&pid) {
            p.position = Position { x: 37.5, y: 437.5 };
            p.direction = crate::protocol::Direction::Right;
            p.desired_direction = crate::protocol::Direction::Right;
            p.push_intent(
                crate::protocol::Direction::Down,
                0, // client_tick
                0, // received_at_server_tick — long ago
                1, // seq
            );
        }
        // Even though Down might be walkable, intent is far older than 12 ticks.
        let pick = room.players[&pid].find_applicable_turn_intent(&room.map, 100);
        assert!(pick.is_none(), "stale intent must be ignored");
    }

    /// Stage 6 input flood: 1000 MoveCommands across 10 ticks must NOT make
    /// the bounded channel or per-player slot grow unboundedly. Replay-seq
    /// guard still wins; `last_processed_input_seq` ends at the max seq sent.
    #[test]
    fn input_flood_does_not_grow_queue() {
        let (mut room, input_tx) = make_room_with_input();
        let (outbound, _r, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), outbound);

        // Push 1000 inputs in seq order. With bounded channel capacity 256,
        // some try_send calls return Full — drops are EXPECTED and proven
        // here. Without bounding, this would queue 1000 messages.
        let mut sent = 0u32;
        for seq in 1..=1000u32 {
            if input_tx
                .try_send(PlayerInput {
                    player_id: pid.clone(),
                    seq,
                    target_tick: 0,
                    direction: Direction::Up,
                })
                .is_ok()
            {
                sent += 1;
            }
            // Drain occasionally to model the game loop catching up.
            if seq % 64 == 0 {
                room.update();
            }
        }
        // Final drain.
        for _ in 0..16 {
            room.update();
        }

        // last_processed_input_seq is the max seq that actually arrived,
        // not necessarily 1000. The point: bounded sends limit memory.
        let last = room.players[&pid].last_processed_input_seq;
        assert!(last > 0, "at least one input must have applied");
        assert!(sent <= 1000);
        // Coalesce + drop counters are populated.
        assert!(room.input_coalesced_count <= sent as u64);
    }

    // ---- P0 speed/state authority + full-state resync ----------------------

    fn drain_events(rx: &mut mpsc::Receiver<ServerMessage>) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ServerMessage::Event(ev) = msg {
                out.push(ev);
            }
        }
        out
    }

    /// Boost is collected AFTER movement (check_item_collection runs post
    /// player.update), so the boosted speed must take effect the NEXT tick — else
    /// the client re-simulates the current tick on boosted speed and the server
    /// does not. effective_tick = collection_tick + 1.
    #[test]
    fn boost_started_speed_applies_next_tick() {
        let (mut room, _input_tx) = make_room_with_input();
        let (outbound, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), outbound);
        let _ = drain_events(&mut rx); // discard PlayerJoined

        // Drop a booster on the player so the next update collects it.
        let pos = room.players[&pid].position;
        room.boosters.push(Booster { id: "b1".into(), position: pos, booster_type: BoosterType::Mushroom });

        room.update();
        let current_tick = room.tick; // tick just simulated
        let evs = drain_events(&mut rx);
        let (server_tick, effective_tick, speed) =
            evs.iter()
                .find_map(|e| match e {
                    ServerEvent::PlayerSpeedChanged {
                        reason, server_tick, effective_tick, speed, ..
                    } if reason == "boost_started" => Some((*server_tick, *effective_tick, *speed)),
                    _ => None,
                })
                .expect("boost_started event emitted on collection tick");

        assert_eq!(server_tick, current_tick, "observed at the collection tick");
        assert_eq!(effective_tick, current_tick + 1, "boost effective the tick AFTER collection");
        assert_eq!(speed, room.config.gameplay.boosted_speed);
    }

    /// Boost expiry runs in update_timers() BEFORE movement, so the expiring tick
    /// already moved on base speed → the change is effective on the SAME tick it is
    /// observed. effective_tick = server_tick (not +1).
    #[test]
    fn boost_expired_effective_tick_matches_update_order() {
        let (mut room, _input_tx) = make_room_with_input();
        let (outbound, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), outbound);

        // Boosted state that expires soon (avoid running the full duration).
        let expire_at = room.tick + 2;
        {
            let p = room.players.get_mut(&pid).unwrap();
            p.is_invincible = true;
            p.speed = room.config.gameplay.boosted_speed;
            p.invincibility_end_tick = Some(expire_at);
            p.boosted_until_tick = Some(expire_at);
        }
        let _ = drain_events(&mut rx);

        let mut found = None;
        for _ in 0..6 {
            room.update();
            for e in drain_events(&mut rx) {
                if let ServerEvent::PlayerSpeedChanged {
                    reason, server_tick, effective_tick, speed, ..
                } = &e
                {
                    if reason == "boost_expired" {
                        found = Some((*server_tick, *effective_tick, *speed));
                    }
                }
            }
            if found.is_some() {
                break;
            }
        }
        let (server_tick, effective_tick, speed) = found.expect("boost_expired event emitted");
        assert_eq!(effective_tick, server_tick, "expiry effective on the observed tick");
        assert_eq!(effective_tick, expire_at, "expiry fires exactly at boosted_until_tick");
        assert_eq!(speed, room.config.gameplay.base_speed);
    }

    /// RequestFullState's answer is a full keyframe of the CURRENT room state.
    #[test]
    fn request_full_state_returns_current_full_keyframe() {
        let (mut room, _input_tx) = make_room_with_input();
        let (outbound, _rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), outbound);
        for _ in 0..10 {
            room.update();
        }

        match room.build_full_state() {
            ServerMessage::GameState(u) => {
                assert_eq!(u.tick, room.tick, "keyframe carries the room's current tick");
                assert!(u.players.iter().any(|p| p.id == pid), "keyframe includes the player");
                assert_eq!(u.enemies.len(), room.enemies.len(), "keyframe includes all enemies");
            }
            _ => panic!("expected GameState keyframe"),
        }
    }

    /// The resync keyframe must carry last_event_id so the client can fold away
    /// events already included in it (idempotent buffered-event replay).
    #[test]
    fn request_full_state_keyframe_has_last_event_id() {
        let (mut room, _input_tx) = make_room_with_input();
        let (outbound, _rx, _s) = test_outbound();
        let (_pid, _) = room.add_player("u".into(), "n".into(), outbound);

        let _ = room.alloc_event_id();
        let _ = room.alloc_event_id();
        let expected = room.last_event_id();
        assert!(expected.is_some(), "events were allocated");

        match room.build_full_state() {
            ServerMessage::GameState(u) => {
                assert_eq!(u.last_event_id, expected, "keyframe carries last_event_id");
            }
            _ => panic!("expected GameState keyframe"),
        }
    }

    // ---- enemy collision: safe zone + forgiving radius + order independence -----

    const SAFE_POS: Position = Position { x: 250.0, y: 500.0 }; // safe-zone centre (config)
    const OPEN_POS: Position = Position { x: 50.0, y: 50.0 }; // far outside the safe zone

    /// Park every enemy far away, then place enemy[0] at `enemy_pos` with `score`.
    fn isolate_enemy(room: &mut Room, enemy_pos: Position, score: u32) {
        for e in room.enemies.iter_mut() {
            e.position = Position { x: 480.0, y: 980.0 };
        }
        room.enemies[0].position = enemy_pos;
        room.enemies[0].score = score;
    }

    fn place_player(room: &mut Room, pid: &str, pos: Position, score: u32, invincible: bool) {
        let p = room.players.get_mut(pid).unwrap();
        p.position = pos;
        p.score = score;
        p.is_invincible = invincible;
        p.is_alive = true;
        // Claim scenario tests exercise the VALIDATION rules; the readiness admission
        // gates have their own dedicated test (death_claim_rejected_until_claim_ready).
        // Min-claim-age doesn't apply here: these admissions are original room members
        // (admitted_at_tick == 0).
        p.world_ready = true;
        p.claim_ready = true;
    }

    fn ate_enemy(events: &[ServerEvent], eater: &str) -> bool {
        events.iter().any(|e| {
            matches!(e,
            ServerEvent::EnemyRespawned { caused_by_player_id: Some(by), .. } if by == eater)
        })
    }
    fn player_died(events: &[ServerEvent], who: &str) -> bool {
        events.iter().any(|e| {
            matches!(e,
            ServerEvent::PlayerRespawned { player_id, .. } if player_id == who)
        })
    }

    // ---- player-initiated eating (EatClaim) --------------------------------

    // Positional test builder — the EatClaim wire shape genuinely has this many fields.
    #[allow(clippy::too_many_arguments)]
    fn eat_claim(
        claim_id: u32,
        kind: EatTargetKind,
        target_id: &str,
        atk_tick: f64,
        tgt_tick: f64,
        atk_pos: Position,
        tgt_pos: Position,
        dist: f32,
    ) -> EatClaim {
        EatClaim {
            claim_id,
            target_kind: kind,
            target_id: target_id.to_string(),
            attacker_render_tick: atk_tick,
            target_render_tick: tgt_tick,
            attacker_position: atk_pos,
            target_position: tgt_pos,
            visual_distance: dist,
            target_generation: None,
        }
    }
    fn park_enemies_far(room: &mut Room) {
        for e in room.enemies.iter_mut() {
            e.position = Position { x: 480.0, y: 980.0 };
            e.speed = 0.0;
        }
    }
    fn freeze_unprotected(room: &mut Room, pid: &str) {
        let p = room.players.get_mut(pid).unwrap();
        p.speed = 0.0;
        p.spawn_protected_until_tick = None;
    }
    fn dist2(a: Position, b: Position) -> f32 {
        ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
    }
    fn drain_rejects(rx: &mut mpsc::Receiver<ServerMessage>) -> Vec<(u32, String)> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ServerMessage::EatClaimRejected { claim_id, reason, .. } = msg {
                out.push((claim_id, reason));
            }
        }
        out
    }
    const OPEN_A: Position = Position { x: 50.0, y: 50.0 };
    const OPEN_B: Position = Position { x: 55.0, y: 50.0 }; // 5px from OPEN_A, open

    /// 1. A current-tick PvP overlap must NOT kill on its own — only an EatClaim can.
    #[test]
    fn pvp_without_claim_does_not_kill() {
        let (mut room, _i, _claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (ob, mut rxb, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        let (b, _) = room.add_player("ub".into(), "B".into(), ob);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 100, false);
        place_player(&mut room, &b, OPEN_B, 10, false);
        freeze_unprotected(&mut room, &a);
        freeze_unprotected(&mut room, &b);
        let _ = drain_events(&mut rxa);
        let _ = drain_events(&mut rxb);
        for _ in 0..5 {
            room.update();
        }
        let mut evs = drain_events(&mut rxa);
        evs.extend(drain_events(&mut rxb));
        assert!(
            !player_died(&evs, &a) && !player_died(&evs, &b),
            "current-tick PvP overlap must NOT kill without an EatClaim"
        );
    }

    /// 2. A valid PvP EatClaim kills the victim and transfers score.
    #[test]
    fn pvp_claim_kills_and_scores() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (ob, mut rxb, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        let (b, _) = room.add_player("ub".into(), "B".into(), ob);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 100, false);
        place_player(&mut room, &b, OPEN_B, 10, false);
        freeze_unprotected(&mut room, &a);
        freeze_unprotected(&mut room, &b);
        for _ in 0..5 {
            room.update();
        }
        let _ = drain_events(&mut rxa);
        let _ = drain_events(&mut rxb);
        let before = room.players[&a].score;
        let t = room.tick as f64;
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(1, EatTargetKind::Player, &b, t, t, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
            })
            .unwrap();
        room.update();
        let mut evs = drain_events(&mut rxa);
        evs.extend(drain_events(&mut rxb));
        assert!(
            evs.iter().any(|e| matches!(e,
            ServerEvent::PlayerRespawned { player_id, killer_player_id: Some(k), .. }
                if player_id == &b && k == &a)),
            "victim respawned with killer_player_id = attacker"
        );
        assert!(room.players[&a].score > before, "attacker gained the victim's score");
    }

    /// 3. PvP claim is rejected when either party is in the safe zone.
    #[test]
    fn pvp_claim_rejected_in_safe_zone() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (ob, _rxb, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        let (b, _) = room.add_player("ub".into(), "B".into(), ob);
        park_enemies_far(&mut room);
        let vic = Position { x: SAFE_POS.x + 5.0, y: SAFE_POS.y };
        place_player(&mut room, &a, SAFE_POS, 100, false);
        place_player(&mut room, &b, vic, 10, false);
        freeze_unprotected(&mut room, &a);
        freeze_unprotected(&mut room, &b);
        for _ in 0..5 {
            room.update();
        }
        let _ = drain_rejects(&mut rxa);
        let t = room.tick as f64;
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(1, EatTargetKind::Player, &b, t, t, SAFE_POS, vic, dist2(SAFE_POS, vic)),
            })
            .unwrap();
        room.update();
        assert!(
            drain_rejects(&mut rxa).iter().any(|(_, r)| r == "safe_zone"),
            "claim rejected with reason=safe_zone"
        );
        assert_eq!(room.players[&a].score, 100, "no score for a rejected eat");
    }

    /// round-3 follow-up (Blocker 1, player-eat side): a stale PvP eat claim against a victim that
    /// has since respawned is rejected by life_id — symmetric with the enemy-death path. The history
    /// frame is the victim's previous life (life 0); the live victim (life 1, protection already
    /// gone) must not be eaten by a claim that only ever saw the old life.
    #[test]
    fn pvp_claim_against_respawned_victim_rejected_by_life_id() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (ob, _rxb, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        let (b, _) = room.add_player("ub".into(), "B".into(), ob);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 100, false);
        place_player(&mut room, &b, OPEN_B, 10, false);
        freeze_unprotected(&mut room, &a);
        freeze_unprotected(&mut room, &b);
        for t in 10..=11 {
            room.tick = t;
            room.record_contact_history();
        }
        // Victim respawned into a new life (freeze_unprotected already cleared protection); the
        // history frame at tick 11 is still life 0.
        room.players.get_mut(&b).unwrap().life_id = 1;
        let _ = drain_rejects(&mut rxa);

        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: a.clone(),
            claim: eat_claim(1, EatTargetKind::Player, &b, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();

        assert!(
            drain_rejects(&mut rxa).iter().any(|(_, r)| r == "victim_life_mismatch"),
            "stale PvP eat against a since-respawned life rejected by life_id"
        );
        assert!(room.players[&b].is_alive, "the new life was not eaten");
    }

    /// 4. An enemy EatClaim is accepted across timelines: the attacker NOW vs the
    ///    enemy as it was rendered earlier — even though same-tick they don't overlap.
    #[test]
    fn enemy_claim_accepted_across_timeline() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 500, false);
        freeze_unprotected(&mut room, &a);
        let enemy_id = room.enemies[0].id.clone();
        room.enemies[0].score = 1;
        // Build history by hand (the AI grid-snaps, so don't drive update()):
        // tick 10 the enemy is CLOSE to the stationary attacker, tick 11 it's FAR.
        room.tick = 10;
        room.enemies[0].position = OPEN_B;
        room.record_contact_history();
        room.tick = 11;
        room.enemies[0].position = Position { x: 480.0, y: 980.0 };
        room.record_contact_history();
        let _ = drain_events(&mut rxa);
        let before = room.players[&a].score;
        // Attacker NOW (tick 11) vs the enemy as it was rendered at tick 10 — overlap
        // on the attacker's view even though same-tick they're far apart.
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(
                    1,
                    EatTargetKind::Enemy,
                    &enemy_id,
                    11.0,
                    10.0,
                    OPEN_A,
                    OPEN_B,
                    dist2(OPEN_A, OPEN_B),
                ),
            })
            .unwrap();
        room.process_claims();
        let evs = drain_events(&mut rxa);
        assert!(ate_enemy(&evs, &a), "enemy eaten via mixed-timeline claim");
        assert!(room.players[&a].score > before, "attacker scored the enemy");
    }

    /// review (Blocker): a FRACTIONAL eat claim whose `ceil(attacker_render_tick)` the sim hasn't
    /// recorded yet must be HELD, never rejected. ContactHistory refuses to sample a fractional tick
    /// without its upper frame, so resolving at floor would reject the honest claim as
    /// no_attacker_history. The resolver gates on ceil(render_tick); once that frame is recorded the
    /// claim resolves on real merits.
    #[test]
    fn fractional_eat_claim_is_held_until_ceil_frame_exists() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 500, false);
        freeze_unprotected(&mut room, &a);
        let enemy_id = room.enemies[0].id.clone();
        room.enemies[0].score = 1;
        for t in [10u64, 11] {
            room.tick = t;
            room.enemies[0].position = OPEN_B;
            room.record_contact_history();
        }
        let _ = drain_rejects(&mut rxa);

        // attacker_render_tick = 11.5 → ceil = 12, but the sim is still at tick 11 → HELD.
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(
                    1,
                    EatTargetKind::Enemy,
                    &enemy_id,
                    11.5,
                    11.0,
                    OPEN_A,
                    OPEN_B,
                    dist2(OPEN_A, OPEN_B),
                ),
            })
            .unwrap();
        room.process_claims();
        // Held ⟺ still pending (a resolved claim — accept OR reject — leaves the pending ring).
        assert_eq!(room.pending_eat_claims.len(), 1, "fractional claim HELD, not resolved at floor");
        assert!(drain_rejects(&mut rxa).is_empty(), "specifically NOT rejected as no_attacker_history");

        // Record frame 12: now ceil(11.5)=12 <= current tick → ready → resolves on its merits.
        room.tick = 12;
        room.enemies[0].position = OPEN_B;
        room.record_contact_history();
        room.process_claims();
        assert_eq!(room.pending_eat_claims.len(), 0, "resolved once the ceil frame exists");
        assert!(
            ate_enemy(&drain_events(&mut rxa), &a),
            "accepted on real merits, not lost to missing history"
        );
    }

    /// review nit: a negative (or non-finite) render tick is an INVALID client timeline — rejected
    /// explicitly as invalid_render_tick at readiness, not coerced to tick 0 and treated as long-past.
    #[test]
    fn eat_claim_with_negative_render_tick_rejected_invalid() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 500, false);
        freeze_unprotected(&mut room, &a);
        let enemy_id = room.enemies[0].id.clone();
        room.tick = 11;
        room.record_contact_history();
        let _ = drain_rejects(&mut rxa);
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(
                    1,
                    EatTargetKind::Enemy,
                    &enemy_id,
                    -0.5,
                    -1.0,
                    OPEN_A,
                    OPEN_B,
                    dist2(OPEN_A, OPEN_B),
                ),
            })
            .unwrap();
        room.process_claims();
        assert_eq!(room.pending_eat_claims.len(), 0, "invalid claim is rejected, not held");
        assert!(
            drain_rejects(&mut rxa).iter().any(|(_, r)| r == "invalid_render_tick"),
            "negative render tick rejected explicitly as invalid_render_tick"
        );
    }

    /// Symmetric to the eat case: a FRACTIONAL enemy-death claim is HELD until ceil(victim_render_tick)
    /// is recorded, not rejected as no_victim_history. (lead review — fractional claim gating)
    #[test]
    fn fractional_enemy_death_claim_is_held_until_ceil_frame_exists() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        park_enemies_far(&mut room);
        place_player(&mut room, &vic, OPEN_A, 10, false); // weak victim
        freeze_unprotected(&mut room, &vic);
        let enemy_id = room.enemies[0].id.clone();
        room.enemies[0].score = 100;
        room.enemies[0].generation = 0;
        room.enemies[0].respawned_at_tick = 0;
        for t in [10u64, 11] {
            room.tick = t;
            room.enemies[0].position = OPEN_B;
            room.record_contact_history();
        }
        let _ = drain_events(&mut rx);

        // victim_render_tick = 11.5 → ceil = 12, sim at tick 11 → HELD.
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.5, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        assert_eq!(
            room.pending_enemy_death_claims.len(),
            1,
            "fractional death claim HELD, not resolved at floor"
        );
        assert!(drain_death_rejects(&mut rx).is_empty(), "specifically NOT rejected as no_victim_history");

        room.tick = 12;
        room.enemies[0].position = OPEN_B;
        room.record_contact_history();
        room.process_claims();
        assert_eq!(room.pending_enemy_death_claims.len(), 0, "resolved once the ceil frame exists");
        assert!(
            player_died(&drain_events(&mut rx), &vic),
            "confirmed on real merits once history is present"
        );
    }

    /// 5. An enemy claim is rejected if the enemy already respawned after the
    ///    claimed target tick.
    #[test]
    fn enemy_claim_rejected_if_already_respawned() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 500, false);
        freeze_unprotected(&mut room, &a);
        let enemy_id = room.enemies[0].id.clone();
        room.enemies[0].score = 1;
        room.tick = 10;
        room.enemies[0].position = OPEN_B;
        room.record_contact_history();
        room.tick = 11;
        room.record_contact_history();
        // The enemy respawned at tick 11 — AFTER the claim's target tick (10).
        room.enemies[0].respawned_at_tick = 11;
        let _ = drain_rejects(&mut rxa);
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(
                    1,
                    EatTargetKind::Enemy,
                    &enemy_id,
                    11.0,
                    10.0,
                    OPEN_A,
                    OPEN_B,
                    dist2(OPEN_A, OPEN_B),
                ),
            })
            .unwrap();
        room.process_claims();
        assert!(
            drain_rejects(&mut rxa).iter().any(|(_, r)| r == "enemy_already_respawned"),
            "claim rejected: enemy already respawned"
        );
    }

    /// 6. A re-used (or stale) claim_id is rejected.
    #[test]
    fn duplicate_claim_id_rejected() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (oa, mut rxa, _) = test_outbound();
        let (ob, _rxb, _) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        let (b, _) = room.add_player("ub".into(), "B".into(), ob);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 100, false);
        place_player(&mut room, &b, OPEN_B, 10, false);
        freeze_unprotected(&mut room, &a);
        freeze_unprotected(&mut room, &b);
        for _ in 0..5 {
            room.update();
        }
        let _ = drain_rejects(&mut rxa);
        let t = room.tick as f64;
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(5, EatTargetKind::Player, &b, t, t, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
            })
            .unwrap();
        room.update(); // id=5 accepted
        let _ = drain_rejects(&mut rxa);
        let t2 = room.tick as f64;
        claim_tx
            .try_send(EatClaimInput {
                player_id: a.clone(),
                claim: eat_claim(5, EatTargetKind::Player, &b, t2, t2, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
            })
            .unwrap();
        room.update(); // id=5 again → duplicate
        assert!(
            drain_rejects(&mut rxa).iter().any(|(id, r)| *id == 5 && r == "duplicate_or_old_claim_id"),
            "re-used claim_id rejected"
        );
    }

    /// Safe zone shields from death but NOT from offense: an invincible player
    /// standing in the safe zone still eats an enemy it overlaps.
    #[test]
    fn safe_zone_invincible_player_still_eats_enemy() {
        let (mut room, _i, claim_tx) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, SAFE_POS, 10, true); // invincible
        freeze_unprotected(&mut room, &pid);
        let enemy_id = room.enemies[0].id.clone();
        let near = Position { x: SAFE_POS.x + 5.0, y: SAFE_POS.y };
        room.enemies[0].score = 1;
        room.tick = 10;
        room.enemies[0].position = near;
        room.record_contact_history();
        room.tick = 11;
        room.record_contact_history();
        let _ = drain_events(&mut rx);
        let before = room.players[&pid].score;
        // Enemy claims aren't safe-zone-gated (only player-vs-player is), so the
        // invincible safe-zone player still eats the enemy via EatClaim.
        claim_tx
            .try_send(EatClaimInput {
                player_id: pid.clone(),
                claim: eat_claim(
                    1,
                    EatTargetKind::Enemy,
                    &enemy_id,
                    11.0,
                    10.0,
                    SAFE_POS,
                    near,
                    dist2(SAFE_POS, near),
                ),
            })
            .unwrap();
        room.process_claims();
        let evs = drain_events(&mut rx);
        assert!(room.players[&pid].score > before, "eating scored");
        assert!(ate_enemy(&evs, &pid), "EnemyRespawned attributed to us");
        assert!(!player_died(&evs, &pid), "we did not die");
    }

    /// Safe zone shields a weak player from being eaten (no respawn fired).
    #[test]
    fn safe_zone_shields_weak_player_from_death() {
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, SAFE_POS, 0, false); // weak, killable
        isolate_enemy(&mut room, SAFE_POS, 5); // enemy can_eat
        let _ = drain_events(&mut rx);

        room.check_enemy_collisions();
        let evs = drain_events(&mut rx);
        assert!(!player_died(&evs, &pid), "safe zone shields from death");
    }

    /// The same weak player OUTSIDE a safe zone dies at the strict radius.
    #[test]
    fn weak_player_outside_safe_zone_dies() {
        let (mut room, _i) = make_room_with_input_server_death();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        isolate_enemy(&mut room, OPEN_POS, 5); // dist 0 < death radius
        seed_confirming_enemy_history(&mut room); // visible timeline confirms the overlap
        let _ = drain_events(&mut rx);

        // Victim-favored: a kill needs persistent contact, so step the required ticks.
        for _ in 0..CONTACT_TICKS_REQUIRED {
            room.check_enemy_collisions();
        }
        let evs = drain_events(&mut rx);
        assert!(player_died(&evs, &pid), "weak player outside safe zone dies after persistent contact");
    }

    /// A freshly respawned player is spawn-protected: even a stronger enemy
    /// overlapping it outside any safe zone can't eat it during the window.
    #[test]
    fn spawn_protection_shields_freshly_respawned_player_from_death() {
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        // Respawn THIS tick → protected for SPAWN_PROTECTION_SEC.
        let tick = room.tick;
        let tr = room.tick_rate();
        room.players.get_mut(&pid).unwrap().respawn(OPEN_POS, tick, tr);
        // re-place so we're weak (score 0) and overlapping the enemy.
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        isolate_enemy(&mut room, OPEN_POS, 5); // enemy can_eat, dist 0 < strict 20
        assert!(room.players[&pid].is_spawn_protected(room.tick), "protected at respawn");
        let _ = drain_events(&mut rx);

        room.check_enemy_collisions();
        let evs = drain_events(&mut rx);
        assert!(!player_died(&evs, &pid), "spawn protection shields from death");
    }

    /// Once the protection window elapses, the same weak player dies normally.
    #[test]
    fn spawn_protection_expires_then_weak_player_dies() {
        let (mut room, _i) = make_room_with_input_server_death();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        let tick = room.tick;
        let tr = room.tick_rate();
        room.players.get_mut(&pid).unwrap().respawn(OPEN_POS, tick, tr);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        isolate_enemy(&mut room, OPEN_POS, 5);
        // Jump just past the protection window.
        room.tick = tick + (room.config.gameplay.spawn_protection_sec * tr as f32) as u64 + 1;
        assert!(!room.players[&pid].is_spawn_protected(room.tick), "window elapsed");
        seed_confirming_enemy_history(&mut room); // visible timeline confirms the overlap
        assert!(!room.players[&pid].is_spawn_protected(room.tick), "still unprotected after warmup");
        let _ = drain_events(&mut rx);

        for _ in 0..CONTACT_TICKS_REQUIRED {
            room.check_enemy_collisions();
        }
        let evs = drain_events(&mut rx);
        assert!(player_died(&evs, &pid), "after protection expires, weak player dies");
    }

    /// The strict enemy-eats-player death radius has NO +6 grace (that grace only
    /// ever applied to player-eats-enemy, which is an EatClaim now): a weak player
    /// just beyond the strict radius does NOT die.
    #[test]
    fn death_radius_stays_strict() {
        let cd = {
            let (room, _i) = make_room_with_input();
            room.config.collision.collision_distance_px
        };
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        isolate_enemy(&mut room, Position { x: OPEN_POS.x + cd + 3.0, y: OPEN_POS.y }, 5);
        let _ = drain_events(&mut rx);
        room.check_enemy_collisions();
        let evs = drain_events(&mut rx);
        assert!(!player_died(&evs, &pid), "death radius stays strict (no +6 grace)");
    }

    /// Extract the death-fairness payload from a player's PlayerRespawned event.
    fn respawn_death(
        events: &[ServerEvent],
        who: &str,
    ) -> Option<(Option<String>, Option<f32>, Option<u32>)> {
        events.iter().find_map(|e| match e {
            ServerEvent::PlayerRespawned {
                player_id, killer_enemy_id, server_dist, contact_ticks, ..
            } if player_id == who => Some((killer_enemy_id.clone(), *server_dist, *contact_ticks)),
            _ => None,
        })
    }

    /// Victim-favored death: a kill needs the enemy in the (shrunk) death radius for
    /// CONTACT_TICKS_REQUIRED consecutive ticks. One brush doesn't kill; the death
    /// event carries the killer id + server distance + contact-tick count.
    #[test]
    fn enemy_eats_player_needs_persistent_contact() {
        let (mut room, _i) = make_room_with_input_server_death();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false); // weak
        isolate_enemy(&mut room, OPEN_POS, 5); // can_eat, dist 0
        seed_confirming_enemy_history(&mut room); // visible timeline confirms the overlap
        let killer_id = room.enemies[0].id.clone();
        let _ = drain_events(&mut rx);

        // One tick of contact must NOT kill.
        room.check_enemy_collisions();
        assert!(!player_died(&drain_events(&mut rx), &pid), "a single-tick brush must not kill");

        // Reaching the threshold kills, with the diagnostics attached.
        for _ in 1..CONTACT_TICKS_REQUIRED {
            room.check_enemy_collisions();
        }
        let evs = drain_events(&mut rx);
        assert!(player_died(&evs, &pid), "persistent contact kills");
        let (killer, dist, ticks) = respawn_death(&evs, &pid).expect("death event present");
        assert_eq!(killer.as_deref(), Some(killer_id.as_str()), "killer id attributed");
        assert_eq!(ticks, Some(CONTACT_TICKS_REQUIRED), "contact_ticks reported");
        assert!(
            dist.unwrap() < room.config.collision.collision_distance_px,
            "server_dist within death radius"
        );
    }

    /// A contact in the GRACE BAND (between the shrunk death radius and the base
    /// collision distance) never kills, no matter how long it persists.
    #[test]
    fn enemy_in_grace_band_never_kills() {
        let cd = {
            let (room, _i) = make_room_with_input();
            room.config.collision.collision_distance_px
        };
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        // Just inside the base radius but outside the shrunk death radius.
        let band = cd - VICTIM_GRACE_PX * 0.5;
        isolate_enemy(&mut room, Position { x: OPEN_POS.x + band, y: OPEN_POS.y }, 5);
        let _ = drain_events(&mut rx);

        for _ in 0..(CONTACT_TICKS_REQUIRED * 3) {
            room.check_enemy_collisions();
        }
        assert!(!player_died(&drain_events(&mut rx), &pid), "grace-band contact never kills");
    }

    /// Seed the contact-history frame at the victim-visible tick (now − interp_delay) with
    /// the killer enemy at `enemy_visible` relative to the player, then set "now" so the
    /// enemy overlaps the player on the server timeline. Returns the player id + a drained
    /// rx. The room is left at `now_tick`; the visible reconstruction samples the seeded
    /// frame, so `enemy_visible` IS what the gate sees as victim_visible_dist.
    /// Fill the contact-history ring so the victim-visible reconstruction CONFIRMS — records
    /// interp_delay+2 ticks at the enemies' CURRENT (overlapping) positions, so a death-test
    /// that drives `check_enemy_collisions` directly isn't held by the armed gate on
    /// no-history. Advances room.tick past the warmup; the kill loop then samples a close
    /// visible frame. (The server-contact rule the test exercises is unchanged.)
    fn seed_confirming_enemy_history(room: &mut Room) {
        let interp = room.config.death_fairness.enemy_interp_delay_ticks;
        let base = room.tick;
        for t in 0..=(interp + 2) {
            room.tick = base + t;
            room.record_contact_history();
        }
    }

    fn arm_enemy_kill_with_visible(
        room: &mut Room,
        rx: &mut mpsc::Receiver<ServerMessage>,
        pid: &str,
        enemy_visible: Position,
    ) {
        let interp = room.config.death_fairness.enemy_interp_delay_ticks;
        place_player(room, pid, OPEN_POS, 0, false);
        room.tick = interp;
        isolate_enemy(room, enemy_visible, 5);
        room.record_contact_history(); // frame @ visible tick
        room.tick = interp * 2; // visible_tick = now − delay = the seeded frame
        isolate_enemy(room, OPEN_POS, 5); // server_dist ~0 NOW
        let _ = drain_events(rx);
    }

    /// Scenario: "server contact, visible NO contact → no kill, indefinitely." Armed gate:
    /// the enemy overlaps NOW on the server, but the player's screen (reconstructed
    /// interp_delay back) still showed it ~200px away. The server must NEVER kill — and
    /// crucially NOT via a timer: there is no liveness cap (server_tick − interp_delay never
    /// catches server_tick), so even after a long held contact it stays deferred. The kill
    /// only ever comes from visible confirmation. (lead review #1/#5)
    #[test]
    fn enemy_kill_deferred_forever_while_visible_shows_gap() {
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        let far = Position { x: OPEN_POS.x + 200.0, y: OPEN_POS.y };
        arm_enemy_kill_with_visible(&mut room, &mut rx, &pid, far);

        // Hold contact far longer than interp_delay — must STILL not kill (no timer).
        let interp = room.config.death_fairness.enemy_interp_delay_ticks as u32;
        for _ in 0..(CONTACT_TICKS_REQUIRED + interp + 20) {
            room.check_enemy_collisions();
        }
        assert!(
            !player_died(&drain_events(&mut rx), &pid),
            "no kill while the victim-visible timeline shows a gap — and NO timer override"
        );
    }

    /// Scenario: "server contact + visible contact → kill." Armed gate, the reconstructed
    /// enemy was within the visible-confirm radius (5px ≤ 20) on the player's screen → the
    /// player saw contact, so the kill lands at the normal contact threshold (no defer).
    /// Guards against the gate making real, visually-fair deaths non-lethal.
    #[test]
    fn enemy_kill_immediate_when_visible_timeline_shows_contact() {
        let (mut room, _i) = make_room_with_input_server_death();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        // Visible enemy 5px from the player — well within the visible-confirm radius (20).
        let close = Position { x: OPEN_POS.x + 5.0, y: OPEN_POS.y };
        arm_enemy_kill_with_visible(&mut room, &mut rx, &pid, close);

        for _ in 0..CONTACT_TICKS_REQUIRED {
            room.check_enemy_collisions();
        }
        let evs = drain_events(&mut rx);
        assert!(player_died(&evs, &pid), "visible contact → kill at the contact threshold, no defer");
        let (_killer, dist, ticks) = respawn_death(&evs, &pid).expect("death event present");
        assert!(dist.unwrap() < room.config.collision.collision_distance_px, "server_dist is truth");
        assert_eq!(ticks, Some(CONTACT_TICKS_REQUIRED), "killed exactly at the threshold");
    }

    /// Scenario: "server contact broken before visible confirmation → suppress." A deferred
    /// candidate (visible far) whose enemy then leaves the kill radius must be DROPPED, not
    /// killed later — kill comes only from a confirmed contact run. (lead review #5)
    #[test]
    fn deferred_candidate_suppressed_when_server_contact_breaks() {
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        let far = Position { x: OPEN_POS.x + 200.0, y: OPEN_POS.y };
        arm_enemy_kill_with_visible(&mut room, &mut rx, &pid, far);
        for _ in 0..CONTACT_TICKS_REQUIRED {
            room.check_enemy_collisions(); // builds a deferred candidate
        }
        assert_eq!(room.players[&pid].death_contact_ticks, CONTACT_TICKS_REQUIRED, "candidate built");

        // Enemy leaves: server contact breaks → candidate suppressed.
        isolate_enemy(&mut room, Position { x: OPEN_POS.x + 300.0, y: OPEN_POS.y }, 5);
        room.check_enemy_collisions();
        assert_eq!(room.players[&pid].death_contact_ticks, 0, "broken contact resets the run");
        assert!(!player_died(&drain_events(&mut rx), &pid), "no kill from a suppressed candidate");
    }

    /// Scenario: "enemy respawns mid-candidate → old candidate cleared." The contact run is
    /// keyed on (enemy_id, generation); bumping the killer's generation must reset the run so
    /// a deferred death can't survive the respawn. (lead review #4)
    #[test]
    fn deferred_candidate_cleared_on_enemy_respawn_generation_bump() {
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        let far = Position { x: OPEN_POS.x + 200.0, y: OPEN_POS.y };
        arm_enemy_kill_with_visible(&mut room, &mut rx, &pid, far);
        room.check_enemy_collisions();
        assert_eq!(room.players[&pid].death_contact_ticks, 1, "run started on gen 0");

        // Same enemy id, but a new life (generation bumped) — keep it overlapping.
        room.enemies[0].generation += 1;
        room.check_enemy_collisions();
        assert_eq!(room.players[&pid].death_contact_ticks, 1, "generation change resets the run");
        assert_eq!(room.players[&pid].death_contact_generation, room.enemies[0].generation);
    }

    /// Scenario: "boost during a deferred contact → candidate cleared; no instant death when
    /// boost expires." Invincible is an explicit shield: it resets the run, so an old contact
    /// can't kill the moment boost ends. (lead review #6)
    #[test]
    fn boost_clears_deferred_contact_no_instant_death_on_expiry() {
        let (mut room, _i) = make_room_with_input();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        let close = Position { x: OPEN_POS.x + 5.0, y: OPEN_POS.y };
        arm_enemy_kill_with_visible(&mut room, &mut rx, &pid, close);
        room.check_enemy_collisions();
        assert!(room.players[&pid].death_contact_ticks >= 1, "contact run started");

        // Boost on → invincible. The candidate is cleared and no kill accrues while shielded.
        room.players.get_mut(&pid).unwrap().is_invincible = true;
        for _ in 0..(CONTACT_TICKS_REQUIRED + 2) {
            room.check_enemy_collisions();
        }
        assert_eq!(room.players[&pid].death_contact_ticks, 0, "invincible resets the run");
        assert!(!player_died(&drain_events(&mut rx), &pid), "invincible player can't be eaten");

        // Boost expires: a FRESH contact run must build from scratch — no instant death.
        room.players.get_mut(&pid).unwrap().is_invincible = false;
        room.check_enemy_collisions();
        assert_eq!(room.players[&pid].death_contact_ticks, 1, "fresh run, not a carried-over one");
        assert!(!player_died(&drain_events(&mut rx), &pid), "no instant death from a stale contact");
    }

    /// Reliable ordering: the authoritative PlayerKilledByEnemy death event is emitted
    /// BEFORE the victim's PlayerRespawned consequence — a death must never arrive after
    /// its own respawn. The kill event also carries the full timeline-explicit context in
    /// one place (lead review #6). The two are separate domain facts.
    #[test]
    fn kill_event_precedes_respawn_and_carries_full_context() {
        let (mut room, _i) = make_room_with_input_server_death();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        let close = Position { x: OPEN_POS.x + 5.0, y: OPEN_POS.y };
        arm_enemy_kill_with_visible(&mut room, &mut rx, &pid, close);
        for _ in 0..CONTACT_TICKS_REQUIRED {
            room.check_enemy_collisions();
        }
        let evs = drain_events(&mut rx);
        let kill_idx = evs
            .iter()
            .position(
                |e| matches!(e, ServerEvent::PlayerKilledByEnemy { victim_id, .. } if victim_id == &pid),
            )
            .expect("PlayerKilledByEnemy emitted");
        let respawn_idx = evs
            .iter()
            .position(|e| matches!(e, ServerEvent::PlayerRespawned { player_id, .. } if player_id == &pid))
            .expect("PlayerRespawned emitted");
        assert!(kill_idx < respawn_idx, "kill must precede respawn (kill={kill_idx} respawn={respawn_idx})");

        let ServerEvent::PlayerKilledByEnemy {
            server_dist,
            server_kill_radius,
            visible_gate_enabled,
            visible_confirm_radius_px,
            interp_delay_ticks,
            contact_ticks,
            decision,
            policy_version,
            reconstructed_enemy_visible_dist,
            killer_enemy_generation,
            ..
        } = &evs[kill_idx]
        else {
            unreachable!()
        };
        assert!(*server_dist < *server_kill_radius, "server_dist within the kill radius");
        assert!(*visible_gate_enabled, "gate armed in the default config");
        // Option<f32> on the wire — armed ⇒ Some(finite), never an Infinity sentinel.
        let confirm_radius = visible_confirm_radius_px.expect("armed gate carries a finite confirm radius");
        assert!(confirm_radius > *server_kill_radius, "visible-confirm radius is wider than the kill radius");
        assert_eq!(
            *interp_delay_ticks, room.config.death_fairness.enemy_interp_delay_ticks,
            "carries the synced interp delay"
        );
        assert_eq!(*contact_ticks, CONTACT_TICKS_REQUIRED);
        assert_eq!(*decision, DeathDecisionCode::KillNowVisibleConfirmed);
        assert_eq!(*policy_version, room.config.death_fairness.policy_version);
        assert_eq!(*killer_enemy_generation, 0, "first-life enemy generation reported");
        assert!(reconstructed_enemy_visible_dist.is_some(), "the enemy-visible timeline was reconstructed");
    }

    // ---- claim-based enemy-eats-player (EnemyDeathClaim, v6) ----------------
    //
    // The SYMMETRIC counterpart of the EatClaim scenarios above: the victim's client reports
    // the render ticks + visible positions it actually drew; the room rewinds contact history
    // and decides. These exercise the full room path (shape → positions → history → validate →
    // emit), not just the pure policy (covered in policies::enemy_death_claim::tests).

    // Positional builder — the EnemyDeathClaim wire shape genuinely has this many fields.
    #[allow(clippy::too_many_arguments)]
    fn death_claim(
        claim_id: u32,
        enemy_id: &str,
        enemy_generation: u32,
        victim_tick: f64,
        enemy_tick: f64,
        victim_pos: Position,
        enemy_pos: Position,
        dist: f32,
    ) -> EnemyDeathClaim {
        EnemyDeathClaim {
            claim_id,
            enemy_id: enemy_id.to_string(),
            enemy_generation,
            victim_render_tick: victim_tick,
            enemy_render_tick: enemy_tick,
            victim_position: victim_pos,
            enemy_position: enemy_pos,
            visual_distance: dist,
        }
    }
    fn drain_death_rejects(rx: &mut mpsc::Receiver<ServerMessage>) -> Vec<(u32, String)> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ServerMessage::EnemyDeathClaimRejected { claim_id, reason, .. } = msg {
                out.push((claim_id, reason));
            }
        }
        out
    }
    /// Park a weak victim + a strong enemy[0] (score 100, generation 0) and record two history
    /// frames (ticks 10, 11) with the enemy at `enemy_pos`, leaving the room AT tick 11. Returns
    /// the killer enemy id. Mirrors the enemy-EatClaim setup but in the death direction.
    fn arm_victim_death_history(
        room: &mut Room,
        victim: &str,
        victim_pos: Position,
        enemy_pos: Position,
    ) -> String {
        park_enemies_far(room);
        place_player(room, victim, victim_pos, 10, false);
        freeze_unprotected(room, victim);
        let enemy_id = room.enemies[0].id.clone();
        room.enemies[0].score = 100;
        room.enemies[0].generation = 0;
        room.enemies[0].respawned_at_tick = 0;
        room.tick = 10;
        room.enemies[0].position = enemy_pos;
        room.record_contact_history();
        room.tick = 11;
        room.enemies[0].position = enemy_pos;
        room.record_contact_history();
        enemy_id
    }

    /// SAFETY NET for the proven 2026-06-10 ghost death, upgraded to the 2026-06-11
    /// readiness split: a death claim from an admission that has NOT declared
    /// ClientClaimReady is rejected (`claim_ready_not_seen`) even when the claim itself
    /// would validate — and ClientWorldReady (the weaker, visual-only fact) is NOT enough.
    /// After ClientClaimReady the SAME claim shape kills. Also pins that the rejection does
    /// not burn the claim id (the honest client just declares and re-claims).
    #[test]
    fn death_claim_rejected_until_claim_ready() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        // Fresh-admission state: claims not declared trustworthy yet. Visual readiness
        // alone (world_ready) must NOT open the claim gate.
        room.players.get_mut(&vic).unwrap().claim_ready = false;
        room.set_player_world_ready(&vic, 1, 11);
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        assert!(
            drain_death_rejects(&mut rx).iter().any(|(id, r)| *id == 1 && r == "claim_ready_not_seen"),
            "claim before ClientClaimReady rejected (visual readiness alone is not enough)"
        );
        assert!(!player_died(&drain_events(&mut rx), &vic), "no death before claim readiness");

        // ClientClaimReady arrives → the same claim id (not burned by the early reject) kills.
        room.set_player_claim_ready(&vic, 1, 11);
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        assert!(player_died(&drain_events(&mut rx), &vic), "same claim accepted once claim-ready");
    }

    /// Resume admission shield (lead 2026-06-11 P0 #1): inside the window a resumed player's
    /// own death claim is rejected (`resume_admission_grace`) and bots cannot kill them
    /// (shielded contact resets the run); past the window the same claim shape kills.
    #[test]
    fn resume_shield_suppresses_deaths_and_claims_until_it_expires() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        room.set_player_claim_ready(&vic, 1, 11);
        // Model a resume admission at tick 0 (room is at tick 11 after the history arm).
        {
            let p = room.players.get_mut(&vic).unwrap();
            p.admitted_via_resume = true;
            p.admitted_at_tick = 0;
        }
        assert!(room.players[&vic].resume_shield_active(room.tick, RESUME_SHIELD_TICKS));
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        assert!(
            drain_death_rejects(&mut rx).iter().any(|(_, r)| r == "resume_admission_grace"),
            "death claim inside the resume shield rejected"
        );
        assert!(!player_died(&drain_events(&mut rx), &vic), "no death inside the shield");

        // Past the shield (admitted at 0 → expires at RESUME_SHIELD_TICKS; also past the
        // min-claim-age which doesn't apply to admitted_at_tick == 0 anyway): re-arm history
        // at the later tick and the same claim shape kills.
        room.tick = RESUME_SHIELD_TICKS;
        room.enemies[0].position = OPEN_B;
        room.record_contact_history();
        room.tick = RESUME_SHIELD_TICKS + 1;
        room.enemies[0].position = OPEN_B;
        room.record_contact_history();
        let vt = RESUME_SHIELD_TICKS as f64 + 1.0;
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(2, &enemy_id, 0, vt, vt, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        assert!(player_died(&drain_events(&mut rx), &vic), "shield expired → claim kills again");
    }

    /// Min-claim-age (lead 2026-06-11 P0 #5): an admission INTO A RUNNING room (nonzero
    /// admitted_at_tick) cannot have claims honoured for MIN_CLAIM_AFTER_ADMISSION_TICKS; an
    /// original member (admitted at tick 0) is exempt — covered implicitly by every other
    /// claim test here.
    #[test]
    fn death_claim_too_soon_after_live_room_admission_rejected() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        room.set_player_claim_ready(&vic, 1, 11);
        // Fresh (non-resume) admission into a live room one tick ago.
        room.players.get_mut(&vic).unwrap().admitted_at_tick = room.tick - 1;
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        assert!(
            drain_death_rejects(&mut rx).iter().any(|(_, r)| r == "too_soon_after_admission"),
            "claim a tick after a live-room admission rejected by the age gate"
        );
        assert!(!player_died(&drain_events(&mut rx), &vic));
    }

    /// Valid death claim: a weak victim reports a visible overlap with a bigger enemy it saw at
    /// tick 11 → kill (decision = KillNowClaimConfirmed, NO server contact run) + respawn, with
    /// the death fact ordered before the respawn (CLAUDE.md backend #6/#9).
    #[test]
    fn enemy_death_claim_valid_kills_with_claim_confirmed_decision() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();

        let evs = drain_events(&mut rx);
        let kill = evs
            .iter()
            .find(|e| {
                matches!(e,
            ServerEvent::PlayerKilledByEnemy { victim_id, .. } if victim_id == &vic)
            })
            .expect("PlayerKilledByEnemy emitted");
        let ServerEvent::PlayerKilledByEnemy {
            decision,
            contact_ticks,
            killer_enemy_id,
            killer_enemy_generation,
            ..
        } = kill
        else {
            unreachable!()
        };
        assert_eq!(*decision, DeathDecisionCode::KillNowClaimConfirmed, "claim-confirmed decision");
        assert_eq!(*contact_ticks, 0, "claim path carries no server contact run");
        assert_eq!(killer_enemy_id, &enemy_id, "killer attributed");
        assert_eq!(*killer_enemy_generation, 0, "claimed enemy life reported");
        assert!(player_died(&evs, &vic), "victim respawned (died)");

        let kill_idx = evs
            .iter()
            .position(|e| {
                matches!(e,
            ServerEvent::PlayerKilledByEnemy { victim_id, .. } if victim_id == &vic)
            })
            .unwrap();
        let respawn_idx = evs
            .iter()
            .position(|e| {
                matches!(e,
            ServerEvent::PlayerRespawned { player_id, .. } if player_id == &vic)
            })
            .unwrap();
        assert!(kill_idx < respawn_idx, "kill must precede respawn (kill={kill_idx} respawn={respawn_idx})");
    }

    /// Anti-cheat: a forged `visual_distance` (claims 0px overlap while the reported positions are
    /// 5px apart) is rejected by the positions-consistent gate — a far-apart kill can't be smuggled
    /// in by lying about the on-screen distance.
    #[test]
    fn enemy_death_claim_forged_visual_distance_rejected() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        let _ = drain_events(&mut rx);

        // positions are 5px apart, but the claim lies that the visual distance was 0.
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, 0.0),
        }));
        room.process_claims();

        assert!(
            drain_death_rejects(&mut rx).iter().any(|(_, r)| r == "visual_distance_mismatch"),
            "forged visual_distance rejected"
        );
        assert!(!player_died(&drain_events(&mut rx), &vic), "no death on a rejected claim");
    }

    /// A stale claim against a victim that has since respawned (and is NOW spawn-protected) is
    /// rejected — the double-kill guard (`victim_spawn_protected_now`). Without it, an old claim
    /// could kill the freshly-respawned life.
    #[test]
    fn enemy_death_claim_against_respawned_protected_victim_rejected() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        // The victim has since respawned: spawn-protected RIGHT NOW (history at tick 11 was clean).
        room.players.get_mut(&vic).unwrap().spawn_protected_until_tick = Some(room.tick + 100);
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();

        assert!(
            drain_death_rejects(&mut rx).iter().any(|(_, r)| r == "victim_spawn_protected_now"),
            "stale claim against the protected new life rejected"
        );
        assert!(!player_died(&drain_events(&mut rx), &vic), "the new life did not die");
    }

    /// round-3 follow-up (Blocker 1): a stale death claim against a victim that has since respawned
    /// is rejected by LIFE ID even after spawn protection has EXPIRED — the case `victim_now_spawn_
    /// protected` no longer covers. The history frame is the previous life (life 0); the live victim
    /// is life 1 and unprotected, so only the life_id mismatch catches it. Without it the old claim
    /// would kill the new life.
    #[test]
    fn enemy_death_claim_against_respawned_victim_after_protection_expired_rejected() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        // The victim has since respawned into a NEW life whose protection is already gone (the helper
        // cleared spawn protection). History at tick 11 is still the previous life (life 0).
        room.players.get_mut(&vic).unwrap().life_id = 1;
        assert!(
            !room.players.get(&vic).unwrap().is_spawn_protected(room.tick),
            "precondition: the new life is NOT spawn-protected, so only life_id can catch the claim"
        );
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();

        assert!(
            drain_death_rejects(&mut rx).iter().any(|(_, r)| r == "victim_life_mismatch"),
            "stale claim against a since-respawned (unprotected) life rejected by life_id"
        );
        assert!(!player_died(&drain_events(&mut rx), &vic), "the new life did not die");
    }

    /// round-3 follow-up (Blocker): an honest claim's render skew can reach the client's prediction
    /// lead PLUS the enemy interp delay (victim predicted ahead, enemy interpolated behind). The skew
    /// cap is config-derived, so such a claim is accepted — the old hardcoded 30-tick cap would
    /// reject it (skew_too_large), and the fallback could then kill a player whose real claim was valid.
    #[test]
    fn enemy_death_claim_accepts_skew_up_to_lead_plus_interp() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let lead = room.config.network.max_client_prediction_lead_ticks;
        let interp = room.config.death_fairness.enemy_interp_delay_ticks;
        let skew = lead + interp; // e.g. 36 + 8 = 44, far beyond the old 30-tick cap
        assert!(
            skew as f64 <= room.death_claim_max_skew_ticks(),
            "the config-derived skew cap must accommodate lead + interp"
        );

        park_enemies_far(&mut room);
        place_player(&mut room, &vic, OPEN_A, 10, false); // weak victim
        freeze_unprotected(&mut room, &vic);
        let enemy_id = room.enemies[0].id.clone();
        room.enemies[0].score = 100;
        room.enemies[0].generation = 0;
        room.enemies[0].respawned_at_tick = 0;
        // Enemy frame at enemy_tick, victim frame at victim_tick = enemy_tick + skew. Both stationary
        // so the reconstructed victim↔enemy distance stays an overlap across the large skew.
        let enemy_tick = 10u64;
        let victim_tick = enemy_tick + skew;
        for &t in &[enemy_tick, victim_tick] {
            room.tick = t;
            room.enemies[0].position = OPEN_B;
            room.record_contact_history();
        }
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            claim: death_claim(
                1,
                &enemy_id,
                0,
                victim_tick as f64,
                enemy_tick as f64,
                OPEN_A,
                OPEN_B,
                dist2(OPEN_A, OPEN_B),
            ),
        }));
        room.process_claims();

        let (mut evs, mut rejects) = (Vec::new(), Vec::new());
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ServerMessage::Event(ev) => evs.push(ev),
                ServerMessage::EnemyDeathClaimRejected { reason, .. } => rejects.push(reason),
                _ => {}
            }
        }
        assert!(
            !rejects.iter().any(|r| r == "skew_too_large"),
            "a large but honest skew (lead + interp) must NOT be rejected (got {rejects:?})"
        );
        assert!(rejects.is_empty(), "claim accepted, no rejects (got {rejects:?})");
        assert!(player_died(&evs, &vic), "the honest high-skew death is confirmed");
    }

    /// v6 review #2: a death that was valid AT the claimed tick still stands even though the killer
    /// enemy has since been eaten/respawned into a NEW generation. The kill is validated against the
    /// gen-0 HISTORY frame, never the live (now gen-1) enemy — checking the live generation would
    /// wrongly reject a legitimate rewound death.
    #[test]
    fn enemy_death_claim_accepted_even_after_enemy_respawned_into_new_generation() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (vic, _) = room.add_player("uv".into(), "V".into(), out);
        let enemy_id = arm_victim_death_history(&mut room, &vic, OPEN_A, OPEN_B);
        // The LIVE enemy has moved on to a new life since the claimed tick.
        room.enemies[0].generation = 1;
        room.enemies[0].respawned_at_tick = 12;
        room.enemies[0].position = Position { x: 480.0, y: 980.0 };
        let _ = drain_events(&mut rx);

        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: vic.clone(),
            // claim names the gen-0 life it actually saw at tick 11.
            claim: death_claim(1, &enemy_id, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();

        // Single drain (rejects + events share one channel; a split drain would discard the other).
        let (mut evs, mut rejects) = (Vec::new(), Vec::new());
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ServerMessage::Event(ev) => evs.push(ev),
                ServerMessage::EnemyDeathClaimRejected { reason, .. } => rejects.push(reason),
                _ => {}
            }
        }
        assert!(rejects.is_empty(), "valid rewound death must NOT be rejected (got {rejects:?})");
        assert!(player_died(&evs, &vic), "rewound death still kills");
        let kill =
            evs.iter()
                .find_map(|e| match e {
                    ServerEvent::PlayerKilledByEnemy {
                        victim_id, killer_enemy_generation, decision, ..
                    } if victim_id == &vic => Some((*killer_enemy_generation, *decision)),
                    _ => None,
                })
                .expect("PlayerKilledByEnemy emitted");
        assert_eq!(kill.0, 0, "the claimed (gen-0) life is recorded, not the live gen-1");
        assert_eq!(kill.1, DeathDecisionCode::KillNowClaimConfirmed);
    }

    /// v6 anti-cheat fallback: at policy 6 the server is normally observe-only (it logs a
    /// fallback_candidate and waits for the client's claim). But if a LETHAL, visibly-confirmed
    /// contact persists `SERVER_FALLBACK_KILL_TICKS` with NO claim arriving, the server kills
    /// anyway — a client can't become immortal vs bots by withholding claims. Below the threshold
    /// it must NOT kill.
    #[test]
    fn v6_sustained_lethal_contact_without_claim_kills_via_anticheat_fallback() {
        let (mut room, _i, _c) = make_room_with_claim(); // policy_version 6 (default)
        assert_eq!(room.config.death_fairness.policy_version, 6, "default config is v6");
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false); // weak
                                                           // The STRICT fallback bar (lead 2026-06-11 P0 #2) requires the victim-projected leg
                                                           // to EXIST: give the victim a lead sample (4 ticks ≈ 6.7px of projection — inside
                                                           // kill_radius + grace). Without it the net must never kill (see the strict test).
        room.players.get_mut(&pid).unwrap().last_input_lead_sample = Some((4, 0));
        isolate_enemy(&mut room, OPEN_POS, 5); // can_eat, dist 0
        seed_confirming_enemy_history(&mut room); // visible timeline confirms the overlap
        let _ = drain_events(&mut rx);

        // Below the fallback threshold: observe-only, NO death however long the (already lethal,
        // visibly-confirmed) contact persists.
        for _ in 0..(SERVER_FALLBACK_KILL_TICKS as usize - 1) {
            room.check_enemy_collisions();
        }
        assert!(
            !player_died(&drain_events(&mut rx), &pid),
            "no claim + below threshold ⇒ server stays observe-only"
        );

        // One more tick reaches SERVER_FALLBACK_KILL_TICKS ⇒ the anti-cheat net kills, tagged with
        // the DISTINCT fallback decision (round-3 #3) — never KillNowVisibleConfirmed/ClaimConfirmed.
        room.check_enemy_collisions();
        let evs = drain_events(&mut rx);
        assert!(
            player_died(&evs, &pid),
            "sustained lethal contact with no claim kills at the fallback threshold"
        );
        let decision = evs
            .iter()
            .find_map(|e| match e {
                ServerEvent::PlayerKilledByEnemy { victim_id, decision, .. } if victim_id == &pid => {
                    Some(*decision)
                }
                _ => None,
            })
            .expect("PlayerKilledByEnemy emitted");
        assert_eq!(
            decision,
            DeathDecisionCode::KillNowClaimMissingSustained,
            "fallback kill is tagged distinctly, not as a visible/claim-confirmed kill"
        );
    }

    /// STRICT fallback bar (lead 2026-06-11 P0 #2): with NO victim-projected leg (no lead
    /// sample — the 07-12-37 fallback ghost's signature), the sustained-missing-claim net
    /// must NEVER kill, however long the lethal contact persists. It defers instead.
    #[test]
    fn v6_fallback_without_projection_never_kills() {
        let (mut room, _i, _c) = make_room_with_claim(); // policy 6
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        // NO last_input_lead_sample → victim projection is None → strict bar can't pass.
        isolate_enemy(&mut room, OPEN_POS, 5);
        seed_confirming_enemy_history(&mut room);
        let _ = drain_events(&mut rx);

        for _ in 0..(SERVER_FALLBACK_KILL_TICKS as usize * 4) {
            room.check_enemy_collisions();
        }
        assert!(
            !player_died(&drain_events(&mut rx), &pid),
            "no projection leg ⇒ the fallback defers forever instead of ghost-killing"
        );
    }

    /// round-3 #2 (+ Blocker follow-up): a PLAUSIBLE pending death claim is NEVER pre-empted before it
    /// could become ready, yet the hold is still time-boxed so a never-resolving / refresh-spammed
    /// claim can't grant immortality. The hold cap must be ≥ the scheduling window + 1 (a claim sits
    /// in pending up to `death_claim_max_future_ticks` ahead before the sim reaches its render tick) —
    /// the previous version locked in a kill at SERVER_FALLBACK_KILL_TICKS + grace (45) even though a
    /// claim could be pending out to max_future (46), which is exactly the edge the lead flagged.
    #[test]
    fn v6_fallback_hold_for_a_pending_death_claim_is_time_boxed() {
        let (mut room, _i, _c) = make_room_with_claim(); // policy 6
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        isolate_enemy(&mut room, OPEN_POS, 5);
        seed_confirming_enemy_history(&mut room);
        room.players.get_mut(&pid).unwrap().last_input_lead_sample = Some((4, 0)); // strict bar needs the projection leg
        let enemy_id = room.enemies[0].id.clone();
        let enemy_gen = room.enemies[0].generation;
        let max_future = room.death_claim_max_future_ticks();
        let cap = room.death_claim_fallback_hold_cap_ticks();
        // BLOCKER invariant: the hold cap covers the whole scheduling window, so a claim admitted to
        // pending as plausible is never killed before it can reach its ready tick.
        assert!(cap as u64 > max_future, "hold cap ({cap}) must exceed max_future ({max_future})");

        // A PLAUSIBLE claim at the FAR edge of the scheduling window (victim_render_tick = now +
        // max_future; skew 0). We NEVER advance the sim / run process_claims, so it stays pending the
        // whole test — modelling a never-resolving (or refresh-spammed) claim.
        let vt = room.tick as f64 + max_future as f64;
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: pid.clone(),
            claim: death_claim(1, &enemy_id, enemy_gen, vt, vt, OPEN_POS, OPEN_POS, 0.0),
        }));
        assert!(
            room.has_plausible_pending_enemy_death_claim(&pid, &enemy_id, enemy_gen),
            "a claim at the far edge of the scheduling window is plausible"
        );
        let _ = drain_events(&mut rx);

        // Through the WHOLE hold cap minus one (this now reaches PAST the old 45-tick kill point): no
        // pre-emption while a plausible claim is pending.
        for _ in 0..(cap as usize - 1) {
            room.check_enemy_collisions();
        }
        assert!(
            !player_died(&drain_events(&mut rx), &pid),
            "a plausible pending claim is not pre-empted within the hold cap (incl. past the old grace)"
        );

        // At the cap: the fallback fires even though the claim is STILL pending — bounded hold, so a
        // refresh-spammed / never-resolving claim can't be immortal.
        room.check_enemy_collisions();
        assert!(
            room.has_plausible_pending_enemy_death_claim(&pid, &enemy_id, enemy_gen),
            "the claim is still pending — we never resolved it"
        );
        assert!(
            player_died(&drain_events(&mut rx), &pid),
            "past the time-boxed hold cap the fallback kills regardless of the pending claim"
        );
    }

    /// round-3 #2: an IMPLAUSIBLE pending death claim (here, far-future) earns NO fallback grace at
    /// all — the kill lands at SERVER_FALLBACK_KILL_TICKS, exactly as if nothing were pending. This
    /// is the stall exploit a bare existence check allowed: spam a future claim, never die. The
    /// `victim_render_tick > tick + death_claim_hold_lead_ticks()` gate (and the tighter scheduling
    /// cap) shut it down.
    #[test]
    fn v6_fallback_ignores_an_implausible_pending_death_claim() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        isolate_enemy(&mut room, OPEN_POS, 5);
        seed_confirming_enemy_history(&mut room);
        room.players.get_mut(&pid).unwrap().last_input_lead_sample = Some((4, 0)); // strict bar needs the projection leg
        let enemy_id = room.enemies[0].id.clone();
        let enemy_gen = room.enemies[0].generation;
        // Far-future: well beyond the hold lead. Ingested into pending (we don't run process_claims,
        // which would itself reject it as claim_too_far_future under death_claim_max_future_ticks()).
        let far = room.tick as f64 + room.death_claim_hold_lead_ticks() + 25.0;
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: pid.clone(),
            claim: death_claim(1, &enemy_id, enemy_gen, far, far, OPEN_POS, OPEN_POS, 0.0),
        }));
        assert!(room.has_pending_enemy_death_claim(&pid, &enemy_id, enemy_gen), "claim is in the ring");
        assert!(
            !room.has_plausible_pending_enemy_death_claim(&pid, &enemy_id, enemy_gen),
            "but it is NOT plausible (too far future), so it earns no grace"
        );
        let _ = drain_events(&mut rx);

        // Does NOT hold: the kill lands at the no-claim threshold.
        for _ in 0..(SERVER_FALLBACK_KILL_TICKS as usize - 1) {
            room.check_enemy_collisions();
        }
        assert!(!player_died(&drain_events(&mut rx), &pid), "below threshold: still observe-only");
        room.check_enemy_collisions();
        assert!(
            player_died(&drain_events(&mut rx), &pid),
            "a far-future pending claim does not delay the fallback past the threshold"
        );
    }

    /// round-3 follow-up (Blocker regression guard): the fallback's plausibility check runs `shape`
    /// too, so it must use the SAME config-derived skew cap — a pending claim whose render skew is the
    /// full honest worst case (lead + interp: victim predicted `lead` ahead, enemy interpolated
    /// `lead + interp` behind) must count as plausible and HOLD the fallback. With the old 30-tick
    /// const it'd be rejected as implausible and the server could pre-empt an honest in-flight claim.
    #[test]
    fn v6_fallback_holds_for_a_high_skew_plausible_claim() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (out, mut rx, _s) = test_outbound();
        let (pid, _) = room.add_player("u".into(), "n".into(), out);
        place_player(&mut room, &pid, OPEN_POS, 0, false);
        isolate_enemy(&mut room, OPEN_POS, 5);
        seed_confirming_enemy_history(&mut room);
        room.players.get_mut(&pid).unwrap().last_input_lead_sample = Some((4, 0)); // strict bar needs the projection leg
        let enemy_id = room.enemies[0].id.clone();
        let enemy_gen = room.enemies[0].generation;
        let lead = room.config.network.max_client_prediction_lead_ticks as f64;
        let interp = room.config.death_fairness.enemy_interp_delay_ticks as f64;
        // victim predicted `lead` ahead of now; enemy interpolated `lead + interp` behind the victim.
        let victim_tick = room.tick as f64 + lead;
        let enemy_tick = victim_tick - (lead + interp);
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: pid.clone(),
            claim: death_claim(1, &enemy_id, enemy_gen, victim_tick, enemy_tick, OPEN_POS, OPEN_POS, 0.0),
        }));
        assert!(room.has_plausible_pending_enemy_death_claim(&pid, &enemy_id, enemy_gen),
            "a high-skew (lead+interp) near-future claim is plausible — has_plausible uses the config skew cap");
        let _ = drain_events(&mut rx);

        // It engages the bounded fallback hold: past the no-claim threshold, still no server kill.
        for _ in 0..(SERVER_FALLBACK_KILL_TICKS as usize + 2) {
            room.check_enemy_collisions();
        }
        assert!(
            !player_died(&drain_events(&mut rx), &pid),
            "the plausible high-skew claim holds the fallback through the grace window"
        );
    }

    /// v6 review #3: an eat claim and an enemy-death claim resolve in ONE chronological order
    /// (oldest effective render tick first), not by a fixed eat-vs-death precedence. We observe the
    /// broadcast event order: whichever claim references the EARLIER tick produces its event first.
    #[test]
    fn claims_resolve_in_chronological_order_by_render_tick() {
        // Returns (room, A's rx, attacker_id, victim_id, e1_id, e2_id), room left at tick 11 with
        // history frames at 10 and 11 for both actors.
        fn setup() -> (Room, mpsc::Receiver<ServerMessage>, String, String, String, String) {
            let (mut room, _i, _c) = make_room_with_claim();
            let (oa, rxa, _s) = test_outbound();
            let (ob, _rxb, _s2) = test_outbound();
            let (a, _) = room.add_player("ua".into(), "A".into(), oa);
            let (v, _) = room.add_player("uv".into(), "V".into(), ob);
            park_enemies_far(&mut room);
            place_player(&mut room, &a, OPEN_A, 200, false); // attacker, bigger than E1
            place_player(&mut room, &v, V_POS, 10, false); // victim, smaller than E2
            freeze_unprotected(&mut room, &a);
            freeze_unprotected(&mut room, &v);
            let e1 = room.enemies[0].id.clone();
            let e2 = room.enemies[1].id.clone();
            room.enemies[0].score = 100;
            room.enemies[0].generation = 0;
            room.enemies[0].respawned_at_tick = 0;
            room.enemies[1].score = 100;
            room.enemies[1].generation = 0;
            room.enemies[1].respawned_at_tick = 0;
            for t in 10..=11 {
                room.tick = t;
                room.enemies[0].position = OPEN_B; // beside the attacker
                room.enemies[1].position = E2_POS; // beside the victim
                room.record_contact_history();
            }
            (room, rxa, a, v, e1, e2)
        }
        const V_POS: Position = Position { x: 150.0, y: 150.0 }; // open, away from A
        const E2_POS: Position = Position { x: 155.0, y: 150.0 }; // 5px from the victim

        // Index of the eat's EnemyRespawned vs the death's PlayerKilledByEnemy in broadcast order.
        fn order(evs: &[ServerEvent], attacker: &str, victim: &str) -> (usize, usize) {
            let eat = evs
                .iter()
                .position(|e| {
                    matches!(e,
                ServerEvent::EnemyRespawned { caused_by_player_id: Some(by), .. } if by == attacker)
                })
                .expect("eat EnemyRespawned emitted");
            let death = evs
                .iter()
                .position(|e| {
                    matches!(e,
                ServerEvent::PlayerKilledByEnemy { victim_id, .. } if victim_id == victim)
                })
                .expect("death PlayerKilledByEnemy emitted");
            (eat, death)
        }

        // Scenario 1: eat is OLDER (tick 10) than death (tick 11) ⇒ eat resolves first.
        let (mut room, mut rxa, a, v, e1, e2) = setup();
        let _ = drain_events(&mut rxa);
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: a.clone(),
            claim: eat_claim(1, EatTargetKind::Enemy, &e1, 10.0, 10.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: v.clone(),
            claim: death_claim(1, &e2, 0, 11.0, 11.0, V_POS, E2_POS, dist2(V_POS, E2_POS)),
        }));
        room.process_claims();
        let (eat_idx, death_idx) = order(&drain_events(&mut rxa), &a, &v);
        assert!(eat_idx < death_idx, "older eat (t10) resolves before newer death (t11)");

        // Scenario 2: death is OLDER (tick 10) than eat (tick 11) ⇒ death resolves first. Same
        // claims, ticks swapped — proves it's tick-ordered, not a fixed eat-before-death rule.
        let (mut room, mut rxa, a, v, e1, e2) = setup();
        let _ = drain_events(&mut rxa);
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: a.clone(),
            claim: eat_claim(1, EatTargetKind::Enemy, &e1, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: v.clone(),
            claim: death_claim(1, &e2, 0, 10.0, 10.0, V_POS, E2_POS, dist2(V_POS, E2_POS)),
        }));
        room.process_claims();
        let (eat_idx, death_idx) = order(&drain_events(&mut rxa), &a, &v);
        assert!(death_idx < eat_idx, "older death (t10) resolves before newer eat (t11)");
    }

    /// round-3 #1: the causal ledger resolves a conflict over the SAME enemy life that pure
    /// tick-sorting can't — the history ring is immutable, so a since-eaten enemy still has a frame
    /// to "kill" from. Three documented policies, all on one enemy E + victim V + attacker A:
    ///   * eat@10 then death@11 ⇒ the later death is REJECTED (enemy already consumed).
    ///   * death@10 then eat@11 ⇒ BOTH apply (a bot can kill someone then itself be eaten).
    ///   * eat@10 and death@10 (exact tie) ⇒ BOTH apply (strict `<` — each player's own outcome).
    #[test]
    fn causal_ledger_resolves_same_enemy_eat_death_conflicts() {
        // Returns (room, V's rx, attacker_id, victim_id, enemy_id). V's rx sees the broadcast
        // events AND the death reject (sent to V). History frames at ticks 10 and 11; room at 11.
        fn setup() -> (Room, mpsc::Receiver<ServerMessage>, String, String, String) {
            let (mut room, _i, _c) = make_room_with_claim();
            let (oa, _rxa, _s) = test_outbound();
            let (ov, rxv, _s2) = test_outbound();
            let (a, _) = room.add_player("ua".into(), "A".into(), oa);
            let (v, _) = room.add_player("uv".into(), "V".into(), ov);
            park_enemies_far(&mut room);
            place_player(&mut room, &a, OPEN_A, 200, false); // bigger than E → can eat it
            place_player(&mut room, &v, OPEN_A, 10, false); // smaller than E → E can eat it
            freeze_unprotected(&mut room, &a);
            freeze_unprotected(&mut room, &v);
            let e = room.enemies[0].id.clone();
            room.enemies[0].score = 100;
            room.enemies[0].generation = 0;
            room.enemies[0].respawned_at_tick = 0;
            for t in 10..=11 {
                room.tick = t;
                room.enemies[0].position = OPEN_B;
                room.record_contact_history();
            }
            (room, rxv, a, v, e)
        }
        fn drain_split(rx: &mut mpsc::Receiver<ServerMessage>) -> (Vec<ServerEvent>, Vec<String>) {
            let (mut evs, mut dr) = (Vec::new(), Vec::new());
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ServerMessage::Event(ev) => evs.push(ev),
                    ServerMessage::EnemyDeathClaimRejected { reason, .. } => dr.push(reason),
                    _ => {}
                }
            }
            (evs, dr)
        }

        // eat@10 then death@11 by the SAME enemy ⇒ death rejected as stale.
        let (mut room, mut rxv, a, v, e) = setup();
        let _ = drain_split(&mut rxv);
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: a.clone(),
            claim: eat_claim(1, EatTargetKind::Enemy, &e, 10.0, 10.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: v.clone(),
            claim: death_claim(1, &e, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        let (evs, dr) = drain_split(&mut rxv);
        assert!(ate_enemy(&evs, &a), "the earlier eat applied");
        assert!(
            dr.iter().any(|r| r == "enemy_already_consumed_by_earlier_claim"),
            "the later death by the since-eaten enemy is rejected"
        );
        assert!(!player_died(&evs, &v), "the stale death did not kill the victim");

        // death@10 then eat@11 by the SAME enemy ⇒ both apply (kill then be eaten).
        let (mut room, mut rxv, a, v, e) = setup();
        let _ = drain_split(&mut rxv);
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: v.clone(),
            claim: death_claim(1, &e, 0, 10.0, 10.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: a.clone(),
            claim: eat_claim(1, EatTargetKind::Enemy, &e, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        let (evs, dr) = drain_split(&mut rxv);
        assert!(dr.is_empty(), "no rejects: death does not consume the enemy");
        assert!(player_died(&evs, &v), "older death applied");
        assert!(ate_enemy(&evs, &a), "later eat of the same enemy ALSO applied");

        // eat@10 and death@10 (exact tie) ⇒ both apply (strict `<` ledger lets the tie stand).
        let (mut room, mut rxv, a, v, e) = setup();
        let _ = drain_split(&mut rxv);
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: a.clone(),
            claim: eat_claim(1, EatTargetKind::Enemy, &e, 10.0, 10.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: v.clone(),
            claim: death_claim(1, &e, 0, 10.0, 10.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        let (evs, dr) = drain_split(&mut rxv);
        assert!(dr.is_empty(), "exact-tie conflict is allowed (no reject)");
        assert!(ate_enemy(&evs, &a), "tie: eat applied");
        assert!(player_died(&evs, &v), "tie: death ALSO applied");
    }

    /// round-3 #1 (the persistence blocker): the conflicting eat + death need NOT arrive in the same
    /// drain batch. `eat@10` resolves in one `process_claims`; the stale `death@11` by that same
    /// since-eaten enemy arrives in a LATER one. A per-batch ledger would be empty by then and the
    /// gen-0 history frame still present, so the death would wrongly stand. The PERSISTENT Room-level
    /// ledger rejects it across batches.
    #[test]
    fn causal_ledger_persists_across_process_claims_batches() {
        let (mut room, _i, _c) = make_room_with_claim();
        let (oa, _rxa, _s) = test_outbound();
        let (ov, mut rxv, _s2) = test_outbound();
        let (a, _) = room.add_player("ua".into(), "A".into(), oa);
        let (v, _) = room.add_player("uv".into(), "V".into(), ov);
        park_enemies_far(&mut room);
        place_player(&mut room, &a, OPEN_A, 200, false); // bigger than E → can eat it
        place_player(&mut room, &v, OPEN_A, 10, false); // smaller than E → E can eat it
        freeze_unprotected(&mut room, &a);
        freeze_unprotected(&mut room, &v);
        let e = room.enemies[0].id.clone();
        room.enemies[0].score = 100;
        room.enemies[0].generation = 0;
        room.enemies[0].respawned_at_tick = 0;
        for t in 10..=11 {
            room.tick = t;
            room.enemies[0].position = OPEN_B;
            room.record_contact_history();
        }
        fn drain_split(rx: &mut mpsc::Receiver<ServerMessage>) -> (Vec<ServerEvent>, Vec<String>) {
            let (mut evs, mut dr) = (Vec::new(), Vec::new());
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ServerMessage::Event(ev) => evs.push(ev),
                    ServerMessage::EnemyDeathClaimRejected { reason, .. } => dr.push(reason),
                    _ => {}
                }
            }
            (evs, dr)
        }
        let _ = drain_split(&mut rxv);

        // BATCH 1 (room at tick 11): A's eat@10 of E gen-0 resolves and consumes that enemy life.
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: a.clone(),
            claim: eat_claim(1, EatTargetKind::Enemy, &e, 10.0, 10.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        let (evs, dr) = drain_split(&mut rxv);
        assert!(ate_enemy(&evs, &a), "batch 1: the eat applied");
        assert!(dr.is_empty(), "batch 1: no rejects");

        // A later tick: record a frame (live enemy is now gen-1) and prune — the gen-0 frame at tick
        // 11 survives in the ring, and the ledger entry survives the prune.
        room.tick = 12;
        room.record_contact_history();

        // BATCH 2: the stale death@11 "this enemy ate me", by the SAME gen-0 life A already ate.
        room.apply_command(RoomCommand::EnemyDeathClaim(EnemyDeathClaimInput {
            player_id: v.clone(),
            claim: death_claim(1, &e, 0, 11.0, 11.0, OPEN_A, OPEN_B, dist2(OPEN_A, OPEN_B)),
        }));
        room.process_claims();
        let (evs, dr) = drain_split(&mut rxv);
        assert!(
            dr.iter().any(|r| r == "enemy_already_consumed_by_earlier_claim"),
            "batch 2: the stale death by the since-eaten enemy is rejected across batches (got {dr:?})"
        );
        assert!(!player_died(&evs, &v), "batch 2: the stale death did not kill the victim");
    }
}

/// Filler-bot scenarios (lead manifesto 2026-06-11): fillers are fake PLAYERS — same wire
/// PlayerState, no bot tell — that fill rooms to a visible-population target and yield
/// seats to humans via DELAYED exits. Every other suite runs filler-free (see make_room);
/// this one opts in with the real `[filler]` config.
#[cfg(test)]
mod filler_tests {
    use super::*;
    use crate::config_shared::{load_config_from_str, load_maze_from_str};
    use tokio::sync::watch;

    fn make_filler_room(seed: u64) -> Room {
        // The suite opts in explicitly — the scenarios must stay green regardless of the
        // shipping toml's flags. Mobs are pinned back on too: fillers perceive them as
        // threats, and the keyframe test asserts the two wire lists stay separate.
        let mut config = load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap();
        config.filler.enabled = true;
        config.ai.count = 12;
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let (room, _input_tx, _claim_tx) = Room::new(Arc::new(config), maze, Some(seed));
        room
    }

    fn test_outbound(
    ) -> (PlayerOutbound, mpsc::Receiver<ServerMessage>, watch::Receiver<Option<ServerMessage>>) {
        let (reliable_tx, reliable_rx) = mpsc::channel(256);
        let (snapshot_tx, snapshot_rx) = watch::channel(None);
        (PlayerOutbound { reliable: reliable_tx, snapshot: snapshot_tx }, reliable_rx, snapshot_rx)
    }

    #[test]
    fn fresh_room_has_no_fillers_until_first_human() {
        let room = make_filler_room(11);
        // Lead review P0 #2: Room::new must NOT pre-spawn — the fill belongs to the first
        // human admission, or the joiner lands in a target+1 room.
        assert_eq!(room.filler_count(), 0, "no fillers before the first human");
        assert_eq!(room.player_count(), 0);
    }

    #[test]
    fn first_human_initial_keyframe_has_exactly_target_visible_players() {
        let mut room = make_filler_room(11);
        let target = room.filler_target();
        assert!(target > 0);

        let (outbound, mut reliable_rx, _snap) = test_outbound();
        let (human_id, initial_full) =
            room.try_add_player("u_first".into(), "first".into(), outbound).unwrap();

        // The join keyframe shows exactly `target` players: the human + target-1 fillers,
        // all as ORDINARY PlayerState entries (the wire type has no is_bot/user_id field).
        let ServerMessage::GameState(full) = initial_full else {
            panic!("join keyframe must be GameState");
        };
        assert_eq!(
            full.players.len(),
            target,
            "first human sees exactly target_visible_players, not target+1"
        );
        assert!(full.players.iter().any(|p| p.id == human_id));
        for p in &full.players {
            if p.id == human_id {
                continue;
            }
            assert!(
                !p.nickname.to_lowercase().starts_with("bot"),
                "filler nickname '{}' must not read as a bot",
                p.nickname
            );
            assert!(
                FILLER_NICKNAMES.contains(&p.nickname.as_str()),
                "nickname '{}' must come from the curated pool",
                p.nickname
            );
        }
        // PvE enemies stay a SEPARATE wire list, untouched by the filler system.
        assert_eq!(full.enemies.len(), room.config.ai.count);

        // No exit is scheduled for the first human (the room was filled FOR them), and
        // their own reliable lane saw no filler PlayerJoined before GameJoined could be
        // sent (the fillers predate their outbox registration).
        assert!(room.scheduled_filler_exits.is_empty(), "first human fill must not trigger an exit");
        let mut early_filler_joins = 0;
        while let Ok(msg) = reliable_rx.try_recv() {
            if let ServerMessage::PlayerJoined { player_id, .. } = msg {
                // The human's OWN PlayerJoined broadcast is pre-existing behavior; only
                // FILLER joins leaking ahead of the keyframe would be new and wrong.
                if player_id != human_id {
                    early_filler_joins += 1;
                }
            }
        }
        assert_eq!(early_filler_joins, 0, "no filler PlayerJoined may precede the join keyframe");
    }

    #[test]
    fn filler_nicknames_are_unique_within_a_room() {
        // Stress the picker well past one room's worth: target == max_players == 40.
        let mut config = load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap();
        config.filler.enabled = true;
        config.filler.target_visible_players = 40;
        config.room.max_players = 40;
        config.ai.count = 0;
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let (mut room, _input_tx, _claim_tx) = Room::new(Arc::new(config), maze, Some(21));

        let (outbound, _r, _s) = test_outbound();
        room.try_add_player("u_h".into(), "h".into(), outbound).unwrap();
        assert_eq!(room.filler_count(), 39);

        let mut seen = HashSet::new();
        for p in room.players.values() {
            assert!(
                seen.insert(p.nickname.clone()),
                "duplicate nickname '{}' in one room — bot tell",
                p.nickname
            );
        }
    }

    #[test]
    fn human_join_schedules_delayed_filler_exit_never_same_tick() {
        let mut room = make_filler_room(12);
        let target = room.filler_target();

        // Human #1 fills the room FOR themselves — no exit. Human #2 puts it one over.
        let (outbound1, mut reliable_rx, _snap1) = test_outbound();
        room.try_add_player("u_h1".into(), "h1".into(), outbound1).unwrap();
        assert!(room.scheduled_filler_exits.is_empty());
        let (outbound2, _rx2, _snap2) = test_outbound();
        let join_tick = room.tick;
        room.try_add_player("u_h2".into(), "h2".into(), outbound2).unwrap();

        // Over target by one ⇒ exactly ONE exit scheduled, strictly in the future and no
        // earlier than the configured minimum delay. NOBODY leaves at the join tick.
        assert_eq!(room.scheduled_filler_exits.len(), 1, "one human over target = one exit");
        let leaver_id = room.scheduled_filler_exits[0].player_id.clone();
        let exit_at = room.scheduled_filler_exits[0].at_tick;
        // The leaver is held far from both humans below so the visibility recheck can't
        // postpone this scenario (the postpone has its own test).
        let away = far_corner_from_humans(&room);
        assert!(
            exit_at >= join_tick + room.config.filler.exit_delay_min_ticks,
            "exit at {exit_at} must respect the min delay from join tick {join_tick}"
        );
        assert_eq!(
            room.filler_count(),
            target - 1,
            "the leaver is still present right after the join (delayed, not same-tick)"
        );

        // Run the sim past the exit tick: the leaver goes through the ORDINARY remove path.
        while room.tick <= exit_at {
            if room.players.contains_key(&leaver_id) {
                pin(&mut room, &leaver_id, away);
            }
            room.update();
        }
        assert_eq!(room.filler_count(), target - 2, "the scheduled filler left after its delay");
        assert!(!room.players.contains_key(&leaver_id), "the leaver's entity is gone");

        // The human's reliable lane saw a PlayerLeft for that exact player — to the client
        // this is indistinguishable from a real player quitting.
        let mut saw_left = false;
        while let Ok(msg) = reliable_rx.try_recv() {
            if let ServerMessage::PlayerLeft { player_id } = msg {
                if player_id == leaver_id {
                    saw_left = true;
                }
            }
        }
        assert!(saw_left, "PlayerLeft for the leaver must reach connected clients");
    }

    #[test]
    fn is_full_counts_humans_only_so_fillers_never_block_a_seat() {
        let mut room = make_filler_room(13);
        let max = room.config.room.max_players;

        // The first human's join fills the room with fillers; every later human seat must
        // still be grantable even though the room reads as "full" of entities.
        let mut keep_alive = Vec::new();
        for i in 0..max {
            let (outbound, r, s) = test_outbound();
            keep_alive.push((r, s));
            room.try_add_player(format!("u{i}"), format!("h{i}"), outbound)
                .unwrap_or_else(|e| panic!("human {i} must get a seat over fillers: {e:?}"));
        }
        assert_eq!(room.human_count(), max);
        assert!(room.filler_count() > 0, "fillers are present while humans pile in");

        // Human capacity is still enforced — the max_players+1-th HUMAN is rejected.
        let (outbound, _r, _s) = test_outbound();
        assert!(
            matches!(
                room.try_add_player("u_extra".into(), "extra".into(), outbound),
                Err(RoomJoinRejected::Full)
            ),
            "human capacity is enforced against humans, not total entities"
        );
    }

    #[test]
    fn end_game_rewards_exclude_fillers_and_clears_them_for_gc() {
        let mut room = make_filler_room(14);
        let (outbound, _r, _s) = test_outbound();
        let (human_id, _) = room.try_add_player("u_real".into(), "real".into(), outbound).unwrap();

        // Make a FILLER the score leader: it may top the scoreboard, but the OFFICIAL
        // GameEnded winner is the best human (lead review P1 — a hidden bot must not
        // steal the final result), and no reward may credit a filler account.
        let filler_id =
            room.players.values().find(|p| p.is_filler()).map(|p| p.id.clone()).expect("room has fillers");
        room.players.get_mut(&filler_id).unwrap().score = 9999;

        room.tick = room.game_end_tick - 1;
        let rewards = room.update().expect("game ends at game_end_tick");

        assert_eq!(rewards.len(), 1, "exactly the one human gets a reward row");
        assert_eq!(rewards[0].user_id, "u_real");
        assert!(
            rewards.iter().all(|r| !r.user_id.starts_with("filler:")),
            "no reward row may carry a filler account"
        );
        assert!(
            rewards[0].is_winner,
            "the official winner is the best HUMAN even when a filler out-scored them"
        );

        // Fillers evaporate with the match so the manager's `inactive && player_count()==0`
        // GC can collect the room once the human's socket closes.
        assert_eq!(room.filler_count(), 0, "ended room must hold no fillers");
        assert!(room.players.contains_key(&human_id), "the human is untouched by the sweep");
    }

    #[test]
    fn filler_brains_steer_their_players_over_time() {
        let mut room = make_filler_room(15);
        let (outbound, _r, _s) = test_outbound();
        room.try_add_player("u_h".into(), "h".into(), outbound).unwrap();
        // Every Player spawns facing Right; brains must issue REAL turns through the same
        // set_direction path a human MoveCommand uses. With first thoughts staggered over
        // 10..90 ticks and replans after, 400 ticks is plenty — and the seed is fixed, so
        // this is deterministic, not a flake.
        for _ in 0..400 {
            room.update();
        }
        let turned = room
            .players
            .values()
            .filter(|p| p.is_filler())
            .filter(|p| p.desired_direction != Direction::Right || p.direction != Direction::Right)
            .count();
        assert!(turned > 0, "at least one filler must have steered within 400 ticks");
    }

    #[test]
    fn human_join_burst_does_not_exceed_transient_population_cap_for_long() {
        let mut room = make_filler_room(17);
        let max = room.config.room.max_players;
        let cap = room.config.filler.max_transient_visible_players;

        // Whole-room burst: every human seat taken with no ticks in between.
        let mut keep_alive = Vec::new();
        for i in 0..max {
            let (outbound, r, s) = test_outbound();
            keep_alive.push((r, s));
            room.try_add_player(format!("u{i}"), format!("h{i}"), outbound).unwrap();
        }
        let overflow = room.player_count().saturating_sub(cap);
        assert!(overflow > 0, "burst must actually overfill for this test to bite");
        assert!(
            room.scheduled_filler_exits.iter().filter(|e| e.expedited).count() >= overflow,
            "the overflow must be covered by EXPEDITED exits"
        );
        assert!(
            room.player_count() > cap,
            "nobody leaves at the join tick — the cap shrinks via short delays, not snaps"
        );

        // Expedited band tops out well under a second; one second later the room obeys
        // the cap and humans kept every seat.
        for _ in 0..60 {
            room.update();
        }
        assert!(
            room.player_count() <= cap,
            "population {} must be back under the transient cap {} within a second",
            room.player_count(),
            cap
        );
        assert_eq!(room.human_count(), max, "humans never lose seats to the cap");
    }

    /// Map corner with the largest min-distance to any human — a guaranteed "offscreen"
    /// parking spot for exit/visibility scenarios.
    fn far_corner_from_humans(room: &Room) -> Position {
        let corners = [
            Position { x: 5.0, y: 5.0 },
            Position { x: room.map.width - 5.0, y: 5.0 },
            Position { x: 5.0, y: room.map.height - 5.0 },
            Position { x: room.map.width - 5.0, y: room.map.height - 5.0 },
        ];
        let min_dist_to_humans = |c: &Position| {
            room.players
                .values()
                .filter(|p| !p.is_filler())
                .map(|p| ((p.position.x - c.x).powi(2) + (p.position.y - c.y).powi(2)).sqrt())
                .fold(f32::MAX, f32::min)
        };
        corners
            .into_iter()
            .max_by(|a, b| {
                min_dist_to_humans(a).partial_cmp(&min_dist_to_humans(b)).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap()
    }

    /// Pin a player to `pos` for the next update: position + speed 0. Re-pin EVERY tick —
    /// the sim clamps positions into the field and a booster pickup silently restores
    /// speed, so a one-shot "freeze" quietly un-freezes mid-scenario.
    fn pin(room: &mut Room, id: &str, pos: Position) {
        let p = room.players.get_mut(id).unwrap();
        p.speed = 0.0;
        p.position = pos;
    }

    #[test]
    fn due_exit_postpones_while_a_human_is_watching() {
        let mut room = make_filler_room(18);
        let (ob1, _r1, _s1) = test_outbound();
        let (h1, _) = room.try_add_player("u_h1".into(), "h1".into(), ob1).unwrap();
        let (ob2, _r2, _s2) = test_outbound();
        room.try_add_player("u_h2".into(), "h2".into(), ob2).unwrap();
        assert_eq!(room.scheduled_filler_exits.len(), 1);
        let leaver_id = room.scheduled_filler_exits[0].player_id.clone();
        let exit_at = room.scheduled_filler_exits[0].at_tick;

        // Hold the leaver right next to human #1 through the due tick.
        let h_pos = room.players[&h1].position;
        let watched = Position { x: h_pos.x + 30.0, y: h_pos.y };
        while room.tick <= exit_at {
            pin(&mut room, &h1, h_pos);
            pin(&mut room, &leaver_id, watched);
            room.update();
        }
        assert!(
            room.players.contains_key(&leaver_id),
            "a leaver in a human's view must be POSTPONED, not evaporate on-screen"
        );
        let e =
            room.scheduled_filler_exits.iter().find(|e| e.player_id == leaver_id).expect("exit re-scheduled");
        assert!(e.postpones >= 1, "the deferral is tracked");

        // Hold the leaver out of sight instead: the retried exit goes through.
        let away = far_corner_from_humans(&room);
        for _ in 0..(FILLER_EXIT_POSTPONE_MAX_TICKS as u64 + 5) {
            if room.players.contains_key(&leaver_id) {
                pin(&mut room, &leaver_id, away);
            }
            room.update();
        }
        assert!(!room.players.contains_key(&leaver_id), "offscreen, the exit completes");
    }

    #[test]
    fn empty_room_sweeps_fillers_after_grace_and_deactivates() {
        let mut room = make_filler_room(19);
        let (outbound, _r, _s) = test_outbound();
        let (human_id, _) = room.try_add_player("u_h".into(), "h".into(), outbound).unwrap();
        assert!(room.filler_count() > 0);

        room.remove_player(&human_id);
        let grace = room.config.filler.empty_room_grace_ticks;
        for _ in 0..(grace + FILLER_REBALANCE_INTERVAL_TICKS + 5) {
            room.update();
        }
        assert_eq!(room.filler_count(), 0, "an uninhabited room sweeps its fillers");
        assert!(!room.is_active, "and deactivates so the manager can GC it");
        assert_eq!(room.player_count(), 0, "GC precondition (inactive + empty) holds");
    }

    #[test]
    fn room_near_match_end_stops_accepting_new_joins() {
        let mut room = make_filler_room(20);
        assert!(room.accepting_new_joins(), "a fresh room is joinable");
        room.tick = room.game_end_tick - MIN_JOINABLE_REMAINING_TICKS + 1;
        assert!(!room.accepting_new_joins(), "a match about to end must not receive new players");
        assert!(room.is_active, "the gate is about joining, not about the room dying");
    }

    /// The race the manager-side filter can't close (lead review): the match crosses
    /// the min-remaining threshold BETWEEN the manager's read-lock pick and the join's
    /// write lock. try_add_player must re-check under the SAME &mut turn that inserts —
    /// exactly like the capacity check — so nobody is ever seated seconds before
    /// GameEnded. Resumes don't enter through try_add_player and stay unaffected.
    #[test]
    fn try_add_player_rejects_new_join_when_match_crossed_min_remaining_under_write_lock() {
        let mut room = make_filler_room(23);
        // Simulate the threshold crossing after the (stale) manager check: the room is
        // still active and has space — only the remaining time disqualifies it.
        room.tick = room.game_end_tick - MIN_JOINABLE_REMAINING_TICKS + 1;
        assert!(room.is_active && room.has_space());

        let (outbound, _r, _s) = test_outbound();
        assert!(
            matches!(
                room.try_add_player("u_late".into(), "late".into(), outbound),
                Err(RoomJoinRejected::TooLateInMatch)
            ),
            "the write-lock re-check must reject the late join"
        );
        assert_eq!(room.human_count(), 0, "nothing was inserted");
        assert_eq!(room.filler_count(), 0, "no filler top-up fired for a rejected join");
    }

    #[test]
    fn replacement_filler_spawns_clear_of_humans() {
        let mut room = make_filler_room(22);
        let target = room.filler_target();
        // Two humans (one over target → an exit), then human #1 leaves → deficit →
        // a runtime replacement spawn fires later.
        let (ob1, _r1, _s1) = test_outbound();
        let (h1, _) = room.try_add_player("u_h1".into(), "h1".into(), ob1).unwrap();
        let (ob2, _r2, _s2) = test_outbound();
        let (h2, _) = room.try_add_player("u_h2".into(), "h2".into(), ob2).unwrap();
        room.remove_player(&h1);

        // Freeze the survivor so "clear of humans" is measured against a fixed point.
        room.players.get_mut(&h2).unwrap().speed = 0.0;
        let known_before: HashSet<String> = room.players.keys().cloned().collect();
        let mut new_filler: Option<String> = None;
        for _ in 0..1200 {
            room.update();
            if let Some(id) = room
                .players
                .values()
                .find(|p| p.is_filler() && !known_before.contains(&p.id))
                .map(|p| p.id.clone())
            {
                new_filler = Some(id);
                break;
            }
        }
        let new_filler = new_filler.expect("a replacement filler eventually spawns");
        let h_pos = room.players[&h2].position;
        let f_pos = room.players[&new_filler].position;
        let d = ((h_pos.x - f_pos.x).powi(2) + (h_pos.y - f_pos.y).powi(2)).sqrt();
        assert!(
            d >= FILLER_SPAWN_MIN_HUMAN_DIST_PX,
            "replacement spawned {d:.0}px from the human — must be >= {FILLER_SPAWN_MIN_HUMAN_DIST_PX}"
        );
        let _ = target;
    }

    // ---- internal eats (lead review P0 #1) ----------------------------------------------

    /// Freeze every entity (speed 0) and silence wandering so eat scenarios control
    /// geometry exactly; returns a room with one claim-ready human well past the
    /// post-admission gate.
    fn eat_arena(seed: u64) -> (Room, String, mpsc::Receiver<ServerMessage>) {
        let mut room = make_filler_room(seed);
        let (outbound, reliable_rx, _snap) = test_outbound();
        let (human_id, _) = room.try_add_player("u_prey".into(), "prey".into(), outbound).unwrap();
        for p in room.players.values_mut() {
            p.speed = 0.0;
        }
        // Trusted world + clean clock + shield expired — the gates a real client earns.
        {
            let h = room.players.get_mut(&human_id).unwrap();
            h.world_ready = true;
            h.claim_ready = true;
        }
        // Sail past MIN_CLAIM_AFTER_ADMISSION_TICKS with everyone frozen in place.
        for _ in 0..(MIN_CLAIM_AFTER_ADMISSION_TICKS + 10) {
            room.update();
        }
        (room, human_id, reliable_rx)
    }

    /// A walkable position outside every safe zone (eats are blocked inside them).
    fn open_position(room: &mut Room) -> Position {
        for _ in 0..64 {
            let pos = room.map.get_random_spawn_position(&mut room.rng);
            if !room.map.is_in_safe_zone(&pos) {
                return pos;
            }
        }
        panic!("no open position found");
    }

    fn strongest_filler_id(room: &Room) -> String {
        room.players
            .values()
            .filter(|p| p.is_filler())
            .max_by_key(|p| p.score)
            .map(|p| p.id.clone())
            .expect("room has fillers")
    }

    fn overlap(room: &mut Room, attacker_id: &str, victim_id: &str, at: Position) {
        room.players.get_mut(attacker_id).unwrap().position = at;
        room.players.get_mut(victim_id).unwrap().position = Position { x: at.x + 4.0, y: at.y };
    }

    #[test]
    fn filler_eats_weaker_claim_ready_human_after_sustained_overlap() {
        let (mut room, human_id, mut rx) = eat_arena(31);
        let attacker_id = strongest_filler_id(&room);
        room.players.get_mut(&attacker_id).unwrap().score = 500;
        room.players.get_mut(&human_id).unwrap().score = 10;
        let spot = open_position(&mut room);
        let life_before = room.players[&human_id].life_id;
        let attacker_score_before = 500;

        // Hold the overlap; positions are re-pinned each tick (speed 0 keeps them put).
        for _ in 0..(FILLER_EAT_SUSTAINED_TICKS as u64 + 4) {
            overlap(&mut room, &attacker_id, &human_id, spot);
            room.update();
        }

        let h = &room.players[&human_id];
        assert_eq!(h.life_id, life_before + 1, "the human died exactly once and respawned");
        assert!(
            room.players[&attacker_id].score > attacker_score_before,
            "the eater absorbed the victim's score"
        );
        // The victim's client got the SAME wire fact a human-claim eat produces.
        let mut saw_eaten = false;
        while let Ok(msg) = rx.try_recv() {
            if let ServerMessage::Event(ev) = msg {
                if format!("{ev:?}").contains("PlayerEaten") {
                    saw_eaten = true;
                }
            }
        }
        assert!(saw_eaten, "PlayerEaten must reach the victim's reliable lane");
    }

    #[test]
    fn filler_cannot_eat_human_before_claim_ready() {
        let (mut room, human_id, _rx) = eat_arena(32);
        let attacker_id = strongest_filler_id(&room);
        room.players.get_mut(&attacker_id).unwrap().score = 500;
        {
            let h = room.players.get_mut(&human_id).unwrap();
            h.score = 10;
            h.claim_ready = false; // client never proved its clock/world
        }
        let spot = open_position(&mut room);
        let life_before = room.players[&human_id].life_id;

        for _ in 0..(FILLER_EAT_SUSTAINED_TICKS as u64 * 4) {
            overlap(&mut room, &attacker_id, &human_id, spot);
            room.update();
        }
        assert_eq!(
            room.players[&human_id].life_id, life_before,
            "a not-claim-ready human must be uneatable by fillers"
        );
    }

    #[test]
    fn filler_cannot_eat_inside_safe_zone() {
        let (mut room, human_id, _rx) = eat_arena(33);
        let attacker_id = strongest_filler_id(&room);
        room.players.get_mut(&attacker_id).unwrap().score = 500;
        room.players.get_mut(&human_id).unwrap().score = 10;
        let zone_center = room.map.safe_zones.first().expect("map has safe zones").center;
        let life_before = room.players[&human_id].life_id;

        for _ in 0..(FILLER_EAT_SUSTAINED_TICKS as u64 * 4) {
            overlap(&mut room, &attacker_id, &human_id, zone_center);
            room.update();
        }
        assert_eq!(
            room.players[&human_id].life_id, life_before,
            "safe zones must block filler eats exactly like claim eats"
        );
    }

    /// Ledger/claim-path parity (lead review): a filler kill records the victim's life
    /// as dead exactly like an accepted human claim does — so a STALE human eat claim
    /// referencing the pre-kill life rejects (life mismatch / kill-by-corpse) instead
    /// of double-killing the freshly respawned victim. Exactly one death total.
    #[test]
    fn filler_kill_records_one_life_kill_and_stale_human_claim_rejects() {
        let (mut room, prey_id, _prey_rx) = eat_arena(35);
        // A second claim-ready human who will file the STALE claim.
        let (ob2, mut atk_rx, _s2) = test_outbound();
        let (atk_id, _) = room.try_add_player("u_atk".into(), "atk".into(), ob2).unwrap();
        {
            let a = room.players.get_mut(&atk_id).unwrap();
            a.speed = 0.0;
            a.world_ready = true;
            a.claim_ready = true;
            a.score = 300;
        }
        room.players.get_mut(&prey_id).unwrap().score = 10;
        let killer_id = strongest_filler_id(&room);
        room.players.get_mut(&killer_id).unwrap().score = 500;
        // This room has TWO humans → it's over the filler target, so the rebalance keeps
        // scheduling DELAYED filler exits. Across this test's ~80 ticks one of those would
        // fire and remove the killer filler mid-scenario — a real flake (passed locally,
        // failed in CI). The scenario is about the EAT/ledger path, not lifecycle, so
        // suppress the exit churn deterministically: clear the exit queue every tick.
        for _ in 0..(MIN_CLAIM_AFTER_ADMISSION_TICKS + 5) {
            room.scheduled_filler_exits.clear();
            room.update();
        }

        // Phase 1: attacker visibly overlaps the prey for a few ticks so contact
        // history records the life-0 era both claim legs will reference.
        let spot = open_position(&mut room);
        let atk_spot = Position { x: spot.x + 4.0, y: spot.y };
        for _ in 0..4 {
            room.scheduled_filler_exits.clear();
            pin(&mut room, &prey_id, spot);
            pin(&mut room, &atk_id, atk_spot);
            // Keep the filler far away while history is being written.
            let far = far_corner_from_humans(&room);
            pin(&mut room, &killer_id, far);
            room.update();
        }
        let stale_tick = room.tick as f64 - 1.0; // life-0 era, fresh enough to process
        let life_before = room.players[&prey_id].life_id;

        // Phase 2: the attacker walks off; the filler takes the kill.
        let away = far_corner_from_humans(&room);
        for _ in 0..(FILLER_EAT_SUSTAINED_TICKS as u64 + 4) {
            room.scheduled_filler_exits.clear();
            pin(&mut room, &prey_id, spot);
            pin(&mut room, &atk_id, away);
            pin(&mut room, &killer_id, Position { x: spot.x + 3.0, y: spot.y });
            room.update();
            if room.players[&prey_id].life_id > life_before {
                break;
            }
        }
        assert_eq!(
            room.players[&prey_id].life_id,
            life_before + 1,
            "the filler killed the prey exactly once"
        );
        let atk_score_before = room.players[&atk_id].score;

        // Phase 3: the STALE human claim against the dead life arrives.
        room.apply_command(RoomCommand::EatClaim(EatClaimInput {
            player_id: atk_id.clone(),
            claim: EatClaim {
                claim_id: 1,
                target_kind: EatTargetKind::Player,
                target_id: prey_id.clone(),
                attacker_render_tick: stale_tick,
                target_render_tick: stale_tick,
                attacker_position: atk_spot,
                target_position: spot,
                visual_distance: 4.0,
                target_generation: None,
            },
        }));
        room.update();

        assert_eq!(
            room.players[&prey_id].life_id,
            life_before + 1,
            "no second death: the stale claim must not kill the respawned life"
        );
        assert_eq!(room.players[&atk_id].score, atk_score_before, "no score from a rejected claim");
        let mut rejects = Vec::new();
        while let Ok(msg) = atk_rx.try_recv() {
            if let ServerMessage::EatClaimRejected { claim_id, reason, .. } = msg {
                rejects.push((claim_id, reason));
            }
        }
        assert!(
            rejects
                .iter()
                .any(|(id, r)| *id == 1
                    && (r.contains("life") || r.contains("corpse") || r.contains("consumed"))),
            "the stale claim must reject against the dead life (got {rejects:?})"
        );
    }

    /// Attacker-side gate (lead review): a filler eaten by ANOTHER filler earlier in
    /// the same internal-eat pass respawns with a new life — and the respawned life
    /// must not inherit the dead life's attack slot in that same pass.
    #[test]
    fn filler_respawned_this_tick_cannot_eat_later_same_tick() {
        let (mut room, _human, _rx) = eat_arena(36);
        // Three fillers by SORTED id: B (first) eats A (second); A had its own kill
        // lined up on C (third). After B's kill, A's slot must be skipped.
        let mut ids: Vec<String> =
            room.players.values().filter(|p| p.is_filler()).map(|p| p.id.clone()).collect();
        ids.sort();
        let (b, a, c) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        room.players.get_mut(&b).unwrap().score = 1000;
        room.players.get_mut(&a).unwrap().score = 500;
        room.players.get_mut(&c).unwrap().score = 10;

        let spot = open_position(&mut room);
        let a_life0 = room.players[&a].life_id;
        // Chain: B—A 4px, A—C 4px (both runs accumulate over the same ticks and reach
        // the sustained threshold on the same tick — the exact race under test).
        for _ in 0..(FILLER_EAT_SUSTAINED_TICKS as u64 + 6) {
            pin(&mut room, &b, spot);
            if room.players[&a].life_id == a_life0 {
                pin(&mut room, &a, Position { x: spot.x + 4.0, y: spot.y });
            }
            pin(&mut room, &c, Position { x: spot.x + 8.0, y: spot.y });
            room.update();
        }

        assert_eq!(room.players[&a].life_id, a_life0 + 1, "B ate A exactly once");
        assert_eq!(
            room.players[&c].life_id, 0,
            "C survives: the respawned A must not fire the dead life's queued attack"
        );
    }

    #[test]
    fn filler_eats_weaker_filler_for_background_life() {
        let (mut room, _human_id, _rx) = eat_arena(34);
        let attacker_id = strongest_filler_id(&room);
        let victim_id = room
            .players
            .values()
            .filter(|p| p.is_filler() && p.id != attacker_id)
            .map(|p| p.id.clone())
            .next()
            .expect("at least two fillers");
        room.players.get_mut(&attacker_id).unwrap().score = 500;
        room.players.get_mut(&victim_id).unwrap().score = 5;
        let spot = open_position(&mut room);
        let life_before = room.players[&victim_id].life_id;

        for _ in 0..(FILLER_EAT_SUSTAINED_TICKS as u64 + 4) {
            overlap(&mut room, &attacker_id, &victim_id, spot);
            room.update();
        }
        assert_eq!(
            room.players[&victim_id].life_id,
            life_before + 1,
            "fillers eat each other — no client-readiness gates apply between bots"
        );
    }

    #[test]
    fn room_tops_back_up_after_human_leaves() {
        let mut room = make_filler_room(16);
        let target = room.filler_target();

        // Two humans: the second over-fills, ONE filler exit gets committed.
        let (ob1, _r1, _s1) = test_outbound();
        let (h1, _) = room.try_add_player("u_h1".into(), "h1".into(), ob1).unwrap();
        let (ob2, _r2, _s2) = test_outbound();
        room.try_add_player("u_h2".into(), "h2".into(), ob2).unwrap();
        assert_eq!(room.scheduled_filler_exits.len(), 1);

        // Human #1 leaves before the scheduled exit fires. The exit still happens (it's
        // committed — a real player wouldn't un-quit), then the throttled rebalance queues
        // a DELAYED replacement and the population drifts back to target.
        room.remove_player(&h1);
        for _ in 0..800 {
            room.update();
            if room.human_count() + room.filler_count() == target && room.scheduled_filler_exits.is_empty() {
                break;
            }
        }
        assert_eq!(room.human_count(), 1);
        assert_eq!(
            room.human_count() + room.filler_count(),
            target,
            "population must drift back to target after a human leaves"
        );
    }
}
