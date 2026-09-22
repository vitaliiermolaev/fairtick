use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use std::str::FromStr;
use tracing::{error, info, warn};
use uuid::Uuid;

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

pub async fn init_db(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    // Connection pragmas applied to EVERY pooled connection (beta hardening): WAL so a reader
    // (join keyframe / RequestFullState) doesn't block the per-tick writer; busy_timeout so a
    // momentary lock RETRIES instead of erroring under load; synchronous=NORMAL (the recommended
    // pairing with WAL) for throughput while staying crash-durable. A `sqlite::memory:` url keeps
    // its in-memory journal — these become no-ops there, not errors.
    //
    // Deliberately NO unconditional `.create_if_missing(true)`: `from_str` already honors
    // `?mode=rwc` in the URL (the dev/compose urls carry it), so create-on-missing stays an
    // EXPLICIT per-URL opt-in. An unconditional create would turn a mistyped path or an
    // unmounted data volume into a silently-created empty database that migrates, reports
    // ready, and "loses" every account — instead of a loud startup failure ops can act on.
    //
    // foreign_keys is intentionally left OFF: the db was written with enforcement off, so turning
    // it on could surface latent violations as runtime insert errors — a correctness decision to
    // make deliberately (with a data audit), not a stability quick-win.
    let opts = SqliteConnectOptions::from_str(database_url)?
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new().max_connections(5).connect_with(opts).await?;

    // Run migrations
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY,
            apple_user_id TEXT UNIQUE NOT NULL,
            -- nickname uniqueness (case-insensitive) is enforced by idx_users_nickname_ci,
            -- added in migration 004/005 (a column-level UNIQUE can't carry COLLATE NOCASE).
            nickname TEXT NOT NULL,
            crystals INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS game_stats (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id TEXT NOT NULL,
            wins INTEGER NOT NULL DEFAULT 0,
            games_played INTEGER NOT NULL DEFAULT 0,
            total_kills INTEGER NOT NULL DEFAULT 0,
            FOREIGN KEY (user_id) REFERENCES users(id)
        )
        "#,
    )
    .execute(&pool)
    .await?;

    // Reward outbox. Once a grant row is committed here (enqueue_rewards, one transaction),
    // it survives a crash/redeploy and `drain_pending_rewards` (startup replay + periodic
    // worker + immediate post-match drain) applies it idempotently. `reward_id` is the
    // idempotency key ({room_id}:{user_id}) — INSERT OR IGNORE makes a replayed match outcome
    // a no-op, and the credit is gated on status='pending' so a reward is credited at most
    // once. status: pending -> applied | failed (user gone; `failed_reason` says why).
    //
    // DURABILITY BOUNDARY (honest): durable AFTER enqueue commits. The match-end → enqueue
    // step is a spawned task, so a crash in the gap between Room::update() ending the match
    // and this row committing still loses that grant (nothing to replay). Closing that needs
    // enqueue-before-GameEnded; deferred for the beta.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS reward_outbox (
            reward_id     TEXT PRIMARY KEY,
            user_id       TEXT NOT NULL,
            crystals      INTEGER NOT NULL CHECK (crystals >= 0),
            status        TEXT NOT NULL DEFAULT 'pending'
                              CHECK (status IN ('pending', 'applied', 'failed')),
            attempts      INTEGER NOT NULL DEFAULT 0,
            created_at    INTEGER NOT NULL,
            applied_at    INTEGER,
            failed_reason TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_reward_outbox_pending ON reward_outbox(status)")
        .execute(&pool)
        .await?;

    // Patch DBs created by an OLDER build. `CREATE TABLE IF NOT EXISTS` above builds the CURRENT
    // shape on a FRESH db, but it is a no-op against a table that already exists with a stale
    // shape — so a column added after the table's first creation would be silently missing on
    // the droplet. run_migrations closes that gap idempotently. (review Blocker 3)
    run_migrations(&pool).await?;

    Ok(pool)
}

