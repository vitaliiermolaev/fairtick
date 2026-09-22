use serde::{Deserialize, Serialize};

/// Messages sent from client to server.
///
/// Stage 0.5: Hello must be first. Server rejects everything else until Welcome.
/// Stage 2:   old `Move { direction }` removed in favour of `MoveCommand { seq, client_tick, direction }`.
///            `SignInWithUserId` removed — clients must use `SignInWithApple` with a nonce.
/// Stage 2.5: `Ping { client_send_time_ms }` enables client-side RTT/offset.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum ClientMessage {
    Hello {
        protocol_version: u32,
        client_build: String,
        platform: String,
        config_hash: String,
        maze_hash: String,
        /// DEBUG metadata (engineer wave 7): ties the server's conn_id to the client's
        /// connect-attempt chain even when the client's own buffered logs die with the
        /// socket. Echoed into hello_received / welcome_sent / connection_closed. All
        /// `#[serde(default)]` — an old client simply reads as attempt 0 / empty.
        #[serde(default)]
        connect_attempt_id: u32,
        #[serde(default)]
        client_session_id: String,
        #[serde(default)]
        resume_intent: bool,
    },

    SignInWithApple {
        apple_token: String,
        /// Raw nonce (Stage 8 verifies SHA256(nonce) matches Apple JWT's nonce claim).
        /// Required at the wire level from Stage 2; full validation lands in Stage 8.
        nonce: String,
    },
    SetNickname {
        nickname: String,
    },

    JoinGame,
    LeaveGame,

    MoveCommand(MoveCommand),

    /// Player-initiated eating: the attacker's client saw its avatar visually
    /// overlap a target on screen and asks the server to score the eat. The
    /// server validates the claim against contact history; it never eats another
    /// player on its own current-tick collision (which diverges from what the
    /// attacker/victim actually saw).
    EatClaim(EatClaim),

    /// Victim-side claim that an enemy ate the LOCAL player (the claim-based death path,
    /// symmetric with [`EatClaim`]). See [`EnemyDeathClaim`].
    EnemyDeathClaim(EnemyDeathClaim),

    /// Client-side "drove through it and lived" probe (OBSERVE-ONLY). When the client logs a
    /// `visual_overlap_without_death` (a sustained on-screen overlap with an enemy that did not
    /// kill it), it sends this so the server can log ITS view of that moment
    /// (`visual_overlap_probe_server_result`) — server distance now / at the named tick, contact
    /// run, the reconstructed + projected legs, and why no kill. Correlates the two sides; the
    /// server computes + logs only, changes no gameplay. (lead manifesto 2026-06-06)
    VisualOverlapProbe {
        enemy_id: String,
        enemy_generation: u32,
        known_server_tick: u64,
    },

    Ping {
        client_send_time_ms: u64,
    },

    /// Client asks for an immediate full keyframe (e.g. after app resume, when its
    /// prediction has fallen too far behind to safely catch up by stepping).
    ///
    /// `request_id` is a client-generated correlation id, echoed back verbatim in the
    /// reply keyframe (`GameStateUpdate::full_state_request_id`). It exists because a
    /// thawed iOS socket replays its receive backlog: without the echo the client can
    /// never PROVE whether a seq=0 keyframe answers the CURRENT request or is an
    /// expired reply to an earlier one (the `snapshot_seq == 0` + min-accept-tick
    /// heuristic is a freshness guess, not a correlation). 0 = legacy client that
    /// sends none (serde default) → the server echoes `None`.
    RequestFullState {
        reason: String,
        #[serde(default)]
        request_id: u64,
    },

    /// A batch of client-side telemetry events, flushed periodically and on
    /// important events. The server stamps its own conn/room/player identity from
    /// the connection (it never trusts the client's claimed ids) and folds each
    /// entry into the unified match telemetry stream.
    ClientLogBatch(ClientLogBatch),

    PlayAgain,
    ReturnToMenu,

    /// Reconnect after an unexpected socket drop: the client presents the `resume_token` it
    /// got in GameJoined and asks to be put back into the SAME room/match with its score,
    /// instead of a fresh JoinGame. The server holds the slot for `reconnect_grace_sec` after
    /// the drop; within that window this succeeds (GameJoined + a full keyframe), otherwise it
    /// replies ResumeRejected and the client falls back to JoinGame.
    ///
    /// Appended at the END of the enum on purpose: ClientMessage derives bincode, which keys
    /// variants by ordinal, so a new variant must not shift existing discriminants. (The live
    /// transport is JSON/serde, externally tagged by name; this keeps the binary path stable
    /// too.) A server that predates this variant can't parse it — it returns Error, it does
    /// NOT silently ignore it; client+server ship together.
    Resume {
        resume_token: String,
    },

    /// "My world is trusted now" — sent once per admission (join/resume/bootstrap) when the
    /// client's world-sync gate releases: fresh anchor applied, live snapshots flowing, sane
    /// render-vs-server lag. The SERVER-SIDE safety net for the proven 2026-06-10 ghost
    /// death: a death claim from a player whose connection has NOT yet declared world-ready
    /// is rejected (`world_not_ready`) — the victim cannot honestly have SEEN a kill in a
    /// world it itself does not trust yet. Readiness is per-admission state on the server
    /// (reset every join/resume), so a stale declaration from a previous life can't leak.
    /// `world_sync_epoch` / `anchor_tick` are diagnostic — they correlate with the client's
    /// client_world_sync_complete event. Appended at the END of the enum (see Resume).
    ClientWorldReady {
        world_sync_epoch: u32,
        anchor_tick: u64,
    },

    /// "My CLAIMS can be trusted now" — the STRICTER second stage of the readiness split
    /// (lead manifesto 2026-06-11): visual world assembled (ClientWorldReady above) AND the
    /// clock re-proven by a clean post-cutoff pong AND the resume admission shield expired.
    /// The server honours death/eat claims only after THIS arrives (`claim_ready_not_seen`
    /// otherwise); ClientWorldReady remains the weaker visual fact (overlay/UX/telemetry).
    /// Appended at the END of the enum (see Resume).
    ClientClaimReady {
        world_sync_epoch: u32,
        anchor_tick: u64,
    },

    /// Reconnect/relaunch auth with the SERVER-ISSUED session token from a previous
    /// verified sign-in (AuthSuccess.session_token) — no repeat of the native
    /// provider (Face ID) flow. Provider-agnostic: the token carries no provider.
    /// Server: unknown token → AuthFailed("Invalid session"); the client clears its
    /// stored token and falls back to the provider sign-in. Appended at the END of
    /// the enum (bincode ordinal append-only rule).
    SignInWithSession {
        session_token: String,
    },
}

/// One flush of the client's ring buffer. `dropped` reports how many events the
/// client's buffer evicted since the last flush (so a gap is visible, not silent).
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct ClientLogBatch {
    pub session_id: String,
    pub flush_reason: String,
    pub dropped: u32,
    /// Client log generation — bumped on each join. Lets analysis tell apart
    /// events from different matches/connections within one session, and the
    /// client drops stale older-generation events before they're flushed.
    #[serde(default)]
    pub gen: u32,
    pub entries: Vec<ClientLogEntry>,
}

