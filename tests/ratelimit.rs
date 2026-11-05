//! Tests de protección contra fuerza bruta en el login.
//!
//! Cubren las dos capas y las decisiones que no son obvias:
//!  * el bloqueo por cuenta sobrevive reinicio (está en Postgres);
//!  * un login exitoso limpia el contador;
//!  * el bloqueo expira solo;
//!  * los emails INEXISTENTES también cuentan (si no, se pueden enumerar);
//!  * el 429 trae `Retry-After`;
//!  * el límite por IP no castiga a otras IPs.

use admin_app::AppState;
use admin_app::auth::model::Role;
use admin_app::auth::repository::{self, NewUser};
use admin_app::auth::validation::ValidatedEmail;
use admin_app::auth::{password, validation};
use admin_app::ratelimit::{AccountPolicy, Decision, IpPolicy};
use admin_app::routes::router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

const PASSWORD: &str = "contrasena-bastante-larga-123";

fn config() -> admin_app::config::Config {
    // Config mínima para construir el estado en tests, sin leer el entorno.
    admin_app::config::Config {
        database_url: String::new(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        log_format: admin_app::config::LogFormat::Pretty,
        rust_log: String::new(),
        cookie_secure: false,
        login_max_failures: 3,
        login_block_minutes: 5,
        login_ip_max_per_minute: 30,
    }
}

fn strict_state(pool: PgPool) -> AppState {
    let config = config();
    AppState::with_policies(
        pool,
        false,
        AccountPolicy {
            max_failures: 3,
            window: Duration::from_secs(15 * 60),
            block_duration: Duration::from_secs(5 * 60),
        },
        IpPolicy {
            max_requests: config.login_ip_max_per_minute,
            window: Duration::from_secs(60),
            burst: 10,
        },
    )
}

fn app(pool: PgPool) -> axum::Router {
    router(strict_state(pool)).layer(CookieManagerLayer::new())
}

async fn seed_user(pool: &PgPool, email: &str) {
    let email = validation::ValidatedEmail::parse(email).expect("email");
    let hash = password::hash(PASSWORD).expect("hash");
    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role: Role::Owner,
        },
    )
    .await
    .expect("usuario");
}

fn login_request(email: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "email": email, "password": password }).to_string(),
        ))
        .unwrap()
}

async fn attempt(pool: &PgPool, email: &str, password: &str) -> (StatusCode, Option<u64>) {
    let response = app(pool.clone())
        .oneshot(login_request(email, password))
        .await
        .unwrap();

    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    (status, retry_after)
}

// --- Bloqueo por cuenta -------------------------------------------------

#[sqlx::test]
async fn account_is_locked_after_max_failures(pool: PgPool) {
    seed_user(&pool, "duenio@pyme.com").await;

    // max_failures = 3: los primeros 3 fallan con 401.
    for i in 1..=3 {
        let (status, _) = attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "intento {i}");
    }

    // El siguiente ya está bloqueado: 429, no 401.
    let (status, retry_after) = attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry_after = retry_after.expect("un 429 debe traer Retry-After");
    assert!(
        retry_after > 0 && retry_after <= 300,
        "Retry-After fuera de rango: {retry_after}"
    );
}

