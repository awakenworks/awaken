#![doc = include_str!("../README.md")]

mod approval;
mod circuit_breaker;
mod configuration;
mod conversation;
mod detached_tool;
mod engine;
mod ingress;
pub mod model;
mod outcome;
pub mod permission;
mod resolve;
mod retry;
mod run;
mod runtime;
mod snapshot_file;
mod tool_discovery;
pub mod tools;

pub use approval::{ApprovalError, ApprovalOptions};
pub use awaken_agent_contract::agent::run::{EndCause, RunState};
pub use awaken_agent_contract::fresh_process_id;
pub use awaken_runtime_contract::diagnostic::RuntimeErrorKind;
pub use awaken_runtime_contract::execution::Error;
/// The retry pacing a host composes into [`Runtime::with_retry_policy`]; the
/// decision that reads it lives with the classification in the runtime contract.
pub use awaken_runtime_contract::resilience::RetryPolicy;
pub use awaken_runtime_contract::resolved::ModelBinding;
pub use awaken_runtime_contract::{
    CancellationToken, ExecutableAgentSnapshot, ResumeResult, RuntimeRunContext,
};
pub use circuit_breaker::CircuitBreakerConfig;
pub use configuration::ConfigurationError;
pub use conversation::Conversation;
pub use detached_tool::{DetachedToolError, PreparedToolExecutor, ResolvedToolExecution};
pub use engine::run_commands::resume_activated_tool_batch;
pub use ingress::DirectAttemptDriver;
pub use outcome::{RunOutcome, RunOutcomeError};
pub use permission::PermissionGate;
pub use run::RunInput;
pub use runtime::{ActiveAttemptScope, Runtime};
