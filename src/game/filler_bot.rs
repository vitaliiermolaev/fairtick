//! Filler bots — server-driven FAKE PLAYERS (lead manifesto 2026-06-11).
//!
//! NOT the red PvE `Enemy` mobs: a filler is a normal [`Player`](crate::game::player::Player)
//! entity (kind = FillerBot) that rides the SAME simulation and the SAME wire `PlayerState` —
//! the client receives no `is_bot`, no separate list, no special color or label. This module
//! owns only the BRAIN: each tick window it looks at a bounded view of the world and produces
//! a `Direction`, which the Room applies through `Player::set_direction` exactly like a human
//! MoveCommand. The bot never teleports, never gets special physics, and can be eaten through
//! the ordinary claim path.
//!
//! Design (per the manifesto): a filler must be PLAUSIBLE, not strong —
//!  * limited perception (radius + short memory), no map-wide omniscience;
//!  * utility goals (collect / hunt / flee / booster / wander) weighted by a PERSONALITY;
//!  * BFS pathfinding over the maze grid, with deliberate wrong/late turns;
//!  * reaction delay between deciding and "pressing the key";
//!  * goal commitment — humans don't re-plan every tick, and they abandon chases.
//!
//! Determinism: every random draw goes through the room's [`RoomRng`], so replays stay stable.

use crate::game::map::GameMap;
use crate::game::rng::RoomRng;
use crate::protocol::{Direction, Position};

/// Curated nickname pool (manifesto: no `bot1..N`; a mix of styles — short, with digits,
/// mixed case, underscores, RU translit and EN — because uniformly "real" names read as
/// fake too). ~180 entries so a room never needs a duplicate (the room picks UNUSED names
/// only; see `Room::pick_filler_nickname`). Duplicates across DIFFERENT rooms are fine —
/// humans repeat nicknames too.
pub const FILLER_NICKNAMES: &[&str] = &[
    // -- original manifesto seeds --
    "k0t",
    "vlad",
    "Misha",
    "lera",
    "sunny",
    "ragekid",
    "fox",
    "den4ik",
    "n1ght",
    "toma",
    "sonya",
    "vovan",
    "alina",
    "m1sha",
    "Kotik",
    "Leha",
    "zzz",
    "pirat",
    "kisa",
    "Dasha",
    "moon",
    "egor",
    "NIKITOS",
    "lapka",
    "querty",
    "tim4ik",
    "Oleg",
    "marmelad",
    "x_x",
    "rita",
    // -- RU translit, short/diminutive --
    "sanya",
    "dimon",
    "kolyan",
    "seryoga",
    "tolik",
    "zheka",
    "yarik",
    "stas",
    "borya",
    "gosha",
    "pasha",
    "roma",
    "artem",
    "tyoma",
    "nikita",
    "danya",
    "vanya",
    "petya",
    "fedya",
    "grisha",
    "lyosha",
    "maks",
    "maksim",
    "kirill",
    "andrey",
    "anton",
    "ilya",
    "matvey",
    "savva",
    "mark",
    "nastya",
    "ksyusha",
    "polina",
    "varya",
    "ulyana",
    "marusya",
    "lizka",
    "katya",
    "olya",
    "ira",
    "sveta",
    "tanya",
    "zhenya",
    "vika",
    "yulka",
    "milana",
    "alisa",
    "uliana",
    "nadya",
    "lena",
    // -- RU translit with digits/leet --
    "d1mon",
    "san9a",
    "vlad1k",
    "n1kita",
    "kat9a",
    "m4ks",
    "ser6a",
    "p4sha",
    "r0ma",
    "ki4a",
    "leha22",
    "dim4ik",
    "vit4a",
    "stas0n",
    "tox4a",
    "zhek4",
    "go5ha",
    "boryan",
    "splin7",
    "ch1f",
    // -- EN words / vibes --
    "shadow",
    "blaze",
    "frost",
    "viper",
    "ghost",
    "raven",
    "storm",
    "ember",
    "drift",
    "nova",
    "pixel",
    "mango",
    "biscuit",
    "waffle",
    "noodle",
    "pepper",
    "cosmo",
    "luna",
    "comet",
    "dusk",
    "wisp",
    "fang",
    "claw",
    "husky",
    "otter",
    "panda",
    "gecko",
    "mole",
    "lynx",
    "corgi",
    // -- EN with digits/decorations --
    "sn4ke",
    "gh0sty",
    "fr0sty",
    "bl1tz",
    "z3ro",
    "n0va",
    "pix3l",
    "wolfie7",
    "kitt3n",
    "m4ngo",
    "xX_dark_Xx",
    "_smile_",
    "o_O",
    "uwu_",
    "no_name",
    "Player_2",
    "guest77",
    "hihi",
    "lolkek",
    "sleepy1",
    "yawn",
    "afk_brb",
    "tea_time",
    "br0",
    "dudec",
    "kefir",
    "syrok",
    "baton",
    "pelmen",
    // -- food / pets / silly (very human) --
    "pirozhok",
    "vatrushka",
    "kompot",
    "borsch",
    "sgushenka",
    "plombir",
    "sushka",
    "halva",
    "barsik",
    "murzik",
    "sharik",
    "tuzik",
    "pushok",
    "ryzhik",
    "vasya_cat",
    "bublik",
    // -- short/lazy --
    "qq",
    "ww",
    "ee",
    "aa1",
    "yo",
    "hm",
    "meh",
    "ok_",
    "nyam",
    "zzz2",
    // -- gamer-ish but not pro --
    "noobik",
    "kamikadze",
    "snaiper228",
    "pro100",
    "ne_pro",
    "rak_v_tanke",
    "imba",
    "nerf_pls",
    "ezpz",
    "gg_wp",
    "tryhard",
    "casual_",
    "free_win",
    "last_hope",
    "respawn",
    "lag_lord",
];

