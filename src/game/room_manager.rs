use crate::clock::now_unix_ms;
use crate::config_shared::{tick_duration, GameplayConfig, MazeData};
use crate::game::outbox::PlayerOutbound;
use crate::game::player::PlayerResumeState;
use crate::game::room::{EatClaimInput, EnemyDeathClaimInput, PlayerInput, Room, VisualOverlapProbeInput};
use crate::metrics::Percentiles;
use crate::protocol::{EatClaim, EnemyDeathClaim, MoveCommand, PlayerReward, ServerMessage};
use crate::telemetry::Telemetry;
// allow-wall-clock: Stage 7 telemetry only — measures how long room ticks
// actually take so the audit log can show p99 budget compliance.
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, watch, RwLock};
use tokio::time::interval;
use tracing::{error, info};
use uuid::Uuid;

/// Stage 6: capacity for the per-player reliable mpsc. Sized so even a
/// degraded connection can hold a fresh GameJoined + a burst of events
/// without overflow; if it does overflow, room records a drop and Stage 9
/// disconnects the player.
const RELIABLE_CHANNEL_CAPACITY: usize = 64;

/// Persist match-end rewards through the reward outbox, then credit them.
///
/// Durable AFTER enqueue, NOT before: step 1 (`enqueue_rewards`) writes every grant as
/// `pending` in one transaction; once it commits the reward survives a crash/redeploy and
/// the startup replay + periodic drain in `main` apply it idempotently. Step 2 drains
/// immediately so the happy path credits without waiting for the periodic sweep.
///
/// Residual loss window (NOT closed): a crash BETWEEN match-end and this enqueue committing
/// loses that grant — this runs as a spawned task after Room::update() already ended the
/// match, so it's not transactional with the match outcome. It's a single fast INSERT off the
/// tick path, so the window is tiny, but it exists; closing it needs enqueue-before-GameEnded.
/// If the enqueue itself fails we log the full grant (RECONCILE) so it's reconstructable from
/// logs; everything AFTER a successful enqueue is recoverable from the table.
///
/// `reward_id = {room_id}:{user_id}` keys idempotency: replaying the same match outcome
/// is a no-op (INSERT OR IGNORE) and each grant is credited at most once.
async fn persist_rewards(db: SqlitePool, room_id: String, rewards: Vec<PlayerReward>) {
    let outbox: Vec<crate::db::OutboxReward> = rewards
        .into_iter()
        .map(|r| crate::db::OutboxReward {
            reward_id: format!("{}:{}", room_id, r.user_id),
            user_id: r.user_id,
            crystals: r.crystals,
        })
        .collect();

    if let Err(e) = crate::db::enqueue_rewards(&db, &outbox).await {
        // Nothing durable was written — surface every grant so it can be reconciled from
        // logs (matches the old RECONCILE contract for the un-persisted case).
        for r in &outbox {
            error!(
                "RECONCILE: reward enqueue failed reward_id={} user={} crystals={}: {} — grant NOT persisted",
                r.reward_id, r.user_id, r.crystals, e
            );
        }
        return;
    }

    // Durable now — best-effort immediate credit. Anything not applied here (transient DB
    // error) stays `pending` and is picked up by the periodic drain / next startup replay.
    crate::db::drain_pending_rewards(&db).await;
}

struct RoomEntry {
    room: Arc<RwLock<Room>>,
    /// Bounded input channel (capacity 256). Overflow drops inputs; the per-
    /// player slot coalesces survivors to "latest intent".
    input_tx: mpsc::Sender<PlayerInput>,
    /// Bounded player-initiated-eat claim channel (capacity 256).
    eat_claim_tx: mpsc::Sender<EatClaimInput>,
    /// OBSERVE-ONLY survived-overlap probe channel (diagnostics; low rate).
    probe_tx: mpsc::Sender<VisualOverlapProbeInput>,
    /// Claim-based enemy-eats-player death channel.
    enemy_death_claim_tx: mpsc::Sender<EnemyDeathClaimInput>,
}

/// One entry in the resume-token registry, keyed by a connection's resume_token. A token is
/// registered the moment a player joins/resumes — NOT only when they drop — so a half-open
/// reconnect (the new socket's Resume beating the dead socket's `disconnect_hold`) finds the
/// token and can force a handoff instead of being wrongly told "no such session". (review
/// Blocker 2: half-open socket resume.)
enum ResumeSlot {
    /// The player is CURRENTLY connected and live in the room. If the SAME user presents this
    /// token, the existing connection must be half-open (its TCP hasn't died on our side yet) —
    /// resume_room force-detaches the stale entity and hands the room slot to the new socket.
    Active {
        room_id: String,
        /// The live entity's per-connection player_id, so a handoff can detach exactly it.
        player_id: String,
        /// Owner — a Resume is only honoured for the SAME authenticated user (no slot hijack
        /// with a leaked token).
        user_id: String,
    },
    /// The socket dropped unexpectedly; the room slot is HELD (capacity reserved, resume state
    /// stashed) until `deadline_unix_ms`, after which the grace sweep releases it and a Resume
    /// is refused. The nickname is NOT stored: resume_room uses the freshly re-authenticated
    /// nickname, so a rename between drop and reconnect is honoured.
    Held {
        room_id: String,
        user_id: String,
        /// Where/how the player was when they dropped — restored on resume so they reappear in
        /// place (not at a random spawn). See [`PlayerResumeState`].
        resume: PlayerResumeState,
        /// allow-wall-clock: connection-liveness deadline (a network drop is a wall-clock event),
        /// NOT a gameplay-tick decision.
        deadline_unix_ms: u64,
    },
}

impl ResumeSlot {
    /// The owning user for an ownership/hijack check, regardless of variant.
    fn owner(&self) -> &str {
        match self {
            ResumeSlot::Active { user_id, .. } | ResumeSlot::Held { user_id, .. } => user_id,
        }
    }
}

/// How long resume_room polls for a HELD session that the old socket's cleanup may not have
/// recorded yet — the reconnect socket can race ahead of the drop's `disconnect_hold` (the gap
/// while it transitions Active→Held). Short: the cleanup runs as soon as the old socket's read
/// loop ends. A half-open socket (TCP still "live" on our side) instead resolves IMMEDIATELY via
/// the Active branch — no polling needed — so this only covers the brief transition window.
const RESUME_RACE_WAIT_MS: u64 = 300;
const RESUME_POLL_MS: u64 = 50;

/// Result of one peek-and-claim attempt at a resume token, computed under the map lock and
/// acted on after the borrow ends.
enum ResumeClaim {
    /// Not present (yet) — poll again until RESUME_RACE_WAIT_MS.
    Absent,
    /// Present but owned by a different user — refuse WITHOUT removing it, so the legit owner
    /// can still resume before the deadline.
    WrongUser,
    /// A HELD slot past its deadline — removed here; the held room slot (carried room_id) is
    /// released by the caller.
    Expired(String),
    /// A HELD slot, ours and live — removed and claimed. Carries (room_id, resume state,
    /// deadline_unix_ms — for the resume_success held_ms telemetry).
    ClaimedHeld(String, PlayerResumeState, u64),
    /// An ACTIVE token, ours — the existing connection is half-open. Carries (room_id, the live
    /// player_id) so resume_room can force-detach it and hand off to the reconnecting socket.
    HandoffActive(String, String),
}

