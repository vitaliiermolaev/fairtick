//! Provider-agnostic JWKS identity-token (JWT) verification.
//!
//! Sign in with Apple (iOS) and Google Sign-In (Android) hand the client an RS256
//! JWT; both providers publish their public keys as a JWKS document and document the
//! same verification recipe: signature against the JWKS key chosen by the token's
//! `kid`, a fixed issuer, audience = OUR app id, and expiry. The stable user
//! identity is the `sub` claim. One verifier, one constructor per provider —
//! `apple()` is live today; `google()` is a constructor away when Android ships.
//!
//! Split for testability: `verify_with_key` is pure (no I/O) and unit-tested with a
//! locally-generated RSA pair; the JWKS fetch/caching wrapper around it is thin.
//! JWKS keys rotate rarely — the cache refreshes only on an UNKNOWN `kid`, rate-
//! limited so a flood of garbage tokens can't turn us into an Apple-JWKS DDoS.

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const APPLE_JWKS_URL: &str = "https://appleid.apple.com/auth/keys";
const APPLE_ISSUER: &str = "https://appleid.apple.com";
// Android, when it lands: Google's JWKS at https://www.googleapis.com/oauth2/v3/certs,
// issuer https://accounts.google.com — same struct, one more constructor.
/// Minimum spacing between JWKS refetches (unknown-kid storms must not hammer Apple).
const JWKS_REFETCH_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// The claims we use. `sub` is the provider's stable per-user id — THE identity.
/// `nonce` is the request-binding claim (audit item 5): present when the client set
/// a nonce on the native request; the caller compares it to what the client committed.
#[derive(Debug, Deserialize)]
pub struct IdentityClaims {
    pub sub: String,
    #[serde(default)]
    pub nonce: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    /// Key type / algorithm / intended use. Apple's signing keys are RSA / RS256 / sig; we
    /// filter on these so a future non-signing or non-RSA key in the document can't be loaded
    /// as a verification key. `alg`/`use` are optional defensively (reject only on a WRONG
    /// value, not a missing one) — `kty` is required to be RSA.
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default, rename = "use")]
    use_: Option<String>,
    n: String,
    e: String,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    /// When we last ATTEMPTED a JWKS fetch (success OR failure). The refetch rate limit keys
    /// off the attempt, not the success — otherwise a failing fetch (Apple down, DNS/TLS
    /// error) never advances the clock, and a flood of unknown-kid tokens during the outage
    /// would re-trigger an outbound fetch on EVERY request (the JWKS-DDoS this is meant to
    /// prevent). Named `_attempt` so the semantics can't be misread.
    last_attempt: Option<Instant>,
}

pub struct JwksJwtVerifier {
    /// Short provider tag ("apple", later "google") — namespaces user ids and logs.
    pub provider: &'static str,
    jwks_url: &'static str,
    issuer: &'static str,
    /// Expected `aud` — the app's bundle/client identifier for this provider.
    audience: String,
    cache: RwLock<KeyCache>,
    http: reqwest::Client,
}

impl JwksJwtVerifier {
    /// Sign in with Apple (iOS). `audience` = the iOS bundle identifier.
    pub fn apple(audience: String) -> Self {
        Self {
            provider: "apple",
            jwks_url: APPLE_JWKS_URL,
            issuer: APPLE_ISSUER,
            audience,
            cache: RwLock::new(KeyCache { keys: HashMap::new(), last_attempt: None }),
            http: reqwest::Client::new(),
        }
    }

    /// Verify an Apple identity token end-to-end. Errors are short reason strings —
    /// the caller logs them server-side and replies with a generic AuthFailed.
    pub async fn verify(&self, token: &str) -> Result<IdentityClaims, String> {
        let header = decode_header(token).map_err(|e| format!("bad jwt header: {e}"))?;
        let kid = header.kid.ok_or("jwt missing kid")?;

        if let Some(key) = self.cache.read().await.keys.get(&kid) {
            return Self::verify_with_key(token, key, self.issuer, &self.audience);
        }
        // Unknown kid: refresh the JWKS (rate-limited) and retry once.
        self.refresh_jwks().await?;
        let cache = self.cache.read().await;
        let key = cache
            .keys
            .get(&kid)
            .ok_or_else(|| format!("no {} key for kid {kid} after refresh", self.provider))?;
        Self::verify_with_key(token, key, self.issuer, &self.audience)
    }

