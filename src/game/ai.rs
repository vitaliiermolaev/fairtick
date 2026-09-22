use crate::config_shared::GameplayConfig;
use crate::game::map::GameMap;
use crate::game::player::Player;
use crate::game::rng::RoomRng;
use crate::protocol::{Direction, EnemyState, Position};
use std::sync::Arc;
use uuid::Uuid;

/// AI cell-center snap logs are pure diagnostics and, at one move-step per tick,
/// fire constantly — they used to drown the claim/death/reject lines. Keep them
/// OFF by default; set `FAIRTICK_LOG_AI=1` to surface them at WARN. Cached once so
/// the per-turn hot path doesn't re-read the environment.
fn ai_log_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("FAIRTICK_LOG_AI").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false)
    })
}

#[derive(Debug, Clone)]
pub struct Enemy {
    pub id: String,
    /// Display name shown to players ("bot1", "bot2", …).
    pub nickname: String,
    pub position: Position,
    pub score: u32,
    /// The score this bot is (re)spawned with — bots keep their identity across
    /// respawns instead of all resetting to one value.
    start_score: u32,
    pub direction: Direction,
    pub desired_direction: Direction,
    pub speed: f32,
    /// Server tick at which the AI re-rolls its desired direction.
    pub next_direction_change_tick: u64,
    /// Server tick this bot last (re)spawned. A short eat-grace after respawn
    /// stops the same bot being eaten again the very next tick when it happens to
    /// respawn inside the eater's mouth (the double-EnemyRespawned bug).
    pub respawned_at_tick: u64,
    /// Increments on every respawn. The `id` is stable across respawns, so logs
    /// of the same `id` at different generations are DIFFERENT lives of one bot —
    /// not "ate the same enemy twice". Starts at 0 for the initial spawn.
    pub generation: u32,
    config: Arc<GameplayConfig>,
}

impl Enemy {
    pub fn new(
        position: Position,
        config: Arc<GameplayConfig>,
        start_tick: u64,
        nickname: String,
        start_score: u32,
    ) -> Self {
        let speed = config.ai.speed;
        Self {
            // Short opaque id (see Player::new): rides in every snapshot delta; a full
            // uuid here was egress weight, not information. Stable across respawns
            // (generations name the lives); STATISTICALLY unique, not enforced.
            id: format!("e_{}", &Uuid::new_v4().simple().to_string()[..8]),
            nickname,
            position,
            score: start_score,
            start_score,
            direction: Direction::Right,
            desired_direction: Direction::Right,
            speed,
            // Pick first direction after 2 seconds of game time.
            next_direction_change_tick: start_tick + 2 * config.room.tick_rate,
            respawned_at_tick: start_tick,
            generation: 0,
            config,
        }
    }