pub struct RoomManager {
    rooms: Arc<RwLock<HashMap<String, RoomEntry>>>,
    db_pool: SqlitePool,
    config: Arc<GameplayConfig>,
    maze: Arc<MazeData>,
    telemetry: Telemetry,
    /// Wall-clock ms of the last COMPLETED tick (0 until the first tick). The
    /// readiness probe reads this to tell a STALLED loop (hung room update,
    /// deadlocked lock, saturated runtime → heartbeat goes stale) from a merely
    /// IDLE server (an empty server still ticks ~tick_rate×/s, so the beat stays
    /// fresh). allow-wall-clock: liveness diagnostic, not a gameplay decision.
    tick_heartbeat: Arc<AtomicU64>,
    /// Live room count, mirrored to the map length at every mutation point under the
    /// rooms lock. The readiness probe reads this LOCK-FREE so `/readyz` can never
    /// itself block on the rooms lock — exactly the contention/deadlock case where it
    /// must return 503 fast, not hang. (review: /readyz hang-risk)
    live_rooms: Arc<AtomicUsize>,
    /// Resume-token registry, keyed by resume_token. Holds `Active` while a player is connected
    /// (registered on join/resume) and `Held` after an unexpected drop (`disconnect_hold`
    /// transitions Active→Held). Consumed on reconnect (`resume_room` — normal resume of a Held
    /// slot OR force-handoff of a half-open Active one), on an explicit leave (`forget_resume`),
    /// and Held entries are expired by the grace sweep in `start`.
    resume_sessions: Arc<RwLock<HashMap<String, ResumeSlot>>>,
    /// Latest tick-perf percentiles, refreshed by the tick loop on its 5 s log cadence.
    /// std (not tokio) lock: writes/reads are tiny copies and never held across awaits.
    /// Observability only — read by the ops dashboard (`/ops/statusz`).
    tick_perf_snapshot: Arc<std::sync::RwLock<TickPerfSnapshot>>,
}

/// Point-in-time copy of the tick loop's performance percentiles (10 s window)
/// plus the run-total over-budget counter. Serialized into `/ops/statusz`.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct TickPerfSnapshot {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub over_budget_total: u64,
}

/// One room's population/lifecycle line on the ops dashboard. Counts only —
/// no nicknames or user ids by design.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RoomOverview {
    pub id_short: String,
    pub humans: usize,
    pub fillers: usize,
    pub reserved: usize,
    pub total: usize,
    pub tick: u64,
    pub remaining_sec: u64,
    pub is_active: bool,
    pub joinable: bool,
}

impl RoomManager {
    pub fn new(
        db_pool: SqlitePool,
        config: Arc<GameplayConfig>,
        maze: Arc<MazeData>,
        telemetry: Telemetry,
    ) -> Self {
        Self {
            rooms: Arc::new(RwLock::new(HashMap::new())),
            db_pool,
            config,
            maze,
            telemetry,
            tick_heartbeat: Arc::new(AtomicU64::new(0)),
            live_rooms: Arc::new(AtomicUsize::new(0)),
            resume_sessions: Arc::new(RwLock::new(HashMap::new())),
            tick_perf_snapshot: Arc::new(std::sync::RwLock::new(TickPerfSnapshot::default())),
        }
    }

