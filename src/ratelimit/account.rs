//! Límite de intentos fallidos por cuenta, persistido en Postgres.
//!
//! Por qué en la DB y no en memoria: el bloqueo debe **sobrevivir un reinicio**.
//! Si reiniciar el servidor limpia el contador, el atacante solo necesita
//! esperar un deploy o tirar el proceso para recuperar sus intentos.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::AccountPolicy;
use crate::auth::validation::ValidatedEmail;

/// Estado del límite para una cuenta, después de evaluarla.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Puede intentar.
    Allowed { remaining: i32 },
    /// Bloqueada hasta el instante indicado.
    Blocked { until: DateTime<Utc> },
}

impl Decision {
    pub fn is_blocked(&self) -> bool {
        matches!(self, Decision::Blocked { .. })
    }
}

#[derive(Clone)]
pub struct AccountLimiter {
    pool: PgPool,
    policy: AccountPolicy,
}

impl AccountLimiter {
    pub fn new(pool: PgPool, policy: AccountPolicy) -> Self {
        Self { pool, policy }
    }

    pub fn policy(&self) -> AccountPolicy {
        self.policy
    }

    /// Consulta si la cuenta puede intentar un login.
    ///
    /// No bloquea ni incrementa: solo lee. El incremento lo hace
    /// [`record_failure`](Self::record_failure) cuando el intento falla.
    pub async fn check(&self, email: &ValidatedEmail) -> Result<Decision, sqlx::Error> {
        let row = sqlx::query!(
            r#"
            SELECT failed_count, blocked_until
            FROM login_attempts
            WHERE email = $1
            "#,
            email.as_str()
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            // Nunca intentó: puede.
            return Ok(Decision::Allowed {
                remaining: self.policy.max_failures,
            });
        };

        // Bloqueo vigente: se responde con el instante de expiración para que
        // el cliente pueda informar al usuario cuánto falta, sin revelar nada
        // que no sea el propio bloqueo de esa cuenta.
        if let Some(until) = row.blocked_until
            && until > Utc::now()
        {
            return Ok(Decision::Blocked { until });
        }

        // Bloqueo vencido: se libera el contador. Se hace acá, en lectura, para
        // no necesitar un job que limpie bloqueos.
        if row.blocked_until.is_some() {
            sqlx::query!(
                r#"
                UPDATE login_attempts
                SET failed_count = 0,
                    blocked_until = NULL,
                    window_start = now(),
                    updated_at = now()
                WHERE email = $1
                "#,
                email.as_str()
            )
            .execute(&self.pool)
            .await?;

            return Ok(Decision::Allowed {
                remaining: self.policy.max_failures,
            });
        }

        Ok(Decision::Allowed {
            remaining: (self.policy.max_failures - row.failed_count).max(0),
        })
    }

    /// Registra un intento fallido y, si corresponde, bloquea la cuenta.
    ///
    /// Es un `UPSERT` atómico: dos intentos simultáneos no pueden ambos
    /// "empezar de cero" y perder el conteo del otro.
    ///
    /// La ventana se reinicia cuando `now() > window_start + window`, así que
    /// los intentos viejos no suman para siempre.
    pub async fn record_failure(&self, email: &ValidatedEmail) -> Result<Decision, sqlx::Error> {
        let window_secs = self.policy.window.as_secs_f64();
        let block_secs = self.policy.block_duration.as_secs_f64();

        let row = sqlx::query!(
            r#"
            INSERT INTO login_attempts (email, failed_count, window_start, blocked_until, updated_at)
            VALUES ($1, 1, now(), NULL, now())
            ON CONFLICT (email) DO UPDATE
            SET failed_count = CASE
                    -- Ventana vencida: el conteo vuelve a empezar.
                    WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                        THEN 1
                    ELSE login_attempts.failed_count + 1
                END,
                window_start = CASE
                    WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                        THEN now()
                    ELSE login_attempts.window_start
                END,
                -- Se bloquea al alcanzar el máximo. El bloqueo se aplica aunque
                -- ya estuviera vencido el anterior: un intento nuevo lo renueva
                -- solo si vuelve a acumular fallas.
                blocked_until = CASE
                    WHEN (CASE
                            WHEN login_attempts.window_start + make_interval(secs => $2) < now()
                                THEN 1
                            ELSE login_attempts.failed_count + 1
                        END) >= $3
                        THEN now() + make_interval(secs => $4)
                    ELSE NULL
                END,
                updated_at = now()
            RETURNING failed_count, blocked_until
            "#,
            email.as_str(),
            window_secs,
            self.policy.max_failures,
            block_secs,
        )
        .fetch_one(&self.pool)
        .await?;

        match row.blocked_until {
            Some(until) if until > Utc::now() => Ok(Decision::Blocked { until }),
            _ => Ok(Decision::Allowed {
                remaining: (self.policy.max_failures - row.failed_count).max(0),
            }),
        }
    }

    /// Limpia el bloqueo tras un login exitoso.
    ///
    /// Necesario: sin esto, un usuario que se equivocó 5 veces y después
    /// recordó su contraseña queda bloqueado igual, y el bloqueo expira recién
    /// por tiempo. Un login correcto es prueba suficiente de que no es un
    /// atacante.
    pub async fn record_success(&self, email: &ValidatedEmail) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "DELETE FROM login_attempts WHERE email = $1",
            email.as_str()
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Borra contadores de cuentas sin actividad reciente.
    ///
    /// Sin esto la tabla crece con cada email que alguien tipeó mal una vez.
    /// Devuelve cuántas filas eliminó.
    pub async fn purge_stale(&self, older_than: std::time::Duration) -> Result<u64, sqlx::Error> {
        let result = sqlx::query!(
            r#"
            DELETE FROM login_attempts
            WHERE updated_at < now() - make_interval(secs => $1)
            "#,
            older_than.as_secs_f64()
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Los tests de comportamiento contra DB viven en tests/ratelimit.rs:
    // necesitan una base real y `#[sqlx::test]` no aplica a unitarios.

    #[test]
    fn decision_knows_when_blocked() {
        assert!(!Decision::Allowed { remaining: 3 }.is_blocked());
        assert!(Decision::Blocked { until: Utc::now() }.is_blocked());
    }
}
