# Contributing

Issues and pull requests are welcome.

## Before you open a PR

The CI gate must pass locally — warnings are build failures in this crate:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --locked
```

## How the code is meant to grow

[`CLAUDE.md`](CLAUDE.md) is the architecture doctrine for this repo (it is written
for AI coding assistants, but it is the same rulebook for humans). The short version:

- The server is authoritative; gameplay code in `src/game` knows nothing about
  sockets, JSON, the DB, or logging.
- If a decision depends on what a player **saw**, name and use the player-visible
  timeline — never silently substitute the current server tick.
- Reliable events must never arrive after a snapshot that already shows their
  consequence.
- Gameplay changes come with scenario tests (`tests/policy_scenarios.rs` is the model).
- Wire-protocol changes need the golden fixtures updated and must stay in lockstep
  with the client.

## License of contributions

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall
be dual licensed as below, without any additional terms or conditions.
See [README → License](README.md#license).
