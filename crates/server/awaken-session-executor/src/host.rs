//! [`ExecutorComponents`] — the Session executor's components, assembled once.
//!
//! The composition root builds it from [`SessionExecutorBuilder`] and selects
//! its shared dispatch lifecycle or [`crate::WorkerRoles`] from it. It holds each
//! component and the process-level facts they share; behaviour lives in the
//! components, which every protocol adapter drives on the same thread ids.

use crate::SessionExecutionError;
#[cfg(test)]
use crate::SessionExecutionErrorKind;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU64;

#[cfg(test)]
use awaken_agent_contract::agent::awaiting::AwaitTarget;
use awaken_agent_contract::agent::awaiting::PermissionDecision;
#[cfg(test)]
use awaken_agent_contract::agent::awaiting::{ResumeTicket, ToolCallAuthority};
#[cfg(test)]
use awaken_agent_contract::agent::message::Id as MessageId;
use awaken_agent_contract::agent::message::Message;
#[cfg(test)]
use awaken_agent_contract::agent::message::Role;
#[cfg(test)]
use awaken_agent_contract::agent::run::EndCause;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
#[cfg(test)]
use awaken_agent_contract::agent::state::StateKey;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
#[cfg(test)]
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_dispatch_contract::Inbox;
#[cfg(test)]
use awaken_dispatch_contract::RunDispatch;
use awaken_dispatch_erasure::AnyDispatchStore;
#[cfg(test)]
use awaken_dispatch_execution::WorkerResolver;
use awaken_ext_skills::{SkillRegistry, SkillSpec};
#[cfg(any(test, feature = "test-support"))]
use awaken_resource_contract::{FileCatalog, FileStore};
use awaken_runtime::{DirectAttemptDriver, Runtime};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::ResumeCommand;
#[cfg(test)]
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
#[cfg(test)]
use awaken_session_application_contract::Pending;
use awaken_session_execution_contract::SessionApplicationBinding;

use awaken_ext_compact::CompactConfig;

use crate::ThreadEventHub;
use awaken_ext_goal::grader::{DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};
use awaken_session_run_delivery::BackgroundRuns;
use awaken_session_run_delivery::SessionCommit;

/// Test fixtures mint process-unique temp roots from this sequence.
#[cfg(test)]
pub(crate) static BASE_SEQ: AtomicU64 = AtomicU64::new(0);

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

mod build;
pub use build::SessionExecutionProcess;
mod dependencies;
pub use dependencies::{SessionExecutionDependencies, SessionExecutorBuilder};
pub(crate) mod capability;
pub use capability::{TrustedHostPolicy, WarmPool};
pub(crate) mod boundary_run;
mod claimed_run;
mod completion;
mod coordination;
mod credential_capabilities;
pub(crate) use credential_capabilities::resident_process_environment;
pub(crate) mod dispatch_execution;
pub(crate) use dispatch_execution::session_worker_resolver;
#[cfg(test)]
mod dispatch_reconciliation;
mod foreground_run;
mod run_admission;
mod run_control;
pub(crate) use credential_capabilities::acp_mcp_client_injection_capabilities;
mod run;
mod session;
mod session_ctx;

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
pub(crate) use tests::fixtures::{
    MemoryHostModel, SelectedOnly, session_executor, test_resource_execution,
    test_resource_validator, test_upstream_authority, with_test_memory_mounter,
    with_test_resource_validation,
};
mod types;
#[cfg(test)]
mod worker_resolver;

pub(crate) use awaken_dispatch_execution::session_worker_resolver::SessionWorkerResolver;
use awaken_session_run_delivery::CommittedStepReceipt;
pub(crate) use awaken_session_run_delivery::CompletionRegistry;
pub(crate) use foreground_run::{ForegroundRun, ForegroundRunPoisoned, ForegroundRunRecord};
pub(crate) use run_control::{
    DurableRunCancellation, LiveAttemptControl, SelectedAttemptInterruption,
};
pub(crate) use session_ctx::{
    ChildExecutionSubstrate, FrozenRuntimeInput, RuntimePublicationIdentity, SessionCtx,
};
pub(crate) use types::HostOutcomeIteration;
pub use types::{HostOutcomeDrive, HostOutcomeReport, HostResume};
#[cfg(test)]
pub(crate) use worker_resolver::test_support::{
    eager_environment, empty_frozen_projection_for_snapshot, managed_test_host, test_activation,
};

/// The assembled Session executor components of one process. Consumers: the
/// composition roots, which select a typed role set from it, and the
/// protocol adapters and tests that drive individual components.
///
/// The builder fixes execution dependencies before these components exist.
/// The checked [`crate::session_dispatch_lifecycle`] constructor selects all
/// application capabilities together before process services start.
pub struct ExecutorComponents {
    /// Platform-managed workspace for flat, no-login local requests. It is
    /// generated (and persisted with durable storage), never compiled in.
    pub(crate) local_workspace: String,
    pub(crate) hub: Arc<ThreadEventHub>,
    /// When set, each thread commits to a durable SQLite database at
    /// `store_dir/<thread>.db`, so an awaiting run survives a process restart. When
    /// `None`, sessions use an in-memory coordinator (ephemeral).
    pub(crate) store_dir: Option<PathBuf>,
    /// The process's one wall clock, chosen by the composition that builds this
    /// Host. The dispatch pool, the reconciler, recovery drives and child
    /// schedulers all drive their Workers on it, so every time the attempt
    /// itself judges shares one timeline.
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    /// Owns temporary run-id registrations for event-driven durable completion,
    /// foreground stream relay, and publication into the existing Thread hub.
    /// The composition selects the pool/Worker completion sink over this
    /// same relay; local Session streaming uses it directly.
    pub(crate) completion: Arc<CompletionRegistry>,
    /// This process's dispatch: topology, pool, terminal reconciliation, hint.
    pub(crate) dispatch: Arc<awaken_dispatch_execution::ProcessDispatch>,
    /// The realization executor and the sandbox providers it realizes through.
    pub(crate) realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    /// Committed Thread readback over the selected authority.
    pub(crate) thread_reads: Arc<crate::CommittedReads>,
    /// The live inboxes of the attempts this process owns.
    pub(crate) live_inboxes: Arc<crate::LiveInboxes>,
    /// Capability assembly for Runs.
    pub(crate) capabilities: Arc<crate::RunCapabilities>,
    /// Session context assembly.
    pub(crate) run_assembly: Arc<crate::RunAssembly>,
    /// Session Run delivery.
    pub(crate) session_runs: Arc<crate::SessionRuns>,
    /// Session root and child Run coordination.
    pub(crate) coordination: Arc<crate::ChildRunCoordination>,
    /// Session Outcome orchestration.
    pub(crate) outcomes: Arc<crate::Outcomes>,
    /// Session usage and budget-paused Runs.
    pub(crate) budget: Arc<awaken_session_run_delivery::SessionBudgets>,
    /// Resources-context ports: Memory, Files, Artifacts.
    pub(crate) resources: Arc<capability::ResourcePorts>,
    /// Content capture (ADR-0050): sink, deployment ceiling, consent source.
    pub(crate) capture: Arc<capability::ContentCaptureCapability>,
    /// The one late binding back into the Session application built over
    /// this executor; the composition root completes it once.
    pub(crate) session_application: SessionApplicationBinding,
}
