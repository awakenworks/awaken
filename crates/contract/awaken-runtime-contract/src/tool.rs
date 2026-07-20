//! Tool execution ports.
//!
//! Two concerns stay separate (tool-and-capability.md): a tool *implementation*
//! (`Tool` typed, `RawTool` schema-erased) and the *executor* port the loop
//! calls to run one resolved call. Where a call physically runs is an
//! implementation detail of whoever implements `ToolExecutor` — owned by the
//! orchestration layer above — and stays out of the neutral runtime contract.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::state::Command as StateCommand;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use crate::llm::ToolCall;

/// What an implementation can safely do after the owner died while an invocation
/// was in flight. This is a trusted property of the executable tool, not a claim
/// supplied by the model or by an agent configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRecoveryCapability {
    /// The external outcome cannot be determined and the call must not be replayed.
    #[default]
    NonRecoverable,
    /// Repeating the call is observationally equivalent to executing it once.
    ReplaySafe,
    /// Repeating the call with the same stable call id is idempotent.
    Idempotent,
    /// The call creates or addresses a durable request by stable identity; recovery
    /// reconnects to that request instead of creating another one.
    DurableRequest,
}

/// The recovery behavior selected for one tool in an executable agent snapshot.
/// `NeverReplay` is always legal; every other mode requires the matching trusted
/// [`ToolRecoveryCapability`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRecoveryMode {
    #[default]
    NeverReplay,
    ReplaySafe,
    Idempotent,
    DurableRequest,
}

impl ToolRecoveryMode {
    /// Fail closed when configuration attempts to widen what the implementation
    /// actually guarantees. A deployment may always choose `NeverReplay`.
    #[must_use]
    pub const fn is_supported_by(self, capability: ToolRecoveryCapability) -> bool {
        matches!(
            (self, capability),
            (Self::NeverReplay, _)
                | (Self::ReplaySafe, ToolRecoveryCapability::ReplaySafe)
                | (Self::Idempotent, ToolRecoveryCapability::Idempotent)
                | (Self::DurableRequest, ToolRecoveryCapability::DurableRequest)
        )
    }
}

/// Per-tool operational recovery settings pinned into the resolved snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRecoveryPolicy {
    #[serde(default)]
    pub mode: ToolRecoveryMode,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u16,
}

const fn default_max_attempts() -> u16 {
    3
}

impl Default for ToolRecoveryPolicy {
    fn default() -> Self {
        Self {
            mode: ToolRecoveryMode::NeverReplay,
            max_attempts: default_max_attempts(),
        }
    }
}

impl ToolRecoveryPolicy {
    #[must_use]
    pub const fn durable_request() -> Self {
        Self {
            mode: ToolRecoveryMode::DurableRequest,
            max_attempts: default_max_attempts(),
        }
    }

    pub fn validate(&self, capability: ToolRecoveryCapability) -> Result<(), ToolRecoveryError> {
        if self.max_attempts == 0 {
            return Err(ToolRecoveryError::ZeroAttempts);
        }
        if !self.mode.is_supported_by(capability) {
            return Err(ToolRecoveryError::Unsupported {
                mode: self.mode,
                capability,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ToolRecoveryError {
    #[error("tool recovery max_attempts must be greater than zero")]
    ZeroAttempts,
    #[error("recovery mode {mode:?} exceeds executable capability {capability:?}")]
    Unsupported {
        mode: ToolRecoveryMode,
        capability: ToolRecoveryCapability,
    },
}

/// Neutral result of one tool invocation. `is_error` lets a tool return a
/// model-visible failure without aborting the run; `state` carries any state
/// transitions the tool wants staged onto the commit boundary (never a direct
/// store write — the same path hooks use).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<StateCommand>,
}

impl ToolOutput {
    pub fn ok(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: content.into(),
            is_error: false,
            state: Vec::new(),
        }
    }

    pub fn error(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: content.into(),
            is_error: true,
            state: Vec::new(),
        }
    }

    /// Stage state transitions to be committed with this tool's result.
    #[must_use]
    pub fn with_state(mut self, state: Vec<StateCommand>) -> Self {
        self.state = state;
        self
    }
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("tool execution failed: {0}")]
    Execution(String),
}

/// Failure to select the executor that will own a run's tool side effects.
/// This is distinct from an invocation failure: selection happens before the
/// runtime starts the run, so a required remote placement must fail closed
/// instead of becoming an implicit local execution.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ToolExecutorSelectionError {
    #[error("tool executor placement unavailable: {0}")]
    Unavailable(String),
    #[error("tool executor placement policy failed: {0}")]
    Policy(String),
}

