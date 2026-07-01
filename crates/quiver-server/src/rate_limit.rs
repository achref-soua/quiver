// SPDX-License-Identifier: AGPL-3.0-only
//! Per-key token-bucket rate limiting (ADR-0049).
//!
//! ADR-0040 bounds the *size* of one request; this bounds the *rate* a single API
//! key may issue them, so no key can monopolize the single-writer engine by request
//! volume. It is opt-in (disabled when `requests_per_second == 0`), in-process
//! (Quiver is single-node), and keyed by the key's non-secret actor identity
//! (`Principal::actor()`) — the same identity the audit log uses.
//!
//! The [`RateLimiter`] holds one [token bucket] per key behind a single mutex; the
//! refill/consume math is a pure function of an injected `Instant`, so it is
//! deterministic and unit-tested with no sleeps.
//!
//! ## Scope: post-authentication by design (F-6)
//!
//! This limiter runs *after* authentication and is keyed by the authenticated
//! actor identity, so it holds at most one bucket **per configured key** — a
//! bounded map an attacker cannot inflate. The trade-off is that it does **not**
//! throttle *unauthenticated* traffic (requests that never present a valid key):
//! throttling anonymous floods by source IP is the job of an upstream reverse
//! proxy / load balancer / WAF, which production deployments already terminate TLS
//! at. A coarse pre-auth per-source limiter would need its own unbounded-by-source
//! map (itself a memory-exhaustion vector) and is deliberately left to that layer;
//! the bounded-by-keys property here is intentional, not an oversight.
//!
//! [token bucket]: https://en.wikipedia.org/wiki/Token_bucket

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Configuration for the per-key limiter. `requests_per_second == 0` (the default)
/// disables it entirely; like every other guard, it is opt-in.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Sustained requests per second per key (the bucket refill rate). `0` (the
    /// default) disables rate limiting.
    pub requests_per_second: u32,
    /// Maximum instantaneous burst (the bucket capacity). Defaults to
    /// `requests_per_second` when left at `0` and the limiter is enabled.
    pub burst: u32,
}

impl RateLimitConfig {
    /// Whether rate limiting is active.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.requests_per_second > 0
    }

    /// The effective bucket capacity (defaults to the per-second rate).
    fn capacity(&self) -> u32 {
        if self.burst > 0 {
            self.burst
        } else {
            self.requests_per_second
        }
    }

    /// Apply `QUIVER_RATE_LIMIT_*` env overrides (the flat env keys do not nest
    /// under the figment `rate_limit` table, mirroring ADR-0040's limits).
    ///
    /// # Errors
    /// Returns the offending key if a value is not a non-negative integer.
    pub fn apply_env_overrides(&mut self) -> Result<(), String> {
        for (key, slot) in [
            (
                "QUIVER_RATE_LIMIT_REQUESTS_PER_SECOND",
                &mut self.requests_per_second,
            ),
            ("QUIVER_RATE_LIMIT_BURST", &mut self.burst),
        ] {
            if let Ok(raw) = std::env::var(key) {
                *slot = raw
                    .parse()
                    .map_err(|_| format!("{key} must be a non-negative integer, got {raw:?}"))?;
            }
        }
        Ok(())
    }
}

/// A successful consume, surfaced as `RateLimit-*` response headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitSnapshot {
    /// The bucket capacity (`RateLimit-Limit`).
    pub limit: u32,
    /// Tokens left after this request (`RateLimit-Remaining`).
    pub remaining: u32,
    /// Seconds until the bucket is full again (`RateLimit-Reset`).
    pub reset_secs: u64,
}

/// The outcome of a rate-limit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The request is admitted; carry the snapshot to the response headers.
    Allowed(RateLimitSnapshot),
    /// The request is refused; reject with 429 / `ResourceExhausted`.
    Limited {
        /// Seconds the client should wait before retrying (`Retry-After`).
        retry_after_secs: u64,
        /// The bucket capacity (`RateLimit-Limit`).
        limit: u32,
    },
}

