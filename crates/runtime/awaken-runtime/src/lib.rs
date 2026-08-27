//! Runtime core implementation skeleton.
//!
//! This crate owns execution behavior over neutral runtime contracts. It must not
//! depend on server routes, protocol DTOs, config CRUD, or durable ingress internals.

mod circuit_breaker;
mod detached_tool;
mod engine;
mod ingress;
mod permission;
mod resolve;
mod retry;
mod run;
mod runtime;
mod snapshot_file;
mod tool_discovery;

pub use circuit_breaker::CircuitBreakerConfig;
pub use detached_tool::{DetachedToolError, PreparedToolExecutor, ResolvedToolExecution};
pub use ingress::{DirectRunIngress, RunIngress, RunService};
pub use permission::PermissionGate;
pub use retry::LlmRetryPolicy;
pub use run::{RunInput, fresh_process_id};
pub use runtime::{ActiveAttemptTracking, Runtime};
