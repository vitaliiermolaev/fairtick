use crate::db;
use crate::error::{GameError, GameResult};
use crate::jwks_jwt::JwksJwtVerifier;
use sqlx::SqlitePool;

/// How identity tokens are checked. Provider-agnostic by construction: the verifier
/// is JWKS-generic (Apple today, Google when Android ships), the session tokens that
/// come back are ours and carry no provider at all.
pub enum AuthMode {
    /// Real verification (beta/prod): identity tokens must be valid provider JWTs.
    /// `allow_test_tokens` additionally admits `test_token_<id>` — for bot_runner
    /// soaks on dev; NEVER enable in prod.
    Verify { verifiers: Vec<JwksJwtVerifier>, allow_test_tokens: bool },
    /// Dev passthrough (pre-auth behavior): the token string IS the user id, except
    /// a JWT-shaped token contributes its unverified `sub` (so a real client against
    /// a local dev server keeps ONE stable identity instead of one per sign-in).
    /// Explicit opt-in via env; the boot log shouts when this is on.
    InsecurePassthrough,
}

pub struct AuthService {
    db_pool: SqlitePool,
    mode: AuthMode,
}

impl AuthService {
    pub fn new(db_pool: SqlitePool, mode: AuthMode) -> Self {
        Self { db_pool, mode }
    }

    /// Issue OUR session token after a verified sign-in: the client stores it and
    /// reconnects with SignInWithSession — no repeat of the native (Face ID) flow,
    /// and no dependence on the provider token's minutes-long lifetime.
    pub async fn issue_session(&self, user_id: &str) -> GameResult<String> {
        Ok(db::create_auth_session(&self.db_pool, user_id).await?)
    }

    /// Resolve a presented session token. Unknown/garbage → Auth error (the client
    /// clears its stored token and falls back to the provider sign-in).
    pub async fn sign_in_with_session(&self, session_token: &str) -> GameResult<db::User> {
        match db::get_user_by_session_token(&self.db_pool, session_token).await? {
            Some(user) => Ok(user),
            None => Err(GameError::InvalidSession),
        }
    }

    /// Get user by Apple User ID (for returning users)
    #[allow(dead_code)]
    pub async fn get_user_by_apple_id(&self, apple_user_id: &str) -> GameResult<Option<db::User>> {
        Ok(db::get_user_by_apple_id(&self.db_pool, apple_user_id).await?)
    }

    /// Authenticate with a provider identity token + the request nonce the client
    /// committed to (audit item 5 — replay binding). `nonce` is the RAW client nonce;
    /// the JWT must carry `SHA256(nonce)` in its `nonce` claim in Verify mode.
    pub async fn sign_in_with_apple(&self, apple_token: &str, nonce: &str) -> GameResult<db::User> {
        let apple_user_id = self.verify_apple_token(apple_token, nonce).await?;

        // Check if user exists
        match db::get_user_by_apple_id(&self.db_pool, &apple_user_id).await? {
            Some(user) => Ok(user),
            None => {
                // User doesn't exist yet, they need to set a nickname
                Err(GameError::Auth("Nickname required".to_string()))
            }
        }
    }

