# Security audit — 2026-06-11

Method: 7-dimension parallel audit (auth/session, transport/DoS, gameplay
authority/anti-cheat, data/secrets, input/panics, ops/deploy, client/protocol)
of the Rust backend + Unity client, each finding adversarially verified by two
independent reviewers (reachability lens + impact lens). 36 raw findings → **15
confirmed** (3 high, 5 medium, 7 low), 21 refuted. The confirmed set
de-duplicates to **8 themes** below (agents named the same issue from several
angles).

Trust model used: clients are UNTRUSTED (any peer can send any ClientMessage);
the public edge is wss:// through Caddy; `/ops` is behind Caddy basic_auth;
gameplay is server-authoritative.

## Bottom line

**No anti-cheat / gameplay-authority hole was found.** The whole eat/death-claim
+ filler-eat path held up under adversarial review — a client cannot forge a kill
of another player, inflate its own score, cross rooms, or replay stale claims;
every "can a client cheat the game" finding was REFUTED. No SQL injection, no
client-input panic in the request path, no memory-safety issue. The real issues
are **operational hardening around the brand-new auth + the missing edge
rate-limiting**, not gameplay integrity.

None of these block a small trusted technical playtest. Items 1–4 should be
closed before a real invite wave.

---

## Confirmed themes (priority order)

### 1. test_token backdoor is live on the dev droplet — HIGH (dev), and there's no guard against it reaching prod
`docker-compose.yml:32-34`, `src/auth.rs:136-144`

The dev droplet runs `FAIRTICK_AUTH_MODE=apple` **with**
`FAIRTICK_ALLOW_TEST_TOKENS=1`. Any untrusted peer can send
`SignInWithApple { apple_token: "test_token_<anyone>" }` and is authenticated as
that user id with zero verification — full impersonation of any existing account.
This is intentional for bot_runner soaks, but **nothing stops the same flag from
silently surviving into a prod contour** (it's just an env var; absence = off,
but presence isn't fatal).

- **Fix:** make `AUTH_MODE=apple` + `ALLOW_TEST_TOKENS=1` a **fatal boot error**
  unless an explicit `FAIRTICK_DEV=1` is also set. Cheap, closes the prod-leak
  path. On dev it stays on behind that extra flag.

### 2. No per-IP rate limiting — a single IP can exhaust the global 150-cap — HIGH→MEDIUM
`Caddyfile:34-38` (acknowledged TODO), `src/network/websocket.rs:173-179`

The connection cap is **global** (150), not per-IP, and there's no idle timeout.
One attacker opens 150 WebSocket connections from one IP, holds them with
rate-compliant Pings, and starves every real player with `503 at capacity` — no
auth required (the cap is checked before the handshake). Severity is HIGH on dev
(test tokens make it trivial), MEDIUM on a real-auth prod (still works to the
pre-auth connection cap; auth isn't required to hold a socket).

- **Fix:** per-IP limits at the edge (the `caddy-ratelimit` plugin the TODO already
  names: ~5 concurrent + N upgrades/min per IP), and/or a per-IP counter in
  `ws_handler` via `axum::ConnectInfo`. Add a pre-auth idle timeout so a silent
  socket can't be held open indefinitely.

### 3. `InsecurePassthrough` is the DEFAULT auth mode — MEDIUM
`src/main.rs:108-135`, `src/auth.rs:157-171`

If `FAIRTICK_AUTH_MODE` is unset, the server accepts **any token string** as a
user id (it only warns). A forgotten env var on a new deployment = the entire
auth system silently off. Safer to fail closed.

- **Fix:** default to refusing auth (or require an explicit
  `FAIRTICK_AUTH_MODE=insecure` to opt into passthrough) so a missing var is a
  loud boot failure, not a silent bypass.

### 4. Session tokens: no expiry/revocation + plaintext in PlayerPrefs — MEDIUM/HIGH
`src/db.rs:145-155, 306-346`, `Unity GameClient.Lifecycle.cs:56`, `MessageHandlers.cs:163`

Two halves of one weakness. Server side: `auth_sessions` rows never expire and
there's no revocation path — a leaked token is valid forever (the `last_used_at`
column exists but nothing sweeps it). Client side: the token is stored in
**plaintext PlayerPrefs** (plist on iOS, SharedPreferences on Android) — physical
access or an unencrypted backup yields a permanent account-takeover token.
Mitigated by server-authoritative gameplay (a stolen token can't cheat the game,
only impersonate) and TLS in transit, so it's not CRITICAL.

- **Fix:** add `expires_at` (e.g. 90 days) + an idle sweep + a revoke-on-demand
  path server-side; store the token in **Keychain (iOS) / KeyStore (Android)**
  client-side, and bind sessions per-device.

### 5. Apple nonce is unimplemented — replay window — LOW/MEDIUM
`Unity MessageHandlers.cs:139,149,153` (sends `nonce=""`), `src/network/websocket.rs:882`, `src/auth.rs`

The client sends an **empty nonce** and the server only logs `nonce_len`, never
validating it against the JWT's `nonce` claim. So a captured Apple identity token
(device/backup extraction; not network, since TLS) can be **replayed**. Real but
narrow — the JWT still has to verify against Apple's JWKS and isn't long-lived.

- **Fix:** client generates a random nonce, sends `SHA256(nonce)`; server requires
  the JWT `nonce` claim to equal it (and reject empty) in `Verify` mode.

### 6. `/ops` bcrypt hash committed to the repo — LOW
`Caddyfile:27`

The dashboard password's bcrypt hash is in version control (and git history). The
comment is right that it costs an offline brute-force of a 24-char random
password — so low — but it's still a secret in the repo, and the impact is only
observability metrics (no gameplay/account access).

- **Fix:** rotate, move to a gitignored env-substituted value / GitHub Secret.

### 7. Deploy SSH runs as root — LOW
`.github/workflows/deploy.yml` (`DEPLOY_USER=root`)

A compromised `DEPLOY_SSH_KEY` gives direct root on the droplet. Only exploitable
*after* GitHub write/secret access is already compromised, so low — but a
least-privilege deploy user with a command-restricted key shrinks the blast
radius.

### 8. ClientLogBatch burst can briefly spike telemetry I/O — LOW
`src/network/websocket.rs:639-650`

Rate-limited (5/s sustained, burst 24), but 24 large batches can pass before
shedding. No security impact beyond a momentary disk-I/O blip; just monitor the
burst size and shrink if telemetry I/O ever shows up under load.

---

## Notable REFUTED claims (checked and dismissed)

Verifiers rejected these — recorded so they aren't re-raised:

- **"A client can forge eat/death claims, inflate score, kill across rooms, replay
  stale claims"** — refuted. Server-side validation (generation/life checks,
  per-room scoping, contact-history reconstruction, the claim ledger) holds.
