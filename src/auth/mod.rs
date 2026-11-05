//! Authentication and authorization.
//!
//! Layers, from the inside out:
//!   model      domain types (User, Role, PasswordHash)
//!   validation input rules (email, password length)
//!   password   argon2id hashing
//!   token      session tokens and their hashes
//!   repository SQL. The only place with queries
//!   service    use cases and policy
//!   extract    `AuthUser`: the only way to identify the actor
//!   handlers   HTTP: DTOs and error translation

pub mod extract;
pub mod handlers;
pub mod model;
pub mod password;
pub mod repository;
pub mod service;
pub mod token;
pub mod validation;

pub use extract::AuthUser;
pub use model::{Role, User};
pub use token::SessionToken;