    /// Finish sign-up for a NEW account (or idempotently complete one). The verified Apple
    /// token already proves identity, so this is also the natural retry/double-submit target:
    /// if this Apple id ALREADY has an account, return it (a sign-in) and IGNORE the submitted
    /// nickname — no write, no validation, no "name taken" for the player's own name. Only a
    /// genuinely-new identity goes through validate + availability + create.
    pub async fn set_nickname(&self, apple_token: &str, nonce: &str, nickname: &str) -> GameResult<db::User> {
        let apple_user_id = self.verify_apple_token(apple_token, nonce).await?;

        // Idempotent finish-signup: a lost response / double-tap re-POSTs the same identity.
        // Return the existing account regardless of the (re-)submitted nickname.
        if let Some(existing) = db::get_user_by_apple_id(&self.db_pool, &apple_user_id).await? {
            return Ok(existing);
        }

        let nickname = validate_nickname(nickname)?;

        // Uniqueness (case-insensitive) — a brand-new account can't grab a taken name. This
        // is the friendly fast path; the UNIQUE(nickname COLLATE NOCASE) index below is the
        // authority under a race.
        if !db::check_nickname_available(&self.db_pool, &nickname).await? {
            return Err(GameError::NicknameTaken);
        }

        // The create can still lose a TOCTOU race the check above couldn't see — two new
        // accounts that both passed the check, or two finish-signup calls for the SAME
        // identity. Map the UNIQUE violation to the RIGHT answer instead of letting it fall
        // through as a raw Database error (which the HTTP layer would surface as a 401 and
        // wrongly bounce the player to sign-in — the exact race the index was added to cover).
        match db::create_user(&self.db_pool, &apple_user_id, &nickname).await {
            Ok(user) => Ok(user),
            Err(e) if is_unique_violation(&e) => {
                // Two UNIQUE constraints can trip. If THIS apple account now exists, a
                // concurrent finish-signup won — return it (idempotent sign-in). Otherwise
                // it's the nickname index: another new account grabbed the name first.
                match db::get_user_by_apple_id(&self.db_pool, &apple_user_id).await? {
                    Some(existing) => Ok(existing),
                    None => Err(GameError::NicknameTaken),
                }
            }
            Err(e) => Err(GameError::Database(e)),
        }
    }

    /// Update nickname for an existing user
    pub async fn update_nickname(&self, user_id: &str, nickname: &str) -> GameResult<db::User> {
        let nickname = validate_nickname(nickname)?;

        // Uniqueness (case-insensitive), excluding self so a no-op or case-only edit of
        // the user's OWN nickname is allowed.
        if !db::check_nickname_available_excluding_user(&self.db_pool, &nickname, user_id).await? {
            return Err(GameError::NicknameTaken);
        }

        // Update user's nickname. The UNIQUE(nickname COLLATE NOCASE) index is the backstop
        // for a TOCTOU race with the check above — map its violation to the friendly "taken"
        // (a 409) instead of a raw DB error (which the HTTP layer would surface as a 401 and
        // bounce the player to sign-in).
        db::update_user_nickname(&self.db_pool, user_id, &nickname)
            .await
            .map_err(unique_violation_as_taken)?;

        // Get updated user
        let user = db::get_user(&self.db_pool, user_id)
            .await?
            .ok_or_else(|| GameError::Auth("User not found".to_string()))?;

        Ok(user)
    }

    // ---- account self-service (authenticated by the SESSION token, off the game socket) ----
    //
    // These power the Settings screen. They authenticate with the stored SESSION token
    // (the same one the game socket presents), NOT a fresh provider identity token — so no
    // Face ID round-trip is needed to rename or sign out. (lead two-scene flow, 2026-06-12)

    /// Is `nickname` free for the holder of `session_token` to take? Resolves the session
    /// first (`InvalidSession` → 401). A self-owned name (any case) reads as available. An
    /// invalid name is NOT collapsed into `false` ("taken") — it returns `NicknameInvalid`
    /// (→ 400) so the client can say "unsupported characters" instead of "already taken",
    /// matching what the rename endpoint reports.
    pub async fn nickname_available(&self, session_token: &str, nickname: &str) -> GameResult<bool> {
        let user = self.sign_in_with_session(session_token).await?;
        let nickname = validate_nickname(nickname)?; // NicknameInvalid → 400, same as rename
        Ok(db::check_nickname_available_excluding_user(&self.db_pool, &nickname, &user.id).await?)
    }

    /// Change the nickname of the session's owner. Authoritative re-check of length +
    /// uniqueness lives in `update_nickname`, so a check that raced loses here.
    pub async fn change_nickname(&self, session_token: &str, nickname: &str) -> GameResult<db::User> {
        let user = self.sign_in_with_session(session_token).await?;
        self.update_nickname(&user.id, nickname).await
    }

    /// Sign out THIS device by invalidating the presented session token. Idempotent: an
    /// unknown/already-gone token still returns Ok (the device ends up signed out either
    /// way). The user's other devices keep their sessions.
    pub async fn sign_out(&self, session_token: &str) -> GameResult<()> {
        db::delete_session_by_token(&self.db_pool, session_token).await?;
        Ok(())
    }

