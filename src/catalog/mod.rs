//! Catálogo de productos y servicios, y el libro de stock.
//!
//! Invariante central del módulo: **el stock no se guarda, se calcula**. Es la
//! suma firmada de `stock_movements`. `products` no tiene columna de stock y
//! `stock_movements` es append-only (lo impone un trigger en la DB).

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
pub mod validation;

pub use model::{MovementReason, Product, ProductKind, ProductWithStock};
