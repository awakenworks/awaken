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

pub use awaken_agent_contract::fresh_process_id;
/// The retry pacing a host composes into [`Runtime::with_retry_policy`]; the
/// decision that reads it lives with the classification in the runtime contract.
pub use awaken_runtime_contract::resilience::RetryPolicy;
pub use circuit_breaker::CircuitBreakerConfig;
pub use detached_tool::{DetachedToolError, PreparedToolExecutor, ResolvedToolExecution};
pub use ingress::DirectAttemptDriver;
pub use permission::PermissionGate;
pub use run::RunInput;
pub use runtime::{ActiveAttemptScope, Runtime};
