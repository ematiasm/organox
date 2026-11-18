//! Payments: wallets, payment methods, and the ledger that connects them.
//!
//! Three tables, one idea: money has a place, and every movement says which
//! place it hit. Methods are configuration, wallets are destinations, and the
//! movement is the fact.

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
