//! Dominio de ventas: carrito persistido y cobro.
//!
//! Ver `docs/sales.md` para el razonamiento.

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;

pub use model::{Sale, SaleItem, SaleStatus};
