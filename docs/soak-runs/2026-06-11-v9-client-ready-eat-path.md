# Soak run record — v9 REALISTIC profile: filler→human eat path under load

**Date:** 2026-06-11
**Verdict:** PASS (strict; bot_runner exit=0, monitor exit=0)
**Floors:** per [docs/perf-baseline.md](../perf-baseline.md)

## Why this run exists

Every earlier soak ran bots that never sent ClientWorldReady/ClientClaimReady, so
every filler→bot eat was (correctly) rejected — proving the SAFETY gate, but not
the path real beta players will live on. `bot_runner --client-ready` (added this
run) walks the real client's readiness handshake: GameJoined → first snapshot →
ClientWorldReady → clean pong → ClientClaimReady. Fillers can then legally eat
the bots (lead review: "bots correctly eat claim-ready clients" was unproven).

## Environment & profile

LOCAL loopback, release build, port 9102, fillers ON, `[ai] count = 0`.
100 bots / 300 s / 30% churn / `--client-ready` / strict; monitor strict with
`--max-p95-ms 10 --max-p99-ms 16 --max-over-budget-delta 30`.

## Eat-path results (the point of the run)

| Metric | Value |
|---|---|
| filler→HUMAN eats accepted (victim_is_human=true) | **72** |
| filler→filler eats accepted | 197 |
| human-victim rejects | 13 — victim_invincible 34*, safe_zone 30*, spawn_protected 2, resume_shield 1 (*reason totals incl. filler victims) |
| `victim_not_claim_ready` rejects | **0** — every bot earned trust, the gate had nothing to block |
| stale-claim / ledger warnings | 0 |
| eaten bots' clients | no errors: PlayerEaten + respawn delivered; victims kept playing and resuming (285 churn resumes, 0 rejected) |

Every reject reason is an HONEST gameplay gate (booster invincibility, safe
zone, spawn protection, resume shield) — no mystery rejects under load.

## Standard gates (all passed)

Bots: 897 308 snapshots, 700 matches, 0 errors, 0 never_joined, 0 unplanned
disconnects, max RTT 18 ms. Monitor: strict PASS within the baseline floors
(loopback run — egress/host-CPU not quoted, as per baseline env rules).

## Reproduce

```sh
FAIRTICK_BIND_ADDR=0.0.0.0:9102 DATABASE_URL="sqlite:/tmp/soak_ready.db?mode=rwc" \
  ./target/release/fairtick &
python3 scripts/soak_monitor.py --url http://127.0.0.1:9102 --pid <pid> \
  --server-log /tmp/soak_ready_server.log --logs-dir logs --interval 3 \
  --duration 380 --strict --max-p95-ms 10 --max-p99-ms 16 --max-over-budget-delta 30
./target/release/bot_runner --count 100 --duration 300 \
  --server ws://127.0.0.1:9102/ws --out ./metrics --churn-frac 0.3 --strict --client-ready
```

**Artifacts:** bots `metrics/bot_run_1781209366.json`, monitor
`/tmp/soak_ready_monitor.{out,ndjson}`, telemetry
`logs/fairtick-run_11-06-2026_20-22-18_000.ndjson`.

The SAFETY profile (no `--client-ready`) remains the v7/v8 records: bots never
claim-ready, every filler→human attempt rejected.
