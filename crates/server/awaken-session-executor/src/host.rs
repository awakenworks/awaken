// @formal evidence worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal evidence worker_drain.grace_expiry_retains_cancellation_observer
//! `SharedHost` — the protocol-neutral, thread-keyed session substrate.
//!
//! This is the "waist" both protocol adapters (Managed Agents, AI SDK) drive. It
//! owns one sandboxed runtime + commit coordinator per **thread id**, and exposes
//! neutral operations — `run`, `resume`, `committed_messages` — over that
//! shared state. Because both adapters key by the same thread id and mutate the
//! same coordinator and awaiting-Run position, a Run started through one protocol
//! can be observed or resumed through the other on the *same thread*.
//!
//! It names no protocol vocabulary: outcomes are the neutral [`RunState`] plus an
//! optional [`Pending`]; each adapter maps those onto its own wire shape.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU64;

use awaken_agent_contract::agent::awaiting::{
    AwaitReason, AwaitTarget, PermissionDecision, ResumeTicket, ToolCallAuthority,
};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Scope, StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunRecoverySource};
use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_dispatch_execution::{CompletionSink, DEFAULT_LEASE_MS, DispatchPool, WorkerResolver};
use awaken_ext_skills::{SkillRegistry, SkillSpec};
#[cfg(any(test, feature = "test-support"))]
use awaken_resource_contract::{FileCatalog, FileStore};
use awaken_dispatch_contract::{Inbox, PendingInput, RunDispatch, SubmitOptions};
use awaken_runtime::{DirectAttemptDriver, Runtime};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::RunDelegations;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_session_application_contract::{DelegatedRun, Pending};
use awaken_tool_contract::tool::ToolOutput;

use awaken_ext_compact::CompactConfig;

use crate::background::BackgroundRuns;
use crate::delegate::HostRunDelegationService;
use crate::hub::{ThreadEvent, ThreadEventHub};
use crate::store::HostCommit;
use awaken_ext_goal::grader::{DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};

/// Test fixtures mint process-unique temp roots from this sequence.
#[cfg(test)]
pub(crate) static BASE_SEQ: AtomicU64 = AtomicU64::new(0);

/// One committed-state predicate shared by Session terminal cleanup and child
/// Thread archive. Neither observer may mistake an occupied execution row for
/// quiescence; their different time budgets and completion owners stay separate.
const fn dispatch_remains_unsettled(state: awaken_dispatch_contract::DispatchState) -> bool {
    matches!(
        state,
        awaken_dispatch_contract::DispatchState::Reserved
            | awaken_dispatch_contract::DispatchState::ReservationLeased
            | awaken_dispatch_contract::DispatchState::Pending
            | awaken_dispatch_contract::DispatchState::Leased
            | awaken_dispatch_contract::DispatchState::Awaiting
    )
}

/// Application wrapper applied to each Session's authoritative
/// Native/ACP/A2A attempt router.
pub type AttemptExecutorDecorator = Arc<
    dyn Fn(
            Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>,
        ) -> Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>
        + Send
        + Sync,
>;

/// One remote-attempt adapter plus the exact credential realization evidence
/// installed beside it. Bundling both prevents Worker/direct admission from
/// drifting away from the transport resolver that performs the effect.
pub struct RemoteAttemptInstallation {
    pub executor: Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>,
    pub credential_realization: awaken_credential_contract::CredentialRealizationCapabilities,
}

/// Retain the actual pool-drain fence future until the caller's absolute
/// deadline. `Some(None)` is the valid direct-host case with no local pool;
/// `None` alone means the fence owner did not return before the deadline.
async fn await_runtime_drain_fence_before<F, T>(
    deadline: tokio::time::Instant,
    fence: F,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout_at(deadline, fence).await.ok()
}

mod build;
pub use build::HostProcess;
mod dependencies;
pub(crate) use dependencies::ApplicationPortBinding;
pub use dependencies::{
    HostDependencies, SessionApplicationBindingError, SessionApplicationBindings,
    SessionExecutionDependencies,
};
pub(crate) mod capability;
pub use capability::{
    DispatchPoolSizing, ExecutionTopology, LeaseOwnerId, LocalExecution, QueueDepthSource,
    RunIngress, TrustedHostPolicy, WarmPool,
};
mod completion;
mod coordination;
mod dispatch_reconciliation;
mod dispatch_resolution;
mod durable_control;
pub use placement::{remote_worker_placement, self_hosted_inference_holder};
mod credential_capabilities;
mod placement;
pub(crate) use credential_capabilities::acp_mcp_client_injection_capabilities;
mod run;
mod session;
mod session_ctx;
mod terminal_reconciliation;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
pub(crate) use tests::fixtures::{
    MemoryHostModel, SelectedOnly, test_resource_execution, test_resource_validator,
    test_upstream_authority, with_test_memory_mounter, with_test_resource_validation,
};
mod types;
mod worker_resolver;