/// How a bot leans. Archetypes from the manifesto: Collector / Hunter / Coward / Greedy /
/// Noob — expressed as continuous weights so two Hunters still differ.
#[derive(Debug, Clone)]
pub struct BotPersonality {
    /// 0..1 — appetite for chasing weaker players.
    pub aggression: f32,
    /// 0..1 — pull toward points/boosters even when risky.
    pub greed: f32,
    /// 0..1 — how early/often it flees stronger actors.
    pub fear: f32,
    /// Chance per decision to take a deliberately wrong-but-legal turn.
    pub mistake_rate: f32,
    /// Reaction delay between deciding and applying the turn, in ticks.
    pub reaction_ticks_min: u64,
    pub reaction_ticks_max: u64,
    /// How often the bot re-thinks its goal, in ticks.
    pub decision_interval_min: u64,
    pub decision_interval_max: u64,
    /// Perception radius (px) — the bot does not know the whole map.
    pub perception_radius_px: f32,
    /// Path replan cadence (ticks) — low path quality replans rarely and turns late.
    pub replan_interval_min: u64,
    pub replan_interval_max: u64,
}

impl BotPersonality {
    /// Roll a personality from one of the manifesto archetypes. RoomRng-driven (replay-stable).
    pub fn roll(rng: &mut RoomRng) -> Self {
        // Archetype weights: collector-heavy mix reads as "people playing the game",
        // not a pack hunting the human (manifesto: "бот должен чаще играть в игру").
        let archetype = rng.range_usize(0..100);
        let jitter = |rng: &mut RoomRng, base: f32, spread: f32| {
            (base + rng.range_f32(-0.5..0.5) * spread).clamp(0.0, 1.0)
        };
        let (aggression, greed, fear, mistake_rate, react_min, react_max) = match archetype {
            // Collector — careful point-vacuum, avoids fights.
            0..=29 => (jitter(rng, 0.15, 0.2), jitter(rng, 0.55, 0.3), jitter(rng, 0.6, 0.3), 0.05, 10, 22),
            // Hunter — seeks weaker players, sometimes overcommits.
            30..=49 => (jitter(rng, 0.75, 0.3), jitter(rng, 0.35, 0.2), jitter(rng, 0.25, 0.2), 0.06, 9, 20),
            // Coward — flees early, hugs quiet corridors.
            50..=69 => (jitter(rng, 0.1, 0.15), jitter(rng, 0.4, 0.2), jitter(rng, 0.85, 0.2), 0.07, 11, 26),
            // Greedy — boosters/points over safety; very human.
            70..=87 => (jitter(rng, 0.4, 0.3), jitter(rng, 0.9, 0.15), jitter(rng, 0.35, 0.25), 0.08, 10, 24),
            // Noob — slow reactions, frequent mistakes, wandering.
            _ => (jitter(rng, 0.3, 0.3), jitter(rng, 0.45, 0.3), jitter(rng, 0.5, 0.3), 0.14, 16, 33),
        };
        Self::from_weights(rng, aggression, greed, fear, mistake_rate, react_min, react_max)
    }

