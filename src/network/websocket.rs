use crate::auth::AuthService;
use crate::clock::now_unix_ms;
use crate::config::Config;
use crate::config_shared::SharedAssets;
use crate::game::room_manager::{JoinedRoom, RoomManager};
use crate::network::health;
use crate::network::outbound_sequencer::{OutboundKind, OutboundSequencer};
use crate::protocol::{ClientLogBatch, ClientMessage, ServerMessage};
use crate::telemetry::Telemetry;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Request, State,
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch, RwLock};
use tracing::{error, info, warn};

/// Cap on a single INBOUND websocket message the server will assemble. Bounds memory against
/// a buggy/hostile peer streaming an unbounded frame (axum's default is 64 MiB). 1 MiB is far
/// above any legit client message — the biggest is a ClientLogBatch flush, itself bounded by
/// the client's ring buffer. The Unity client enforces its own (tighter) inbound cap too.
const MAX_WS_MESSAGE_BYTES: usize = 1024 * 1024;

/// Cap on an HTTP auth/account request body. These carry only small JSON (a session token
/// plus a nickname of at most 30 chars), so 4 KiB is far above any legit payload while
/// denying a memory-amplification body. axum's default would otherwise allow 2 MiB.
const MAX_AUTH_BODY_BYTES: usize = 4096;

/// A connection that opens but doesn't complete auth within this long is dropped
/// (security audit item 2): a silent socket can't be held open pre-auth to squat a
/// global-cap slot. 90s, NOT a tight value, ON PURPOSE: the client shows the native
/// Sign in with Apple sheet INSIDE this window (socket open → Welcome → sheet), and a
/// user can sit on that Face ID prompt for a while. The per-IP cap (audit item 2) is
/// what actually bounds a flood, so this only needs to be longer than a human deciding
/// on the Apple sheet, not short. (lead 2026-06-12 — 20s could close mid-sign-in.)
const PRE_AUTH_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// RAII counter for the global concurrent-connection cap. Acquired (incrementing) before a
/// socket upgrade and held for the connection's lifetime; Drop decrements — so the count is
/// correct even if the handler panics. A flood (or one tester hot-looping reconnects on a bad
/// network) can't exhaust the server: over-cap upgrades are refused with 503.
struct ConnGuard {
    counter: Arc<AtomicUsize>,
}

impl ConnGuard {
    fn try_acquire(counter: Arc<AtomicUsize>, max: usize) -> Option<Self> {
        // Optimistic add-then-check: if we overshot, back the increment out and refuse.
        let n = counter.fetch_add(1, Ordering::AcqRel) + 1;
        if n > max {
            counter.fetch_sub(1, Ordering::AcqRel);
            None
        } else {
            Some(ConnGuard { counter })
        }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Per-IP concurrent-connection map (security audit item 2). The global ConnGuard alone
/// lets a single IP open all `max_conns` sockets and starve everyone; this caps how many
/// one client IP may hold at once. The IP comes from the proxy's X-Forwarded-For /
/// X-Real-IP (only Caddy talks to the backend, so the header is trusted); a direct
/// connection with no forwarded header (local dev, no proxy) is NOT enforced.
type PerIpMap = Arc<Mutex<HashMap<String, usize>>>;

/// RAII per-IP slot. `None` ip / cap 0 = disabled (returns a no-op guard).
struct PerIpGuard {
    map: PerIpMap,
    ip: Option<String>,
}

impl PerIpGuard {
    fn try_acquire(map: PerIpMap, ip: Option<String>, max_per_ip: usize) -> Option<Self> {
        let Some(ip) = ip else {
            return Some(PerIpGuard { map, ip: None }); // no proxy header → not enforced
        };
        if max_per_ip == 0 {
            return Some(PerIpGuard { map, ip: None }); // disabled (e.g. the soak/dev box)
        }
        let mut m = map.lock().unwrap();
        let n = m.entry(ip.clone()).or_insert(0);
        if *n >= max_per_ip {
            return None;
        }
        *n += 1;
        drop(m);
        Some(PerIpGuard { map, ip: Some(ip) })
    }
}

impl Drop for PerIpGuard {
    fn drop(&mut self) {
        if let Some(ip) = &self.ip {
            let mut m = self.map.lock().unwrap();
            if let Some(n) = m.get_mut(ip) {
                *n -= 1;
                if *n == 0 {
                    m.remove(ip); // don't leak a row per IP ever seen
                }
            }
        }
    }
}

/// Client IP from the reverse-proxy headers (X-Forwarded-For's first hop, else
/// X-Real-IP). None when neither is present (direct/local — per-IP limiting is skipped).
/// Trusted because only Caddy on the compose network reaches the backend port.
fn client_ip_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let ip = first.trim();
            if !ip.is_empty() {
                return Some(ip.to_string());
            }
        }
    }
    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub struct WebSocketServer {
    _config: Config,
    db_pool: SqlitePool,
    room_manager: Arc<RoomManager>,
    auth_service: Arc<AuthService>,
    assets: Arc<SharedAssets>,
    telemetry: Telemetry,
}