/// A single client telemetry event. `fields_json` is a JSON object encoded as a
/// string (simpler over the wire than a dynamic map); the server parses it back
/// into a real object, or stores it raw if it doesn't parse.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct ClientLogEntry {
    pub seq: u64,
    pub client_unix_ms: i64,
    pub client_mono_ms: i64,
    pub level: String,
    pub event_name: String,
    pub message: String,
    pub room_id: String,
    pub player_id: String,
    pub server_tick: i64,
    pub client_tick: i64,
    pub render_tick: f64,
    pub fields_json: String,
}

/// Single turn intent. `seq` is monotonic so the server can ignore
/// retransmitted/out-of-order inputs. `target_tick` is the server tick at which
/// the client wants this input APPLIED — the server buffers the command and
/// applies it when its simulation reaches `target_tick`, so client prediction
/// and server authority turn on the same tick (no "snake" from receive-time
/// application). The client lives a few ticks ahead so the command arrives in
/// time; a late command is applied as soon as it arrives.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct MoveCommand {
    pub seq: u32,
    pub target_tick: u64,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum EatTargetKind {
    Player,
    Enemy,
}

/// Stable codes for the enemy-death decision (vs. a free-form string). The wire event
/// `PlayerKilledByEnemy` carries one of the two `KillNow*` codes (a defer/suppress produces no
/// death); the full set is the contract telemetry/replay key off — so a deferral or a
/// suppression is unambiguous, not a guessed string. `snake_case` on the JSON wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
#[serde(rename_all = "snake_case")]
pub enum DeathDecisionCode {
    /// Visible gate ARMED: server contact confirmed AND the victim-visible timeline confirmed
    /// contact (reconstructed enemy-visible distance ≤ the confirm radius) → killed.
    KillNowVisibleConfirmed,
    /// Visible gate DISABLED (observe-only): killed on server-truth contact alone. The visible
    /// timeline was reconstructed + logged but did NOT gate the kill — so this death must NOT be
    /// read as visibly-confirmed. `visible_confirm_radius_px` is `None` for this code.
    KillNowServerOnlyObserveOnly,
    /// Claim-based death (lead manifesto): the VICTIM'S client claimed the kill and the server
    /// confirmed it by rewinding to the victim's reported render ticks (the honest path — no
    /// server-side guessing of what the phone drew).
    KillNowClaimConfirmed,
    /// v6 ANTI-CHEAT FALLBACK: a lethal, visibly-confirmed server contact persisted past the
    /// sustained-missing-claim threshold with NO client EnemyDeathClaim ever arriving (withholding
    /// / broken / cheating client). The server killed on its own authority. MUST be distinguishable
    /// from `KillNowClaimConfirmed` (honest claim) and `KillNowVisibleConfirmed` (the legacy
    /// pre-v6 server kill) in logs/replays. (lead review round-3)
    KillNowClaimMissingSustained,
    /// Server confirmed, but the reconstructed enemy-visible distance is beyond the confirm
    /// radius (the player hasn't seen contact) → held.
    DelayVisibleGap,
    /// Server confirmed, but the visible timeline couldn't be reconstructed (no/old history,
    /// generation mismatch) → held rather than trusting server-current.
    DelayNoVisibleHistory,
    /// v5: both visible legs nominally confirm, but the kill leans hard on the constant-velocity
    /// victim projection at a turn-sensitive lead (which goes stale at a turn) → held this tick.
    DelayPredictionUncertain,
    /// A built candidate dropped because server contact broke before visible confirmation.
    SuppressContactBroken,
    /// Contact ignored because the victim is shielded (safe zone / spawn-protect / invincible).
    SuppressShielded,
    /// Server contact still building toward the threshold.
    Building,
}

impl DeathDecisionCode {
    pub fn as_str(self) -> &'static str {
        match self {
            DeathDecisionCode::KillNowVisibleConfirmed => "kill_now_visible_confirmed",
            DeathDecisionCode::KillNowServerOnlyObserveOnly => "kill_now_server_only_observe_only",
            DeathDecisionCode::KillNowClaimConfirmed => "kill_now_claim_confirmed",
            DeathDecisionCode::KillNowClaimMissingSustained => "kill_now_claim_missing_sustained",
            DeathDecisionCode::DelayVisibleGap => "delay_visible_gap",
            DeathDecisionCode::DelayNoVisibleHistory => "delay_no_visible_history",
            DeathDecisionCode::DelayPredictionUncertain => "delay_prediction_uncertain",
            DeathDecisionCode::SuppressContactBroken => "suppress_contact_broken",
            DeathDecisionCode::SuppressShielded => "suppress_shielded",
            DeathDecisionCode::Building => "building",
        }
    }
}

/// "On my screen my avatar visually overlapped `target`, and I can eat it."
/// The attacker reports the render ticks and visible positions it actually drew;
/// the server reconstructs those from its contact history and accepts only if the
/// claim is consistent and within the latency window.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct EatClaim {
    pub claim_id: u32,
    pub target_kind: EatTargetKind,
    pub target_id: String,
    /// Tick at which the attacker rendered its own local avatar (fractional —
    /// Unity renders between predicted ticks).
    pub attacker_render_tick: f64,
    /// Tick at which the attacker rendered the target (RemoteEntity.LastRenderTick).
    pub target_render_tick: f64,
    /// Server-space position converted from the attacker's visible world position.
    pub attacker_position: Position,
    /// Server-space position converted from the target's visible world position.
    pub target_position: Position,
    /// On-screen distance between attacker_position and target_position.
    pub visual_distance: f32,
    /// Generation of the enemy life the client believes it saw (`EnemyState.generation`
    /// / the last `EnemyRespawned.generation`). Enemy ids are REUSED across respawns, so
    /// this pins the claim to one life. `None` = legacy client that doesn't track it; the
    /// server then falls back to the respawned_at_tick/target_floor stale-claim guard.
    /// `Some(g)` is validated strictly: a mismatch against the live enemy is rejected
    /// (`enemy_generation_mismatch`) — a claim against a since-respawned enemy. Only
    /// meaningful for `target_kind == Enemy`.
    #[serde(default)]
    pub target_generation: Option<u32>,
}

/// "On my screen an enemy visually overlapped MY avatar and is big enough to eat me."
/// The SYMMETRIC counterpart of [`EatClaim`] for the enemy-eats-player direction: instead of
/// the server guessing what the victim's phone drew (the v4/v5 projection), the victim's client
/// reports the render ticks + visible positions it actually drew, and the server reconstructs
/// those from contact history and accepts only if consistent and within the latency window.
/// The client predicts its own death immediately; the server confirms (PlayerRespawned) or
/// rejects (EnemyDeathClaimRejected). (lead manifesto 2026-06-06)
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct EnemyDeathClaim {
    pub claim_id: u32,
    pub enemy_id: String,
    /// Enemy life the victim believes it saw (ids are reused across respawns) — validated
    /// strictly against the live enemy, like `EatClaim.target_generation`.
    pub enemy_generation: u32,
    /// Tick at which the victim rendered its own local avatar (fractional).
    pub victim_render_tick: f64,
    /// Tick at which the victim rendered the killer enemy (RemoteEntity.LastRenderTick).
    pub enemy_render_tick: f64,
    /// Server-space position converted from the victim's visible world position.
    pub victim_position: Position,
    /// Server-space position converted from the enemy's visible world position.
    pub enemy_position: Position,
    /// On-screen distance between victim_position and enemy_position.
    pub visual_distance: f32,
}

