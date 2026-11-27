//! Customers integration tests.
//!
//! These protect the rules that make a registry usable rather than decorative:
//! a document is unique when present but optional, blank is not a value, a
//! customer is deactivated instead of deleted, and a page reports its real
//! total so the screen cannot truncate silently (the defect issue #22 fixed for
//! products).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use organox::AppState;
use organox::auth::model::Role;
use organox::auth::repository::{self, NewUser};
use organox::auth::{password, validation};
use organox::customers::service::{self, CreateCustomerInput, CustomerError, UpdateCustomerInput};
use organox::customers::validation::ValidationError;
use organox::routes::router;
use rust_decimal_macros::dec;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

/// A password generated per test run, so no credential is written down.
fn test_password() -> String {
    format!("pw-{}", Uuid::new_v4())
}

fn app(pool: PgPool) -> axum::Router {
    router(AppState::new(pool, false))
}

async fn seed_user_with_password(pool: &PgPool, email: &str, role: Role) -> (Uuid, String) {
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

fn new_customer(name: &str) -> CreateCustomerInput {
    CreateCustomerInput {
        name: name.to_string(),
        tax_id: None,
        email: None,
        phone: None,
        credit_limit: None,
    }
}

async fn seed_customer(pool: &PgPool, name: &str, tax_id: Option<&str>) -> Uuid {
    service::create_customer(
        pool,
        CreateCustomerInput {
            name: name.to_string(),
            tax_id: tax_id.map(str::to_string),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .expect("customer")
}

// --- Creation and validation --------------------------------------------

#[sqlx::test]
async fn creates_a_customer_and_trims_its_fields(pool: PgPool) {
    let id = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "  Ana Pérez  ".to_string(),
            tax_id: Some("  20-12345678-9 ".to_string()),
            email: Some(" ana@example.com ".to_string()),
            phone: Some(" 555-0100 ".to_string()),
            credit_limit: None,
        },
    )
    .await
    .expect("customer");

    let customer = service::get_customer(&pool, id).await.expect("customer");
    assert_eq!(customer.name, "Ana Pérez");
    assert_eq!(customer.tax_id.as_deref(), Some("20-12345678-9"));
    assert_eq!(customer.email.as_deref(), Some("ana@example.com"));
    assert_eq!(customer.phone.as_deref(), Some("555-0100"));
    assert!(customer.is_active);
}

#[sqlx::test]
async fn rejects_a_blank_name_and_a_blank_document(pool: PgPool) {
    let blank_name = service::create_customer(&pool, new_customer("   ")).await;
    assert!(matches!(
        blank_name,
        Err(CustomerError::Validation(ValidationError::NameBlank))
    ));

    // An empty document is "no document", not a blank value: it becomes NULL
    // so the partial unique index never sees an empty string.
    let no_document = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "Walk-in".to_string(),
            tax_id: Some("    ".to_string()),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .expect("customer");
    let customer = service::get_customer(&pool, no_document)
        .await
        .expect("customer");
    assert_eq!(customer.tax_id, None);
}

#[sqlx::test]
async fn a_document_exists_at_most_once_but_many_customers_have_none(pool: PgPool) {
    // Two customers without a document do not collide: a partial unique index
    // permits any number of NULLs.
    seed_customer(&pool, "First", None).await;
    seed_customer(&pool, "Second", None).await;

    seed_customer(&pool, "Ana", Some("20-12345678-9")).await;

    let same_document = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "Ana again".to_string(),
            tax_id: Some("20-12345678-9".to_string()),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await;
    assert!(matches!(same_document, Err(CustomerError::TaxIdTaken)));

    // A document has no case: the same taxpayer typed in lower case is the
    // same customer, not a second one.
    let different_case = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "Ana in lower case".to_string(),
            tax_id: Some("ab-123".to_string()),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await
    .expect("customer");
    assert!(different_case != Uuid::nil());

    let lower_case_again = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "Ana in upper case".to_string(),
            tax_id: Some("AB-123".to_string()),
            email: None,
            phone: None,
            credit_limit: None,
        },
    )
    .await;
    assert!(matches!(lower_case_again, Err(CustomerError::TaxIdTaken)));
}

