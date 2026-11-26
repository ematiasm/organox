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

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::IpPolicy;

/// Default maximum number of active IP buckets tracked in memory.
pub const DEFAULT_MAX_ENTRIES: usize = 100_000;

/// Normalize a client IP into the rate-limit bucket key.
///
/// * IPv4 addresses are kept at `/32`.
/// * IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`, common on dual-stack
///   sockets) are unwrapped to their IPv4 address so dual-stack IPv4 clients
///   do not collapse into a single `::/64` bucket or bypass an IPv4 bucket.
/// * All other IPv6 addresses are masked to their `/64` prefix so rotating
///   host bits inside a single `/64` allocation shares one quota and one
///   bucket entry.
pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else {
                let [a, b, c, d, ..] = v6.segments();
                IpAddr::V6(Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0))
            }
        }
    }
}

/// Fixed window per IP.
///
/// A fixed rather than sliding window is chosen to stop spikes, not to be
/// millisecond-accurate, keeping per-IP state to two numbers.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    window_start: Instant,
    count: u32,
}

#[derive(Default)]
struct LimiterState {
    buckets: HashMap<IpAddr, Bucket>,
    /// Insertion and window-reset order for O(1) amortized expiry pruning.
    ///
    /// Every bucket shares the same fixed `policy.window`, so under monotonic
    /// `Instant`s the oldest window is always at the front. Stale entries left
    /// behind after an in-place window reset are skipped on pop because their
    /// recorded `window_start` no longer matches `buckets[key]`.
    expiry_queue: VecDeque<(IpAddr, Instant)>,
}

impl LimiterState {
    /// Pop one stale or expired entry from the front of `expiry_queue`.
    ///
    /// Returns `true` if an entry was popped, or `false` if the queue is empty
    /// or its front entry is still live and unexpired.
    fn pop_one_expired_or_stale(&mut self, now: Instant, window: Duration) -> bool {
        let Some(&(key, recorded_start)) = self.expiry_queue.front() else {
            return false;
        };

        let is_stale = self
            .buckets
            .get(&key)
            .is_none_or(|bucket| bucket.window_start != recorded_start);
        if is_stale {
            self.expiry_queue.pop_front();
            return true;
        }

        if now.saturating_duration_since(recorded_start) >= window {
            self.expiry_queue.pop_front();
            self.buckets.remove(&key);
            return true;
        }

        false
    }

    fn prune_expired_bounded(&mut self, now: Instant, window: Duration, max_pops: usize) {
        for _ in 0..max_pops {
            if !self.pop_one_expired_or_stale(now, window) {
                break;
            }
        }
    }
}

pub struct IpLimiter {
    policy: IpPolicy,
    state: Mutex<LimiterState>,
    /// Strict upper bound on tracked buckets to prevent unbounded memory growth
    /// and O(N) scans under distributed floods.
    max_entries: usize,
}

impl IpLimiter {
    pub fn new(policy: IpPolicy) -> Self {
        Self::with_max_entries(policy, DEFAULT_MAX_ENTRIES)
    }

    pub fn with_max_entries(policy: IpPolicy, max_entries: usize) -> Self {
        Self {
            policy,
            state: Mutex::new(LimiterState::default()),
            max_entries,
        }
    }

    /// Record a request and say whether it is allowed.
    ///
    /// Returns `Ok(())` if allowed, or `Err(retry_after)` with the suggested
    /// seconds to wait if rejected.
    pub fn check_and_record(&self, ip: IpAddr) -> Result<(), u64> {
        self.check_and_record_with_clock(ip, Instant::now)
    }

    /// Variant with injectable time, for deterministic tests.
    #[cfg(test)]
    fn check_and_record_at(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        self.check_and_record_with_clock(ip, || now)
    }