impl WebSocketServer {
    pub fn new(
        config: Config,
        db_pool: SqlitePool,
        room_manager: Arc<RoomManager>,
        auth_service: Arc<AuthService>,
        assets: Arc<SharedAssets>,
        telemetry: Telemetry,
    ) -> Self {
        Self { _config: config, db_pool, room_manager, auth_service, assets, telemetry }
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error>> {
        // Edge backstops for the closed beta (override via env). max_conns bounds a flood /
        // self-DOS; matchmaking can be cut for maintenance without killing live matches.
        let max_conns = std::env::var("FAIRTICK_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(500);
        // Per-IP concurrent-connection cap (audit item 2). Default 8 PROTECTS prod from a
        // single-IP socket flood exhausting the global cap. Set to 0 to DISABLE on a box
        // that doubles as a load-test target (the dev droplet: all soak bots arrive from
        // ONE external IP through Caddy, which an 8-cap would block). Only enforced when a
        // proxy forwards the client IP — direct/local connections aren't limited.
        let max_conns_per_ip = std::env::var("FAIRTICK_MAX_CONNECTIONS_PER_IP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8);
        let matchmaking_enabled =
            !matches!(std::env::var("FAIRTICK_DISABLE_MATCHMAKING").as_deref(), Ok("1") | Ok("true"));
        if !matchmaking_enabled {
            warn!("FAIRTICK_DISABLE_MATCHMAKING set — JoinGame will be refused (maintenance)");
        }
        info!(
            "Edge limits: max_connections={max_conns}, max_per_ip={} matchmaking_enabled={matchmaking_enabled}",
            if max_conns_per_ip == 0 { "disabled".to_string() } else { max_conns_per_ip.to_string() }
        );

        // Per-IP rate limit for the HTTP auth/account routes (none existed; the WS caps
        // don't cover REST). Burst tolerates the live nickname-availability check while
        // typing; the sustained rate bounds /auth/apple (JWKS crypto) flooding + nickname
        // enumeration. Override via env on a load-test box.
        let auth_rate_burst =
            std::env::var("FAIRTICK_AUTH_RATE_BURST").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(40);
        let auth_rate_per_sec = std::env::var("FAIRTICK_AUTH_RATE_PER_SEC")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|&r| {
                // A non-positive sustained rate would mean "burst then NEVER refill" (a
                // permanent lockout once the burst drains), not "disabled" — reject it and
                // keep the default rather than silently brick the auth routes.
                let ok = r > 0.0;
                if !ok {
                    warn!(
                        "FAIRTICK_AUTH_RATE_PER_SEC={r} is non-positive (would never refill); using default"
                    );
                }
                ok
            })
            .unwrap_or(8.0);
        info!("HTTP auth/account rate limit: burst={auth_rate_burst}, {auth_rate_per_sec}/s per IP");

        let app_state = Arc::new(AppState {
            room_manager: self.room_manager,
            auth_service: self.auth_service,
            assets: self.assets,
            telemetry: self.telemetry,
            db_pool: self.db_pool,
            active_conns: Arc::new(AtomicUsize::new(0)),
            max_conns,
            per_ip: Arc::new(Mutex::new(HashMap::new())),
            max_conns_per_ip,
            matchmaking_enabled,
            started_at: std::time::Instant::now(),
            build_id: std::env::var("FAIRTICK_BUILD_ID").unwrap_or_else(|_| "dev".to_string()),
            auth_limiter: Arc::new(crate::network::rate_limit::IpRateLimiter::new(
                auth_rate_burst,
                auth_rate_per_sec,
            )),
            // Sign-out gets its OWN, more generous bucket so a burst of nickname-availability
            // checks that tripped the shared limiter (429) can't ALSO block the session
            // revoke — "sign out" should reach the server even right after fast typing. It's
            // cheap (one indexed DELETE) and idempotent, so a higher allowance is safe.
            signout_limiter: Arc::new(crate::network::rate_limit::IpRateLimiter::new(20, 4.0)),
        });

        // HTTP auth + account — sign-in / rename / availability / sign out, all OFF the
        // game websocket (lead 2026-06-12). Sign-in verifies the provider JWT and returns a
        // session token; account routes authenticate with that session. Grouped so they
        // share abuse controls the WS caps don't provide: a per-IP rate limit and a tight
        // request-body cap (these are tiny JSON payloads).
        // Sign-out on its OWN lenient limiter (see signout_limiter) so a nickname-check flood
        // can't starve the revoke. Separate router → separate route_layer.
        let signout_route = Router::new()
            .route("/account/signout", post(account_sign_out))
            .route_layer(middleware::from_fn_with_state(Arc::clone(&app_state), signout_rate_limit));

        let auth_routes = Router::new()
            .route("/auth/apple", post(auth_apple))
            .route("/auth/nickname", post(auth_set_nickname))
            .route("/account/nickname", post(account_change_nickname))
            .route("/account/nickname/check", post(account_nickname_check))
            .route_layer(middleware::from_fn_with_state(Arc::clone(&app_state), auth_rate_limit))
            .merge(signout_route)
            .layer(DefaultBodyLimit::max(MAX_AUTH_BODY_BYTES));

        let app = Router::new()
            .route("/ws", get(ws_handler))
            // Liveness/readiness probes (see `network::health`). GET-only, no
            // handshake — an orchestrator/LB polls these over plain HTTP.
            .route("/healthz", get(healthz))
            .route("/readyz", get(readyz))
            // Bare public liveness for external uptime checks (DO Uptime etc.):
            // same handler as /healthz, but Caddy exposes THIS path while /healthz
            // and /readyz stay edge-blocked (they carry topology).
            .route("/pingz", get(healthz))
            // Ops dashboard (see `network::ops`) — Caddy gates /ops/* behind
            // basic_auth at the edge; the backend itself does no auth here.
            .route("/ops", get(ops_dashboard))
            .route("/ops/", get(ops_dashboard))
            .route("/ops/statusz", get(ops_statusz))
            .merge(auth_routes)
            .with_state(app_state);

        // Bind address is overridable via FAIRTICK_BIND_ADDR (defaults to the
        // canonical 0.0.0.0:8080). Handy when 8080 is already taken locally.
        let addr = std::env::var("FAIRTICK_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let listener = tokio::net::TcpListener::bind(&addr).await?;

        info!("WebSocket server listening on {}", addr);
        info!("HTTP health endpoints: GET /healthz (liveness), GET /readyz (readiness)");

        axum::serve(listener, app).await?;

        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    room_manager: Arc<RoomManager>,
    auth_service: Arc<AuthService>,
    assets: Arc<SharedAssets>,
    telemetry: Telemetry,
    /// Held for the readiness probe's `SELECT 1` — the gameplay path goes
    /// through RoomManager/AuthService, not this pool directly.
    db_pool: SqlitePool,
    /// Live websocket connections, for the global concurrent-connection cap (see ConnGuard).
    active_conns: Arc<AtomicUsize>,
    /// Max concurrent connections; over it, upgrades are refused 503. Env
    /// `FAIRTICK_MAX_CONNECTIONS` (default 500) — a self-DOS backstop for the closed beta.
    max_conns: usize,
    /// Per-IP concurrent-connection counts (audit item 2 — single-IP flood protection).
    per_ip: PerIpMap,
    /// Max concurrent connections from ONE proxied client IP (0 = disabled). Env
    /// `FAIRTICK_MAX_CONNECTIONS_PER_IP` (default 8).
    max_conns_per_ip: usize,
    /// Emergency "disable matchmaking" switch (env `FAIRTICK_DISABLE_MATCHMAKING=1`): when
    /// false, JoinGame is refused (maintenance) while in-flight matches keep running and
    /// dropped players can still Resume — so the server drains instead of hard-cutting.
    matchmaking_enabled: bool,
    /// Process start, for the ops dashboard's uptime. Wall-clock-free (Instant) and
    /// transport-layer only — never a gameplay input.
    started_at: std::time::Instant,
    /// Build/release id (env `FAIRTICK_BUILD_ID`, baked by the deploy image; "dev" locally).
    build_id: String,
    /// Per-IP token bucket for the HTTP auth/account routes (the WS caps don't cover REST).
    auth_limiter: Arc<crate::network::rate_limit::IpRateLimiter>,
    /// Separate, more generous per-IP bucket for `/account/signout` so a nickname-check flood
    /// on `auth_limiter` can't block the session revoke.
    signout_limiter: Arc<crate::network::rate_limit::IpRateLimiter>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    // Refuse BEFORE upgrading if we're at the GLOBAL connection cap — a flood can't exhaust us.
    let Some(guard) = ConnGuard::try_acquire(Arc::clone(&state.active_conns), state.max_conns) else {
        warn!("ws connection refused: at capacity ({})", state.max_conns);
        return (StatusCode::SERVICE_UNAVAILABLE, "server at capacity").into_response();
    };
    // Then the PER-IP cap (audit item 2): one client IP can't squat all the global slots.
    let client_ip = client_ip_from_headers(&headers);
    let Some(ip_guard) =
        PerIpGuard::try_acquire(Arc::clone(&state.per_ip), client_ip.clone(), state.max_conns_per_ip)
    else {
        warn!("ws connection refused: per-IP cap ({}) for {:?}", state.max_conns_per_ip, client_ip);
        return (StatusCode::SERVICE_UNAVAILABLE, "too many connections from your address").into_response();
    };
    // Bound inbound frames/messages so a buggy or hostile peer can't balloon server memory.
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, guard, ip_guard))
}

// ---- HTTP auth (off the game websocket) ---------------------------------------------
//
// The sign-in screen calls these; the game socket is never involved in authentication.
// Both verify the provider identity token (Apple JWT + nonce, via AuthService) and, on
// success, mint a server SESSION TOKEN the client stores (Keychain) and later presents
// over the game socket with SignInWithSession. Stateless: a brand-new user re-sends the
// (still-valid) identity token + nonce to /auth/nickname with their chosen nickname.

#[derive(serde::Deserialize)]
struct AppleAuthRequest {
    identity_token: String,
    nonce: String,
}

#[derive(serde::Deserialize)]
struct SetNicknameRequest {
    identity_token: String,
    nonce: String,
    nickname: String,
}

#[derive(serde::Serialize)]
struct AuthResponse {
    /// Present when authenticated; the client stores it and uses SignInWithSession.
    session_token: Option<String>,
    user_id: Option<String>,
    nickname: Option<String>,
    crystals: Option<i64>,
    /// True = the identity verified but no account exists yet; collect a nickname and
    /// call /auth/nickname with the same identity_token + nonce.
    needs_nickname: bool,
}

/// Per-IP rate limit for the auth/account routes (AppState.auth_limiter). A request with no
/// proxied client IP (direct/local — not through Caddy) is allowed, matching the PerIpGuard
/// posture; in prod Caddy always sets X-Forwarded-For. Over budget → 429.
async fn auth_rate_limit(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(ip) = client_ip_from_headers(req.headers()) {
        if !state.auth_limiter.check(&ip) {
            warn!("auth/account route rate-limited for {ip}");
            return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
        }
    }
    next.run(req).await
}

/// Like `auth_rate_limit` but on the dedicated, more generous `signout_limiter` — so
/// `/account/signout` isn't starved by a burst of nickname checks on the shared bucket.
async fn signout_rate_limit(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(ip) = client_ip_from_headers(req.headers()) {
        if !state.signout_limiter.check(&ip) {
            warn!("signout route rate-limited for {ip}");
            return (StatusCode::TOO_MANY_REQUESTS, "too many requests").into_response();
        }
    }
    next.run(req).await
}

/// Build the authenticated response (session issued) for a known user. HTTP sign-in is
/// USELESS without a session token — the Authorization scene stores it and the game socket
/// presents it. So a session-issue failure is a hard 500, NOT a 200 with a null token: the
/// client must not load Home only to immediately bounce back to sign-in (a confusing loop).
async fn auth_ok(state: &Arc<AppState>, user: crate::db::User) -> Response {
    let session = match state.auth_service.issue_session(&user.id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("auth: session issue failed for {}: {e}", user.id);
            return (StatusCode::INTERNAL_SERVER_ERROR, "session issue failed").into_response();
        }
    };
    Json(AuthResponse {
        session_token: Some(session),
        user_id: Some(user.id),
        nickname: Some(user.nickname),
        crystals: Some(user.crystals),
        needs_nickname: false,
    })
    .into_response()
}

/// `POST /auth/apple` — verify an Apple identity token (+ nonce) off the game socket.
async fn auth_apple(State(state): State<Arc<AppState>>, Json(req): Json<AppleAuthRequest>) -> Response {
    match state.auth_service.sign_in_with_apple(&req.identity_token, &req.nonce).await {
        Ok(user) => auth_ok(&state, user).await,
        Err(crate::error::GameError::Auth(msg)) if msg == "Nickname required" => Json(AuthResponse {
            session_token: None,
            user_id: None,
            nickname: None,
            crystals: None,
            needs_nickname: true,
        })
        .into_response(),
        // A DB hiccup AFTER a valid token verified is OURS (500), not an auth rejection —
        // a 401 here would wrongly tell the client its sign-in was refused.
        Err(crate::error::GameError::Database(e)) => {
            tracing::error!("auth/apple DB error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
        // Verification failure (bad/forged/expired token, nonce mismatch) → 401, no detail.
        Err(_) => (StatusCode::UNAUTHORIZED, "authentication failed").into_response(),
    }
}

/// `POST /auth/nickname` — finish sign-up: re-verify the identity token + nonce, create
/// the account with the chosen nickname, issue a session.
async fn auth_set_nickname(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SetNicknameRequest>,
) -> Response {
    match state.auth_service.set_nickname(&req.identity_token, &req.nonce, &req.nickname).await {
        Ok(user) => auth_ok(&state, user).await,
        Err(e) => nickname_error(e),
    }
}

// ---- account self-service (session-authenticated; powers the Settings screen) --------
//
// These authenticate with the stored SESSION token (not a fresh identity token), so a
// player can rename or sign out without re-doing the native Face ID sheet.

#[derive(serde::Deserialize)]
struct SessionNicknameRequest {
    session_token: String,
    nickname: String,
}

#[derive(serde::Deserialize)]
struct SignOutRequest {
    session_token: String,
}

#[derive(serde::Serialize)]
struct NicknameCheckResponse {
    available: bool,
}

#[derive(serde::Serialize)]
struct NicknameChangeResponse {
    nickname: String,
}

/// Map a nickname set/change failure to an HONEST HTTP status, matching on the TYPED error
/// (not a brittle message substring): 409 already taken, 400 bad nickname (the reason rides
/// in the body), 401 invalid session / verification failure, 500 for a DB outage. The last
/// one matters — a transient DB error mapped to 401 would wrongly tell the client its
/// session expired and bounce it to sign-in. No internal detail leaks. Shared by sign-up
/// (/auth/nickname) and the Settings rename (/account/nickname).
fn nickname_error(e: crate::error::GameError) -> Response {
    use crate::error::GameError;
    match e {
        GameError::NicknameTaken => (StatusCode::CONFLICT, "nickname already taken").into_response(),
        GameError::NicknameInvalid(reason) => (StatusCode::BAD_REQUEST, reason).into_response(),
        GameError::InvalidSession => (StatusCode::UNAUTHORIZED, "session invalid").into_response(),
        GameError::Database(e) => {
            tracing::error!("account/nickname DB error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
        // Anything else (e.g. identity-token verification failure) — generic 401.
        _ => (StatusCode::UNAUTHORIZED, "authentication failed").into_response(),
    }
}

/// `POST /account/nickname` — rename the session's owner (Settings ▸ Account). The
/// authoritative length + uniqueness re-check lives in the service, so a stale
/// availability check loses here.
async fn account_change_nickname(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionNicknameRequest>,
) -> Response {
    match state.auth_service.change_nickname(&req.session_token, &req.nickname).await {
        Ok(user) => Json(NicknameChangeResponse { nickname: user.nickname }).into_response(),
        Err(e) => nickname_error(e),
    }
}

/// `POST /account/nickname/check` — is a nickname free for the session's owner? `available`
/// only ever means free-vs-taken; an INVALID nickname is a 400 (same as the rename), an
/// invalid session a 401, a DB hiccup a 500.
async fn account_nickname_check(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionNicknameRequest>,
) -> Response {
    match state.auth_service.nickname_available(&req.session_token, &req.nickname).await {
        Ok(available) => Json(NicknameCheckResponse { available }).into_response(),
        // Invalid nickname → 400 with the reason (the client shows "unsupported"/"1-30"),
        // NOT a false "taken" — the check and the rename agree on what's invalid.
        Err(crate::error::GameError::NicknameInvalid(reason)) => {
            (StatusCode::BAD_REQUEST, reason).into_response()
        }
        // Only an invalid session is the client's problem (401); a DB hiccup is ours (500),
        // not a reason to tell the client to re-authenticate.
        Err(crate::error::GameError::InvalidSession) => {
            (StatusCode::UNAUTHORIZED, "session invalid").into_response()
        }
        Err(e) => {
            tracing::error!("account/nickname check error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// `POST /account/signout` — invalidate the presented session (this device only). The
/// client also clears its local copy; either way the next launch returns to sign-in.
/// Idempotent: an unknown token still returns 200.
async fn account_sign_out(State(state): State<Arc<AppState>>, Json(req): Json<SignOutRequest>) -> Response {
    match state.auth_service.sign_out(&req.session_token).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "sign-out failed").into_response(),
    }
}

/// Liveness: 200 if the process is up and the HTTP server can answer. Checks
/// nothing else by design — see `network::health`.
async fn healthz() -> &'static str {
    health::LIVENESS_OK
}

/// Readiness: 200 if this instance should take players (DB reachable + tick loop
/// advancing), else 503 with a per-check JSON body. `network::health::evaluate`
/// owns the decision; this handler just gathers the observed inputs.
async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    // The DB probe is timeout-bounded; the heartbeat and room count are lock-free
    // reads — so this handler can never itself block on a wedged rooms lock / tick
    // loop, which is exactly when it must return 503 fast.
    let db = health::probe_db(&state.db_pool).await;
    // Log the detailed DB failure server-side, but don't leak the raw driver error
    // (schema/paths) in the public body — /readyz is reachable through Caddy.
    let db = db.map_err(|detail| {
        tracing::warn!("readyz: db probe failed: {detail}");
        "unavailable".to_string()
    });
    // allow-wall-clock: heartbeat age is a readiness diagnostic, not gameplay.
    let tick_age_ms = now_unix_ms().saturating_sub(state.room_manager.last_tick_unix_ms());
    let readiness = health::evaluate(
        db,
        tick_age_ms,
        state.room_manager.tick_rate(),
        state.room_manager.room_count(),
        state.active_conns.load(Ordering::Relaxed), // lock-free gauge ≈ players online
    );
    (readiness.status_code(), Json(readiness)).into_response()
}

/// `/ops/statusz`: the live-status JSON for the ops dashboard (see `network::ops`).
/// Caddy gates `/ops/*` behind basic_auth at the edge.
async fn ops_statusz(State(state): State<Arc<AppState>>) -> Response {
    let rooms = state.room_manager.rooms_overview().await;
    let resume_holds = state.room_manager.resume_sessions_count().await;
    // allow-wall-clock: ops diagnostics, not gameplay.
    let heartbeat_age_ms = now_unix_ms().saturating_sub(state.room_manager.last_tick_unix_ms());
    let status = crate::network::ops::build_status(crate::network::ops::StatusInputs {
        build: state.build_id.clone(),
        uptime_sec: state.started_at.elapsed().as_secs(),
        active_conns: state.active_conns.load(Ordering::Relaxed),
        max_conns: state.max_conns,
        rooms,
        resume_holds,
        perf: state.room_manager.tick_perf(),
        heartbeat_age_ms,
        matchmaking_enabled: state.matchmaking_enabled,
    });
    Json(status).into_response()
}

/// `/ops`: the dashboard page itself — static HTML polling `/ops/statusz`.
async fn ops_dashboard() -> Response {
    ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], crate::network::ops::DASHBOARD_HTML)
        .into_response()
}

async fn handle_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    _conn_guard: ConnGuard,
    _ip_guard: PerIpGuard,
) {
    // `_conn_guard` / `_ip_guard` are held for the whole connection; their Drop decrements the
    // global + per-IP counts when this task ends (or panics), keeping both accurate.
    let (ws_sender, mut receiver) = socket.split();
    // TWO outbound lanes to the socket, never sharing a FIFO:
    //   out_tx  : control + reliable events — bounded ordered mpsc.
    //   snap_tx : movement snapshots — LATEST-ONLY watch. A backlog of snapshots
    //             can never form (a fresher one overwrites the slot), so a Wi-Fi
    //             stall can't push Pong/RTT behind a queue of stale frames.
    let (out_tx, out_rx) = mpsc::channel::<Message>(64);
    let (snap_tx, snap_rx) = watch::channel::<Option<Message>>(None);

    // The socket writer is the LOWER mux. bug #33: RELIABLE (events/control) must reach the socket
    // BEFORE the snapshot that already reflects their consequence. It is the SAME tested
    // OutboundSequencer the room→forwarder hop uses (here over `Message`, with control sharing the
    // reliable lane) — so reliable-before-snapshot is one proven component on BOTH hops, not an
    // inline select re-deriving the bias. (lead review — prove the lower socket mux.)
    let sender_task = tokio::spawn(async move {
        let mut ws_sender = ws_sender;
        let mut mux = OutboundSequencer::new(out_rx, snap_rx);
        while let Some(item) = mux.next().await {
            if ws_sender.send(item.message).await.is_err() {
                break;
            }
        }
    });

    let client_state = Arc::new(RwLock::new(ClientState::new()));
    // Per-connection transport DoS guard. Local to this single-threaded read loop (no lock): every
    // inbound message for this socket is handled here in sequence. Enforces the move-command rate
    // (previously dead config) and caps claim-class messages. (round-3 follow-up)
    let mut rate_limiter = GameplayRateLimiter::new(
        state.assets.config.network.move_command_rate_limit_per_sec,
        state.assets.config.network.ping_interval_ms,
        now_unix_ms(),
    );

    info!("New WebSocket connection established");

    // Why this connection's read loop ended — emitted in connection_closed below so a dead
    // reconnect (the 2026-06-10 hello-then-silence) names its cause instead of leaving the
    // NDJSON to infer it from absence. Overwritten at each break site.
    let mut close_reason = "receiver_stream_ended";
    // Lifecycle forensics (engineer wave 7 #9): how long the connection lived, how long
    // after Hello it died, and what the LAST inbound message was — all local to this
    // single-task read loop, no locks.
    let opened_at = std::time::Instant::now();
    let mut hello_at: Option<std::time::Instant> = None;
    let mut last_inbound_kind: &'static str = "none";
    // Pre-auth idle deadline (audit item 2): until the connection authenticates, every
    // inbound wait is bounded — a socket opened and held silent (or stuck mid-handshake)
    // is dropped instead of squatting a global/per-IP slot. Cleared once authenticated
    // (a real game connection then lives indefinitely on its own pings).
    let mut authed = false;

    loop {
        let msg = if authed {
            match receiver.next().await {
                Some(m) => m,
                None => break,
            }
        } else {
            let remaining = PRE_AUTH_IDLE_TIMEOUT.saturating_sub(opened_at.elapsed());
            match tokio::time::timeout(remaining, receiver.next()).await {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(_) => {
                    close_reason = "pre_auth_idle_timeout";
                    warn!("ws dropping unauthenticated idle connection after {PRE_AUTH_IDLE_TIMEOUT:?}");
                    break;
                }
            }
        };
        match msg {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientMessage>(&text) {
                Ok(client_msg) => {
                    last_inbound_kind = client_msg_kind(&client_msg);
                    if hello_at.is_none() && matches!(client_msg, ClientMessage::Hello { .. }) {
                        hello_at = Some(std::time::Instant::now());
                    }
                    let response = process_message(
                        client_msg,
                        Arc::clone(&client_state),
                        Arc::clone(&state),
                        out_tx.clone(),
                        snap_tx.clone(),
                        &mut rate_limiter,
                    )
                    .await;
                    if let Some(resp) = response {
                        if !send_reply(&out_tx, &resp, "text-response") {
                            close_reason = "reply_send_failed";
                            break;
                        }
                    }
                    // A handler (wire_joined) may have lost a control send it couldn't
                    // return through process_message — it flags the connection instead.
                    if client_state.read().await.terminate {
                        close_reason = "terminated_by_server";
                        break;
                    }
                }
                Err(e) => {
                    error!("Failed to parse message: {}", e);
                    let error_msg =
                        ServerMessage::Error { message: format!("Invalid message format: {}", e) };
                    if !send_reply(&out_tx, &error_msg, "text-parse-error") {
                        close_reason = "reply_send_failed";
                        break;
                    }
                }
            },
            Ok(Message::Binary(data)) => match ClientMessage::deserialize(&data) {
                Ok(client_msg) => {
                    last_inbound_kind = client_msg_kind(&client_msg);
                    let response = process_message(
                        client_msg,
                        Arc::clone(&client_state),
                        Arc::clone(&state),
                        out_tx.clone(),
                        snap_tx.clone(),
                        &mut rate_limiter,
                    )
                    .await;
                    if let Some(resp) = response {
                        if !send_reply(&out_tx, &resp, "binary-response") {
                            close_reason = "reply_send_failed";
                            break;
                        }
                    }
                    if client_state.read().await.terminate {
                        close_reason = "terminated_by_server";
                        break;
                    }
                }
                Err(e) => {
                    error!("Failed to parse binary message: {:?}", e);
                }
            },
            Ok(Message::Close(_)) => {
                info!("WebSocket connection closed by client");
                close_reason = "client_close_frame";
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                close_reason = "socket_error";
                break;
            }
            _ => {}
        }
        // Lift the pre-auth idle deadline once this connection authenticates (a real
        // game connection then lives indefinitely on its own ping cadence). One cheap
        // read-lock per pre-auth message only; never after auth. (audit item 2)
        if !authed && client_state.read().await.user_id.is_some() {
            authed = true;
        }
    }

    // Socket closed. If the player is still in a room here, this is an UNEXPECTED drop
    // (an explicit LeaveGame/ReturnToMenu already cleared current_room, so it's skipped).
    // Hold the slot for `reconnect_grace_sec` so a Resume can return them to the match;
    // the grace sweep frees it if they don't come back. Falls back to an immediate leave
    // if we somehow lack the identity needed to record a resume session.
    let state_lock = client_state.read().await;
    // connection_closed pairs with hello_received: how far did this connection get before it
    // died? The 2026-06-10 log had two hello-then-silence connections with NOTHING to say
    // whether auth never arrived, the socket died, or the server closed. (lead manifesto)
    let phase_known_by_server =
        match (state_lock.welcomed, state_lock.user_id.is_some(), state_lock.current_room.is_some()) {
            (_, _, true) => "in_room",
            (_, true, false) => "authenticated",
            (true, false, _) => "welcomed",
            (false, _, _) => "pre_hello",
        };
    state.telemetry.server_info(
        "connection_closed",
        serde_json::json!({
            "conn_id": state_lock.conn_id,
            "close_reason": close_reason,
            "phase_known_by_server": phase_known_by_server,
            "had_hello": state_lock.welcomed,
            "had_welcome_sent": state_lock.welcomed, // welcomed flips exactly when Welcome is enqueued
            "had_auth": state_lock.user_id.is_some(),
            "user_id": state_lock.user_id,
            "room_id": state_lock.current_room,
            "player_id": state_lock.player_id,
            // Lifecycle forensics (engineer wave 7 #9).
            "elapsed_ms_since_open": opened_at.elapsed().as_millis() as u64,
            "elapsed_ms_since_hello": hello_at.map(|t| t.elapsed().as_millis() as u64),
            "last_inbound_message": last_inbound_kind,
            "client_connect_attempt_id": state_lock.client_connect_attempt_id,
            "client_session_id": state_lock.client_session_id,
            "client_resume_intent": state_lock.client_resume_intent,
        }),
    );
    if let (Some(room_id), Some(player_id)) = (&state_lock.current_room, &state_lock.player_id) {
        match (&state_lock.user_id, &state_lock.resume_token) {
            (Some(user_id), Some(resume_token)) => {
                state.room_manager.disconnect_hold(room_id, player_id, user_id, resume_token).await;
            }
            _ => {
                state.room_manager.leave_room(room_id, player_id).await;
            }
        }
    }
    drop(state_lock);

    drop(out_tx);
    drop(snap_tx);
    let _ = sender_task.await;
}

type WsSender = mpsc::Sender<Message>;

/// What to do when a reply can't be enqueued (out_tx full/closed). NAMED policy per
/// message kind, not one blanket rule (lead review):
///   * `BestEffortDrop` — Pong ONLY. It is re-requested every ping interval and TimeSync
///     just skips the missing sample; killing the connection over a lost Pong would turn
///     ordinary slow-client back-pressure into reconnect churn.
///   * `FatalClose` — everything else. Welcome / auth + nickname results / GameJoined /
///     GameLeft / ResumeRejected / resync keyframes / errors are awaited by the client's
///     connection state machine and never retransmitted; the client has no handshake-phase
///     timeout, so a silent loss wedges it forever on a live socket. Closing (same policy
///     as the reliable forwarder lane) kicks it onto the reconnect/resume path instead.
enum ReplyDropPolicy {
    FatalClose,
    BestEffortDrop,
}

fn reply_drop_policy(msg: &ServerMessage) -> ReplyDropPolicy {
    match msg {
        ServerMessage::Pong { .. } => ReplyDropPolicy::BestEffortDrop,
        _ => ReplyDropPolicy::FatalClose,
    }
}

/// Stable variant name for reply diagnostics — a "control reply dropped" log line must say
/// WHICH message was lost (Pong vs Welcome vs ResumeRejected reads very differently in an
/// incident), not just which call site enqueued it.
fn server_msg_kind(msg: &ServerMessage) -> &'static str {
    match msg {
        ServerMessage::Welcome { .. } => "Welcome",
        ServerMessage::AuthSuccess { .. } => "AuthSuccess",
        ServerMessage::AuthFailed { .. } => "AuthFailed",
        ServerMessage::NicknameSet { .. } => "NicknameSet",
        ServerMessage::NicknameUnavailable => "NicknameUnavailable",
        ServerMessage::GameJoined { .. } => "GameJoined",
        ServerMessage::GameLeft => "GameLeft",
        ServerMessage::GameStarting { .. } => "GameStarting",
        ServerMessage::GameState(_) => "GameState",
        ServerMessage::GameStateDelta(_) => "GameStateDelta",
        ServerMessage::PlayerJoined { .. } => "PlayerJoined",
        ServerMessage::PlayerLeft { .. } => "PlayerLeft",
        ServerMessage::Event(ev) => ev.kind(),
        ServerMessage::EatClaimRejected { .. } => "EatClaimRejected",
        ServerMessage::EnemyDeathClaimRejected { .. } => "EnemyDeathClaimRejected",
        ServerMessage::Pong { .. } => "Pong",
        ServerMessage::GameEnded { .. } => "GameEnded",
        ServerMessage::Error { .. } => "Error",
        ServerMessage::ResumeRejected { .. } => "ResumeRejected",
    }
}

/// Wire encoding per message kind (protocol v6). SNAPSHOTS (GameState / GameStateDelta) go as
/// bincode BINARY frames: at ~30/s per client they dominated egress as JSON (field names +
/// 36-char uuid ids ≈ 3.2KB per delta → measured 78 Mbit/s at 100 conns). Everything else —
/// handshake, control, reliable events — stays human-readable JSON Text: tiny volume, and
/// debuggability/golden-fixture diffing is worth more there. ENCODING ONLY: same values
/// (f32 bit-exact), same 30/s cadence, same ordering through the same lanes — the client's
/// prediction/interpolation feel is untouched by this change (cross-checked by the
/// snapshot_frames golden fixture decoded by BOTH codec paths on the Unity side).
fn encode_server_msg(msg: &ServerMessage) -> Option<Message> {
    match msg {
        ServerMessage::GameState(_) | ServerMessage::GameStateDelta(_) => {
            msg.serialize().ok().map(Message::Binary)
        }
        _ => serde_json::to_string(msg).ok().map(Message::Text),
    }
}

/// Enqueue a reply on the bounded control FIFO. Returns `false` ONLY when a FATAL reply was
/// lost — the caller must then close the connection. A best-effort drop (Pong) returns `true`
/// and logs at debug. The fatal log keeps the literal "control reply dropped" (the soak
/// monitor's `ctrl` needle) and names the lost variant.
#[must_use]
fn send_reply(out_tx: &WsSender, resp: &ServerMessage, ctx: &str) -> bool {
    let Some(frame) = encode_server_msg(resp) else {
        // Our own wire types always serialize; but if that ever breaks, the reply is just
        // as LOST as on a full FIFO — so it follows the same per-variant policy. A
        // critical reply that can't serialize must still close the connection (the client
        // would wedge waiting for it); only a Pong is survivable.
        tracing::error!("reply serialize failed ({ctx}): type={}", server_msg_kind(resp));
        return matches!(reply_drop_policy(resp), ReplyDropPolicy::BestEffortDrop);
    };
    match out_tx.try_send(frame) {
        Ok(()) => true,
        // CLOSED ≠ FULL: the sender task is already gone (socket teardown raced this
        // reply) — nothing is degraded and nobody is reachable. Quietly tell the caller
        // to stop; warning here would feed the soak monitor's 'ctrl' needle with
        // ordinary disconnect races.
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::debug!("reply not sent ({ctx}): connection gone, type={}", server_msg_kind(resp));
            false
        }
        // FULL on a live socket — real back-pressure; per-variant policy applies.
        Err(mpsc::error::TrySendError::Full(_)) => match reply_drop_policy(resp) {
            ReplyDropPolicy::BestEffortDrop => {
                tracing::debug!(
                    "reply dropped (best-effort, {ctx}): type={} out_tx full",
                    server_msg_kind(resp)
                );
                true
            }
            ReplyDropPolicy::FatalClose => {
                tracing::warn!(
                    "control reply dropped ({ctx}): type={} out_tx full — closing connection",
                    server_msg_kind(resp)
                );
                false
            }
        },
    }
}