pub(crate) use completion::CompletionRegistry;
pub(crate) use session_ctx::{
    ChildExecutionSubstrate, ClaimedRuntimeInput, RuntimePublicationIdentity, SessionCtx,
};
pub use types::{
    CommittedStepReceipt, HostError, HostErrorKind, HostOutcomeDrive, HostOutcomeIteration,
    HostOutcomeReport, HostResume,
};
pub(crate) use worker_resolver::HostWorkerResolver;
#[cfg(test)]
pub(crate) use worker_resolver::test_support::{
    eager_environment, empty_frozen_projection_for_snapshot, managed_test_host, test_activation,
};

/// The protocol-neutral, thread-keyed session substrate shared by every adapter.
///
/// The fields directly on this struct are the CORE every capability reads:
/// Workspace/Session identity, the event hub, deployment, durable authority,
/// the process clock and completion relay. Everything else is grouped by the
/// capability that owns it (module `host::capability`); each group is a plain owned
/// struct, so a module that uses one capability names exactly that group.
///
/// The one constructor is `From<`[`HostDependencies`]`>`: every dependency is
/// selected on that value before the Host exists, and nothing installs one
/// into a constructed Host.
pub struct SharedHost {
    /// Platform-managed workspace for flat, no-login local requests. It is
    /// generated (and persisted with durable storage), never compiled in.
    pub(crate) local_workspace: String,
    /// Process-local projections materialized for prepared Sessions. The private
    /// slot makes their lifecycle atomic while the durable manifest stays authoritative.
    pub(crate) installed_sessions: Arc<crate::installed_session::InstalledSessions>,
    pub(crate) hub: Arc<ThreadEventHub>,
    /// When set, each thread commits to a durable SQLite database at
    /// `store_dir/<thread>.db`, so an awaiting run survives a process restart. When
    /// `None`, sessions use an in-memory coordinator (ephemeral).
    pub(crate) store_dir: Option<PathBuf>,
    /// Where Session truth lives: the Coordinator-owned durable capability
    /// ([`crate::LocalAuthority`]) or a database-less Worker's claim-fenced
    /// upstream transports ([`crate::UpstreamAuthority`]). Every role-dependent
    /// decision is asked of it; the Host never asks which role it plays.
    pub(crate) authority: Arc<dyn crate::SessionAuthority>,
    /// The process's one wall clock, chosen by the composition that builds this
    /// Host. The dispatch pool, the reconciler, recovery drives and child
    /// schedulers all drive their Workers on it, so every time the attempt
    /// itself judges shares one timeline.
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    /// The Host's one process-local BackgroundTask slot registry. Native tool
    /// submission and committed terminal observation share this exact owner.
    pub(crate) background_supervisor: Arc<awaken_ext_background_task::BackgroundTaskSupervisor>,
    /// Owns temporary run-id registrations for event-driven durable completion,
    /// foreground stream relay, and publication into the existing Thread hub.
    /// Injected into the pool as its `CompletionSink` and into each local Session
    /// worker as its `StreamSink`.
    pub(crate) completion: Arc<CompletionRegistry>,
    pub(crate) session_execution: dependencies::SessionExecutionDependencies,
    pub(crate) environment_binding_sink: std::sync::RwLock<
        Option<Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>>,
    >,
    /// Durable dispatch: queue transport, process pool, and the weak Session
    /// application edges the pool's Workers call back into.
    pub(crate) dispatch: capability::DispatchCapability,
    /// Claim-fenced transports a database-less Worker installs.
    pub(crate) worker: capability::WorkerTransports,
    /// Default inference, per-thread model routing, remote attempts, and the
    /// immutable Agent publications/Judge the Host executes.
    pub(crate) agent: capability::AgentExecution,
    /// ACP backend, attempt decoration, and the credential/session-blob
    /// adapters the attempt executors share.
    pub(crate) exec: capability::AttemptBackends,
    /// The Session realizer and the sandbox providers it realizes through.
    pub(crate) realizer: crate::realization_executor::RealizationExecutor,
    /// Tool, Skill, plugin, and gate offering installed on every thread.
    pub(crate) tools: capability::ToolOffering,
    /// Resources-context ports: Memory, Files, Artifacts.
    pub(crate) resources: capability::ResourcePorts,
    /// The Session MCP lifecycle and its Worker relay for sandboxed ACP sessions.
    pub(crate) mcp: awaken_session_mcp::McpSessions,
    /// Content capture (ADR-0050): sink, deployment ceiling, consent source.
    pub(crate) capture: capability::ContentCaptureCapability,
    /// The edges back into the Session application built over this Host.
    pub(crate) session_application: SessionApplicationBindings,
}