    /// Verify an identity token and extract the canonical user id. `nonce` is the RAW
    /// client nonce; in Verify mode the JWT's `nonce` claim must equal `SHA256(nonce)`
    /// (audit item 5). Async because Verify may fetch the provider JWKS on an unknown kid.
    async fn verify_apple_token(&self, token: &str, nonce: &str) -> GameResult<String> {
        if token.is_empty() {
            return Err(GameError::Auth("Invalid token".to_string()));
        }
        match &self.mode {
            AuthMode::Verify { verifiers, allow_test_tokens } => {
                if let Some(user_id) = token.strip_prefix("test_token_") {
                    return if *allow_test_tokens {
                        tracing::info!("🔑 test token accepted (dev): {user_id}");
                        Ok(user_id.to_string())
                    } else {
                        Err(GameError::Auth("Test tokens are not allowed".to_string()))
                    };
                }
                // Bind the token to the request nonce. An empty nonce or a JWT whose
                // `nonce` claim doesn't match SHA256(nonce) is a replay/forgery — reject.
                if nonce.is_empty() {
                    return Err(GameError::Auth("Missing sign-in nonce".to_string()));
                }
                let expected_nonce = sha256_hex(nonce);
                let mut last_err = String::from("no identity verifier configured");
                for v in verifiers {
                    match v.verify(token).await {
                        Ok(claims) => {
                            if claims.nonce.as_deref() != Some(expected_nonce.as_str()) {
                                tracing::warn!("identity token nonce mismatch (replay?)");
                                return Err(GameError::Auth("Sign-in nonce mismatch".to_string()));
                            }
                            // Namespace by provider: "apple:<sub>" can never collide with
                            // a google sub or a legacy dev device id.
                            return Ok(format!("{}:{}", v.provider, claims.sub));
                        }
                        Err(e) => last_err = e,
                    }
                }
                tracing::warn!("identity token rejected: {last_err}");
                Err(GameError::Auth("Invalid identity token".to_string()))
            }
            AuthMode::InsecurePassthrough => {
                if let Some(user_id) = token.strip_prefix("test_token_") {
                    tracing::info!("🔑 Using test token for user: {user_id}");
                    return Ok(user_id.to_string());
                }
                // JWT-shaped? Take the UNVERIFIED sub so a real client against a dev
                // server keeps one stable identity across sign-ins. Anything else is
                // the legacy raw device id.
                if let Some(sub) = unverified_jwt_sub(token) {
                    tracing::warn!("⚠️ INSECURE auth mode: accepting unverified JWT sub");
                    return Ok(format!("apple:{sub}"));
                }
                Ok(token.to_string())
            }
        }
    }
}

/// Validate + canonicalize a nickname for storage. Thin wrapper over the shared nickname
/// policy ([`crate::nickname`]) so the live write path and migration 004 agree on the rules;
/// the reason string rides as a typed [`GameError::NicknameInvalid`] (→ HTTP 400 with that
/// exact reason). The server is authoritative — the client's own sanitize is advisory and
/// trivially bypassed over HTTP.
fn validate_nickname(nickname: &str) -> GameResult<String> {
    crate::nickname::validate(nickname).map_err(GameError::NicknameInvalid)
}

/// True when `e` is a DB UNIQUE-constraint violation (any unique index on the statement).
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

