//! Durable and encrypted adapters for Credential Vault ports.

#![forbid(unsafe_code)]

#[cfg(feature = "postgres")]
pub mod postgres;
mod schema;
#[cfg(feature = "sealed-aead")]
pub mod sealed;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::{PostgresCredentialRepo, PostgresSealedBlobStore};
pub use schema::{BUNDLE_ID, credential_bundle};
#[cfg(feature = "sealed-aead")]
pub use sealed::{SealedAeadSecretStore, generate_seal_key_hex, parse_seal_key};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteCredentialRepo, SqliteSealedBlobStore};
