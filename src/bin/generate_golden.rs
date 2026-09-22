// Generates tests/fixtures/sim_parity.json — the cross-platform parity fixture.
//
// Both Rust (cargo test) and iOS (SimParityRunner) replay the same scenarios
// and assert that their pure simulation matches the recorded trajectory to
// within 0.001px. If the file is missing or stale, run:
//
//   cargo run --bin generate_golden
//
// then re-run `scripts/generate_ios_assets.sh` so iOS picks up the new fixture.

use fairtick::config_shared::{load_config_from_str, load_maze_from_str};
use fairtick::game::map::GameMap;
use fairtick::game::rng::RoomRng;
use fairtick::game::sim::{simulate_player_tick, MoveAxis, PlayerSimState};
use fairtick::protocol::{Direction, Position};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Serialize, Deserialize)]
pub struct InputChange {
    pub at_tick: u64,
    pub direction: Direction,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub initial: PlayerSimState,
    /// Direction changes scheduled at exact tick boundaries.
    pub inputs: Vec<InputChange>,
    pub total_ticks: u64,
    /// Position recorded at each `sample_every` tick boundary (1-based) so the
    /// JSON stays compact. Mismatch on ANY sample fails parity.
    pub sample_every: u64,
    pub samples: Vec<Position>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GoldenFixture {
    pub version: u32,
    pub config_hash: String,
    pub maze_hash: String,
    pub scenarios: Vec<Scenario>,
}

fn run_scenario(
    name: &str,
    initial: PlayerSimState,
    inputs: Vec<InputChange>,
    total_ticks: u64,
    sample_every: u64,
    map: &GameMap,
    config: &fairtick::config_shared::GameplayConfig,
) -> Scenario {
    let mut state = initial;
    let mut samples = Vec::with_capacity((total_ticks / sample_every) as usize);

    for tick in 1..=total_ticks {
        // Apply scheduled input BEFORE the simulation step for this tick.
        if let Some(change) = inputs.iter().find(|c| c.at_tick == tick) {
            state.desired_direction = change.direction;
        }
        state = simulate_player_tick(state, map, config);
        if tick % sample_every == 0 {
            samples.push(state.position);
        }
    }

    Scenario { name: name.to_string(), initial, inputs, total_ticks, sample_every, samples }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config_bytes = std::fs::read("gameplay_config.toml")?;
    let maze_bytes = std::fs::read("maze.json")?;
    let config = load_config_from_str(std::str::from_utf8(&config_bytes)?)?;
    let maze = load_maze_from_str(std::str::from_utf8(&maze_bytes)?)?;

    use sha2::{Digest, Sha256};
    let config_hash = {
        let mut h = Sha256::new();
        h.update(&config_bytes);
        format!("{:x}", h.finalize())
    };
    let maze_hash = {
        let mut h = Sha256::new();
        h.update(&maze_bytes);
        format!("{:x}", h.finalize())
    };

    let config_arc = std::sync::Arc::new(config.clone());
    let maze_arc = std::sync::Arc::new(maze);
    let mut rng = RoomRng::from_seed(0);
    let map = GameMap::new(&config_arc, &maze_arc, &mut rng);

    // Helper: build a freshly-spawned state. With axis-locked movement,
    // the spawn helper guarantees axis/lane_center match position+direction.
    let spawn = |pos: Position, dir: Direction| -> PlayerSimState {
        let (axis, lane_center) = PlayerSimState::lane_for_spawn(pos, dir);
        PlayerSimState {
            position: pos,
            direction: dir,
            desired_direction: dir,
            speed: config.gameplay.base_speed,
            axis,
            lane_center,
        }
    };

    // Scenarios cover (a) straight movement, (b) 90° side turn,
    // (c) multi-turn over 1000 ticks, (d) the new lane-locked reverse —
    // auditor's specifically requested SimParity-reverse scenario.
    let scenarios = vec![
        run_scenario(
            "straight_right_500_ticks",
            spawn(Position { x: 37.5, y: 437.5 }, Direction::Right),
            vec![],
            500,
            10,
            &map,
            &config,
        ),
        run_scenario(
            "turn_down_at_intersection",
            spawn(Position { x: 37.5, y: 437.5 }, Direction::Right),
            vec![InputChange { at_tick: 30, direction: Direction::Down }],
            200,
            5,
            &map,
            &config,
        ),
        run_scenario(
            "multi_turn_1000_ticks",
            spawn(Position { x: 37.5, y: 37.5 + 25.0 }, Direction::Down),
            vec![
                InputChange { at_tick: 100, direction: Direction::Right },
                InputChange { at_tick: 250, direction: Direction::Down },
                InputChange { at_tick: 400, direction: Direction::Left },
                InputChange { at_tick: 600, direction: Direction::Up },
                InputChange { at_tick: 800, direction: Direction::Right },
            ],
            1000,
            10,
            &map,
            &config,
        ),
        // Auditor: reverse must be instant, no snap, lane unchanged.
        // Player runs right for 60 ticks, then reverses 5 times back and
        // forth. y must stay 437.5 the entire scenario; x oscillates.
        run_scenario(
            "reverse_oscillation_300_ticks",
            spawn(Position { x: 137.5, y: 437.5 }, Direction::Right),
            vec![
                InputChange { at_tick: 60, direction: Direction::Left },
                InputChange { at_tick: 120, direction: Direction::Right },
                InputChange { at_tick: 180, direction: Direction::Left },
                InputChange { at_tick: 240, direction: Direction::Right },
            ],
            300,
            5,
            &map,
            &config,
        ),
    ];
    let _ = MoveAxis::Horizontal; // silence unused import on optimized builds

    let fixture = GoldenFixture { version: 1, config_hash, maze_hash, scenarios };

    let out_path = Path::new("tests/fixtures/sim_parity.json");
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&fixture)?;
    std::fs::write(out_path, json)?;
    println!("Wrote {} with {} scenarios", out_path.display(), fixture.scenarios.len());

