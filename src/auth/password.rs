//! Password hashing and verification with argon2id.
//!
//! Rules:
//!  * never compare plaintext passwords with `==`; always use verify.
//!  * the hash decides its parameters (included in the PHC string), so
//!    increasing costs does not invalidate old hashes.
//!  * `verify` is intentionally expensive: the service layer runs it in
//!    `spawn_blocking` to avoid blocking the tokio runtime.

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash as PhcHash};

use super::model::PasswordHash;
use super::validation::{ValidationError, validate_password};

/// Minimum password length. No composition rules (uppercase/symbols): the
/// evidence says they make passwords worse. What matters is length and that
/// the password is not on a common-password list.
pub const MIN_PASSWORD_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("invalid password: {0}")]
    Invalid(#[from] ValidationError),

    #[error("could not hash the password")]
    Hash,

    #[error("could not verify the password")]
    Verify,
}

/// Hash a new password, first validating the business rules.
///
/// Intentionally expensive (argon2id). Call from `spawn_blocking`.
pub fn hash(password: &str) -> Result<PasswordHash, PasswordError> {
    validate_password(password)?;

    let phc = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|_| PasswordError::Hash)?;

    Ok(PasswordHash::from_phc(phc.to_string()))
}

/// Verify a password against a stored hash.
///
/// Returns `Ok(false)` if it does not match and `Err` only if the stored hash
/// is corrupt (a bug, not a failed login attempt).
pub fn verify(password: &str, stored: &PasswordHash) -> Result<bool, PasswordError> {
    let parsed = PhcHash::new(stored.as_str()).map_err(|_| PasswordError::Verify)?;

    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::PasswordInvalid) => Ok(false),
        Err(_) => Err(PasswordError::Verify),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_accepts_correct_password_and_rejects_wrong_one() {
        let hash = hash("a-long-enough-password").expect("hash");

        assert!(verify("a-long-enough-password", &hash).unwrap());
        assert!(!verify("another-arbitrary-password", &hash).unwrap());
    }

    #[test]
    fn same_password_produces_different_hashes() {
        // Random salt per hash: two hashes of the same password must not
        // match; if they do, the salt is not being generated.
        let a = hash("a-long-enough-password").expect("hash");
        let b = hash("a-long-enough-password").expect("hash");

        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn rejects_short_passwords() {
        let err = hash("corta").expect_err("must reject");
        assert!(matches!(err, PasswordError::Invalid(_)));
    }

    #[test]
    fn debug_does_not_leak_hash() {
        let hash = hash("a-long-enough-password").expect("hash");
        let rendered = format!("{hash:?}");

        assert!(!rendered.contains("argon2"), "Debug leaked the hash");
    }
}
