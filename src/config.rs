use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Server-only config: bind address, database URL, log level.
///
/// Gameplay constants (tick_rate, max_players, game_duration, speeds, etc.)
/// live in `gameplay_config.toml` and are loaded into `SharedAssets`.
/// They are NOT duplicated here — single source of truth is the shared config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_addr: SocketAddr,
    pub database_url: String,
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self::default())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_addr: "0.0.0.0:8080".parse().unwrap(),
            // Overridable via DATABASE_URL so the container can point at a persisted
            // volume. RUNBOOK: the URL must OPT INTO database creation with `?mode=rwc`
            // (e.g. sqlite:/app/data/fairtick.db?mode=rwc — what compose/Dockerfile ship).
            // Without it a missing file FAILS STARTUP on purpose: a mistyped path or an
            // unmounted volume must not silently boot a fresh empty database (init_db
            // deliberately sets no unconditional create_if_missing).
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:fairtick.db?mode=rwc".to_string()),
        }
    }
}