    pub async fn start(&self) {
        let rooms = Arc::clone(&self.rooms);
        let db_pool = self.db_pool.clone();
        let telemetry = self.telemetry.clone();
        let live_rooms = Arc::clone(&self.live_rooms);
        let heartbeat = Arc::clone(&self.tick_heartbeat);
        // The tick task evicts rooms (panic / inactive sweep); it must also purge those rooms'
        // resume-token registrations or Active tokens leak. (review HP2)
        let tick_sessions = Arc::clone(&self.resume_sessions);
        let perf_snapshot = Arc::clone(&self.tick_perf_snapshot);
        // Fix: use seconds-based division (16.666ms at 60Hz), NOT integer millis (16ms).
        let tick_dur = tick_duration(self.config.room.tick_rate);
        let tick_rate = self.config.room.tick_rate;
        // Stage 7.1: 600 samples ≈ 10s at 60Hz — long enough to see steady-
        // state percentiles, short enough that a transient spike still hurts.
        let mut tick_perf = Percentiles::new(600);
        // Budget: spending more than half a tick on the room update is the
        // line where we'd start missing the next deadline.
        let budget_us = (tick_dur.as_micros() as u64) / 2;
        let mut tick_over_budget_count: u64 = 0;

        tokio::spawn(async move {
            let mut ticker = interval(tick_dur);
            // Under overload, prefer slowing down to bursting: never run several
            // Room::update back-to-back to "catch up", which would emit a
            // compressed batch of snapshots and a wave of client corrections.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut tick_count: u64 = 0;

            loop {
                ticker.tick().await;
                tick_count += 1;
                let tick_started = Instant::now(); // allow-wall-clock: tick perf telemetry

                let room_entries: Vec<(String, Arc<RwLock<Room>>)> = {
                    let rooms_lock = rooms.read().await;
                    rooms_lock.iter().map(|(id, entry)| (id.clone(), Arc::clone(&entry.room))).collect()
                };

                // Rooms are independent (each its own Arc<RwLock<Room>>, no cross-room shared
                // state), so update them CONCURRENTLY instead of one-after-another on this single
                // task (review #4 — the sequential loop made the per-tick budget the SUM over all
                // rooms, so one heavy/contended room delayed every other). Each room still has a
                // single writer via its own lock; the join barrier keeps the invariant "one update
                // per room per tick, all complete before the next tick".
                //
                // A panic in one room's update can't kill the tick DRIVER (it's caught by the
                // JoinError below), but it is NOT harmless: the panic unwound mid-update, so that
                // room's state is partially mutated and no longer a trustworthy authoritative
                // simulation. We must NOT keep ticking corrupt state — on `is_panic()` the room is
                // EVICTED from the map (its players' sockets drop → they reconnect into a fresh
                // room) and a telemetry event is emitted. (review #9 — logging alone is not enough
                // for an authoritative sim.)
                let mut handles = Vec::with_capacity(room_entries.len());
                for (room_id, room_arc) in room_entries {
                    let db = db_pool.clone();
                    let log_id = room_id.clone();
                    handles.push((
                        room_id,
                        tokio::spawn(async move {
                            let mut room = room_arc.write().await;
                            if let Some(rewards) = room.update() {
                                tokio::spawn(persist_rewards(db, log_id.clone(), rewards));
                                info!("Room {} ended", log_id);
                            }
                        }),
                    ));
                }
                for (room_id, h) in handles {
                    if let Err(e) = h.await {
                        if e.is_panic() {
                            error!(
                                "💥 room {} update task PANICKED — evicting room (state may be corrupt): {e}",
                                room_id
                            );
                            let mut w = rooms.write().await;
                            w.remove(&room_id);
                            Self::publish_room_count(&live_rooms, w.len()); // keep lock-free count in sync
                            drop(w);
                            // The room is gone — purge its resume tokens so a connected player's
                            // Active entry doesn't linger forever (it has no deadline). (HP2)
                            Self::purge_resume_for_room_in_map(&mut *tick_sessions.write().await, &room_id);
                            telemetry.server_warn(
                                "room_update_panic_evicted",
                                serde_json::json!({ "room_id": room_id }),
                            );
                        } else {
                            // Cancellation (e.g. runtime shutdown) — not a corruption signal.
                            error!("room {} update task cancelled: {e}", room_id);
                        }
                    }
                }

                // Stage 7.1: record how long this tick's room-update batch
                // took (wall-clock at the telemetry boundary, NOT gameplay).
                let elapsed = tick_started.elapsed(); // allow-wall-clock: tick perf telemetry
                tick_perf.push_duration(elapsed);
                if elapsed.as_micros() as u64 > budget_us {
                    tick_over_budget_count = tick_over_budget_count.saturating_add(1);
                }

                if tick_count.is_multiple_of(60) {
                    let mut rooms_lock = rooms.write().await;
                    let mut removed_ids: Vec<String> = Vec::new();
                    rooms_lock.retain(|id, entry| {
                        if let Ok(room) = entry.room.try_read() {
                            if !room.is_active && room.player_count() == 0 {
                                info!("Removing inactive room {}", id);
                                removed_ids.push(id.clone());
                                return false;
                            }
                        }
                        true
                    });
                    Self::publish_room_count(&live_rooms, rooms_lock.len()); // keep lock-free count in sync
                    drop(rooms_lock);
                    // Purge any resume tokens for the removed rooms (cheap insurance: an inactive
                    // room shouldn't carry Held/Active tokens, but a removed room must never leave
                    // a dangling registration). (HP2)
                    if !removed_ids.is_empty() {
                        let mut s = tick_sessions.write().await;
                        for id in &removed_ids {
                            Self::purge_resume_for_room_in_map(&mut s, id);
                        }
                    }
                }

                // Stage 7.1: 5-second log of the tick-perf percentiles so the
                // audit pipeline has a steady stream of server-side numbers.
                if tick_count.is_multiple_of(tick_rate * 5) && !tick_perf.is_empty() {
                    info!(
                        "📊 [SERVER] tick_p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms over_budget={}",
                        tick_perf.p50_ms(),
                        tick_perf.p95_ms(),
                        tick_perf.p99_ms(),
                        tick_perf.max_ms(),
                        tick_over_budget_count
                    );
                    // Same numbers, machine-readable: the ops dashboard reads this
                    // snapshot instead of scraping the log line above.
                    if let Ok(mut snap) = perf_snapshot.write() {
                        *snap = TickPerfSnapshot {
                            p50_ms: tick_perf.p50_ms(),
                            p95_ms: tick_perf.p95_ms(),
                            p99_ms: tick_perf.p99_ms(),
                            max_ms: tick_perf.max_ms(),
                            over_budget_total: tick_over_budget_count,
                        };
                    }
                }

                // Liveness heartbeat: this tick iteration finished. The readiness
                // probe compares this against now() — it stays fresh even on a
                // zero-room server (idle != stalled), but goes stale if the loop
                // hangs (a wedged room update never reaches here). See
                // `last_tick_unix_ms`. allow-wall-clock: readiness diagnostic.
                heartbeat.store(now_unix_ms(), Ordering::Relaxed);
            }
        });

        // Resume grace sweep: release slots whose reconnect window has closed. A dropped
        // player's slot is held (capacity reserved, score stashed) until this fires; after
        // it, a Resume is refused and the player rejoins fresh. 1 Hz — grace is in seconds,
        // so sub-second precision isn't needed. Runs independently of the tick loop.
        {
            let rooms = Arc::clone(&self.rooms);
            let sessions = Arc::clone(&self.resume_sessions);
            let telemetry = self.telemetry.clone();
            tokio::spawn(async move {
                let mut sweep = interval(std::time::Duration::from_secs(1));
                sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    sweep.tick().await;
                    let now = now_unix_ms();
                    // Drain expired sessions under the map lock so a concurrent resume_room
                    // can't also handle the same token (whoever removes it first wins).
                    let expired: Vec<(String, String)> = {
                        let mut map = sessions.write().await;
                        // Only HELD slots expire — an Active token has no deadline (it lives as
                        // long as the connection) and must never be swept out from under a live
                        // player.
                        let exp: Vec<(String, String, String)> = map
                            .iter()
                            .filter_map(|(t, s)| match s {
                                ResumeSlot::Held { deadline_unix_ms, room_id, user_id, .. }
                                    if now > *deadline_unix_ms =>
                                {
                                    Some((t.clone(), room_id.clone(), user_id.clone()))
                                }
                                _ => None,
                            })
                            .collect();
                        for (t, _, _) in &exp {
                            map.remove(t);
                        }
                        exp.into_iter().map(|(_, room_id, user_id)| (room_id, user_id)).collect()
                    };
                    for (room_id, user_id) in expired {
                        // Clone the Arc out of the map, then lock the room — never hold the
                        // rooms read guard across the room write lock.
                        let room_arc = rooms.read().await.get(&room_id).map(|e| Arc::clone(&e.room));
                        if let Some(room_arc) = room_arc {
                            room_arc.write().await.release_reservation();
                        }
                        info!("🔌 resume grace expired — released a held slot in room {room_id}");
                        telemetry.server_info(
                            "resume_hold_expired",
                            serde_json::json!({ "room_id": room_id, "user_id": user_id }),
                        );
                        // Registry shape after the expiry (lead manifesto #3) — the sweep task
                        // has no &self, so count under a fresh read lock.
                        let (total, held, held_for_user) =
                            Self::hold_counts(&*sessions.read().await, &user_id);
                        telemetry.server_info(
                            "resume_hold_state",
                            serde_json::json!({
                                "action": "expired",
                                "room_id": room_id,
                                "user_id": user_id,
                                "sessions_total": total,
                                "held_total": held,
                                "held_for_user": held_for_user,
                            }),
                        );
                    }
                }
            });
        }
    }

    pub async fn find_or_create_room(&self) -> (String, Arc<RwLock<Room>>) {
        let rooms = self.rooms.read().await;
        for (id, entry) in rooms.iter() {
            let room = entry.room.read().await;
            // accepting_new_joins also rejects rooms about to hit game_end (a fresh
            // player must not land in a match that ends seconds later); resumes don't
            // come through here and keep working until the actual end.
            if room.has_space() && room.accepting_new_joins() {
                info!("✅ Found available room: {}", &id[..8]);
                return (id.clone(), Arc::clone(&entry.room));
            }
        }
        drop(rooms);

        let mut rooms = self.rooms.write().await;
        let (mut new_room, input_tx, eat_claim_tx) = Room::new(self.config.clone(), self.maze.clone(), None);
        let probe_tx = new_room.probe_sender();
        let enemy_death_claim_tx = new_room.enemy_death_claim_sender();
        new_room.attach_telemetry(self.telemetry.clone());
        let room_id = new_room.id.clone();
        self.telemetry.server_info(
            "room_created",
            serde_json::json!({
                "room_id": room_id,
                "max_players": self.config.room.max_players,
                "game_end_tick": new_room.game_end_tick,
                "bot_count": new_room.enemies.len(),
            }),
        );
        let room_arc = Arc::new(RwLock::new(new_room));

        rooms.insert(
            room_id.clone(),
            RoomEntry { room: Arc::clone(&room_arc), input_tx, eat_claim_tx, probe_tx, enemy_death_claim_tx },
        );
        Self::publish_room_count(&self.live_rooms, rooms.len()); // keep lock-free count in sync
        info!("🆕 Created new room: {}", &room_id[..8]);

        (room_id, room_arc)
    }

    pub async fn get_room(&self, room_id: &str) -> Option<Arc<RwLock<Room>>> {
        let rooms = self.rooms.read().await;
        rooms.get(room_id).map(|entry| Arc::clone(&entry.room))
    }

    /// Wall-clock ms of the last completed tick (0 before the first tick). The
    /// readiness probe compares this against now() to detect a stalled tick loop.
    /// Lock-free read of the heartbeat the tick loop stores each iteration.
    pub fn last_tick_unix_ms(&self) -> u64 {
        self.tick_heartbeat.load(Ordering::Relaxed)
    }

    /// Configured simulation rate (Hz). The readiness probe derives its stall
    /// threshold from this so the check tracks config, not a magic constant.
    pub fn tick_rate(&self) -> u64 {
        self.config.room.tick_rate
    }

    /// Number of live rooms — observe-only, for the readiness body. LOCK-FREE: reads
    /// the atomic mirrored to the map length at each mutation, so `/readyz` never blocks
    /// on the rooms lock (the deadlock/contention case where it must fail fast).
    pub fn room_count(&self) -> usize {
        self.live_rooms.load(Ordering::Relaxed)
    }

    /// SINGLE chokepoint for publishing the lock-free room count: call this with the map
    /// length right after EVERY rooms mutation (create / cleanup / panic-evict) so the
    /// `/readyz` atomic can never silently drift. Associated fn (not `&self`) so the
    /// tick-loop task — which holds a cloned `live_rooms` Arc, not `self` — uses it too.
    /// (review: don't let a future mutation forget to update live_rooms.)
    fn publish_room_count(live_rooms: &AtomicUsize, len: usize) {
        live_rooms.store(len, Ordering::Relaxed);
    }

    /// Latest tick-perf percentiles (refreshed by the tick loop every 5 s).
    pub fn tick_perf(&self) -> TickPerfSnapshot {
        self.tick_perf_snapshot.read().map(|s| *s).unwrap_or_default()
    }

    /// Resume-token registry size (Active + Held) — ops visibility for "how many
    /// reconnect slots are outstanding right now".
    pub async fn resume_sessions_count(&self) -> usize {
        self.resume_sessions.read().await.len()
    }

    /// Per-room population snapshot for the ops dashboard. Read locks only; counts
    /// and ids, deliberately NO nicknames/user ids (an ops page must not become a
    /// roster leak). Observability — makes no decision.
    pub async fn rooms_overview(&self) -> Vec<RoomOverview> {
        let rooms = self.rooms.read().await;
        let mut out = Vec::with_capacity(rooms.len());
        for (id, entry) in rooms.iter() {
            let room = entry.room.read().await;
            let tick_rate = room.tick_rate().max(1);
            out.push(RoomOverview {
                id_short: id.chars().take(8).collect(),
                humans: room.human_count(),
                fillers: room.filler_count(),
                reserved: room.reserved_slots(),
                total: room.player_count(),
                tick: room.tick,
                remaining_sec: room.game_end_tick.saturating_sub(room.tick) / tick_rate,
                is_active: room.is_active,
                joinable: room.has_space() && room.accepting_new_joins(),
            });
        }
        // Stable order for the dashboard (HashMap iteration jitters between polls).
        out.sort_by(|a, b| a.id_short.cmp(&b.id_short));
        out
    }

    pub async fn join_room(&self, user_id: String, nickname: String) -> Result<JoinedRoom, String> {
        info!("🎮 join_room called for user: {} ({})", nickname, user_id);

        // find_or_create_room() picks a room under a read lock, but join validity is
        // only authoritative under the room's write lock. Between the two, the room can
        // fill (another joiner wins the race) OR end (its match finished). So we validate
        // via try_add_player() under the same write lock that inserts, and on EITHER
        // rejection (Full / Inactive) loop back to pick another room.
        //
        // The bound must survive a CONNECT STORM: when N clients join at once they all
        // read the SAME candidate room, max_players of them win, and the rest re-race —
        // worst case ~N/max_players rounds for the unluckiest joiner. The 2026-06-11
        // filler burst soak (100 simultaneous bots, rooms of 10) produced exactly one
        // 8-round loser under the old bound of 8. Each retry is just a lock + map scan,
        // and every round makes global progress (a room fills or a fresh one is minted),
        // so a generous bound costs nothing: 64 covers a 500-conn storm with slack.
        const MAX_JOIN_ATTEMPTS: usize = 64;
        for attempt in 0..MAX_JOIN_ATTEMPTS {
            let (room_id, room_arc) = self.find_or_create_room().await;

            // Stage 6: two outbound channels per player.
            //  - reliable: bounded mpsc; the websocket task drains it FIFO.
            //  - snapshot: latest-only watch; overwrites on overflow.
            // Built per attempt: on a lost capacity race we drop these unused
            // and rebuild for the next room (cheap; only happens on contention).
            let (reliable_tx, reliable_rx) = mpsc::channel::<ServerMessage>(RELIABLE_CHANNEL_CAPACITY);
            let (snapshot_tx, snapshot_rx) = watch::channel::<Option<ServerMessage>>(None);

            let outbound = PlayerOutbound { reliable: reliable_tx, snapshot: snapshot_tx };

            let mut room = room_arc.write().await;
            match room.try_add_player(user_id.clone(), nickname.clone(), outbound) {
                Ok((player_id, initial_full)) => {
                    let server_tick = room.tick;
                    drop(room);

                    info!(
                        "✅ Player {} ({}) joined room {} as player_id {}",
                        nickname,
                        user_id,
                        &room_id[..8],
                        &player_id[..8]
                    );

                    // Register the live connection so a half-open reconnect can find it and hand
                    // off, instead of being told "no session" and ghost-joining. (Blocker 2)
                    let resume_token = Uuid::new_v4().to_string();
                    self.register_active(&resume_token, &room_id, &player_id, &user_id).await;

                    return Ok(JoinedRoom {
                        room_id,
                        player_id,
                        server_tick,
                        server_time_ms: now_unix_ms(),
                        resume_token,
                        via_resume: false,
                        initial_full,
                        reliable_rx,
                        snapshot_rx,
                    });
                }
                Err(reason) => {
                    drop(room);
                    info!(
                        "🔁 Room {} rejected join ({:?}) under the lock; retrying join for {} (attempt {}/{})",
                        &room_id[..8], reason, nickname, attempt + 1, MAX_JOIN_ATTEMPTS
                    );
                }
            }
        }

        error!(
            "❌ join_room exhausted {} attempts for {} ({}) — every candidate room filled under the lock",
            MAX_JOIN_ATTEMPTS, nickname, user_id
        );
        Err("could not join a room: capacity contention".to_string())
    }

    pub async fn leave_room(&self, room_id: &str, player_id: &str) {
        if let Some(room_arc) = self.get_room(room_id).await {
            let mut room = room_arc.write().await;
            room.remove_player(player_id);
        } else {
            error!("❌ Room {} not found when removing player", room_id);
        }
    }

    /// Unexpected socket drop (NOT an explicit LeaveGame): hold the player's slot for
    /// `reconnect_grace_sec` so a Resume within the window returns them to THIS match with
    /// their score. The room reserves the capacity and removes the dead-connection entity;
    /// we stash the resume state + identity keyed by the resume_token. No session is recorded
    /// if the room or player is already gone — the caller then has nothing to resume into.
    ///
    /// DELIBERATE gameplay rule (lead review): the dropped player's ENTITY is removed, so if
    /// the match ends before they resume, `end_game` (which rewards `self.players`) does NOT
    /// reward them. "You must be connected — or resumed — at match end to earn the grant." A
    /// fuller design would keep a held-participant ledger that shares in rewards; deferred.
    pub async fn disconnect_hold(&self, room_id: &str, player_id: &str, user_id: &str, resume_token: &str) {
        // Claim THIS connection's Active registration before touching the room. If the token is
        // gone (or no longer Active for this player_id), a half-open reconnect has ALREADY
        // force-detached this entity and handed its slot to a new socket — so there is nothing
        // left to hold, and proceeding would double-hold or resurrect a dead slot. The map is the
        // single arbiter: whoever removes the token first owns the right to act on this player.
        // (Blocker 2)
        {
            let mut map = self.resume_sessions.write().await;
            match map.get(resume_token) {
                // Full invariant: the token must be Active for THIS exact (room, player, user).
                // player_id is a UUID so the pid check alone is practically sufficient, but using
                // all three turns a desynced ClientState/token into a harmless no-op instead of a
                // surprising detach of someone else's slot. (review medium #3)
                Some(ResumeSlot::Active { room_id: rid, player_id: pid, user_id: uid })
                    if pid == player_id && rid == room_id && uid == user_id =>
                {
                    map.remove(resume_token);
                }
                _ => return,
            }
        }

        let Some(room_arc) = self.get_room(room_id).await else {
            return;
        };
        let held = {
            let mut room = room_arc.write().await;
            // A match that already ENDED must not hold a slot (lead review): resuming into an
            // inactive room is rejected anyway, and a held slot in a dead room is a phantom
            // reservation that makes the room look full. Just remove the entity (the Active
            // registration is already removed above, so nothing leaks).
            if !room.is_active {
                room.remove_player(player_id);
                return;
            }
            room.hold_player_for_resume(player_id).map(|state| (state, room.reserved_slots()))
        };
        let Some((resume, reserved_after)) = held else {
            return; // player already removed — nothing to hold
        };
        let score = resume.score;
        let deadline_unix_ms = now_unix_ms() + (self.config.network.reconnect_grace_sec as u64) * 1000;
        // Transition this token Active → Held.
        self.resume_sessions.write().await.insert(
            resume_token.to_string(),
            ResumeSlot::Held {
                room_id: room_id.to_string(),
                user_id: user_id.to_string(),
                resume,
                deadline_unix_ms,
            },
        );
        info!(
            "🔌 resume hold: room={} user={} score={} grace={}s reserved_slots={}",
            room_id, user_id, score, self.config.network.reconnect_grace_sec, reserved_after
        );
        // Observability for the "vanish while disconnected" beta rule: pairs with resume_success /
        // resume_hold_expired so the logs show whether players abuse reconnect as an invulnerability
        // window (held_ms on resume reveals how long they were untouchable). (review medium #4)
        self.telemetry.server_info(
            "resume_hold_started",
            serde_json::json!({
                "room_id": room_id,
                "user_id": user_id,
                "score": score,
                "grace_sec": self.config.network.reconnect_grace_sec,
                "reserved_slots_after": reserved_after,
                "deadline_unix_ms": deadline_unix_ms,
            }),
        );
        self.emit_resume_hold_state("created", room_id, user_id).await;
    }

    /// Register a live connection's resume_token so a half-open reconnect can find it. Called on a
    /// successful join/resume. Overwrites any prior entry for the token (tokens are fresh UUIDs,
    /// so this only ever inserts). (Blocker 2)
    async fn register_active(&self, resume_token: &str, room_id: &str, player_id: &str, user_id: &str) {
        self.resume_sessions.write().await.insert(
            resume_token.to_string(),
            ResumeSlot::Active {
                room_id: room_id.to_string(),
                player_id: player_id.to_string(),
                user_id: user_id.to_string(),
            },
        );
    }

    /// Drop a connection's resume-token registration on an EXPLICIT leave (LeaveGame /
    /// ReturnToMenu / PlayAgain). Those free the slot immediately and must NOT hold for resume, so
    /// the Active entry would otherwise linger until the socket closed. No-op if already gone.
    ///
    /// The call sites today always fire from a LIVE (Active) connection, but the name is broad —
    /// so this is variant-aware: if it ever removes a HELD slot (which owns a room reservation),
    /// it releases that reservation rather than leaking capacity. (review HP1)
    pub async fn forget_resume(&self, resume_token: &str) {
        let removed = self.resume_sessions.write().await.remove(resume_token);
        match &removed {
            Some(ResumeSlot::Held { room_id, user_id, .. }) => {
                // CANARY: the explicit-leave call sites fire from a LIVE (Active) connection, so a
                // Held slot here means some lifecycle path reached forget_resume in an unexpected
                // state. The release below keeps it safe (no capacity leak), but surface it.
                tracing::warn!(
                    "forget_resume removed a HELD slot (unexpected) room={room_id} — released its reservation"
                );
                let (room_id, user_id) = (room_id.clone(), user_id.clone());
                if let Some(room) = self.get_room(&room_id).await {
                    room.write().await.release_reservation();
                }
                self.emit_resume_hold_state("cleared", &room_id, &user_id).await;
            }
            Some(ResumeSlot::Active { room_id, user_id, .. }) => {
                let (room_id, user_id) = (room_id.clone(), user_id.clone());
                self.emit_resume_hold_state("cleared", &room_id, &user_id).await;
            }
            None => {}
        }
    }

    /// Drop EVERY resume-token registration (Active OR Held) belonging to `room_id`. Used when a
    /// room leaves the map OUTSIDE the normal per-player leave paths — panic eviction and the
    /// inactive-room sweep — where Active tokens would otherwise leak forever (only Held entries
    /// have a deadline the grace sweep expires). Associated fn (not `&self`) so the tick-loop task,
    /// which holds a cloned `resume_sessions` Arc rather than `self`, can call it. Held entries for
    /// an evicted room carry a reservation, but the room object is being dropped WITH its
    /// `reserved_slots`, so there's nothing to release — just forget the tokens. (review HP2)
    fn purge_resume_for_room_in_map(map: &mut HashMap<String, ResumeSlot>, room_id: &str) {
        map.retain(|_, slot| match slot {
            ResumeSlot::Active { room_id: r, .. } | ResumeSlot::Held { room_id: r, .. } => r != room_id,
        });
    }

    /// Registry shape: (total sessions, held slots, held slots for `user_id`). The live
    /// counters resume_hold_state reports after every holds mutation (lead manifesto #3 —
    /// reserved-slot stacking must be visible the moment it forms).
    fn hold_counts(map: &HashMap<String, ResumeSlot>, user_id: &str) -> (usize, usize, usize) {
        let total = map.len();
        let mut held = 0;
        let mut held_for_user = 0;
        for slot in map.values() {
            if let ResumeSlot::Held { user_id: uid, .. } = slot {
                held += 1;
                if uid == user_id {
                    held_for_user += 1;
                }
            }
        }
        (total, held, held_for_user)
    }

    /// One resume_hold_state event after a holds mutation (created / consumed / cleared —
    /// the sweep task emits its own "expired" variant without &self).
    async fn emit_resume_hold_state(&self, action: &str, room_id: &str, user_id: &str) {
        let (total, held, held_for_user) = Self::hold_counts(&*self.resume_sessions.read().await, user_id);
        self.telemetry.server_info(
            "resume_hold_state",
            serde_json::json!({
                "action": action,
                "room_id": room_id,
                "user_id": user_id,
                "sessions_total": total,
                "held_total": held,
                "held_for_user": held_for_user,
            }),
        );
    }

    /// Observe-only peek at a user's resume session, for the hold-lookup forensics
    /// (engineer wave 7 #10) — same Held-preferred scan as find_resume_token_for_user,
    /// but returns the DESCRIPTION and consumes nothing.
    pub async fn peek_resume_session_for_user(&self, user_id: &str) -> Option<ResumeSessionPeek> {
        let map = self.resume_sessions.read().await;
        let mut best_held: Option<ResumeSessionPeek> = None;
        let mut any_active: Option<ResumeSessionPeek> = None;
        for slot in map.values() {
            match slot {
                ResumeSlot::Held { user_id: uid, room_id, deadline_unix_ms, resume } if uid == user_id => {
                    if best_held
                        .as_ref()
                        .map(|b| *deadline_unix_ms > b.deadline_unix_ms.unwrap_or(0))
                        .unwrap_or(true)
                    {
                        best_held = Some(ResumeSessionPeek {
                            room_id: room_id.clone(),
                            held: true,
                            deadline_unix_ms: Some(*deadline_unix_ms),
                            score: Some(resume.score),
                        });
                    }
                }
                ResumeSlot::Active { user_id: uid, room_id, .. } if uid == user_id => {
                    any_active = Some(ResumeSessionPeek {
                        room_id: room_id.clone(),
                        held: false,
                        deadline_unix_ms: None,
                        score: None,
                    });
                }
                _ => {}
            }
        }
        best_held.or(any_active)
    }

    /// Any resume session (Held preferred, else a half-open Active) belonging to `user_id`.
    /// The lead-manifesto fix for the 2026-06-10 ghost join: a client that lost its resume
    /// token (the reconnect attempt died between Hello and Resume) falls back to JoinGame —
    /// and the server would happily admit a SECOND player for a user whose held slot is
    /// still reserved (score lost, reservation leaked until grace expiry, reserved_slots
    /// stacking). The join path uses this to convert that JoinGame into a resume instead.
    /// Held wins over Active (a held slot is the canonical "come back" state); among
    /// multiple Helds the freshest deadline wins. Expiry is NOT checked here — resume_room
    /// is the single arbiter and the caller falls back to a fresh join on Err.
    pub async fn find_resume_token_for_user(&self, user_id: &str) -> Option<String> {
        let map = self.resume_sessions.read().await;
        let mut best_held: Option<(&String, u64)> = None;
        let mut any_active: Option<&String> = None;
        for (token, slot) in map.iter() {
            match slot {
                ResumeSlot::Held { user_id: uid, deadline_unix_ms, .. } if uid == user_id => {
                    if best_held.map(|(_, d)| *deadline_unix_ms > d).unwrap_or(true) {
                        best_held = Some((token, *deadline_unix_ms));
                    }
                }
                ResumeSlot::Active { user_id: uid, .. } if uid == user_id => {
                    any_active = Some(token);
                }
                _ => {}
            }
        }
        best_held.map(|(t, _)| t.clone()).or_else(|| any_active.cloned())
    }

    /// Reconnect with a resume_token: put the player back into the SAME room/match with their
    /// score restored, or return Err(reason) so the caller replies ResumeRejected and the
    /// client falls back to a fresh JoinGame. The session and its held slot are consumed
    /// exactly once on EVERY path (resume releases via `resume_player`; every reject path
    /// releases the reservation explicitly). The token must belong to the reconnecting
    /// authenticated user — a token from a different user is refused (no slot hijack).
    pub async fn resume_room(
        &self,
        resume_token: &str,
        user_id: String,
        nickname: String,
    ) -> Result<JoinedRoom, String> {
        // Claim the token under the map lock (the map is the single arbiter — whoever removes the
        // token first owns the right to act on it, so a racing sweep / disconnect_hold can't
        // double-handle it, and a wrong-user attempt can't burn the legit owner's session).
        //   - HELD slot, ours, live   → normal resume.
        //   - ACTIVE token, ours      → the existing connection is half-open; force-detach it and
        //                                hand the slot to this socket (the Blocker-2 fix — a
        //                                half-open old socket no longer ghosts the room).
        //   - absent                  → poll briefly: a fresh drop may still be transitioning
        //                                Active→Held in disconnect_hold.
        let mut waited_ms = 0u64;
        let resolved = loop {
            let claim = {
                let mut map = self.resume_sessions.write().await;
                let now = now_unix_ms();
                // Decide under an immutable borrow (the guard reads the slot), THEN remove — so
                // the scrutinee borrow ends before the mutable remove. (Owned discriminant only.)
                enum Decision {
                    Absent,
                    WrongUser,
                    Handoff,
                    Expired,
                    ClaimHeld,
                }
                let decision = match map.get(resume_token) {
                    None => Decision::Absent,
                    Some(slot) if slot.owner() != user_id.as_str() => Decision::WrongUser,
                    Some(ResumeSlot::Active { .. }) => Decision::Handoff,
                    Some(ResumeSlot::Held { deadline_unix_ms, .. }) if now > *deadline_unix_ms => {
                        Decision::Expired
                    }
                    Some(ResumeSlot::Held { .. }) => Decision::ClaimHeld,
                };
                match decision {
                    Decision::Absent => ResumeClaim::Absent,
                    Decision::WrongUser => ResumeClaim::WrongUser,
                    Decision::Handoff => match map.remove(resume_token) {
                        Some(ResumeSlot::Active { room_id, player_id, .. }) => {
                            ResumeClaim::HandoffActive(room_id, player_id)
                        }
                        _ => ResumeClaim::Absent,
                    },
                    Decision::Expired => match map.remove(resume_token) {
                        Some(ResumeSlot::Held { room_id, .. }) => ResumeClaim::Expired(room_id),
                        _ => ResumeClaim::Absent,
                    },
                    Decision::ClaimHeld => match map.remove(resume_token) {
                        Some(ResumeSlot::Held { room_id, resume, deadline_unix_ms, .. }) => {
                            ResumeClaim::ClaimedHeld(room_id, resume, deadline_unix_ms)
                        }
                        _ => ResumeClaim::Absent,
                    },
                }
            };
            match claim {
                ResumeClaim::ClaimedHeld(room_id, resume, deadline) => {
                    break ResumeClaim::ClaimedHeld(room_id, resume, deadline)
                }
                ResumeClaim::HandoffActive(room_id, pid) => break ResumeClaim::HandoffActive(room_id, pid),
                ResumeClaim::WrongUser => {
                    // Refuse but DON'T remove — the real owner may still resume before deadline.
                    return Err("resume token does not belong to this user".to_string());
                }
                ResumeClaim::Expired(room_id) => {
                    if let Some(r) = self.get_room(&room_id).await {
                        r.write().await.release_reservation();
                    }
                    return Err("resume grace window expired".to_string());
                }
                ResumeClaim::Absent => {
                    if waited_ms >= RESUME_RACE_WAIT_MS {
                        return Err("no active resume session (expired or unknown token)".to_string());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(RESUME_POLL_MS)).await;
                    waited_ms += RESUME_POLL_MS;
                }
            }
        };

        // Destructure once: a Held slot carries its resume state (+ deadline for held_ms); a
        // half-open Active handoff carries the live player_id to detach (its resume state is read
        // under the room lock).
        let (room_id, half_open_pid, held_resume, held_deadline) = match resolved {
            ResumeClaim::ClaimedHeld(room_id, resume, deadline) => {
                (room_id, None, Some(resume), Some(deadline))
            }
            ResumeClaim::HandoffActive(room_id, pid) => (room_id, Some(pid), None, None),
            // The loop only ever breaks with one of the two above.
            _ => unreachable!("resume loop broke with a non-resolved claim"),
        };

        let Some(room_arc) = self.get_room(&room_id).await else {
            // Room evicted/gone since the drop — nothing to resume into. For a HELD slot the
            // reservation went with the room; for a half-open Active token the old player went
            // with it too. Either way there's nothing left to clean up here.
            return Err("the match is no longer available".to_string());
        };

        // Per-player outbound channels — same shape as join_room.
        let (reliable_tx, reliable_rx) = mpsc::channel::<ServerMessage>(RELIABLE_CHANNEL_CAPACITY);
        let (snapshot_tx, snapshot_rx) = watch::channel::<Option<ServerMessage>>(None);
        let outbound = PlayerOutbound { reliable: reliable_tx, snapshot: snapshot_tx };

        let mut room = room_arc.write().await;
        // Resolve the resume state under the room lock. For a half-open Active handoff we detach
        // the stale entity HERE (reserving its slot), so resume_player can immediately re-admit
        // onto the new socket — net-zero reservation, same as the disconnect_hold→Held path.
        let resume = match (held_resume, &half_open_pid) {
            (Some(resume), _) => resume,
            (None, Some(old_player_id)) => match room.hold_player_for_resume(old_player_id) {
                Some(resume) => {
                    self.telemetry.server_info(
                        "resume_handoff_active",
                        serde_json::json!({
                            "room_id": room_id,
                            "old_player_id": old_player_id,
                            "user_id": user_id,
                        }),
                    );
                    resume
                }
                None => {
                    // The old entity vanished between our claim and the lock (e.g. it was eaten,
                    // or the match ended). Nothing to hand off — fall back to a fresh join.
                    drop(room);
                    return Err("could not resume: the previous connection's entity is gone".to_string());
                }
            },
            (None, None) => unreachable!("resolved claim carries either a held state or a pid"),
        };

        let score = resume.score;
        match room.resume_player(user_id.clone(), nickname, outbound, resume) {
            Ok((player_id, initial_full)) => {
                let server_tick = room.tick;
                let reserved_after = room.reserved_slots();
                drop(room);
                // Fresh token so a SECOND drop in the same match can resume again — registered
                // Active so it, too, survives a half-open reconnect.
                let resume_token = Uuid::new_v4().to_string();
                self.register_active(&resume_token, &room_id, &player_id, &user_id).await;
                let via = if half_open_pid.is_some() { "half-open-handoff" } else { "held-slot" };
                // held_ms = how long this player was OUT of the sim (untouchable) before resuming.
                // For a held slot: now - hold_start, where hold_start = deadline - grace. For a
                // half-open handoff there was no hold window. (review medium #4 instrumentation)
                let grace_ms = (self.config.network.reconnect_grace_sec as u64) * 1000;
                let held_ms = held_deadline.map(|d| now_unix_ms().saturating_sub(d.saturating_sub(grace_ms)));
                info!(
                    "🔄 resume ok: room={} player={} score={} reserved_slots={} via={} held_ms={:?}",
                    room_id, player_id, score, reserved_after, via, held_ms
                );
                self.telemetry.server_info(
                    "resume_success",
                    serde_json::json!({
                        "room_id": room_id,
                        "player_id": player_id,
                        "user_id": user_id,
                        "score": score,
                        "via": via,
                        "held_ms": held_ms,
                        "reserved_slots_after": reserved_after,
                    }),
                );
                self.emit_resume_hold_state("consumed", &room_id, &user_id).await;
                Ok(JoinedRoom {
                    room_id,
                    player_id,
                    server_tick,
                    server_time_ms: now_unix_ms(),
                    resume_token,
                    via_resume: true,
                    initial_full,
                    reliable_rx,
                    snapshot_rx,
                })
            }
            Err(reason) => {
                // resume_player consumed the reservation on its failure path already.
                drop(room);
                Err(format!("could not resume: {reason:?}"))
            }
        }
    }

    pub async fn player_move(&self, room_id: &str, player_id: &str, cmd: MoveCommand) {
        let rooms = self.rooms.read().await;
        if let Some(entry) = rooms.get(room_id) {
            // Bounded — try_send returns Full when the input pipe backed up.
            // Don't drop silently: the client retransmits unacked inputs, but a
            // dropped direction packet is a desync risk, so log it loudly.
            if let Err(e) = entry.input_tx.try_send(PlayerInput {
                player_id: player_id.to_string(),
                seq: cmd.seq,
                target_tick: cmd.target_tick,
                direction: cmd.direction,
            }) {
                tracing::warn!(
                    "⚠️ [ROOM {}] input pipe full, dropped MoveCommand seq={} for {}: {}",
                    room_id,
                    cmd.seq,
                    player_id,
                    e
                );
            }
        }
    }

    /// Route a player-initiated EatClaim into the room. Bounded — a full pipe drops
    /// the claim (the client resends a fresh claim_id while the overlap persists).
    pub async fn player_eat_claim(&self, room_id: &str, player_id: &str, claim: EatClaim) {
        let rooms = self.rooms.read().await;
        if let Some(entry) = rooms.get(room_id) {
            let claim_id = claim.claim_id;
            if let Err(e) =
                entry.eat_claim_tx.try_send(EatClaimInput { player_id: player_id.to_string(), claim })
            {
                tracing::warn!(
                    "⚠️ [ROOM {}] eat claim pipe full, dropped claim={} for {}: {}",
                    room_id,
                    claim_id,
                    player_id,
                    e
                );
            }
        }
    }

    /// Route a victim's claim-based death into the room. RELIABLE delivery — at v6 the claim is the
    /// authoritative, PRECISE death path (rewound to the victim's own render timeline), NOT
    /// best-effort. A dropped claim no longer makes the player immortal: the server-side path is
    /// observe-only only BELOW the sustained-missing-claim threshold and DOES kill above it (the
    /// anti-cheat fallback, decision `kill_now_claim_missing_sustained`, after ~0.5s of lethal
    /// contact with no claim). But that fallback is a blunt, delayed, server-timeline death — we
    /// still want the honest claim to land instead, so we `send().await` (bounded backpressure; the
    /// ring is drained every tick so it won't realistically block), and on a closed channel (room
    /// gone) there's nothing left to deliver to. (v6 review #1; round-3 anti-cheat fallback follow-up)
    pub async fn player_enemy_death_claim(&self, room_id: &str, player_id: &str, claim: EnemyDeathClaim) {
        // Clone the sender out from under the read lock so the await doesn't hold it.
        let tx = {
            let rooms = self.rooms.read().await;
            rooms.get(room_id).map(|entry| entry.enemy_death_claim_tx.clone())
        };
        if let Some(tx) = tx {
            let claim_id = claim.claim_id;
            if let Err(e) = tx.send(EnemyDeathClaimInput { player_id: player_id.to_string(), claim }).await {
                tracing::warn!(
                    "⚠️ [ROOM {}] enemy death claim channel closed, lost claim={} for {}: {}",
                    room_id,
                    claim_id,
                    player_id,
                    e
                );
            }
        }
    }

    /// Route an OBSERVE-ONLY survived-overlap probe into the room. Bounded — a full pipe simply
    /// drops the diagnostic (no gameplay impact; the next overlap re-probes).
    pub async fn player_visual_overlap_probe(
        &self,
        room_id: &str,
        player_id: &str,
        enemy_id: String,
        enemy_generation: u32,
        known_server_tick: u64,
    ) {
        let rooms = self.rooms.read().await;
        if let Some(entry) = rooms.get(room_id) {
            let _ = entry.probe_tx.try_send(VisualOverlapProbeInput {
                player_id: player_id.to_string(),
                enemy_id,
                enemy_generation,
                known_server_tick,
            });
        }
    }
}

