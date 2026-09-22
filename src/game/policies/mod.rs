//! Pure gameplay *policies* — the tunable rules (death, eat-claim, …) pulled out of the
//! imperative `Room` shell so each is one testable function instead of an `if` buried in
//! a tick loop.
//!
//! A policy takes an explicit context and returns a decision. It holds no state, does no
//! I/O, and never touches the world (no channels, no telemetry, no `&mut Room`). The
//! Room keeps the *world query* (who is closest, is the player shielded) and the *side
//! effects* (respawn, telemetry, broadcast); the policy owns only the decision. This is
//! the Functional Core / Imperative Shell split from Plan.md.

pub mod eat_claim;
pub mod enemy_contact;
pub mod enemy_death_candidate;
pub mod enemy_death_claim;
pub mod filler_eat;