    fn from_weights(
        rng: &mut RoomRng,
        aggression: f32,
        greed: f32,
        fear: f32,
        mistake_rate: f32,
        react_min: u64,
        react_max: u64,
    ) -> Self {
        Self {
            aggression,
            greed,
            fear,
            mistake_rate,
            reaction_ticks_min: react_min,
            reaction_ticks_max: react_max,
            decision_interval_min: 15 + rng.range_usize(0..20) as u64, // 0.25–0.6s base
            decision_interval_max: 45 + rng.range_usize(0..30) as u64, // 0.75–1.25s
            perception_radius_px: 180.0 + rng.range_f32(0.0..80.0),
            replan_interval_min: 18 + rng.range_usize(0..18) as u64,
            replan_interval_max: 50 + rng.range_usize(0..40) as u64,
        }
    }
}

/// What the bot currently wants. Goals persist across thinks (commitment) until they
/// expire, complete, or a flee preempts them — humans don't re-plan every frame.
#[derive(Debug, Clone, PartialEq)]
pub enum BotGoal {
    CollectPoint { id: String, pos: Position },
    GetBooster { id: String, pos: Position },
    Hunt { player_id: String },
    Flee { from: Position },
    Wander { to: Position },
    Idle,
}

/// One visible actor (human, other filler, or PvE enemy) inside the perception radius.
pub struct SeenActor {
    pub id: String,
    pub pos: Position,
    pub score: u32,
    /// PvE enemies count as threats/never-prey; fillers must dodge them like humans do
    /// (a bot calmly standing inside a mob is an instant tell).
    pub is_pve_enemy: bool,
}

/// The bounded world the Room shows a bot each think: ONLY what's inside perception.
pub struct BotView<'a> {
    pub tick: u64,
    pub me_pos: Position,
    pub me_score: u32,
    pub actors: &'a [SeenActor],
    /// (id, pos) of visible points / boosters.
    pub points: &'a [(String, Position)],
    pub boosters: &'a [(String, Position)],
    pub map: &'a GameMap,
}

/// Per-filler brain state. Lives in the Room beside the Player entity; removed with it.
#[derive(Debug)]
pub struct BotController {
    pub personality: BotPersonality,
    goal: BotGoal,
    goal_until_tick: u64,
    next_think_tick: u64,
    /// Decided turn waiting out the reaction delay: (direction, apply_at_tick).
    pending: Option<(Direction, u64)>,
    /// Current BFS path as grid waypoints (reversed: pop from the back).
    path: Vec<(usize, usize)>,
    next_replan_tick: u64,
    /// (from, to) of the latest goal-KIND transition, parked until the Room drains it
    /// for telemetry (the brain decides, the room observes — no telemetry in here).
    goal_change: Option<(&'static str, &'static str)>,
}

impl BotController {
    pub fn new(rng: &mut RoomRng, start_tick: u64) -> Self {
        let personality = BotPersonality::roll(rng);
        Self {
            personality,
            goal: BotGoal::Idle,
            goal_until_tick: 0,
            // Stagger first thoughts so a fresh room's bots don't all "wake up" on one tick.
            next_think_tick: start_tick + rng.range_usize(10..90) as u64,
            pending: None,
            path: Vec::new(),
            next_replan_tick: 0,
            goal_change: None,
        }
    }

