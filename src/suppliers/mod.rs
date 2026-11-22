//! Supplier registry.
//!
//! Central module invariant: **a supplier is deactivated, never deleted**.
//! Purchases point at a supplier row with `ON DELETE RESTRICT`, so there is no
//! DELETE path here.
//!
//! A supplier is identified by its name and, when present, by its document
//! (`tax_id`), which is unique in the registry and compared without case.

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
pub mod validation;

pub use model::Supplier;