/// Apply additive schema migrations idempotently. Each migration runs at most once (recorded in
/// `schema_migrations`); column-adds are ALSO guarded by a pragma check so they're safe even on a
/// fresh db where `CREATE TABLE` already included the column. APPEND-ONLY: never edit or reorder
/// an existing entry — add a new one. This is the "why does the dev db work but the droplet's
/// doesn't" insurance: schema evolution stops depending on `CREATE TABLE IF NOT EXISTS` noticing
/// new columns (it doesn't). (review Blocker 3 — reward_outbox grew `failed_reason`.)
pub(crate) async fn run_migrations(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    TEXT PRIMARY KEY,
            applied_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await?;

    // 001: reward_outbox.failed_reason — added when apply_pending_reward started recording WHY a
    // grant was downgraded to `failed` (user gone). A pre-this table lacks it, so the UPDATE that
    // writes failed_reason would error on every drain sweep against an old db.
    if !migration_applied(pool, "001_reward_outbox_failed_reason").await? {
        ensure_column(pool, "reward_outbox", "failed_reason", "TEXT").await?;
        record_migration(pool, "001_reward_outbox_failed_reason").await?;
    }

    // 002: auth_sessions — server-issued long-lived session tokens (Sign in with Apple).
    // Apple identity tokens expire in minutes, so after one verified sign-in the server
    // hands the client its OWN token; reconnects present it instead of re-running the
    // native (Face ID) flow. Only the SHA-256 of the token is stored — a leaked DB must
    // not be a bag of bearer tokens.
    if !migration_applied(pool, "002_auth_sessions").await? {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS auth_sessions (
                token_hash   TEXT PRIMARY KEY,
                user_id      TEXT NOT NULL,
                created_at   INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL
            )",
        )
        .execute(pool)
        .await?;
        record_migration(pool, "002_auth_sessions").await?;
    }

    // 003: auth_sessions.expires_at — sessions must EXPIRE (security audit item 4). A
    // leaked token shouldn't be valid forever. Backfill existing rows to created_at +
    // the default lifetime so no live session is invalidated mid-beta; new rows get a
    // real expiry from create_auth_session.
    if !migration_applied(pool, "003_auth_sessions_expires_at").await? {
        ensure_column(pool, "auth_sessions", "expires_at", "INTEGER").await?;
        sqlx::query("UPDATE auth_sessions SET expires_at = created_at + ? WHERE expires_at IS NULL")
            .bind(SESSION_LIFETIME_SECS)
            .execute(pool)
            .await?;
        record_migration(pool, "003_auth_sessions_expires_at").await?;
    }

    // 004: users.nickname UNIQUE (case-insensitive). Nicknames went from "display name"
    // back to unique identity (Settings rename + availability check). The app-level check
    // is the friendly 409 path; this index is the belt against a TOCTOU race between that
    // check and the write. Case-insensitive duplicates from the no-uniqueness era are deduped
    // FIRST (oldest keeps the bare name) so the index can't fail the boot.
    //
    // APPEND-ONLY: this entry is intentionally left at its ORIGINAL behavior (dedupe-only).
    // The stronger CANONICALIZATION (trim + strip control/zero-width/bidi) was added LATER as
    // a SEPARATE migration (005) rather than by editing 004 in place — a DB that already
    // recorded 004 would never re-run an edited 004, so the new behavior has to be its own id.
    if !migration_applied(pool, "004_users_nickname_ci_unique").await? {
        dedupe_nicknames_ci(pool).await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_nickname_ci ON users(nickname COLLATE NOCASE)",
        )
        .execute(pool)
        .await?;
        record_migration(pool, "004_users_nickname_ci_unique").await?;
    }

    // 005: canonicalize legacy nicknames to the live write-path shape (trim, strip
    // control/zero-width/bidi, truncate) so the UNIQUE index can't keep visually-identical
    // twins (" Bob " vs "Bob", "Bob\u{200B}" vs "Bob") that 004's dedupe-only pass left
    // behind (NOCASE treats them as distinct). DROP the index first: a canonicalize UPDATE
    // (" Bob " -> "Bob") can collide with an existing "Bob", which the live index would
    // reject — so normalize WITHOUT the constraint, then rebuild it on the cleaned data.
    if !migration_applied(pool, "005_users_nickname_canonical_shape").await? {
        sqlx::query("DROP INDEX IF EXISTS idx_users_nickname_ci").execute(pool).await?;
        normalize_and_dedupe_nicknames_ci(pool).await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_nickname_ci ON users(nickname COLLATE NOCASE)",
        )
        .execute(pool)
        .await?;
        record_migration(pool, "005_users_nickname_canonical_shape").await?;
    }

    Ok(())
}

/// Rename case-insensitive duplicate nicknames so a UNIQUE(nickname COLLATE NOCASE) index can
/// be created without failing the boot. The oldest account (by created_at, then id) keeps the
/// bare name; each later collision gets a deterministic "<base>#<short-id>" suffix. This is
/// migration 004's ORIGINAL dedupe-only pass — kept byte-stable for append-only correctness;
/// migration 005 ([`normalize_and_dedupe_nicknames_ci`]) does the stronger canonicalization.
async fn dedupe_nicknames_ci(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, nickname FROM users ORDER BY created_at ASC, id ASC")
            .fetch_all(pool)
            .await?;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (id, nick) in rows {
        if seen.insert(nick.to_lowercase()) {
            continue; // first (oldest) holder keeps the bare name
        }
        let short = &id[..id.len().min(4)]; // UUIDs are ASCII hex — slicing is char-safe
        let mut attempt = 0u32;
        let candidate = loop {
            let suffix = if attempt == 0 { format!("#{short}") } else { format!("#{short}{attempt}") };
            let keep = 30usize.saturating_sub(suffix.chars().count());
            let base: String = nick.chars().take(keep).collect();
            let cand = format!("{base}{suffix}");
            if seen.insert(cand.to_lowercase()) {
                break cand;
            }
            attempt += 1;
        };
        sqlx::query("UPDATE users SET nickname = ? WHERE id = ?")
            .bind(&candidate)
            .bind(&id)
            .execute(pool)
            .await?;
        // Debug-escape the values (`{:?}`): a pre-validation legacy nick can carry
        // newlines/control chars, and migration logs are read exactly during an incident.
        info!("schema migration 004: deduped nickname {nick:?} -> {candidate:?} (user {id})");
    }
    Ok(())
}

