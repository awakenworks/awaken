//! Runtime core implementation skeleton.
//!
//! This crate owns execution behavior over neutral runtime contracts. It must not
//! depend on server routes, protocol DTOs, config CRUD, or durable ingress internals.

mod engine;
mod ingress;
pub mod memory;
mod resolve;
mod runtime;

pub use ingress::{DirectRunIngress, RunIngress};
pub use runtime::Runtime;
