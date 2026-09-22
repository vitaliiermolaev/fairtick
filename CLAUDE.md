# Real-Time Game Architecture Rules

This project is a real-time multiplayer game. Preserve architecture that favors
deterministic gameplay, explicit timelines, authoritative server simulation, and
thin transport/presentation layers.

The goal is not enterprise-style overengineering. Prefer small, explicit modules
and testable gameplay rules over large abstractions, hidden side effects, or
"manager of managers" classes.

> **The single most important rule:**
> If a gameplay decision depends on what a player saw, the code must explicitly
> name and use the player-visible timeline. Do not silently use the current
> server tick as a substitute for rendered client state. This is the principle
> that protects against "the server killed fairly by its own clock, but the
> player saw something completely different."

This repo is the **authoritative Rust backend**. The Unity client lives in a
separate project (by default a sibling `../unity` checkout). When a change touches the wire
protocol, both sides must be updated together (see Protocol rules).

---

## Before editing

Before making changes, identify which layer the change belongs to:

- gameplay core
- room actor / command processing
- protocol mapping
- transport
- snapshot projection
- Unity client state
- Unity prediction/interpolation
- Unity presentation
- telemetry/diagnostics
- persistence/auth

Do not put code in a layer just because it is convenient. If the change crosses
layers, keep the boundary explicit and explain why.

---

## Core principles

1. **Server gameplay is authoritative.**
   The backend owns canonical game state: positions, collisions, scoring,
   deaths, respawns, boosts, room lifecycle, and reward decisions. The client may
   predict, interpolate, and submit claims, but the server validates all gameplay
   outcomes.
2. **Separate simulation from transport.**
   Gameplay logic must not depend on WebSocket details, JSON serialization,
   channels, database calls, Unity presentation, or logging implementation.
   Network code may call gameplay code, but gameplay code should not know how
   messages are delivered.
3. **Prefer Functional Core / Imperative Shell.**
   Core gameplay should be as deterministic and testable as practical. Given
   state + command/tick, it should produce new state + domain events/effects.
   Side effects such as sending messages, writing logs, saving rewards, or
   publishing snapshots belong outside the core.
4. **Make room state changes single-threaded and ordered.**
   A game room should be mutated through one ordered command stream: join, leave,
   input, eat claim, tick, reconnect/resync. Avoid multiple tasks mutating room
   state directly. If concurrency is needed, use a Room Actor / command mailbox
   model.
5. **Use explicit domain events.**
   Important gameplay facts must be represented as domain events, for example:
   PlayerJoined, PlayerKilledByEnemy, PlayerRespawned, EnemyKilledByPlayer,
   SpeedChanged, RoomEnded. Do not hide important gameplay transitions as direct
   state mutations only.
6. **Separate domain events from wire protocol.**
   Domain events describe game facts. Protocol DTOs describe network format.
   Mapping between them should be explicit. Do not let wire protocol types become
   the internal gameplay model.
7. **Separate events from effects.**
   A domain event is something that happened in the game. An effect is something
   the outside world must do, such as SendReliable, BroadcastReliable,
   PublishSnapshot, PersistReward, DisconnectPlayer. Gameplay rules should produce
   facts/effects; outer layers execute effects.
8. **Snapshots are projections, not gameplay.**
   Snapshot generation should project current game state into a network-friendly
   representation. Snapshot code must not contain gameplay decisions such as
   killing, scoring, boosting, or respawning.
9. **Reliable gameplay events must be ordered before snapshots.**
   Hard/reliable events such as death, respawn, speed change, eat
   accepted/rejected, room ended must not be delivered after a snapshot that
   already includes their consequences. If reliable events and snapshots are both
   pending, send reliable events first.
10. **Timelines must be explicit.**
    Do not casually mix server_tick, client_tick, render_tick, unix_ms, mono_ms,
    snapshot tick, attacker tick, or target tick. If modifying time-sensitive
    code, name the timeline clearly and prefer typed wrappers/value objects where
    practical.
