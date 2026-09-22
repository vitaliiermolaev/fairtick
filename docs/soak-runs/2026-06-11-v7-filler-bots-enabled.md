# Soak run record — v7 filler bots ENABLED (beta pre-gate, local)

**Date:** 2026-06-11
**Verdict:** PASS (strict gate; bot_runner exit=0, monitor exit=0)

## Environment

| | |
|---|---|
| Server | LOCAL loopback (Mac dev box), release build, port 9100 |
| Backend commit | feature/filler-bots `dccafd6` (+ this report's branch tip) |
| Filler config | enabled=true, target_visible_players=8, max_transient=10, exit 48..150t |
| PvE mobs | `[ai] count = 0` (mob-free, the shipping intent) |
| DB | sqlite /tmp (fresh) |

> ⚠️ LOCAL run: egress-per-connection and host-CPU gates are NOT meaningful over
> loopback and were not enforced. This run answers the filler-specific questions
> (tick budget, lifecycle churn, overflow, room GC, internal-eat load). The
> droplet wss re-run (profile identical to the 2026-06-10 v6 record) must happen
> at the next dev deploy before the invite wave.

## Load profile (main run)

| | |
|---|---|
| Bots / duration | 100 / 300 s |
| Churn | 30% of bots, drop+Resume every ~30 s |
| Cadence per bot | moves 4 Hz, pings 4 Hz, log batches ~0.5 Hz |
| Matchmaking shape | rooms pack to max_players=10 humans → fillers yield via delayed exits |

## Gates (strict, all passed — floors per [docs/perf-baseline.md](../perf-baseline.md))

| Gate | Threshold | Measured |
|---|---|---|
| Tick p95 (peak sample) | ≤ 10 ms | **2.03 ms** |
| Tick p99 (peak sample) | ≤ 16 ms | **2.24 ms** |
| over_budget_delta | ≤ 30 / 380 s | **1** |
| panic / outtx / ctrl / shed | 0 (strict) | **0 / 0 / 0 / 0** |
| /readyz | 200 all samples | ✓ (0 non-200) |
| Bot errors / never_joined / unplanned disconnects | 0 (strict) | **0 / 0 / 0** |
| RSS peak | ≤ 64 MB (manual hard gate) | **29.9 MB** — passed |
| Rooms at end (GC) | 0 (manual hard gate) | **0** (peak 90, all collected) — passed |

(Hard floors per [docs/perf-baseline.md](../perf-baseline.md). This run predates the
baseline file; the gates above were reconciled to it post-hoc — every floor holds.
RSS and rooms-at-end were verified manually from the monitor output, not enforced
by a flag: noted explicitly so PASS never reads as "tool-enforced" when it wasn't.)

## Bot-side results (main run)

| Metric | Value |
|---|---|
| Snapshots delivered | 897 824 |
| Moves sent | 111 968 |
| Matches completed | 698 |
| Planned reconnects (churn) | 285 |
| Resume rejected (expected: room ended mid-drop) | 2 |
| Max RTT | 19 ms |

## Filler behavior under load (server telemetry, main run)

| Event | Count | Read |
|---|---|---|
| filler_spawned | 1645 | rooms top up at first-human admission |
| filler_exit_scheduled / filler_removed | 446 / 446 | every scheduled exit completed; rest cleared at game end |
| filler_exit_postponed_near_human | 28 | visibility recheck fires in practice |
| filler_spawn_postponed_no_safe_position | 0 | map is sparse enough at this density |
| filler_internal_eat_accept / reject | 188 / 219 | fillers eat each other (synthetic bots are never claim_ready, so filler→bot eats are correctly blocked) |
| filler_room_swept | 0 | rooms always held humans or ended naturally (unit test covers the sweep) |
| room_population heartbeats | 685 | 10 s cadence held |
| rooms ended | 235 | |
| filler_top1 / human_lost_to_filler | 170 / 235 (72%) | see note below |

**filler_top1 = 72% is a PROFILE ARTIFACT, not balance data:** synthetic bots
random-walk and never pursue points, while fillers actively collect — of course
fillers out-score them. The official GameEnded winner is the best human by
construction. Watch this metric in REAL beta sessions; if it stays > ~30% with
humans actually playing, tune filler greed/aggression down.

## Burst profile (100 bots, 0 churn, 60 s — connect storm)

**Round 1: FAIL (strict, errors=1 never_joined=1) → diagnosed → fixed → round 2 PASS.**

Round 1 findings, both NOT filler-specific:

1. **Join-storm retry exhaustion.** 100 simultaneous joiners all read the same
   candidate room; 10 win, the rest re-race — worst case ~N/max_players rounds.
   `MAX_JOIN_ATTEMPTS` was 8; bot_35 lost 8 straight races (895 room-full
   rejections across the storm) and errored. Pre-existing constant, exposed by
   the burst. **Fixed:** bound raised to 64 (each retry is a lock + map scan and
   every round makes global progress). Round 2: 26 rejections total, 100/100
   joined, 0 errors.
2. **Reliable-overflow teardown noise at mass deadline-drop.** All 100 bots
   abandon sockets the same instant at run end → PlayerLeft broadcast storms
   into dead-but-not-yet-closed lanes → 8 forced-resync teardowns at one tick.
   This is the designed dead-lane teardown (drop outbound, force resync);
   distinct from the v6 closed-channel noise already fixed. No action.

Also fixed from round 1: the room-full log line showed total ENTITIES vs the
human cap ("room full (17/10)") — misleading in a filler-populated room; now
logs humans+reserved vs cap plus entity count separately.

| Round 2 gate | Measured |
|---|---|
| Joined / errors / unplanned disconnects | **100/100, 0, 0** |
| Snapshots / matches | 180 000 / 100 |
| Max RTT | 20 ms |
| Tick p95 / p99 during storm | 0.32 / 0.42 ms |
| Join-race rejections | 26 (vs 895 with the old bound) |

## Sparse profile (4 bots, 50% churn, 120 s — sustained filler brains)

Strict PASS (exit 0): 14 356 snapshots, 12 matches, 7 resumes, 0 errors.
The interesting part is sustained filler ACTIVITY in an underfilled room
(4 humans + 4-6 fillers for the whole run):

| Filler activity (2-minute window) | Value |
|---|---|
| Internal eats accepted / rejected | 9 / 20 |
| Goal histogram (population heartbeats) | wander 59, flee 18, hunt 13, booster 6, collect 3 |

The goal mix reads as "players playing the game" (wander-dominant with
occasional hunts/flees), and fillers eat each other at a believable rate while
every filler→human attempt on the never-claim-ready synthetic bots was
correctly rejected.

## Notes

- Internal eats run AFTER human claims each tick; ledger records filler kills so
  stale human claims reject as kill-by-corpse. No claim-path warnings surfaced.
- The transient-cap path (expedited exits) is exercised by every join burst into
  a topped-up room; covered by unit scenario + the burst profile here.
- `peak rooms = 90`: ended rooms awaiting their humans' socket close + the 60-tick
  GC sweep cadence; all collected to 0 by run end — no room leak with fillers.
- Telemetry log growth 19.76 MB / 380 s @ 100 CCU is dominated by per-room
  heartbeats + filler lifecycle at full churn; fine for beta retention settings.

## Reproduce

```sh
FAIRTICK_BIND_ADDR=0.0.0.0:9100 DATABASE_URL="sqlite:/tmp/soak_filler.db?mode=rwc" \
  ./target/release/fairtick > /tmp/soak_server_filler.log 2>&1 &
python3 scripts/soak_monitor.py --url http://127.0.0.1:9100 --pid <pid> \
  --server-log /tmp/soak_server_filler.log --logs-dir logs --interval 3 \
  --duration 380 --strict --max-p95-ms 10 --max-p99-ms 16 \
  --max-over-budget-delta 30
./target/release/bot_runner --count 100 --duration 300 \
  --server ws://127.0.0.1:9100/ws --out ./metrics --churn-frac 0.3 --strict
```

**Artifacts:** bots `metrics/bot_run_1781195786.json`, monitor
`/tmp/soak_monitor_filler.{out,ndjson}`, server log `/tmp/soak_server_filler.log`,
telemetry `logs/fairtick-run_11-06-2026_16-36-05_000.ndjson`.