/// Messages sent from server to client.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum ServerMessage {
    Welcome {
        protocol_version: u32,
        min_supported_build: String,
        server_config_hash: String,
        server_maze_hash: String,
        tick_rate: u64,
        /// Authoritative enemy-kill model, sent so the client logs the SERVER's
        /// real numbers (not a client guess): an enemy eats the player at
        /// `enemy_kill_radius_px` held for `enemy_kill_contact_ticks` ticks. This is
        /// SMALLER than the visual sprite radius by design (hitbox < sprite).
        #[serde(default)]
        enemy_kill_radius_px: f32,
        #[serde(default)]
        enemy_kill_contact_ticks: u32,
        /// Ticks the client should render a remote ENEMY behind server-now (interp delay).
        /// SINGLE SOURCE OF TRUTH for the enemy presentation delay: the server reconstructs
        /// the victim-visible timeline with this exact value, so client and server agree
        /// (closes the old 7-vs-8 drift). The client uses it as its enemy interp floor.
        #[serde(default)]
        enemy_interp_delay_ticks: u64,
        /// Visible-confirm radius (px): the server holds a confirmed enemy kill until the
        /// reconstructed enemy-visible distance is within this. SEPARATE from the kill radius
        /// — it's the sprite-overlap distance (≈18) + a small grace. Sent so the client's
        /// death debug can label the same threshold the server gated on.
        #[serde(default)]
        enemy_death_visible_confirm_radius_px: f32,
        /// Whether the visible-confirm gate is ARMED (`death_fairness.visible_gate_enabled`).
        /// When false (observe-only) the radius above is informational only — the server kills
        /// on server-truth contact alone. Sent explicitly so the client never mistakes a
        /// non-zero radius for an armed gate.
        #[serde(default)]
        enemy_death_visible_gate_enabled: bool,
        /// `death_fairness.policy_version` — which death-rule set is in force.
        #[serde(default)]
        enemy_death_policy_version: u32,
        /// Safe-zone circle (server space) so the client can gate its OWN death claims: you can't
        /// be eaten inside it, so the client shouldn't optimistic-die / claim there (v6 review #3).
        #[serde(default)]
        safe_zone_center_x: f32,
        #[serde(default)]
        safe_zone_center_y: f32,
        #[serde(default)]
        safe_zone_radius_px: f32,
        /// Max ticks the client predicts its OWN avatar ahead of server-now — the upper bound of the
        /// client's `ComputeLeadTicks` clamp. SINGLE SOURCE OF TRUTH with the server's death-claim
        /// fallback hold/future windows (both from `config.network.max_client_prediction_lead_ticks`),
        /// so an honest victim's near-future claim isn't pre-empted and the two sides can't drift.
        /// (round-3 follow-up)
        #[serde(default)]
        max_client_prediction_lead_ticks: u64,
        /// Post-respawn protection window (seconds) — `config.gameplay.spawn_protection_sec`. Sent so
        /// the client's respawn blink covers EXACTLY the server's invulnerability window from one
        /// source, instead of a hand-synced client constant. (review #3)
        #[serde(default)]
        spawn_protection_sec: f32,
        /// CLAIM visible-overlap radius (px) the server VALIDATES eat AND death claims against —
        /// `config.claim_fairness.visible_overlap_radius_px`. Sent so the client CREATES a claim using
        /// the exact same number the server checks, instead of a hardcoded 18f that could drift. NOT
        /// eat-only and NOT the rendered sprite size (presentation is a separate invariant). 0 = old
        /// server → client keeps its built-in default. (arch review Blocker 3 — generalized name)
        #[serde(default)]
        claim_visible_overlap_radius_px: f32,
        /// Resume admission shield window (ticks) — `RESUME_SHIELD_TICKS`. For this long after
        /// a RESUME admission the server suppresses the player's deaths AND eats (symmetric
        /// grace); sent so the client's claim gates mirror the exact same window from one
        /// source (`suppress_resume_shield`) instead of a hand-synced constant. 0 = old server
        /// → client keeps its built-in default. (lead manifesto 2026-06-11 P0 #1)
        #[serde(default)]
        resume_shield_ticks: u64,
    },

    AuthSuccess {
        user_id: String,
        nickname: String,
        crystals: i64,
        /// Server-issued long-lived session token, present on a fresh PROVIDER sign-in
        /// (store it; reconnect with SignInWithSession). None on session-based auths —
        /// the client already holds the token it just used. Additive field with a
        /// default so older clients/fixtures parse unchanged.
        #[serde(default)]
        session_token: Option<String>,
    },
    AuthFailed {
        reason: String,
    },
    NicknameSet {
        nickname: String,
    },
    NicknameUnavailable,

    GameJoined {
        room_id: String,
        /// Server-assigned per-connection player id. The client must use this,
        /// NOT nickname, to identify itself in subsequent snapshots.
        player_id: String,
        /// Tick at the moment the player joined — bootstraps client tick domain.
        server_tick: u64,
        /// Server wall-clock at the moment of join. Used by TimeSync as a
        /// starting reference until Ping/Pong refines the offset.
        server_time_ms: u64,
        /// Opaque token the client stores for Stage 9 reconnect/resume.
        resume_token: String,
        /// This admission went through the RESUME path on the server — including a
        /// JoinGame the server CONVERTED to a resume (lost-token reconnect). The client
        /// adopts it as its entry kind so resume semantics (shield mirror, gameplay
        /// hold, black cover) apply even though it sent JoinGame; without it the
        /// converted entry ran fresh-join logic against a server that had shielded it.
        /// (engineer wave 6 #3) `#[serde(default)]` keeps old peers/fixtures decoding.
        #[serde(default)]
        admitted_via_resume: bool,
    },
    GameLeft,
    GameStarting {
        countdown: u8,
    },

    GameState(GameStateUpdate),
    GameStateDelta(GameStateDelta),

    PlayerJoined {
        player_id: String,
        nickname: String,
    },
    PlayerLeft {
        player_id: String,
    },

    /// Stage 5.5: ALL gameplay-affecting events flow through Event(...) with
    /// event_id + server_tick. Client uses event_id for idempotency and the
    /// server_tick + reorder buffer to apply effects in deterministic order.
    Event(ServerEvent),

    /// Sent ONLY to the attacker when an EatClaim is not accepted, so its client
    /// can clear the pending claim and stop waiting. `reason` is for diagnostics.
    EatClaimRejected {
        claim_id: u32,
        server_tick: u64,
        reason: String,
    },

    /// Sent ONLY to the victim when an EnemyDeathClaim is not accepted, so its client can roll
    /// back the predicted death and resync. Symmetric with `EatClaimRejected`.
    EnemyDeathClaimRejected {
        claim_id: u32,
        server_tick: u64,
        reason: String,
    },

    Pong {
        /// Echoed verbatim from the matching Ping for round-trip pairing.
        client_send_time_ms: u64,
        server_time_ms: u64,
        server_tick: u64,
    },

    GameEnded {
        winner: GameWinner,
        rewards: Vec<PlayerReward>,
    },

    Error {
        message: String,
    },

    /// Reply to a Resume whose token/session is no longer valid (grace expired, the room
    /// ended, or the server was restarted and lost the in-memory session). The client treats
    /// this as "your old match is gone" and falls back to a fresh JoinGame. Appended at the
    /// END of the enum so it can't shift existing bincode variant ordinals (see ClientMessage).
    ResumeRejected {
        reason: String,
    },
}

