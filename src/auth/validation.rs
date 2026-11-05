//! Validación de entradas de autenticación.
//!
//! Se valida en el borde (antes de tocar la DB) y se devuelve un error que el
//! cliente puede accionar. No hay dependencia de un crate de validación: las
//! reglas son pocas y explícitas.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("el email no es válido")]
    EmailInvalid,
    #[error("la contraseña debe tener al menos {min} caracteres")]
    PasswordTooShort { min: usize },
    #[error("la contraseña supera el máximo de {max} bytes")]
    PasswordTooLong { max: usize },
    #[error("la contraseña es demasiado común")]
    PasswordTooCommon,
}

/// Máximo en bytes, no en caracteres: argon2 procesa bytes y una contraseña de
/// 1 MB es un DoS trivial.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Lista mínima de contraseñas prohibidas. Crece cuando aparezcan casos
/// reales; no pretendo cubrir el espacio completo acá (eso es trabajo de
/// una librería dedicada el día que haga falta).
const FORBIDDEN_PASSWORDS: &[&str] = &[
    "password",
    "contraseña",
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

/// Normaliza y valida un email.
///
/// Normalización: recorta espacios y baja a minúsculas. La DB usa CITEXT, así
/// que el unique es case-insensitive igual; esto es para que lo que se guarda
/// sea canónico y las comparaciones en Rust no dependan de la DB.
pub fn normalize_email(raw: &str) -> Result<String, ValidationError> {
    let trimmed = raw.trim().to_lowercase();

    if !is_plausible_email(&trimmed) {
        return Err(ValidationError::EmailInvalid);
    }

    Ok(trimmed)
}

/// Validación deliberadamente simple: exactamente un `@`, algo antes, un
/// dominio con al menos un punto y sin espacios. La validación definitiva de
/// que el email existe es mandarle un correo; una regex RFC 5322 no prueba eso.
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

/// Email que ya pasó por `normalize_email`. No se puede construir a mano, y por
/// eso cualquier función que lo reciba sabe que está normalizado y válido.
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
            assert!(
                ValidatedEmail::parse(bad).is_err(),
                "debió rechazar {bad:?}"
            );
        }
    }

    #[test]
    fn accepts_realistic_emails() {
        // `a@b.c` incluido a propósito: es estructuralmente válido y no
        // validamos TLDs (la lista envejece y no prueba que el buzón exista).
        for good in [
            "juan@pyme.com",
            "juan.perez@pyme.com.ar",
            "j+tag@sub.dominio.co",
            "a@b.c",
        ] {
            assert!(
                ValidatedEmail::parse(good).is_ok(),
                "debió aceptar {good:?}"
            );
        }
    }

    #[test]
    fn rejects_oversized_email() {
        let long_local = "a".repeat(65);
        assert!(ValidatedEmail::parse(&format!("{long_local}@dominio.com")).is_err());
    }
}