/// Transport-layer token bucket (NOT gameplay): wall-clock based, refills continuously. Sheds
/// excess inbound messages per connection before they reach the room actor.
struct TokenBucket {
    tokens: f64,
    capacity: f64,
    refill_per_ms: f64,
    last_ms: f64,
    /// Whether this connection has already had a drop logged at INFO (see `allow_logged`).
    shed_logged: bool,
}

impl TokenBucket {
    /// `rate_per_sec` is the SUSTAINED ceiling (continuous refill); `burst` is the bucket capacity
    /// (momentary allowance). A malicious flood is bounded by the sustained rate regardless of burst,
    /// so a generous burst only buys honest spikes headroom — it never weakens the DoS guard.
    fn new(rate_per_sec: f64, burst: f64, now_ms: f64) -> Self {
        Self {
            tokens: burst,
            capacity: burst,
            refill_per_ms: rate_per_sec / 1000.0,
            last_ms: now_ms,
            shed_logged: false,
        }
    }
    /// Refill by elapsed wall time, then consume one token. `true` = allowed. A non-positive
    /// capacity means UNLIMITED (always allow), so a config rate of 0 disables the limit rather than
    /// blocking everything. Monotonic-clock skew is guarded (`elapsed` floored at 0).
    fn allow(&mut self, now_ms: f64) -> bool {
        if self.capacity <= 0.0 {
            return true;
        }
        let elapsed = (now_ms - self.last_ms).max(0.0);
        self.last_ms = now_ms;
        self.tokens = (self.tokens + elapsed * self.refill_per_ms).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
    /// `allow`, plus VISIBLE shedding: the FIRST drop per connection per class is logged at
    /// INFO — the default production log level, and what the soak monitor greps for
    /// ("rate-limited") — so back-pressure can't happen invisibly. Subsequent drops log at
    /// DEBUG so a sustained flood can't turn the log into its own DoS.
    fn allow_logged(&mut self, now_ms: f64, what: &str) -> bool {
        if self.allow(now_ms) {
            return true;
        }
        if self.shed_logged {
            tracing::debug!("rate-limited {what}");
        } else {
            self.shed_logged = true;
            tracing::info!("rate-limited {what} — first drop for this connection (further drops at debug)");
        }
        false
    }
}

/// Per-connection rate for player-initiated EAT claims (eat several enemies in quick succession is
/// honest, so this is the most generous claim budget). Fixed (transport DoS guard, not config). (#)
const EAT_CLAIM_RATE_LIMIT_PER_SEC: f64 = 10.0;
/// EnemyDeathClaim is the gameplay-CRITICAL precise death path AND an honest client holds only ONE
/// in flight at a time, so it gets its OWN small bucket — never shared with eat/probe, so an eat or
/// diagnostic burst can't shed a death claim (which would force the blunt sustained-contact fallback
/// on a different timeline). The room's per-player pending cap + monotonic id are the real bound;
/// this only sheds a gross flood. (round-3 follow-up)
const DEATH_CLAIM_RATE_LIMIT_PER_SEC: f64 = 5.0;
/// VisualOverlapProbe is observe-only diagnostics — its own tiny bucket so it can never eat a
/// gameplay budget. (round-3 follow-up)
const PROBE_RATE_LIMIT_PER_SEC: f64 = 5.0;
/// RequestFullState (resync) builds a full keyframe under the room READ lock, contending with the
/// per-tick write lock — so an unbounded resync spam can stall the room. An honest client resyncs
/// only on a stall/reconnect (rare), so a tiny bucket is plenty. (review #3)
const RESYNC_RATE_LIMIT_PER_SEC: f64 = 3.0;
/// Ping handling is cheap but takes the room READ lock to stamp server_tick (so a flood contends
/// the per-tick writer). The honest cadence is `config.network.ping_interval_ms` (the SAME config
/// value the generated Unity GameConstants drives the client with — single source of truth, so a
/// client cadence tune can't silently outrun the server limit); the sustained ceiling is 2.5× the
/// honest rate (e.g. 250ms → 4/sec honest → 10/sec), with a further 2× burst for reconnect
/// spikes. (beta hardening, review follow-up)
const PING_RATE_HEADROOM: f64 = 2.5;
/// ClientLogBatch is already bounded per-message (≤200 entries, oversized fields elided), but
/// nothing capped its FREQUENCY — a client could flush in a tight loop and pin the telemetry
/// writer + disk. An honest client flushes ~every 2s plus event bursts; 5/sec sustained is
/// generous. (beta hardening)
const CLIENT_LOG_BATCH_RATE_LIMIT_PER_SEC: f64 = 5.0;
/// Burst capacity for ClientLogBatch — sized to the client's LEGITIMATE worst-case drain, not to
/// the steady rate: at match end / app pause, the Unity client logger's FlushAllAsync drains its
/// whole ring buffer back-to-back as up to MaxBuffered(1000)/MaxBatch(50) = 20 awaited batches
/// BEFORE the graceful close — exactly the end-of-match death/claim telemetry the fairness
/// pipeline correlates. 20 + headroom = 24; a flood beyond the burst is still capped by the
/// sustained 5/sec refill. (review: don't shed the deliberate end-of-match flush)
const CLIENT_LOG_BATCH_BURST: f64 = 24.0;

/// Auth-message budget on the GAME SOCKET. The current client does Apple sign-in over HTTP, so
/// a legit game socket sends at most a couple auth messages (a SignInWithApple that needs a
/// nickname, then one SetNickname). But the legacy WS auth path is still accepted, and each
/// SignInWithApple runs JWT verify (and can trigger a JWKS refresh) — so a hostile socket could
/// spam it within the 90s pre-auth window with no per-message cap. A small bucket bounds the
/// cost: burst covers honest retries, the slow refill caps a flood. (review: WS auth surface)
const AUTH_MSG_RATE_LIMIT_PER_SEC: f64 = 0.5;
const AUTH_MSG_BURST: f64 = 6.0;

/// Per-connection inbound DoS guard (transport, NOT gameplay — it sheds messages, it decides no
/// gameplay outcome). One bucket PER message class so a high-rate class can never starve a low-rate
/// one — in particular the gameplay-critical EnemyDeathClaim is isolated from moves, eat claims, and
/// diagnostic probes:
///   * `moves`        — enforces `config.network.move_command_rate_limit_per_sec` (was dead config).
///   * `eat_claims`   — EatClaim.
///   * `death_claims` — EnemyDeathClaim (its own small budget; see the const).
///   * `probes`       — VisualOverlapProbe (diagnostics).
///   * `resyncs`      — RequestFullState (builds a keyframe under the room lock; review #3).
///   * `pings`        — Ping (takes the room read lock to stamp server_tick; beta hardening).
///   * `log_batches`  — ClientLogBatch (telemetry write + disk; beta hardening).
///
/// Over-budget messages are dropped before reaching the room actor (moves supersede/retransmit;
/// claims time out → resync). An honest client never approaches these rates. (round-3 follow-up)
pub(crate) struct GameplayRateLimiter {
    moves: TokenBucket,
    eat_claims: TokenBucket,
    death_claims: TokenBucket,
    probes: TokenBucket,
    resyncs: TokenBucket,
    pings: TokenBucket,
    log_batches: TokenBucket,
    /// Legacy WS auth messages (SignInWithApple / SetNickname). See AUTH_MSG_* — the game
    /// socket isn't the auth path anymore, so this is deliberately tight.
    auth: TokenBucket,
}

impl GameplayRateLimiter {
    fn new(move_rate_per_sec: u32, ping_interval_ms: u64, now_ms: u64) -> Self {
        let now = now_ms as f64;
        let move_rate = move_rate_per_sec as f64;
        // Honest ping rate from the shared config cadence (250ms → 4/sec), with headroom.
        // ping_interval_ms is validated > 0 at config load.
        let ping_rate = 1000.0 / ping_interval_ms as f64 * PING_RATE_HEADROOM;
        Self {
            // Moves get a 2× burst: the client resends every unacked input (≤1 per 80ms each, up to
            // a 64-deep pending buffer), so a post-stall recovery can briefly spike well above the
            // sustained rate. The burst absorbs that honest spike; the sustained refill still caps a
            // real flood. Claims never spike (≤ a few/sec honestly), so a 1s burst is plenty.
            moves: TokenBucket::new(move_rate, 2.0 * move_rate, now),
            eat_claims: TokenBucket::new(EAT_CLAIM_RATE_LIMIT_PER_SEC, EAT_CLAIM_RATE_LIMIT_PER_SEC, now),
            death_claims: TokenBucket::new(
                DEATH_CLAIM_RATE_LIMIT_PER_SEC,
                DEATH_CLAIM_RATE_LIMIT_PER_SEC,
                now,
            ),
            probes: TokenBucket::new(PROBE_RATE_LIMIT_PER_SEC, PROBE_RATE_LIMIT_PER_SEC, now),
            resyncs: TokenBucket::new(RESYNC_RATE_LIMIT_PER_SEC, RESYNC_RATE_LIMIT_PER_SEC, now),
            // 2× burst on pings absorbs an honest reconnect spike.
            pings: TokenBucket::new(ping_rate, 2.0 * ping_rate, now),
            // Log batches DO spike legitimately (end-of-match FlushAllAsync drain) — see the
            // burst const for the derivation.
            log_batches: TokenBucket::new(CLIENT_LOG_BATCH_RATE_LIMIT_PER_SEC, CLIENT_LOG_BATCH_BURST, now),
            auth: TokenBucket::new(AUTH_MSG_RATE_LIMIT_PER_SEC, AUTH_MSG_BURST, now),
        }
    }
    fn allow_move(&mut self, now_ms: u64) -> bool {
        self.moves.allow_logged(now_ms as f64, "MoveCommand (over move_command_rate_limit_per_sec)")
    }
    fn allow_eat_claim(&mut self, now_ms: u64) -> bool {
        self.eat_claims.allow_logged(now_ms as f64, "EatClaim (eat-claim flood)")
    }
    fn allow_death_claim(&mut self, now_ms: u64) -> bool {
        self.death_claims.allow_logged(now_ms as f64, "EnemyDeathClaim (death-claim flood)")
    }
    fn allow_probe(&mut self, now_ms: u64) -> bool {
        self.probes.allow_logged(now_ms as f64, "VisualOverlapProbe (probe flood)")
    }
    fn allow_resync(&mut self, now_ms: u64) -> bool {
        self.resyncs.allow_logged(now_ms as f64, "RequestFullState (resync flood)")
    }
    fn allow_ping(&mut self, now_ms: u64) -> bool {
        self.pings.allow_logged(now_ms as f64, "Ping (flood)")
    }
    fn allow_log_batch(&mut self, now_ms: u64) -> bool {
        self.log_batches.allow_logged(now_ms as f64, "ClientLogBatch (flood)")
    }
    fn allow_auth(&mut self, now_ms: u64) -> bool {
        self.auth.allow_logged(now_ms as f64, "WS auth message (SignInWithApple/SetNickname flood)")
    }
}

pub(crate) async fn process_message(
    message: ClientMessage,
    client_state: Arc<RwLock<ClientState>>,
    state: Arc<AppState>,
    ws_sender: WsSender,
    snap_sender: watch::Sender<Option<Message>>,
    rate_limiter: &mut GameplayRateLimiter,
) -> Option<ServerMessage> {
    // Handshake gate. Stage 0.5: nothing except Hello is processed until Welcome.
    let needs_handshake = !matches!(message, ClientMessage::Hello { .. });
    if needs_handshake {
        let welcomed = client_state.read().await.welcomed;
        if !welcomed {
            return Some(ServerMessage::Error {
                message: "Handshake required: send Hello first".to_string(),
            });
        }
    }

    match message {
        ClientMessage::Hello {
            protocol_version,
            client_build,
            platform,
            config_hash,
            maze_hash,
            connect_attempt_id,
            client_session_id,
            resume_intent,
        } => {
            info!("Hello received: proto={} build={} platform={}", protocol_version, client_build, platform);
            // "Hello must be first" also means "Hello happens ONCE": a second Hello on an
            // established connection is a client lifecycle bug (2026-06-10: the menu-teardown
            // window re-handshook a dying socket and the server happily re-Welcomed it,
            // wedging the client). Reject it instead of resetting handshake state.
            let (conn_id, already_welcomed) = {
                let cs = client_state.read().await;
                (cs.conn_id.clone(), cs.welcomed)
            };
            if already_welcomed {
                state.telemetry.server_info(
                    "hello_rejected",
                    serde_json::json!({ "conn_id": conn_id, "reason": "duplicate_hello" }),
                );
                return Some(ServerMessage::Error {
                    message: "Duplicate Hello on established connection".to_string(),
                });
            }
            // NDJSON: the handshake's first hop. Without it a reconnect that dies between
            // Hello and auth is invisible (2026-06-10: a post-round reconnect's socket went
            // silent with no way to tell "Hello never arrived" from "Welcome never sent").
            // Stash the client's attempt chain on the connection so connection_closed /
            // welcome_sent can echo it (engineer wave 7: tie conn_id ↔ attempt id even
            // when the client's buffered logs die with the socket).
            {
                let mut cs = client_state.write().await;
                cs.client_connect_attempt_id = connect_attempt_id;
                cs.client_session_id = if client_session_id.is_empty() {
                    None
                } else {
                    Some(client_session_id.chars().take(64).collect())
                };
                cs.client_resume_intent = resume_intent;
            }
            state.telemetry.server_info(
                "hello_received",
                serde_json::json!({
                    "conn_id": conn_id,
                    "protocol_version": protocol_version,
                    "build": client_build,
                    "platform": platform,
                    "client_connect_attempt_id": connect_attempt_id,
                    "client_resume_intent": resume_intent,
                }),
            );
            let reject = |reason: &str, message: String| {
                state.telemetry.server_info(
                    "hello_rejected",
                    serde_json::json!({ "conn_id": conn_id, "reason": reason }),
                );
                Some(ServerMessage::Error { message })
            };
            if protocol_version != state.assets.config.network.protocol_version {
                return reject(
                    "protocol_version_mismatch",
                    format!(
                        "Protocol version mismatch: server={}, client={}",
                        state.assets.config.network.protocol_version, protocol_version
                    ),
                );
            }
            if config_hash != state.assets.config_hash {
                return reject("config_hash_mismatch", "Config mismatch: update client bundle".to_string());
            }
            if maze_hash != state.assets.maze_hash {
                return reject("maze_hash_mismatch", "Maze mismatch: update client bundle".to_string());
            }

            client_state.write().await.welcomed = true;

            // "Welcome SENT" is its own fact (engineer wave 7 #8): hello_received followed
            // by connection_closed(welcomed) could not distinguish "Welcome never sent"
            // from "client closed before auth". (Enqueue failure still terminates the
            // connection — reply_send_failed in connection_closed covers that leg.)
            state.telemetry.server_info(
                "welcome_sent",
                serde_json::json!({
                    "conn_id": conn_id,
                    "protocol_version": state.assets.config.network.protocol_version,
                    "client_connect_attempt_id": connect_attempt_id,
                    "client_resume_intent": resume_intent,
                }),
            );

            Some(ServerMessage::Welcome {
                protocol_version: state.assets.config.network.protocol_version,
                min_supported_build: "0.1.0".to_string(),
                server_config_hash: state.assets.config_hash.clone(),
                server_maze_hash: state.assets.maze_hash.clone(),
                tick_rate: state.assets.config.room.tick_rate,
                // Server-authoritative kill model so the client logs real numbers.
                enemy_kill_radius_px: state.assets.config.collision.collision_distance_px
                    - crate::game::room::VICTIM_GRACE_PX,
                enemy_kill_contact_ticks: crate::game::room::CONTACT_TICKS_REQUIRED,
                // Single source of truth for the enemy interp delay + visible gate, from
                // config.death_fairness (the death policy and the client presentation must
                // agree on these; the values are folded into config_hash).
                enemy_interp_delay_ticks: state.assets.config.death_fairness.enemy_interp_delay_ticks,
                enemy_death_visible_confirm_radius_px: state
                    .assets
                    .config
                    .death_fairness
                    .visible_confirm_radius_px(),
                enemy_death_visible_gate_enabled: state.assets.config.death_fairness.visible_gate_enabled,
                enemy_death_policy_version: state.assets.config.death_fairness.policy_version,
                safe_zone_center_x: state.assets.config.safe_zone.center_x,
                safe_zone_center_y: state.assets.config.safe_zone.center_y,
                safe_zone_radius_px: state.assets.config.safe_zone.radius_px,
                // Shared truth for the client's prediction-lead clamp AND the server's death-claim
                // hold/future windows — so the two can't drift. (round-3 follow-up)
                max_client_prediction_lead_ticks: state
                    .assets
                    .config
                    .network
                    .max_client_prediction_lead_ticks,
                // Single source for the client's respawn-blink window = the server's protection window.
                spawn_protection_sec: state.assets.config.gameplay.spawn_protection_sec,
                // The exact overlap radius the server validates eat/death claims against, so the
                // client creates claims against the same number instead of a hardcoded 18f. (review #3)
                claim_visible_overlap_radius_px: state.assets.config.claim_fairness.visible_overlap_radius_px,
                // Resume admission shield window — the client's claim gates mirror the exact
                // same window the server suppresses with. (lead 2026-06-11 P0 #1)
                resume_shield_ticks: crate::game::room::RESUME_SHIELD_TICKS,
            })
        }

        // Stage 2 wire format includes nonce. Full Apple JWT/nonce verification
        // is Stage 8; for now we just accept-and-log the nonce and use the
        // existing token-extraction path.
        ClientMessage::SignInWithApple { apple_token, nonce } => {
            // Per-connection auth budget: the game socket isn't the auth path anymore, so a
            // flood of verify-triggering sign-ins on one socket is shed early. (review)
            if !rate_limiter.allow_auth(now_unix_ms()) {
                return Some(ServerMessage::AuthFailed { reason: "too many sign-in attempts".to_string() });
            }
            info!("Processing SignInWithApple (nonce_len={})", nonce.len());
            match state.auth_service.sign_in_with_apple(&apple_token, &nonce).await {
                Ok(user) => {
                    // Fresh provider sign-in earns OUR session token: the client stores
                    // it and never re-runs the native (Face ID) flow on reconnects.
                    let session = issue_session_logged(&state, &user.id).await;
                    Some(grant_auth_success(&client_state, &state, user, session).await)
                }
                Err(crate::error::GameError::Auth(msg)) if msg == "Nickname required" => {
                    let mut cs = client_state.write().await;
                    cs.pending_apple_token = Some(apple_token);
                    cs.pending_apple_nonce = Some(nonce);
                    Some(ServerMessage::AuthFailed { reason: msg })
                }
                Err(crate::error::GameError::Auth(msg)) => {
                    // Verification failures (bad/forged/expired token) are AuthFailed,
                    // not Error: the client falls back to a fresh provider sign-in.
                    Some(ServerMessage::AuthFailed { reason: msg })
                }
                Err(e) => Some(ServerMessage::Error { message: e.to_string() }),
            }
        }

        ClientMessage::SignInWithSession { session_token } => {
            // This IS the normal game-socket auth path — bound it too (a hostile socket can
            // spam junk tokens, each a DB lookup), sharing the tight auth budget. (review)
            if !rate_limiter.allow_auth(now_unix_ms()) {
                return Some(ServerMessage::AuthFailed { reason: "too many sign-in attempts".to_string() });
            }
            match state.auth_service.sign_in_with_session(&session_token).await {
                // No new token in the reply — the client already holds the one it used.
                Ok(user) => Some(grant_auth_success(&client_state, &state, user, None).await),
                // Unknown/expired session: AuthFailed("...session...") tells the client to
                // CLEAR its stored token and fall back to the provider sign-in.
                Err(crate::error::GameError::InvalidSession) => {
                    Some(ServerMessage::AuthFailed { reason: "Invalid session".to_string() })
                }
                // A DB hiccup is NOT an invalid session — send a generic Error (which the
                // client surfaces without wiping the session), so a transient outage can't
                // log the player out. (review: same false-logout class as the HTTP path)
                Err(e) => {
                    tracing::error!("session auth DB error: {e}");
                    Some(ServerMessage::Error {
                        message: "authentication temporarily unavailable".to_string(),
                    })
                }
            }
        }

        ClientMessage::SetNickname { nickname } => {
            // Shares the auth budget — SetNickname only follows a SignInWithApple that needed
            // a nickname, so an honest flow spends 2 tokens total. (review)
            if !rate_limiter.allow_auth(now_unix_ms()) {
                return Some(ServerMessage::NicknameUnavailable);
            }
            // Log the LENGTH, not the raw value: an unvalidated nickname can carry newlines
            // (log injection) — and validation happens inside set_/update_nickname below.
            info!("SetNickname request (len={})", nickname.chars().count());
            let (apple_token, apple_nonce, user_id) = {
                let cs = client_state.read().await;
                (cs.pending_apple_token.clone(), cs.pending_apple_nonce.clone(), cs.user_id.clone())
            };

            if let Some(token) = apple_token {
                let nonce = apple_nonce.unwrap_or_default();
                match state.auth_service.set_nickname(&token, &nonce, &nickname).await {
                    Ok(user) => {
                        // A brand-new user finishing sign-up IS a fresh provider
                        // sign-in — it earns a session token like the direct path.
                        let session = issue_session_logged(&state, &user.id).await;
                        Some(grant_auth_success(&client_state, &state, user, session).await)
                    }
                    Err(_) => Some(ServerMessage::NicknameUnavailable),
                }
            } else if let Some(uid) = user_id {
                match state.auth_service.update_nickname(&uid, &nickname).await {
                    Ok(user) => {
                        let mut cs = client_state.write().await;
                        cs.nickname = Some(user.nickname.clone());
                        Some(ServerMessage::NicknameSet { nickname: user.nickname })
                    }
                    Err(_) => Some(ServerMessage::NicknameUnavailable),
                }
            } else {
                Some(ServerMessage::Error { message: "Must authenticate first".to_string() })
            }
        }

        ClientMessage::JoinGame => {
            info!("Processing JoinGame request");
            do_join_game(&client_state, &state, &ws_sender, &snap_sender).await
        }

        ClientMessage::Resume { resume_token } => {
            info!("Processing Resume request");
            do_resume(&client_state, &state, &ws_sender, &snap_sender, resume_token).await
        }

        ClientMessage::MoveCommand(cmd) => {
            // Transport rate guard (enforces move_command_rate_limit_per_sec): shed silently when
            // over budget — moves are idempotent-ish (a newer move supersedes; the client retransmits
            // and reconciliation recovers), so dropping is safe and cheaper than an error reply.
            if !rate_limiter.allow_move(now_unix_ms()) {
                return None;
            }
            let (room_id, player_id) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone()),
                    _ => return Some(ServerMessage::Error { message: "Not in a game".to_string() }),
                }
            };
            state.room_manager.player_move(&room_id, &player_id, cmd).await;
            None
        }

        ClientMessage::EatClaim(claim) => {
            // Transport rate guard (own bucket): shed silently when over budget. The room's
            // per-attacker dedup/cap already bound accepted claims; this stops the flood earlier.
            if !rate_limiter.allow_eat_claim(now_unix_ms()) {
                return None;
            }
            let (room_id, player_id) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone()),
                    _ => return Some(ServerMessage::Error { message: "Not in a game".to_string() }),
                }
            };
            state.room_manager.player_eat_claim(&room_id, &player_id, claim).await;
            None
        }

        ClientMessage::EnemyDeathClaim(claim) => {
            // Transport rate guard — its OWN bucket (not shared with eat/probe) so an eat or probe
            // burst can't shed this gameplay-critical claim. The room's per-player pending cap is the
            // authoritative bound; this only sheds a gross flood at the edge.
            if !rate_limiter.allow_death_claim(now_unix_ms()) {
                return None;
            }
            let (room_id, player_id) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone()),
                    _ => return Some(ServerMessage::Error { message: "Not in a game".to_string() }),
                }
            };
            state.room_manager.player_enemy_death_claim(&room_id, &player_id, claim).await;
            None
        }

        ClientMessage::ClientWorldReady { world_sync_epoch, anchor_tick } => {
            // VISUAL readiness — the overlay-can-drop fact. Claims are gated on the
            // STRICTER ClientClaimReady below (lead 2026-06-11 readiness split). Honest
            // cadence is once per join/resume; a duplicate is a cheap idempotent set.
            let (room_id, player_id) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone()),
                    _ => return None, // not in a game — nothing to mark
                }
            };
            if let Some(room) = state.room_manager.get_room(&room_id).await {
                room.write().await.set_player_world_ready(&player_id, world_sync_epoch, anchor_tick);
            }
            None
        }

        ClientMessage::ClientClaimReady { world_sync_epoch, anchor_tick } => {
            // CLAIM readiness — visual world + re-proven clock + shield expired. Flips the
            // per-admission flag the death/eat claim `claim_ready_not_seen` gates check.
            let (room_id, player_id) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone()),
                    _ => return None,
                }
            };
            if let Some(room) = state.room_manager.get_room(&room_id).await {
                room.write().await.set_player_claim_ready(&player_id, world_sync_epoch, anchor_tick);
            }
            None
        }

        ClientMessage::VisualOverlapProbe { enemy_id, enemy_generation, known_server_tick } => {
            // Observe-only diagnostic, but it still reaches the room — rate-limit it under its OWN
            // probe budget so diagnostics can never eat a gameplay budget. Shed silently.
            if !rate_limiter.allow_probe(now_unix_ms()) {
                return None;
            }
            let (room_id, player_id) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone()),
                    // Not in a game: drop the diagnostic silently (observe-only, no error path).
                    _ => return None,
                }
            };
            state
                .room_manager
                .player_visual_overlap_probe(
                    &room_id,
                    &player_id,
                    enemy_id,
                    enemy_generation,
                    known_server_tick,
                )
                .await;
            None
        }

        // Stage 2.5: TimeSync. Server echoes client_send_time_ms verbatim so the
        // client can pair Pings with Pongs; server_time_ms is the wall-clock at
        // send time. Allowed wall-clock use — network boundary, not gameplay.
        ClientMessage::Ping { client_send_time_ms } => {
            // Transport rate guard (beta hardening): the Pong path takes the room READ lock to
            // stamp server_tick, so a ping flood contends the per-tick writer. Shed at the edge;
            // a dropped Pong is harmless (TimeSync just skips this sample and the client pings
            // again) — which is also why the OUTBOUND Pong reply is the one BestEffortDrop in
            // ReplyDropPolicy, never a reason to close the connection. An honest cadence
            // (config.network.ping_interval_ms) never trips this.
            if !rate_limiter.allow_ping(now_unix_ms()) {
                return None;
            }
            // Read the room tick with a real awaited lock. A `try_read().unwrap_or(0)`
            // here returns server_tick=0 whenever the room lock is momentarily held,
            // which poisons the client's TimeSync mid-match (its prediction clock
            // hard-snaps back to ~0). Waiting briefly for the lock is far better.
            let room_id = {
                let cs = client_state.read().await;
                cs.current_room.clone()
            };
            let server_tick = if let Some(room_id) = room_id {
                if let Some(room) = state.room_manager.get_room(&room_id).await {
                    room.read().await.tick
                } else {
                    tracing::warn!("Pong requested for missing room {}", room_id);
                    0
                }
            } else {
                0
            };
            Some(ServerMessage::Pong { client_send_time_ms, server_time_ms: now_unix_ms(), server_tick })
        }

        ClientMessage::RequestFullState { reason, request_id } => {
            // Transport rate guard (review #3): a resync builds a full keyframe under the room READ
            // lock, contending with the per-tick write lock — so shed a resync flood at the edge. The
            // client's own resume-retry recovers, and an honest client resyncs only rarely.
            if !rate_limiter.allow_resync(now_unix_ms()) {
                return None;
            }
            // Client-supplied — bound it so a misbehaving client can't blow up log
            // lines / allocations with a giant reason string.
            let reason: String = reason.chars().take(64).collect();
            // 0 = legacy client without correlation ids → the reply echoes None.
            let request_id = (request_id != 0).then_some(request_id);
            let (conn_id, user_id, player_id, room_id) = {
                let cs = client_state.read().await;
                (cs.conn_id.clone(), cs.user_id.clone(), cs.player_id.clone(), cs.current_room.clone())
            };
            let Some(room_id) = room_id else {
                tracing::warn!("RequestFullState ignored: no room, reason={}", reason);
                state.telemetry.server_warn(
                    "full_state_request_ignored",
                    serde_json::json!({
                        "request_id": request_id, "conn_id": conn_id, "user_id": user_id,
                        "client_reason": reason, "ignore_reason": "no_room",
                    }),
                );
                return None;
            };
            match state.room_manager.get_room(&room_id).await {
                Some(room) => {
                    let room = room.read().await;
                    tracing::info!(
                        "📦 full state requested room={} tick={} reason={} request_id={:?}",
                        room_id,
                        room.tick,
                        reason,
                        request_id
                    );
                    // request_received + response_sent under the same room read lock: the
                    // tick can't advance between them, so the pair is one correlated fact
                    // (request_id ties them to the client's client_full_state_request_sent /
                    // client_full_state_received — the lead's resume-backlog proof chain).
                    state.telemetry.server_info(
                        "full_state_request_received",
                        serde_json::json!({
                            "request_id": request_id, "conn_id": conn_id, "user_id": user_id,
                            "room_id": room_id, "player_id": player_id,
                            "server_tick": room.tick, "client_reason": reason,
                        }),
                    );
                    let reply = room.build_full_state_reply(request_id);
                    if let ServerMessage::GameState(full) = &reply {
                        state.telemetry.server_info(
                            "full_state_response_sent",
                            serde_json::json!({
                                "request_id": request_id, "conn_id": conn_id,
                                "room_id": room_id, "player_id": player_id,
                                "snapshot_tick": full.tick, "server_tick_now": room.tick,
                                "last_event_id": full.last_event_id,
                                "last_processed_input_seq": full.last_processed_input_seq,
                                "players_count": full.players.len(),
                                "enemies_count": full.enemies.len(),
                            }),
                        );
                    }
                    Some(reply)
                }
                None => {
                    tracing::warn!("RequestFullState ignored: missing room {} reason={}", room_id, reason);
                    state.telemetry.server_warn(
                        "full_state_request_ignored",
                        serde_json::json!({
                            "request_id": request_id, "conn_id": conn_id, "user_id": user_id,
                            "room_id": room_id, "client_reason": reason,
                            "ignore_reason": "missing_room",
                        }),
                    );
                    None
                }
            }
        }

        ClientMessage::LeaveGame => {
            let (room_id, player_id, resume_token) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone(), cs.resume_token.clone()),
                    _ => return Some(ServerMessage::Error { message: "Not in a game".to_string() }),
                }
            };
            state.room_manager.leave_room(&room_id, &player_id).await;
            // Explicit leave: drop the resume-token registration too, else the Active entry
            // lingers in the registry until the socket eventually closes. (Blocker 2)
            if let Some(token) = &resume_token {
                state.room_manager.forget_resume(token).await;
            }
            let mut cs = client_state.write().await;
            cs.current_room = None;
            cs.player_id = None;
            cs.resume_token = None; // explicit leave: free the slot, never hold for resume
            Some(ServerMessage::GameLeft)
        }

        ClientMessage::PlayAgain => {
            let leave_result = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => Some((rid.clone(), pid.clone(), cs.resume_token.clone())),
                    _ => None,
                }
            };
            if let Some((room_id, player_id, resume_token)) = leave_result {
                state.room_manager.leave_room(&room_id, &player_id).await;
                if let Some(token) = &resume_token {
                    state.room_manager.forget_resume(token).await;
                }
            }
            {
                let mut cs = client_state.write().await;
                cs.current_room = None;
                cs.player_id = None;
                cs.resume_token = None; // explicit leave of the old match: don't hold its slot
            }
            do_join_game(&client_state, &state, &ws_sender, &snap_sender).await
        }

        ClientMessage::ReturnToMenu => {
            let (room_id, player_id, resume_token) = {
                let cs = client_state.read().await;
                match (&cs.current_room, &cs.player_id) {
                    (Some(rid), Some(pid)) => (rid.clone(), pid.clone(), cs.resume_token.clone()),
                    _ => return Some(ServerMessage::Error { message: "Not in a game".to_string() }),
                }
            };
            state.room_manager.leave_room(&room_id, &player_id).await;
            if let Some(token) = &resume_token {
                state.room_manager.forget_resume(token).await;
            }
            let mut cs = client_state.write().await;
            cs.current_room = None;
            cs.player_id = None;
            cs.resume_token = None; // explicit return to menu: free the slot, never hold for resume
            Some(ServerMessage::GameLeft)
        }

        ClientMessage::ClientLogBatch(batch) => {
            // Transport rate guard (beta hardening): each batch is already size-bounded, but
            // nothing capped frequency — a tight flush loop could pin the telemetry writer + disk.
            // Shed over-budget batches at the edge; losing some client telemetry is acceptable,
            // an honest client flushes ~every 2s.
            if !rate_limiter.allow_log_batch(now_unix_ms()) {
                return None;
            }
            // Snapshot the SERVER-AUTHORITATIVE identity for this connection. We
            // stamp these onto every client event ourselves; the client's own
            // claimed room/player ids inside each entry are advisory only.
            let (conn_id, user_id, room_id, player_id) = {
                let cs = client_state.read().await;
                (cs.conn_id.clone(), cs.user_id.clone(), cs.current_room.clone(), cs.player_id.clone())
            };
            handle_client_log_batch(
                &state.telemetry,
                &conn_id,
                user_id.as_deref(),
                room_id.as_deref(),
                player_id.as_deref(),
                batch,
            );
            None
        }
    }
}