/// Stage 5.5: gameplay events. Each carries a unique `event_id` (room-scoped,
/// monotonic) and the `server_tick` at which the event happened. Reliable
/// transport: delivered exactly once via the reliable channel, idempotent on
/// the client via appliedEventIds.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum ServerEvent {
    PlayerEaten {
        event_id: u64,
        server_tick: u64,
        eater_id: String,
        eaten_id: String,
        eaten_position: Position,
    },
    /// Hard event — clears prediction history on the client.
    PlayerRespawned {
        event_id: u64,
        server_tick: u64,
        player_id: String,
        position: Position,
        state_patch: StatePatch,
        /// The PLAYER that ate this player (Some only for a player-vs-player death,
        /// always via an accepted EatClaim). Mutually exclusive with killer_enemy_id.
        killer_player_id: Option<String>,
        /// The enemy that ate the player (Some only for a bot kill). Lets the client
        /// correlate the respawn with the exact killer for diagnostics.
        killer_enemy_id: Option<String>,
        /// Death-fairness diagnostics (None for a player-vs-player death): the
        /// server-truth distance at the killing tick, how many consecutive ticks
        /// the kill contact persisted, and the killer/victim positions at death.
        /// The client compares server_dist against the on-screen distance it
        /// rendered to quantify the predicted-vs-interpolated skew.
        server_dist: Option<f32>,
        contact_ticks: Option<u32>,
        killer_position: Option<Position>,
        victim_position: Option<Position>,
    },
    /// Hard event — clears prediction/interpolation history on the client.
    PortalTeleport {
        event_id: u64,
        server_tick: u64,
        player_id: String,
        from: Position,
        to: Position,
        state_patch: StatePatch,
    },
    /// Soft event — the player's move speed changed (boost start/expire). NOT a
    /// hard snap: the client schedules the new speed at this exact tick so its
    /// prediction/replay uses the authoritative speed, instead of drifting until
    /// the next snapshot. `reason` is for logs.
    PlayerSpeedChanged {
        event_id: u64,
        /// Tick at which the change was OBSERVED/emitted.
        server_tick: u64,
        /// First tick whose movement must use the new speed. Boost start lands the
        /// tick AFTER collection (movement already ran this tick on old speed), so
        /// it is `current_tick + 1`; boost expiry runs in `update_timers` BEFORE
        /// movement, so that tick already moved on base speed → `current_tick`.
        /// The client schedules the speed patch at this exact tick.
        effective_tick: u64,
        player_id: String,
        speed: f32,
        is_invincible: bool,
        reason: String,
    },
    /// Hard event — an enemy relocated. Clears the client's interpolation history
    /// for that entity so the respawn isn't lerped across the map. `reason` +
    /// `caused_by_player_id` let the client attribute the respawn (only an
    /// `player_ate_enemy` caused by US confirms a local visual touch).
    EnemyRespawned {
        event_id: u64,
        server_tick: u64,
        enemy_id: String,
        position: Position,
        direction: Direction,
        speed: f32,
        score: u32,
        reason: String,
        caused_by_player_id: Option<String>,
        /// Generation of the new enemy life (incremented on every respawn). The client
        /// stores it on the RemoteEntity so a later EatClaim can name the exact life it
        /// saw. `#[serde(default)]` keeps old fixtures/clients decoding (→ 0).
        #[serde(default)]
        generation: u32,
    },
    /// An enemy killed a player — the authoritative death fact, carrying the FULL
    /// timeline-explicit decision context in ONE event (so a death can be investigated
    /// without gluing `enemy_ate_player` telemetry to a `PlayerRespawned` wire event by
    /// tick/id). Emitted reliably BEFORE the victim's `PlayerRespawned`. Death and respawn
    /// are separate domain facts. `#[serde(default)]` on the newer fields keeps old
    /// clients decoding; the client correlates its own death-debug against this.
    PlayerKilledByEnemy {
        event_id: u64,
        server_tick: u64,
        victim_id: String,
        killer_enemy_id: String,
        /// Generation (life) of the killer — enemy ids are reused across respawns, so the
        /// kill is unambiguous only with the generation.
        #[serde(default)]
        killer_enemy_generation: u32,
        #[serde(default)]
        killer_enemy_respawned_at_tick: u64,
        /// Server-truth distance at the killing tick.
        server_dist: f32,
        /// Tick the killer was reconstructed at on the victim's presentation timeline
        /// (server_tick − interp_delay).
        #[serde(default)]
        reconstructed_enemy_tick: u64,
        /// Reconstructed enemy-visible distance (victim's current position vs the killer at
        /// `reconstructed_enemy_tick`). Named honestly: the victim's own position is not
        /// reconstructed (see EnemyDeathCandidatePolicy).
        #[serde(default)]
        reconstructed_enemy_visible_dist: Option<f32>,
        #[serde(default)]
        server_kill_radius: f32,
        /// Whether the visible-confirmation gate was ARMED for this death. `false` = observe-only
        /// (`decision` is `kill_now_server_only_observe_only`): the gate was reconstructed +
        /// logged but did not gate the kill, and `visible_confirm_radius_px` is `None`.
        #[serde(default)]
        visible_gate_enabled: bool,
        /// The visible-confirm radius the gate used (sprite-overlap + grace, NOT the kill radius)
        /// when armed; `None` in observe-only mode. `Some` ⇒ the kill landed because
        /// `reconstructed_enemy_visible_dist ≤ this`. No `Infinity` sentinel on the wire.
        #[serde(default)]
        visible_confirm_radius_px: Option<f32>,
        #[serde(default)]
        interp_delay_ticks: u64,
        contact_ticks: u32,
        /// Stable decision code: `kill_now_visible_confirmed` (gate armed) or
        /// `kill_now_server_only_observe_only` (gate off). Replaces the old free-form string.
        decision: DeathDecisionCode,
        /// Which death-fairness rule set decided this (config `death_fairness.policy_version`),
        /// so a replay knows the model even if the rules later change.
        #[serde(default)]
        policy_version: u32,
        killer_position: Position,
        victim_position: Position,
    },
    PointCollected {
        event_id: u64,
        server_tick: u64,
        point_id: String,
        player_id: String,
    },
    BoosterCollected {
        event_id: u64,
        server_tick: u64,
        booster_id: String,
        player_id: String,
    },
    PointSpawned {
        event_id: u64,
        server_tick: u64,
        point: PointItem,
    },
    BoosterSpawned {
        event_id: u64,
        server_tick: u64,
        booster: Booster,
    },
}

