use crate::config_shared::GameplayConfig;
use crate::game::map::GameMap;
use crate::protocol::{Direction, Position};

/// Axis the player is moving along. Retained for wire/state compatibility
/// (`PlayerSimState`, `StatePatch`, the golden fixture), but it no longer
/// constrains movement — see `simulate_player_tick`, which is now FREE
/// (lane-locking removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MoveAxis {
    Horizontal,
    Vertical,
}

impl MoveAxis {
    pub fn for_direction(d: Direction) -> Self {
        match d {
            Direction::Left | Direction::Right => MoveAxis::Horizontal,
            Direction::Up | Direction::Down => MoveAxis::Vertical,
        }
    }
}

/// Movement state contract — what both Rust and the client simulate.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlayerSimState {
    pub position: Position,
    pub direction: Direction,
    pub desired_direction: Direction,
    pub speed: f32,
    pub axis: MoveAxis,
    pub lane_center: f32,
}

impl PlayerSimState {
    /// Derive axis + lane_center from a freshly-set position + direction.
    /// Kept for compatibility; movement no longer reads `lane_center`.
    pub fn lane_for_spawn(position: Position, direction: Direction) -> (MoveAxis, f32) {
        let axis = MoveAxis::for_direction(direction);
        let lane_center = match axis {
            MoveAxis::Horizontal => position.y,
            MoveAxis::Vertical => position.x,
        };
        (axis, lane_center)
    }
}

/// Pure movement step — FREE movement (no lane-locking). Same input → same
/// output. Mirrored byte-for-byte in the client `PlayerSim.Step`.
///
///   1. Turn is instant — the player faces `desired_direction` immediately,
///      at any position (no waiting for a cell centre).
///   2. Advance `speed * dt` along that direction.
///   3. Clamp the position to the field bounds [0, width] x [0, height].
pub fn simulate_player_tick(
    mut state: PlayerSimState,
    map: &GameMap,
    config: &GameplayConfig,
) -> PlayerSimState {
    let dt = 1.0_f32 / config.room.tick_rate as f32;

    // Instant turn — free movement.
    state.direction = state.desired_direction;
    state.axis = MoveAxis::for_direction(state.direction);

    let dist = state.speed * dt;
    match state.direction {
        Direction::Up => state.position.y -= dist,
        Direction::Down => state.position.y += dist,
        Direction::Left => state.position.x -= dist,
        Direction::Right => state.position.x += dist,
    }

    // Keep inside the field.
    if state.position.x < 0.0 {
        state.position.x = 0.0;
    } else if state.position.x > map.width {
        state.position.x = map.width;
    }
    if state.position.y < 0.0 {
        state.position.y = 0.0;
    } else if state.position.y > map.height {
        state.position.y = map.height;
    }

    // Keep lane_center consistent with position (unused by movement now).
    state.lane_center = match state.axis {
        MoveAxis::Horizontal => state.position.y,
        MoveAxis::Vertical => state.position.x,
    };

    state
}

/// Walk-test still used by the room's Stage 6.5 turn-intent buffer. With the
/// empty map this is true everywhere except at the grid edge.
pub fn can_step(pos: &Position, direction: &Direction, map: &GameMap) -> bool {
    let (gx, gy) = map.pos_to_grid(pos);
    let (nx, ny) = match direction {
        Direction::Up => (gx, gy.saturating_sub(1)),
        Direction::Down => (gx, gy + 1),
        Direction::Left => (gx.saturating_sub(1), gy),
        Direction::Right => (gx + 1, gy),
    };
    !map.is_wall(nx, ny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_shared::{load_config_from_str, load_maze_from_str};
    use crate::game::rng::RoomRng;
    use std::sync::Arc;

    fn make_map() -> (Arc<GameplayConfig>, GameMap) {
        let cfg = Arc::new(load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap());
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let mut rng = RoomRng::from_seed(0);
        let map = GameMap::new(&cfg, &maze, &mut rng);
        (cfg, map)
    }

    fn spawn(position: Position, direction: Direction) -> PlayerSimState {
        let (axis, lane_center) = PlayerSimState::lane_for_spawn(position, direction);
        PlayerSimState { position, direction, desired_direction: direction, speed: 100.0, axis, lane_center }
    }

    #[test]
    fn same_input_same_output() {
        let (cfg, map) = make_map();
        let s = spawn(Position { x: 100.0, y: 200.0 }, Direction::Right);
        assert_eq!(simulate_player_tick(s, &map, &cfg), simulate_player_tick(s, &map, &cfg));
    }

    #[test]
    fn moves_straight_at_speed() {
        let (cfg, map) = make_map();
        let s = spawn(Position { x: 100.0, y: 200.0 }, Direction::Right);
        let n = simulate_player_tick(s, &map, &cfg);
        let expected_x = 100.0_f32 + 100.0 / 60.0;
        assert!((n.position.x - expected_x).abs() < 0.001, "x={}", n.position.x);
        assert!((n.position.y - 200.0).abs() < 0.001, "y={}", n.position.y);
    }

    /// The whole point of removing lanes: a turn applies immediately, at any
    /// position, with no snapping to a cell centre.
    #[test]
    fn turns_instantly_anywhere() {
        let (cfg, map) = make_map();
        let mut s = spawn(Position { x: 107.0, y: 203.0 }, Direction::Right);
        s.desired_direction = Direction::Down;
        let n = simulate_player_tick(s, &map, &cfg);
        assert_eq!(n.direction, Direction::Down);
        let expected_y = 203.0_f32 + 100.0 / 60.0;
        assert!((n.position.y - expected_y).abs() < 0.001, "y={}", n.position.y);
        assert!((n.position.x - 107.0).abs() < 0.001, "x stayed put: {}", n.position.x);
    }

    #[test]
    fn clamps_to_field_bounds() {
        let (cfg, map) = make_map();
        let mut s = spawn(Position { x: 1.0, y: 200.0 }, Direction::Left);
        s.speed = 10_000.0;
        let n = simulate_player_tick(s, &map, &cfg);
        assert!((n.position.x - 0.0).abs() < 0.001, "left clamp x={}", n.position.x);

        let mut s2 = spawn(Position { x: map.width - 1.0, y: map.height - 1.0 }, Direction::Right);
        s2.speed = 10_000.0;
        let n2 = simulate_player_tick(s2, &map, &cfg);
        assert!((n2.position.x - map.width).abs() < 0.001, "right clamp x={}", n2.position.x);
    }
}