    pub fn update(
        &mut self,
        current_tick: u64,
        dt: f32,
        players: &[Player],
        map: &GameMap,
        rng: &mut RoomRng,
    ) {
        if current_tick >= self.next_direction_change_tick {
            if let Some(target) = self.find_nearest_player(players) {
                self.desired_direction = self.get_direction_to_target(&target.position);
            } else {
                self.desired_direction = self.random_direction(rng);
            }
            let wait_sec = rng.range_f32(1.0..3.0);
            self.next_direction_change_tick =
                current_tick + (wait_sec * self.config.room.tick_rate as f32) as u64;
        }

        let (grid_x, grid_y) = map.pos_to_grid(&self.position);
        let cell_center = map.grid_to_pos(grid_x, grid_y);

        let dx_to_center = (self.position.x - cell_center.x).abs();
        let dy_to_center = (self.position.y - cell_center.y).abs();
        // Turn near the cell centre, but cap the corrective grid-snap to about one
        // tick of movement (subpixel) instead of turn_threshold_px (4px). With a
        // zoomed-in follow camera, a 4px snap reads as jitter.
        let tick_move_px = self.speed / self.config.room.tick_rate as f32;
        let turn_threshold = (tick_move_px + 0.10).max(0.35);
        let near_center = dx_to_center < turn_threshold && dy_to_center < turn_threshold;

        if self.desired_direction != self.direction
            && near_center
            && self.can_move_in_direction(&self.desired_direction, map)
        {
            // DIAG (log only): how big is the cell-center snap on a turn? If a
            // visible enemy jerk never coincides with one of these, AI snap is
            // ruled out as the cause.
            let snap_dist = ((cell_center.x - self.position.x).powi(2)
                + (cell_center.y - self.position.y).powi(2))
            .sqrt();
            if snap_dist > 0.75 && ai_log_enabled() {
                tracing::warn!(
                        "🤖 EnemyAISnapTurn enemy={} tick={} snap={:.2} from=({:.2},{:.2}) to=({:.2},{:.2}) dir={:?}->{:?}",
                        self.id, current_tick, snap_dist,
                        self.position.x, self.position.y, cell_center.x, cell_center.y,
                        self.direction, self.desired_direction
                    );
            }
            self.position.x = cell_center.x;
            self.position.y = cell_center.y;
            self.direction = self.desired_direction;
        }

        if near_center && !self.can_move_in_direction(&self.direction, map) {
            let directions = [Direction::Up, Direction::Down, Direction::Left, Direction::Right];
            for &dir in &directions {
                if self.can_move_in_direction(&dir, map) {
                    let snap_dist = ((cell_center.x - self.position.x).powi(2)
                        + (cell_center.y - self.position.y).powi(2))
                    .sqrt();
                    if snap_dist > 0.75 && ai_log_enabled() {
                        tracing::warn!(
                            "🤖 EnemyAISnapBlocked enemy={} tick={} snap={:.2} from=({:.2},{:.2}) to=({:.2},{:.2}) dir={:?}->{:?}",
                            self.id, current_tick, snap_dist,
                            self.position.x, self.position.y, cell_center.x, cell_center.y,
                            self.direction, dir
                        );
                    }
                    self.position.x = cell_center.x;
                    self.position.y = cell_center.y;
                    self.direction = dir;
                    break;
                }
            }
        }

        self.move_in_direction(dt, map);
    }

    fn can_move_in_direction(&self, direction: &Direction, map: &GameMap) -> bool {
        let (grid_x, grid_y) = map.pos_to_grid(&self.position);
        // Explicit bounds: saturating_sub(1) on the top/left edge returns index 0
        // (the SAME cell), not out-of-bounds, so a non-wall edge cell wrongly let
        // the AI step Up at grid_y==0 / Left at grid_x==0 and walk off the map into
        // negative coords. Reject those moves outright. (Confirmed: enemies reached
        // server x/y ≈ -2000..-3000.)
        let (next_x, next_y) = match direction {
            Direction::Up => {
                if grid_y == 0 {
                    return false;
                }
                (grid_x, grid_y - 1)
            }
            Direction::Down => {
                if grid_y + 1 >= map.grid_height {
                    return false;
                }
                (grid_x, grid_y + 1)
            }
            Direction::Left => {
                if grid_x == 0 {
                    return false;
                }
                (grid_x - 1, grid_y)
            }
            Direction::Right => {
                if grid_x + 1 >= map.grid_width {
                    return false;
                }
                (grid_x + 1, grid_y)
            }
        };
        !map.is_wall(next_x, next_y)
    }

    fn move_in_direction(&mut self, dt: f32, map: &GameMap) {
        let cell_size = map.cell_size;
        let distance = self.speed * dt;

        let (grid_x, grid_y) = map.pos_to_grid(&self.position);
        let cell_center = map.grid_to_pos(grid_x, grid_y);

        let cell_left = grid_x as f32 * cell_size;
        let cell_right = cell_left + cell_size;
        let cell_top = grid_y as f32 * cell_size;
        let cell_bottom = cell_top + cell_size;

        let next_cell_is_wall = !self.can_move_in_direction(&self.direction, map);

        let mut next_pos = self.position;
        match self.direction {
            Direction::Up => {
                next_pos.y -= distance;
                if next_cell_is_wall {
                    next_pos.y = next_pos.y.max(cell_top + cell_size / 2.0);
                }
            }
            Direction::Down => {
                next_pos.y += distance;
                if next_cell_is_wall {
                    next_pos.y = next_pos.y.min(cell_bottom - cell_size / 2.0);
                }
            }
            Direction::Left => {
                next_pos.x -= distance;
                if next_cell_is_wall {
                    next_pos.x = next_pos.x.max(cell_left + cell_size / 2.0);
                }
            }
            Direction::Right => {
                next_pos.x += distance;
                if next_cell_is_wall {
                    next_pos.x = next_pos.x.min(cell_right - cell_size / 2.0);
                }
            }
        }

        self.position = next_pos;

        match self.direction {
            Direction::Up | Direction::Down => {
                self.position.x = cell_center.x;
            }
            Direction::Left | Direction::Right => {
                self.position.y = cell_center.y;
            }
        }

        // Safety net: even if a future movement/turn bug slips through, never emit a
        // position outside the map — the client should never receive x/y ≈ -3000.
        self.clamp_to_map(map);
    }