// One key's bucket: a fractional token count and the instant it was last refilled.
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// An opt-in, in-memory, per-key token-bucket rate limiter.
pub struct RateLimiter {
    config: RateLimitConfig,
    // ponytail: one global Mutex over the bucket map — fine for a single-node
    // server; shard by key hash if lock contention ever shows up under load.
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// A limiter for `config` (a no-op when `config.requests_per_second == 0`).
    #[must_use]
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Whether rate limiting is active (callers can skip the lock when not).
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.enabled()
    }

    /// Consume one token for `actor`, deciding whether the request is admitted.
    #[must_use]
    pub fn check(&self, actor: &str) -> RateDecision {
        self.check_at(actor, Instant::now())
    }

    // The pure core: `now` is injected so the refill math is deterministic in tests.
    fn check_at(&self, actor: &str, now: Instant) -> RateDecision {
        if !self.config.enabled() {
            return RateDecision::Allowed(RateLimitSnapshot {
                limit: 0,
                remaining: 0,
                reset_secs: 0,
            });
        }
        let capacity = f64::from(self.config.capacity());
        let rate = f64::from(self.config.requests_per_second);
        // Recover rather than panic if a previous holder panicked mid-update.
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = buckets.entry(actor.to_owned()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        // Refill for the elapsed time, capped at capacity.
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.last = now;

        let limit = self.config.capacity();
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            // Seconds to refill back to full from the current level.
            let reset_secs = ((capacity - bucket.tokens) / rate).ceil() as u64;
            RateDecision::Allowed(RateLimitSnapshot {
                limit,
                remaining: bucket.tokens as u32,
                reset_secs,
            })
        } else {
            // Seconds until at least one token is available again (≥ 1).
            let retry_after_secs = ((1.0 - bucket.tokens) / rate).ceil().max(1.0) as u64;
            RateDecision::Limited {
                retry_after_secs,
                limit,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(rps: u32, burst: u32) -> RateLimitConfig {
        RateLimitConfig {
            requests_per_second: rps,
            burst,
        }
    }

    #[test]
    fn disabled_limiter_always_admits() {
        let rl = RateLimiter::new(cfg(0, 0));
        assert!(!rl.enabled());
        for _ in 0..1000 {
            assert!(matches!(rl.check("k"), RateDecision::Allowed(_)));
        }
    }

    #[test]
    fn burst_is_allowed_then_the_next_request_is_limited() {
        let rl = RateLimiter::new(cfg(10, 3));
        let t0 = Instant::now();
        // Three tokens in the bucket → three admits, fourth refused (no time passed).
        for expected_remaining in [2, 1, 0] {
            match rl.check_at("k", t0) {
                RateDecision::Allowed(s) => {
                    assert_eq!(s.limit, 3);
                    assert_eq!(s.remaining, expected_remaining);
                }
                RateDecision::Limited { .. } => panic!("burst should be admitted"),
            }
        }
        match rl.check_at("k", t0) {
            RateDecision::Limited {
                retry_after_secs,
                limit,
            } => {
                assert_eq!(limit, 3);
                assert!(retry_after_secs >= 1);
            }
            RateDecision::Allowed(_) => panic!("4th request in a burst of 3 must be limited"),
        }
    }

    #[test]
    fn tokens_refill_at_the_configured_rate() {
        let rl = RateLimiter::new(cfg(2, 2)); // 2 tokens/sec, capacity 2
        let t0 = Instant::now();
        assert!(matches!(rl.check_at("k", t0), RateDecision::Allowed(_)));
        assert!(matches!(rl.check_at("k", t0), RateDecision::Allowed(_)));
        assert!(matches!(rl.check_at("k", t0), RateDecision::Limited { .. }));
        // After 1s, 2 tokens have refilled → two more admits.
        let t1 = t0 + Duration::from_secs(1);
        assert!(matches!(rl.check_at("k", t1), RateDecision::Allowed(_)));
        assert!(matches!(rl.check_at("k", t1), RateDecision::Allowed(_)));
        assert!(matches!(rl.check_at("k", t1), RateDecision::Limited { .. }));
    }

    #[test]
    fn keys_have_independent_buckets() {
        let rl = RateLimiter::new(cfg(5, 1));
        let t0 = Instant::now();
        assert!(matches!(rl.check_at("a", t0), RateDecision::Allowed(_)));
        assert!(matches!(rl.check_at("a", t0), RateDecision::Limited { .. }));
        // A different key is unaffected.
        assert!(matches!(rl.check_at("b", t0), RateDecision::Allowed(_)));
    }

    #[test]
    fn burst_defaults_to_the_per_second_rate() {
        let rl = RateLimiter::new(cfg(4, 0)); // burst unset → capacity 4
        let t0 = Instant::now();
        for _ in 0..4 {
            assert!(matches!(rl.check_at("k", t0), RateDecision::Allowed(_)));
        }
        assert!(matches!(rl.check_at("k", t0), RateDecision::Limited { .. }));
    }

    #[test]
    fn env_overrides_parse_and_reject_garbage() {
        let mut c = RateLimitConfig::default();
        // SAFETY: single-threaded test; set then clear.
        unsafe {
            std::env::set_var("QUIVER_RATE_LIMIT_REQUESTS_PER_SECOND", "25");
            std::env::set_var("QUIVER_RATE_LIMIT_BURST", "50");
        }
        c.apply_env_overrides().unwrap();
        assert_eq!(c.requests_per_second, 25);
        assert_eq!(c.burst, 50);
        unsafe {
            std::env::set_var("QUIVER_RATE_LIMIT_BURST", "lots");
        }
        assert!(c.apply_env_overrides().is_err());
        unsafe {
            std::env::remove_var("QUIVER_RATE_LIMIT_REQUESTS_PER_SECOND");
            std::env::remove_var("QUIVER_RATE_LIMIT_BURST");
        }
    }
}
