# fairtick — Fairness-Aware Authoritative Multiplayer Server

A production real-time multiplayer game backend in **Rust**: server-authoritative
simulation at **60 ticks/second**, fairness-aware netcode, client-side prediction
support, and a filler-bot population system — measured holding **100 concurrent
players on a single 1 vCPU / 2 GB box** at p99 tick latency **≤ 12 ms**.

Free and open source under **MIT or Apache-2.0** — use it for anything, commercial
included. If it saves you time, [support the work](#support).

**Write-up:** [The enemy on your screen is 133 milliseconds old](https://vitaliiermolaev.github.io/fairtick/)
explains the design, the numbers, and what is still unsolved.

It ships with a playable reference game — a top-down maze arena (move, collect,
eat, respawn, boost) with PvE enemies and server-driven filler bots. The interesting
part is not the game — it is the netcode: getting *visual fairness* right on real
networks.

---

## Why this exists

Most of a real-time multiplayer backend is plumbing. The hard part is **fairness**:
deciding a kill against what the player actually *saw* on screen, not what the
server's clock said a moment later. A client draws other entities from interpolated
snapshots, behind the server, while its own avatar is predicted ahead of it, so a
collision that is real on the server can be a visible miss on the phone. fairtick
makes that explicit: every decision that depends on what a player saw names the
timeline it uses, and the numbers behind it are measured and reproducible.

> **The core principle:** if a gameplay decision depends on what a player saw, the
> code explicitly names and uses the player-visible timeline — it never silently
> substitutes the current server tick. This is what prevents *"the server killed
> fairly by its own clock, but the player saw something completely different."*
> The full design doctrine is in [`CLAUDE.md`](CLAUDE.md).

---

## Proof it works

Measured on a **$12/month** droplet (1 vCPU / 2 GB) over the real internet through
a TLS edge, driven by 100 bots walking the full real-player handshake. Every run is
reproducible and gated by CI floors stricter than playability needs.

| Metric | Measured | Floor |
|---|---|---|
| Concurrent players | 100 | — |
| Tick p99 (peak) | 10–12 ms | ≤ 16.67 ms (never breached) |
| Egress per player | 12–13 KB/s | ≤ 30 KB/s |
| Host CPU (steady) | ~45 % · load 0.9 | — |
| RSS peak | 17 MB · no leak | ≤ 64 MB |
| Errors / unplanned disconnects | 0 / 0 | 0 |
| Snapshots / matches (soak) | 1.77 M / 1,399 | — |
| Capacity knee (1 vCPU) | ~170–180 CCU | cap 150 |

Full evidence trail: [`docs/perf-baseline.md`](docs/perf-baseline.md) (the
authoritative floors), [`docs/capacity-planning.md`](docs/capacity-planning.md)
(CCU ramp + hardware forecast), and the dated soak records under
[`docs/soak-runs/`](docs/soak-runs/) (v6 → v10).

---

## Quick start

Requires a recent stable Rust toolchain. The server reads `gameplay_config.toml`
and `maze.json` from the working directory, so run from the repo root.

```sh
# Local dev server — insecure local auth, binds 0.0.0.0:8090
./run_server.sh                       # override port: FAIRTICK_PORT=9000 ./run_server.sh

# Or plainly (debug):
cargo run

# Release build (exactly what the Docker image builds):
cargo build --release --locked --bin fairtick
./target/release/fairtick

# Tests + the CI gate (all three must pass):
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
```

`run_server.sh` sets `FAIRTICK_DEV=1` and `FAIRTICK_AUTH_MODE=insecure` so the
Editor / device builds authenticate locally. A production contour instead sets
`FAIRTICK_AUTH_MODE=apple` (see [Configuration](#configuration)). The server
**fails closed**: it refuses to boot with an unset auth mode or a dev backdoor
that isn't explicitly opted into.

Endpoints: `/ws` (game socket), `/healthz` · `/readyz` · `/pingz` (probes),
`/ops` (identity-free status dashboard, edge-auth), `/auth/*` and `/account/*`
(HTTP auth & account routes — sign-in is off the game socket).

---

## Architecture

Layered, with a hard boundary between deterministic gameplay and everything else.
Network code may call gameplay code; **gameplay code knows nothing about
transport, JSON, sockets, the DB, or logging.**

- **Functional core** (`src/game`) — each **Room** sits behind one lock and advances
  one tick per update; inputs and claims arrive through queues that only the tick
  drains, so room state changes one step at a time. The room emits domain
  **events** + **snapshot projections**. Pure movement (`sim.rs`) is mirrored
  byte-for-byte by the client so prediction matches. Gameplay randomness routes
  through a seeded per-room RNG (entity ids are still random UUIDs, so replays are
  not yet bit-exact). A test rejects any wall-clock read in `src/game/**`, and every
  policy entry point takes **typed timeline** newtypes (`ServerTick` / `RenderTick`)
  the compiler won't let you mix; the wire carries plain numbers, wrapped at the
  boundary.
- **Fairness policies** (`src/game/policies`) — small decision modules with no I/O
  (no sockets, DB, or logging), extracted from the room: eat-claim validation, enemy-contact death,
  the timeline-explicit enemy-death-candidate model, and the filler-eat stand-in.
  Claims are validated by **reconstructing what the client could plausibly see**
  at the relevant render tick, against a bounded, generation-aware contact history.
- **Imperative shell** (`src/network`, `RoomManager`) — an axum WebSocket server
  terminates the wire protocol, runs the handshake, translates inbound DTOs into
  ordered room calls, and pumps events + snapshots back. A generic
  **OutboundSequencer** structurally guarantees a hard event (death, respawn) can
  never arrive after a snapshot that already shows its consequence.
- **Contract layer** (`src/protocol`, `src/config_shared`) — wire DTOs (tagged
  JSON for control, **bincode binary** frames for snapshots since proto v6) and a
  **single-source-of-truth gameplay config** whose SHA-256 (`config_hash`) gates
  the client handshake, so client and server can never silently disagree.

---

## Repository map

### `src/game` — functional-core gameplay
| Module | Purpose |
|---|---|
| `room.rs` | The Room and the whole tick loop: drains commands, applies buffered inputs at their target tick, drives fillers, moves entities, records contact history, resolves eat/death claims chronologically, broadcasts snapshots, ends the match. |
| `room_manager.rs` | Imperative shell: spawns the tick task (all rooms per tick, panic evicts the room), matchmaking with under-lock capacity checks, resume-token registry + grace sweep, reward persistence via the DB outbox. |
| `sim.rs` | Pure movement step `(state, map, config) → next state`; mirrored byte-for-byte by the Unity client. |
| `timeline.rs` | Typed `ServerTick` / `RenderTick` / `UnixMs` newtypes so the compiler rejects mixing timelines. |
| `rng.rs` | Seeded per-room ChaCha8 RNG for all gameplay randomness. |
| `map.rs` | Maze grid, safe zones, portals, grid↔world conversion, spawn positions. |
| `player.rs` | Player entity + `ActorKind{Human,FillerBot}`: score, boost/invincibility/spawn-protection timers, life id, intent buffer, resume + claim-readiness flags. |
| `ai.rs` | The PvE enemy mob (distinct from filler bots): maze movement, nearest-alive targeting, respawn. |
| `filler_bot.rs` | Filler-bot **brain**: perception-limited utility goals, BFS pathfinding, reaction delay + deliberate mistakes, name pool. On the wire, indistinguishable from a human. |
| `snapshot.rs` | `SnapshotProjector`: pure, stateless projection of a room view into keyframe / delta DTOs. Holds no state, makes no gameplay decision. |
| `events.rs` | `DomainEvent` (game facts) and the single mapping that stamps `event_id` + `server_tick` and converts to the wire event. |
| `outbox.rs` | Per-player reliable lane + latest-only snapshot slot; reliable overflow forces a resync so no snapshot can follow an undelivered hard event. |
| `claim_ledger.rs` · `contact_history.rs` | Causal ledger of consumed lives + the bounded, generation-aware history sampler that backs all timeline reconstruction. |
| `policies/` | Pure fairness rules: `eat_claim`, `enemy_contact`, `enemy_death_candidate`, `enemy_death_claim`, `filler_eat`. |

### `src/network` — transport / imperative shell
| Module | Purpose |
|---|---|
| `websocket.rs` | axum router + WS upgrade, Hello/Welcome handshake, HTTP auth/account routes, inbound `ClientMessage` → `RoomManager` dispatch, join/resume wiring, per-connection DoS guard. |
| `outbound_sequencer.rs` | The one place the **reliable-before-snapshot** invariant lives; merges a reliable FIFO and a latest-only snapshot watch into one ordered stream. |
| `rate_limit.rs` | Per-IP token bucket for the HTTP auth/account routes, with a hard cap on tracked IPs. |
| `health.rs` | `/readyz` as a pure decision: 503 if the DB is unreachable or the tick heartbeat is stale. |
| `ops.rs` (+ `ops_dashboard.html`) | Identity-free `/ops` status snapshot (conns, human/filler split, per-room tick p95/p99). |

### Other top-level modules
| Module | Purpose |
|---|---|
| `main.rs` | Server entrypoint: fail-closed auth resolution, load config+maze, init DB, replay reward outbox, spawn sweepers, run the server. |
| `lib.rs` | Library crate re-exporting every module so bins and tests share one build. |
| `protocol.rs` | All wire DTOs (`ClientMessage` / `ServerMessage` / `ServerEvent` + snapshots); serde JSON + bincode; golden roundtrip/back-compat tests. |
| `config_shared.rs` · `gameplay_config.toml` | Shared gameplay config schema + the TOML data file whose raw bytes are the `config_hash`. |
| `config.rs` | Server-only runtime config (bind address + `DATABASE_URL`); holds no gameplay constants. |
| `auth.rs` · `jwks_jwt.rs` | Sign in with Apple (RS256 JWT verified against Apple's JWKS) + session minting; provider-agnostic verifier. |
| `db.rs` · `nickname.rs` | SQLite via sqlx: users / sessions / reward outbox, in-code append-only migrations, nickname rules. |
| `telemetry.rs` · `metrics.rs` | Per-run NDJSON telemetry writer + tick-perf metrics. |
| `clock.rs` · `error.rs` | Wall-clock helper (network boundary only) + typed `GameError` for honest HTTP status mapping. |

### Binaries (`src/bin`)
| Binary | What it does |
|---|---|
| `fairtick` | The game server (default-run). |
| `bot_runner` | Headless soak/load generator: N bots through the full handshake, 4 Hz moves + pings, match cycling, reconnect/resume churn. `--strict` makes it a GO/NO-GO gate; `--client-ready` walks the real readiness handshake. |
| `lag_proxy` | Deterministic local WS lag/jitter/loss proxy for netcode testing (`--profile`, `--seed`). |
| `generate_constants` | Codegen: renders the Unity client's `GameConstants.cs` from `gameplay_config.toml`. |
| `generate_golden` | Codegen: writes the cross-platform sim-parity + binary-snapshot golden fixtures under `tests/fixtures/`. |

---

## Testing

`cargo test --locked` runs three integration suites plus the in-module unit tests:

- **`tests/policy_scenarios.rs`** — replays 25 named given/when/then cases
  (unfair death, fair death, eat-claim accepted/rejected, stale claim) against the
  pure policies; asserts the decision *and* the exact reject-reason string. Claim
  tolerances load from the shipped config, not hardcoded literals.
- **`tests/sim_parity.rs`** — replays a golden trajectory fixture through the pure
  sim and asserts reproduction within 0.001 px. The *same fixture* is consumed by
  the Unity/iOS clients — this is the cross-platform determinism guarantee.
- **`tests/reward_outbox.rs`** — 7 async DB tests for the crash-safe, exactly-once
  match-end reward outbox (enqueue/drain, crash-replay, 8-way concurrent apply).
- **Protocol golden + backward-compat tests** (in `protocol.rs`) pin the binary
  wire format so a reorder can't silently break shipped clients.

CI (`.github/workflows/ci.yml`) gates every push/PR on `cargo fmt --check`,
`cargo clippy -D warnings`, and `cargo test --locked`. **Warnings are build
failures** at the crate level too (`[lints]` in `Cargo.toml`) — you fix them, you
don't suppress them.

---

## Configuration

Gameplay constants live once, in **`gameplay_config.toml`** (tick rate 60,
`max_players` 10, `protocol_version` 6, fairness tolerances, filler defaults). Its
SHA-256 is the `config_hash` the client must match at handshake — editing it forces
a client bundle re-sync. Server-only behavior is set by environment:

| Env var | Default | Meaning |
|---|---|---|
| `FAIRTICK_AUTH_MODE` | **required** | `apple` (prod) or `insecure` (dev). Missing/unknown = hard boot error. |
| `FAIRTICK_APPLE_AUDIENCE` | — | Required when `apple`: the iOS bundle id used as the JWT `aud`. |
| `FAIRTICK_DEV` | unset | Unlocks dev-only paths; required by `insecure` mode and by test tokens. |
| `FAIRTICK_ALLOW_TEST_TOKENS` | unset | Admits `test_token_*` (soak bots); fatal unless `FAIRTICK_DEV=1`. |
| `DATABASE_URL` | `sqlite:fairtick.db?mode=rwc` | SQLite path; **must** include `?mode=rwc` or a missing file fails startup (no silent empty DB). |
| `FAIRTICK_BIND_ADDR` | `0.0.0.0:8080` | TCP listen address. |
| `FAIRTICK_MAX_CONNECTIONS` | `500` | Global concurrent cap (recommend `150` on 1 vCPU); over it, `/ws` returns 503. |
| `FAIRTICK_MAX_CONNECTIONS_PER_IP` | `8` | Per-proxied-IP cap (`0` disables — set on the soak box where all bots share one IP). |
| `FAIRTICK_DISABLE_MATCHMAKING` | unset | Maintenance switch: refuse new joins, keep live matches + resumes. |
| `FAIRTICK_AUTH_RATE_BURST` / `_PER_SEC` | `40` / `8.0` | Per-IP token bucket for the HTTP auth/account routes. |
| `FAIRTICK_FILLER_*` | see TOML | Restart-only filler kill-switch (`ENABLED`, `TARGET_VISIBLE_PLAYERS`, …), applied **after** hashing so it never strands a client. |
| `FAIRTICK_TELEMETRY_VERBOSE` · `_LOG_DIR` · `_LOG_MAX_MB` · `_LOG_RETAIN` | off · `logs` · `64` · `12` | NDJSON telemetry writer. |
| `RUST_LOG` | `info` | Tracing verbosity. |

The dev deployment's exact values are the reference in
[`docker-compose.yml`](docker-compose.yml).

---

## Operations & deployment

- **`Dockerfile`** — two-stage release build; runtime image bakes in the binary +
  config + maze, with a `/readyz` HEALTHCHECK.
- **`docker-compose.yml`** — the dev-droplet stack: backend (exposes 8080 only) +
  Caddy (TLS 443, edge basic-auth for `/ops`).
- **`.github/workflows/deploy.yml`** + **`scripts/remote_deploy.sh`** — manual
  deploy that ships the image over SSH (no registry) and rolls forward behind a
  health gate with **automatic rollback** of image, compose, and Caddyfile.
- **Runbooks & tooling** — `docs/ops-pause-restore.md` (take the box off billing
  and restore in ~10 min), `scripts/backup_db.sh` + `restore_check.sh` +
  `ops.crontab` (hourly crash-consistent backups, weekly restore drill),
  `scripts/soak_combined.sh` + `soak_monitor.py` (the reproducible load harness).
- **Security posture** — `docs/security-audit-2026-06-11.md` records a 7-dimension
  adversarial audit: no gameplay-authority / anti-cheat hole survived (every
  eat/death-claim forgery attempt was refuted); findings were auth-hardening and
  edge rate-limiting, since fixed.

---

## The Unity client

The matching Unity client (client-side prediction, remote interpolation,
presentation) is a **separate project** that serves as the *reference client* —
it proves the protocol end-to-end and mirrors the server sim byte-for-byte
via the shared golden fixtures. It is functional but not a polished product. The
codegen (`generate_constants`) and sync scripts keep the client's constants and
config hashes in lockstep with this backend.

---

## Support

This is free to use, forever. If it helped you ship — or just taught you something
about netcode — you can chip in via the **Sponsor** button at the top of the repo.
Money is optional; bug reports, fixes, and a star help too.

Contributing: see [`CONTRIBUTING.md`](CONTRIBUTING.md). Security issues: see
[`SECURITY.md`](SECURITY.md) — please report them privately.

---

## License

Licensed under either of

- Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([`LICENSE-MIT`](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.

The license covers the code, not the *fairtick* name.