    /// Clamp the enemy to the playable area (half a cell in from each edge so the
    /// dot centre stays inside the border cells).
    fn clamp_to_map(&mut self, map: &GameMap) {
        let half = map.cell_size * 0.5;
        self.position.x = self.position.x.clamp(half, map.width - half);
        self.position.y = self.position.y.clamp(half, map.height - half);
    }

    fn find_nearest_player(&self, players: &[Player]) -> Option<Player> {
        players
            .iter()
            .filter(|p| p.is_alive)
            // A non-finite distance (degenerate position) is EXCLUDED outright. total_cmp
            // alone is not enough: IEEE totalOrder puts NEGATIVE NaN below -inf — and the
            // default x86 quiet NaN (e.g. 0.0/0.0) is negative and survives dx*dx+dy*dy and
            // sqrt — so a -NaN distance would sort as NEAREST and every enemy would silently
            // chase the phantom. Filtering re-establishes the real invariant: a NaN-position
            // player is never picked, regardless of NaN sign.
            .filter(|p| self.distance_to(&p.position).is_finite())
            .min_by(|a, b| {
                let dist_a = self.distance_to(&a.position);
                let dist_b = self.distance_to(&b.position);
                // total_cmp, not partial_cmp().unwrap(): comparing must NOT panic the room
                // update — it would unwind mid-tick and the room manager would have to evict
                // the room. (Non-finite candidates are already filtered out above.)
                dist_a.total_cmp(&dist_b)
            })
            .cloned()
    }

    fn distance_to(&self, pos: &Position) -> f32 {
        let dx = self.position.x - pos.x;
        let dy = self.position.y - pos.y;
        (dx * dx + dy * dy).sqrt()
    }

    fn get_direction_to_target(&self, target: &Position) -> Direction {
        let dx = target.x - self.position.x;
        let dy = target.y - self.position.y;

        if dx.abs() > dy.abs() {
            if dx > 0.0 {
                Direction::Right
            } else {
                Direction::Left
            }
        } else if dy > 0.0 {
            Direction::Down
        } else {
            Direction::Up
        }
    }

    fn random_direction(&self, rng: &mut RoomRng) -> Direction {
        match rng.range_usize(0..4) {
            0 => Direction::Up,
            1 => Direction::Down,
            2 => Direction::Left,
            _ => Direction::Right,
        }
    }

    pub fn can_eat(&self, player: &Player) -> bool {
        if !player.is_alive || player.is_invincible {
            return false;
        }
        self.score > player.score
    }

    pub fn can_be_eaten_by(&self, player: &Player) -> bool {
        if !player.is_alive {
            return false;
        }
        player.is_invincible || player.score > self.score
    }

    pub fn respawn(&mut self, position: Position, direction: Direction, current_tick: u64) {
        self.position = position;
        self.score = self.start_score; // keep this bot's identity across respawns
                                       // Reset heading too: a stale direction in a fresh cell makes the AI
                                       // grid-snap on the very next tick (a visible jump under the zoom camera).
        self.direction = direction;
        self.desired_direction = direction;
        self.next_direction_change_tick = current_tick + self.config.room.tick_rate;
        self.respawned_at_tick = current_tick;
        self.generation = self.generation.saturating_add(1);
    }

