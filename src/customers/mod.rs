//! Customer registry.
//!
//! Central module invariant: **a customer is deactivated, never deleted**. Sales
//! and credit notes point at a customer row, and a row that disappears would
//! break history. There is no DELETE path here and the foreign key on `sales`
//! is `ON DELETE RESTRICT`.
//!
//! A customer is identified by its name and, when it has one, by its document
//! (`tax_id`), which is unique in the registry and compared without case.

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
pub mod validation;

pub use model::Customer;