    write_snapshot_wire_fixture()?;
    Ok(())
}

/// Protocol v6 wire fixture: the SAME keyframe + delta serialized BOTH ways — bincode
/// (snapshot_keyframe.bin / snapshot_delta.bin, the binary snapshot lane) and JSON
/// (snapshot_frames.json, the legacy/text path the Unity codec already parses). The Unity
/// test decodes both and asserts field equality, pinning the C# BincodeDecoder to the Rust
/// encoder byte-for-byte. Values are handpicked to exercise every field shape: fractional
/// f32 (exactly representable), every Direction, Some/None Options, every entity kind
/// (players/enemies/boosters/POINTS — each has its own decode path), an empty Vec, and
/// every bincode varint marker the snapshot types can hit: inline (<251), 251→u16
/// (score 251), 252→u32 (last_event_id 70_000), 253→u64 (server_time_ms).
fn snapshot_fixture_messages() -> (fairtick::protocol::ServerMessage, fairtick::protocol::ServerMessage) {
    use fairtick::protocol as p;

    let keyframe = p::ServerMessage::GameState(p::GameStateUpdate {
        players: vec![
            p::PlayerState {
                id: "p_0a1b2c3d".into(),
                nickname: "alice".into(),
                position: p::Position { x: 123.5, y: -0.25 },
                direction: p::Direction::Up,
                score: 251, // varint u16 boundary
                speed: 100.0,
                is_invincible: true,
                invincibility_remaining: 1.5,
            },
            p::PlayerState {
                id: "p_ffeeddcc".into(),
                nickname: "bob".into(),
                position: p::Position { x: 0.0, y: 999.75 },
                direction: p::Direction::Left,
                score: 7,
                speed: 130.0,
                is_invincible: false,
                invincibility_remaining: 0.0,
            },
        ],
        enemies: vec![p::EnemyState {
            id: "e_00112233".into(),
            nickname: "bot1".into(),
            position: p::Position { x: 64.0, y: 64.0 },
            direction: p::Direction::Down,
            speed: 100.0,
            score: 300,
            generation: 4,
        }],
        boosters: vec![p::Booster {
            id: "boost-1".into(),
            position: p::Position { x: 32.0, y: 96.0 },
            booster_type: p::BoosterType::Mushroom,
        }],
        points: vec![p::PointItem { id: "pt_0001".into(), position: p::Position { x: 16.0, y: 48.5 } }],
        portal: Some(p::PortalState { position: p::Position { x: 250.0, y: 500.0 } }),
        time_remaining: 37,
        tick: 1200,
        server_time_ms: 1_781_000_000_123,
        last_processed_input_seq: Some(42),
        last_event_id: None,
        snapshot_seq: 0,
        // Some + a 251→u16 varint payload: pins the trailing request-id echo's Option
        // tag AND its >250 encoding in the binary lane (None is covered by the delta-
        // less per-tick fulls the live server sends).
        full_state_request_id: Some(9001),
    });
    let delta = p::ServerMessage::GameStateDelta(p::GameStateDelta {
        players: vec![p::PlayerPositionUpdate {
            id: "p_0a1b2c3d".into(),
            position: p::Position { x: 124.75, y: -0.25 },
            direction: p::Direction::Right,
            speed: 100.0,
            score: 252,
        }],
        enemies: vec![p::EnemyPositionUpdate {
            id: "e_00112233".into(),
            position: p::Position { x: 65.5, y: 64.0 },
            direction: p::Direction::Down,
            speed: 100.0,
            generation: 4,
        }],
        tick: 1234,
        time_remaining: 36,
        server_time_ms: 1_781_000_000_456,
        last_processed_input_seq: Some(43),
        // > 65535 on purpose: hits the bincode varint 252→u32 marker (9000 would only
        // re-test the 251→u16 path that score=251 already covers).
        last_event_id: Some(70_000),
        snapshot_seq: 617,
    });
    (keyframe, delta)
}