/// Schema-erased tool: the dynamic call boundary used by the runtime and by
/// MCP/server/client adapters. Concrete implementations live in
/// extension/adapter crates, never in neutral crates.
#[async_trait]
pub trait RawTool: Send + Sync {
    fn id(&self) -> &str;
    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::NonRecoverable
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError>;
}

/// Invoke an ordinary tool while honoring the Run's cooperative cancellation.
/// This helper is intentionally tool-agnostic: extensions construct their own
/// [`ToolCall`] payloads and do not depend on one another's concrete tool crates.
pub async fn invoke_raw_tool(
    tool: &dyn RawTool,
    call: ToolCall,
    cancellation: Option<&crate::CancellationToken>,
) -> Result<ToolOutput, ToolError> {
    match cancellation {
        Some(token) => {
            tokio::select! {
                result = tool.invoke(call) => result,
                _ = token.cancelled() => Err(ToolError::Execution("tool invocation cancelled".into())),
            }
        }
        None => tool.invoke(call).await,
    }
}

/// Preferred typed tool API. Authors implement this with concrete argument and
/// output types; an adapter erases it into a `RawTool` for execution.
#[async_trait]
pub trait Tool: Send + Sync {
    type Args: serde::de::DeserializeOwned + Send;
    type Output: Serialize + Send;

    fn id(&self) -> &str;
    fn recovery_capability(&self) -> ToolRecoveryCapability {
        ToolRecoveryCapability::NonRecoverable
    }
    async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError>;
}

/// The port the execution loop calls to run one already-authorized tool call.
/// Where the call runs is hidden behind this port and owned by its implementer
/// (the orchestration layer above), not the runtime core.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Trusted recovery capability of the concrete executor for `tool_id`.
    /// Remote/general executors fail closed unless they explicitly advertise one.
    fn recovery_capability(&self, _tool_id: &str) -> ToolRecoveryCapability {
        ToolRecoveryCapability::NonRecoverable
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError>;
}

/// Hand placement (ADR-0046): the single call-site port that chooses **which**
/// [`ToolExecutor`] a run uses. A host installs one provider; per run it returns
/// the in-process default or a remote executor over a channel to a placed hand.
///
/// The port is placement-*mechanism*-agnostic (G16): it takes a run activation
/// and returns a `ToolExecutor` — no worker registry, lease, pool, or scheduler
/// type crosses it. The default the runtime ships selects from static config; any
/// richer (e.g. dynamically scheduling) policy is a host-supplied alternative the
/// runtime never names. Mirrors the `InferenceExecutorMaterializer`/`SandboxProvider` seams.
///
/// `provide` is **async**: the static default resolves in a trivial ready future,
/// but a dynamic policy (consult a fleet, lease a worker, dial it) needs to await
/// I/O before it can name the executor. Making the seam async is what lets a
/// scheduling driver live behind it without blocking the run loop's thread.
#[async_trait]
pub trait ToolExecutorProvider: Send + Sync {
    /// The tool executor for this run. Returning `Ok(None)` means "use the kernel's
    /// in-process `LocalToolExecutor`" — a deployment that places no hand installs
    /// no provider (or a provider that always returns `None`) and is unaffected.
    ///
    /// A remote executor returned here owns whatever placement it acquired (e.g. a
    /// leased worker); it releases that on drop when the run's context is dropped,
    /// with the lease's own TTL/epoch as the backstop — so the port needs no
    /// separate release call (keeps it minimal, G16).
    async fn provide(
        &self,
        activation: &crate::activation::RunActivation,
    ) -> Result<Option<Arc<dyn ToolExecutor>>, ToolExecutorSelectionError>;
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn configuration_can_only_match_or_reduce_capability() {
        assert!(
            ToolRecoveryMode::NeverReplay.is_supported_by(ToolRecoveryCapability::DurableRequest)
        );
        assert!(
            ToolRecoveryMode::DurableRequest
                .is_supported_by(ToolRecoveryCapability::DurableRequest)
        );
        assert!(
            !ToolRecoveryMode::ReplaySafe.is_supported_by(ToolRecoveryCapability::NonRecoverable)
        );
        assert!(!ToolRecoveryMode::Idempotent.is_supported_by(ToolRecoveryCapability::ReplaySafe));
    }

    #[test]
    fn zero_attempt_budget_fails_closed() {
        let policy = ToolRecoveryPolicy {
            max_attempts: 0,
            ..ToolRecoveryPolicy::default()
        };
        assert_eq!(
            policy.validate(ToolRecoveryCapability::NonRecoverable),
            Err(ToolRecoveryError::ZeroAttempts)
        );
    }
}