#[sqlx::test]
async fn rejects_an_email_without_at_and_a_too_long_name(pool: PgPool) {
    let bad_email = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "Ana".to_string(),
            tax_id: None,
            email: Some("ana.example.com".to_string()),
            phone: None,
            credit_limit: None,
        },
    )
    .await;
    assert!(matches!(
        bad_email,
        Err(CustomerError::Validation(ValidationError::EmailInvalid))
    ));

    let long_name = "x".repeat(201);
    let too_long = service::create_customer(&pool, new_customer(&long_name)).await;
    assert!(matches!(
        too_long,
        Err(CustomerError::Validation(ValidationError::NameTooLong {
            max: 200
        }))
    ));
}

// --- Listing, search and pagination -------------------------------------

#[sqlx::test]
async fn a_page_reports_the_real_total_and_searches_name_or_document(pool: PgPool) {
    seed_customer(&pool, "Harina Norte", Some("30-1")).await;
    seed_customer(&pool, "Bebidas Sur", Some("30-2")).await;
    seed_customer(&pool, "Lácteos Centro", Some("30-3")).await;

    let (page, total) = service::list_customers(&pool, false, None, 2, 0)
        .await
        .expect("page");
    assert_eq!(page.len(), 2, "the page honours the limit");
    assert_eq!(total, 3, "the total ignores the page");

    // Search by name, case-insensitively.
    let (by_name, total) = service::list_customers(&pool, false, Some("harina"), 50, 0)
        .await
        .expect("search");
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].name, "Harina Norte");
    assert_eq!(total, 1);

    // Search by document fragment, which is what the counter actually types.
    let (by_document, _) = service::list_customers(&pool, false, Some("30-2"), 50, 0)
        .await
        .expect("search");
    assert_eq!(by_document.len(), 1);
    assert_eq!(by_document[0].name, "Bebidas Sur");

    // An offset past the end reports the real total instead of claiming the
    // registry is empty.
    let (empty, total) = service::list_customers(&pool, false, None, 2, 99)
        .await
        .expect("page");
    assert!(empty.is_empty());
    assert_eq!(total, 3);
}

#[sqlx::test]
async fn deactivated_customers_are_hidden_unless_asked_for(pool: PgPool) {
    let active = seed_customer(&pool, "Active", None).await;
    let deactivated = seed_customer(&pool, "Gone", None).await;
    service::deactivate_customer(&pool, deactivated)
        .await
        .expect("deactivate");

    let (default_page, total) = service::list_customers(&pool, false, None, 50, 0)
        .await
        .expect("page");
    assert_eq!(default_page.len(), 1);
    assert_eq!(default_page[0].id, active);
    assert_eq!(total, 1);

    let (wide_page, total) = service::list_customers(&pool, true, None, 50, 0)
        .await
        .expect("page");
    assert_eq!(wide_page.len(), 2);
    assert_eq!(total, 2);
}

// --- Update and deactivation --------------------------------------------

#[sqlx::test]
async fn update_touches_only_the_fields_sent(pool: PgPool) {
    let id = seed_customer(&pool, "Ana", Some("20-1")).await;

    service::update_customer(
        &pool,
        id,
        UpdateCustomerInput {
            name: Some("Ana María".to_string()),
            tax_id: None,
            email: None,
            phone: None,
            is_active: None,
            credit_limit: None,
        },
    )
    .await
    .expect("update");

    let customer = service::get_customer(&pool, id).await.expect("customer");
    assert_eq!(customer.name, "Ana María");
    assert_eq!(
        customer.tax_id.as_deref(),
        Some("20-1"),
        "an absent field is untouched, not cleared"
    );
}

#[sqlx::test]
async fn update_rejects_a_document_another_customer_already_has(pool: PgPool) {
    seed_customer(&pool, "Ana", Some("20-1")).await;
    let other = seed_customer(&pool, "Beto", Some("20-2")).await;

    let taken = service::update_customer(
        &pool,
        other,
        UpdateCustomerInput {
            name: None,
            tax_id: Some("20-1".to_string()),
            email: None,
            phone: None,
            is_active: None,
            credit_limit: None,
        },
    )
    .await;
    assert!(matches!(taken, Err(CustomerError::TaxIdTaken)));

    // Keeping its own document is not a conflict with itself.
    service::update_customer(
        &pool,
        other,
        UpdateCustomerInput {
            name: Some("Beto".to_string()),
            tax_id: Some("20-2".to_string()),
            email: None,
            phone: None,
            is_active: None,
            credit_limit: None,
        },
    )
    .await
    .expect("its own document is not a conflict");
}