/// Observe-only description of a user's resume session (engineer wave 7 #10) — feeds the
/// resume_hold_lookup telemetry; never holds locks or consumes the session.
pub struct ResumeSessionPeek {
    pub room_id: String,
    pub held: bool,
    pub deadline_unix_ms: Option<u64>,
    pub score: Option<u32>,
}

pub struct JoinedRoom {
    pub room_id: String,
    pub player_id: String,
    pub server_tick: u64,
    pub server_time_ms: u64,
    pub resume_token: String,
    /// The admission used the resume path (held slot / half-open handoff / a JoinGame the
    /// server converted) — carried into GameJoined.admitted_via_resume so the client adopts
    /// resume semantics regardless of which message it sent. (engineer wave 6 #3)
    pub via_resume: bool,
    /// Immediate full-state keyframe; do_join_game sends it strictly after
    /// GameJoined so the client inits in a guaranteed order.
    pub initial_full: ServerMessage,
    /// Bounded FIFO of reliable messages (events + GameJoined etc.).
    pub reliable_rx: mpsc::Receiver<ServerMessage>,
    /// Latest-only movement snapshot slot.
    pub snapshot_rx: watch::Receiver<Option<ServerMessage>>,
}

#[cfg(test)]
mod heartbeat_tests {
    use super::*;
    use crate::config_shared::{load_config_from_str, load_maze_from_str};