    pub fn to_state(&self) -> EnemyState {
        EnemyState {
            id: self.id.clone(),
            nickname: self.nickname.clone(),
            position: self.position,
            direction: self.direction,
            speed: self.speed,
            score: self.score,
            generation: self.generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_shared::{load_config_from_str, load_maze_from_str};

    fn make_map() -> (Arc<GameplayConfig>, GameMap) {
        let config = Arc::new(load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap());
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let mut rng = RoomRng::from_seed(1);
        let map = GameMap::new(&config, &maze, &mut rng);
        (config, map)
    }

    fn make_enemy(config: Arc<GameplayConfig>) -> Enemy {
        Enemy::new(Position { x: 0.0, y: 0.0 }, config, 0, "bot1".to_string(), 10)
    }

    /// The escape bug: saturating_sub(1) let the AI step Up at grid_y==0 / Left at
    /// grid_x==0 (it re-checked cell 0, not out-of-bounds) and walk off the map.
    #[test]
    fn ai_cannot_step_off_top_or_left_edge() {
        let (config, map) = make_map();
        let mut enemy = make_enemy(config);

        enemy.position = map.grid_to_pos(map.grid_width / 2, 0); // top row
        assert!(
            !enemy.can_move_in_direction(&Direction::Up, &map),
            "AI must not step Up off the top edge (grid_y==0)"
        );

        enemy.position = map.grid_to_pos(0, map.grid_height / 2); // left column
        assert!(
            !enemy.can_move_in_direction(&Direction::Left, &map),
            "AI must not step Left off the left edge (grid_x==0)"
        );
    }

    /// Safety net: clamp pins any out-of-bounds position back inside the map.
    #[test]
    fn clamp_keeps_enemy_inside_map() {
        let (config, map) = make_map();
        let mut enemy = make_enemy(config);
        let half = map.cell_size * 0.5;

        enemy.position = Position { x: -5000.0, y: -5000.0 };
        enemy.clamp_to_map(&map);
        assert!(enemy.position.x >= half && enemy.position.x <= map.width - half);
        assert!(enemy.position.y >= half && enemy.position.y <= map.height - half);

        enemy.position = Position { x: 99_999.0, y: 99_999.0 };
        enemy.clamp_to_map(&map);
        assert!(enemy.position.x <= map.width - half);
        assert!(enemy.position.y <= map.height - half);
    }

    /// A NaN-position player must NEVER be picked as the nearest target — for EITHER NaN
    /// sign. total_cmp alone only guarantees this for +NaN (IEEE totalOrder puts -NaN below
    /// -inf, and the default x86 quiet NaN is negative), so the non-finite filter is the
    /// invariant under test. The old partial_cmp().unwrap() panicked here; now the
    /// degenerate candidate is skipped and the finite player wins, without a panic.
    #[test]
    fn nan_position_player_is_never_targeted() {
        let (config, _map) = make_map();
        let mut enemy = make_enemy(config.clone());
        enemy.position = Position { x: 100.0, y: 100.0 };

        let finite =
            Player::new("u_far".into(), "far".into(), Position { x: 900.0, y: 900.0 }, config.clone());
        for nan in [f32::NAN, -f32::NAN] {
            let mut phantom =
                Player::new("u_nan".into(), "phantom".into(), Position { x: 0.0, y: 0.0 }, config.clone());
            phantom.position = Position { x: nan, y: nan };

            let target = enemy
                .find_nearest_player(&[phantom, finite.clone()])
                .expect("the finite player must be found");
            assert_eq!(
                target.nickname,
                "far",
                "NaN (sign bit {}) position must never win nearest-target selection",
                nan.is_sign_negative()
            );
        }
        // ALL candidates degenerate → no target at all (not a phantom chase, not a panic).
        let mut phantom = Player::new("u_nan".into(), "phantom".into(), Position { x: 0.0, y: 0.0 }, config);
        phantom.position = Position { x: -f32::NAN, y: 0.0 };
        assert!(enemy.find_nearest_player(&[phantom]).is_none());
    }

    /// Drive a real AI for a full match worth of ticks: it must never emit a
    /// position outside the map (this is exactly what reached the client as -3000).
    #[test]
    fn ai_never_escapes_map_over_many_ticks() {
        let (config, map) = make_map();
        let mut rng = RoomRng::from_seed(7);
        let mut enemy = make_enemy(config);
        // Start it hard against the top-left so it keeps probing the edges.
        enemy.position = map.grid_to_pos(0, 0);
        let players: Vec<Player> = Vec::new();
        let dt = 1.0 / 60.0;
        for tick in 0..3600u64 {
            enemy.update(tick, dt, &players, &map, &mut rng);
            assert!(
                enemy.position.x >= 0.0 && enemy.position.x <= map.width,
                "enemy x off-map at tick {}: {}",
                tick,
                enemy.position.x
            );
            assert!(
                enemy.position.y >= 0.0 && enemy.position.y <= map.height,
                "enemy y off-map at tick {}: {}",
                tick,
                enemy.position.y
            );
        }
    }
}
