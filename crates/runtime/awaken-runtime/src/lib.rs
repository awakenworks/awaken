//! Runtime core implementation skeleton.
//!
//! This crate owns execution behavior over neutral runtime contracts. It must not
//! depend on server routes, protocol DTOs, config CRUD, or durable ingress internals.

mod circuit_breaker;
mod engine;
mod ingress;
pub mod memory;
mod permission;
mod resolve;
mod retry;
mod run;
mod runtime;

pub use circuit_breaker::CircuitBreakerConfig;
pub use ingress::{DirectRunIngress, RunIngress};
pub use permission::PermissionGate;
pub use retry::LlmRetryPolicy;
pub use run::RunInput;
pub use runtime::Runtime;
