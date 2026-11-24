//! Purchase returns (supplier returns) against completed purchases.
//!
//! A completed purchase is immutable. Returning merchandise writes an
//! append-only `purchase_returns` document, deducts stock via `purchase_return`
//! stock movements (checking stock availability first), cancels open payable
//! debt on that purchase first, and lets the operator choose whether any
//! already-paid surplus is refunded to the original wallets (`wallet_refund`)
//! or left as unapplied advance credit with the supplier (`supplier_credit`).

pub mod handlers;
pub mod model;
pub mod repository;
pub mod service;