11. **Design for visual fairness.**
    Gameplay interactions that depend on what the player saw must account for
    interpolation/prediction delay. Do not validate visual interactions purely
    against the current server tick if the client was rendering an older
    timeline.
12. **Use reconstruction for client-visible interactions.**
    For mechanics like player eating enemies, claims should be validated by
    reconstructing what the client could plausibly see at the relevant
    render/timeline tick. Extend this principle to enemy/player fairness where
    appropriate.
13. **Keep prediction and presentation client-side.**
    Unity may predict local movement, interpolate remote entities, and smooth
    rendering. These systems must not become the source of gameplay truth.
    Client-side state is presentation/prediction, not authority.
14. **Use config as a single source of truth.**
    Gameplay constants such as tick rate, collision radii, kill radii, speed,
    boost duration, interpolation delay, snapshot interval, and protocol/config
    version must not be duplicated as stale magic constants. Prefer server-provided
    config or generated/shared config.
15. **Log gameplay decisions with enough context.**
    Logs for deaths, claims, respawns, speed changes, and reconciliation should
    include tick, relevant entity ids, distances, radii, event ids, generation
    ids, and timeline information. Logs should explain why a decision was made.
16. **Telemetry observes; it does not decide.**
    Telemetry/debug logging must not contain gameplay rules. Gameplay emits
    events/decisions; telemetry records them.
17. **Tests should target scenarios, not only functions.**
    Important real-time behavior should have scenario/replay tests: unfair death,
    fair death, eat claim accepted, eat claim rejected, respawn, boost
    start/expire, event ordering, reconnect/resync, room ending.
18. **Prefer small policies for tunable gameplay rules.**
    Collision, enemy contact, eat claim validation, scoring, boost, spawn, and
    respawn rules should live in small policy modules/functions when they become
    nontrivial. Avoid burying them in huge room/client classes.
19. **Keep God classes from growing.**
    Do not add unrelated responsibilities to Room or Unity GameClient if a small
    focused component would be clearer. Prefer extracting modules such as
    SnapshotProjector, EatClaimPolicy, EnemyContactPolicy, PlayerOutbox,
    ClientGameState, SnapshotApplier, ConnectionFlow, DeathDebugTelemetry.
20. **Avoid architecture theater.**
    Do not add interfaces, factories, dependency injection containers,
    inheritance hierarchies, generic event buses, CQRS, ECS rewrites, or
    repositories unless there is a concrete current need. Small plain
    modules/functions are preferred.

---

## Backend rules

1. Room/core gameplay code must not import network/websocket/database/logging
   implementation modules.
2. WebSocket handlers should translate inbound protocol messages into RoomCommand
   values.
3. Room processing should emit DomainEvent and RoomEffect values instead of
   directly performing external side effects where practical.
4. All room mutations should happen through the room's ordered execution path.
5. Room capacity checks must happen under the same lock/actor turn that adds the
   player.
6. Reliable events and snapshots must be multiplexed through an ordered
   outbox/sequencer.
7. Enemy/player death validation must be fairness-aware. If the player would
   visually see no contact on their presentation timeline, do not introduce
   immediate server-current-tick deaths without explicit grace/reconstruction
   logic.
8. Eat claim validation must check entity generation/respawn tick to prevent
   stale claims from killing newly respawned enemies.
9. Anti-cheat tolerances must be explicit, bounded, logged, and tested. Do not
   silently widen hit radii.
10. Reward/persistence logic must be triggered by room/game outcome events, not by
    scattered gameplay branches.

---

## Unity client rules

1. **GameClient should orchestrate, not own all logic.** New responsibilities
   should usually go into focused components.
2. Keep these concerns separate:
   - connection state machine
   - protocol parsing/dispatch
   - client game state store
   - snapshot application
   - local prediction
   - remote interpolation
   - eat claim creation/tracking
   - death/fairness debug telemetry
   - presentation/views/HUD
3. Unity presentation code may read client state and render it, but should not
   contain gameplay authority rules.
4. Client-side constants that mirror server gameplay must come from
   config/welcome/shared schema, not hardcoded values.
