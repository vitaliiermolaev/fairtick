# Soak run record — v10 COMBINED: full client-ready player-path on real hardware
**Date:** 2026-07-05
**Verdict:** PASS with capacity-note (strict gate; bot_runner exit=0 both runs; monitor exit=0 on the clean re-run, exit=1 on run 1 solely on over_budget 35 vs 30 — documented tail variance)
**Floors:** per docs/perf-baseline.md

---

## Why this run exists
v8 proved network economics on the real droplet, but bots were rejected by the safety
gate (not client-ready) → eat-path never exercised. v9 proved the full filler→human
eat-path under load, but on loopback → egress/host-CPU not measured. This record closes
the gap with a **two-point matrix on the same $12 hardware**:

- **v10** — 100 CCU, real network path, full `--client-ready` handshake, egress/CPU/latency measured. Eat-path dormant under load *by design* (full rooms → filler mechanic idle).
- **v10b** — sparse profile, real filler→human eats on the same droplet, egress/CPU measured.

Together = full player-path is cheap at capacity **and** eat-path is proven on real hardware, with no gap.

---

## Environment
| | |
|---|---|
| Server | dev droplet (1 vCPU / 2 GB, lon1), Caddy TLS edge, restored from pause snapshot |
| Endpoint | wss://<dev-domain>/ws (real production path; DNS repointed to new IP, LE cert survived the snapshot) |
| Backend build | b10707b (PR #11 `feature/filler-bots` merge on `main`; proto=6, tick_rate=60) |
| Filler config | enabled=true, target 8, max_transient 10; PvE mobs [ai] count = 0 |
| Load generator | bot_runner (release) from external Mac, residential uplink |

---

## v10 — 100 CCU, full client-ready path (capacity point)

**Load profile:** 100 bots / 300 s / 30% churn / moves 4 Hz / pings 4 Hz / `--client-ready` / strict.
Monitor strict: `--max-p95-ms 10 --max-p99-ms 16 --max-over-budget-delta 30 --net-iface eth0 --max-tx-kbs-per-conn 30`.
Run twice (run 1, then an identical re-run to test over_budget variance).

### Client-ready handshake
| Metric | Value |
|---|---|
| Bots completing full handshake (auth + world_ready + claim_ready) | 100 / 100 (never_joined=0; ClientWorldReady + ClientClaimReady fired on every admission, incl. churn re-joins) |
| never_joined | 0 |
| resumes rejected | 0 (run 2) · 1 of 275 = 0.4% (run 1) |

### Network economics (the point vs v9)
| Gate | Floor | Measured |
|---|---|---|
| Egress per connection | ≤ 30 KB/s | **12.2–13.3** KB/s |
| Host CPU (steady) | — | median **44–47%** / p90 50–56% / peak 57–63% |
| Load avg (1-min, 1 vCPU) | < 1.0 | **0.9** (Grafana) |
| RSS peak | ≤ 64 MB | **17** MB |
| Bot max RTT (internet, churn incl.) | — | 192–250 ms |

### Stability / latency
| Gate | Floor | Measured (run 1 · run 2) |
|---|---|---|
| Tick p95 / p99 (peak) | ≤ 10 / ≤ 16 ms | 3.74 / **12.17**  ·  3.41 / **10.07** ms |
| over_budget_delta | ≤ 30 | **35**  ·  **23** — see capacity note |
| panic / outtx / ctrl / shed | 0 | 0 / 0 / 0 / 0 |
| /readyz | 200 always | ✓ (0 non-200) |
| Bot errors / never_joined / unplanned disconnects | 0 | 0 / 0 / 0 |
| Matches / snapshots | — | 699 / 881,958  ·  700 / 885,205 |

### Capacity note — over_budget 35 vs floor 30
First run where the **full `--client-ready` path** executed at 100 CCU on the droplet.
v8 bots were rejected by the safety gate (cheaper), so over_budget was 5; the honest
handshake cost raises it to 35. **No tick breached the p99 budget** (peak 12.17 ms <
16.67 ms) — this is 35 tail max-ticks out of 22,800 (0.15%), accumulating in steady
state and correlating with the CPU hump at 17:30–17:34 BST.
Stability re-run result: **varies 23–35 across two identical runs** (straddles the floor).
Disposition: documented as the tail-variance cost of the client-ready handshake; **floor
left at ≤ 30** (not loosened — credibility over a green checkmark), lead sign-off recorded
in docs/perf-baseline.md History. Not a beta or sale blocker. Optional post-sale work:
profile the mild p99 cluster at the CPU peak (optimization, not correctness).

---

## v10b — sparse profile, real eat-path on hardware (correctness point)

**Load profile:** 4 bots / 180 s / 50% churn (rooms under-filled so fillers persist and hunt) / `--client-ready` / strict, same droplet + egress/CPU monitoring.

### Eat-path results (the point of this run)
| Metric | Value |
|---|---|
| filler→HUMAN eats accepted (victim_is_human=true) | **3** |
| filler→filler eats accepted | 1 |
| victim_not_claim_ready rejects | **0** — every bot earned claim-ready trust, the gate had nothing to block (matches v9, now on real HW) |
| human-victim rejects (honest gates only) | 1 — victim_invincible 0*, safe_zone 0, spawn_protected 0, resume_shield 1 (*the 1 victim_invincible reject had a filler victim) |
| mystery / stale-claim / ledger rejects | **0** |
| eaten clients | PlayerEaten + respawn delivered (player_respawned=4); victims kept playing; resumes rejected 0. Fillers persisted (35 spawned / 15 removed / 17 exit_postponed_near_human) vs 561/560 immediate removal at 100 CCU |

### Network economics during active eat-path (proves eat doesn't inflate cost)
| Metric | Measured |
|---|---|
| Egress per connection | n/a — 4 CCU is below the monitor's ≥10-conn egress-gate floor, so per-conn egress is (correctly) not quoted, same rule as loopback |
| Host CPU | median 5.1% / peak 9.4% |
| Tick p95 / p99 | 0.23 / 0.5 ms |

---

## The combined argument (v10 + v10b together)
On a single $12/mo droplet over the real internet:
1. **Full player-path is cheap at capacity** — 100 live players, full client-ready handshake, egress 12–13 KB/s, p99 ≤ 12.2 ms, load < 1.0. *(v10)*
2. **Eat-mechanic is proven on real hardware** and only activates when rooms are
   under-filled — so under full load it is dormant *by design* and adds no egress/CPU
   cost. This is not a gap; it is the mechanic behaving correctly. *(v10b)*

→ The full real-time game loop runs on commodity hardware at a per-player cost of
~13 KB/s egress and ~0.5% of one vCPU, with **zero errors across 1.77 M snapshots and
1,399 matches** (the two 100-CCU runs combined).

---

## Reproduce
```bash
# One-shot harness (monitor on droplet + client-ready bots locally, artifacts collected):
scripts/soak_combined.sh root@<droplet-ip> 100 300 0.3 v10        # 100-CCU capacity
scripts/soak_combined.sh root@<droplet-ip> 100 300 0.3 v10-rerun  # variance re-run
scripts/soak_combined.sh root@<droplet-ip> 4   180 0.5 v10b-sparse # real eat-path

# Equivalent manual form (what the harness runs):
# on the droplet — container-ip 172.18.0.3, host-pid 1421 this run:
python3 /tmp/soak_monitor.py --url http://172.18.0.3:8080 --pid 1421 \
  --server-log /tmp/soak_server_v10.log --logs-dir /opt/fairtick/logs \
  --interval 3 --duration 380 --strict --max-p95-ms 10 --max-p99-ms 16 \
  --max-over-budget-delta 30 --net-iface eth0 --max-tx-kbs-per-conn 30
# on the external Mac:
bot_runner --count 100 --duration 300 --server wss://<dev-domain>/ws \
  --churn-frac 0.3 --strict --client-ready
# v10b sparse: same, --count 4 --duration 180 --churn-frac 0.5 (sparseness = low count; no flag)
```

## Artifacts
- v10 run 1 bots: `metrics/combined-20260705T162732Z/bot_run_1783268860.json`
- v10 run 2 bots: `metrics/combined-v10-rerun-20260705T165537Z/bot_run_1783270545.json`
- v10 monitor: `soak_monitor_v10.{out,ndjson}` (run 1) · `soak_monitor_v10-rerun.{out,ndjson}` (run 2), inside those dirs
- v10b bots: `metrics/combined-v10b-sparse-20260705T165004Z/bot_run_1783270212.json`
- v10b monitor: `soak_monitor_v10b-sparse.{out,ndjson}` (same dir)
- server logs / telemetry: `soak_server_*.log` + `fairtick-run_05-07-2026_16-17-57_000.ndjson` in each dir
- config_hash: `6483caf2170936b4a154b12edc983c83a90774ab38b66a346d0e2b2c14e70be6` · maze_hash: `58e148a3e1f0c54251f515b5c4b332abad3ba4b8c3bcb08a467662fcbfc9b418` · seed: `3203386110`

---

## Notes for the reader
- All floors are **internal CI gates**, deliberately stricter than playability requires
  (e.g. p99 budget 16 ms; observed 10–12 ms).
- over_budget is a tail-latency tripwire, not a breach: 0.10–0.15% of ticks, none over the p99 budget.
- Hardware is intentionally the cheapest tier ($12/mo, 1 vCPU) to demonstrate floor cost, not a tuned ceiling.