#[sqlx::test]
async fn deactivation_is_idempotent_and_an_unknown_id_is_not_found(pool: PgPool) {
    let id = seed_customer(&pool, "Ana", None).await;

    service::deactivate_customer(&pool, id)
        .await
        .expect("first deactivation");
    service::deactivate_customer(&pool, id)
        .await
        .expect("second deactivation is idempotent");

    let customer = service::get_customer(&pool, id).await.expect("customer");
    assert!(!customer.is_active, "the row stays, only the flag flips");

    let unknown = service::deactivate_customer(&pool, Uuid::now_v7()).await;
    assert!(matches!(unknown, Err(CustomerError::NotFound)));

    let missing = service::get_customer(&pool, Uuid::now_v7()).await;
    assert!(matches!(missing, Err(CustomerError::NotFound)));
}

// --- HTTP contract ------------------------------------------------------

#[sqlx::test]
async fn http_flow_needs_a_session_and_maps_errors_to_problem_json(pool: PgPool) {
    let router = app(pool.clone());

    // No cookie: every customers route is behind a session.
    let anonymous = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/customers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let (_actor, password) = seed_user_with_password(&pool, "owner@pyme.com", Role::Owner).await;
    let cookie = login(&router, "owner@pyme.com", &password).await;

    // Create: 201 with the new id.
    let created = send(
        &router,
        "POST",
        "/api/v1/customers",
        Some(&cookie),
        Some(json!({"name": "Ana", "tax_id": "20-1"})),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    assert!(created.1["id"].is_string(), "the body carries the new id");

    // A duplicate document is a conflict with a readable message, not a 500
    // leaking a constraint name.
    let duplicate = send(
        &router,
        "POST",
        "/api/v1/customers",
        Some(&cookie),
        Some(json!({"name": "Ana again", "tax_id": "20-1"})),
    )
    .await;
    assert_eq!(duplicate.0, StatusCode::CONFLICT);
    assert_eq!(duplicate.1["detail"], "the document is already in use");

    // Missing name: the server is the validator.
    let invalid = send(
        &router,
        "POST",
        "/api/v1/customers",
        Some(&cookie),
        Some(json!({"name": "  "})),
    )
    .await;
    assert_eq!(invalid.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(invalid.1["detail"], "the name cannot be blank");

    // Unknown id: problem+json 404.
    let missing = send(
        &router,
        "GET",
        &format!("/api/v1/customers/{}", Uuid::now_v7()),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);

    // The list uses the catalog page shape, not a bare array.
    let listed = send(&router, "GET", "/api/v1/customers", Some(&cookie), None).await;
    assert_eq!(listed.0, StatusCode::OK);
    assert_eq!(listed.1["total"], 1);
    assert_eq!(listed.1["limit"], 50);
    assert_eq!(listed.1["offset"], 0);
    assert!(listed.1["items"].is_array());
}

/// Log in through the real route and keep the cookie header it sets.
async fn login(router: &axum::Router, email: &str, password: &str) -> String {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"email": email, "password": password}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "login must succeed");
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };

    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

// --- Uniqueness is the database's rule, not the service's ----------------