/// Bring legacy nicknames into stored shape and remove case-insensitive duplicates so a
/// UNIQUE(nickname COLLATE NOCASE) index can be created without failing the boot. This is
/// migration 005's pass — run AFTER the index is dropped, then the index is rebuilt.
///
/// Two coupled steps per row (oldest first, by created_at then id):
///   1. CANONICALIZE the value the SAME way the live write path stores it
///      ([`crate::nickname::canonicalize_legacy`]): trim, drop control/zero-width/bidi
///      chars, truncate. Without this the index would still admit " Bob " alongside "Bob",
///      or "Bob\u{200B}" alongside "Bob" — visually one identity, two rows.
///   2. DEDUPE case-insensitively using ASCII folding (`to_ascii_lowercase`), matching
///      SQLite's NOCASE collation exactly (NOCASE is ASCII-only — folding with the full
///      Unicode `to_lowercase` here would be STRICTER than the index and rename pairs the
///      index would happily allow). The oldest holder keeps the bare name; each later
///      collision gets a deterministic "<base>#<short-id>" suffix, truncated to fit 30
///      chars and re-checked against the running set.
///
/// Any row whose stored value changes is written back and logged with debug-escaped values
/// (`{:?}`) — a pre-validation legacy nick could contain newlines/control chars, which must
/// not ride raw into the log (injection). No-op for a row already in canonical, unique shape
/// (the common case).
async fn normalize_and_dedupe_nicknames_ci(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, nickname FROM users ORDER BY created_at ASC, id ASC")
            .fetch_all(pool)
            .await?;
    let max = crate::nickname::MAX_NICKNAME_CHARS;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (id, raw) in rows {
        let canonical = crate::nickname::canonicalize_legacy(&raw);
        let final_nick = if seen.insert(canonical.to_ascii_lowercase()) {
            canonical // first (oldest) holder keeps the canonical name
        } else {
            let short = &id[..id.len().min(4)]; // UUIDs are ASCII hex — slicing is char-safe
            let mut attempt = 0u32;
            loop {
                let suffix = if attempt == 0 { format!("#{short}") } else { format!("#{short}{attempt}") };
                let keep = max.saturating_sub(suffix.chars().count());
                let base: String = canonical.chars().take(keep).collect();
                let cand = format!("{base}{suffix}");
                if seen.insert(cand.to_ascii_lowercase()) {
                    break cand;
                }
                attempt += 1;
            }
        };
        if final_nick != raw {
            sqlx::query("UPDATE users SET nickname = ? WHERE id = ?")
                .bind(&final_nick)
                .bind(&id)
                .execute(pool)
                .await?;
            info!("schema migration 005: nickname {raw:?} -> {final_nick:?} (user {id})");
        }
    }
    Ok(())
}

/// Session lifetime (90 days). Long enough that a real player rarely re-signs-in,
/// short enough that a leaked token eventually dies on its own. The idle sweep
/// (`sweep_expired_sessions`) drops rows past this.
pub const SESSION_LIFETIME_SECS: i64 = 90 * 24 * 60 * 60;

async fn migration_applied(pool: &SqlitePool, version: &str) -> Result<bool, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as("SELECT version FROM schema_migrations WHERE version = ?")
        .bind(version)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

async fn record_migration(pool: &SqlitePool, version: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?, ?)")
        .bind(version)
        .bind(now_secs())
        .execute(pool)
        .await?;
    Ok(())
}

/// Add `column` to `table` only if it isn't already there (so it's safe on a fresh db where the
/// `CREATE TABLE` already defined it). `table`/`column`/`decl` are compile-time constants from
/// run_migrations, never user input — no injection surface despite the format!.
async fn ensure_column(pool: &SqlitePool, table: &str, column: &str, decl: &str) -> Result<(), sqlx::Error> {
    let cols: Vec<(String,)> =
        sqlx::query_as(&format!("SELECT name FROM pragma_table_info('{table}')")).fetch_all(pool).await?;
    if !cols.iter().any(|(name,)| name == column) {
        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}")).execute(pool).await?;
        info!("schema migration: added column {table}.{column} {decl}");
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    #[allow(dead_code)]
    pub apple_user_id: String,
    pub nickname: String,
    pub crystals: i64,
}

pub async fn create_user(
    pool: &SqlitePool,
    apple_user_id: &str,
    nickname: &str,
) -> Result<User, sqlx::Error> {
    let id = Uuid::new_v4().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;

    sqlx::query(
        "INSERT INTO users (id, apple_user_id, nickname, crystals, created_at) VALUES (?, ?, ?, 0, ?)",
    )
    .bind(&id)
    .bind(apple_user_id)
    .bind(nickname)
    .bind(now)
    .execute(pool)
    .await?;

    Ok(User { id, apple_user_id: apple_user_id.to_string(), nickname: nickname.to_string(), crystals: 0 })
}

pub async fn get_user_by_apple_id(
    pool: &SqlitePool,
    apple_user_id: &str,
) -> Result<Option<User>, sqlx::Error> {
    let user = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, apple_user_id, nickname, crystals FROM users WHERE apple_user_id = ?",
    )
    .bind(apple_user_id)
    .fetch_optional(pool)
    .await?;

    Ok(user.map(|(id, apple_user_id, nickname, crystals)| User { id, apple_user_id, nickname, crystals }))
}

/// True if no other account holds this nickname. CASE-INSENSITIVE (`COLLATE NOCASE`)
/// so "Bob" and "bob" can't both exist — a display name doubles as how players tell
/// each other apart, and case-only twins read as impersonation. (NOCASE is ASCII-only
/// folding, which is fine for the beta; revisit if we need full-Unicode case folding.)
/// This is the FRIENDLY fast path (a 409 before the write); the UNIQUE index
/// `idx_users_nickname_ci` is the authority that closes the TOCTOU window — a check that
/// races still loses at the index, mapped back to NicknameTaken (not a duplicate row).
pub async fn check_nickname_available(pool: &SqlitePool, nickname: &str) -> Result<bool, sqlx::Error> {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE nickname = ? COLLATE NOCASE")
        .bind(nickname)
        .fetch_one(pool)
        .await?;

    Ok(count.0 == 0)
}

