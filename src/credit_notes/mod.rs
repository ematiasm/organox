//! Credit notes: the correction mechanism for a charged sale.
//!
//! Layer split follows the rest of the crate: `model` holds the domain types and
//! the pure refund allocation, `repository` owns the SQL, `service` the rules
//! and `handlers` the HTTP surface. A completed sale is never edited; a customer
//! returning goods gets a document of its own, with its own stock movements and
//! its own money movements.

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