impl ServerEvent {
    pub fn event_id(&self) -> u64 {
        match self {
            ServerEvent::PlayerEaten { event_id, .. } => *event_id,
            ServerEvent::PlayerRespawned { event_id, .. } => *event_id,
            ServerEvent::PortalTeleport { event_id, .. } => *event_id,
            ServerEvent::PlayerSpeedChanged { event_id, .. } => *event_id,
            ServerEvent::EnemyRespawned { event_id, .. } => *event_id,
            ServerEvent::PlayerKilledByEnemy { event_id, .. } => *event_id,
            ServerEvent::PointCollected { event_id, .. } => *event_id,
            ServerEvent::BoosterCollected { event_id, .. } => *event_id,
            ServerEvent::PointSpawned { event_id, .. } => *event_id,
            ServerEvent::BoosterSpawned { event_id, .. } => *event_id,
        }
    }

    pub fn server_tick(&self) -> u64 {
        match self {
            ServerEvent::PlayerEaten { server_tick, .. } => *server_tick,
            ServerEvent::PlayerRespawned { server_tick, .. } => *server_tick,
            ServerEvent::PortalTeleport { server_tick, .. } => *server_tick,
            ServerEvent::PlayerSpeedChanged { server_tick, .. } => *server_tick,
            ServerEvent::EnemyRespawned { server_tick, .. } => *server_tick,
            ServerEvent::PlayerKilledByEnemy { server_tick, .. } => *server_tick,
            ServerEvent::PointCollected { server_tick, .. } => *server_tick,
            ServerEvent::BoosterCollected { server_tick, .. } => *server_tick,
            ServerEvent::PointSpawned { server_tick, .. } => *server_tick,
            ServerEvent::BoosterSpawned { server_tick, .. } => *server_tick,
        }
    }

    /// Stable wire variant name (for the server_ws_send ordering trace).
    pub fn kind(&self) -> &'static str {
        match self {
            ServerEvent::PlayerEaten { .. } => "PlayerEaten",
            ServerEvent::PlayerRespawned { .. } => "PlayerRespawned",
            ServerEvent::PortalTeleport { .. } => "PortalTeleport",
            ServerEvent::PlayerSpeedChanged { .. } => "PlayerSpeedChanged",
            ServerEvent::EnemyRespawned { .. } => "EnemyRespawned",
            ServerEvent::PlayerKilledByEnemy { .. } => "PlayerKilledByEnemy",
            ServerEvent::PointCollected { .. } => "PointCollected",
            ServerEvent::BoosterCollected { .. } => "BoosterCollected",
            ServerEvent::PointSpawned { .. } => "PointSpawned",
            ServerEvent::BoosterSpawned { .. } => "BoosterSpawned",
        }
    }
}

