//! Tests de integración de autenticación.
//!
//! Cada test corre contra una base de datos efímera creada por `#[sqlx::test]`
//! y ejerce el `Router` en memoria (sin abrir puerto ni proceso servidor).

use admin_app::AppState;
use admin_app::auth::model::Role;
use admin_app::auth::repository::{self, NewUser};
use admin_app::auth::{password, validation};
use admin_app::routes::router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tower_cookies::CookieManagerLayer;

const PASSWORD: &str = "contrasena-bastante-larga-123";

fn app(pool: sqlx::PgPool) -> axum::Router {
    router(AppState::new(pool, false)).layer(CookieManagerLayer::new())
}

/// Inserta un usuario directo en la DB, sin pasar por HTTP.
async fn seed_user(pool: &sqlx::PgPool, email: &str, role: Role) {
    let email = validation::ValidatedEmail::parse(email).expect("email válido");
    let hash = password::hash(PASSWORD).expect("hash");

    repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role,
        },
    )
    .await
    .expect("crear usuario");
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 64)
        .await
        .expect("body");
    serde_json::from_slice(&bytes).expect("json")
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

/// Extrae la cookie de sesión de un `Set-Cookie`.
fn session_cookie(response: &axum::response::Response) -> String {
    let header = response
        .headers()
        .get("set-cookie")
        .expect("debe emitir set-cookie")
        .to_str()
        .unwrap();

    let value = header
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
        .map(|(_, v)| v.to_string())
        .expect("cookie con formato name=value");

    assert!(
        header.contains("HttpOnly"),
        "la cookie de sesión debe ser HttpOnly: {header}"
    );
    assert!(
        header.contains("SameSite=Strict"),
        "la cookie de sesión debe ser SameSite=Strict: {header}"
    );

    value
}

// --- Login --------------------------------------------------------------

#[sqlx::test]
async fn login_with_valid_credentials_returns_200_and_cookie(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;
    let app = app(pool);

    let response = app
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let cookie = session_cookie(&response);
    assert!(!cookie.is_empty());

    let body = body_json(response).await;
    assert_eq!(body["user"]["email"], "duenio@pyme.com");
    assert_eq!(body["user"]["role"], "owner");
    // El hash nunca viaja en la respuesta.
    assert!(body["user"].get("password_hash").is_none());
}

#[sqlx::test]
async fn login_is_case_insensitive_on_email(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;
    let app = app(pool);

    let response = app
        .oneshot(login_request("  Duenio@PyME.COM ", PASSWORD))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

/// El mismo 401 para "contraseña incorrecta" y "email inexistente": el cliente
/// no puede distinguir si un email está registrado.
#[sqlx::test]
async fn login_failures_are_indistinguishable(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let wrong_password = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", "otra-contrasena-123"))
        .await
        .unwrap();

    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);
    let wrong_password_body = body_json(wrong_password).await;

    let unknown_email = app(pool)
        .oneshot(login_request("nadie@pyme.com", PASSWORD))
        .await
        .unwrap();

    assert_eq!(unknown_email.status(), StatusCode::UNAUTHORIZED);
    let unknown_email_body = body_json(unknown_email).await;

    assert_eq!(wrong_password_body, unknown_email_body);
    assert_eq!(unknown_email_body["status"], 401);
}

#[sqlx::test]
async fn login_with_malformed_email_returns_401(pool: sqlx::PgPool) {
    let response = app(pool)
        .oneshot(login_request("no-es-email", PASSWORD))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// --- Sesión -------------------------------------------------------------

#[sqlx::test]
async fn me_requires_session_cookie(pool: sqlx::PgPool) {
    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn me_returns_authenticated_user(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["email"], "duenio@pyme.com");
}

#[sqlx::test]
async fn me_rejects_forged_session_token(pool: sqlx::PgPool) {
    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", "session=token-inventado-que-no-existe")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn me_rejects_expired_session(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    // Expirar la sesión directamente en la DB.
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(&pool)
        .await
        .unwrap();

    let response = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", format!("session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Logout borra la sesión en la DB: la cookie robada deja de servir.
#[sqlx::test]
async fn logout_invalidates_session_server_side(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);
    let cookie_header = format!("session={cookie}");

    let logout = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/logout")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);

    // La misma cookie ya no autentica.
    let after = app(pool)
        .oneshot(
            Request::builder()
                .uri("/api/v1/auth/me")
                .header("cookie", &cookie_header)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
}

/// El token en claro no se persiste: solo su SHA-256.
#[sqlx::test]
async fn session_token_is_not_stored_in_plaintext(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].len(), 32, "el token guardado debe ser un SHA-256");
    assert_ne!(
        stored[0],
        cookie.as_bytes(),
        "el token en claro no debe estar en la DB"
    );
}

// --- Alta de usuarios (RBAC) -------------------------------------------

fn create_user_request(cookie: &str, email: &str, role: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/users")
        .header("content-type", "application/json")
        .header("cookie", format!("session={cookie}"))
        .body(Body::from(
            json!({ "email": email, "password": PASSWORD, "role": role }).to_string(),
        ))
        .unwrap()
}

#[sqlx::test]
async fn owner_can_create_user(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool.clone())
        .oneshot(create_user_request(
            &cookie,
            "empleado@pyme.com",
            "encargado",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let role: Role = sqlx::query_scalar("SELECT role FROM users WHERE email = 'empleado@pyme.com'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(role, Role::Encargado);
}

/// RBAC: un encargado no puede crear usuarios. 403, y sin efectos: no se crea
/// el usuario ni se gasta un hash.
#[sqlx::test]
async fn encargado_cannot_create_user(pool: sqlx::PgPool) {
    seed_user(&pool, "encargado@pyme.com", Role::Encargado).await;

    let login = app(pool.clone())
        .oneshot(login_request("encargado@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool.clone())
        .oneshot(create_user_request(&cookie, "otro@pyme.com", "owner"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'otro@pyme.com'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "un 403 no debe dejar efectos");
}

#[sqlx::test]
async fn anonymous_cannot_create_user(pool: sqlx::PgPool) {
    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/users")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "email": "x@pyme.com", "password": PASSWORD, "role": "owner" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn duplicate_email_returns_409_and_keeps_single_row(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool.clone())
        .oneshot(create_user_request(&cookie, "duenio@pyme.com", "encargado"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'duenio@pyme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test]
async fn weak_password_is_rejected_without_creating_user(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/users")
                .header("content-type", "application/json")
                .header("cookie", format!("session={cookie}"))
                .body(Body::from(
                    json!({ "email": "debil@pyme.com", "password": "corta", "role": "encargado" })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'debil@pyme.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn invalid_role_in_payload_is_rejected(pool: sqlx::PgPool) {
    seed_user(&pool, "duenio@pyme.com", Role::Owner).await;

    let login = app(pool.clone())
        .oneshot(login_request("duenio@pyme.com", PASSWORD))
        .await
        .unwrap();
    let cookie = session_cookie(&login);

    let response = app(pool)
        .oneshot(create_user_request(&cookie, "x@pyme.com", "superadmin"))
        .await
        .unwrap();

    // axum rechaza el body antes de llegar al handler.
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