    /// The tick-loop heartbeat is the readiness probe's "is the simulation
    /// actually running" signal. It must be 0 before start() (so a probe before
    /// the first tick reports not-ready) and advance to a FRESH value once the
    /// loop runs — crucially even with ZERO rooms, because idle != stalled.
    #[tokio::test]
    async fn tick_loop_heartbeat_advances_and_stays_fresh_when_idle() {
        let pool = crate::db::init_db("sqlite::memory:").await.unwrap();
        let config = Arc::new(load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap());
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let telemetry = crate::telemetry::Telemetry::new().await.unwrap();
        let rm = RoomManager::new(pool, config, maze, telemetry);

        // Before start(): no tick has completed, so the heartbeat reads 0 — a
        // readiness probe sees a huge age and reports not-ready (correct: the
        // server can't run a match until the loop is up).
        assert_eq!(rm.last_tick_unix_ms(), 0);

        rm.start().await;
        // A few tick periods at the configured rate, with NO rooms joined.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;

        let hb = rm.last_tick_unix_ms();
        assert!(hb > 0, "heartbeat must advance once the tick loop runs");
        let age = now_unix_ms().saturating_sub(hb);
        // Compare against the SAME threshold the readiness probe uses, not a hardcoded
        // 1000ms — on a loaded CI box a fixed ms bound can flake. (review)
        let threshold = crate::network::health::stall_threshold_ms(rm.tick_rate());
        assert!(
            age < threshold,
            "an idle (zero-room) loop must still beat; age={age}ms threshold={threshold}ms"
        );
    }
}

