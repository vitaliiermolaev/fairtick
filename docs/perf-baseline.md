# Performance baseline — hard floors for every perf-measuring run

**This file is the single source of truth for performance gates.** Every soak /
load / burst run (and any future perf harness) MUST quote these floors and fail
when it dips below them. A run record that says PASS means "passed THIS file's
floors as of its date". Reports live in `docs/soak-runs/` and must link here.

Rules of change:

- Floors only get **stricter** silently. Loosening a floor requires a lead
  decision recorded in the History section below (date, old → new, why).
- Reference values (the "best known" column) update freely as better runs land —
  they exist so a regression is visible long before it hits the floor.
- If a tool default and this file disagree, THIS FILE wins; fix the tool.

## Environments

| Env | What it is | Which gates apply |
|---|---|---|
| **droplet** | dev droplet (1 vCPU) behind Caddy TLS, `wss://<dev-domain>/ws`, bot_runner from an external host | ALL gates incl. egress + host CPU. The only env whose PASS counts for a beta go/no-go. |
| **local** | release server on loopback (Mac dev box) | tick / warns / readyz / GC / bot gates. Egress + host-CPU numbers are meaningless here — never quote them as results. |

## Canonical profiles

| Profile | Command core | Purpose |
|---|---|---|
| **gate** | `bot_runner --count 100 --duration 300 --churn-frac 0.3 --strict` + monitor below | the go/no-go soak |
| **burst** | `bot_runner --count 100 --duration 60 --churn-frac 0 --strict` | connect-storm: matchmaking races, transient population cap |
| **sparse** | `bot_runner --count 4 --duration 120 --churn-frac 0.5 --strict` | sustained filler brains/eats in underfilled rooms |

Monitor (gate profile, exact flags — these ARE the floors below):

```sh
python3 scripts/soak_monitor.py --url <base> --pid <pid> \
  --server-log <log> --logs-dir logs --interval 3 --duration 380 \
  --strict --max-p95-ms 10 --max-p99-ms 16 --max-over-budget-delta 30 \
  [droplet only:] --net-iface eth0 --max-tx-kbs-per-conn 30
```

## Hard floors (fail the run if violated)

Tick budget @ 60 tps = 16.67 ms.

| Gate | Floor | Env | Notes |
|---|---|---|---|
| `/readyz` | 200 on EVERY sample | both | one non-200 = fail |
| panic / room eviction | 0 | both | |
| `outtx` (reliable overflow on live lanes) | 0 | both | mass-teardown noise at run END must be diagnosed + documented in the report, never waved through silently |
| `ctrl` (dropped control replies) | 0 | both | |
| `shed` (rate-limit drops on the honest profile) | 0 | both | |
| Tick p95 (peak sample) | ≤ 10 ms | both | |
| Tick p99 (peak sample) | ≤ 16 ms | both | i.e. p99 never exceeds the tick budget |
| over_budget_delta | ≤ 30 / 380 s window | both | threshold the v6 record left open — now fixed |
| Egress per connection | ≤ 30 KB/s | droplet | binary snapshots; see v6 record for the JSON→bincode history |
| Bot errors | 0 | both | |
| Bots never_joined | 0 | both | incl. the burst profile — a connect storm may not strand anyone |
| Unplanned disconnects | 0 | both | planned churn drops excluded by the runner |
| RSS peak @ 100 CCU | ≤ 64 MB | both | generous ceiling; the real signal is the reference column + no monotonic growth across the run |
| Rooms after run + GC sweep | 0 | both | no room (incl. filler-only) may outlive its humans + grace |
| resumes_rejected | ≤ 5% of planned reconnects | both | each one must be explainable (room ended mid-drop) |

## Reference best-known (update freely; regressions vs these get a report note)

| Metric | droplet best (v8, 2026-06-11) | local best (v7, 2026-06-11) |
|---|---|---|
| Tick p95 / p99 peak | 2.52 / 5.62 ms | 2.03 / 2.24 ms |
| Egress per conn | 12.5 KB/s | n/a |
| RSS peak | 15.4 MB | 29.9 MB |
| over_budget_delta | 5 / 380 s | 1 / 380 s |
| Steady-state host CPU @100 CCU | 35 % median | n/a |
| Max bot RTT | 383 ms (internet + churn) | 20 ms |
| Matches completed (gate profile) | 698 | 698 |
| Snapshots delivered | 890 181 | 897 824 |
| Burst: join-race rejections | n/a | 26 (100 simultaneous joiners) |

**Two droplet load profiles now exist — read the over_budget column with this.**
The v8 column above is the *rejected-path* profile (bots never claim-ready, every
filler→human eat rejected — cheaper). The **client-ready** profile (bots walk the
real `ClientWorldReady`/`ClientClaimReady` handshake — the actual beta path) is
more expensive on the over_budget tail: v10 (2026-07-05, same droplet, 100 CCU)
measured **over_budget 23 and 35 across two runs**, straddling the ≤ 30 floor, with
p99 10–12 ms (in budget), egress 12–13 KB/s/conn, host CPU median 44–47 %. Treat
23–35 as the client-ready reference range; the ≤ 30 floor is unchanged (a run
that lands 35 is documented tail variance, not a regression — see the v10 record).

Capacity limits & hardware forecast: [capacity-planning.md](capacity-planning.md)
(feel-safe knee ≈ 170–180 CCU on 1 vCPU; recommended cap 150).

## Qualitative checks (must hold, reported not thresholded)

- Filler lifecycle alive under load: spawns/exits flowing, exit postpones firing
  near humans, zero `filler_spawn_postponed_no_safe_position` pile-ups.
- Internal eats: accepts AND rejects present in sparse profile; every
  filler→human attempt on not-claim-ready clients rejected.
- `filler_top1` rate: profile artifact vs synthetic bots — but in runs with REAL
  clients, > ~30% means tune filler greed/aggression down (lead 2026-06-11).
- Telemetry log growth sane for retention settings (see ops rotation caps).

## History

- **2026-06-11** — initial baseline. Floors consolidated from the v6 droplet
  record (egress/tick/warn gates) + v7 filler run (burst/never_joined, GC-to-0,
  resume %). `over_budget_delta ≤ 30/380s` fixed (was an open TODO in v6).
  RSS ceiling 64 MB set at ~2× the worst observed peak.
- **2026-07-05** — v10 combined run introduced the **client-ready** droplet load
  profile (real readiness handshake). over_budget floor **kept at ≤ 30** (NOT
  loosened); documented that the client-ready tail is 23–35 across runs and grazes
  the floor as variance, p99 always in budget. Lead sign-off (this is a reference
  note, not a floor change). See
  [v10 record](soak-runs/2026-07-05-v10-combined-eat-path-real-hardware.md).