/// Like [`check_nickname_available`] but ignores `exclude_user_id` — so a user "keeping"
/// their own nickname (or only changing its case) sees it as available.
pub async fn check_nickname_available_excluding_user(
    pool: &SqlitePool,
    nickname: &str,
    exclude_user_id: &str,
) -> Result<bool, sqlx::Error> {
    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE nickname = ? COLLATE NOCASE AND id != ?")
            .bind(nickname)
            .bind(exclude_user_id)
            .fetch_one(pool)
            .await?;

    Ok(count.0 == 0)
}

pub async fn get_user(pool: &SqlitePool, user_id: &str) -> Result<Option<User>, sqlx::Error> {
    let user = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, apple_user_id, nickname, crystals FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;

    Ok(user.map(|(id, apple_user_id, nickname, crystals)| User { id, apple_user_id, nickname, crystals }))
}

/// Issue a session token for `user_id`: 32 random bytes, hex-encoded. Only the
/// SHA-256 of the token is stored — the plaintext exists exactly once, in the
/// AuthSuccess reply to the device that earned it. Provider-agnostic: the same
/// table serves Apple today and Google/Android tomorrow. Carries a hard expiry
/// (audit item 4) so a leaked token can't live forever.
pub async fn create_auth_session(pool: &SqlitePool, user_id: &str) -> Result<String, sqlx::Error> {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let now = now_secs();
    sqlx::query(
        "INSERT INTO auth_sessions (token_hash, user_id, created_at, last_used_at, expires_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(session_token_hash(&token))
    .bind(user_id)
    .bind(now)
    .bind(now)
    .bind(now + SESSION_LIFETIME_SECS)
    .execute(pool)
    .await?;
    Ok(token)
}

/// Resolve a presented session token to its user. None = unknown OR EXPIRED token
/// (an expired row is treated as absent and deleted). Touches last_used_at on a hit.
pub async fn get_user_by_session_token(pool: &SqlitePool, token: &str) -> Result<Option<User>, sqlx::Error> {
    let hash = session_token_hash(token);
    let now = now_secs();
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT user_id, expires_at FROM auth_sessions WHERE token_hash = ?")
            .bind(&hash)
            .fetch_optional(pool)
            .await?;
    let Some((user_id, expires_at)) = row else { return Ok(None) };
    if now >= expires_at {
        // Lazily reap the dead row; the periodic sweep handles the ones never presented.
        sqlx::query("DELETE FROM auth_sessions WHERE token_hash = ?").bind(&hash).execute(pool).await?;
        return Ok(None);
    }
    sqlx::query("UPDATE auth_sessions SET last_used_at = ? WHERE token_hash = ?")
        .bind(now)
        .bind(&hash)
        .execute(pool)
        .await?;
    get_user(pool, &user_id).await
}

/// Delete all expired sessions. Run periodically (the boot/worker loop) so rows
/// that are never presented again don't accumulate. Returns rows removed.
pub async fn sweep_expired_sessions(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let res =
        sqlx::query("DELETE FROM auth_sessions WHERE expires_at <= ?").bind(now_secs()).execute(pool).await?;
    Ok(res.rows_affected())
}

/// Revoke EVERY session for a user (e.g. a "sign out everywhere" / lost-device
/// action). Returns rows removed. The next connect from any device re-runs the
/// provider sign-in. (audit item 4 — revocation path)
pub async fn revoke_user_sessions(pool: &SqlitePool, user_id: &str) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM auth_sessions WHERE user_id = ?").bind(user_id).execute(pool).await?;
    Ok(res.rows_affected())
}