/// Fold a flushed client log batch into the unified telemetry stream. Bounded so
/// a misbehaving client can't flood the file: at most `MAX_ENTRIES` written and
/// oversized `fields_json` is replaced with a marker rather than parsed.
fn handle_client_log_batch(
    telemetry: &Telemetry,
    conn_id: &str,
    user_id: Option<&str>,
    room_id: Option<&str>,
    player_id: Option<&str>,
    batch: ClientLogBatch,
) {
    const MAX_ENTRIES: usize = 200;
    const MAX_FIELD_BYTES: usize = 8192;

    let total = batch.entries.len();
    let written = total.min(MAX_ENTRIES);
    // The per-batch RECEIPT summary is verbose-only (one per client flush adds up across
    // clients). The actual client entries are still folded below — that's the unified
    // stream's whole point — but the client only sends lean (non-firehose) entries now,
    // so the fold volume is small.
    if telemetry.is_verbose() {
        telemetry.server_info(
            "client_log_batch_received",
            serde_json::json!({
                "conn_id": conn_id,
                "player_id": player_id,
                "room_id": room_id,
                "session_id": batch.session_id,
                "flush_reason": batch.flush_reason,
                "client_gen": batch.gen,
                "entries": total,
                "written": written,
                "dropped_by_client": batch.dropped,
            }),
        );
    }

    // Belt-and-suspenders: even if an OLD client build still ships the per-frame firehose,
    // drop those entries from the fold when not verbose — BEFORE the expensive fields_json
    // parse — so the server's CPU/disk can't be pinned by a chatty client we haven't
    // redeployed yet. The client also gates these at the source (companion change); every
    // other client event (anomalies, deaths, eat claims, lifecycle) still folds.
    const CLIENT_FIREHOSE: [&str; 2] = ["client_snapshot_received", "client_net_stats"];
    let verbose = telemetry.is_verbose();
    for entry in batch.entries.into_iter().take(MAX_ENTRIES) {
        if !verbose && CLIENT_FIREHOSE.contains(&entry.event_name.as_str()) {
            continue;
        }
        let fields = if entry.fields_json.len() <= MAX_FIELD_BYTES {
            serde_json::from_str::<serde_json::Value>(&entry.fields_json)
                .unwrap_or_else(|_| serde_json::json!({ "raw_fields": entry.fields_json }))
        } else {
            serde_json::json!({ "fields_too_large": true, "bytes": entry.fields_json.len() })
        };
        telemetry.emit(serde_json::json!({
            "source": "client",
            "level": entry.level,
            "event": entry.event_name,
            "run_id": telemetry.run_id(),
            "conn_id": conn_id,
            "session_id": batch.session_id,
            "client_gen": batch.gen,
            "user_id": user_id,
            "room_id": room_id,
            "player_id": player_id,
            "client_seq": entry.seq,
            "client_event_unix_ms": entry.client_unix_ms,
            "client_mono_ms": entry.client_mono_ms,
            "server_tick": entry.server_tick,
            "client_tick": entry.client_tick,
            "render_tick": entry.render_tick,
            "message": entry.message,
            "fields": fields,
        }));
    }
}

