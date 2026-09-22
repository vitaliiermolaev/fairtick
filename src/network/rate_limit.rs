use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Per-IP token-bucket rate limiter for the HTTP auth/account endpoints.
///
/// The game websocket has its own caps (ConnGuard / PerIpGuard), but the REST routes
/// (`/auth/*`, `/account/*`) had NONE — so a flood of `/auth/apple` (JWKS crypto per call)
/// or `/account/nickname/check` (a DB scan + nickname enumeration per call) was unbounded.
/// In-process and lock-guarded, matching the existing PerIpGuard style — no extra deps.
///
/// Keyed by the proxied client IP (X-Forwarded-For / X-Real-IP). A request with NO such
/// header (direct/local, i.e. not through Caddy) is left to the caller to allow — the same
/// posture as PerIpGuard; in prod Caddy always sets the header.
pub struct IpRateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    /// Hard cap on tracked IPs so the map itself can't become a memory-DoS; once reached,
    /// idle (fully-refilled) buckets are dropped before a new IP is admitted.
    max_tracked: usize,
    buckets: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl IpRateLimiter {
    /// `capacity` = burst size (tokens available at rest); `refill_per_sec` = sustained
    /// allowed rate. e.g. (40, 8.0) ⇒ a 40-request burst, then 8 req/s per IP.
    pub fn new(capacity: u32, refill_per_sec: f64) -> Self {
        Self::with_max_tracked(capacity, refill_per_sec, 50_000)
    }

    fn with_max_tracked(capacity: u32, refill_per_sec: f64, max_tracked: usize) -> Self {
        Self {
            capacity: capacity.max(1) as f64,
            refill_per_sec: refill_per_sec.max(0.0),
            max_tracked: max_tracked.max(1),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Consume one token for `ip`. Returns true if allowed, false if the bucket is empty
    /// (the caller answers 429).
    pub fn check(&self, ip: &str) -> bool {
        self.check_at(ip, Instant::now())
    }

    /// `check` with an injected clock, for deterministic tests.
    pub fn check_at(&self, ip: &str, now: Instant) -> bool {
        let mut map = self.buckets.lock().unwrap();

        // Admitting a brand-new IP at the cap would grow the map past max_tracked — a memory
        // DoS via rotating/spoofed source IPs. First reclaim idle buckets (refilled back to
        // full — nobody is actively spending them); if that frees no room, DENY the new IP
        // rather than insert. Deny (not evict-an-active-bucket) is the safe posture for these
        // auth/account routes: an attacker can't push a legit, actively-rate-limited IP out
        // by flooding fresh ones. Existing tracked IPs are unaffected (they take the entry()
        // path below). This is the HARD cap the field name promises.
        if map.len() >= self.max_tracked && !map.contains_key(ip) {
            let cap = self.capacity;
            let rate = self.refill_per_sec;
            map.retain(|_, b| {
                let refilled =
                    (b.tokens + now.saturating_duration_since(b.last).as_secs_f64() * rate).min(cap);
                refilled < cap
            });
            if map.len() >= self.max_tracked {
                return false; // no room — the new IP is not admitted until a slot frees
            }
        }

        let b = map.entry(ip.to_string()).or_insert(Bucket { tokens: self.capacity, last: now });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn allows_burst_then_blocks_then_refills() {
        let rl = IpRateLimiter::new(3, 1.0); // burst 3, 1/s
        let t0 = Instant::now();

        // The burst of 3 is allowed; the 4th in the same instant is denied.
        assert!(rl.check_at("1.2.3.4", t0));
        assert!(rl.check_at("1.2.3.4", t0));
        assert!(rl.check_at("1.2.3.4", t0));
        assert!(!rl.check_at("1.2.3.4", t0), "burst exhausted");

        // After 1s, ~1 token refilled → exactly one more allowed.
        let t1 = t0 + Duration::from_secs(1);
        assert!(rl.check_at("1.2.3.4", t1));
        assert!(!rl.check_at("1.2.3.4", t1), "only one refilled");
    }

    #[test]
    fn buckets_are_per_ip() {
        let rl = IpRateLimiter::new(1, 0.0); // 1 token, no refill
        let t0 = Instant::now();
        assert!(rl.check_at("a", t0));
        assert!(!rl.check_at("a", t0), "a is spent");
        assert!(rl.check_at("b", t0), "b has its own bucket");
    }

    /// The map is a HARD cap: with no refill, every tracked bucket is non-idle (spent), so a
    /// brand-new IP at the cap is DENIED instead of growing the map. Tracked IPs still work.
    #[test]
    fn does_not_grow_past_max_tracked_when_no_idle_buckets() {
        let rl = IpRateLimiter::with_max_tracked(1, 0.0, 2); // burst 1, no refill, cap 2 IPs
        let t0 = Instant::now();

        // Fill both slots and spend each bucket so neither is idle (refilled-to-full).
        assert!(rl.check_at("ip-1", t0));
        assert!(rl.check_at("ip-2", t0));
        assert!(!rl.check_at("ip-1", t0), "ip-1 spent");

        // A third, never-seen IP can't be admitted — no idle bucket to reclaim.
        assert!(!rl.check_at("ip-3", t0), "new IP denied at the cap (no map growth)");
        assert!(!rl.check_at("ip-4", t0), "still denied for another new IP");

        // An ALREADY-tracked IP is unaffected by the cap once its bucket refills.
        let t1 = t0 + Duration::from_secs(0); // no refill configured → still spent
        assert!(!rl.check_at("ip-1", t1), "tracked-but-spent IP follows its own bucket, not the cap");
    }

    /// With refill, an idle (fully-refilled) bucket IS reclaimed, making room for a new IP.
    #[test]
    fn idle_bucket_is_evicted_to_admit_a_new_ip() {
        let rl = IpRateLimiter::with_max_tracked(2, 1.0, 1); // cap 1 IP, refills 1/s to burst 2
        let t0 = Instant::now();
        assert!(rl.check_at("old", t0)); // old now has 1 token left (non-idle)

        // 5s later "old" has refilled back to full (idle) → it's evicted to admit "new".
        let t5 = t0 + Duration::from_secs(5);
        assert!(rl.check_at("new", t5), "idle bucket reclaimed, new IP admitted");
    }
}
