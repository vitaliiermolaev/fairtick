# Soak run record — v8 filler bots ENABLED on the droplet (beta gate)

**Date:** 2026-06-11
**Verdict:** PASS (strict gate; bot_runner exit=0, monitor exit=0, perf_fail=[])
**Floors:** per [docs/perf-baseline.md](../perf-baseline.md)

## Environment

| | |
|---|---|
| Server | dev droplet (1 vCPU / 2 GB), Caddy TLS edge |
| Endpoint | wss://<dev-domain>/ws (real production path) |
| Backend build | `a9add11` (feature/filler-bots tip: filler v1 + P0/P1 fixes + kill switch) |
| Filler config | enabled=true, target 8, max_transient 10; PvE mobs `[ai] count = 0` |
| Load generator | bot_runner (release) from external Mac, residential uplink |

## Load profile

100 bots / 300 s / 30% churn (drop+Resume ~30 s) / moves 4 Hz / pings 4 Hz /
logs ~0.5 Hz — the canonical **gate** profile.

## Gates (all enforced, all passed)

| Gate | Floor | Measured |
|---|---|---|
| Egress per connection | ≤ 30 KB/s | **12.5 KB/s** (99 samples) |
| Tick p95 / p99 (peak) | ≤ 10 / ≤ 16 ms | **2.52 / 5.62 ms** |
| over_budget_delta | ≤ 30 / 380 s | **5** |
| panic / outtx / ctrl / shed | 0 | **0 / 0 / 0 / 0** |
| /readyz | 200 always | ✓ (0 non-200) |
| Bot errors / never_joined / unplanned disconnects | 0 | **0 / 0 / 0** |
| RSS peak | ≤ 64 MB | 15.4 MB |

Steady-state host CPU (t=60..280): median **35%**, p90 40%, max 45%. The 100%
peak is the initial 100-TLS-handshake storm (same artifact as v6).

## vs v6 (the pre-filler droplet record, same profile)

| Metric | v6 (mobs ON, no fillers) | v8 (fillers ON, mobs OFF) |
|---|---|---|
| Egress per conn | 20.9 KB/s | **12.5 KB/s** (−40%: no 12-mob enemy list in every snapshot) |
| Tick p95 / p99 | 3.56 / 8.59 ms | **2.52 / 5.62 ms** |
| RSS peak | 16.5 MB | 15.4 MB |
| Matches | 694 | 698 |
| Bot max RTT | n/a | 383 ms (internet path, churn reconnects included) |

Filler bots are net CHEAPER than the PvE mobs they replace: fewer simulated
entities per room at human-packed capacity (fillers yield seats; mobs never
did), and snapshots dropped the enemies list weight.

## Reproduce

```sh
# droplet (monitor)
python3 /tmp/soak_monitor.py --url http://<container-ip>:8080 --pid <host-pid> \
  --server-log /tmp/soak_server.log --logs-dir /opt/fairtick/logs \
  --interval 3 --duration 380 --strict --max-p95-ms 10 --max-p99-ms 16 \
  --max-over-budget-delta 30 --net-iface eth0 --max-tx-kbs-per-conn 30
# external host
bot_runner --count 100 --duration 300 --server wss://<dev-domain>/ws \
  --churn-frac 0.3 --strict
```

**Artifacts:** bots `metrics/bot_run_1781200536.json` (load host); monitor
`/tmp/soak_monitor_v8.{out,ndjson}`, server log `/tmp/soak_server.log` (droplet).
