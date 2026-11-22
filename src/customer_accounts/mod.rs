//! Customer accounts: what a customer owes and what was collected against it.
//!
//! The debt ledger, the mirror of the money ledger in [`crate::payments`]. A
//! sale can leave part of its total owed instead of requiring the money at the
//! till; that debt lives here, never in a wallet, and never as a column on
//! `sales` or `customers`.

pub mod model;
pub mod repository;
