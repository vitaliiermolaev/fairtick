// Integration test that replays tests/fixtures/sim_parity.json and asserts the
// Rust pure simulation reproduces the recorded samples within 0.001px.
//
// This is the same fixture iOS consumes via SimParityRunner. If this test
// fails after a code change, regenerate the fixture:
//
//   cargo run --bin generate_golden
//
// then re-run scripts/generate_ios_assets.sh to keep iOS in sync.

use fairtick::config_shared::{load_config_from_str, load_maze_from_str};
use fairtick::game::map::GameMap;
use fairtick::game::rng::RoomRng;
use fairtick::game::sim::{simulate_player_tick, PlayerSimState};
use fairtick::protocol::{Direction, Position};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
struct InputChange {
    at_tick: u64,
    direction: Direction,
}

#[derive(Deserialize)]
struct Scenario {
    name: String,
    initial: PlayerSimState,
    inputs: Vec<InputChange>,
    total_ticks: u64,
    sample_every: u64,
    samples: Vec<Position>,
}

#[derive(Deserialize)]
struct GoldenFixture {
    #[allow(dead_code)]
    version: u32,
    #[allow(dead_code)]
    config_hash: String,
    #[allow(dead_code)]
    maze_hash: String,
    scenarios: Vec<Scenario>,
}

const EPSILON: f32 = 0.001;

#[test]
fn sim_parity_against_golden_fixture() {
    let fixture_path = "tests/fixtures/sim_parity.json";
    let raw = std::fs::read_to_string(fixture_path)
        .expect("run `cargo run --bin generate_golden` to create the fixture");
    let fixture: GoldenFixture = serde_json::from_str(&raw).unwrap();

    let config_str = include_str!("../gameplay_config.toml");
    let maze_str = include_str!("../maze.json");
    let config = Arc::new(load_config_from_str(config_str).unwrap());
    let maze = Arc::new(load_maze_from_str(maze_str).unwrap());
    let mut rng = RoomRng::from_seed(0);
    let map = GameMap::new(&config, &maze, &mut rng);

    for scenario in &fixture.scenarios {
        let mut state = scenario.initial;
        let mut sample_idx = 0;
        for tick in 1..=scenario.total_ticks {
            if let Some(change) = scenario.inputs.iter().find(|c| c.at_tick == tick) {
                state.desired_direction = change.direction;
            }
            state = simulate_player_tick(state, &map, &config);
            if tick % scenario.sample_every == 0 {
                let expected = scenario.samples[sample_idx];
                let dx = (state.position.x - expected.x).abs();
                let dy = (state.position.y - expected.y).abs();
                assert!(
                    dx < EPSILON && dy < EPSILON,
                    "scenario `{}` tick {}: got ({},{}), expected ({},{}); diff ({},{}) > {}",
                    scenario.name,
                    tick,
                    state.position.x,
                    state.position.y,
                    expected.x,
                    expected.y,
                    dx,
                    dy,
                    EPSILON
                );
                sample_idx += 1;
            }
        }
        assert_eq!(
            sample_idx,
            scenario.samples.len(),
            "scenario `{}` did not consume all samples",
            scenario.name
        );
    }
}