/// The service performs no read-then-check: it inserts and lets the partial
/// unique index arbitrate. A probe that only used the service would still pass
/// with the index dropped, so this one bypasses it.
#[sqlx::test]
async fn the_partial_index_rejects_a_case_variant_document_without_the_service(pool: PgPool) {
    for name in ["One", "Two", "Three"] {
        sqlx::query("INSERT INTO customers (id, name) VALUES ($1, $2)")
            .bind(Uuid::now_v7())
            .bind(name)
            .execute(&pool)
            .await
            .expect("a customer without a document");
    }

    sqlx::query("INSERT INTO customers (id, name, tax_id) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind("Doc Holder")
        .bind("ab-123")
        .execute(&pool)
        .await
        .expect("the first holder of the document");

    let duplicate = sqlx::query("INSERT INTO customers (id, name, tax_id) VALUES ($1, $2, $3)")
        .bind(Uuid::now_v7())
        .bind("Same taxpayer")
        .bind("AB-123")
        .execute(&pool)
        .await
        .expect_err("a document has no case: this is the same taxpayer");

    let error = duplicate
        .as_database_error()
        .expect("a constraint violation, not a transport error");
    assert_eq!(error.constraint(), Some("customers_tax_id_unique"));
}

/// Two creates racing for the same document: the database rejects one, and the
/// client is told. Without that, two rows would carry the same taxpayer.
#[sqlx::test]
async fn two_concurrent_creates_with_the_same_document_leave_one_customer(pool: PgPool) {
    let (_actor, password) = seed_user_with_password(&pool, "owner@pyme.com", Role::Owner).await;
    let router = app(pool.clone());
    let cookie = login(&router, "owner@pyme.com", &password).await;

    let body = json!({"name": "Same taxpayer", "tax_id": "20-12345678-9"});
    let first = send(
        &router,
        "POST",
        "/api/v1/customers",
        Some(&cookie),
        Some(body.clone()),
    );
    let second = send(
        &router,
        "POST",
        "/api/v1/customers",
        Some(&cookie),
        Some(body),
    );
    let (first, second) = tokio::join!(first, second);

    let statuses = [first.0, second.0];
    assert!(
        statuses.contains(&StatusCode::CREATED),
        "one create wins: {statuses:?}"
    );
    assert!(
        statuses.contains(&StatusCode::CONFLICT),
        "the other is told, not silently merged: {statuses:?}"
    );

    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM customers WHERE upper(tax_id) = upper('20-12345678-9')",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(stored, 1);
}

/// `%` and `_` are LIKE metacharacters, and they used to reach the pattern
/// untouched: a search for `%` returned the whole registry and a name that
/// contains one could not be found literally. The escape belongs in the query
/// the operator meant, not in a second special case at the call site.
#[sqlx::test]
async fn search_treats_the_wildcards_literally(pool: PgPool) {
    seed_customer(&pool, "Kiosco 50% Off", None).await;
    seed_customer(&pool, "50X Off", None).await;
    seed_customer(&pool, "under_score", None).await;
    seed_customer(&pool, "underXscore", None).await;

    // A bare `%` is not "everything": only the row that contains one.
    let (percent, total) = service::list_customers(&pool, false, Some("%"), 50, 0)
        .await
        .expect("search");
    assert_eq!(total, 1, "a bare % must not match every customer");
    assert_eq!(percent[0].name, "Kiosco 50% Off");

    // A `%` after a prefix is not "anything after": `50X` must not match.
    let (prefixed, _) = service::list_customers(&pool, false, Some("50%"), 50, 0)
        .await
        .expect("search");
    assert_eq!(prefixed.len(), 1);
    assert_eq!(prefixed[0].name, "Kiosco 50% Off");

    // `_` means one character in LIKE; typed by an operator it means a dash.
    let (underscore, _) = service::list_customers(&pool, false, Some("_"), 50, 0)
        .await
        .expect("search");
    assert_eq!(underscore.len(), 1, "a bare _ must not match every name");
    assert_eq!(underscore[0].name, "under_score");

    let (underscored, _) = service::list_customers(&pool, false, Some("under_"), 50, 0)
        .await
        .expect("search");
    assert_eq!(underscored.len(), 1);
    assert_eq!(underscored[0].name, "under_score");

    // The substring search the counter relies on still works.
    let (plain, _) = service::list_customers(&pool, false, Some("off"), 50, 0)
        .await
        .expect("search");
    assert_eq!(
        plain.len(),
        2,
        "an ordinary word still matches by substring"
    );
}

/// The escape character is a character too: a name with a literal backslash has
/// to be findable, and one with a slash is a different name. The helper's unit
/// test proves the doubling; this proves it decodes under the declared ESCAPE.
#[sqlx::test]
async fn search_finds_a_literal_backslash(pool: PgPool) {
    seed_customer(&pool, "Ruta C:\\Proveedores", None).await;
    seed_customer(&pool, "Ruta C:/Proveedores", None).await;

    let (found, total) = service::list_customers(&pool, false, Some("C:\\"), 50, 0)
        .await
        .expect("search");
    assert_eq!(
        total, 1,
        "a backslash is not a wildcard and not a separator"
    );
    assert_eq!(found[0].name, "Ruta C:\\Proveedores");
}

/// The page query and its count query must receive the same pattern. The
/// re-count only runs when a page comes back empty, so nothing else exercises
/// it with a metacharacter.
#[sqlx::test]
async fn a_page_past_the_end_recounts_with_the_escaped_pattern(pool: PgPool) {
    seed_customer(&pool, "Kiosco 50% Off", None).await;
    seed_customer(&pool, "Plain Shop", None).await;

    let (empty, total) = service::list_customers(&pool, false, Some("%"), 50, 99)
        .await
        .expect("page");
    assert!(empty.is_empty());
    assert_eq!(total, 1, "the re-count uses the escaped pattern too");
}

// --- The credit limit ---------------------------------------------------

/// The limit is optional, readable and clearable. Clearing is the one case
/// the text fields do not have, so it goes through HTTP on purpose: the
/// omitted-field / explicit-`null` distinction is serde's, and only the
/// route can prove it survived the wire.
#[sqlx::test]
async fn a_credit_limit_is_optional_and_can_be_cleared(pool: PgPool) {
    // Created without one: unlimited is the column default, not a value.
    let id = service::create_customer(&pool, new_customer("Ana"))
        .await
        .expect("customer");
    let customer = service::get_customer(&pool, id).await.expect("customer");
    assert_eq!(customer.credit_limit, None);

    let (_actor, password) = seed_user_with_password(&pool, "owner@pyme.com", Role::Owner).await;
    let router = app(pool.clone());
    let cookie = login(&router, "owner@pyme.com", &password).await;

    // Set through PATCH, read back through the service and through the
    // route: money is a string with its cents, like every other amount.
    let (status, _) = send(
        &router,
        "PATCH",
        &format!("/api/v1/customers/{id}"),
        Some(&cookie),
        Some(json!({ "credit_limit": "100.00" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let customer = service::get_customer(&pool, id).await.expect("customer");
    assert_eq!(customer.credit_limit, Some(dec!(100.00)));

    let (status, body) = send(
        &router,
        "GET",
        &format!("/api/v1/customers/{id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["credit_limit"], "100.00");

    // An absent field leaves the limit untouched, exactly like every other
    // PATCH field.
    let (status, _) = send(
        &router,
        "PATCH",
        &format!("/api/v1/customers/{id}"),
        Some(&cookie),
        Some(json!({ "name": "Ana María" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let customer = service::get_customer(&pool, id).await.expect("customer");
    assert_eq!(customer.credit_limit, Some(dec!(100.00)));
    assert_eq!(customer.name, "Ana María");

    // The explicit `null` is the one body that returns the customer to
    // unlimited, and the response says so.
    let (status, _) = send(
        &router,
        "PATCH",
        &format!("/api/v1/customers/{id}"),
        Some(&cookie),
        Some(json!({ "credit_limit": null })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let customer = service::get_customer(&pool, id).await.expect("customer");
    assert_eq!(customer.credit_limit, None);

    let (status, body) = send(
        &router,
        "GET",
        &format!("/api/v1/customers/{id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["credit_limit"], Value::Null);
}

/// A limit is money the shop is willing to be owed, so it follows the two
/// rules every money amount here follows: never negative, never finer than
/// cents — and a rejected value never reaches the row.
#[sqlx::test]
async fn a_negative_credit_limit_is_rejected(pool: PgPool) {
    let negative = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "Ana".to_string(),
            tax_id: None,
            email: None,
            phone: None,
            credit_limit: Some(dec!(-10.00)),
        },
    )
    .await;
    assert!(matches!(
        negative,
        Err(CustomerError::Validation(
            ValidationError::CreditLimitNegative
        ))
    ));

    // `numeric(14, 2)` would round a third decimal away; rejecting it is the
    // point, the same rule the payments and the ledger follow.
    let third_decimal = service::create_customer(
        &pool,
        CreateCustomerInput {
            name: "Ana".to_string(),
            tax_id: None,
            email: None,
            phone: None,
            credit_limit: Some(dec!(10.005)),
        },
    )
    .await;
    assert!(matches!(
        third_decimal,
        Err(CustomerError::Validation(ValidationError::CreditLimitTooManyDecimals {
            amount
        })) if amount == dec!(10.005)
    ));

    // The update path validates through the same rule, so a limit already
    // on the row cannot be replaced with a value that never fit it.
    let id = seed_customer(&pool, "Beto", None).await;
    let result = service::update_customer(
        &pool,
        id,
        UpdateCustomerInput {
            name: None,
            tax_id: None,
            email: None,
            phone: None,
            is_active: None,
            credit_limit: Some(Some(dec!(-1.00))),
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(CustomerError::Validation(
            ValidationError::CreditLimitNegative
        ))
    ));

    // None of the rejected values reached the registry.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM customers")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(stored, 1, "only Beto, without a limit");
    let beto = service::get_customer(&pool, id).await.expect("customer");
    assert_eq!(beto.credit_limit, None);
}
