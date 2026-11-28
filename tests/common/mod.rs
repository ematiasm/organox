//! Shared scaffolding for the integration-test crates: a generated password,
//! the default app router, and direct user seeding.

// Each integration crate compiles this module on its own and uses only a
// subset of the helpers; dead-code analysis is per crate, so the unused rest
// would fail `cargo clippy --all-targets -- -D warnings`.
#![allow(dead_code)]

use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::routes::router;
use sqlx::PgPool;
use uuid::Uuid;

/// A password generated per test run, so no credential is written down.
///
/// Not a literal on purpose: `rust/hard-coded-cryptographic-value` flags any
/// fixed value used as a credential, and it is right to — the habit is what
/// leaks into production code. The rest of the repo already carries open
/// instances of that alert; this helper does not add to the pile.
pub fn test_password() -> String {
    format!("pw-{}", Uuid::new_v4())
}

pub fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false))
}

/// Creates a user and returns both its ID and the generated password, so an
/// HTTP test can log in with the same value.
pub async fn seed_user_with_password(pool: &PgPool, email: &str, role: Role) -> (Uuid, String) {
    let password = test_password();
    let email = validation::ValidatedEmail::parse(email).expect("email");
    let hash = password::hash(&password).expect("hash");
    let id = repository::create_user(
        pool,
        NewUser {
            email: &email,
            password_hash: &hash,
            role,
        },
    )
    .await
    .expect("user");
    (id, password)
}