async fn do_join_game(
    client_state: &Arc<RwLock<ClientState>>,
    state: &Arc<AppState>,
    ws_sender: &WsSender,
    snap_sender: &watch::Sender<Option<Message>>,
) -> Option<ServerMessage> {
    // One connection = at most one room binding. A JoinGame while current_room/player_id
    // are still set (e.g. after a client-side duplicate handshake) would wire_joined() OVER
    // the existing binding — a ghost slot / forwarder leak. Explicit leave (ReturnToMenu/
    // LeaveGame) and PlayAgain clear the binding first, so the normal flows are unaffected.
    // (lead review)
    {
        let cs = client_state.read().await;
        if cs.current_room.is_some() || cs.player_id.is_some() {
            return Some(ServerMessage::Error {
                message: "Already in a game; leave or PlayAgain first".to_string(),
            });
        }
    }
    // Maintenance / emergency switch: refuse NEW matches while keeping live ones running (a
    // dropped player can still Resume into an in-flight match — only fresh entry is gated).
    if !state.matchmaking_enabled {
        return Some(ServerMessage::Error {
            message: "Matchmaking is temporarily disabled for maintenance. Try again soon.".to_string(),
        });
    }

    let (user_id, nickname) = match authenticated_identity(client_state).await {
        Ok(id) => id,
        Err(msg) => return Some(ServerMessage::Error { message: msg }),
    };

    // JoinGame while this user still has a live resume session = a reconnect that LOST its
    // token (proven 2026-06-10: the first reconnect attempt died between Hello and Resume,
    // the retry fell back to JoinGame, and the server admitted a SECOND player while the
    // held slot — and the score — sat reserved until grace expiry). Explicit leaves
    // (LeaveGame / ReturnToMenu / PlayAgain) forget the session first, so an INTENTIONAL
    // fresh game never lands here; the only way in is a lost-token reconnect, and the
    // honest answer to that is the resume the client meant to ask for. On any resume
    // failure (expired / room gone / races) fall through to the fresh join.
    let resume_session = state.room_manager.find_resume_token_for_user(&user_id).await;
    // "Join REQUESTED" is its own fact (lead manifesto #2), with the hold state at that
    // moment — so a ghost join can never again hide behind a missing resume_requested.
    state.telemetry.server_info(
        "join_game_requested",
        serde_json::json!({
            "conn_id": client_state.read().await.conn_id,
            "user_id": user_id,
            "nickname": nickname,
            "has_active_resume_session": resume_session.is_some(),
        }),
    );
    // Hold-lookup forensics + the protocol-mismatch assertion (engineer wave 7 #10/#11):
    // the server holds a session while the CLIENT arrived as a fresh join — the exact
    // desync (lost token / pre-auth clear) the wave-7 client logs are hunting. Echoes the
    // client's Hello-reported attempt/intent so both sides of the mismatch sit in one line.
    {
        let peek = state.room_manager.peek_resume_session_for_user(&user_id).await;
        let cs = client_state.read().await;
        let now = now_unix_ms();
        let grace_ms = (state.assets.config.network.reconnect_grace_sec as u64) * 1000;
        state.telemetry.server_info(
            "resume_hold_lookup",
            serde_json::json!({
                "conn_id": cs.conn_id,
                "request_kind": "join",
                "user_id": user_id,
                "has_active_resume_session": peek.is_some(),
                "hold_room_id": peek.as_ref().map(|p| p.room_id.clone()),
                "hold_is_held": peek.as_ref().map(|p| p.held),
                "hold_remaining_ms": peek.as_ref().and_then(|p| p.deadline_unix_ms).map(|d| d.saturating_sub(now)),
                "held_ms": peek.as_ref().and_then(|p| p.deadline_unix_ms)
                    .map(|d| grace_ms.saturating_sub(d.saturating_sub(now))),
                "hold_score": peek.as_ref().and_then(|p| p.score),
                "client_connect_attempt_id": cs.client_connect_attempt_id,
                "client_resume_intent": cs.client_resume_intent,
                "decision": if peek.is_some() { "convert_join_to_resume" } else { "fresh_join" },
            }),
        );
        if peek.is_some() {
            state.telemetry.server_warn(
                "resume_protocol_mismatch",
                serde_json::json!({
                    "conn_id": cs.conn_id,
                    "request_kind": "join",
                    "user_id": user_id,
                    "has_active_resume_session": true,
                    "client_connect_attempt_id": cs.client_connect_attempt_id,
                    "client_resume_intent": cs.client_resume_intent,
                    "decision": "converted_to_resume",
                }),
            );
        }
    }
    if let Some(token) = resume_session {
        let conn_id = { client_state.read().await.conn_id.clone() };
        match state.room_manager.resume_room(&token, user_id.clone(), nickname.clone()).await {
            Ok(joined) => {
                state.telemetry.server_info(
                    "join_converted_to_resume",
                    serde_json::json!({
                        "conn_id": conn_id,
                        "user_id": user_id,
                        "room_id": joined.room_id,
                        "player_id": joined.player_id,
                        "server_tick": joined.server_tick,
                    }),
                );
                wire_joined(client_state, state, ws_sender, snap_sender, joined).await;
                return None;
            }
            Err(reason) => {
                state.telemetry.server_info(
                    "join_resume_conversion_failed",
                    serde_json::json!({
                        "conn_id": conn_id,
                        "user_id": user_id,
                        "reason": reason,
                    }),
                );
                // fall through to the fresh join below
            }
        }
    }

    match state.room_manager.join_room(user_id, nickname).await {
        Ok(joined) => {
            wire_joined(client_state, state, ws_sender, snap_sender, joined).await;
            None
        }
        Err(e) => Some(ServerMessage::Error { message: e }),
    }
}