#[cfg(test)]
mod resume_lifecycle_tests {
    //! Manager-level lifecycle tests for the resume-token registry (Blocker 2). These do NOT run
    //! the tick loop (no `start()`), so a freshly created room stays active for the whole test.
    use super::*;
    use crate::config_shared::{load_config_from_str, load_maze_from_str};

    async fn make_manager() -> RoomManager {
        let pool = crate::db::init_db("sqlite::memory:").await.unwrap();
        // Filler-free baseline: these tests assert exact player_count()s around
        // hold/resume, and a topped-up room would drown those counts in fillers.
        let mut config = load_config_from_str(include_str!("../../gameplay_config.toml")).unwrap();
        config.filler.enabled = false;
        let maze = Arc::new(load_maze_from_str(include_str!("../../maze.json")).unwrap());
        let telemetry = crate::telemetry::Telemetry::new().await.unwrap();
        RoomManager::new(pool, Arc::new(config), maze, telemetry)
    }

    async fn players_in(rm: &RoomManager, room_id: &str) -> usize {
        rm.get_room(room_id).await.unwrap().read().await.player_count()
    }
    async fn reserved_in(rm: &RoomManager, room_id: &str) -> usize {
        rm.get_room(room_id).await.unwrap().read().await.reserved_slots()
    }
    async fn registry_len(rm: &RoomManager) -> usize {
        rm.resume_sessions.read().await.len()
    }