    /// Short label of the current goal, for population telemetry histograms.
    pub fn goal_kind(&self) -> &'static str {
        Self::kind_of(&self.goal)
    }

    fn kind_of(goal: &BotGoal) -> &'static str {
        match goal {
            BotGoal::CollectPoint { .. } => "collect",
            BotGoal::GetBooster { .. } => "booster",
            BotGoal::Hunt { .. } => "hunt",
            BotGoal::Flee { .. } => "flee",
            BotGoal::Wander { .. } => "wander",
            BotGoal::Idle => "idle",
        }
    }

    /// Drain the latest goal-kind transition (verbose telemetry; the Room emits it).
    pub fn take_goal_change(&mut self) -> Option<(&'static str, &'static str)> {
        self.goal_change.take()
    }

    /// One tick of brain. Returns Some(direction) when the bot "presses a key" this tick
    /// (after its reaction delay); the Room feeds that into Player::set_direction.
    pub fn tick(&mut self, view: &BotView, rng: &mut RoomRng) -> Option<Direction> {
        // 1. A previously decided turn whose reaction delay elapsed fires now.
        if let Some((dir, at)) = self.pending {
            if view.tick >= at {
                self.pending = None;
                return Some(dir);
            }
        }

        // 2. Re-think the goal on the personality's cadence (or when the goal expired).
        if view.tick >= self.next_think_tick || view.tick >= self.goal_until_tick {
            self.think(view, rng);
        }

        // 3. Follow the current goal: replan the path on the personality's cadence and
        //    emit the next turn (with reaction delay + occasional deliberate mistake).
        if view.tick >= self.next_replan_tick {
            self.replan_path(view, rng);
        }
        self.step_along_path(view, rng)
    }

    /// Utility scoring over visible options, personality-weighted, with the manifesto's
    /// "almost-best" choice (humans don't always pick the optimum).
    fn think(&mut self, view: &BotView, rng: &mut RoomRng) {
        let p = &self.personality;
        let mut candidates: Vec<(f32, BotGoal)> = Vec::new();

        // Threats: anything visibly stronger (or a PvE mob) close by.
        let mut nearest_threat: Option<(f32, Position)> = None;
        for a in view.actors {
            let threatening = a.is_pve_enemy || a.score > view.me_score;
            if !threatening {
                continue;
            }
            let d = dist(&view.me_pos, &a.pos);
            if nearest_threat.map(|(bd, _)| d < bd).unwrap_or(true) {
                nearest_threat = Some((d, a.pos));
            }
        }
        if let Some((d, pos)) = nearest_threat {
            // Closer threat + higher fear ⇒ flee scores higher; cowards bail early.
            let urgency = (1.0 - (d / p.perception_radius_px)).clamp(0.0, 1.0);
            candidates.push((40.0 + 80.0 * urgency * (0.4 + p.fear), BotGoal::Flee { from: pos }));
        }

        // Prey: visibly weaker players (humans or other fillers — bots hunting bots is
        // exactly the background activity that makes the room read as alive).
        for a in view.actors {
            if a.is_pve_enemy || a.score >= view.me_score {
                continue;
            }
            let d = dist(&view.me_pos, &a.pos);
            let closeness = (1.0 - (d / p.perception_radius_px)).clamp(0.0, 1.0);
            candidates.push((
                25.0 + 55.0 * closeness * (0.3 + p.aggression),
                BotGoal::Hunt { player_id: a.id.clone() },
            ));
        }

        // Points / boosters.
        for (id, pos) in view.points {
            let d = dist(&view.me_pos, pos);
            let closeness = (1.0 - (d / p.perception_radius_px)).clamp(0.0, 1.0);
            candidates.push((
                20.0 + 45.0 * closeness * (0.4 + p.greed),
                BotGoal::CollectPoint { id: id.clone(), pos: *pos },
            ));
        }
        for (id, pos) in view.boosters {
            let d = dist(&view.me_pos, pos);
            let closeness = (1.0 - (d / p.perception_radius_px)).clamp(0.0, 1.0);
            candidates.push((
                25.0 + 55.0 * closeness * (0.2 + p.greed),
                BotGoal::GetBooster { id: id.clone(), pos: *pos },
            ));
        }

        // Wander is always on the table — "just moving somewhere" is the most human goal.
        candidates.push((22.0, BotGoal::Wander { to: random_walkable_near(view, rng, 6, 12) }));

        // Sort best-first; pick best 80%, second 15%, third (odd-but-legal) 5%.
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let roll = rng.range_usize(0..100);
        let idx = if roll < 80 {
            0
        } else if roll < 95 {
            1
        } else {
            2
        };
        let goal =
            candidates.get(idx.min(candidates.len() - 1)).map(|(_, g)| g.clone()).unwrap_or(BotGoal::Idle);

        let commit = rng.range_usize(
            (self.personality.decision_interval_min * 2) as usize
                ..(self.personality.decision_interval_max * 3) as usize,
        ) as u64;
        let old_kind = Self::kind_of(&self.goal);
        let new_kind = Self::kind_of(&goal);
        if old_kind != new_kind {
            self.goal_change = Some((old_kind, new_kind));
        }
        self.goal = goal;
        self.goal_until_tick = view.tick + commit;
        self.next_think_tick = view.tick
            + rng.range_usize(
                self.personality.decision_interval_min as usize
                    ..self.personality.decision_interval_max as usize,
            ) as u64;
        self.path.clear(); // new goal → new path
        self.next_replan_tick = 0;
    }

    /// BFS to the goal's target cell. Bounded (the maze is ~20×40 cells); deliberately
    /// NOT optimal for low path quality — replan cadence does the "late turn" work.
    fn replan_path(&mut self, view: &BotView, rng: &mut RoomRng) {
        self.next_replan_tick = view.tick
            + rng.range_usize(
                self.personality.replan_interval_min as usize..self.personality.replan_interval_max as usize,
            ) as u64;
        let target = match &self.goal {
            BotGoal::CollectPoint { pos, .. }
            | BotGoal::GetBooster { pos, .. }
            | BotGoal::Wander { to: pos } => *pos,
            BotGoal::Hunt { player_id } => match view.actors.iter().find(|a| &a.id == player_id) {
                Some(a) => a.pos,
                None => {
                    // Prey left perception — a human abandons the chase too.
                    self.goal = BotGoal::Idle;
                    self.goal_until_tick = 0;
                    return;
                }
            },
            BotGoal::Flee { from } => flee_target(view, rng, from),
            BotGoal::Idle => return,
        };
        self.path = bfs_path(view.map, &view.me_pos, &target);
    }

    /// Emit the next turn along the path, through the reaction delay, with the
    /// personality's wrong-turn chance.
    fn step_along_path(&mut self, view: &BotView, rng: &mut RoomRng) -> Option<Direction> {
        if self.pending.is_some() {
            return None; // a turn is already in flight
        }
        let me_cell = view.map.pos_to_grid(&view.me_pos);
        // Drop waypoints we've reached.
        while self.path.last().map(|c| *c == me_cell).unwrap_or(false) {
            self.path.pop();
        }
        let next = *self.path.last()?;
        let mut dir = dir_between(me_cell, next)?;

        // Deliberate mistake: a wrong-but-walkable perpendicular turn, kept brief by the
        // next replan. Looks like a human fumbling a corner, not a broken agent.
        if rng.range_f32(0.0..1.0) < self.personality.mistake_rate {
            if let Some(wrong) = wrong_turn(view.map, me_cell, dir, rng) {
                dir = wrong;
                self.path.clear(); // the mistake invalidates the plan; replan soon
            }
        }

        let react_min = self.personality.reaction_ticks_min as usize;
        let react_max = (self.personality.reaction_ticks_max as usize).max(react_min + 1);
        let reaction = rng.range_usize(react_min..react_max) as u64;
        self.pending = Some((dir, view.tick + reaction));
        None
    }
}