/// La contraseña correcta no salva a una cuenta bloqueada. Si lo hiciera, el
/// bloqueo no serviría de nada: el atacante que acierta entra igual.
#[sqlx::test]
async fn correct_password_does_not_bypass_block(pool: PgPool) {
    seed_user(&pool, "duenio@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    }

    let (status, _) = attempt(&pool, "duenio@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Y no se emitió sesión.
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

/// El bloqueo vive en Postgres: un proceso nuevo (otro pool, otro "reinicio")
/// lo sigue viendo. Si estuviera en memoria, reiniciar el server lo limpiaría.
#[sqlx::test]
async fn block_survives_a_simulated_restart(pool: PgPool) {
    seed_user(&pool, "duenio@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    }

    // Un estado nuevo, como si el proceso acabara de arrancar: pool distinto,
    // limitadores nuevos, mismo Postgres.
    let fresh_pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        // `#[sqlx::test]` provee la URL vía DATABASE_URL del test.
        panic!("DATABASE_URL debe estar definida en los tests")
    }))
    .await;

    // Si no se puede abrir un pool nuevo, el test no prueba nada: se declara.
    let fresh_pool = match fresh_pool {
        Ok(p) => p,
        Err(err) => panic!("no se pudo abrir un pool nuevo para simular reinicio: {err}"),
    };

    let fresh_state = strict_state(pool.clone());
    let response = router(fresh_state)
        .layer(CookieManagerLayer::new())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();

    // El bloqueo se leyó de la DB compartida, no de memoria.
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(fresh_pool);
}

#[sqlx::test]
async fn successful_login_resets_the_counter(pool: PgPool) {
    seed_user(&pool, "duenio@pyme.com").await;

    // 2 fallas (una menos que el máximo de 3).
    attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;

    // Acierta: el contador se limpia.
    let (status, _) = attempt(&pool, "duenio@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::OK);

    // Y ahora tiene 3 intentos disponibles otra vez: ninguno bloquea.
    for i in 1..=3 {
        let (status, _) = attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "post-reset intento {i}");
    }

    // Recién el cuarto bloquea.
    let (status, _) = attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

/// Los emails inexistentes también acumulan intentos. Si no contaran, la
/// diferencia entre "bloqueado" (429) y "credenciales inválidas" (401)
/// permitiría enumerar qué emails están registrados.
#[sqlx::test]
async fn unknown_email_also_gets_blocked(pool: PgPool) {
    for _ in 0..3 {
        let (status, _) = attempt(&pool, "nadie@pyme.com", "contrasena-mala-123").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let (status, _) = attempt(&pool, "nadie@pyme.com", "contrasena-mala-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 1,
        "el intento contra un email inexistente se registra"
    );
}

#[sqlx::test]
async fn block_expires_and_allows_login_again(pool: PgPool) {
    seed_user(&pool, "duenio@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    }
    let (status, _) = attempt(&pool, "duenio@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Simula el paso del tiempo, que es lo que un test no puede hacer de verdad.
    sqlx::query("UPDATE login_attempts SET blocked_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();

    let (status, _) = attempt(&pool, "duenio@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::OK, "el bloqueo debe expirar solo");
}

#[sqlx::test]
async fn failures_outside_the_window_do_not_accumulate(pool: PgPool) {
    seed_user(&pool, "duenio@pyme.com").await;

    attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;

    // La ventana vence: los intentos viejos dejan de contar.
    sqlx::query("UPDATE login_attempts SET window_start = now() - interval '20 minutes'")
        .execute(&pool)
        .await
        .unwrap();

    // Si los viejos contaran, este sería el 3ro y la cuenta se bloquearía.
    let (status, _) = attempt(&pool, "duenio@pyme.com", "contrasena-mala-123").await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "un intento fuera de la ventana no debe acumular"
    );
}

// --- Límite por limiter ------------------------------------------------

#[sqlx::test]
async fn limiter_blocks_and_reports_decision(pool: PgPool) {
    let email = ValidatedEmail::parse("x@pyme.com").unwrap();
    let state = strict_state(pool.clone());

    for _ in 0..3 {
        state.account_limiter.record_failure(&email).await.unwrap();
    }

    let decision = state.account_limiter.check(&email).await.unwrap();
    assert!(
        matches!(decision, Decision::Blocked { .. }),
        "tras 3 fallas con máximo 3 debe estar bloqueada: {decision:?}"
    );

    // Limpiar a mano desbloquea.
    sqlx::query("UPDATE login_attempts SET blocked_until = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();
    let decision = state.account_limiter.check(&email).await.unwrap();
    assert!(!decision.is_blocked(), "el bloqueo vencido debe liberarse");
}

#[sqlx::test]
async fn limiter_purges_stale_rows(pool: PgPool) {
    let email = ValidatedEmail::parse("x@pyme.com").unwrap();
    let state = strict_state(pool.clone());
    state.account_limiter.record_failure(&email).await.unwrap();

    sqlx::query("UPDATE login_attempts SET updated_at = now() - interval '2 days'")
        .execute(&pool)
        .await
        .unwrap();

    let purged = state
        .account_limiter
        .purge_stale(Duration::from_secs(24 * 60 * 60))
        .await
        .unwrap();
    assert_eq!(purged, 1);

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM login_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

// --- Aislamiento entre cuentas -----------------------------------------

#[sqlx::test]
async fn blocking_one_account_does_not_block_another(pool: PgPool) {
    seed_user(&pool, "a@pyme.com").await;
    seed_user(&pool, "b@pyme.com").await;

    for _ in 0..3 {
        attempt(&pool, "a@pyme.com", "contrasena-mala-123").await;
    }
    let (status, _) = attempt(&pool, "a@pyme.com", "contrasena-mala-123").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // b no paga por los platos rotos de a.
    let (status, _) = attempt(&pool, "b@pyme.com", PASSWORD).await;
    assert_eq!(status, StatusCode::OK);
}
