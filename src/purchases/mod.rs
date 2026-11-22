//! Purchases domain: persisted drafts and cash supplier purchases.
//!
//! Mirrors `sales`: an operator builds a draft across reloads (attaching an
//! active supplier and product lines with quantities and unit costs) and
//! confirms it in one transaction that enters stock (`reason = 'purchase'`)
//! and deducts wallets (`kind = 'purchase_payment'`).

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;

pub use model::{Purchase, PurchaseStatus};
