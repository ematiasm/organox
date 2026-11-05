//! Session tokens.
//!
//! Desired properties:
//!  * 256 bits of entropy from a CSPRNG: guessable only by brute force.
//!  * the DB stores **only the SHA-256** of the token. Reading the table shows
//!    that a session exists, but does not allow using it.
//!  * constant-time comparison (`constant_time_eq`) so an attacker cannot
//!    extract information by measuring latency.
//!  * the token travels in an `HttpOnly; Secure; SameSite=Strict` cookie.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// A newly generated session token, still in plaintext. It lives only long
/// enough to write the cookie.
#[derive(Clone)]
pub struct SessionToken(String);

impl SessionToken {
    /// 32 bytes from `getrandom`. If the CSPRNG fails, there is no safe way to
    /// continue: the error is fatal to that request.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the system CSPRNG is unavailable");
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Reconstruct a token received from the client. It does not validate that
    /// we issued it: the DB lookup by hash does that.
    pub fn from_raw(raw: &str) -> Self {
        Self(raw.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// SHA-256 of the token, which is persisted and looked up.
    ///
    /// SHA-256 rather than argon2: the token already has 256 bits of entropy,
    /// there is no dictionary to attack, and this runs on every request.
    pub fn hash(&self) -> TokenHash {
        let digest = Sha256::digest(self.0.as_bytes());
        TokenHash(digest.to_vec())
    }
}

impl std::fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

/// SHA-256 of a session token. It is the primary key of `sessions`.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenHash(Vec<u8>);

impl TokenHash {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenHash(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_unique_tokens() {
        let a = SessionToken::generate();
        let b = SessionToken::generate();

        assert_ne!(a.as_str(), b.as_str());
        // 32 bytes in base64url without padding = 43 characters.
        assert_eq!(a.as_str().len(), 43);
    }

    #[test]
    fn hash_is_stable_and_differs_per_token() {
        let a = SessionToken::generate();
        let b = SessionToken::generate();

        assert_eq!(a.hash().as_bytes(), a.hash().as_bytes());
        assert_ne!(a.hash().as_bytes(), b.hash().as_bytes());
        assert_eq!(a.hash().as_bytes().len(), 32);
    }

    #[test]
    fn debug_does_not_leak_token() {
        let token = SessionToken::generate();
        let rendered = format!("{token:?}");

        assert!(!rendered.contains(token.as_str()), "Debug leaked the token");
    }
}