    /// Pure verification against ONE key: RS256 + iss + aud + exp. No I/O.
    fn verify_with_key(
        token: &str,
        key: &DecodingKey,
        issuer: &str,
        audience: &str,
    ) -> Result<IdentityClaims, String> {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[issuer]);
        validation.set_audience(&[audience]);
        validation.validate_exp = true;
        decode::<IdentityClaims>(token, key, &validation)
            .map(|data| data.claims)
            .map_err(|e| format!("jwt verification failed: {e}"))
    }

    async fn refresh_jwks(&self) -> Result<(), String> {
        // Cooldown gate + attempt stamp in a SHORT critical section, then RELEASE the lock
        // before the network call. Holding the write lock across the fetch would block even
        // verifies of already-cached known kids (their read lock can't proceed) for up to the
        // 5s timeout — under an unknown-kid flood that's "freeze every Apple sign-in once per
        // cooldown". Re-check under the lock so a concurrent task that just stamped wins the
        // race and the others return early (only one fetch per cooldown). (review)
        {
            let mut cache = self.cache.write().await;
            if let Some(at) = cache.last_attempt {
                if at.elapsed() < JWKS_REFETCH_MIN_INTERVAL {
                    return Ok(()); // recent attempt — whatever it brought (incl. nothing) is what we have
                }
            }
            // Stamp the ATTEMPT before the network call so a FAILED fetch still opens the
            // cooldown — otherwise an Apple/DNS/TLS outage + an unknown-kid flood would
            // refetch on every request.
            cache.last_attempt = Some(Instant::now());
        }
        let jwks: Jwks = self
            .http
            .get(self.jwks_url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("jwks fetch failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("jwks parse failed: {e}"))?;
        let mut keys = HashMap::new();
        for k in jwks.keys {
            // Defense in depth: only load RSA signing keys (RS256). A wrong alg/use is
            // skipped; a missing one is tolerated (Apple always sends them today). The
            // signature + iss/aud/exp checks in verify_with_key remain the real boundary.
            if k.kty != "RSA"
                || matches!(k.alg.as_deref(), Some(a) if a != "RS256")
                || matches!(k.use_.as_deref(), Some(u) if u != "sig")
            {
                tracing::warn!(
                    "skipping unexpected {} JWK kid={} (kty={}, alg={:?}, use={:?})",
                    self.provider,
                    k.kid,
                    k.kty,
                    k.alg,
                    k.use_
                );
                continue;
            }
            match DecodingKey::from_rsa_components(&k.n, &k.e) {
                Ok(dk) => {
                    keys.insert(k.kid, dk);
                }
                Err(e) => tracing::warn!("skipping unusable {} JWK {}: {e}", self.provider, k.kid),
            }
        }
        tracing::info!("{} JWKS refreshed: {} keys", self.provider, keys.len());
        // Re-acquire the lock just to swap in the freshly-parsed keys (last_attempt was
        // already stamped in the critical section above).
        self.cache.write().await.keys = keys;
        Ok(())
    }

    /// Test-only constructor with injected keys (no network).
    #[cfg(test)]
    fn with_static_keys(audience: String, keys: HashMap<String, DecodingKey>) -> Self {
        Self {
            provider: "apple",
            jwks_url: APPLE_JWKS_URL,
            issuer: APPLE_ISSUER,
            audience,
            cache: RwLock::new(KeyCache { keys, last_attempt: Some(Instant::now()) }),
            http: reqwest::Client::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;

    // Throwaway RSA pair generated FOR THESE TESTS ONLY (openssl genrsa 2048).
    // Never used outside this module; embedding it keeps the tests hermetic.
    const TEST_PRIVATE_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDTkYH0Lt6t1TBy
45rVbMs1qnumRVd8+bQwfxiNaCFqlpmLMoQKt0VxAkOIv3Gd60ZVcfz0l1iXiohM
1QuPx96oDH4C0F4kiCRt0Oo1PMPhj2ismXXmaOjqSy+INvJHaKBYlnpP3gCb9Aym
DCl0uOb7VgjbTfpUp7yp6xP2L6apGgp3B+DCCbFCsDebscKELbr/agOgOlPJZBBR
JTzFbgfV4Cgvt/DwA7bI0gLDgZFIbMBfhapCmiQ5lt78ZtFyDh2QNMfifCFZfaAi
NDtuHQsc+ivrI8yOFafFc2XYKlYzAcVSutVPuoZsOkB6AE5SyF/5tytfdpnWVCZQ
vAQvIoTtAgMBAAECggEACOeILvhLO/xYi5IjyDuWrYQNRZ+IG2tapGqwMSdpHKM8
N1auP9GcQeTeZckbWMVE7D/qoK3yakv7PGYzeqvNWO8c3WgN3+WKlV3XgnUaKdcP
PBB1A16pl7U6/QJfBEovHkguQeIUmxVUjcDxfIdAHxaGzRm4YWUlgVTl13s3vWJj
ay2XFXi3jtVPCHim357bcCkVtB8VcpX5wHo4bPbpx9GsnAkS4Bq53QbqARqlAI8h
z1ZfLE5UX3zWV8HkIeBb1LACDyCix5I2DJlGnekEO74pSNgWFOuF2GhdnT0jcNGr
iSF9v4Qw/C2Lkur2VnoZDnw4IxcT2cugre2t8aOsOwKBgQD/vlkOQLO6vRIAO5ym
Sy6SkpbQpmx/WNmWB7tASt7njHKJhZ9aawkm3krzYnmNKjU1A/kFlGcDu5Vx1DAI
7xthyVkX/6lzADa+/mpSMeus6vX2iMpIYbiXl8rRi+vypv7/5mKLnqcsj6yksp4o
k5fC51xCDR/1jNiCG41fKtuo+wKBgQDTx9HMAxNu95Vf45skt6caVbQl4S63yE9Y
LIPq09lHG0lwgjxBD7GRyTXpplZ/DgvQlohHu4bv4FZwMUdJNLpjLOGmhKOY9IgY
gCIUcvnYMIzvUiEJ3d0eEb1w7V4dDUIU5aMFZDvfQIawdeRkmDvgr5jvTpso/+xT
SkNrEFv1NwKBgQCucYlPdoTiCJuhuwfESp4O7pye4BY720A33Tg1x5w6NwvdkF69
Dyuj7pcTYwVka/j1G6udybdmzWpHxaOqRGbaEbyK6SINRoURTHr7a//E6FQ0AORx
8O43wRtgSd/8mTpxFRX9BJAlji8F/KxzIxGuqZ+9kjRNivAX93E8DADfRwKBgF/H
A8u3LGfIEsceAYEWib0wO1vSPjWhorim0TY3fxFYdtsqGyP1fAIJtJcpwf6OFKvO
GG4QklMT6yOsNagW76CAoMCVRgObu50Q/divsuyh8Gsfgo+axjCeJ0XWI/URlOws
epCqpyUtYnyVpKgV7SaNY6X+r89YBsIYWOsnp977AoGBAJZyXO34JiJm1IqetQrL
iHH1k9IXt0nAxEaRSqxXJaHm2/4Dro2h71my6imW3kDnMbwy1Hx/e1NSL+iaIc2E
h8TO52zGaxloCts7UMbT1NJ9Pp1Y1qlOFdkQb1pcct92+qEjqJZStSF5WS7WxIZL
QrdauMvgJ9d0TZLNUzJVVJTv
-----END PRIVATE KEY-----";
    const TEST_PUBLIC_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA05GB9C7erdUwcuOa1WzL
Nap7pkVXfPm0MH8YjWghapaZizKECrdFcQJDiL9xnetGVXH89JdYl4qITNULj8fe
qAx+AtBeJIgkbdDqNTzD4Y9orJl15mjo6ksviDbyR2igWJZ6T94Am/QMpgwpdLjm
+1YI2036VKe8qesT9i+mqRoKdwfgwgmxQrA3m7HChC26/2oDoDpTyWQQUSU8xW4H
1eAoL7fw8AO2yNICw4GRSGzAX4WqQpokOZbe/GbRcg4dkDTH4nwhWX2gIjQ7bh0L
HPor6yPMjhWnxXNl2CpWMwHFUrrVT7qGbDpAegBOUshf+bcrX3aZ1lQmULwELyKE
7QIDAQAB
-----END PUBLIC KEY-----";

    const AUD: &str = "com.test.fairtick";

    #[derive(Serialize)]
    struct TestClaims {
        sub: String,
        iss: String,
        aud: String,
        exp: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        nonce: Option<String>,
    }

    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    fn make_token(iss: &str, aud: &str, exp: u64, kid: Option<&str>) -> String {
        make_token_nonce(iss, aud, exp, kid, None)
    }

    fn make_token_nonce(iss: &str, aud: &str, exp: u64, kid: Option<&str>, nonce: Option<&str>) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(String::from);
        let claims = TestClaims {
            sub: "001234.abcdef".into(),
            iss: iss.into(),
            aud: aud.into(),
            exp,
            nonce: nonce.map(String::from),
        };
        let key = EncodingKey::from_rsa_pem(TEST_PRIVATE_PEM.as_bytes()).unwrap();
        encode(&header, &claims, &key).unwrap()
    }

    fn verifier() -> JwksJwtVerifier {
        let mut keys = HashMap::new();
        keys.insert("testkid".to_string(), DecodingKey::from_rsa_pem(TEST_PUBLIC_PEM.as_bytes()).unwrap());
        JwksJwtVerifier::with_static_keys(AUD.into(), keys)
    }

    #[tokio::test]
    async fn valid_token_yields_sub() {
        let v = verifier();
        let t = make_token(APPLE_ISSUER, AUD, now() + 600, Some("testkid"));
        let claims = v.verify(&t).await.expect("valid token verifies");
        assert_eq!(claims.sub, "001234.abcdef");
    }

    #[tokio::test]
    async fn verify_surfaces_nonce_claim() {
        let v = verifier();
        let t = make_token_nonce(APPLE_ISSUER, AUD, now() + 600, Some("testkid"), Some("abc123"));
        let claims = v.verify(&t).await.expect("valid token verifies");
        assert_eq!(
            claims.nonce.as_deref(),
            Some("abc123"),
            "the nonce claim is surfaced for the caller to check"
        );
        let t2 = make_token(APPLE_ISSUER, AUD, now() + 600, Some("testkid"));
        assert_eq!(v.verify(&t2).await.unwrap().nonce, None, "absent nonce → None");
    }

    #[tokio::test]
    async fn wrong_audience_rejected() {
        let v = verifier();
        let t = make_token(APPLE_ISSUER, "com.other.app", now() + 600, Some("testkid"));
        assert!(v.verify(&t).await.is_err(), "another app's token must not authenticate here");
    }

    #[tokio::test]
    async fn wrong_issuer_rejected() {
        let v = verifier();
        let t = make_token("https://evil.example", AUD, now() + 600, Some("testkid"));
        assert!(v.verify(&t).await.is_err());
    }

    #[tokio::test]
    async fn expired_token_rejected() {
        let v = verifier();
        let t = make_token(APPLE_ISSUER, AUD, now().saturating_sub(3600), Some("testkid"));
        assert!(v.verify(&t).await.is_err(), "expired identity tokens must not authenticate");
    }

    #[tokio::test]
    async fn tampered_token_rejected() {
        let v = verifier();
        let t = make_token(APPLE_ISSUER, AUD, now() + 600, Some("testkid"));
        // Flip a payload byte: signature must no longer match.
        let mut parts: Vec<String> = t.split('.').map(String::from).collect();
        let mut payload = parts[1].clone().into_bytes();
        let i = payload.len() / 2;
        payload[i] = if payload[i] == b'A' { b'B' } else { b'A' };
        parts[1] = String::from_utf8(payload).unwrap();
        let forged = parts.join(".");
        assert!(v.verify(&forged).await.is_err());
    }
}