    /// Core check-and-record path.
    ///
    /// The clock is invoked **after** acquiring `self.state` so concurrent
    /// callers observe monotonically non-decreasing timestamps inside the
    /// critical section, preserving `expiry_queue`'s sorted order and O(1)
    /// amortized pruning invariants.
    fn check_and_record_with_clock(
        &self,
        ip: IpAddr,
        now_fn: impl FnOnce() -> Instant,
    ) -> Result<(), u64> {
        let key = normalize_ip(ip);
        let limit = self.policy.max_requests.saturating_add(self.policy.burst);
        if limit == 0 {
            return Err(self.policy.window.as_secs().max(1));
        }

        // A poisoned mutex must not crash the server: if it happens, reset
        // state once and clear the poison flag so subsequent requests resume
        // enforcing rate limits instead of wiping state on every call.
        let mut guard = match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.buckets.clear();
                guard.expiry_queue.clear();
                self.state.clear_poison();
                guard
            }
        };
        let state = &mut *guard;
        let now = now_fn();

        // Incremental O(1) pruning keeps `expiry_queue` bounded across resets
        // without ever doing an O(N) scan under the mutex.
        state.prune_expired_bounded(now, self.policy.window, 2);

        if let Some(bucket) = state.buckets.get_mut(&key) {
            if now.saturating_duration_since(bucket.window_start) >= self.policy.window {
                bucket.window_start = now;
                bucket.count = 0;
                state.expiry_queue.push_back((key, now));
            }

            if bucket.count >= limit {
                let elapsed = now.saturating_duration_since(bucket.window_start).as_secs();
                let retry_after = self.policy.window.as_secs().saturating_sub(elapsed).max(1);
                return Err(retry_after);
            }

            bucket.count += 1;
            return Ok(());
        }

        // New key: reclaim expired slots from the front of the queue if at
        // capacity. If all `max_entries` buckets are still active, fail closed
        // with 429 so neither memory nor mutex hold time grows unbounded.
        while state.buckets.len() >= self.max_entries {
            if !state.pop_one_expired_or_stale(now, self.policy.window) {
                break;
            }
        }

        if state.buckets.len() >= self.max_entries {
            let retry_after = state
                .expiry_queue
                .front()
                .map(|&(_, oldest_start)| {
                    let elapsed = now.saturating_duration_since(oldest_start).as_secs();
                    self.policy.window.as_secs().saturating_sub(elapsed).max(1)
                })
                .unwrap_or_else(|| self.policy.window.as_secs().max(1));
            return Err(retry_after);
        }

        state.buckets.insert(
            key,
            Bucket {
                window_start: now,
                count: 1,
            },
        );
        state.expiry_queue.push_back((key, now));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(max: u32, burst: u32) -> IpLimiter {
        IpLimiter::new(IpPolicy {
            max_requests: max,
            window: Duration::from_secs(60),
            burst,
        })
    }

    fn limiter_with_cap(max: u32, burst: u32, max_entries: usize) -> IpLimiter {
        IpLimiter::with_max_entries(
            IpPolicy {
                max_requests: max,
                window: Duration::from_secs(60),
                burst,
            },
            max_entries,
        )
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

    #[test]
    fn ipv6_addresses_in_same_64_share_quota_and_single_bucket() {
        let limiter = limiter(2, 0);
        let now = Instant::now();
        let a: IpAddr = "2001:db8:abcd:1234::1".parse().unwrap();
        let b: IpAddr = "2001:db8:abcd:1234::2".parse().unwrap();
        let c: IpAddr = "2001:db8:abcd:1234:ffff:ffff:ffff:ffff".parse().unwrap();

        assert!(limiter.check_and_record_at(a, now).is_ok());
        assert!(limiter.check_and_record_at(b, now).is_ok());
        assert_eq!(
            limiter.check_and_record_at(c, now),
            Err(60),
            "third host in the same /64 must share the quota"
        );
        let state = limiter.state.lock().unwrap();
        assert_eq!(
            state.buckets.len(),
            1,
            "same /64 prefix must consume only one bucket"
        );
        assert_eq!(state.expiry_queue.len(), 1);
    }

    #[test]
    fn ipv6_addresses_in_different_64_are_independent() {
        let limiter = limiter(1, 0);
        let now = Instant::now();
        let a: IpAddr = "2001:db8:abcd:1234::1".parse().unwrap();
        let b: IpAddr = "2001:db8:abcd:1235::1".parse().unwrap();

        assert!(limiter.check_and_record_at(a, now).is_ok());
        assert!(limiter.check_and_record_at(a, now).is_err());
        assert!(
            limiter.check_and_record_at(b, now).is_ok(),
            "distinct /64 prefixes must have independent quotas"
        );
    }

    #[test]
    fn ipv4_mapped_ipv6_unwraps_to_ipv4() {
        let limiter = limiter(1, 0);
        let now = Instant::now();
        let v4: IpAddr = "192.0.2.1".parse().unwrap();
        let mapped_same: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
        let mapped_other: IpAddr = "::ffff:192.0.2.2".parse().unwrap();

        assert!(limiter.check_and_record_at(v4, now).is_ok());
        assert_eq!(
            limiter.check_and_record_at(mapped_same, now),
            Err(60),
            "IPv4-mapped IPv6 must share the IPv4 bucket"
        );
        assert!(
            limiter.check_and_record_at(mapped_other, now).is_ok(),
            "different IPv4-mapped IPv6 addresses must not collapse into ::/64"
        );
        assert_eq!(normalize_ip(mapped_same), v4);
        assert_eq!(
            normalize_ip("2001:db8:1:2:3:4:5:6".parse().unwrap()),
            "2001:db8:1:2::".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn enforces_strict_upper_bound_and_fails_closed_at_capacity() {
        let limiter = limiter_with_cap(2, 0, 3);
        let now = Instant::now();

        let ip1 = IpAddr::from([10, 0, 0, 1]);
        let ip2 = IpAddr::from([10, 0, 0, 2]);
        let ip3 = IpAddr::from([10, 0, 0, 3]);
        let ip4 = IpAddr::from([10, 0, 0, 4]);

        assert!(limiter.check_and_record_at(ip1, now).is_ok());
        assert!(
            limiter
                .check_and_record_at(ip2, now + Duration::from_secs(5))
                .is_ok()
        );
        assert!(
            limiter
                .check_and_record_at(ip3, now + Duration::from_secs(10))
                .is_ok()
        );

        // Map is at capacity with 3 active entries: a 4th distinct IP fails closed.
        assert_eq!(
            limiter.check_and_record_at(ip4, now + Duration::from_secs(10)),
            Err(50),
            "new IP past max_entries must fail closed with retry_after until oldest expires"
        );
        {
            let state = limiter.state.lock().unwrap();
            assert_eq!(
                state.buckets.len(),
                3,
                "map size must never exceed max_entries"
            );
            assert_eq!(state.expiry_queue.len(), 3);
        }

        // Already-tracked IP keeps its remaining quota even while at capacity.
        assert!(
            limiter
                .check_and_record_at(ip1, now + Duration::from_secs(10))
                .is_ok()
        );
        assert_eq!(
            limiter.check_and_record_at(ip1, now + Duration::from_secs(10)),
            Err(50)
        );
    }

    #[test]
    fn reclaims_expired_buckets_when_at_capacity() {
        let limiter = limiter_with_cap(1, 0, 2);
        let now = Instant::now();

        let ip1 = IpAddr::from([10, 0, 0, 1]);
        let ip2 = IpAddr::from([10, 0, 0, 2]);
        let ip3 = IpAddr::from([10, 0, 0, 3]);
        let ip4 = IpAddr::from([10, 0, 0, 4]);

        assert!(limiter.check_and_record_at(ip1, now).is_ok());
        assert!(
            limiter
                .check_and_record_at(ip2, now + Duration::from_secs(15))
                .is_ok()
        );
        assert_eq!(
            limiter.check_and_record_at(ip3, now + Duration::from_secs(30)),
            Err(30),
            "oldest bucket expires in 30s"
        );

        // Once ip1 expires at t=60, ip3 reclaims its slot while ip2 stays active.
        assert!(
            limiter
                .check_and_record_at(ip3, now + Duration::from_secs(60))
                .is_ok()
        );
        assert_eq!(
            limiter.check_and_record_at(ip4, now + Duration::from_secs(65)),
            Err(10),
            "oldest active bucket (ip2, inserted at t=15) expires in 10s"
        );
        assert!(
            limiter
                .check_and_record_at(ip4, now + Duration::from_secs(75))
                .is_ok()
        );

        let state = limiter.state.lock().unwrap();
        assert!(state.buckets.len() <= 2);
        assert!(state.expiry_queue.len() <= 2);
    }

    #[test]
    fn expiry_queue_stays_bounded_across_repeated_window_resets() {
        let cap = 8;
        let limiter = limiter_with_cap(2, 0, cap);
        let base = Instant::now();

        // Fill capacity at t=0.
        for i in 0..cap as u8 {
            assert!(
                limiter
                    .check_and_record_at(IpAddr::from([10, 0, 0, i]), base)
                    .is_ok()
            );
        }

        // Repeatedly reset existing keys in reverse order across multiple windows.
        for round in 1..=5u64 {
            let t = base + Duration::from_secs(round * 60);
            for i in (0..cap as u8).rev() {
                assert!(
                    limiter
                        .check_and_record_at(IpAddr::from([10, 0, 0, i]), t)
                        .is_ok()
                );
                let state = limiter.state.lock().unwrap();
                assert!(
                    state.buckets.len() <= cap,
                    "buckets exceeded cap on round {round}, ip {i}"
                );
                assert!(
                    state.expiry_queue.len() <= cap,
                    "expiry_queue exceeded cap on round {round}, ip {i}"
                );
            }
        }
    }

    #[test]
    fn zero_limit_and_zero_max_entries_fail_closed_without_storing_buckets() {
        let now = Instant::now();
        let ip = IpAddr::from([10, 0, 0, 1]);

        let zero_limit = limiter(0, 0);
        assert_eq!(zero_limit.check_and_record_at(ip, now), Err(60));
        assert_eq!(zero_limit.state.lock().unwrap().buckets.len(), 0);

        let zero_cap = limiter_with_cap(5, 0, 0);
        assert_eq!(zero_cap.check_and_record_at(ip, now), Err(60));
        assert_eq!(zero_cap.state.lock().unwrap().buckets.len(), 0);
    }

    #[test]
    fn poisoned_mutex_resets_once_and_continues_enforcing_limits() {
        let limiter = std::sync::Arc::new(limiter_with_cap(2, 0, 10));
        let poisoned_ref = std::sync::Arc::clone(&limiter);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned_ref.state.lock().unwrap();
            panic!("simulated panic while holding limiter lock");
        })
        .join();

        let ip = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();
        assert!(limiter.check_and_record_at(ip, now).is_ok());
        assert!(limiter.check_and_record_at(ip, now).is_ok());
        assert_eq!(
            limiter.check_and_record_at(ip, now),
            Err(60),
            "limiter must continue enforcing quota after recovering from mutex poison"
        );
    }

    #[test]
    fn concurrent_check_and_record_keeps_expiry_queue_monotonic() {
        let limiter = std::sync::Arc::new(limiter_with_cap(1, 0, 10_000));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();

        for t in 0..8u8 {
            let lim = std::sync::Arc::clone(&limiter);
            let bar = std::sync::Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                bar.wait();
                for i in 0..500u16 {
                    let ip = IpAddr::from([10, t, (i >> 8) as u8, (i & 0xff) as u8]);
                    let _ = lim.check_and_record(ip);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let state = limiter.state.lock().unwrap();
        let (first, second) = state.expiry_queue.as_slices();
        let timestamps: Vec<Instant> = first
            .iter()
            .chain(second.iter())
            .map(|&(_, ts)| ts)
            .collect();
        for pair in timestamps.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "expiry_queue timestamps must be monotonically non-decreasing under concurrency"
            );
        }
    }

    #[test]
    fn normalize_ip_handles_ipv6_boundaries_and_special_addresses() {
        assert_eq!(
            normalize_ip("2001:db8:0:1:8000::".parse().unwrap()),
            "2001:db8:0:1::".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            normalize_ip("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap()),
            "ffff:ffff:ffff:ffff::".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            normalize_ip("::ffff:0.0.0.0".parse().unwrap()),
            "0.0.0.0".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            normalize_ip("::ffff:255.255.255.255".parse().unwrap()),
            "255.255.255.255".parse::<IpAddr>().unwrap()
        );
        // Deprecated IPv4-compatible IPv6 must NOT unwrap to IPv4.
        assert_eq!(
            normalize_ip("::192.0.2.1".parse().unwrap()),
            "::".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            normalize_ip("::1".parse().unwrap()),
            "::".parse::<IpAddr>().unwrap()
        );
        assert_ne!(
            normalize_ip("::1".parse().unwrap()),
            normalize_ip("127.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn extreme_policy_values_stay_bounded_without_overflow() {
        let now = Instant::now();
        let ip = IpAddr::from([10, 0, 0, 1]);

        let zero_win_zero_cap = IpLimiter::with_max_entries(
            IpPolicy {
                max_requests: 1,
                window: Duration::ZERO,
                burst: 0,
            },
            0,
        );
        assert_eq!(zero_win_zero_cap.check_and_record_at(ip, now), Err(1));

        let saturating = IpLimiter::with_max_entries(
            IpPolicy {
                max_requests: u32::MAX,
                window: Duration::from_secs(60),
                burst: u32::MAX,
            },
            2,
        );
        assert!(saturating.check_and_record_at(ip, now).is_ok());
    }
}