5. Client logs for suspicious deaths must include server distance, visual
   distance, kill radius, visual threshold, render tick, server tick,
   interpolation delay, RTT, jitter, killer id, and event id.
6. Prediction/reconciliation code should be isolated and tested. Avoid mixing
   reconciliation with protocol parsing or GameObject rendering.
7. Remote entity interpolation should operate on buffered snapshots and render
   tick, not directly on WebSocket receive time.
8. Client eat claims should clearly record attacker_tick, target_tick, visual
   distance, expected skew, actual skew, and claim id.

---

## Protocol rules

1. Protocol messages are DTOs, not domain models.
2. Every hard gameplay event should have a stable event_id/server_tick where
   applicable.
3. Snapshots should have snapshot_seq and snapshot_server_tick.
4. Include generation/respawn identifiers for reusable entities such as enemies.
5. Add protocol fields in a backward-compatible way where possible.
6. Update both backend and Unity protocol handling together.
7. Add or update golden fixtures/tests for important protocol messages.

---

## Time and fairness rules

1. Never compare positions from different timelines unless the conversion is
   explicit and documented.
2. Server-current distance and client-visible distance are different concepts.
3. A death/kill/eat decision should log which timeline was used.
4. Interpolation delay must be treated as gameplay-relevant for fairness
   decisions.
5. RTT/jitter may affect tolerance, but tolerance must be capped and logged.
6. Do not fix unfairness by only increasing radii or adding arbitrary magic
   numbers. Prefer explicit timeline reconstruction or delayed confirmation.

---

## Testing requirements for gameplay changes

When changing collision, movement, prediction, interpolation, snapshots, death,
respawn, boosts, claims, scoring, or room lifecycle:

1. Add or update scenario tests.
2. Include at least one edge case around tick boundaries.
3. Include at least one stale/out-of-order/replay case if protocol messages are
   involved.
4. Include logging assertions or snapshot/event assertions where practical.
5. Do not rely only on manual playtesting.

Required scenario classes over time:

- fair enemy kills player
- unfair enemy death candidate does not kill immediately
- player eats enemy claim accepted
- stale eat claim rejected after enemy respawn/generation change
- reliable event delivered before snapshot consequence
- boost start/expire affects speed and invincibility
- room ends exactly once
- player join capacity cannot race
- reconnect/resync receives coherent state

---

## Refactoring rules

1. Prefer small refactors that preserve behavior.
2. Do not mix large architectural refactors with gameplay tuning in the same
   change.
3. If extracting a module, first move code without changing behavior, then change
   behavior in a separate step.
4. Keep public behavior stable unless the task explicitly asks for gameplay
   change.
5. Preserve telemetry fields unless there is a clear replacement.
6. When removing logs, explain why they are no longer useful.
7. Avoid speculative abstractions. Extract only around actual complexity.

---

## God class prevention

Do not add new long-lived state or new behavior branches to Room or GameClient
unless it truly belongs there.

If adding logic related to:

- eat claims -> prefer EatClaimPolicy / EatClaimController
- enemy contact/death -> prefer EnemyContactPolicy / DeathDebugTelemetry
- snapshots -> prefer SnapshotProjector / SnapshotApplier
- connection phases -> prefer ConnectionFlow
- outbound ordering -> prefer PlayerOutbox / OutboundSequencer
- rendering GameObjects -> prefer Presentation/View components
- config values -> prefer RuntimeConfig / ConfigProvider

Room should coordinate gameplay state. GameClient should coordinate client
systems. Neither should become the place where all project complexity
accumulates.

---

## Definition of Done

A change is not done unless:

1. Gameplay authority remains on the server.
2. Timelines used by the change are explicit.
3. Reliable events cannot be reordered behind dependent snapshots.
4. New gameplay rules are covered by scenario/unit tests where practical.
5. Config values are not duplicated as stale constants.
6. Logs/debug output still explain important gameplay decisions.
7. Unity presentation remains separate from gameplay authority.
8. The change does not grow Room or GameClient with unrelated responsibilities.
9. Existing protocol compatibility is considered.
10. The simplest sufficient architecture was chosen.
