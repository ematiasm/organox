//! Supplier accounts: what the business owes a supplier and what was paid
//! against it.
//!
//! The payables ledger, mirroring [`crate::customer_accounts`]. A purchase can
//! leave part of its total on account instead of paying everything upfront from
//! a wallet; that payable lives here, never in a wallet, and never as a stored
//! column on `purchases` or `suppliers`.

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