/// Reconnect after an unexpected drop: present the resume_token and ask to be put back into
/// the SAME match. On success the wiring is identical to a fresh join (GameJoined, keyframe,
/// then the forwarder) — only the room/player/score are restored. On failure reply
/// ResumeRejected so the client falls back to JoinGame.
async fn do_resume(
    client_state: &Arc<RwLock<ClientState>>,
    state: &Arc<AppState>,
    ws_sender: &WsSender,
    snap_sender: &watch::Sender<Option<Message>>,
    resume_token: String,
) -> Option<ServerMessage> {
    let (user_id, nickname) = match authenticated_identity(client_state).await {
        Ok(id) => id,
        Err(msg) => return Some(ServerMessage::Error { message: msg }),
    };

    // "Resume RECEIVED" is its own fact: without it, a reconnect that dies before sending
    // Resume and one whose Resume the server swallowed look identical in the NDJSON
    // (resume_rejected only covers the refusal path; resume_success only the happy one).
    state.telemetry.server_info(
        "resume_requested",
        serde_json::json!({
            "conn_id": client_state.read().await.conn_id,
            "user_id": user_id,
        }),
    );
    // Same hold-lookup forensics as the join path (engineer wave 7 #10), observe-only —
    // resume_room remains the single arbiter that actually consumes the session.
    {
        let peek = state.room_manager.peek_resume_session_for_user(&user_id).await;
        let cs = client_state.read().await;
        let now = now_unix_ms();
        let grace_ms = (state.assets.config.network.reconnect_grace_sec as u64) * 1000;
        state.telemetry.server_info(
            "resume_hold_lookup",
            serde_json::json!({
                "conn_id": cs.conn_id,
                "request_kind": "resume",
                "user_id": user_id,
                "has_active_resume_session": peek.is_some(),
                "hold_room_id": peek.as_ref().map(|p| p.room_id.clone()),
                "hold_is_held": peek.as_ref().map(|p| p.held),
                "hold_remaining_ms": peek.as_ref().and_then(|p| p.deadline_unix_ms).map(|d| d.saturating_sub(now)),
                "held_ms": peek.as_ref().and_then(|p| p.deadline_unix_ms)
                    .map(|d| grace_ms.saturating_sub(d.saturating_sub(now))),
                "hold_score": peek.as_ref().and_then(|p| p.score),
                "client_connect_attempt_id": cs.client_connect_attempt_id,
                "client_resume_intent": cs.client_resume_intent,
                "decision": "resume_requested",
            }),
        );
    }

    match state.room_manager.resume_room(&resume_token, user_id.clone(), nickname).await {
        Ok(joined) => {
            wire_joined(client_state, state, ws_sender, snap_sender, joined).await;
            None
        }
        Err(reason) => {
            // Observability: which resumes are refused and why (wrong user / expired / unknown
            // token / match gone). Pairs with resume_hold_started/resume_success. (review medium #4)
            state
                .telemetry
                .server_info("resume_rejected", serde_json::json!({ "user_id": user_id, "reason": reason }));
            Some(ServerMessage::ResumeRejected { reason })
        }
    }
}