/// Authoritative player state at the moment a hard event fires.
///
/// The client compares this against its latest snapshot for that entity. If
/// they already match, the visual hard-snap can be skipped; otherwise the
/// patch is applied directly to clear any stale prediction/interpolation.
#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct StatePatch {
    pub position: Position,
    pub direction: Direction,
    pub speed: f32,
    pub is_invincible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct GameStateUpdate {
    pub players: Vec<PlayerState>,
    pub enemies: Vec<EnemyState>,
    pub boosters: Vec<Booster>,
    pub points: Vec<PointItem>,
    pub portal: Option<PortalState>,
    pub time_remaining: u64,
    pub tick: u64,
    pub server_time_ms: u64,
    /// Highest input seq the server has applied for the recipient. None if the
    /// player hasn't sent any MoveCommand yet. Used for reconciliation.
    pub last_processed_input_seq: Option<u32>,
    /// Highest event_id already reflected in this keyframe. Lets the client
    /// ignore a hard event it already absorbed via this full snapshot.
    pub last_event_id: Option<u64>,
    /// Monotonic per-room snapshot sequence (one per tick's snapshot send). 0 marks
    /// an OUT-OF-BAND keyframe (join / RequestFullState) that the client must NOT
    /// count toward drop detection. Lets the client spot a gap = dropped snapshot.
    #[serde(default)]
    pub snapshot_seq: u64,
    /// Echo of `RequestFullState::request_id` when this keyframe is the reply to that
    /// exact request; `None` for join keyframes, per-tick fulls, and legacy (id 0)
    /// requests. Appended at the END of the struct on purpose: snapshots ride the
    /// bincode binary lane, which encodes fields in declaration order — the C#
    /// SnapshotBincode reader decodes this as the trailing Option, and inserting it
    /// anywhere else would silently shift every later field. JSON keeps old
    /// fixtures/peers decoding via `#[serde(default)]`.
    #[serde(default)]
    pub full_state_request_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct GameStateDelta {
    pub players: Vec<PlayerPositionUpdate>,
    pub enemies: Vec<EnemyPositionUpdate>,
    pub tick: u64,
    pub time_remaining: u64,
    pub server_time_ms: u64,
    pub last_processed_input_seq: Option<u32>,
    /// Stage 5.5: highest event_id the server has emitted up to this tick.
    /// Lets the client know which events MUST have arrived (or be late and
    /// safely ignored) by the time this snapshot lands.
    pub last_event_id: Option<u64>,
    /// Monotonic per-room snapshot sequence (one per tick). A gap on the client =
    /// a dropped snapshot.
    #[serde(default)]
    pub snapshot_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct PlayerPositionUpdate {
    pub id: String,
    pub position: Position,
    pub direction: Direction,
    pub speed: f32,
    pub score: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct EnemyPositionUpdate {
    pub id: String,
    pub position: Position,
    pub direction: Direction,
    pub speed: f32,
    /// Current life of this (id-reused) enemy. Lets the client detect a respawn it
    /// missed the hard event for and refuse a stale eat. `#[serde(default)]` → 0 for
    /// old peers.
    #[serde(default)]
    pub generation: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct PlayerState {
    pub id: String,
    pub nickname: String,
    pub position: Position,
    pub direction: Direction,
    pub score: u32,
    pub speed: f32,
    pub is_invincible: bool,
    pub invincibility_remaining: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct EnemyState {
    pub id: String,
    pub nickname: String,
    pub position: Position,
    pub direction: Direction,
    pub speed: f32,
    pub score: u32,
    /// Current life of this (id-reused) enemy — bumped on every respawn. The client
    /// seeds RemoteEntity.Generation from the keyframe so eat claims can name the life.
    /// `#[serde(default)]` → 0 for old peers / fixtures.
    #[serde(default)]
    pub generation: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct Booster {
    pub id: String,
    pub position: Position,
    pub booster_type: BoosterType,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub enum BoosterType {
    Mushroom,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct PointItem {
    pub id: String,
    pub position: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct PortalState {
    pub position: Position,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct GameWinner {
    /// The PERSISTENT account id (`users.id`), NOT a per-connection player_id — the winner is
    /// an account, and the connection's player_id dies with the room. Named honestly so it
    /// can't be mistaken for the snapshot/event player_id (which IS the per-connection id).
    pub user_id: String,
    pub nickname: String,
    pub score: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, bincode::Encode, bincode::Decode)]
pub struct PlayerReward {
    /// PERSISTENT account id (`users.id`) the crystals are credited to — NOT a per-connection
    /// player_id. The reward outbox keys idempotency on it; analytics/support tools must read
    /// it as the account.
    pub user_id: String,
    pub crystals: i64,
    pub is_winner: bool,
}

impl ClientMessage {
    #[allow(dead_code)]
    pub fn serialize(&self) -> Result<Vec<u8>, Box<bincode::error::EncodeError>> {
        Ok(bincode::encode_to_vec(self, bincode::config::standard())?)
    }

    pub fn deserialize(data: &[u8]) -> Result<Self, Box<bincode::error::DecodeError>> {
        let (decoded, _): (Self, usize) = bincode::decode_from_slice(data, bincode::config::standard())?;
        Ok(decoded)
    }
}

impl ServerMessage {
    /// Bincode (standard config: little-endian + varint) — the BINARY wire encoding used for
    /// snapshots (GameState / GameStateDelta) since protocol v6. The Unity BincodeDecoder
    /// mirrors this byte-for-byte; the snapshot_frames golden fixture pins the two together.
    pub fn serialize(&self) -> Result<Vec<u8>, Box<bincode::error::EncodeError>> {
        Ok(bincode::encode_to_vec(self, bincode::config::standard())?)
    }

    #[allow(dead_code)]
    pub fn deserialize(data: &[u8]) -> Result<Self, Box<bincode::error::DecodeError>> {
        let (decoded, _): (Self, usize) = bincode::decode_from_slice(data, bincode::config::standard())?;
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live wire is JSON (Message::Text + serde_json — see network::websocket). These
    // pin the enemy-generation fields as a BACKWARD-COMPATIBLE additive change: a new peer
    // round-trips the value, and an OLD peer's JSON (field absent) decodes via
    // `#[serde(default)]` instead of failing. Protocol rule 7 (golden test for an important
    // message) + rule 4 (generation ids for reusable entities).

    #[test]
    fn enemy_state_generation_roundtrips_and_defaults() {
        let s = EnemyState {
            id: "e0".into(),
            nickname: "bot1".into(),
            position: Position { x: 1.0, y: 2.0 },
            direction: Direction::Up,
            speed: 100.0,
            score: 7,
            generation: 4,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: EnemyState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.generation, 4, "generation survives a JSON round-trip");

        // Old-peer keyframe with no `generation` field → defaults to 0, doesn't fail.
        let legacy = r#"{"id":"e0","nickname":"bot1","position":{"x":1.0,"y":2.0},"direction":"Up","speed":100.0,"score":7}"#;
        let decoded: EnemyState = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.generation, 0, "absent generation defaults to 0");
    }

    #[test]
    fn enemy_position_update_generation_defaults_when_absent() {
        let legacy = r#"{"id":"e0","position":{"x":0.0,"y":0.0},"direction":"Down","speed":100.0}"#;
        let decoded: EnemyPositionUpdate = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.generation, 0);
    }

    #[test]
    fn eat_claim_target_generation_roundtrips_and_defaults() {
        let mut claim = EatClaim {
            claim_id: 1,
            target_kind: EatTargetKind::Enemy,
            target_id: "e0".into(),
            attacker_render_tick: 100.0,
            target_render_tick: 98.0,
            attacker_position: Position { x: 0.0, y: 0.0 },
            target_position: Position { x: 1.0, y: 1.0 },
            visual_distance: 5.0,
            target_generation: Some(3),
        };
        let back: EatClaim = serde_json::from_str(&serde_json::to_string(&claim).unwrap()).unwrap();
        assert_eq!(back.target_generation, Some(3), "named generation survives round-trip");

        // Legacy claim (old client) omits the field → None → server takes the legacy path.
        let legacy = r#"{"claim_id":1,"target_kind":"Enemy","target_id":"e0","attacker_render_tick":100.0,"target_render_tick":98.0,"attacker_position":{"x":0.0,"y":0.0},"target_position":{"x":1.0,"y":1.0},"visual_distance":5.0}"#;
        let decoded: EatClaim = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.target_generation, None, "absent target_generation defaults to None");

        // A None claim also bincode-roundtrips (the Binary inbound path).
        claim.target_generation = None;
        let bytes = ClientMessage::EatClaim(claim).serialize().unwrap();
        assert!(ClientMessage::deserialize(&bytes).is_ok());
    }

    #[test]
    fn player_killed_by_enemy_roundtrips_full_context() {
        let ev = ServerEvent::PlayerKilledByEnemy {
            event_id: 9,
            server_tick: 1200,
            victim_id: "p1".into(),
            killer_enemy_id: "e0".into(),
            killer_enemy_generation: 2,
            killer_enemy_respawned_at_tick: 800,
            server_dist: 5.0,
            reconstructed_enemy_tick: 1192,
            reconstructed_enemy_visible_dist: Some(15.0),
            server_kill_radius: 11.0,
            visible_gate_enabled: true,
            visible_confirm_radius_px: Some(20.0),
            interp_delay_ticks: 8,
            contact_ticks: 3,
            decision: DeathDecisionCode::KillNowVisibleConfirmed,
            policy_version: 2,
            killer_position: Position { x: 1.0, y: 2.0 },
            victim_position: Position { x: 3.0, y: 4.0 },
        };
        // The stable code serializes snake_case on the JSON wire.
        assert!(serde_json::to_string(&ev).unwrap().contains("\"kill_now_visible_confirmed\""));
        let back: ServerEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        match back {
            ServerEvent::PlayerKilledByEnemy {
                killer_enemy_generation,
                reconstructed_enemy_visible_dist,
                visible_gate_enabled,
                visible_confirm_radius_px,
                interp_delay_ticks,
                decision,
                policy_version,
                ..
            } => {
                assert_eq!(killer_enemy_generation, 2, "killer generation survives");
                assert_eq!(reconstructed_enemy_visible_dist, Some(15.0));
                assert!(visible_gate_enabled, "armed gate flag survives");
                assert_eq!(
                    visible_confirm_radius_px,
                    Some(20.0),
                    "Option radius survives (no Infinity sentinel)"
                );
                assert_eq!(interp_delay_ticks, 8);
                assert_eq!(decision, DeathDecisionCode::KillNowVisibleConfirmed);
                assert_eq!(policy_version, 2);
            }
            _ => panic!("expected PlayerKilledByEnemy"),
        }
    }

    /// Observe-only death: the gate is OFF, so the wire carries the distinct
    /// `kill_now_server_only_observe_only` code and a `None` confirm radius — never an
    /// `Infinity` sentinel masquerading as a real threshold. (lead review)
    #[test]
    fn observe_only_death_carries_none_radius_and_distinct_code() {
        let ev = ServerEvent::PlayerKilledByEnemy {
            event_id: 1,
            server_tick: 10,
            victim_id: "p1".into(),
            killer_enemy_id: "e0".into(),
            killer_enemy_generation: 0,
            killer_enemy_respawned_at_tick: 0,
            server_dist: 5.0,
            reconstructed_enemy_tick: 2,
            reconstructed_enemy_visible_dist: Some(32.0),
            server_kill_radius: 11.0,
            visible_gate_enabled: false,
            visible_confirm_radius_px: None,
            interp_delay_ticks: 8,
            contact_ticks: 3,
            decision: DeathDecisionCode::KillNowServerOnlyObserveOnly,
            policy_version: 2,
            killer_position: Position { x: 1.0, y: 2.0 },
            victim_position: Position { x: 3.0, y: 4.0 },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"kill_now_server_only_observe_only\""));
        assert!(!json.contains("inf"), "no Infinity sentinel on the wire");
        let back: ServerEvent = serde_json::from_str(&json).unwrap();
        match back {
            ServerEvent::PlayerKilledByEnemy {
                visible_gate_enabled,
                visible_confirm_radius_px,
                decision,
                ..
            } => {
                assert!(!visible_gate_enabled);
                assert_eq!(visible_confirm_radius_px, None);
                assert_eq!(decision, DeathDecisionCode::KillNowServerOnlyObserveOnly);
            }
            _ => panic!("expected PlayerKilledByEnemy"),
        }
    }

    /// Welcome carries `enemy_death_visible_gate_enabled` so the client never reads a non-zero
    /// confirm radius as an armed gate (observe-only sends a radius but gate=false). Round-trips
    /// both states. (lead review)
    #[test]
    fn welcome_carries_visible_gate_enabled_flag() {
        for gate in [true, false] {
            let msg = ServerMessage::Welcome {
                protocol_version: 3,
                min_supported_build: "0.1.0".into(),
                server_config_hash: "abc".into(),
                server_maze_hash: "def".into(),
                tick_rate: 60,
                enemy_kill_radius_px: 11.0,
                enemy_kill_contact_ticks: 3,
                enemy_interp_delay_ticks: 8,
                enemy_death_visible_confirm_radius_px: 20.0,
                enemy_death_visible_gate_enabled: gate,
                enemy_death_policy_version: 2,
                safe_zone_center_x: 250.0,
                safe_zone_center_y: 500.0,
                safe_zone_radius_px: 80.0,
                max_client_prediction_lead_ticks: 36,
                spawn_protection_sec: 3.0,
                claim_visible_overlap_radius_px: 18.0,
                resume_shield_ticks: 90,
            };
            let back: ServerMessage = serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
            match back {
                ServerMessage::Welcome {
                    enemy_death_visible_gate_enabled,
                    enemy_death_visible_confirm_radius_px,
                    claim_visible_overlap_radius_px,
                    resume_shield_ticks,
                    ..
                } => {
                    assert_eq!(enemy_death_visible_gate_enabled, gate, "gate flag survives");
                    assert_eq!(enemy_death_visible_confirm_radius_px, 20.0);
                    // The generalized claim overlap radius round-trips (snake_case wire key
                    // claim_visible_overlap_radius_px). (review Blocker 3 / required test)
                    assert_eq!(claim_visible_overlap_radius_px, 18.0);
                    assert_eq!(resume_shield_ticks, 90, "resume shield window survives");
                }
                _ => panic!("expected Welcome"),
            }
        }
    }

    /// Protocol v6: snapshots ride as bincode. Pin (a) lossless round-trip of a realistic
    /// delta — bit-exact f32, every field — and (b) that the binary frame is a real egress
    /// win over the JSON it replaces (the whole point; at 30/s × 100 conns JSON deltas
    /// measured 78 Mbit/s). Threshold is deliberately loose (×2) so it flags a regression
    /// to bloat, not normal drift.
    #[test]
    fn binary_delta_roundtrips_and_beats_json_by_2x() {
        let delta = ServerMessage::GameStateDelta(GameStateDelta {
            players: (0..8)
                .map(|i| PlayerPositionUpdate {
                    id: format!("p_{:08x}", 0x1000 + i),
                    position: Position { x: 100.5 + i as f32, y: 200.25 },
                    direction: Direction::Right,
                    speed: 100.0,
                    score: 50 + i,
                })
                .collect(),
            enemies: (0..12)
                .map(|i| EnemyPositionUpdate {
                    id: format!("e_{:08x}", 0x2000 + i),
                    position: Position { x: 300.0, y: 400.75 + i as f32 },
                    direction: Direction::Down,
                    speed: 100.0,
                    generation: 3,
                })
                .collect(),
            tick: 12345,
            time_remaining: 30,
            server_time_ms: 1_781_000_000_000,
            last_processed_input_seq: Some(777),
            last_event_id: Some(4242),
            snapshot_seq: 6172,
        });
        let bin = delta.serialize().unwrap();
        let json = serde_json::to_string(&delta).unwrap();
        assert!(
            bin.len() * 2 < json.len(),
            "binary delta ({}B) must be < half the JSON ({}B)",
            bin.len(),
            json.len()
        );
        let back = ServerMessage::deserialize(&bin).unwrap();
        match back {
            ServerMessage::GameStateDelta(d) => {
                assert_eq!(d.players.len(), 8);
                assert_eq!(d.enemies.len(), 12);
                assert_eq!(d.players[3].position.x, 103.5, "f32 bit-exact");
                assert_eq!(d.players[3].id, "p_00001003");
                assert_eq!(d.last_processed_input_seq, Some(777));
                assert_eq!(d.snapshot_seq, 6172);
            }
            _ => panic!("expected GameStateDelta"),
        }
    }

    /// The C# SnapshotBincode decoder hardcodes the snapshot variant ordinals (8/9) — this
    /// pins them on the Rust side. Bincode keys enum variants by DECLARATION ORDER, so
    /// inserting a ServerMessage variant before GameState would silently shift these and
    /// break every v6 client. Variants are append-only by contract (see the Resume note);
    /// this test makes violating that loud. The first wire byte IS the ordinal (varint <251).
    #[test]
    fn snapshot_variant_ordinals_are_pinned_for_binary_lane() {
        let key = ServerMessage::GameState(GameStateUpdate {
            players: vec![],
            enemies: vec![],
            boosters: vec![],
            points: vec![],
            portal: None,
            time_remaining: 0,
            tick: 0,
            server_time_ms: 0,
            last_processed_input_seq: None,
            last_event_id: None,
            snapshot_seq: 0,
            full_state_request_id: None,
        });
        let delta = ServerMessage::GameStateDelta(GameStateDelta {
            players: vec![],
            enemies: vec![],
            tick: 0,
            time_remaining: 0,
            server_time_ms: 0,
            last_processed_input_seq: None,
            last_event_id: None,
            snapshot_seq: 0,
        });
        assert_eq!(key.serialize().unwrap()[0], 8, "GameState ordinal moved — v6 binary lane broken");
        assert_eq!(delta.serialize().unwrap()[0], 9, "GameStateDelta ordinal moved — v6 binary lane broken");
    }

    /// Full-state correlation id (lead manifesto 2026-06-10): the request round-trips its
    /// id, a LEGACY request without the field decodes to 0, and the reply keyframe carries
    /// the echo as a trailing Option that (a) JSON-round-trips, (b) defaults to None on old
    /// fixtures, and (c) bincode-round-trips in the binary snapshot lane.
    #[test]
    fn full_state_request_id_roundtrips_and_defaults() {
        // Request side: round-trip + legacy default.
        let req = ClientMessage::RequestFullState { reason: "app-resume".into(), request_id: 42 };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"request_id\":42"));
        match serde_json::from_str::<ClientMessage>(&json).unwrap() {
            ClientMessage::RequestFullState { request_id, .. } => assert_eq!(request_id, 42),
            _ => panic!("expected RequestFullState"),
        }
        let legacy = r#"{"RequestFullState":{"reason":"app-resume"}}"#;
        match serde_json::from_str::<ClientMessage>(legacy).unwrap() {
            ClientMessage::RequestFullState { request_id, .. } => {
                assert_eq!(request_id, 0, "absent request_id defaults to 0 (legacy client)")
            }
            _ => panic!("expected RequestFullState"),
        }

        // Reply side: the echo survives JSON and the bincode binary lane, and an old
        // keyframe without the field decodes to None.
        let key = GameStateUpdate {
            players: vec![],
            enemies: vec![],
            boosters: vec![],
            points: vec![],
            portal: None,
            time_remaining: 0,
            tick: 1036,
            server_time_ms: 1,
            last_processed_input_seq: None,
            last_event_id: None,
            snapshot_seq: 0,
            full_state_request_id: Some(42),
        };
        let back: GameStateUpdate = serde_json::from_str(&serde_json::to_string(&key).unwrap()).unwrap();
        assert_eq!(back.full_state_request_id, Some(42), "echo survives JSON");
        let legacy_key = r#"{"players":[],"enemies":[],"boosters":[],"points":[],"portal":null,"time_remaining":0,"tick":1036,"server_time_ms":1,"last_processed_input_seq":null,"last_event_id":null,"snapshot_seq":0}"#;
        let decoded: GameStateUpdate = serde_json::from_str(legacy_key).unwrap();
        assert_eq!(decoded.full_state_request_id, None, "absent echo defaults to None");
        let bin = ServerMessage::GameState(key).serialize().unwrap();
        match ServerMessage::deserialize(&bin).unwrap() {
            ServerMessage::GameState(s) => {
                assert_eq!(s.full_state_request_id, Some(42), "echo survives the bincode snapshot lane")
            }
            _ => panic!("expected GameState"),
        }
    }

    /// Hello's debug metadata (engineer wave 7) is additive: a legacy Hello without the
    /// fields decodes with attempt 0 / empty session / no intent.
    #[test]
    fn hello_debug_metadata_defaults_for_legacy_clients() {
        let legacy = r#"{"Hello":{"protocol_version":6,"client_build":"0.1.0","platform":"unity","config_hash":"c","maze_hash":"m"}}"#;
        match serde_json::from_str::<ClientMessage>(legacy).unwrap() {
            ClientMessage::Hello { connect_attempt_id, client_session_id, resume_intent, .. } => {
                assert_eq!(connect_attempt_id, 0);
                assert!(client_session_id.is_empty());
                assert!(!resume_intent);
            }
            _ => panic!("expected Hello"),
        }
    }

    /// GameJoined carries admitted_via_resume (engineer wave 6 #3) so a server-CONVERTED
    /// JoinGame still lands as a resume on the client. Round-trips; absent on old wire → false.
    #[test]
    fn game_joined_admitted_via_resume_roundtrips_and_defaults() {
        let msg = ServerMessage::GameJoined {
            room_id: "r1".into(),
            player_id: "p1".into(),
            server_tick: 1929,
            server_time_ms: 5,
            resume_token: "t".into(),
            admitted_via_resume: true,
        };
        match serde_json::from_str::<ServerMessage>(&serde_json::to_string(&msg).unwrap()).unwrap() {
            ServerMessage::GameJoined { admitted_via_resume, .. } => {
                assert!(admitted_via_resume, "flag survives the round-trip")
            }
            _ => panic!("expected GameJoined"),
        }
        let legacy = r#"{"GameJoined":{"room_id":"r1","player_id":"p1","server_tick":0,"server_time_ms":5,"resume_token":"t"}}"#;
        match serde_json::from_str::<ServerMessage>(legacy).unwrap() {
            ServerMessage::GameJoined { admitted_via_resume, .. } => {
                assert!(!admitted_via_resume, "absent flag defaults to false")
            }
            _ => panic!("expected GameJoined"),
        }
    }

    /// ClientWorldReady (lead manifesto 2026-06-10): the wire shape the Unity client sends
    /// (externally tagged, snake_case) decodes, round-trips, and — being appended at the END
    /// of ClientMessage — also bincode-round-trips without disturbing earlier ordinals.
    #[test]
    fn client_world_ready_decodes_and_roundtrips() {
        let wire = r#"{"ClientWorldReady":{"world_sync_epoch":3,"anchor_tick":1399}}"#;
        match serde_json::from_str::<ClientMessage>(wire).unwrap() {
            ClientMessage::ClientWorldReady { world_sync_epoch, anchor_tick } => {
                assert_eq!(world_sync_epoch, 3);
                assert_eq!(anchor_tick, 1399);
            }
            _ => panic!("expected ClientWorldReady"),
        }
        let msg = ClientMessage::ClientWorldReady { world_sync_epoch: 3, anchor_tick: 1399 };
        let bytes = msg.serialize().unwrap();
        assert!(matches!(
            ClientMessage::deserialize(&bytes).unwrap(),
            ClientMessage::ClientWorldReady { world_sync_epoch: 3, anchor_tick: 1399 }
        ));

        // The stricter second stage (readiness split, 2026-06-11) — same wire shape checks.
        let wire = r#"{"ClientClaimReady":{"world_sync_epoch":4,"anchor_tick":1500}}"#;
        match serde_json::from_str::<ClientMessage>(wire).unwrap() {
            ClientMessage::ClientClaimReady { world_sync_epoch, anchor_tick } => {
                assert_eq!(world_sync_epoch, 4);
                assert_eq!(anchor_tick, 1500);
            }
            _ => panic!("expected ClientClaimReady"),
        }
        let msg = ClientMessage::ClientClaimReady { world_sync_epoch: 4, anchor_tick: 1500 };
        assert!(matches!(
            ClientMessage::deserialize(&msg.serialize().unwrap()).unwrap(),
            ClientMessage::ClientClaimReady { world_sync_epoch: 4, anchor_tick: 1500 }
        ));
    }

    #[test]
    fn enemy_respawned_event_carries_generation() {
        let ev = ServerEvent::EnemyRespawned {
            event_id: 5,
            server_tick: 200,
            enemy_id: "e0".into(),
            position: Position { x: 3.0, y: 4.0 },
            direction: Direction::Left,
            speed: 100.0,
            score: 9,
            reason: "player_ate_enemy".into(),
            caused_by_player_id: Some("p1".into()),
            generation: 2,
        };
        let back: ServerEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        match back {
            ServerEvent::EnemyRespawned { generation, .. } => assert_eq!(generation, 2),
            _ => panic!("expected EnemyRespawned"),
        }
    }
}