fn write_snapshot_wire_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let (keyframe, delta) = snapshot_fixture_messages();
    std::fs::write("tests/fixtures/snapshot_keyframe.bin", keyframe.serialize()?)?;
    std::fs::write("tests/fixtures/snapshot_delta.bin", delta.serialize()?)?;
    let frames_json = serde_json::json!({
        "keyframe": serde_json::to_value(&keyframe)?,
        "delta": serde_json::to_value(&delta)?,
    });
    std::fs::write("tests/fixtures/snapshot_frames.json", serde_json::to_string_pretty(&frames_json)?)?;
    println!("Wrote tests/fixtures/snapshot_{{keyframe,delta}}.bin + snapshot_frames.json");
    Ok(())
}

#[cfg(test)]
mod snapshot_fixture_tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name))
            .unwrap_or_else(|e| panic!("read fixture {name}: {e} — run cargo run --bin generate_golden"))
    }

    /// STALE-FIXTURE GUARD: the COMMITTED .bin/.json must equal what the CURRENT encoder
    /// produces. Without this, a protocol struct change with a forgotten
    /// `cargo run --bin generate_golden` leaves the Unity golden test green against
    /// yesterday's bytes while the live server already sends a new layout — the fixture
    /// would prove nothing exactly when it matters most.
    #[test]
    fn committed_snapshot_fixtures_match_current_encoder() {
        let (keyframe, delta) = snapshot_fixture_messages();
        assert_eq!(
            fixture("snapshot_keyframe.bin"),
            keyframe.serialize().unwrap(),
            "snapshot_keyframe.bin is STALE — regenerate (cargo run --bin generate_golden) and resync Unity assets"
        );
        assert_eq!(
            fixture("snapshot_delta.bin"),
            delta.serialize().unwrap(),
            "snapshot_delta.bin is STALE — regenerate and resync Unity assets"
        );
        let expected_json = serde_json::json!({
            "keyframe": serde_json::to_value(&keyframe).unwrap(),
            "delta": serde_json::to_value(&delta).unwrap(),
        });
        let committed: serde_json::Value = serde_json::from_slice(&fixture("snapshot_frames.json")).unwrap();
        assert_eq!(committed, expected_json, "snapshot_frames.json is STALE — regenerate");
    }
}
