//! The role wire value and the input aliases kept for compatibility.
//!
//! Both are contracts that no other test pins: `Role` is only exercised
//! through HTTP, and the Spanish aliases in `ProductKind`/`MovementReason` are
//! accepted input that callers may still send.

use std::str::FromStr;

use organox::auth::model::Role;
use organox::auth::validation::{ValidationError, validate_password};
use organox::catalog::model::{MovementReason, ProductKind};

#[test]
fn role_serializes_to_its_wire_value() {
    assert_eq!(Role::Owner.as_str(), "owner");
    assert_eq!(Role::Manager.as_str(), "manager");
    assert_eq!(Role::Manager.to_string(), "manager");
    assert_eq!(serde_json::to_string(&Role::Owner).unwrap(), "\"owner\"");
    assert_eq!(
        serde_json::to_string(&Role::Manager).unwrap(),
        "\"manager\""
    );
}

#[test]
fn role_parses_the_wire_value_case_insensitively() {
    assert_eq!("owner".parse::<Role>().unwrap(), Role::Owner);
    assert_eq!("manager".parse::<Role>().unwrap(), Role::Manager);
    assert_eq!("MANAGER".parse::<Role>().unwrap(), Role::Manager);
    assert_eq!(
        serde_json::from_str::<Role>("\"manager\"").unwrap(),
        Role::Manager
    );
}

#[test]
fn role_rejects_the_old_label() {
    assert_eq!(
        "encargado".parse::<Role>().unwrap_err(),
        "unknown role: \"encargado\" (valid roles: owner, manager)"
    );
    assert!(serde_json::from_str::<Role>("\"encargado\"").is_err());
}

#[test]
fn catalog_keeps_its_spanish_aliases() {
    // Accepted input, not prose: removing these would break callers that send
    // the Spanish spelling.
    assert_eq!(
        ProductKind::from_str("producto").unwrap(),
        ProductKind::Product
    );
    assert_eq!(
        ProductKind::from_str("servicio").unwrap(),
        ProductKind::Service
    );
    assert_eq!(
        MovementReason::from_str("compra").unwrap(),
        MovementReason::Purchase
    );
    assert_eq!(
        MovementReason::from_str("venta").unwrap(),
        MovementReason::Sale
    );
    assert_eq!(
        MovementReason::from_str("ajuste").unwrap(),
        MovementReason::Adjustment
    );
    assert_eq!(
        MovementReason::from_str("perdida").unwrap(),
        MovementReason::Loss
    );
    assert_eq!(
        MovementReason::from_str("p\u{e9}rdida").unwrap(),
        MovementReason::Loss
    );
}

#[test]
fn forbidden_passwords_are_still_rejected() {
    for bad in [
        "123456789012",
        "qwertyuiopas",
        "administrador",
        "adminadminadmin",
    ] {
        assert_eq!(
            validate_password(bad),
            Err(ValidationError::PasswordTooCommon),
            "{bad}"
        );
    }
    assert!(validate_password("a-long-enough-password-1234").is_ok());
}