fn dist(a: &Position, b: &Position) -> f32 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Direction from one grid cell to an ADJACENT one (None for non-adjacent/diagonal).
fn dir_between(from: (usize, usize), to: (usize, usize)) -> Option<Direction> {
    let dx = to.0 as i64 - from.0 as i64;
    let dy = to.1 as i64 - from.1 as i64;
    match (dx, dy) {
        (1, 0) => Some(Direction::Right),
        (-1, 0) => Some(Direction::Left),
        (0, 1) => Some(Direction::Down),
        (0, -1) => Some(Direction::Up),
        _ => None,
    }
}

/// A walkable perpendicular to `dir` from `cell` (the "wrong turn"), if any.
fn wrong_turn(map: &GameMap, cell: (usize, usize), dir: Direction, rng: &mut RoomRng) -> Option<Direction> {
    let perp = match dir {
        Direction::Up | Direction::Down => [Direction::Left, Direction::Right],
        Direction::Left | Direction::Right => [Direction::Up, Direction::Down],
    };
    let first = rng.range_usize(0..2);
    for d in [perp[first], perp[1 - first]] {
        if let Some(next) = step_cell(cell, d) {
            if !map.is_wall(next.0, next.1) {
                return Some(d);
            }
        }
    }
    None
}

fn step_cell(cell: (usize, usize), dir: Direction) -> Option<(usize, usize)> {
    match dir {
        Direction::Right => Some((cell.0 + 1, cell.1)),
        Direction::Left => cell.0.checked_sub(1).map(|x| (x, cell.1)),
        Direction::Down => Some((cell.0, cell.1 + 1)),
        Direction::Up => cell.1.checked_sub(1).map(|y| (cell.0, y)),
    }
}

