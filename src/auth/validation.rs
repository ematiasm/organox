//! Authentication input validation.
//!
//! Validate at the boundary (before touching the DB) and return an actionable
//! error to the client. There is no dependency on a validation crate: the
//! rules are few and explicit.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("the email is invalid")]
    EmailInvalid,
    #[error("the password must contain at least {min} characters")]
    PasswordTooShort { min: usize },
    #[error("the password exceeds the maximum of {max} bytes")]
    PasswordTooLong { max: usize },
    #[error("the password is too common")]
    PasswordTooCommon,
}

/// Maximum in bytes, not characters: argon2 processes bytes and a 1 MB
/// password is a trivial DoS.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Minimal list of forbidden passwords. It grows as real cases arise; it does
/// not attempt to cover the entire space (that is the job of a dedicated
/// library when needed).
const FORBIDDEN_PASSWORDS: &[&str] = &[
    "password",
    "contrase\u{f1}a",
    "123456789012",
    "qwertyuiopas",
    "administrador",
    "adminadminadmin",
];

pub fn validate_password(password: &str) -> Result<(), ValidationError> {
    let len = password.chars().count();
    if len < super::password::MIN_PASSWORD_LEN {
        return Err(ValidationError::PasswordTooShort {
            min: super::password::MIN_PASSWORD_LEN,
        });
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(ValidationError::PasswordTooLong {
            max: MAX_PASSWORD_BYTES,
        });
    }
    let lowered = password.to_lowercase();
    if FORBIDDEN_PASSWORDS.contains(&lowered.as_str()) {
        return Err(ValidationError::PasswordTooCommon);
    }
    Ok(())
}

/// Normalize and validate an email.
///
/// Normalization trims whitespace and lowercases. The DB uses CITEXT, so the
/// unique constraint is case-insensitive anyway; this makes stored values
/// canonical and Rust comparisons independent of the DB.
pub fn normalize_email(raw: &str) -> Result<String, ValidationError> {
    let trimmed = raw.trim().to_lowercase();

    if !is_plausible_email(&trimmed) {
        return Err(ValidationError::EmailInvalid);
    }

    Ok(trimmed)
}

/// Deliberately simple validation: exactly one `@`, something before it, a
/// domain with at least one dot, and no spaces. Definitive validation that the
/// email exists requires sending mail; an RFC 5322 regex does not prove it.
fn is_plausible_email(email: &str) -> bool {
    if email.is_empty() || email.len() > 254 || email.contains(char::is_whitespace) {
        return false;
    }

    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };

    !local.is_empty()
        && local.len() <= 64
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
}

impl fmt::Display for ValidatedEmail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Email already passed through `normalize_email`. It cannot be constructed
/// by hand, so any function receiving it knows it is normalized and valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEmail(String);

impl ValidatedEmail {
    pub fn parse(raw: &str) -> Result<Self, ValidationError> {
        normalize_email(raw).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_case_and_whitespace() {
        let email = ValidatedEmail::parse("  Juan.Perez@PyME.COM  ").unwrap();
        assert_eq!(email.as_str(), "juan.perez@pyme.com");
    }

    #[test]
    fn rejects_malformed_emails() {
        for bad in [
            "",
            "sin-arroba.com",
            "dos@@arrobas.com",
            "dos@arrobas@com",
            "@dominio.com",
            "usuario@",
            "usuario@sindominio",
            "usuario@dominio.",
            "usuario@.dominio",
            "usuario@do..minio",
            "con espacio@dominio.com",
        ] {
            assert!(ValidatedEmail::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn accepts_realistic_emails() {
        // `a@b.c` is included deliberately: it is structurally valid and we
        // do not validate TLDs (the list ages and does not prove the mailbox exists).
        for good in [
            "juan@pyme.com",
            "juan.perez@pyme.com.ar",
            "j+tag@sub.dominio.co",
            "a@b.c",
        ] {
            assert!(
                ValidatedEmail::parse(good).is_ok(),
                "should accept {good:?}"
            );
        }
    }

    #[test]
    fn rejects_oversized_email() {
        let long_local = "a".repeat(65);
        assert!(ValidatedEmail::parse(&format!("{long_local}@dominio.com")).is_err());
    }
}
