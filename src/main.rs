use fairtick::config::Config;
use fairtick::config_shared::load_shared_assets;
use fairtick::jwks_jwt;
use fairtick::network::websocket::WebSocketServer;
use fairtick::{auth, db, game};
use std::path::Path;
use std::sync::Arc;
use tracing::info;

/// Resolve the identity-verification mode from env, FAIL CLOSED (audit items 1 + 3).
///   FAIRTICK_AUTH_MODE=apple     real JWKS verification (the only safe prod value)
///   FAIRTICK_APPLE_AUDIENCE=<iOS bundle id>
///   FAIRTICK_ALLOW_TEST_TOKENS=1 accept test_token_* — DEV ONLY; valid only alongside
///                                 FAIRTICK_DEV=1, else a fatal error (the dev backdoor
///                                 can never ride into a prod box).
///   FAIRTICK_AUTH_MODE=insecure  passthrough — must be EXPLICIT + FAIRTICK_DEV=1.
/// A MISSING FAIRTICK_AUTH_MODE is a hard boot error: no silent passthrough default.
fn resolve_auth_mode() -> Result<auth::AuthMode, Box<dyn std::error::Error>> {
    let is_dev = matches!(std::env::var("FAIRTICK_DEV").as_deref(), Ok("1") | Ok("true"));
    match std::env::var("FAIRTICK_AUTH_MODE").as_deref() {
        Ok("apple") => {
            let audience = std::env::var("FAIRTICK_APPLE_AUDIENCE").map_err(|_| {
                "FAIRTICK_AUTH_MODE=apple requires FAIRTICK_APPLE_AUDIENCE (the iOS bundle id)"
            })?;
            let allow_test_tokens =
                matches!(std::env::var("FAIRTICK_ALLOW_TEST_TOKENS").as_deref(), Ok("1") | Ok("true"));
            if allow_test_tokens && !is_dev {
                return Err("FAIRTICK_ALLOW_TEST_TOKENS=1 requires FAIRTICK_DEV=1 \
                    (the test-token backdoor is dev-only and must never run in prod)"
                    .into());
            }
            info!(
                "Auth mode: APPLE (audience={audience}, test_tokens={})",
                if allow_test_tokens { "ALLOWED (dev)" } else { "off" }
            );
            Ok(auth::AuthMode::Verify {
                verifiers: vec![jwks_jwt::JwksJwtVerifier::apple(audience)],
                allow_test_tokens,
            })
        }
        Ok("insecure") => {
            if !is_dev {
                return Err("FAIRTICK_AUTH_MODE=insecure requires FAIRTICK_DEV=1 \
                    (passthrough auth must never run in prod)"
                    .into());
            }
            tracing::warn!("⚠️ Auth mode: INSECURE passthrough — any token authenticates. DEV ONLY.");
            Ok(auth::AuthMode::InsecurePassthrough)
        }
        Ok(other) => Err(format!("unknown FAIRTICK_AUTH_MODE '{other}' (apple|insecure)").into()),
        Err(_) => Err("FAIRTICK_AUTH_MODE is required (apple for prod; \
            insecure needs FAIRTICK_DEV=1)"
            .into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // RUST_LOG-controllable verbosity, default info. The old hardcoded
    // `with_max_level(Level::INFO)` capped logs at compile time, so debug-level
    // diagnostics (e.g. per-drop rate-limit sheds) could never be raised on a
    // live box without a rebuild.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Build/release id, baked into the image at build time (Dockerfile ARG → ENV
    // FAIRTICK_BUILD_ID = the git sha). Logged first so every run's logs name the exact
    // build they came from — so a bad deploy is identifiable and the rollback target is
    // unambiguous. "dev" when run outside the image (local cargo run).
    let build_id = std::env::var("FAIRTICK_BUILD_ID").unwrap_or_else(|_| "dev".to_string());
    info!("Starting fairtick game server (build {build_id})");

    let config = Config::load()?;
    info!("Server config loaded");

    // Resolve the auth mode FIRST — before any side-effecting startup (db, telemetry,
    // the reward-drain task) — so a misconfigured deployment fails fast and clean
    // instead of tearing down half-spawned tasks. (audit items 1 + 3, fail closed)
    let auth_mode = resolve_auth_mode()?;

    let mut assets = load_shared_assets(Path::new("gameplay_config.toml"), Path::new("maze.json"))?;
    info!(
        "Shared gameplay assets loaded — config_hash={}.. maze_hash={}.. tick_rate={}",
        &assets.config_hash[..16],
        &assets.maze_hash[..16],
        assets.config.room.tick_rate
    );
    // Filler ops kill switch: env overrides applied AFTER hashing, so config_hash (and
    // therefore every shipped client) is untouched — [filler] is server-only. A bad
    // override value fails the boot on purpose: a typo'd kill switch must be loud.
    {
        let cfg = Arc::get_mut(&mut assets.config).expect("assets.config has no other owners at boot");
        let applied = cfg
            .filler
            .apply_env_overrides(|k| std::env::var(k).ok())
            .map_err(|e| format!("invalid filler env override: {e}"))?;
        for (k, v) in &applied {
            info!("⚙️ filler env override applied: {k}={v}");
        }
        // Cross-field sanity on the EFFECTIVE config (toml + overrides) — a parseable
        // but nonsensical combination must fail the boot loudly, not run weirdly.
        let max_players = cfg.room.max_players;
        cfg.filler.validate(max_players).map_err(|e| format!("invalid filler config: {e}"))?;
        info!(
            "Filler bots: enabled={} target_visible_players={} max_transient={}",
            cfg.filler.enabled, cfg.filler.target_visible_players, cfg.filler.max_transient_visible_players
        );
    }
    let assets = Arc::new(assets);

    let db_pool = db::init_db(&config.database_url).await?;
    info!("Database initialized");

    // Reward durability: replay any reward_outbox rows left `pending` by a previous
    // crash/redeploy BEFORE accepting players, then keep a low-rate drain worker running
    // so a transient DB outage that outlasts a match's immediate credit is recovered
    // without a restart. Both are idempotent (credit gated on status='pending').
    let replayed = db::drain_pending_rewards(&db_pool).await;
    if replayed > 0 {
        info!("Reward outbox: replayed {replayed} pending grant(s) on startup");
    }
    {
        let db_pool = db_pool.clone();
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(std::time::Duration::from_secs(30));
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                sweep.tick().await;
                db::drain_pending_rewards(&db_pool).await;
            }
        });
    }
    // Expired-session reaper (audit item 4): drop auth_sessions rows past their
    // expiry hourly so a long-running server doesn't accumulate dead tokens.
    {
        let db_pool = db_pool.clone();
        tokio::spawn(async move {
            let mut sweep = tokio::time::interval(std::time::Duration::from_secs(3600));
            sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                sweep.tick().await;
                match db::sweep_expired_sessions(&db_pool).await {
                    Ok(n) if n > 0 => info!("Session sweep: removed {n} expired session(s)"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("session sweep failed: {e}"),
                }
            }
        });
    }

    let telemetry = fairtick::telemetry::Telemetry::new().await?;

    let room_manager = Arc::new(game::room_manager::RoomManager::new(
        db_pool.clone(),
        assets.config.clone(),
        assets.maze.clone(),
        telemetry.clone(),
    ));
    let auth_service = Arc::new(auth::AuthService::new(db_pool.clone(), auth_mode));

    room_manager.start().await;

    let ws_server = WebSocketServer::new(
        config.clone(),
        db_pool.clone(),
        Arc::clone(&room_manager),
        Arc::clone(&auth_service),
        Arc::clone(&assets),
        telemetry,
    );

    info!("WebSocket server starting on port 8080");
    info!("Connect via: ws://localhost:8080/ws");

    ws_server.run().await?;

    Ok(())
}
