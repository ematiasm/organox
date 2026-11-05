//! Sales domain: persisted cart and checkout.
//!
//! See `docs/sales.md` for the reasoning.

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;

pub use model::{Sale, SaleItem, SaleStatus};
