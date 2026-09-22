# Soak run record — protocol v6 binary snapshots (beta gate)

**Date:** 2026-06-10
**Verdict:** PASS (strict gate, monitor EXIT=0, bot_runner exit=0)

## Environment

| | |
|---|---|
| Server | dev droplet (1 vCPU), Caddy TLS edge |
| Endpoint | wss://<dev-domain>/ws (real production path, NOT loopback) |
| Backend commit | `4210943` (protocol v6: bincode snapshots, short ids, Full/Closed split) |
| Client protocol | v6 (Unity client `5982fdf` — SnapshotBincode decoder) |
| Load generator | bot_runner from external host (Mac, residential uplink) |

## Load profile

| | |
|---|---|
| Bots / duration | 100 / 300 s |
| Churn | 30% of bots, drop+Resume every ~30 s |
| Cadence per bot | moves 4 Hz, pings 4 Hz, log batches ~0.5 Hz |

## Gates (all enforced, all passed)

| Gate | Threshold | Measured |
|---|---|---|
| Egress per connection | ≤ 30 KB/s | **20.9 KB/s** (99 samples) |
| Tick p95 | ≤ 10 ms | **3.56 ms** |
| Tick p99 | ≤ 16 ms | **8.59 ms** |
| panic / outtx / ctrl / shed | 0 (strict) | **0 / 0 / 0 / 0** |
| /readyz | 200 all samples | ✓ |
| Bot errors / never_joined / unplanned disconnects | 0 (strict) | **0 / 0 / 0** |

## Results vs JSON baseline (run 2026-06-09, same profile)

| Metric | JSON (v5) | Binary (v6) | Δ |
|---|---|---|---|
| Egress per connection | 97 KB/s | 20.9 KB/s | **4.6× less** |
| Total egress @100 conns | 78 Mbit/s | 16.5 Mbit/s | 4.7× less |
| Mobile data per hour of play | ~350 MB | ~75 MB | 4.6× less |
| Tick p95 / p99 | 3.23 / 6.37 ms | 3.56 / 8.59 ms | parity |
| RSS peak | 17.4 MB | 16.5 MB | parity |
| Host CPU (loaded, steady) | ~40–50% | ~40–50% | parity |
| Snapshots delivered (bots) | 878 812 | 889 744 | parity, 0 decode errors |
| Matches completed | 696 | 694 | parity |

## Notes

- Round 1 of v6 failed the strict gate with outtx=69 — diagnosed as churn-teardown
  noise (Closed channel logged as overflow), fixed in `4210943`, round 2 clean.
- host_cpu peak 100% = a single 3 s sample at start (100 concurrent TLS handshakes
  through Caddy); steady-state 40–50%.
- over_budget_delta = 27 ticks / 6 min — not gated yet; pick a threshold for
  future runs (`--max-over-budget-delta`).
- OPEN: on-device confirmation pending — `ws_rx_kbs` from iPhone telemetry must
  corroborate ~21 KB/s, and feel diagnostics (rtt/jitter/interp delay, death
  fairness logs) must show no regression.
- ADDENDUM 2026-06-11: gates from this record were consolidated into
  [docs/perf-baseline.md](../perf-baseline.md) — the single source of truth for
  all future runs; the open `over_budget_delta` threshold is fixed there (≤ 30
  per 380 s window).

## Reproduce

```sh
# droplet
python3 /tmp/soak_monitor.py --url http://<container-ip>:8080 --pid <pid> \
  --server-log /tmp/soak_server.log --logs-dir /opt/fairtick/logs \
  --interval 3 --duration 380 --strict --net-iface eth0 \
  --max-tx-kbs-per-conn 30 --max-p95-ms 10 --max-p99-ms 16
# external host
bot_runner --count 100 --duration 300 --server wss://<dev-domain>/ws \
  --churn-frac 0.3 --strict
```

**Artifacts:** bot `metrics/bot_run_1781109803.json` (load host), monitor
`/tmp/soak_monitor.{out,ndjson}` (droplet), server log `/tmp/soak_server.log`.
