//! Product and service catalog, and stock ledger.
//!
//! Central module invariant: **stock is not stored, it is calculated**. It is
//! the signed sum of `stock_movements`. `products` has no stock column and
//! `stock_movements` is append-only (enforced by a DB trigger).

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
pub mod validation;

pub use model::{MovementReason, Product, ProductKind, ProductWithStock};
