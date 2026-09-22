use crate::config_shared::{GameplayConfig, MazeData};
use crate::game::rng::RoomRng;
use crate::protocol::Position;
use std::sync::Arc;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct GameMap {
    pub width: f32,
    pub height: f32,
    pub cell_size: f32,
    pub grid_width: usize,
    pub grid_height: usize,
    pub safe_zones: Vec<SafeZone>,
    pub portals: Vec<Portal>,
    /// true = wall, false = path. Indexed as grid[row][col] i.e. grid[y][x].
    pub grid: Vec<Vec<bool>>,
}

#[derive(Debug, Clone)]
pub struct SafeZone {
    pub center: Position,
    pub radius: f32,
}

#[derive(Debug, Clone)]
pub struct Portal {
    pub position: Position,
}

impl GameMap {
    pub fn new(config: &Arc<GameplayConfig>, maze: &Arc<MazeData>, rng: &mut RoomRng) -> Self {
        let mut map = Self {
            width: config.map.width,
            height: config.map.height,
            cell_size: config.map.cell_size,
            grid_width: config.map.grid_width,
            grid_height: config.map.grid_height,
            safe_zones: Vec::new(),
            portals: Vec::new(),
            grid: Vec::new(),
        };

        map.generate_grid_maze(maze);
        map.create_safe_zones(config);
        map.create_portals(rng);

        map
    }

    fn generate_grid_maze(&mut self, maze: &MazeData) {
        self.grid = vec![vec![false; self.grid_width]; self.grid_height];

        for row in 0..self.grid_height {
            let pattern_row = row % maze.pattern_height;
            for col in 0..self.grid_width {
                self.grid[row][col] = maze.pattern[pattern_row][col] == 1;
            }
        }
    }

    pub fn is_wall(&self, grid_x: usize, grid_y: usize) -> bool {
        if grid_x >= self.grid_width || grid_y >= self.grid_height {
            return true;
        }
        self.grid[grid_y][grid_x]
    }

    pub fn pos_to_grid(&self, pos: &Position) -> (usize, usize) {
        let grid_x = (pos.x / self.cell_size).floor() as usize;
        let grid_y = (pos.y / self.cell_size).floor() as usize;
        (grid_x.min(self.grid_width - 1), grid_y.min(self.grid_height - 1))
    }

    pub fn grid_to_pos(&self, grid_x: usize, grid_y: usize) -> Position {
        Position {
            x: grid_x as f32 * self.cell_size + self.cell_size / 2.0,
            y: grid_y as f32 * self.cell_size + self.cell_size / 2.0,
        }
    }

    #[allow(dead_code)]
    pub fn is_walkable(&self, pos: &Position) -> bool {
        let (grid_x, grid_y) = self.pos_to_grid(pos);
        !self.is_wall(grid_x, grid_y)
    }

    fn create_safe_zones(&mut self, config: &GameplayConfig) {
        self.safe_zones.push(SafeZone {
            center: Position { x: config.safe_zone.center_x, y: config.safe_zone.center_y },
            radius: config.safe_zone.radius_px,
        });
    }

    fn create_portals(&mut self, rng: &mut RoomRng) {
        let portal_pos = self.get_random_spawn_position(rng);
        self.portals.push(Portal { position: portal_pos });
    }

    pub fn spawn_portal(&mut self, rng: &mut RoomRng) {
        self.portals.clear();
        let portal_pos = self.get_random_spawn_position(rng);
        self.portals.push(Portal { position: portal_pos });
    }

    pub fn get_random_spawn_position(&self, rng: &mut RoomRng) -> Position {
        for _ in 0..50 {
            let grid_x = rng.range_usize(1..self.grid_width - 1);
            let grid_y = rng.range_usize(1..self.grid_height - 1);

            if !self.is_wall(grid_x, grid_y) {
                return self.grid_to_pos(grid_x, grid_y);
            }
        }

        self.grid_to_pos(self.grid_width / 2, self.grid_height / 2)
    }

    pub fn is_in_safe_zone(&self, pos: &Position) -> bool {
        self.safe_zones.iter().any(|zone| {
            let dx = pos.x - zone.center.x;
            let dy = pos.y - zone.center.y;
            (dx * dx + dy * dy).sqrt() <= zone.radius
        })
    }

    pub fn check_portal(&self, pos: &Position, pickup_radius: f32) -> bool {
        if let Some(portal) = self.portals.first() {
            let dx = pos.x - portal.position.x;
            let dy = pos.y - portal.position.y;
            (dx * dx + dy * dy).sqrt() <= pickup_radius
        } else {
            false
        }
    }

    pub fn remove_portal(&mut self) {
        self.portals.clear();
    }
}
