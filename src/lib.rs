// Library entry point. Re-exports every module so `cargo test` and the
// `generate_golden` bin can use them. The thin `main.rs` bin keeps the actual
// server entry point.

pub mod auth;
pub mod clock;
pub mod config;
pub mod config_shared;
pub mod db;
pub mod error;
pub mod game;
pub mod jwks_jwt;
pub mod metrics;
pub mod network;
pub mod nickname;
pub mod protocol;
pub mod telemetry;