- **SQL injection in migrations / pragma queries** — refuted; queries are
  parameterized and the `format!` sites use compile-time constants only.
- **Client-input panic in the request path / malformed bincode crashes the
  decoder** — refuted; no attacker-reachable unwrap/index found in the hot path.
- **Pre-auth Ping → room lock contention DoS** — refuted (Pings don't touch room
  locks pre-auth).
- **JWKS refetch flood → Apple DDoS** — refuted; the 30s refetch rate-limit holds.
- **SQLite world-readable in container / container-as-root → data theft** —
  refuted as a *remote* vuln (needs shell on the box first; tracked under item 7
  as defense-in-depth).
- **Concurrent apple_user_id registration race → account takeover** — refuted.
- Several client-side "stored in PlayerPrefs" / "token in plaintext wire field"
  duplicates folded into item 4 or dismissed (the wire is TLS; the at-rest issue
  is the real one).

---

## Suggested order of work

1. Fatal-guard `AUTH_MODE=apple + ALLOW_TEST_TOKENS` unless `DEV=1` (item 1) — minutes.
2. Default auth to fail-closed (item 3) — minutes.
3. Session `expires_at` + sweep + revoke (item 4 server half) — small.
4. Per-IP edge rate limit + idle timeout (item 2) — needs the custom Caddy build.
5. Keychain/KeyStore for the session token (item 4 client half) — native plugin.
6. Apple nonce end-to-end (item 5); rotate the `/ops` secret out of the repo (item 6).

---

## Remediation status (2026-06-11, branch feature/filler-bots)

All confirmed items fixed except #7 (accepted, see below). Commits:

| Item | Status | Where |
|---|---|---|
| 1 — test_token guard | **FIXED** | `main.rs::resolve_auth_mode` — `apple+ALLOW_TEST_TOKENS` fatal without `FAIRTICK_DEV=1` |
| 2 — per-IP cap + idle timeout | **FIXED** | `websocket.rs` `PerIpGuard` (default 8/IP, dev=0) + 20s pre-auth idle timeout |
| 3 — fail-closed auth default | **FIXED** | `main.rs` — `FAIRTICK_AUTH_MODE` required; `insecure` needs `DEV=1` |
| 4 — session expiry/revoke (server) | **FIXED** | `db.rs` migration 003 `expires_at` + lazy reap + hourly sweep + `revoke_user_sessions` |
| 4 — session at-rest (client) | **FIXED** | iOS Keychain (`SecureStorageNative.mm` + `SecureStorage` seam); Android = PlayerPrefs fallback (TODO: Keystore when Android ships) |
| 5 — Apple nonce | **FIXED** | client random nonce → native `request.nonce=SHA256` → wire raw nonce; server requires JWT `nonce` claim == `SHA256(wire)` |
| 6 — `/ops` secret in repo | **FIXED** | Caddyfile reads `{$OPS_BASIC_AUTH_HASH}` from gitignored `ops.env` on the droplet; password rotated |
| 7 — root SSH deploy | **ACCEPTED** | see below |
| 8 — ClientLogBatch burst | **ACCEPTED** | no security impact; the audit's own fix was "monitor, shrink if I/O bottlenecks" — left as-is |

**Item 7 (root deploy) — accepted, not code-fixable here.** The real fix is rotating
the GitHub `DEPLOY_SSH_KEY` secret to a least-privilege `deploy` user's key — that
needs repo-admin access to set the secret (the user's, not automatable), and a
docker-group deploy user is ≈root anyway (docker socket = root). Proper paths:
rootless Docker on the droplet, or a `command="..."`-restricted authorized_keys entry.
Tracked as a known, low-severity operational item; the blast radius only opens *after*
GitHub write/secret access is already compromised.

**Note on dev vs prod:** the dev droplet (`docker-compose.yml`) keeps `FAIRTICK_DEV=1`
+ test tokens + per-IP disabled — it doubles as the soak target, so the public
`api-dev` endpoint accepts `test_token_*` by design (the dev box is not a security
boundary). A real beta/prod contour MUST run with `FAIRTICK_AUTH_MODE=apple` and
WITHOUT `FAIRTICK_DEV` / `FAIRTICK_ALLOW_TEST_TOKENS` — the server then boots
`test_tokens=off` (the code is fail-closed: apple mode + test tokens without DEV
refuses to boot), and `FAIRTICK_MAX_CONNECTIONS_PER_IP` defaults to 8. How that
contour is provisioned (separate droplet, env file, etc.) is a deployment decision for
when the beta actually stands up — not pre-built here.