/// Map a DB UNIQUE-constraint violation to the typed "already taken" (→ HTTP 409). Any other
/// DB error passes through unchanged. Used on the rename write (single UNIQUE on the
/// statement — the nickname index), so a race that loses doesn't surface as a 401.
fn unique_violation_as_taken(e: sqlx::Error) -> GameError {
    if is_unique_violation(&e) {
        GameError::NicknameTaken
    } else {
        GameError::Database(e)
    }
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Best-effort UNVERIFIED `sub` extraction from a JWT-shaped token (insecure dev mode
/// only — never used in Verify mode).
fn unverified_jwt_sub(token: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    v.get("sub").and_then(|s| s.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// InsecurePassthrough so the identity token IS the user id — no JWKS, no nonce
    /// binding — which keeps these tests about nickname/session policy, not JWT verify.
    async fn svc() -> AuthService {
        AuthService::new(db::test_pool().await, AuthMode::InsecurePassthrough)
    }

    #[tokio::test]
    async fn set_nickname_enforces_case_insensitive_uniqueness() {
        let svc = svc().await;
        svc.set_nickname("apple_a", "", "Otter").await.unwrap();

        // A different account can't take the same name in any case.
        let taken = svc.set_nickname("apple_b", "", "otter").await;
        assert!(matches!(taken, Err(GameError::NicknameTaken)));

        // A free name is fine; an over-long one is a length error (distinct from "taken").
        svc.set_nickname("apple_b", "", "Falcon").await.unwrap();
        let long = svc.set_nickname("apple_c", "", &"x".repeat(31)).await;
        assert!(matches!(long, Err(GameError::NicknameInvalid(_))));
    }

    /// The signup TOCTOU the UNIQUE index backstops: a create that loses the race must come
    /// back as NicknameTaken (→ 409) or an idempotent sign-in — NEVER a raw Database error
    /// (→ 401, which would bounce the player to sign-in). Driven deterministically, no
    /// reliance on real thread interleaving.
    #[tokio::test]
    async fn signup_create_race_loser_maps_to_taken_not_db_error() {
        let svc = svc().await;

        // (1) apple_user_id UNIQUE branch — reachable for real: a nickname that PASSES the
        // availability check, but a create that collides because a concurrent finish-signup
        // already made THIS identity's account. The loser is signed into the existing
        // account (idempotent), not handed a DB error.
        db::create_user(&svc.db_pool, "apple_dup", "FirstName").await.unwrap();
        let again = svc.set_nickname("apple_dup", "", "SecondName").await.unwrap();
        assert_eq!(again.nickname, "FirstName", "concurrent signup winner's account is returned");

        // (2) nickname-index UNIQUE branch — the app check normally catches this first, so
        // prove the mapping at the exact boundary the create-race relies on: a duplicate
        // create is a UNIQUE violation, and is mapped to NicknameTaken, not Database.
        let collision = db::create_user(&svc.db_pool, "apple_other", "firstname").await.unwrap_err();
        assert!(is_unique_violation(&collision), "a duplicate nickname is a UNIQUE violation");
        assert!(matches!(unique_violation_as_taken(collision), GameError::NicknameTaken));
    }

    /// /auth/nickname must be idempotent: a lost response / double-tap re-POSTs the same
    /// verified identity, and the second call returns the SAME account (a sign-in), not a
    /// "nickname taken" for the player's own name.
    #[tokio::test]
    async fn set_nickname_is_idempotent_after_account_already_exists() {
        let svc = svc().await;
        let first = svc.set_nickname("apple_a", "", "Otter").await.unwrap();
        let retry = svc.set_nickname("apple_a", "", "Otter").await.unwrap();
        assert_eq!(retry.id, first.id, "retry returns the same account");
        assert_eq!(retry.nickname, "Otter");
    }

    /// An existing-account retry IGNORES the submitted nickname entirely — even a name held
    /// by someone else (a stale/garbage payload) resolves to a sign-in, not NicknameTaken.
    #[tokio::test]
    async fn set_nickname_existing_account_ignores_submitted_taken_name() {
        let svc = svc().await;
        let a = svc.set_nickname("apple_a", "", "Otter").await.unwrap();
        svc.set_nickname("apple_b", "", "Lynx").await.unwrap();
        let retry = svc.set_nickname("apple_a", "", "Lynx").await.unwrap();
        assert_eq!(retry.id, a.id, "still apple_a's account");
        assert_eq!(retry.nickname, "Otter", "the submitted (taken) name is ignored, not written");
    }

    #[tokio::test]
    async fn change_nickname_via_session_renames_and_blocks_taken() {
        let svc = svc().await;
        let a = svc.set_nickname("apple_a", "", "Otter").await.unwrap();
        svc.set_nickname("apple_b", "", "Lynx").await.unwrap();
        let a_token = svc.issue_session(&a.id).await.unwrap();

        // Rename to a free name works and is readable back.
        let renamed = svc.change_nickname(&a_token, "Falcon").await.unwrap();
        assert_eq!(renamed.nickname, "Falcon");

        // Keeping own name (case-only edit) is allowed — excludes self.
        assert_eq!(svc.change_nickname(&a_token, "falcon").await.unwrap().nickname, "falcon");

        // A name held by someone else is rejected.
        let clash = svc.change_nickname(&a_token, "Lynx").await;
        assert!(matches!(clash, Err(GameError::NicknameTaken)));

        // An invalid session can't rename anyone.
        assert!(svc.change_nickname("bogus", "Whatever").await.is_err());
    }

    #[tokio::test]
    async fn nickname_available_excludes_self_and_validates_length() {
        let svc = svc().await;
        let a = svc.set_nickname("apple_a", "", "Otter").await.unwrap();
        svc.set_nickname("apple_b", "", "Lynx").await.unwrap();
        let a_token = svc.issue_session(&a.id).await.unwrap();

        assert!(svc.nickname_available(&a_token, "Falcon").await.unwrap(), "free name");
        assert!(svc.nickname_available(&a_token, "otter").await.unwrap(), "own name (any case)");
        assert!(!svc.nickname_available(&a_token, "lynx").await.unwrap(), "taken by another");
        // Invalid names are NOT "false" (taken) — they surface NicknameInvalid (→ 400), the
        // same answer the rename gives, so the client says "unsupported"/"1-30", not "taken".
        assert!(
            matches!(svc.nickname_available(&a_token, "").await, Err(GameError::NicknameInvalid(_))),
            "empty"
        );
        assert!(
            matches!(
                svc.nickname_available(&a_token, &"x".repeat(31)).await,
                Err(GameError::NicknameInvalid(_))
            ),
            "too long"
        );
        assert!(
            matches!(
                svc.nickname_available(&a_token, "ze\u{200B}ro").await,
                Err(GameError::NicknameInvalid(_))
            ),
            "forbidden char is invalid, not taken"
        );
        assert!(svc.nickname_available("bogus", "Falcon").await.is_err(), "invalid session errors");
    }

    #[test]
    fn validate_nickname_trims_and_rejects_dangerous_input() {
        // Trims, keeps internal spaces.
        assert_eq!(validate_nickname("  Sly Fox  ").unwrap(), "Sly Fox");
        // Whitespace-only / empty / too long → length error (distinct 400 reason).
        for bad in ["", "   ", &"x".repeat(31)] {
            assert!(matches!(
                validate_nickname(bad),
                Err(GameError::NicknameInvalid(r)) if r.contains("1-30")
            ));
        }
        // Control chars (log injection) and zero-width / bidi (spoofing) → unsupported-chars.
        for bad in ["evil\nadmin", "ab\tcd", "ze\u{200B}ro", "rtl\u{202E}flip", "bom\u{FEFF}"] {
            assert!(
                matches!(validate_nickname(bad), Err(GameError::NicknameInvalid(r)) if r.contains("unsupported")),
                "should reject {bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn change_nickname_stores_the_trimmed_value() {
        let svc = svc().await;
        let a = svc.set_nickname("apple_a", "", "  Padded  ").await.unwrap();
        assert_eq!(a.nickname, "Padded", "set_nickname stores the trimmed value");
        let token = svc.issue_session(&a.id).await.unwrap();
        assert_eq!(svc.change_nickname(&token, "  Spaced Out  ").await.unwrap().nickname, "Spaced Out");
    }

    #[tokio::test]
    async fn sign_out_invalidates_the_presented_session() {
        let svc = svc().await;
        let a = svc.set_nickname("apple_a", "", "Otter").await.unwrap();
        let token = svc.issue_session(&a.id).await.unwrap();
        assert!(svc.sign_in_with_session(&token).await.is_ok(), "valid before sign-out");

        svc.sign_out(&token).await.unwrap();
        assert!(svc.sign_in_with_session(&token).await.is_err(), "session is dead after sign-out");
        // Idempotent: signing out an already-dead token is still Ok.
        svc.sign_out(&token).await.unwrap();
    }
}
