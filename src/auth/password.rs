//! Hash y verificación de contraseñas con argon2id.
//!
//! Reglas:
//!  * nunca comparamos contraseñas en claro con `==`; siempre vía verify.
//!  * el hash es el que decide sus parámetros (vienen en el PHC string), así
//!    que subir los costos no invalida hashes viejos.
//!  * `verify` es intentionally costoso: se corre en `spawn_blocking` desde la
//!    capa de servicio para no bloquear el runtime de tokio.

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash as PhcHash};

use super::model::PasswordHash;
use super::validation::{ValidationError, validate_password};

/// Largo mínimo de contraseña. No impongo reglas de composición
/// (mayúsculas/símbolos): la evidencia dice que empeoran las contraseñas.
/// Lo que sí importa es el largo y que no esté en una lista de las comunes.
pub const MIN_PASSWORD_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("contraseña inválida: {0}")]
    Invalid(#[from] ValidationError),

    #[error("no se pudo hashear la contraseña")]
    Hash,

    #[error("no se pudo verificar la contraseña")]
    Verify,
}

/// Hashea una contraseña nueva, validando las reglas de negocio antes.
///
/// Es costoso a propósito (argon2id). Llamar desde `spawn_blocking`.
pub fn hash(password: &str) -> Result<PasswordHash, PasswordError> {
    validate_password(password)?;

    let phc = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|_| PasswordError::Hash)?;

    Ok(PasswordHash::from_phc(phc.to_string()))
}

/// Verifica una contraseña contra un hash almacenado.
///
/// Devuelve `Ok(false)` cuando no coincide y `Err` solo si el hash almacenado
/// está corrupto (lo que sería un bug, no un intento de login fallido).
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
        let hash = hash("una-contraseña-bastante-larga").expect("hash");

        assert!(verify("una-contraseña-bastante-larga", &hash).unwrap());
        assert!(!verify("otra-contraseña-cualquiera", &hash).unwrap());
    }

    #[test]
    fn same_password_produces_different_hashes() {
        // Salt aleatorio por hash: dos hashes de la misma contraseña no deben
        // coincidir, si coinciden el salt no se está generando.
        let a = hash("una-contraseña-bastante-larga").expect("hash");
        let b = hash("una-contraseña-bastante-larga").expect("hash");

        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn rejects_short_passwords() {
        let err = hash("corta").expect_err("debe rechazar");
        assert!(matches!(err, PasswordError::Invalid(_)));
    }

    #[test]
    fn debug_does_not_leak_hash() {
        let hash = hash("una-contraseña-bastante-larga").expect("hash");
        let rendered = format!("{hash:?}");

        assert!(!rendered.contains("argon2"), "el Debug filtró el hash");
    }
}
