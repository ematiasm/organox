//! Brute-force protection.
//!
//! # What it is and is not
//!
//! Rate limiting does **not prevent** abuse: it **bounds** it. It does not stop
//! someone trying to guess a password; it makes the attempt slow, finite and
//! observable. Any design promising to "prevent" abuse is lying or will deny
//! service to legitimate users.
//!
//! # Two layers, two threats
//!
//! | Layer | Targets | Where | Survives restart |
//! |---|---|---|---|
//! | Per account ([`account`]) | Vertical attack: many IPs against ONE user | Postgres | Yes |
//! | Per IP ([`ip`]) | Horizontal attack: one IP against many users | Memory | No |
//!
//! Both are necessary. IP only: a botnet bypasses it. Account only: the
//! attacker can try one password per account across 100,000 accounts.
//!
//! # Why the lock is soft
//!
//! A hard lock (until manual intervention) turns rate limiting into a
//! **denial-of-service vector**: anyone knowing the owner's email can lock them
//! out of their own system by entering the wrong password five times. Therefore
//! the lock lasts minutes and expires on its own.

pub mod account;
pub mod ip;
pub mod middleware;

pub use account::{AccountLimiter, Decision};
pub use ip::IpLimiter;

use std::net::IpAddr;
use std::time::Duration;

/// Per-account rate-limit policy.
#[derive(Debug, Clone, Copy)]
pub struct AccountPolicy {
    /// Failed attempts allowed within the window.
    ///
    /// With `max_failures = 3`: the first three failures return 401 and the
    /// fourth is not evaluated (429). The attempt triggering the lock still
    /// returns 401 because its password was indeed incorrect.
    pub max_failures: i32,
    /// Duration of the counting window.
    pub window: Duration,
    /// Lock duration once the maximum is reached.
    pub block_duration: Duration,
}

impl Default for AccountPolicy {
    fn default() -> Self {
        Self {
            max_failures: 5,
            window: Duration::from_secs(15 * 60),
            // 5 minutes: enough to make brute force infeasible (5 attempts
            // every 5 min = 1440/day against ~10^12 combinations), and short
            // enough that a real user who made a mistake does not wait half an hour.
            block_duration: Duration::from_secs(5 * 60),
        }
    }
}

/// Per-IP rate-limit policy.
#[derive(Debug, Clone, Copy)]
pub struct IpPolicy {
    /// Requests allowed per window, per IP.
    pub max_requests: u32,
    /// Limiter window.
    pub window: Duration,
    /// Burst tolerance above the sustained rate.
    pub burst: u32,
}

impl Default for IpPolicy {
    fn default() -> Self {
        Self {
            // 30 logins per minute from one IP: a human makes 1 or 2. The
            // margin covers offices with NAT (multiple employees behind one
            // public IP) without opening the door to brute force.
            max_requests: 30,
            window: Duration::from_secs(60),
            burst: 10,
        }
    }
}

/// Extract the client's IP.
///
/// `ConnectInfo` is the trusted source: it is the TCP connection's IP.
///
/// `X-Forwarded-For` is ignored **unless** a trusted proxy is configured,
/// because the client controls the header: honoring it without a proxy lets
/// an attacker choose their own IP on each request and completely bypass rate
/// limiting. This is the most common way to break this defense.
pub fn client_ip(peer: Option<IpAddr>) -> IpAddr {
    // Without ConnectInfo (e.g. tests without `into_make_service_with_connect_info`),
    // group everything under a loopback IP: preferable to disabling the limit.
    peer.unwrap_or(IpAddr::from([127, 0, 0, 1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_restrictive_but_human_friendly() {
        let account = AccountPolicy::default();
        assert_eq!(account.max_failures, 5);
        assert!(account.block_duration <= Duration::from_secs(15 * 60));

        let ip = IpPolicy::default();
        assert!(ip.max_requests >= 10);
        assert!(ip.max_requests <= 100);
    }

    #[test]
    fn missing_peer_ip_falls_back_to_loopback() {
        assert_eq!(client_ip(None), IpAddr::from([127, 0, 0, 1]));
        assert_eq!(
            client_ip(Some(IpAddr::from([10, 0, 0, 1]))),
            IpAddr::from([10, 0, 0, 1])
        );
    }
}
