//! Autenticación y autorización.
//!
//! Capas, de adentro hacia afuera:
//!   model      tipos del dominio (User, Role, PasswordHash)
//!   validation reglas de entrada (email, largo de contraseña)
//!   password   hashing argon2id
//!   token      tokens de sesión y su hash
//!   repository SQL. Único lugar con consultas
//!   service    casos de uso y política
//!   extract    `AuthUser`: la única vía para conocer al actor
//!   handlers   HTTP: DTOs y traducción de errores

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