/// Issue a session token for a freshly provider-verified user; a DB failure here is
/// logged but NOT fatal — auth still succeeds, the client just re-runs the provider
/// flow next launch instead of a session reconnect.
async fn issue_session_logged(state: &Arc<AppState>, user_id: &str) -> Option<String> {
    match state.auth_service.issue_session(user_id).await {
        Ok(token) => Some(token),
        Err(e) => {
            tracing::warn!("session issue failed for {user_id}: {e} — continuing without");
            None
        }
    }
}

/// Record the authenticated identity on the connection and emit `auth_success`. EVERY
/// branch that replies AuthSuccess must go through here — a new user authing via the
/// SetNickname path was telemetry-blind when only the SignInWithApple branch emitted
/// the event (lead review).
async fn grant_auth_success(
    client_state: &Arc<RwLock<ClientState>>,
    state: &Arc<AppState>,
    user: crate::db::User,
    session_token: Option<String>,
) -> ServerMessage {
    let mut cs = client_state.write().await;
    cs.user_id = Some(user.id.clone());
    cs.nickname = Some(user.nickname.clone());
    cs.pending_apple_token = None;
    cs.pending_apple_nonce = None;
    state.telemetry.server_info(
        "auth_success",
        serde_json::json!({
            "conn_id": cs.conn_id,
            "user_id": user.id,
            "via_session": session_token.is_none(),
        }),
    );
    ServerMessage::AuthSuccess {
        user_id: user.id,
        nickname: user.nickname,
        crystals: user.crystals,
        session_token,
    }
}

/// The authenticated (user_id, nickname) for this connection, or an error message if the
/// client hasn't finished auth + nickname yet.
async fn authenticated_identity(client_state: &Arc<RwLock<ClientState>>) -> Result<(String, String), String> {
    let cs = client_state.read().await;
    match (&cs.user_id, &cs.nickname) {
        (Some(uid), Some(nick)) => Ok((uid.clone(), nick.clone())),
        _ => Err("Not authenticated".to_string()),
    }
}

/// Wire a freshly joined OR resumed player to the socket: record room/player/resume_token in
/// the connection state, send GameJoined then the initial full keyframe IN ORDER, then spawn
/// the forwarder that pumps room reliable events + snapshots to the socket. Shared by
/// JoinGame and Resume so both get identical ordering guarantees.
async fn wire_joined(
    client_state: &Arc<RwLock<ClientState>>,
    state: &Arc<AppState>,
    ws_sender: &WsSender,
    snap_sender: &watch::Sender<Option<Message>>,
    joined: JoinedRoom,
) {
    {
        let mut cs = client_state.write().await;
        cs.current_room = Some(joined.room_id.clone());
        cs.player_id = Some(joined.player_id.clone());
        // Stash the token so an unexpected drop can hold THIS slot for a resume.
        cs.resume_token = Some(joined.resume_token.clone());
    }

    // Guaranteed order: send GameJoined, then the initial full keyframe, on the ordered
    // out_tx — BEFORE spawning the forwarder, so neither a room event nor a snapshot can
    // overtake GameJoined. The client must see: GameJoined → GameState full → events/deltas.
    let game_joined = ServerMessage::GameJoined {
        room_id: joined.room_id.clone(),
        player_id: joined.player_id.clone(),
        server_tick: joined.server_tick,
        server_time_ms: joined.server_time_ms,
        resume_token: joined.resume_token.clone(),
        admitted_via_resume: joined.via_resume,
    };
    // Both are FATAL control replies (ReplyDropPolicy): losing either wedges the client in
    // Joining forever. On failure flag the connection for termination (the read loop closes
    // it; disconnect-hold lets the client resume) and DON'T spawn the forwarder for a
    // connection that is about to die.
    let sent = send_reply(ws_sender, &game_joined, "game-joined")
        && send_reply(ws_sender, &joined.initial_full, "initial-keyframe");
    if !sent {
        client_state.write().await.terminate = true;
        return;
    }

    // Forwarder: room reliable events → the ordered out_tx FIFO; room snapshots → the
    // LATEST-ONLY snap watch (so a stalled socket overwrites stale frames instead of
    // queueing them behind Pongs).
    let sender = ws_sender.clone();
    let snap = snap_sender.clone();
    // The reliable-before-snapshot ordering lives in ONE named, unit-tested component
    // (review #3 / rule 9) instead of an inline biased select here. The forwarder just
    // pulls ordered items and routes each to its downstream lane.
    let mut sequencer = OutboundSequencer::new(joined.reliable_rx, joined.snapshot_rx);
    // Ordering-trace telemetry: ws_seq advances on EVERY send (reliable + snapshot); only
    // RELIABLE sends are logged by default (snapshots are ~30/s) — gaps in the logged ws_seq
    // reveal interleaved snapshots without per-frame spam. Snapshot sends are logged only
    // under verbose. Pairs with the client's client_ws_recv to prove reliable-before-snapshot.
    let send_telemetry = state.telemetry.clone();
    let send_conn_id = { client_state.read().await.conn_id.clone() };
    let send_room_id = joined.room_id.clone();
    let send_player_id = joined.player_id.clone();
    // For flagging the connection fatal on reliable-lane overflow (see below).
    let term_state = Arc::clone(client_state);
    tokio::spawn(async move {
        // One ordered stream: reliable events always precede the snapshot reflecting them
        // (bug #33). `item.seq` is the ws_seq ordering trace; routing is by lane.
        while let Some(item) = sequencer.next().await {
            match item.kind {
                OutboundKind::Reliable => {
                    // Ordering trace is VERBOSE-ONLY: building+writing this json per reliable
                    // send is hot-path CPU + NDJSON disk I/O that pins a core during a busy
                    // round. item.seq still advances unconditionally, so a verbose run still
                    // shows the snapshot gaps between reliable sends.
                    if send_telemetry.is_verbose() {
                        let (event_type, event_id, server_tick) = ws_msg_meta(&item.message);
                        send_telemetry.server_info(
                            "server_ws_send",
                            serde_json::json!({
                                "conn_id": send_conn_id, "room_id": send_room_id, "player_id": send_player_id,
                                "ws_seq": item.seq, "kind": "reliable",
                                "event_type": event_type, "event_id": event_id, "server_tick": server_tick,
                            }),
                        );
                    }
                    // An unencodable reliable event is as LOST as one dropped by a full
                    // FIFO — and a lost reliable event is a degraded stream, so the same
                    // connection-fatal policy applies (can't happen for our own types,
                    // but if it ever does it must be loud, not a silent skip).
                    let Some(frame) = encode_server_msg(&item.message) else {
                        tracing::error!(
                            "reliable encode failed: type={} — closing connection",
                            server_msg_kind(&item.message)
                        );
                        term_state.write().await.terminate = true;
                        break;
                    };
                    // Reliable events must NOT be silently dropped — but FULL and CLOSED
                    // are different facts and must not share one alarm (soak round-2:
                    // 69 false "overflow" warns that were all ordinary churn teardowns):
                    //   * Full   — real back-pressure on a LIVE socket: warn (the soak
                    //     monitor's hard 'outtx' needle) + terminate, so the client
                    //     reconnects instead of living with a dead reliable stream.
                    //   * Closed — the socket task already ended (disconnect/churn) and
                    //     dropped out_rx while the room drains events for the held slot.
                    //     Nothing is degraded and nobody is reachable: end quietly.
                    match sender.try_send(frame) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::warn!("reliable out_tx full — closing connection");
                            term_state.write().await.terminate = true;
                            break;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            tracing::debug!("reliable out_tx closed (connection gone) — ending forwarder");
                            break;
                        }
                    }
                }
                OutboundKind::Snapshot => {
                    if send_telemetry.is_verbose() {
                        let (_t, _id, server_tick) = ws_msg_meta(&item.message);
                        send_telemetry.server_info(
                            "server_ws_send",
                            serde_json::json!({
                                "conn_id": send_conn_id, "room_id": send_room_id, "player_id": send_player_id,
                                "ws_seq": item.seq, "kind": "snapshot",
                                "server_tick": server_tick, "snapshot_seq": snapshot_seq_of(&item.message),
                            }),
                        );
                    }
                    // A snapshot that fails to encode is survivable (the next one is 33ms
                    // away) — but never silently: log it so an encoder bug can't present
                    // as "client just stopped receiving state".
                    let Some(frame) = encode_server_msg(&item.message) else {
                        tracing::error!("snapshot encode failed: type={}", server_msg_kind(&item.message));
                        continue;
                    };
                    // Latest-only hop to the socket. Snapshots are the binary lane's
                    // whole purpose — encode_server_msg makes them bincode frames.
                    if snap.send(Some(frame)).is_err() {
                        break;
                    }
                }
            }
        }
    });
    // GameJoined + the initial keyframe were already sent above, in order.
}

/// Stable inbound variant name for the connection_closed `last_inbound_message` field.
fn client_msg_kind(msg: &ClientMessage) -> &'static str {
    match msg {
        ClientMessage::Hello { .. } => "Hello",
        ClientMessage::SignInWithApple { .. } => "SignInWithApple",
        ClientMessage::SignInWithSession { .. } => "SignInWithSession",
        ClientMessage::SetNickname { .. } => "SetNickname",
        ClientMessage::JoinGame => "JoinGame",
        ClientMessage::LeaveGame => "LeaveGame",
        ClientMessage::MoveCommand(_) => "MoveCommand",
        ClientMessage::EatClaim(_) => "EatClaim",
        ClientMessage::EnemyDeathClaim(_) => "EnemyDeathClaim",
        ClientMessage::VisualOverlapProbe { .. } => "VisualOverlapProbe",
        ClientMessage::Ping { .. } => "Ping",
        ClientMessage::RequestFullState { .. } => "RequestFullState",
        ClientMessage::ClientLogBatch(_) => "ClientLogBatch",
        ClientMessage::PlayAgain => "PlayAgain",
        ClientMessage::ReturnToMenu => "ReturnToMenu",
        ClientMessage::Resume { .. } => "Resume",
        ClientMessage::ClientWorldReady { .. } => "ClientWorldReady",
        ClientMessage::ClientClaimReady { .. } => "ClientClaimReady",
    }
}

/// (event_type, event_id, server_tick) for the server_ws_send ordering trace.
fn ws_msg_meta(msg: &ServerMessage) -> (&'static str, Option<u64>, Option<u64>) {
    match msg {
        ServerMessage::Event(ev) => (ev.kind(), Some(ev.event_id()), Some(ev.server_tick())),
        ServerMessage::GameState(s) => ("GameState", None, Some(s.tick)),
        ServerMessage::GameStateDelta(d) => ("GameStateDelta", None, Some(d.tick)),
        _ => ("control", None, None),
    }
}

