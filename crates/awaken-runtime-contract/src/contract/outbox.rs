//! Outbox error type.
//!
//! The outbox is a server/store concern: the `OutboxStore` trait, message/draft
//! data, status enum and lane/target constants all live in
//! `awaken-server-contract`. The runtime-contract surface keeps only
//! `OutboxError`, which the commit-coordinator write boundary names in its
//! result types.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OutboxError {
    #[error("validation error: {0}")]
    Validation(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serialization(String),
}
