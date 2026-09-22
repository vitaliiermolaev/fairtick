use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MapConfig {
    pub width: f32,
    pub height: f32,
    pub cell_size: f32,
    pub grid_width: usize,
    pub grid_height: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PlayerConfig {
    pub base_speed: f32,
    pub boosted_speed: f32,
    pub initial_score: u32,
    pub invincibility_duration_sec: f32,
    pub turn_threshold_px: f32,
    /// Post-respawn invulnerability window (seconds). A FAIRNESS value, not presentation: the server
    /// can't be eaten during it and the client mirrors it with a respawn blink — so it must be the
    /// SAME number on both sides. Sourced here (config → Welcome) instead of duplicated constants in
    /// player.rs and the Unity client. (review #3 — config single source of truth)
    pub spawn_protection_sec: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct CollisionConfig {
    pub collision_distance_px: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SafeZoneConfig {
    pub center_x: f32,
    pub center_y: f32,
    pub radius_px: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct PortalConfig {
    pub pickup_radius_px: f32,
    pub respawn_interval_sec: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SpawnConfig {
    pub point_interval_sec: f32,
    pub booster_interval_sec: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RoomConfig {
    pub max_players: usize,
    pub game_duration_sec: u64,
    pub tick_rate: u64,
    pub send_interval_ticks: u64,
    pub full_sync_interval_ticks: u64,
}

/// Enemy-eats-player death-fairness rules. In CONFIG (not hardcoded consts) so the rules
/// are versioned and folded into `config_hash` — a death's fairness model is reproducible
/// for replay/diagnostics, and a rule change forces a client bundle re-sync. The visible
/// gate confirms a kill only when the reconstructed enemy-visible distance is within
/// `player_render_radius_px + enemy_render_radius_px + visible_contact_grace_px`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct DeathFairnessConfig {
    /// Bumped whenever the death RULE changes (not just a tuning value), so a log reader can
    /// tell which model decided a death.
    pub policy_version: u32,
    /// Rendered sprite radii — the visible-confirm radius is their sum + the grace, i.e. the
    /// centre distance at which the two dots visibly overlap.
    pub player_render_radius_px: f32,
    pub enemy_render_radius_px: f32,
    /// Tolerance added to the sprite-overlap distance for the visible confirm.
    pub visible_contact_grace_px: f32,
    /// Ticks the client renders a remote ENEMY behind server-now; the server reconstructs the
    /// enemy-visible position this far back. Death-critical: client and server must agree.
    pub enemy_interp_delay_ticks: u64,
    /// Master switch: when false, the visible gate is OBSERVE-ONLY (reconstruct + log, never
    /// hold a kill).
    pub visible_gate_enabled: bool,
}

impl DeathFairnessConfig {
    /// Centre distance at which the two sprites visibly overlap, plus grace — the threshold
    /// the visible gate confirms a kill against. SEPARATE from the server kill radius.
    pub fn visible_confirm_radius_px(&self) -> f32 {
        self.player_render_radius_px + self.enemy_render_radius_px + self.visible_contact_grace_px
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EconomyConfig {
    pub winner_crystals: i64,
    pub participant_crystals: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NetworkConfig {
    pub protocol_version: u32,
    pub move_command_rate_limit_per_sec: u32,
    pub reconnect_grace_sec: u32,
    /// Max ticks a client predicts its OWN avatar ahead of server-now — the upper bound of the
    /// client's prediction-lead clamp (`ComputeLeadTicks`). SINGLE SOURCE OF TRUTH shared by the
    /// client (sent in Welcome) and the server: the death-claim fallback hold/future windows derive
    /// from it, so an honest victim's claim (which can lead by up to this much on bad RTT) is treated
    /// as plausible and not pre-empted. Folded into config_hash so the two sides can't drift.
    /// (round-3 follow-up)
    pub max_client_prediction_lead_ticks: u64,
    /// Honest client Ping cadence (ms). SINGLE SOURCE OF TRUTH shared by the Unity client
    /// (generated into GameConstants.PingIntervalMs) and the server's ping rate limit
    /// (derived as 1000/this × headroom in the websocket edge guard) — so tuning the
    /// cadence can never silently outrun the limit, and the limit can't strand an honest
    /// client. Folded into config_hash like every other shared value.
    pub ping_interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AiConfig {
    /// Number of enemy bots spawned in a room.
    pub count: usize,
    /// Bot movement speed (px/s). Standard = the player's base_speed.
    pub speed: f32,
    /// Bots get distinct starting scores spread across [min..=max] so they read
    /// as individuals (and only some are eatable at a given player score).
    pub min_start_score: u32,
    pub max_start_score: u32,
}

/// Filler bots — fake PLAYERS that keep casual rooms feeling alive (see
/// [`game::filler_bot`](crate::game::filler_bot)). Distinct from [`AiConfig`] (the red PvE
/// `Enemy` mobs). Server-only knobs: the client never learns which players are fillers, so
/// nothing here is forwarded in Welcome — but it lives in the shared toml (and config_hash)
/// like every other gameplay constant so deploys can't drift.
///
/// `#[serde(default)]` (unlike the strict sections above) so a stale toml without `[filler]`
/// still boots a server — and boots it with fillers OFF (lead review P0: a missing section
/// must never surprise-enable bots; the shipping toml opts in explicitly).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct FillerConfig {
    /// Master switch. Off = rooms contain only humans + PvE enemies (pre-filler behavior).
    /// Default FALSE: enabling is always an explicit toml/env decision.
    pub enabled: bool,
    /// The room tops up total players (humans + fillers) to this count. Humans always win
    /// slots: fillers yield via DELAYED exits, and `is_full` counts humans only.
    pub target_visible_players: usize,
    /// When a human joins and the room is over target, ONE filler leaves after a delay drawn
    /// from `[min..max]` ticks — never the same tick (an instant join→quit pair is the
    /// classic bot tell). At 60 tps the default 48..150 reads as 0.8–2.5 s.
    pub exit_delay_min_ticks: u64,
    pub exit_delay_max_ticks: u64,
    /// Hard-ish ceiling on TOTAL entities (humans + fillers) during a human join burst
    /// (lead review P1): humans always seat, but past this the overflow fillers leave on
    /// EXPEDITED short-delay exits instead of the polite 0.8–2.5 s ones.
    pub max_transient_visible_players: usize,
    /// A room with zero humans AND zero reserved resume slots keeps its fillers warm for
    /// this many ticks, then sweeps them and deactivates (lead review P1: filler-only
    /// rooms must not burn CPU until game_end). 180 @ 60tps = 3 s.
    pub empty_room_grace_ticks: u64,
}

impl Default for FillerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            target_visible_players: 8,
            exit_delay_min_ticks: 48,
            exit_delay_max_ticks: 150,
            max_transient_visible_players: 10,
            empty_room_grace_ticks: 180,
        }
    }
}

impl FillerConfig {
    /// Restart-only ops kill switch (lead review: "выключить filler без пересборки
    /// клиента — must-have"). Env beats toml WITHOUT touching the hashed toml bytes:
    /// `config_hash` is computed over the FILE, and the client never reads `[filler]`,
    /// so flipping these can never strand a shipped build on a hash mismatch.
    ///
    /// `get` is injected (production passes `std::env::var(k).ok()`) so tests don't
    /// mutate process env. An UNPARSABLE value is a hard Err — the caller must fail the
    /// BOOT loudly: a typo'd kill switch that silently kept bots ON is the worst
    /// possible failure mode for the one knob ops reaches for in an incident.
    pub fn apply_env_overrides(
        &mut self,
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<Vec<(&'static str, String)>, String> {
        fn parse_bool(key: &str, v: &str) -> Result<bool, String> {
            match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Ok(true),
                "0" | "false" | "off" | "no" => Ok(false),
                other => Err(format!("{key}: expected a boolean (1/0/true/false), got '{other}'")),
            }
        }
        fn parse_num<T: std::str::FromStr>(key: &str, v: &str) -> Result<T, String>
        where
            T::Err: std::fmt::Display,
        {
            v.trim().parse::<T>().map_err(|e| format!("{key}: {e} (value '{v}')"))
        }

        let mut applied: Vec<(&'static str, String)> = Vec::new();
        if let Some(v) = get("FAIRTICK_FILLER_ENABLED") {
            self.enabled = parse_bool("FAIRTICK_FILLER_ENABLED", &v)?;
            applied.push(("FAIRTICK_FILLER_ENABLED", v));
        }
        if let Some(v) = get("FAIRTICK_FILLER_TARGET_VISIBLE_PLAYERS") {
            self.target_visible_players = parse_num("FAIRTICK_FILLER_TARGET_VISIBLE_PLAYERS", &v)?;
            applied.push(("FAIRTICK_FILLER_TARGET_VISIBLE_PLAYERS", v));
        }
        if let Some(v) = get("FAIRTICK_FILLER_MAX_TRANSIENT_VISIBLE_PLAYERS") {
            self.max_transient_visible_players =
                parse_num("FAIRTICK_FILLER_MAX_TRANSIENT_VISIBLE_PLAYERS", &v)?;
            applied.push(("FAIRTICK_FILLER_MAX_TRANSIENT_VISIBLE_PLAYERS", v));
        }
        if let Some(v) = get("FAIRTICK_FILLER_EXIT_DELAY_MIN_TICKS") {
            self.exit_delay_min_ticks = parse_num("FAIRTICK_FILLER_EXIT_DELAY_MIN_TICKS", &v)?;
            applied.push(("FAIRTICK_FILLER_EXIT_DELAY_MIN_TICKS", v));
        }
        if let Some(v) = get("FAIRTICK_FILLER_EXIT_DELAY_MAX_TICKS") {
            self.exit_delay_max_ticks = parse_num("FAIRTICK_FILLER_EXIT_DELAY_MAX_TICKS", &v)?;
            applied.push(("FAIRTICK_FILLER_EXIT_DELAY_MAX_TICKS", v));
        }
        if let Some(v) = get("FAIRTICK_FILLER_EMPTY_ROOM_GRACE_TICKS") {
            self.empty_room_grace_ticks = parse_num("FAIRTICK_FILLER_EMPTY_ROOM_GRACE_TICKS", &v)?;
            applied.push(("FAIRTICK_FILLER_EMPTY_ROOM_GRACE_TICKS", v));
        }
        Ok(applied)
    }

    /// Cross-field sanity for the EFFECTIVE filler config (toml + env overrides), run
    /// at boot AFTER `apply_env_overrides` (lead review): a value that parses fine can
    /// still describe a nonsensical runtime — target 0 rooms, a transient cap below
    /// target (instant expedited-exit churn), an inverted delay range (panics inside
    /// RoomRng), a zero grace (rooms sweep the same tick a human's socket blips). All
    /// of those must fail the BOOT with a named field, not "survive" weirdly.
    pub fn validate(&self, room_max_players: usize) -> Result<(), String> {
        if !self.enabled {
            return Ok(()); // disabled fillers constrain nothing
        }
        if self.target_visible_players == 0 {
            return Err("filler.target_visible_players must be >= 1".into());
        }
        if self.target_visible_players > room_max_players {
            return Err(format!(
                "filler.target_visible_players ({}) must be <= room.max_players ({room_max_players})",
                self.target_visible_players
            ));
        }
        if self.max_transient_visible_players < self.target_visible_players {
            return Err(format!(
                "filler.max_transient_visible_players ({}) must be >= target_visible_players ({})",
                self.max_transient_visible_players, self.target_visible_players
            ));
        }
        if self.exit_delay_min_ticks == 0 {
            return Err("filler.exit_delay_min_ticks must be >= 1 (same-tick exits are the bot tell)".into());
        }
        if self.exit_delay_max_ticks < self.exit_delay_min_ticks {
            return Err(format!(
                "filler.exit_delay_max_ticks ({}) must be >= exit_delay_min_ticks ({})",
                self.exit_delay_max_ticks, self.exit_delay_min_ticks
            ));
        }
        if self.empty_room_grace_ticks == 0 {
            return Err("filler.empty_room_grace_ticks must be >= 1".into());
        }
        Ok(())
    }
}

/// Claim VALIDATION tolerances shared by [`EatClaimPolicy`](crate::game::policies::eat_claim) (player
/// eats X) and [`EnemyDeathClaimPolicy`](crate::game::policies::enemy_death_claim) (enemy eats
/// player). These are the geometry/timeline tolerances the server re-validates a client's claim
/// against. In CONFIG (not duplicated `const`s in the two policy modules — which is exactly how they
/// could silently drift from each other) so they are versioned into `config_hash`, and the visual
/// radius is forwarded to the client (Welcome) so Unity's claim CREATION uses the SAME number the
/// server validates against. (architecture review #3 — config single source of truth)
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ClaimFairnessConfig {
    /// CLAIM overlap radius (px): the largest reconstructed/visible attacker↔target distance that
    /// still counts as a visible overlap when the server VALIDATES a claim (eat AND death). NOT a
    /// presentation SOT — it is not the rendered sprite size; it happens to equal the ~18px the
    /// client currently draws, but the rendered dot scale is a SEPARATE invariant. If you change this
    /// without changing the sprite size, the claim gate moves while the picture doesn't, which can
    /// re-break "player saw X, server validates X". (review Blocker 3 / High — generalized, honest name)
    pub visible_overlap_radius_px: f32,
    /// How far a client's REPORTED position may sit from the server's RECONSTRUCTED position at the
    /// claimed render tick before the claim is called inconsistent (anti-cheat / desync guard).
    pub position_tolerance_px: f32,
    /// Interpolation / fractional-tick slack added to `visible_overlap_radius_px` to get the max reconstructed
    /// overlap distance (see [`reconstruct_overlap_radius_px`](Self::reconstruct_overlap_radius_px)).
    pub reconstruct_slack_px: f32,
    /// Max `attacker_render_tick - target_render_tick` (eat) the shape gate accepts — the attacker
    /// renders itself ahead and its target behind, so a bounded forward skew is honest.
    pub max_view_skew_ticks: f64,
    /// An eat claim that arrives more than this many ticks after its attacker render tick is too late
    /// to reconstruct reliably and is rejected (the client resends a fresh claim while the overlap holds).
    pub max_process_delay_ticks: u64,
    /// Length of the contact-history ring (ticks) the server keeps to reconstruct past positions a
    /// claim references. Also bounds the oldest render tick a claim may target.
    pub history_ticks: usize,
    /// A just-respawned enemy can't be eaten again for this many ticks (kills the respawn-in-mouth
    /// double-eat).
    pub enemy_respawn_eat_grace_ticks: u64,
    /// How far a death claim's reported `visual_distance` may differ from the geometric distance
    /// between its reported positions before it's called internally inconsistent (stops a forged
    /// visual_distance=0 with far-apart positions).
    pub visual_distance_tolerance_px: f32,
}

impl ClaimFairnessConfig {
    /// Max reconstructed attacker↔target distance that still counts as an overlap: the visual radius
    /// plus interpolation/fractional-tick slack. Beyond this the server could not reconstruct the
    /// overlap the client claimed. SINGLE definition shared by both claim policies (was duplicated as
    /// `26.0` in one and `18 + 8` in the other).
    #[inline]
    pub fn reconstruct_overlap_radius_px(&self) -> f32 {
        self.visible_overlap_radius_px + self.reconstruct_slack_px
    }

    /// The shipped values, for policy UNIT tests that exercise the rules directly (the Room tests
    /// load the real TOML). Kept in lockstep with `gameplay_config.toml` by `claim_fairness_sample_matches_toml`.
    #[cfg(test)]
    pub fn sample() -> Self {
        Self {
            visible_overlap_radius_px: 18.0,
            position_tolerance_px: 12.0,
            reconstruct_slack_px: 8.0,
            // Lead 2026-06-11 wave 4: lead cap (36) + adaptive enemy delay top (18).
            max_view_skew_ticks: 54.0,
            max_process_delay_ticks: 18,
            history_ticks: 96,
            enemy_respawn_eat_grace_ticks: 3,
            visual_distance_tolerance_px: 1.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct GameplayConfig {
    pub map: MapConfig,
    pub gameplay: PlayerConfig,
    pub collision: CollisionConfig,
    pub safe_zone: SafeZoneConfig,
    pub portal: PortalConfig,
    pub spawn: SpawnConfig,
    pub room: RoomConfig,
    pub economy: EconomyConfig,
    pub network: NetworkConfig,
    pub ai: AiConfig,
    #[serde(default)]
    pub filler: FillerConfig,
    pub death_fairness: DeathFairnessConfig,
    pub claim_fairness: ClaimFairnessConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MazeData {
    pub version: u32,
    pub grid_width: usize,
    pub grid_height: usize,
    pub pattern_height: usize,
    pub pattern: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct SharedAssets {
    pub config: Arc<GameplayConfig>,
    pub maze: Arc<MazeData>,
    pub config_hash: String,
    pub maze_hash: String,
}

pub fn load_config_from_str(s: &str) -> Result<GameplayConfig, toml::de::Error> {
    toml::from_str::<GameplayConfig>(s)
}

pub fn load_maze_from_str(s: &str) -> Result<MazeData, serde_json::Error> {
    serde_json::from_str::<MazeData>(s)
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn load_shared_assets(
    config_path: &Path,
    maze_path: &Path,
) -> Result<SharedAssets, Box<dyn std::error::Error>> {
    let config_bytes =
        std::fs::read(config_path).map_err(|e| format!("Failed to read {}: {}", config_path.display(), e))?;
    let maze_bytes =
        std::fs::read(maze_path).map_err(|e| format!("Failed to read {}: {}", maze_path.display(), e))?;

    let config_str = std::str::from_utf8(&config_bytes)?;
    let maze_str = std::str::from_utf8(&maze_bytes)?;

    let config = load_config_from_str(config_str)?;
    let maze = load_maze_from_str(maze_str)?;

    validate(&config, &maze)?;

    Ok(SharedAssets {
        config: Arc::new(config),
        maze: Arc::new(maze),
        config_hash: hash_bytes(&config_bytes),
        maze_hash: hash_bytes(&maze_bytes),
    })
}

fn validate(config: &GameplayConfig, maze: &MazeData) -> Result<(), String> {
    if maze.grid_width != config.map.grid_width {
        return Err(format!(
            "maze.grid_width ({}) != config.map.grid_width ({})",
            maze.grid_width, config.map.grid_width
        ));
    }
    if maze.grid_height != config.map.grid_height {
        return Err(format!(
            "maze.grid_height ({}) != config.map.grid_height ({})",
            maze.grid_height, config.map.grid_height
        ));
    }
    if maze.pattern.len() != maze.pattern_height {
        return Err(format!(
            "maze.pattern.len() ({}) != maze.pattern_height ({})",
            maze.pattern.len(),
            maze.pattern_height
        ));
    }
    for (row_idx, row) in maze.pattern.iter().enumerate() {
        if row.len() != maze.grid_width {
            return Err(format!(
                "maze.pattern[{}].len() ({}) != grid_width ({})",
                row_idx,
                row.len(),
                maze.grid_width
            ));
        }
    }
    if config.room.tick_rate == 0 {
        return Err("tick_rate must be > 0".to_string());
    }
    // The victim-favored enemy-kill radius is `collision_distance_px - VICTIM_GRACE_PX`
    // (game::policies::enemy_contact). If the grace meets or exceeds the collision
    // distance the kill radius is <= 0 and enemies could NEVER kill — fail fast at config
    // load rather than ship a silently-unkillable match. The policy constant is the single
    // source of truth, so this guard tracks any future tuning of the grace.
    let grace = crate::game::policies::enemy_contact::VICTIM_GRACE_PX;
    if config.collision.collision_distance_px <= grace {
        return Err(format!(
            "collision.collision_distance_px ({}) must be > VICTIM_GRACE_PX ({}); the \
             victim-grace kill radius would be <= 0 and enemies could never kill",
            config.collision.collision_distance_px, grace
        ));
    }
    // Client prediction-lead cap. The server multiplies it into the death-claim fallback hold,
    // future-scheduling, and render-skew windows, so a stray TOML value would balloon those. Bound
    // it to a sane band (matches the client's own ComputeLeadTicks clamp) — fail fast at load.
    if !(4..=120).contains(&config.network.max_client_prediction_lead_ticks) {
        return Err(format!(
            "network.max_client_prediction_lead_ticks ({}) must be in 4..=120",
            config.network.max_client_prediction_lead_ticks
        ));
    }
    // Ping cadence: the client pings at this interval and the server derives its ping rate
    // limit from it. 0 would divide-by-zero the derived limit; an absurdly chatty/slow cadence
    // is a config typo — fail fast. (review follow-up: ping limit from config, not a magic 250)
    if !(50..=5000).contains(&config.network.ping_interval_ms) {
        return Err(format!(
            "network.ping_interval_ms ({}) must be in 50..=5000",
            config.network.ping_interval_ms
        ));
    }
    // Spawn protection is a fairness window the client mirrors (via Welcome). Fail fast on a value
    // the client would silently fall back from (0/NaN) or an absurd one — that's exactly the drift
    // moving it to config was meant to kill. Bound it explicitly. (review #3)
    let spawn = config.gameplay.spawn_protection_sec;
    if !spawn.is_finite() || spawn <= 0.0 || spawn > 30.0 {
        return Err(format!("gameplay.spawn_protection_sec ({spawn}) must be finite and in (0, 30]"));
    }
    // Claim-validation tolerances gate every accepted eat / death claim. They must be not just
    // finite+positive but BOUNDED and mutually consistent — an absurd value (typo, or a deliberate
    // attempt to widen the hitbox) would silently turn off anti-cheat / fairness. Anti-cheat
    // tolerances are explicit, bounded, logged, tested (backend rule 9). The visual radius is also
    // forwarded to the client, so a bad value desyncs claim CREATION from VALIDATION too. Fail fast
    // at load. (review Blocker 2)
    let cf = &config.claim_fairness;
    let tr = config.room.tick_rate;
    // (name, value, inclusive upper bound). Lower bound is always > 0.
    let bounded_px: [(&str, f32, f32); 4] = [
        ("visible_overlap_radius_px", cf.visible_overlap_radius_px, 64.0),
        ("position_tolerance_px", cf.position_tolerance_px, 64.0),
        ("reconstruct_slack_px", cf.reconstruct_slack_px, 32.0),
        ("visual_distance_tolerance_px", cf.visual_distance_tolerance_px, 16.0),
    ];
    for (name, v, max) in bounded_px {
        if !v.is_finite() || v <= 0.0 || v > max {
            return Err(format!("claim_fairness.{name} ({v}) must be finite and in (0, {max}]"));
        }
    }
    if !cf.max_view_skew_ticks.is_finite() || cf.max_view_skew_ticks <= 0.0 || cf.max_view_skew_ticks > 600.0
    {
        return Err(format!(
            "claim_fairness.max_view_skew_ticks ({}) must be finite and in (0, 600]",
            cf.max_view_skew_ticks
        ));
    }
    // The accepted reconstructed-overlap radius is the SUM (visual + slack) — bound it EXPLICITLY:
    // two individually-in-band addends could still sum out of band, and this sum IS the anti-cheat gate.
    let reconstruct = cf.reconstruct_overlap_radius_px();
    if !reconstruct.is_finite() || reconstruct > 96.0 {
        return Err(format!(
            "claim_fairness reconstruct-overlap radius ({reconstruct}) must be finite and <= 96"
        ));
    }
    if cf.history_ticks == 0 || cf.history_ticks > tr as usize * 5 {
        return Err(format!("claim_fairness.history_ticks ({}) must be in 1..={}", cf.history_ticks, tr * 5));
    }
    // A claim can be processed at most as late as the history window can still reconstruct it.
    if cf.max_process_delay_ticks == 0 || cf.max_process_delay_ticks > cf.history_ticks as u64 {
        return Err(format!(
            "claim_fairness.max_process_delay_ticks ({}) must be in 1..=history_ticks ({})",
            cf.max_process_delay_ticks, cf.history_ticks
        ));
    }
    if cf.enemy_respawn_eat_grace_ticks > tr {
        return Err(format!(
            "claim_fairness.enemy_respawn_eat_grace_ticks ({}) must be <= tick_rate ({tr})",
            cf.enemy_respawn_eat_grace_ticks
        ));
    }
    Ok(())
}

/// Tick duration derived from tick_rate — NEVER use integer millis division.
/// At tick_rate=60 this returns 16.6666... ms, not 16ms.
pub fn tick_duration(tick_rate: u64) -> std::time::Duration {
    std::time::Duration::from_secs_f64(1.0 / tick_rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Beta-safe boot (lead review P0): a stale toml WITHOUT a [filler] section must parse —
    /// and must come up with fillers DISABLED. The serde(default) escape hatch exists so an
    /// old config can't fail a deploy; it must never surprise-enable bots instead.
    #[test]
    fn missing_filler_section_defaults_to_disabled() {
        let full = include_str!("../gameplay_config.toml");
        // Strip the [filler] section: drop lines from its header to the next section header.
        let mut stripped = String::new();
        let mut in_filler = false;
        for line in full.lines() {
            if line.trim_start().starts_with("[filler]") {
                in_filler = true;
                continue;
            }
            if in_filler && line.trim_start().starts_with('[') {
                in_filler = false;
            }
            if !in_filler {
                stripped.push_str(line);
                stripped.push('\n');
            }
        }
        assert!(!stripped.contains("[filler]"), "test must actually remove the section");
        let cfg = load_config_from_str(&stripped).expect("stale toml without [filler] still boots");
        assert!(!cfg.filler.enabled, "missing [filler] must mean fillers OFF, never ON");
    }

    /// Cross-field validation (lead review): parseable-but-nonsensical effective
    /// configs must fail the boot with a named field, never "survive" weirdly.
    #[test]
    fn filler_validate_rejects_nonsensical_combinations() {
        let ok = FillerConfig { enabled: true, ..FillerConfig::default() };
        assert!(ok.validate(10).is_ok(), "the shipping defaults are sane");

        // Disabled fillers constrain nothing — any garbage passes.
        let off = FillerConfig { enabled: false, target_visible_players: 0, ..FillerConfig::default() };
        assert!(off.validate(10).is_ok());

        let cases: Vec<(FillerConfig, &str)> = vec![
            (
                FillerConfig { enabled: true, target_visible_players: 0, ..FillerConfig::default() },
                "target_visible_players must be >= 1",
            ),
            (
                FillerConfig {
                    enabled: true,
                    target_visible_players: 11,
                    max_transient_visible_players: 12,
                    ..FillerConfig::default()
                },
                "must be <= room.max_players",
            ),
            (
                FillerConfig { enabled: true, max_transient_visible_players: 3, ..FillerConfig::default() },
                "max_transient_visible_players",
            ),
            (
                FillerConfig { enabled: true, exit_delay_min_ticks: 0, ..FillerConfig::default() },
                "exit_delay_min_ticks",
            ),
            (
                FillerConfig {
                    enabled: true,
                    exit_delay_min_ticks: 50,
                    exit_delay_max_ticks: 49,
                    ..FillerConfig::default()
                },
                "exit_delay_max_ticks",
            ),
            (
                FillerConfig { enabled: true, empty_room_grace_ticks: 0, ..FillerConfig::default() },
                "empty_room_grace_ticks",
            ),
        ];
        for (cfg, needle) in cases {
            let err = cfg.validate(10).expect_err("must reject");
            assert!(err.contains(needle), "error '{err}' must name '{needle}'");
        }
    }

    /// The ops kill switch: env beats toml, no env means toml untouched, and a TYPO'D
    /// value is a hard error (a silently-ignored kill switch is the worst failure mode).
    #[test]
    fn filler_env_overrides_beat_toml_and_fail_loud_on_garbage() {
        let mut cfg = FillerConfig { enabled: true, ..FillerConfig::default() };
        // No env set: nothing changes, nothing reported.
        let applied = cfg.apply_env_overrides(|_| None).unwrap();
        assert!(applied.is_empty());
        assert!(cfg.enabled);

        // Kill switch flips the toml value and reports what it did.
        let applied = cfg
            .apply_env_overrides(|k| (k == "FAIRTICK_FILLER_ENABLED").then(|| "false".to_string()))
            .unwrap();
        assert!(!cfg.enabled, "env must beat toml");
        assert_eq!(applied.len(), 1);

        // Numeric override.
        cfg.apply_env_overrides(|k| (k == "FAIRTICK_FILLER_TARGET_VISIBLE_PLAYERS").then(|| "5".to_string()))
            .unwrap();
        assert_eq!(cfg.target_visible_players, 5);

        // Garbage value: hard Err — the boot must fail loudly, not keep bots ON.
        let err = cfg
            .apply_env_overrides(|k| (k == "FAIRTICK_FILLER_ENABLED").then(|| "flase".to_string()))
            .unwrap_err();
        assert!(err.contains("FAIRTICK_FILLER_ENABLED"), "error names the bad key: {err}");
    }

    #[test]
    fn tick_duration_60hz_is_1666_micros() {
        let d = tick_duration(60);
        // 1/60 sec = 16,666.666... microseconds
        assert_eq!(d.as_micros(), 16_666);
    }

    #[test]
    fn tick_duration_not_integer_millis_division() {
        // Integer division 1000/60 = 16ms — the bug we are fixing.
        let buggy = std::time::Duration::from_millis(1000 / 60);
        let fixed = tick_duration(60);
        assert_ne!(buggy, fixed);
        assert!(fixed > buggy);
    }

    #[test]
    fn config_loads_from_str() {
        let s = include_str!("../gameplay_config.toml");
        let cfg = load_config_from_str(s).expect("should parse");
        assert_eq!(cfg.room.tick_rate, 60);
        assert_eq!(cfg.map.width, 500.0);
        assert_eq!(cfg.safe_zone.radius_px, 80.0);
        assert_eq!(cfg.gameplay.base_speed, 100.0);
        assert_eq!(cfg.economy.winner_crystals, 20);
    }

    #[test]
    fn maze_loads_from_str() {
        let s = include_str!("../maze.json");
        let m = load_maze_from_str(s).expect("should parse");
        assert_eq!(m.version, 1);
        assert_eq!(m.grid_width, 20);
        assert_eq!(m.grid_height, 40);
        assert_eq!(m.pattern.len(), 10);
        for row in &m.pattern {
            assert_eq!(row.len(), 20);
        }
    }

    #[test]
    fn hash_is_stable_for_identical_bytes() {
        let bytes = b"hello world";
        let h1 = hash_bytes(bytes);
        let h2 = hash_bytes(bytes);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // sha256 hex
    }

    #[test]
    fn hash_changes_with_one_byte() {
        let h1 = hash_bytes(b"hello world");
        let h2 = hash_bytes(b"hello worlD");
        assert_ne!(h1, h2);
    }

    #[test]
    fn validate_rejects_mismatched_grid() {
        let s = include_str!("../gameplay_config.toml");
        let cfg = load_config_from_str(s).unwrap();
        let bad_maze = MazeData {
            version: 1,
            grid_width: 21,
            grid_height: 40,
            pattern_height: 10,
            pattern: vec![vec![0; 21]; 10],
        };
        assert!(validate(&cfg, &bad_maze).is_err());
    }

    #[test]
    fn validate_rejects_out_of_band_prediction_lead() {
        let mut cfg = load_config_from_str(include_str!("../gameplay_config.toml")).unwrap();
        let maze = load_maze_from_str(include_str!("../maze.json")).unwrap();
        assert!(validate(&cfg, &maze).is_ok(), "the shipped config validates as-is");
        cfg.network.max_client_prediction_lead_ticks = 10_000; // typo: would balloon the death windows
        assert!(validate(&cfg, &maze).is_err(), "an absurd lead is rejected at load");
        cfg.network.max_client_prediction_lead_ticks = 0;
        assert!(validate(&cfg, &maze).is_err(), "a too-small lead is rejected at load");
    }

    #[test]
    fn validate_rejects_bad_spawn_protection() {
        let mut cfg = load_config_from_str(include_str!("../gameplay_config.toml")).unwrap();
        let maze = load_maze_from_str(include_str!("../maze.json")).unwrap();
        assert!(validate(&cfg, &maze).is_ok(), "shipped 3.0s validates");
        for bad in [0.0_f32, -1.0, f32::NAN, f32::INFINITY, 60.0] {
            cfg.gameplay.spawn_protection_sec = bad;
            assert!(
                validate(&cfg, &maze).is_err(),
                "spawn_protection_sec={bad} must be rejected (would drift from the client's blink)"
            );
        }
    }

    /// The `ClaimFairnessConfig::sample()` fixture the policy UNIT tests use must equal the shipped
    /// TOML — else those tests would pass against stale numbers while the server runs different ones.
    #[test]
    fn claim_fairness_sample_matches_toml() {
        let cfg = load_config_from_str(include_str!("../gameplay_config.toml")).unwrap();
        assert_eq!(cfg.claim_fairness, ClaimFairnessConfig::sample());
    }

    #[test]
    fn validate_rejects_bad_claim_fairness() {
        let base = load_config_from_str(include_str!("../gameplay_config.toml")).unwrap();
        let maze = load_maze_from_str(include_str!("../maze.json")).unwrap();
        assert!(validate(&base, &maze).is_ok(), "shipped claim_fairness validates");

        // Each mutation must independently fail (lower AND upper bounds + cross-field), so an
        // absurd value can't silently widen/disable anti-cheat. (review Blocker 2)
        type Mutate = fn(&mut GameplayConfig);
        let cases: Vec<(&str, Mutate)> = vec![
            ("zero visual_radius", |c| c.claim_fairness.visible_overlap_radius_px = 0.0),
            ("NaN visual_radius", |c| c.claim_fairness.visible_overlap_radius_px = f32::NAN),
            ("absurd visual_radius", |c| c.claim_fairness.visible_overlap_radius_px = 10000.0),
            ("absurd position_tolerance", |c| c.claim_fairness.position_tolerance_px = 5000.0),
            ("absurd reconstruct_slack", |c| c.claim_fairness.reconstruct_slack_px = 10000.0),
            ("inf visual_distance_tolerance", |c| {
                c.claim_fairness.visual_distance_tolerance_px = f32::INFINITY
            }),
            ("absurd visual_distance_tolerance", |c| c.claim_fairness.visual_distance_tolerance_px = 9999.0),
            ("absurd max_view_skew", |c| c.claim_fairness.max_view_skew_ticks = 1_000_000.0),
            ("zero max_process_delay", |c| c.claim_fairness.max_process_delay_ticks = 0),
            ("process_delay > history", |c| c.claim_fairness.max_process_delay_ticks = 999_999),
            ("zero history_ticks", |c| c.claim_fairness.history_ticks = 0),
            ("absurd history_ticks", |c| c.claim_fairness.history_ticks = 100_000_000),
            ("absurd respawn_grace", |c| c.claim_fairness.enemy_respawn_eat_grace_ticks = 999_999),
        ];
        for (name, mutate) in cases {
            let mut cfg = base.clone();
            mutate(&mut cfg);
            assert!(validate(&cfg, &maze).is_err(), "claim_fairness must reject: {name}");
        }

        // Reconstruct-radius (sum) bound. NOTE: with the current caps (64 + 32) the max sum is exactly
        // 96, so the sum check is REDUNDANT with the individual caps today — kept explicit so that if
        // the individual caps are ever raised, the actual anti-cheat gate (the sum) stays bounded.
        // Here we assert the boundary (sum == 96 allowed) and that the individual cap fires above it.
        let mut cfg = base.clone();
        cfg.claim_fairness.visible_overlap_radius_px = 64.0;
        cfg.claim_fairness.reconstruct_slack_px = 32.0; // 64 + 32 = 96, exactly at the cap
        assert!(validate(&cfg, &maze).is_ok(), "reconstruct sum exactly at the 96 cap is allowed");
        cfg.claim_fairness.visible_overlap_radius_px = 64.1; // over its own 64 cap (would also push sum > 96)
        assert!(validate(&cfg, &maze).is_err(), "out-of-band radius rejected (individual cap fires here)");
    }
}