    /// Drop → hold → resume: the held slot is restored and the session consumed; a SECOND resume
    /// with the now-stale token is refused.
    #[tokio::test]
    async fn held_slot_resume_consumes_session_and_rearms_token() {
        let rm = make_manager().await;
        let j = rm.join_room("u1".into(), "alice".into()).await.unwrap();
        let room = j.room_id.clone();
        assert_eq!(players_in(&rm, &room).await, 1);
        assert_eq!(reserved_in(&rm, &room).await, 0);
        assert_eq!(registry_len(&rm).await, 1, "join registers an Active token");

        rm.disconnect_hold(&room, &j.player_id, "u1", &j.resume_token).await;
        assert_eq!(players_in(&rm, &room).await, 0, "entity removed on drop");
        assert_eq!(reserved_in(&rm, &room).await, 1, "slot held");
        assert_eq!(registry_len(&rm).await, 1, "token transitioned Active→Held");

        let r = rm.resume_room(&j.resume_token, "u1".into(), "alice".into()).await.unwrap();
        assert_eq!(players_in(&rm, &room).await, 1, "back in the match");
        assert_eq!(reserved_in(&rm, &room).await, 0, "reservation consumed");
        assert_ne!(r.resume_token, j.resume_token, "a fresh token is minted for the next drop");
        assert_eq!(registry_len(&rm).await, 1, "old token gone, new Active token present");

        // The consumed (old) token can't be resumed again.
        let err = rm.resume_room(&j.resume_token, "u1".into(), "alice".into()).await;
        assert!(err.is_err(), "stale token no longer resolves");
    }

