//! Protección contra fuerza bruta.
//!
//! # Qué es y qué no es
//!
//! Rate limiting **no prohíbe** el abuso: lo **acota**. No impide que alguien
//! intente adivinar una contraseña; hace que intentarlo sea lento, finito y
//! observable. Cualquier diseño que prometa "prohibir" el abuso está mintiendo o
//! va a negar servicio a usuarios legítimos.
//!
//! # Dos capas, dos amenazas
//!
//! | Capa | Ataca a | Dónde vive | Sobrevive reinicio |
//! |---|---|---|---|
//! | Por cuenta ([`account`]) | Ataque vertical: muchas IPs contra UN usuario | Postgres | Sí |
//! | Por IP ([`ip`]) | Ataque horizontal: una IP contra muchos usuarios | Memoria | No |
//!
//! Las dos son necesarias. Solo por IP: un botnet lo esquiva. Solo por cuenta:
//! el atacante puede probar una contraseña por cuenta en 100 000 cuentas.
//!
//! # Por qué el bloqueo es suave
//!
//! Un bloqueo duro (hasta intervención manual) convierte el rate limiting en un
//! **vector de denegación de servicio**: cualquiera que conozca el email del
//! dueño lo deja fuera de su propio sistema escribiendo mal la contraseña cinco
//! veces. Por eso el bloqueo dura minutos y expira solo.

pub mod account;
pub mod ip;
pub mod middleware;

pub use account::{AccountLimiter, Decision};
pub use ip::IpLimiter;

use std::net::IpAddr;
use std::time::Duration;

/// Política del límite por cuenta.
#[derive(Debug, Clone, Copy)]
pub struct AccountPolicy {
    /// Intentos fallidos permitidos dentro de la ventana.
    ///
    /// Con `max_failures = 3`: los tres primeros fallos responden 401 y el
    /// cuarto ya no se evalúa (429). El intento que dispara el bloqueo sigue
    /// devolviendo 401, porque su contraseña era efectivamente incorrecta.
    pub max_failures: i32,
    /// Duración de la ventana de conteo.
    pub window: Duration,
    /// Cuánto dura el bloqueo una vez alcanzado el máximo.
    pub block_duration: Duration,
}

impl Default for AccountPolicy {
    fn default() -> Self {
        Self {
            max_failures: 5,
            window: Duration::from_secs(15 * 60),
            // 5 minutos: suficiente para que un ataque por fuerza bruta sea
            // inviable (5 intentos cada 5 min = 1440/día contra ~10^12
            // combinaciones), y corto para que un usuario real que se equivocó
            // no quede esperando media hora.
            block_duration: Duration::from_secs(5 * 60),
        }
    }
}

/// Política del límite por IP.
#[derive(Debug, Clone, Copy)]
pub struct IpPolicy {
    /// Requests permitidos por ventana, por IP.
    pub max_requests: u32,
    /// Ventana del limitador.
    pub window: Duration,
    /// Cuánto tolerar ráfagas por encima del ritmo sostenido.
    pub burst: u32,
}

impl Default for IpPolicy {
    fn default() -> Self {
        Self {
            // 30 logins por minuto desde una IP: un humano hace 1 o 2. El
            // margen cubre oficinas con NAT (varios empleados detrás de una
            // sola IP pública) sin abrir la puerta a fuerza bruta.
            max_requests: 30,
            window: Duration::from_secs(60),
            burst: 10,
        }
    }
}

/// Extrae la IP del cliente.
///
/// `ConnectInfo` es la fuente confiable: es la IP de la conexión TCP.
///
/// `X-Forwarded-For` se ignora **salvo** que haya un proxy confiable
/// configurado, porque es un header que el cliente controla: honrarlo sin
/// proxy permite a un atacante elegir su propia IP en cada request y anular el
/// rate limiting por completo. Es la forma más común de romper esta defensa.
pub fn client_ip(peer: Option<IpAddr>) -> IpAddr {
    // Sin ConnectInfo (por ejemplo en tests sin `into_make_service_with_connect_info`),
    // se agrupa todo bajo una IP de loopback: preferible a desactivar el límite.
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
