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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU64;

use awaken_agent_contract::agent::awaiting::{
    AwaitReason, AwaitTarget, PermissionDecision, ResumeTicket, ToolCallAuthority,
};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunRecoverySource};
use awaken_dispatch_erasure::AnyDispatchStore;
use awaken_dispatch_execution::{CompletionSink, WorkerResolver};
use awaken_ext_skills::{SkillRegistry, SkillSpec};
#[cfg(any(test, feature = "test-support"))]
use awaken_resource_contract::{FileCatalog, FileStore};
#[cfg(test)]
use awaken_dispatch_contract::RunDispatch;
use awaken_dispatch_contract::{Inbox, PendingInput, SubmitOptions};
use awaken_runtime::{DirectAttemptDriver, Runtime};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_session_application_contract::Pending;
use awaken_tool_contract::tool::ToolOutput;

use awaken_ext_compact::CompactConfig;

use crate::background::BackgroundRuns;
use crate::hub::{ThreadEvent, ThreadEventHub};
use crate::store::HostCommit;
use awaken_ext_goal::grader::{DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};

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
pub use build::HostProcess;
mod dependencies;
pub(crate) use dependencies::ApplicationPortBinding;
pub use dependencies::{
    HostDependencies, SessionApplicationBindingError, SessionApplicationBindings,
    SessionExecutionDependencies,
};
pub(crate) mod capability;
pub use capability::{TrustedHostPolicy, WarmPool};
pub(crate) mod boundary_run;
mod claimed_run;
mod completion;
mod coordination;
mod credential_capabilities;
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
pub(crate) use completion::CompletionRegistry;
pub(crate) use foreground_run::{ForegroundRun, ForegroundRunPoisoned, ForegroundRunRecord};
pub(crate) use run_control::{
    DurableRunCancellation, LiveAttemptControl, SelectedAttemptInterruption,
};
pub(crate) use session_ctx::{
    ChildExecutionSubstrate, ClaimedRuntimeInput, RuntimePublicationIdentity, SessionCtx,
};
pub use types::{
    CommittedStepReceipt, HostError, HostErrorKind, HostOutcomeDrive, HostOutcomeIteration,
    HostOutcomeReport, HostResume,
};
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
    /// Owns temporary run-id registrations for event-driven durable completion,
    /// foreground stream relay, and publication into the existing Thread hub.
    /// Injected into the pool as its `CompletionSink` and into each local Session
    /// worker as its `StreamSink`.
    pub(crate) completion: Arc<CompletionRegistry>,
    /// This process's dispatch: topology, pool, terminal reconciliation, hint.
    pub(crate) dispatch: Arc<awaken_dispatch_execution::ProcessDispatch>,
    /// The realization executor and the sandbox providers it realizes through.
    pub(crate) realization_executor: Arc<crate::realization_executor::RealizationExecutor>,
    /// Committed Thread readback over the selected authority.
    pub(crate) thread_reads: Arc<crate::ThreadReads>,
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
    pub(crate) budget: Arc<crate::BudgetAccounting>,
    /// Resources-context ports: Memory, Files, Artifacts.
    pub(crate) resources: Arc<capability::ResourcePorts>,
    /// Content capture (ADR-0050): sink, deployment ceiling, consent source.
    pub(crate) capture: Arc<capability::ContentCaptureCapability>,
    /// The edges back into the Session application built over this Host.
    pub(crate) session_application: SessionApplicationBindings,
}

impl SharedHost {
    /// Persist every Agent-authored output before the environment can be disposed.
    /// The `(Session, logical path, content)` harvest key makes retries idempotent;
    /// Files API reads only this durable catalog and never scan the Sandbox.
    pub async fn harvest_thread_artifacts(
        &self,
        thread: &str,
    ) -> Result<
        crate::artifact_harvest::HarvestedArtifacts,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.realization_executor
            .effects()
            .artifacts()
            .harvest(thread)
            .await
    }
}
