//! In-memory per-IP request limit.
//!
//! # Why in memory
//!
//! This layer stops attacks **before** touching the database. An attacker
//! making 10,000 requests per second must not cause 10,000 Postgres queries
//! or 10,000 argon2 hashes: that would already be a successful denial-of-service
//! attack, even without guessing a password.
//!
//! # Accepted cost
//!
//! State lives in the process, so:
//!  * it is not shared across replicas: with N replicas, the effective limit is N×;
//!  * it is lost on restart.
//!
//! This is acceptable because this layer is a **first barrier**, not the main
//! defense. The defense surviving restarts and replicas is per-account
//! (Postgres). If a distributed per-IP limit is needed, move it to Redis
//! without changing the [`IpLimiter`] interface.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use super::IpPolicy;

/// Fixed window per IP.
///
/// A fixed rather than sliding window is chosen to stop spikes, not to be
/// millisecond-accurate, keeping per-IP state to two numbers.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    window_start: Instant,
    count: u32,
}

pub struct IpLimiter {
    policy: IpPolicy,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
    /// Entry cap to prevent unbounded map growth from spoofed IPs or scans
    /// from many origins.
    max_entries: usize,
}

impl IpLimiter {
    pub fn new(policy: IpPolicy) -> Self {
        Self {
            policy,
            buckets: Mutex::new(HashMap::new()),
            max_entries: 100_000,
        }
    }

    /// Record a request and say whether it is allowed.
    ///
    /// Returns `None` if allowed, or the suggested seconds to wait if rejected.
    pub fn check_and_record(&self, ip: IpAddr) -> Result<(), u64> {
        self.check_and_record_at(ip, Instant::now())
    }

    /// Variant with injectable time, for deterministic tests.
    fn check_and_record_at(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let limit = self.policy.max_requests.saturating_add(self.policy.burst);

        // A poisoned mutex must not crash the server: if it happens, the
        // limiter resets, degrading the defense rather than going down.
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Simple pruning: if the map grew too large, discard expired windows.
        // This happens only under pressure, so an LRU is not worthwhile.
        if buckets.len() > self.max_entries {
            buckets.retain(|_, b| now.duration_since(b.window_start) < self.policy.window);
        }

        let bucket = buckets.entry(ip).or_insert(Bucket {
            window_start: now,
            count: 0,
        });

        if now.duration_since(bucket.window_start) >= self.policy.window {
            bucket.window_start = now;
            bucket.count = 0;
        }

        if bucket.count >= limit {
            let elapsed = now.duration_since(bucket.window_start).as_secs();
            let retry_after = self.policy.window.as_secs().saturating_sub(elapsed).max(1);
            return Err(retry_after);
        }

        bucket.count += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn limiter(max: u32, burst: u32) -> IpLimiter {
        IpLimiter::new(IpPolicy {
            max_requests: max,
            window: Duration::from_secs(60),
            burst,
        })
    }

    #[test]
    fn allows_up_to_limit_then_rejects() {
        let limiter = limiter(3, 0);
        let ip = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();

        for _ in 0..3 {
            assert!(limiter.check_and_record_at(ip, now).is_ok());
        }
        assert_eq!(limiter.check_and_record_at(ip, now), Err(60));
    }

    #[test]
    fn burst_extends_the_allowed_count() {
        let limiter = limiter(3, 2);
        let ip = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();

        for _ in 0..5 {
            assert!(limiter.check_and_record_at(ip, now).is_ok());
        }
        assert!(limiter.check_and_record_at(ip, now).is_err());
    }

    #[test]
    fn ip_limits_are_independent() {
        let limiter = limiter(1, 0);
        let a = IpAddr::from([10, 0, 0, 1]);
        let b = IpAddr::from([10, 0, 0, 2]);
        let now = Instant::now();

        assert!(limiter.check_and_record_at(a, now).is_ok());
        assert!(limiter.check_and_record_at(a, now).is_err());
        // IP b does not suffer the consequences of IP a.
        assert!(limiter.check_and_record_at(b, now).is_ok());
    }

    #[test]
    fn window_resets_after_expiry() {
        let limiter = limiter(1, 0);
        let ip = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();

        assert!(limiter.check_and_record_at(ip, now).is_ok());
        assert!(limiter.check_and_record_at(ip, now).is_err());
        // Once the window expires, requests are allowed again.
        let later = now + Duration::from_secs(61);
        assert!(limiter.check_and_record_at(ip, later).is_ok());
    }

    #[test]
    fn retry_after_counts_down_within_window() {
        let limiter = limiter(1, 0);
        let ip = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();

        limiter.check_and_record_at(ip, now).unwrap();
        let retry = limiter
            .check_and_record_at(ip, now + Duration::from_secs(20))
            .unwrap_err();
        assert_eq!(retry, 40, "40s remain in a 60s window");
    }
}
