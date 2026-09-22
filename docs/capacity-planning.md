# Capacity planning — measured limits & forecast

**Measured:** 2026-06-11, dev droplet (DigitalOcean, **1 vCPU / 2 GB RAM**),
Caddy TLS on the same box, backend build `a9add11` (fillers ON, PvE mobs off),
real wss path. Method: stepped CCU ramp (150 → 200 → 250 → 300, 150 s each,
15% churn) on top of the passing 100-CCU gate run
([v8 record](soak-runs/2026-06-11-v8-filler-droplet-gate.md)). Floors per
[perf-baseline.md](perf-baseline.md).

## Measured ramp (steady-state per step)

| CCU | host CPU med/p90/max | tick p95/p99 peak (ms) | over_budget (step) | egress/conn | RSS | verdict vs floors |
|---|---|---|---|---|---|---|
| 100 | 35 / 40 / 45 % | 2.52 / 5.62 | Δ5 | 12.5 KB/s | 15 MB | **PASS** |
| 150 | 51 / 58 / 63 % | 3.50 / 6.71 | Δ≈13 | 11.6 KB/s | 18 MB | **PASS** |
| 200 | 66 / 72 / 78 % | 5.96 / 15.05 | Δ≈430 | 11.6 KB/s | 23 MB | **FAIL** (over_budget; p99 at the edge) |
| 250 | 82 / 90 / 95 % | 17.77 / 30.62 | hundreds | 11.6 KB/s | 23 MB | FAIL (p95 + p99) |
| 300 | 94 / 97 / 98 % | 26.57 / 42.80 | Δ≈1200 | 11.4 KB/s | 26 MB | FAIL (CPU saturated) |

Crucially: **degradation is graceful.** Even at 300 CCU on one core, the
player-visible failure count was ZERO — bot errors 0, never_joined 0, unplanned
disconnects 0, /readyz 200 on every sample, no panics/sheds/overflows across the
whole ramp. The server runs late (long ticks → laggy feel), it does not break.

## The model (1 vCPU, TLS included)

CPU is linear in CCU across the measured range:

```
host_cpu% ≈ 6% + 0.295% × CCU        (fit of 100→35, 150→51, 200→66, 250→82, 300→94)
```

- **Feel-safe knee:** between 150 (PASS) and 200 (over_budget floor breaks):
  ≈ **170–180 CCU** on this box. Past ~80 % CPU, tick latency goes non-linear.
- **Bottleneck is CPU only.** Egress is flat ~11.6 KB/s per conn (300 CCU ≈
  28 Mbit/s — far below droplet bandwidth); RSS is trivial (26 MB at 300 CCU on
  a 2 GB box); disk is rotation-capped. Memory/network/disk would carry
  thousands of CCU; the single core will not.
- Per-room cost dominates over per-conn cost: rooms pack to 10 humans, so CCU/10
  rooms each tick run movement + collisions + claims + (now) filler lifecycle.
  Fillers proved net cheaper than the mobs they replaced (see v8 record).

## Operating limits — current hardware

| | |
|---|---|
| **Recommended cap (good feel)** | **150 CCU** |
| Absolute graceful max (degraded feel, no failures) | ~300 CCU |
| Action | set `FAIRTICK_MAX_CONNECTIONS=150` on the 1 vCPU droplet — the default 500 lets the box accept load it can only serve badly; refusing conn #151 protects the feel of the 150 already playing |

## Forecast — future hardware

Room updates are spawned as parallel tasks per tick (one task per room, join
barrier per tick), so the simulation parallelizes across rooms and scales with
cores until single-room cost dominates — at 10 humans/room there are always
enough rooms to spread. Caddy TLS shares the same cores (~included in all
numbers above). Linear-in-cores is therefore a reasonable first-order forecast,
with a safety discount for scheduler/lock overhead:

| Hardware (DO droplet) | Forecast feel-safe CCU | Notes |
|---|---|---|
| 1 vCPU / 2 GB (current) | **150** | measured |
| 2 vCPU / 4 GB | ~280–320 | ≈2× minus overhead; verify with one ramp after resize |
| 4 vCPU / 8 GB | ~550–650 | covers a 500-invite beta even if everyone shows up at once |
| 8 vCPU / 16 GB | ~1.1–1.3k | beyond beta scope; re-measure before trusting |

Rules of thumb per +1 vCPU: **+150 CCU**, +15 rooms, +14 Mbit/s egress.
RAM: ~0.1 MB/conn server-side — never the constraint at these scales.

For the **500-invite closed beta**: invited ≠ concurrent; realistic peak CCU is
~20–40 % of invites (100–200). A **2 vCPU / 4 GB** resize covers that with
headroom AND survives a 100 %-show-up burst gracefully. Resize is a reboot on
DO — do it before the invite wave, re-run the gate profile once after.

## Standing follow-ups

1. After ANY resize: one gate run + one 2-step ramp (target CCU and 1.5×) to
   re-validate the per-core model; append the numbers here.
2. The connect-storm CPU spike (100 % for ~3 s at mass TLS handshake) is
   load-generator behavior; real invites trickle. If a marketing push ever
   schedules a synchronized start, pre-warm or stagger.
3. If single-room cost ever grows (bigger maps, more entities), the
   rooms-parallelism assumption weakens — re-measure the knee, don't scale it.
