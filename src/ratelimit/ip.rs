//! Límite de requests por IP, en memoria.
//!
//! # Por qué en memoria
//!
//! Es la capa que corta el ataque **antes** de tocar la base de datos. Un
//! atacante que hace 10 000 requests por segundo no debe provocar 10 000
//! queries a Postgres ni 10 000 hashes de argon2: eso ya sería un ataque de
//! denegación de servicio exitoso, aunque nunca adivine una contraseña.
//!
//! # Costo asumido
//!
//! El estado vive en el proceso, así que:
//!  * no se comparte entre réplicas: con N réplicas, el límite efectivo es N×;
//!  * se pierde al reiniciar.
//!
//! Es aceptable porque esta capa es una **primera barrera**, no la defensa
//! principal. La defensa que sobrevive reinicios y réplicas es la de por cuenta
//! (Postgres). Si algún día se necesita un límite por IP distribuido, se mueve a
//! Redis sin cambiar la interfaz de [`IpLimiter`].

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use super::IpPolicy;

/// Ventana fija por IP.
///
/// Se elige ventana fija y no deslizante porque el objetivo es cortar picos, no
/// ser exacto al milisegundo, y así el estado por IP es de dos números.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    window_start: Instant,
    count: u32,
}

pub struct IpLimiter {
    policy: IpPolicy,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
    /// Techo de entradas para que el mapa no crezca sin límite con IPs
    /// spoofeadas o un escaneo desde muchos orígenes.
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

    /// Registra un request y dice si está permitido.
    ///
    /// Devuelve `None` si se permite, o los segundos sugeridos de espera si se
    /// rechaza.
    pub fn check_and_record(&self, ip: IpAddr) -> Result<(), u64> {
        self.check_and_record_at(ip, Instant::now())
    }

    /// Variante con tiempo inyectable, para tests deterministas.
    fn check_and_record_at(&self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        let limit = self.policy.max_requests.saturating_add(self.policy.burst);

        // Un mutex envenenado no debe tumbar el servidor: si eso pasa, el
        // limitador se reinicia, lo que es degradar la defensa y no caer.
        let mut buckets = match self.buckets.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Poda simple: si el mapa creció demasiado, se descartan las ventanas
        // vencidas. Solo ocurre bajo presión, así que no vale un LRU.
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
        // La IP b no paga los platos rotos de la a.
        assert!(limiter.check_and_record_at(b, now).is_ok());
    }

    #[test]
    fn window_resets_after_expiry() {
        let limiter = limiter(1, 0);
        let ip = IpAddr::from([10, 0, 0, 1]);
        let now = Instant::now();

        assert!(limiter.check_and_record_at(ip, now).is_ok());
        assert!(limiter.check_and_record_at(ip, now).is_err());
        // Pasada la ventana, se vuelve a permitir.
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
        assert_eq!(retry, 40, "faltan 40s de una ventana de 60s");
    }
}
