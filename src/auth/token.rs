//! Tokens de sesión.
//!
//! Propiedades que se buscan acá:
//!  * 256 bits de entropía de un CSPRNG: adivinable solo por fuerza bruta.
//!  * en la DB se guarda **solo el SHA-256** del token. Una lectura de la tabla
//!    permite ver que hay una sesión, no usarla.
//!  * comparación en tiempo constante (`constant_time_eq`) para que un atacante
//!    no pueda sacar información midiendo latencias.
//!  * el token viaja en una cookie `HttpOnly; Secure; SameSite=Strict`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Un token de sesión recién generado, todavía en claro. Vive solo el tiempo de
/// escribir la cookie.
#[derive(Clone)]
pub struct SessionToken(String);

impl SessionToken {
    /// 32 bytes de `getrandom`. Si el CSPRNG falla, no hay forma segura de
    /// seguir: el error es fatal para esa request.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("el CSPRNG del sistema no está disponible");
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    /// Reconstruye un token recibido del cliente. No valida que sea uno que
    /// emitimos: eso lo hace la búsqueda por hash en la DB.
    pub fn from_raw(raw: &str) -> Self {
        Self(raw.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// SHA-256 del token, que es lo que se persiste y lo que se busca.
    ///
    /// SHA-256 y no argon2: el token ya tiene 256 bits de entropía, no hay
    /// diccionario que atacar, y esta función se llama en cada request.
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

/// SHA-256 de un token de sesión. Es la clave primaria de `sessions`.
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
        // 32 bytes en base64url sin padding = 43 caracteres.
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

        assert!(
            !rendered.contains(token.as_str()),
            "el Debug filtró el token"
        );
    }
}