/// Sign out THIS device: drop the single presented session, leaving the user's other
/// devices signed in (unlike `revoke_user_sessions`). Idempotent — returns rows removed
/// (0 if the token was already unknown/expired). (audit item 4 — revocation path)
pub async fn delete_session_by_token(pool: &SqlitePool, token: &str) -> Result<u64, sqlx::Error> {
    let res = sqlx::query("DELETE FROM auth_sessions WHERE token_hash = ?")
        .bind(session_token_hash(token))
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

fn session_token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

pub async fn update_user_nickname(
    pool: &SqlitePool,
    user_id: &str,
    nickname: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET nickname = ? WHERE id = ?")
        .bind(nickname)
        .bind(user_id)
        .execute(pool)
        .await?;

    Ok(())
}

/// A match-end reward to persist durably. `reward_id` is the idempotency key
/// ({room_id}:{user_id}); `user_id` is the `users.id` to credit.
#[derive(Debug, Clone)]
pub struct OutboxReward {
    pub reward_id: String,
    pub user_id: String,
    pub crystals: i64,
}

/// Outcome of applying one outbox row, for logging/metrics by the drain.
pub enum RewardApply {
    /// This call credited the user.
    Applied { user_id: String, crystals: i64 },
    /// The row was claimed but the user no longer exists — marked `failed`, not credited.
    UserMissing { user_id: String },
    /// Nothing to do: already applied/failed, or a concurrent drain claimed it first.
    Skipped,
}

/// Durably record match-end reward intents in ONE transaction. THIS is the durable
/// point: once it commits, every grant survives a crash/redeploy — the drain
/// (startup replay + periodic worker + immediate post-match) applies them idempotently.
/// `INSERT OR IGNORE` on the `reward_id` PK makes a replayed match outcome a no-op, so
/// the same match can never enqueue a grant twice.
pub async fn enqueue_rewards(pool: &SqlitePool, rewards: &[OutboxReward]) -> Result<(), sqlx::Error> {
    if rewards.is_empty() {
        return Ok(());
    }
    let now = now_secs();
    let mut tx = pool.begin().await?;
    for r in rewards {
        sqlx::query(
            "INSERT OR IGNORE INTO reward_outbox \
             (reward_id, user_id, crystals, status, attempts, created_at) \
             VALUES (?, ?, ?, 'pending', 0, ?)",
        )
        .bind(&r.reward_id)
        .bind(&r.user_id)
        .bind(r.crystals)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Apply ONE pending reward atomically and idempotently. The claim is a conditional
/// `UPDATE ... WHERE status='pending'`: only one caller's update matches the row, so a
/// concurrent or replayed apply sees 0 rows and skips (credited at most once). The
/// credit to `users.crystals` happens in the SAME transaction as the status flip, so
/// they commit or roll back together — a crash mid-apply leaves the row `pending` for
/// the next drain. If the user row is gone the grant is marked `failed` (surfaced, not
/// silently swallowed).
pub async fn apply_pending_reward(pool: &SqlitePool, reward_id: &str) -> Result<RewardApply, sqlx::Error> {
    let now = now_secs();
    let mut tx = pool.begin().await?;

    // Atomic claim: flip pending -> applied. A racing drain gets rows_affected()==0.
    let claimed = sqlx::query(
        "UPDATE reward_outbox SET status='applied', applied_at=?, attempts=attempts+1 \
         WHERE reward_id=? AND status='pending'",
    )
    .bind(now)
    .bind(reward_id)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(RewardApply::Skipped);
    }

    let (user_id, crystals): (String, i64) =
        sqlx::query_as("SELECT user_id, crystals FROM reward_outbox WHERE reward_id=?")
            .bind(reward_id)
            .fetch_one(&mut *tx)
            .await?;

    let credited = sqlx::query("UPDATE users SET crystals = crystals + ? WHERE id = ?")
        .bind(crystals)
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;
    if credited.rows_affected() != 1 {
        // User vanished — don't claim success. Downgrade to terminal `failed` (same tx) with a
        // reason, so this stops retrying and is visible for manual reconcile beyond the log.
        sqlx::query(
            "UPDATE reward_outbox SET status='failed', applied_at=?, failed_reason=? WHERE reward_id=?",
        )
        .bind(now)
        .bind("user_id not found when crediting")
        .bind(reward_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(RewardApply::UserMissing { user_id });
    }

    tx.commit().await?;
    Ok(RewardApply::Applied { user_id, crystals })
}

/// Apply all currently-`pending` rewards idempotently. Called at startup (replay after a
/// crash/redeploy), right after each match enqueues, and on a periodic timer — so a
/// transient DB outage that outlasts the immediate attempt is recovered without a
/// restart. Bounded (`LIMIT`) so one sweep can't run unboundedly. Returns the number
/// credited by THIS sweep.
pub async fn drain_pending_rewards(pool: &SqlitePool) -> u64 {
    let ids: Vec<(String,)> = match sqlx::query_as(
        "SELECT reward_id FROM reward_outbox WHERE status='pending' ORDER BY created_at LIMIT 1000",
    )
    .fetch_all(pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            error!("reward drain: listing pending failed: {e} — will retry next sweep");
            return 0;
        }
    };

    let mut applied = 0u64;
    for (reward_id,) in ids {
        match apply_pending_reward(pool, &reward_id).await {
            Ok(RewardApply::Applied { user_id, crystals }) => {
                applied += 1;
                info!("reward applied reward_id={reward_id} user={user_id} crystals={crystals}");
            }
            Ok(RewardApply::UserMissing { user_id }) => {
                error!(
                    "RECONCILE: reward_id={reward_id} user={user_id} missing — marked failed, NOT credited"
                );
            }
            Ok(RewardApply::Skipped) => {}
            Err(e) => {
                // Transient: leave the row `pending`, the next sweep retries it.
                warn!("reward apply reward_id={reward_id} failed (transient): {e} — will retry");
            }
        }
    }
    applied
}

/// Single-connection in-memory pool with the schema a real `init_db` would build, for
/// unit tests across the crate (db + auth). `sqlite::memory:` is per-connection, so
/// `max_connections(1)` keeps ONE db across calls; the two base tables are created the
/// way `init_db` does before `run_migrations` adds the rest (auth_sessions, …).
#[cfg(test)]
pub(crate) async fn test_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, apple_user_id TEXT UNIQUE NOT NULL, nickname TEXT NOT NULL, crystals INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS reward_outbox (reward_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, crystals INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, applied_at INTEGER, failed_reason TEXT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    run_migrations(&pool).await.unwrap();
    pool
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use sqlx::Row;

    async fn cols(pool: &SqlitePool, table: &str) -> Vec<String> {
        sqlx::query(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.get::<String, _>("name"))
            .collect()
    }

    async fn mem_db() -> SqlitePool {
        test_pool().await
    }

    /// A fresh session resolves to its user; an EXPIRED one resolves to None and is
    /// reaped (audit item 4). create_auth_session always stamps a future expiry, so
    /// the test rewrites expires_at into the past to exercise the gate deterministically.
    #[tokio::test]
    async fn session_expiry_rejects_and_reaps() {
        let pool = mem_db().await;
        create_user(&pool, "apple:sub1", "nick").await.unwrap();
        let user = get_user_by_apple_id(&pool, "apple:sub1").await.unwrap().unwrap();

        let token = create_auth_session(&pool, &user.id).await.unwrap();
        assert!(
            get_user_by_session_token(&pool, &token).await.unwrap().is_some(),
            "a fresh session authenticates"
        );

        // Force this session into the past.
        sqlx::query("UPDATE auth_sessions SET expires_at = ? WHERE user_id = ?")
            .bind(now_secs() - 1)
            .bind(&user.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            get_user_by_session_token(&pool, &token).await.unwrap().is_none(),
            "an expired session must NOT authenticate"
        );
        let remaining: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_sessions").fetch_one(&pool).await.unwrap();
        assert_eq!(remaining.0, 0, "the expired row was lazily reaped on lookup");
    }

    #[tokio::test]
    async fn sweep_removes_only_expired_and_revoke_drops_all_for_user() {
        let pool = mem_db().await;
        create_user(&pool, "apple:a", "a").await.unwrap();
        create_user(&pool, "apple:b", "b").await.unwrap();
        let a = get_user_by_apple_id(&pool, "apple:a").await.unwrap().unwrap();
        let b = get_user_by_apple_id(&pool, "apple:b").await.unwrap().unwrap();

        let _a_tok = create_auth_session(&pool, &a.id).await.unwrap(); // fresh
        let b_tok = create_auth_session(&pool, &b.id).await.unwrap(); // will expire
        sqlx::query("UPDATE auth_sessions SET expires_at = ? WHERE user_id = ?")
            .bind(now_secs() - 1)
            .bind(&b.id)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(sweep_expired_sessions(&pool).await.unwrap(), 1, "only b's expired row swept");
        assert!(get_user_by_session_token(&pool, &b_tok).await.unwrap().is_none());

        // Revoke a's sessions: a fresh issue + a's existing one both gone.
        let a_tok2 = create_auth_session(&pool, &a.id).await.unwrap();
        let removed = revoke_user_sessions(&pool, &a.id).await.unwrap();
        assert_eq!(removed, 2, "revoke drops all of a's sessions");
        assert!(get_user_by_session_token(&pool, &a_tok2).await.unwrap().is_none());
    }

    /// Nickname uniqueness is CASE-INSENSITIVE, and a user keeping their own name (any
    /// case) still sees it as available (excluding self).
    #[tokio::test]
    async fn nickname_uniqueness_is_case_insensitive_and_excludes_self() {
        let pool = mem_db().await;
        create_user(&pool, "apple:owner", "Otter").await.unwrap();
        let owner = get_user_by_apple_id(&pool, "apple:owner").await.unwrap().unwrap();

        // A free name is available; the taken one is not — regardless of case.
        assert!(check_nickname_available(&pool, "Falcon").await.unwrap());
        assert!(!check_nickname_available(&pool, "Otter").await.unwrap());
        assert!(!check_nickname_available(&pool, "otter").await.unwrap(), "case-insensitive collision");
        assert!(!check_nickname_available(&pool, "OTTER").await.unwrap(), "case-insensitive collision");

        // Excluding the owner: their own name (even re-cased) is free for them; a name
        // held by someone else is not.
        create_user(&pool, "apple:other", "Lynx").await.unwrap();
        assert!(check_nickname_available_excluding_user(&pool, "otter", &owner.id).await.unwrap());
        assert!(check_nickname_available_excluding_user(&pool, "Otter", &owner.id).await.unwrap());
        assert!(!check_nickname_available_excluding_user(&pool, "lynx", &owner.id).await.unwrap());
    }

    /// Signing out one device drops only that session token; the user's other sessions
    /// stay valid. Deleting an unknown token is a no-op (idempotent).
    #[tokio::test]
    async fn delete_session_by_token_is_scoped_and_idempotent() {
        let pool = mem_db().await;
        create_user(&pool, "apple:multi", "nick").await.unwrap();
        let user = get_user_by_apple_id(&pool, "apple:multi").await.unwrap().unwrap();

        let phone = create_auth_session(&pool, &user.id).await.unwrap();
        let tablet = create_auth_session(&pool, &user.id).await.unwrap();

        let removed = delete_session_by_token(&pool, &phone).await.unwrap();
        assert_eq!(removed, 1, "exactly the presented session is dropped");
        assert!(get_user_by_session_token(&pool, &phone).await.unwrap().is_none(), "phone signed out");
        assert!(get_user_by_session_token(&pool, &tablet).await.unwrap().is_some(), "tablet stays signed in");

        assert_eq!(delete_session_by_token(&pool, &phone).await.unwrap(), 0, "second sign-out is a no-op");
        assert_eq!(delete_session_by_token(&pool, "bogus").await.unwrap(), 0, "unknown token is a no-op");
    }

    /// Migration 004 dedupes pre-existing case-insensitive nickname collisions (oldest keeps
    /// the bare name) and then the UNIQUE index blocks new ones. Built by hand because
    /// test_pool already runs migrations (so the index would reject the colliding seed rows).
    #[tokio::test]
    async fn migration_004_dedupes_then_enforces_ci_unique_nicknames() {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        // Pre-migration base tables (run_migrations expects these to exist).
        sqlx::query(
            "CREATE TABLE users (id TEXT PRIMARY KEY, apple_user_id TEXT UNIQUE NOT NULL, nickname TEXT NOT NULL, crystals INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)",
        ).execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE reward_outbox (reward_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, crystals INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, applied_at INTEGER, failed_reason TEXT)")
            .execute(&pool).await.unwrap();

        // Seed a case-insensitive collision (no index yet): older "Bob" + newer "bob".
        for (id, apple, nick, created) in
            [("id-old", "apple:old", "Bob", 100i64), ("id-new", "apple:new", "bob", 200i64)]
        {
            sqlx::query("INSERT INTO users (id, apple_user_id, nickname, crystals, created_at) VALUES (?, ?, ?, 0, ?)")
                .bind(id).bind(apple).bind(nick).bind(created)
                .execute(&pool).await.unwrap();
        }

        run_migrations(&pool).await.unwrap();

        let old = get_user(&pool, "id-old").await.unwrap().unwrap();
        let new = get_user(&pool, "id-new").await.unwrap().unwrap();
        assert_eq!(old.nickname, "Bob", "oldest keeps the bare name");
        assert_ne!(new.nickname.to_lowercase(), "bob", "the newer collision was renamed");

        // The UNIQUE(COLLATE NOCASE) index now blocks a fresh duplicate.
        assert!(create_user(&pool, "apple:c", "BOB").await.is_err(), "CI duplicate is rejected by the index");
        assert!(create_user(&pool, "apple:d", "Carol").await.is_ok(), "a free name still works");
    }

    /// Migration 005 CANONICALIZES legacy rows (trim, strip control/zero-width chars) to the
    /// live write-path shape — so " Bob " and "Bob\u{200B}" don't survive as distinct rows
    /// next to "bob" the index would consider the same name. 004's dedupe-only pass left those
    /// near-twins (NOCASE treats " Bob " != "Bob"); 005 cleans them. Driven through the full
    /// run_migrations (004 then 005).
    #[tokio::test]
    async fn migration_005_canonicalizes_legacy_names_before_unique_index() {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE users (id TEXT PRIMARY KEY, apple_user_id TEXT UNIQUE NOT NULL, nickname TEXT NOT NULL, crystals INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)",
        ).execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE reward_outbox (reward_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, crystals INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, applied_at INTEGER, failed_reason TEXT)")
            .execute(&pool).await.unwrap();

        // Oldest holds a PADDED + zero-width-suffixed "Bob"; a newer plain "bob" is a
        // case-insensitive twin of the canonical form; a third carries a newline (log
        // injection) that must be stripped.
        for (id, apple, nick, created) in [
            ("id-old", "apple:old", " Bob ", 100i64),
            ("id-mid", "apple:mid", "Bob\u{200B}", 150i64),
            ("id-new", "apple:new", "bob", 200i64),
            ("id-nl", "apple:nl", "Eve\nadmin", 250i64),
        ] {
            sqlx::query("INSERT INTO users (id, apple_user_id, nickname, crystals, created_at) VALUES (?, ?, ?, 0, ?)")
                .bind(id).bind(apple).bind(nick).bind(created)
                .execute(&pool).await.unwrap();
        }

        run_migrations(&pool).await.unwrap();

        // Oldest is canonicalized to the trimmed "Bob" (NOT " Bob ").
        assert_eq!(get_user(&pool, "id-old").await.unwrap().unwrap().nickname, "Bob");
        // The two near-twins were renamed off the canonical "Bob".
        assert_ne!(get_user(&pool, "id-mid").await.unwrap().unwrap().nickname.to_ascii_lowercase(), "bob");
        assert_ne!(get_user(&pool, "id-new").await.unwrap().unwrap().nickname.to_ascii_lowercase(), "bob");
        // The newline was stripped (no control char survives into the stored value).
        let eve = get_user(&pool, "id-nl").await.unwrap().unwrap().nickname;
        assert_eq!(eve, "Eveadmin");
        assert!(!eve.contains('\n'));

        // Every stored value now satisfies the live validator AND the unique index holds.
        for id in ["id-old", "id-mid", "id-new", "id-nl"] {
            let n = get_user(&pool, id).await.unwrap().unwrap().nickname;
            assert!(crate::nickname::validate(&n).is_ok(), "stored {n:?} must validate");
        }
        assert!(create_user(&pool, "apple:z", "BOB").await.is_err(), "the canonical name is now unique");
    }

    /// THE reason 005 exists: a DB that ALREADY recorded 004 (dedupe-only) — so an edited-004
    /// would never re-run — still gets its legacy nicknames canonicalized when the new code
    /// runs. Simulates that DB: 004 recorded, index built over un-canonicalized data (" Bob "
    /// living next to "Bob" because NOCASE treats them as distinct), then run_migrations only
    /// has 005 left to do.
    #[tokio::test]
    async fn migration_005_canonicalizes_even_when_004_already_recorded() {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE users (id TEXT PRIMARY KEY, apple_user_id TEXT UNIQUE NOT NULL, nickname TEXT NOT NULL, crystals INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)",
        ).execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE reward_outbox (reward_id TEXT PRIMARY KEY, user_id TEXT NOT NULL, crystals INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, applied_at INTEGER, failed_reason TEXT)")
            .execute(&pool).await.unwrap();
        sqlx::query("CREATE TABLE schema_migrations (version TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();

        // 004 already ran here: it deduped (these two are NOCASE-distinct, so it left both)
        // and built the index over the un-canonicalized values.
        for (id, apple, nick) in [("id-a", "apple:a", " Bob "), ("id-b", "apple:b", "Bob")] {
            sqlx::query("INSERT INTO users (id, apple_user_id, nickname, crystals, created_at) VALUES (?, ?, ?, 0, 100)")
                .bind(id).bind(apple).bind(nick).execute(&pool).await.unwrap();
        }
        sqlx::query("CREATE UNIQUE INDEX idx_users_nickname_ci ON users(nickname COLLATE NOCASE)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO schema_migrations (version, applied_at) VALUES ('004_users_nickname_ci_unique', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();

        run_migrations(&pool).await.unwrap(); // only 005 (+ 001-003) has work to do

        // " Bob " is canonicalized to "Bob", which now collides with the real "Bob" — so ONE
        // keeps "Bob" and the other is deduped off it. Neither is the padded original.
        let a = get_user(&pool, "id-a").await.unwrap().unwrap().nickname;
        let b = get_user(&pool, "id-b").await.unwrap().unwrap().nickname;
        assert!(!a.contains(' ') && !b.contains(' '), "padding stripped from both: {a:?} {b:?}");
        assert_ne!(a.to_ascii_lowercase(), b.to_ascii_lowercase(), "the canonical collision was deduped");
        assert!(create_user(&pool, "apple:z", "BOB").await.is_err(), "index rebuilt on canonical data");
    }

    /// A db created by an OLDER build (reward_outbox WITHOUT failed_reason) must gain the column
    /// when the new code runs migrations — otherwise apply_pending_reward's `failed` UPDATE errors
    /// on every drain against the droplet's existing db. max_connections(1): a `sqlite::memory:`
    /// db is per-connection, so a pinned single connection keeps one db across the calls.
    #[tokio::test]
    async fn adds_failed_reason_to_legacy_reward_outbox_and_is_idempotent() {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();

        // Base table run_migrations expects (init_db creates it first; migration 004 reads it).
        sqlx::query("CREATE TABLE users (id TEXT PRIMARY KEY, apple_user_id TEXT UNIQUE NOT NULL, nickname TEXT NOT NULL, crystals INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)")
            .execute(&pool).await.unwrap();
        // Legacy pre-Blocker-3 shape: no failed_reason column.
        sqlx::query(
            "CREATE TABLE reward_outbox (
                reward_id  TEXT PRIMARY KEY,
                user_id    TEXT NOT NULL,
                crystals   INTEGER NOT NULL,
                status     TEXT NOT NULL DEFAULT 'pending',
                attempts   INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                applied_at INTEGER
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert!(!cols(&pool, "reward_outbox").await.contains(&"failed_reason".to_string()));

        run_migrations(&pool).await.unwrap();
        assert!(
            cols(&pool, "reward_outbox").await.contains(&"failed_reason".to_string()),
            "migration must add the missing failed_reason column"
        );

        // Re-running is a no-op (recorded in schema_migrations; pragma guard also covers it).
        run_migrations(&pool).await.unwrap();
        // Count THIS migration's record specifically — the migration list grows over
        // time (002_auth_sessions etc.), and this test pins 001's idempotency only.
        let applied: Vec<(String,)> = sqlx::query_as(
            "SELECT version FROM schema_migrations WHERE version = '001_reward_outbox_failed_reason'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(applied.len(), 1, "migration recorded exactly once across re-runs");
    }

    /// The REAL droplet case: a db built by the merged code has failed_reason in its CREATE TABLE
    /// but no schema_migrations row yet. run_migrations must NOT blind-ALTER the existing column
    /// (which errors "duplicate column") — the pragma guard skips the add and just records it.
    #[tokio::test]
    async fn current_db_with_column_but_no_migration_record_is_safe() {
        let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();

        // Base table run_migrations expects (init_db creates it first; migration 004 reads it).
        sqlx::query("CREATE TABLE users (id TEXT PRIMARY KEY, apple_user_id TEXT UNIQUE NOT NULL, nickname TEXT NOT NULL, crystals INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL)")
            .execute(&pool).await.unwrap();
        // Current shape: failed_reason ALREADY present, but schema_migrations doesn't exist yet.
        sqlx::query(
            "CREATE TABLE reward_outbox (
                reward_id     TEXT PRIMARY KEY,
                user_id       TEXT NOT NULL,
                crystals      INTEGER NOT NULL,
                status        TEXT NOT NULL DEFAULT 'pending',
                attempts      INTEGER NOT NULL DEFAULT 0,
                created_at    INTEGER NOT NULL,
                applied_at    INTEGER,
                failed_reason TEXT
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        run_migrations(&pool).await.unwrap(); // must not error on the duplicate column
        assert!(cols(&pool, "reward_outbox").await.contains(&"failed_reason".to_string()));
        let recorded: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM schema_migrations WHERE version = '001_reward_outbox_failed_reason'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(recorded.0, 1, "migration recorded even when the column already existed");
    }
}