fn snapshot_seq_of(msg: &ServerMessage) -> Option<u64> {
    match msg {
        ServerMessage::GameState(s) => Some(s.snapshot_seq),
        ServerMessage::GameStateDelta(d) => Some(d.snapshot_seq),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct ClientState {
    /// Short per-connection id, generated server-side, so telemetry can correlate
    /// all events from one socket even before auth/join assign a user/player id.
    pub conn_id: String,
    pub welcomed: bool,
    /// Client-reported attempt chain from Hello (engineer wave 7): 0/None/false = old
    /// client. Echoed in welcome_sent / connection_closed so the server side of a dead
    /// reconnect names WHICH client attempt it was.
    pub client_connect_attempt_id: u32,
    pub client_session_id: Option<String>,
    pub client_resume_intent: bool,
    pub user_id: Option<String>,
    pub nickname: Option<String>,
    pub pending_apple_token: Option<String>,
    /// Nonce from this connection's SignInWithApple, stashed so the SetNickname
    /// follow-up can re-verify the same token+nonce binding. (audit item 5)
    pub pending_apple_nonce: Option<String>,
    pub current_room: Option<String>,
    pub player_id: Option<String>,
    /// Token from this connection's GameJoined. Stashed so an UNEXPECTED drop can hold the
    /// slot for a resume (the cleanup path keys the resume session by it). Cleared on an
    /// explicit LeaveGame/ReturnToMenu so those free the slot immediately.
    pub resume_token: Option<String>,
    /// Set when this connection's outbound stream is irrecoverably degraded: a FATAL
    /// control reply (GameJoined / initial keyframe in `wire_joined`) was lost to a
    /// full/closed out_tx, or the reliable forwarder hit the same overflow (a dead
    /// reliable stream under a live socket). The read loop checks it after each inbound
    /// message and closes the connection: the disconnect-hold cleanup still runs, so the
    /// client reconnects + resumes instead of wedging on a socket that will never deliver
    /// what it is waiting for.
    pub terminate: bool,
}

impl ClientState {
    fn new() -> Self {
        Self {
            conn_id: format!("c_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            welcomed: false,
            client_connect_attempt_id: 0,
            client_session_id: None,
            client_resume_intent: false,
            user_id: None,
            nickname: None,
            pending_apple_token: None,
            pending_apple_nonce: None,
            current_room: None,
            player_id: None,
            resume_token: None,
            terminate: false,
        }
    }
}

#[cfg(test)]
mod per_ip_tests {
    use super::*;

    #[test]
    fn per_ip_cap_blocks_excess_and_releases_on_drop() {
        let map: PerIpMap = Arc::new(Mutex::new(HashMap::new()));
        let ip = Some("203.0.113.7".to_string());
        // Cap 2: two acquire, the third is refused.
        let g1 = PerIpGuard::try_acquire(map.clone(), ip.clone(), 2);
        let g2 = PerIpGuard::try_acquire(map.clone(), ip.clone(), 2);
        assert!(g1.is_some() && g2.is_some());
        assert!(PerIpGuard::try_acquire(map.clone(), ip.clone(), 2).is_none(), "3rd over cap");
        // Drop one → a slot frees.
        drop(g1);
        let g3 = PerIpGuard::try_acquire(map.clone(), ip.clone(), 2);
        assert!(g3.is_some(), "a freed slot is reusable");
        // A DIFFERENT ip has its own budget.
        assert!(PerIpGuard::try_acquire(map.clone(), Some("198.51.100.9".into()), 2).is_some());
        drop(g2);
        drop(g3);
        // All released → the map doesn't leak the ip row.
        assert!(!map.lock().unwrap().contains_key("203.0.113.7"), "row removed at zero");
    }

    #[test]
    fn per_ip_disabled_paths_never_block() {
        let map: PerIpMap = Arc::new(Mutex::new(HashMap::new()));
        // cap 0 = disabled: unlimited.
        for _ in 0..50 {
            assert!(PerIpGuard::try_acquire(map.clone(), Some("1.2.3.4".into()), 0).is_some());
        }
        // No forwarded IP (direct/local) = not enforced even with a cap.
        for _ in 0..50 {
            assert!(PerIpGuard::try_acquire(map.clone(), None, 2).is_some());
        }
    }

    #[test]
    fn client_ip_prefers_forwarded_first_hop() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.7, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip_from_headers(&h).as_deref(), Some("203.0.113.7"));
        let mut h2 = HeaderMap::new();
        h2.insert("x-real-ip", "198.51.100.9".parse().unwrap());
        assert_eq!(client_ip_from_headers(&h2).as_deref(), Some("198.51.100.9"));
        assert_eq!(client_ip_from_headers(&HeaderMap::new()), None, "no proxy header → not enforced");
    }
}

#[cfg(test)]
mod handshake_tests {
    use super::*;
    use crate::config_shared::{load_config_from_str, load_maze_from_str, SharedAssets};
    use crate::protocol::Direction;

    fn make_assets() -> Arc<SharedAssets> {
        let config_str = include_str!("../../gameplay_config.toml");
        let maze_str = include_str!("../../maze.json");
        let config = Arc::new(load_config_from_str(config_str).unwrap());
        let maze = Arc::new(load_maze_from_str(maze_str).unwrap());
        Arc::new(SharedAssets {
            config,
            maze,
            config_hash: {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(include_bytes!("../../gameplay_config.toml"));
                format!("{:x}", h.finalize())
            },
            maze_hash: {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(include_bytes!("../../maze.json"));
                format!("{:x}", h.finalize())
            },
        })
    }

    #[test]
    fn only_hello_bypasses_handshake_gate() {
        let hello_like = ClientMessage::Hello {
            protocol_version: 2,
            client_build: "x".into(),
            platform: "ios".into(),
            config_hash: "h".into(),
            maze_hash: "h".into(),
            connect_attempt_id: 0,
            client_session_id: String::new(),
            resume_intent: false,
        };
        let other_msgs = [
            ClientMessage::JoinGame,
            ClientMessage::LeaveGame,
            ClientMessage::SignInWithApple { apple_token: "t".into(), nonce: "n".into() },
            ClientMessage::MoveCommand(crate::protocol::MoveCommand {
                seq: 1,
                target_tick: 0,
                direction: Direction::Up,
            }),
            ClientMessage::Ping { client_send_time_ms: 0 },
        ];

        let is_hello = |m: &ClientMessage| matches!(m, ClientMessage::Hello { .. });
        assert!(is_hello(&hello_like));
        for m in &other_msgs {
            assert!(!is_hello(m), "non-Hello must require handshake: {:?}", m);
        }
    }

    #[test]
    fn asset_hashes_are_stable() {
        let a1 = make_assets();
        let a2 = make_assets();
        assert_eq!(a1.config_hash, a2.config_hash);
        assert_eq!(a1.maze_hash, a2.maze_hash);
        assert_eq!(a1.config_hash.len(), 64);
        assert_eq!(a1.maze_hash.len(), 64);
    }
}

#[cfg(test)]
mod reply_policy_tests {
    use super::*;

    /// The reply-drop policy is per-VARIANT, not blanket (lead review): a dropped Pong is
    /// best-effort (re-requested every ping interval; killing the socket over it would turn
    /// slow-client back-pressure into reconnect churn), while every state-machine reply is
    /// fatal — the client awaits it with no handshake-phase timeout, so the only safe
    /// recovery is closing the connection.
    #[test]
    fn dropped_pong_is_best_effort_dropped_control_reply_is_fatal() {
        // Capacity-1 channel, pre-filled: every try_send below hits "full".
        let (tx, _rx) = mpsc::channel::<Message>(1);
        tx.try_send(Message::Text("fill".into())).unwrap();

        let pong = ServerMessage::Pong { client_send_time_ms: 1, server_time_ms: 2, server_tick: 3 };
        assert!(send_reply(&tx, &pong, "test"), "a dropped Pong must NOT close the connection");

        for fatal in [
            ServerMessage::Error { message: "x".into() },
            ServerMessage::AuthFailed { reason: "x".into() },
            ServerMessage::GameLeft,
            ServerMessage::ResumeRejected { reason: "x".into() },
        ] {
            assert!(
                !send_reply(&tx, &fatal, "test"),
                "a dropped {} must close the connection",
                server_msg_kind(&fatal)
            );
        }
    }

    /// CLOSED is teardown, not back-pressure: every reply (even best-effort Pong) tells
    /// the caller to stop — the connection is already gone — and none of it is the
    /// warn-level "control reply dropped" alarm (soak round-2: ordinary churn teardowns
    /// must not feed the monitor's hard needles).
    #[test]
    fn closed_channel_means_stop_quietly_for_all_replies() {
        let (tx, rx) = mpsc::channel::<Message>(1);
        drop(rx);
        let pong = ServerMessage::Pong { client_send_time_ms: 1, server_time_ms: 2, server_tick: 3 };
        assert!(!send_reply(&tx, &pong, "test"), "closed channel stops even best-effort sends");
        assert!(!send_reply(&tx, &ServerMessage::GameLeft, "test"));
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;

    #[test]
    fn token_bucket_bursts_then_throttles_then_refills() {
        let mut b = TokenBucket::new(10.0, 10.0, 0.0); // 10/sec sustained, 10 burst
        for i in 0..10 {
            assert!(b.allow(0.0), "burst token {i} allowed from a full bucket");
        }
        assert!(!b.allow(0.0), "11th request in the same instant is throttled");
        // 100ms later → exactly one token refilled (10/sec).
        assert!(b.allow(100.0), "one token refilled after 100ms");
        assert!(!b.allow(100.0), "but only one");
        // A long gap refills to capacity, never beyond (no unbounded accrual).
        for _ in 0..10 {
            assert!(b.allow(5000.0));
        }
        assert!(!b.allow(5000.0), "capacity is the ceiling");
    }

    #[test]
    fn token_bucket_burst_exceeds_sustained_rate() {
        // A burst larger than the per-second rate is allowed at once (honest spike headroom), but
        // the SUSTAINED rate still bounds the long run.
        let mut b = TokenBucket::new(30.0, 60.0, 0.0); // 30/sec sustained, 60 burst
        for _ in 0..60 {
            assert!(b.allow(0.0));
        }
        assert!(!b.allow(0.0), "burst capacity is 60, the 61st is throttled");
    }

    #[test]
    fn log_batch_burst_covers_full_client_drain() {
        // The end-of-match FlushAllAsync drain is up to MaxBuffered(1000)/MaxBatch(50) = 20
        // back-to-back batches in well under a second. The burst must pass ALL of them —
        // shedding any would silently lose exactly the end-of-match death/claim telemetry.
        let mut rl = GameplayRateLimiter::new(30, 250, 0);
        for i in 0..20 {
            assert!(rl.allow_log_batch(0), "drain batch {i} must pass the burst");
        }
    }

    #[test]
    fn ping_limit_derives_from_config_interval() {
        // 250ms cadence → 4/sec honest → 10/sec sustained (2.5× headroom) with a 2× burst (20).
        // Pins the config derivation so a cadence tune moves the limit with it.
        let mut rl = GameplayRateLimiter::new(30, 250, 0);
        for i in 0..20 {
            assert!(rl.allow_ping(0), "burst ping {i} allowed");
        }
        assert!(!rl.allow_ping(0), "21st ping in the same instant is throttled");
        // One second later the sustained rate has refilled 10 tokens.
        for i in 0..10 {
            assert!(rl.allow_ping(1000), "sustained ping {i} after 1s");
        }
        assert!(!rl.allow_ping(1000));
    }

    #[test]
    fn token_bucket_zero_rate_is_unlimited() {
        // A config rate of 0 disables the limit (unlimited) rather than blocking everything.
        let mut b = TokenBucket::new(0.0, 0.0, 0.0);
        for _ in 0..1000 {
            assert!(b.allow(0.0));
        }
    }

    #[test]
    fn message_class_budgets_are_independent() {
        // Every class has its OWN bucket: flooding moves, eat claims, and probes must NOT shed a
        // single gameplay-critical EnemyDeathClaim, nor a resync. (round-3 follow-up + review #3)
        let mut rl = GameplayRateLimiter::new(30, 250, 0);
        // Drain moves, eat claims, probes, pings, and log batches to empty at the same instant.
        for class in ["move", "eat", "probe", "ping", "logbatch"] {
            let mut n = 0;
            while match class {
                "move" => rl.allow_move(0),
                "eat" => rl.allow_eat_claim(0),
                "probe" => rl.allow_probe(0),
                "ping" => rl.allow_ping(0),
                _ => rl.allow_log_batch(0),
            } {
                n += 1;
                assert!(n < 10_000, "{class} bucket must be finite");
            }
        }
        // The death-claim and resync buckets are untouched by all the floods above (including the
        // new ping / log-batch classes — a ping or telemetry flood must not shed a death claim).
        for _ in 0..DEATH_CLAIM_RATE_LIMIT_PER_SEC as u32 {
            assert!(rl.allow_death_claim(0), "death claims unaffected by move/eat/probe floods");
        }
        assert!(!rl.allow_death_claim(0), "death-claim budget exhausts independently");
        for _ in 0..RESYNC_RATE_LIMIT_PER_SEC as u32 {
            assert!(rl.allow_resync(0), "resyncs unaffected by move/eat/probe floods");
        }
        assert!(!rl.allow_resync(0), "resync budget exhausts independently");
    }

    #[test]
    fn ws_auth_messages_share_a_tight_isolated_budget() {
        let mut rl = GameplayRateLimiter::new(30, 250, 0);
        // The honest flow (SignInWithApple-needs-nickname + SetNickname) is 2 tokens; the
        // burst tolerates a few retries, then the slow refill caps a flood.
        for i in 0..AUTH_MSG_BURST as u32 {
            assert!(rl.allow_auth(0), "burst auth message {i} allowed");
        }
        assert!(!rl.allow_auth(0), "auth flood is shed past the burst");
        // Gameplay/diagnostic budgets are untouched by an auth flood (own bucket).
        assert!(rl.allow_move(0) && rl.allow_ping(0), "auth flood doesn't starve other classes");
        // The slow refill (0.5/sec) hands back one token after ~2s, not sooner.
        assert!(!rl.allow_auth(1000), "no auth token after only 1s");
        assert!(rl.allow_auth(2000), "one auth token refilled after ~2s");
    }
}