    /// A Resume from a DIFFERENT user is refused WITHOUT burning the session — the legit owner can
    /// still resume afterwards (no slot hijack via a leaked token).
    #[tokio::test]
    async fn wrong_user_refused_then_owner_resumes() {
        let rm = make_manager().await;
        let j = rm.join_room("owner".into(), "alice".into()).await.unwrap();
        rm.disconnect_hold(&j.room_id, &j.player_id, "owner", &j.resume_token).await;

        let bad = rm.resume_room(&j.resume_token, "attacker".into(), "mallory".into()).await;
        assert!(bad.is_err(), "wrong user must be refused");
        assert_eq!(registry_len(&rm).await, 1, "refused attempt must not remove the session");
        assert_eq!(reserved_in(&rm, &j.room_id).await, 1, "slot still held for the owner");

        let ok = rm.resume_room(&j.resume_token, "owner".into(), "alice".into()).await;
        assert!(ok.is_ok(), "the real owner can still resume");
    }

    /// An expired held slot is refused AND its reservation released (no leaked capacity).
    #[tokio::test]
    async fn expired_resume_releases_reservation() {
        let rm = make_manager().await;
        let j = rm.join_room("u1".into(), "alice".into()).await.unwrap();
        rm.disconnect_hold(&j.room_id, &j.player_id, "u1", &j.resume_token).await;
        assert_eq!(reserved_in(&rm, &j.room_id).await, 1);

        // Force the deadline into the past (deterministic — avoids waiting out reconnect_grace_sec).
        {
            let mut map = rm.resume_sessions.write().await;
            if let Some(ResumeSlot::Held { deadline_unix_ms, .. }) = map.get_mut(&j.resume_token) {
                *deadline_unix_ms = 0;
            } else {
                panic!("expected a Held slot after disconnect_hold");
            }
        }

        let err = rm.resume_room(&j.resume_token, "u1".into(), "alice".into()).await;
        assert!(err.is_err(), "expired resume must be refused");
        assert_eq!(reserved_in(&rm, &j.room_id).await, 0, "expired slot's reservation is released");
        assert_eq!(registry_len(&rm).await, 0, "expired session removed");
    }

    /// Half-open socket: a Resume arrives while the old socket is still "live" (no disconnect_hold
    /// yet). The stale entity is force-detached and the slot handed to the new socket — exactly
    /// ONE player remains, not two. The old socket's late cleanup is then a harmless no-op (no
    /// double-hold, no ghost slot). This is the core Blocker-2 scenario.
    #[tokio::test]
    async fn half_open_reconnect_hands_off_without_ghost() {
        let rm = make_manager().await;
        let j = rm.join_room("u1".into(), "alice".into()).await.unwrap();
        let room = j.room_id.clone();
        assert_eq!(players_in(&rm, &room).await, 1);

        // No disconnect_hold (old socket half-open). Resume on a NEW socket with the SAME token.
        let r = rm.resume_room(&j.resume_token, "u1".into(), "alice".into()).await.unwrap();
        assert_ne!(r.player_id, j.player_id, "handoff mints a fresh player entity");
        assert_eq!(players_in(&rm, &room).await, 1, "stale entity replaced, NOT duplicated");
        assert_eq!(reserved_in(&rm, &room).await, 0, "net-zero reservation across the handoff");
        assert_eq!(registry_len(&rm).await, 1, "only the new Active token remains");

        // The old socket finally dies and runs its cleanup with the OLD (now stale) token/pid.
        rm.disconnect_hold(&room, &j.player_id, "u1", &j.resume_token).await;
        assert_eq!(players_in(&rm, &room).await, 1, "late cleanup must NOT remove the live player");
        assert_eq!(reserved_in(&rm, &room).await, 0, "late cleanup must NOT create a phantom hold");
        assert_eq!(registry_len(&rm).await, 1, "registry untouched by the stale cleanup");
    }

    /// An explicit leave drops the Active registration immediately (no lingering entry until the
    /// socket closes).
    #[tokio::test]
    async fn forget_resume_drops_active_registration() {
        let rm = make_manager().await;
        let j = rm.join_room("u1".into(), "alice".into()).await.unwrap();
        assert_eq!(registry_len(&rm).await, 1);
        rm.forget_resume(&j.resume_token).await;
        assert_eq!(registry_len(&rm).await, 0, "explicit leave forgets the token");
        // A resume after an explicit leave is refused (token gone).
        assert!(rm.resume_room(&j.resume_token, "u1".into(), "alice".into()).await.is_err());
    }

    /// forget_resume is named broadly — if it's ever handed a HELD token (which owns a room
    /// reservation), it must RELEASE that reservation, not leak capacity. (review HP1)
    #[tokio::test]
    async fn forget_resume_on_held_releases_reservation() {
        let rm = make_manager().await;
        let j = rm.join_room("u1".into(), "alice".into()).await.unwrap();
        rm.disconnect_hold(&j.room_id, &j.player_id, "u1", &j.resume_token).await;
        assert_eq!(reserved_in(&rm, &j.room_id).await, 1, "held slot reserved");

        rm.forget_resume(&j.resume_token).await;
        assert_eq!(reserved_in(&rm, &j.room_id).await, 0, "forgetting a Held slot frees its reservation");
        assert_eq!(registry_len(&rm).await, 0, "session removed");
    }

    /// The room-eviction purge drops EVERY token (Active or Held) for the evicted room — and ONLY
    /// that room — so a panic/inactive eviction can't leak Active tokens (which have no deadline).
    /// (review HP2)
    #[test]
    fn purge_resume_for_room_removes_active_and_held_for_that_room_only() {
        use crate::protocol::{Direction, Position};
        let resume = || PlayerResumeState {
            position: Position { x: 0.0, y: 0.0 },
            direction: Direction::Right,
            score: 5,
        };
        let mut map: HashMap<String, ResumeSlot> = HashMap::new();
        map.insert(
            "a-r1".into(),
            ResumeSlot::Active { room_id: "R1".into(), player_id: "p1".into(), user_id: "u1".into() },
        );
        map.insert(
            "h-r1".into(),
            ResumeSlot::Held {
                room_id: "R1".into(),
                user_id: "u1".into(),
                resume: resume(),
                deadline_unix_ms: 123,
            },
        );
        map.insert(
            "a-r2".into(),
            ResumeSlot::Active { room_id: "R2".into(), player_id: "p2".into(), user_id: "u2".into() },
        );

        RoomManager::purge_resume_for_room_in_map(&mut map, "R1");

        assert_eq!(map.len(), 1, "both R1 tokens (Active + Held) purged");
        assert!(map.contains_key("a-r2"), "the unrelated room's token is untouched");
    }
}