/// Flee destination: the walkable cell ~8 cells away that maximizes distance from the
/// threat. Greedy and imperfect on purpose — panicked humans don't run optimally.
fn flee_target(view: &BotView, rng: &mut RoomRng, from: &Position) -> Position {
    let mut best = random_walkable_near(view, rng, 5, 9);
    let mut best_d = dist(&best, from);
    for _ in 0..4 {
        let cand = random_walkable_near(view, rng, 5, 9);
        let d = dist(&cand, from);
        if d > best_d {
            best = cand;
            best_d = d;
        }
    }
    best
}

/// A random walkable cell `min..max` cells away from the bot (Chebyshev-ish sampling).
fn random_walkable_near(view: &BotView, rng: &mut RoomRng, min_cells: i64, max_cells: i64) -> Position {
    let me = view.map.pos_to_grid(&view.me_pos);
    for _ in 0..12 {
        let dx = rng.range_usize(0..(2 * max_cells + 1) as usize) as i64 - max_cells;
        let dy = rng.range_usize(0..(2 * max_cells + 1) as usize) as i64 - max_cells;
        if dx.abs().max(dy.abs()) < min_cells {
            continue; // too close to count as "going somewhere"
        }
        let gx = (me.0 as i64 + dx).max(0) as usize;
        let gy = (me.1 as i64 + dy).max(0) as usize;
        if !view.map.is_wall(gx, gy) {
            return view.map.grid_to_pos(gx, gy);
        }
    }
    view.me_pos // nowhere sensible — stay put; the next think re-rolls
}

/// Plain BFS over the maze grid; returns waypoints REVERSED (pop from the back).
/// The grid is ~20×40 cells, so this is microseconds — no need for A*.
fn bfs_path(map: &GameMap, from_pos: &Position, to_pos: &Position) -> Vec<(usize, usize)> {
    use std::collections::VecDeque;
    let from = map.pos_to_grid(from_pos);
    let to = map.pos_to_grid(to_pos);
    if from == to || map.is_wall(to.0, to.1) {
        return Vec::new();
    }
    let w = map.grid_width;
    let h = map.grid_height;
    let idx = |c: (usize, usize)| c.1 * w + c.0;
    let mut prev: Vec<Option<(usize, usize)>> = vec![None; w * h];
    let mut seen = vec![false; w * h];
    let mut q = VecDeque::new();
    seen[idx(from)] = true;
    q.push_back(from);
    while let Some(cell) = q.pop_front() {
        if cell == to {
            // Rebuild reversed (target first, next-step last → pop() walks forward).
            let mut path = Vec::new();
            let mut cur = to;
            while cur != from {
                path.push(cur);
                cur = match prev[idx(cur)] {
                    Some(p) => p,
                    None => return Vec::new(),
                };
            }
            return path;
        }
        for d in [Direction::Up, Direction::Down, Direction::Left, Direction::Right] {
            if let Some(n) = step_cell(cell, d) {
                if n.0 < w && n.1 < h && !map.is_wall(n.0, n.1) && !seen[idx(n)] {
                    seen[idx(n)] = true;
                    prev[idx(n)] = Some(cell);
                    q.push_back(n);
                }
            }
        }
    }
    Vec::new() // unreachable target — the next think picks something else
}

#[cfg(test)]
mod nickname_pool_tests {
    use super::FILLER_NICKNAMES;

    /// Room-level uniqueness picks UNUSED pool names, which only works if the pool itself
    /// has no duplicates and is comfortably larger than any realistic room population.
    #[test]
    fn pool_is_duplicate_free_and_large_enough() {
        let mut seen = std::collections::HashSet::new();
        for n in FILLER_NICKNAMES {
            assert!(seen.insert(*n), "duplicate nickname in pool: {n}");
            assert!(!n.to_lowercase().starts_with("bot"), "'{n}' reads as a bot tell");
            assert!(!n.is_empty() && n.len() <= 16, "'{n}' is empty or too long for HUD");
        }
        assert!(
            FILLER_NICKNAMES.len() >= 150,
            "pool must stay >=150 (lead review) - got {}",
            FILLER_NICKNAMES.len()
        );
    }
}
