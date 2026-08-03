//! `SharedHost` — the protocol-neutral, thread-keyed session substrate.
//!
//! This is the "waist" both protocol adapters (Managed Agents, AI SDK) drive. It
//! owns one sandboxed runtime + commit coordinator per **thread id**, and exposes
//! neutral operations — `run`, `resume`, `committed_messages` — over that
//! shared state. Because both adapters key by the same thread id and mutate the
//! same coordinator and awaiting-run position, a turn started through one protocol
//! can be observed or resumed through the other on the *same thread*.
//!
//! It names no protocol vocabulary: outcomes are the neutral [`RunState`] plus an
//! optional [`PendingTool`]; each adapter maps those onto its own wire shape.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::awaiting::{AwaitReason, ResumeTicket};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
#[cfg(test)]
use awaken_agent_contract::agent::run::EndCause;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Scope, StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunRecoverySource};
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_ext_skills::{SkillRegistry, SkillSpec};
use awaken_file_store::FileStore;
use awaken_resource_contract::FileCatalog;
use awaken_run_ingress::{
    AnyDispatchStore, CompletionSink, DEFAULT_LEASE_MS, DispatchPool, DispatchQueue,
    DispatchServiceConfig, DurableRunIngress, Inbox, PendingInput, RunDispatch, SubmitOptions,
    SystemClock, WorkerResolver,
};
use awaken_runtime::{DirectRunIngress, RunIngress, RunService, Runtime};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::RunDelegations;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_runtime_contract::tool::ToolOutput;
#[cfg(any(test, feature = "test-support"))]
use awaken_store_inmem::MemoryCommitCoordinator;
#[cfg(any(test, feature = "test-support"))]
use awaken_store_inmem::MemoryStreamCheckpointStore;
// The Workdir-tier sandbox realized through the neutral provisioning contract:
// `LocalProvider::create_sandbox` yields a `LocalSandbox` whose host-tier helpers
// (rooted tools, repos, artifacts) the host composes into each session's runtime.
use awaken_sandbox_local::LocalProvider;
use awaken_session_contract::DelegatedRun;
use awaken_store_fs::{FsCommitCoordinator, FsStreamCheckpointStore};
use awaken_store_sqlite::SqliteCommitCoordinator;

use awaken_ext_compact::{CompactConfig, CompactPlugin};

use crate::background::BackgroundRuns;
use crate::compact::{
    compact_backend as build_compact_backend, compact_runner as build_compact_runner,
};
use crate::config::{
    build_runtime, config_permission_ruleset, server_config, server_gate_with_toolsets,
};
use crate::delegate::HostRunDelegationService;
use crate::hub::{ThreadEvent, ThreadEventHub};
use crate::store::HostCommit;
use awaken_ext_goal::grader::{DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};

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
    pub credential_realization: awaken_runtime_contract::CredentialRealizationCapabilities,
}

/// A unique temp-dir base for a sub-agent sandbox provider. `kind` tags the use
/// (e.g. `judge`, `deleg`); empty for the host's own provider.
fn sub_base(kind: &str) -> PathBuf {
    let n = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let name = if kind.is_empty() {
        format!("{pid}-{n}")
    } else {
        format!("{pid}-{kind}-{n}")
    };
    std::env::temp_dir().join("awaken-coordinator").join(name)
}

pub(crate) use crate::store::sanitize_thread;

/// Wall-clock milliseconds since the Unix epoch — the dispatch queue's lease and
/// recovery clock (slice D). Falls back to `0` if the clock is before the epoch.
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

mod build;
mod completion;
pub use completion::remote_worker_placement;
pub use completion::self_hosted_inference_holder;
mod credential_capabilities;
mod run;
mod session;
mod session_ctx;
mod terminal_reconciliation;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::{MemoryHostModel, bind_test_memory};
mod types;
mod worker_resolver;

pub(crate) use completion::CompletionRegistry;
pub(crate) use session_ctx::{SessionCtx, SessionState};
pub use types::{
    HostError, HostErrorKind, HostOutcomeIteration, HostOutcomeReport, HostResume, PendingTool,
    RunResult,
};
pub(crate) use worker_resolver::HostWorkerResolver;

/// The protocol-neutral, thread-keyed session substrate shared by every adapter.
pub struct SharedHost {
    /// Platform-managed workspace for flat, no-login local requests. It is
    /// generated (and persisted with durable storage), never compiled in.
    pub(crate) local_workspace: String,
    /// Process-local projections materialized for prepared Sessions. The private
    /// slot makes their lifecycle atomic while the durable manifest stays authoritative.
    pub(crate) session_slots: crate::session_slot::SessionRuntimeSlots,
    /// The host DEFAULT executor: used by auxiliary sub-agents (judge, compactor,
    /// memory) and by an explicitly local composition with no
    /// [`InferenceExecutorMaterializer`]. Once a materializer is installed, a rejected
    /// publication pin fails closed instead of falling back to this executor.
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    /// Per-thread model→executor routing (R1/R2). See [`crate::inference_routing`].
    pub(crate) inference_routing: crate::inference_routing::InferenceRouting,
    /// ACP runtime backend (R3/R4): serves `acp:*` sessions on an external CLI.
    pub(crate) acp: Option<Arc<crate::acp_backend::AcpBackend>>,
    /// Higher-layer transport adapter for Session-owned tools exposed to ACP.
    /// The Host names only this port; concrete MCP server assembly remains in
    /// `awaken-coordinator` and does not add a protocol dependency to the substrate.
    pub(crate) acp_tool_exporter: Option<Arc<dyn crate::AcpToolExporter>>,
    /// Remote attempt adapter injected by the composition root. The neutral host
    /// owns only the `RunAttemptExecutor` port and never names the A2A protocol.
    pub(crate) remote_attempt_executor:
        Option<Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>>,
    pub(crate) remote_credential_realization:
        awaken_runtime_contract::CredentialRealizationCapabilities,
    /// Optional application wrapper around the complete per-Session attempt
    /// router. It cannot replace or bypass the built-in backend registry.
    pub(crate) application_attempt_decorator: Option<AttemptExecutorDecorator>,
    /// Optional claim-time projection into the authoritative Session environment.
    pub(crate) application_session_provisioner:
        Option<Arc<dyn crate::ApplicationSessionProvisioner>>,
    /// Outbound Control command paired with the provisioner. A Worker must
    /// never fall back to installing its locally produced plan.
    pub(crate) application_session_control: Option<Arc<dyn crate::ApplicationSessionControlClient>>,
    pub(crate) provider: LocalProvider,
    /// Provider for the Session-owned environment shared by Native/ACP/children.
    /// Kept separate from deliberately-fresh housekeeping sandboxes.
    pub(crate) session_provider: crate::session_environment::SessionEnvironmentProvider,
    /// Trusted-host environment selected only for BackendOwned provisioning.
    /// It is a policy branch over the same Session owner, not a second executor.
    pub(crate) backend_owned_session_provider:
        Option<crate::session_environment::SessionEnvironmentProvider>,
    /// The composition root installed the authoritative Session provider. ACP
    /// assembly must reuse it instead of constructing a deployment-derived peer.
    pub(crate) session_provider_explicit: bool,
    pub(crate) judge_snapshot: Option<ExecutableAgentSnapshot>,
    pub(crate) client_tools: HashSet<String>,
    /// The skill offering (ADR-0036): the static configured set, the optional durable
    /// `/v1/skills` catalog, and its sync-read cache — grouped behind one type that
    /// owns the cache↔store coherence invariant. See [`crate::skill_catalog`].
    pub(crate) skills: crate::skill_catalog::SkillCatalog,
    /// Whether a `context: fork` Skill reuses the parent Session environment.
    /// Ordinary delegated child Runs always share that environment.
    pub(crate) skill_fork_placement: crate::skills::SkillForkPlacement,
    /// Runtime plugins this host activates on every thread, and their config
    /// sections (e.g. the tool state machine). Empty by default.
    pub(crate) plugin_ids: Vec<String>,
    pub(crate) plugin_config: std::collections::BTreeMap<String, serde_json::Value>,
    /// One provider registry is used to derive authoring schema and to dispatch
    /// Native/ACP WebSearch calls. External compositions extend this registry;
    /// sessions never construct a provider-specific side registry.
    pub(crate) web_search_providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    /// Canonical exact materializer used by runtime extensions. The Managed
    /// adapter may retain another clone, but both share the same repositories,
    /// secret store, revision checks, and extension-consumer registry.
    pub(crate) credential_materializer: Option<crate::PinnedCredentialMaterializer>,
    pub(crate) hub: Arc<ThreadEventHub>,
    /// When set, each thread commits to a durable SQLite database at
    /// `store_dir/<thread>.db`, so an awaiting run survives a process restart. When
    /// `None`, sessions use an in-memory coordinator (ephemeral).
    pub(crate) store_dir: Option<PathBuf>,
    /// The cell server this host is a database-less **worker** of, if any. When set,
    /// every thread's read boundary is a non-authoritative recovery projection;
    /// writes use the attempt's claim-fenced operation coordinator. Set via
    /// [`with_upstream`](Self::with_upstream); `None` is a store-owning server/host.
    pub(crate) upstream: Option<awaken_worker_transport_security::WorkerUpstream>,
    /// Sole adapter for opaque credentials owned by this Worker process. Control
    /// and Session code retain only exact non-secret references.
    pub(crate) worker_credential_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    /// The deployment axes (store/dispatch backend, durable ingress, wake), parsed
    /// once from the environment at construction. The runtime reads this typed
    /// config instead of reaching into process env at each call site.
    pub(crate) deployment: crate::deployment_config::DeploymentConfig,
    /// Explicit Worker-side dispatch transport. Coordinator processes leave this empty
    /// and resolve the configured durable backend; Workers inject their HTTP
    /// transport here so no process-global compatibility slot becomes a second
    /// composition authority.
    pub(crate) dispatch_store_override: Option<Arc<awaken_run_ingress::AnyDispatchStore>>,
    /// When set, an ACP CLI's session is harvested/restored under this durable root
    /// (keyed by thread+adapter) so it survives a move to another directory or
    /// worker. Point it at a **shared** location for cross-machine recovery; leave
    /// `None` on a single machine (the per-thread config home is already stable).
    pub(crate) session_blob_root: Option<PathBuf>,
    /// Canonical adapter over `session_blob_root`, shared by ACP recovery and
    /// data-subject erasure so those paths cannot drift.
    pub(crate) session_blob_store: Option<Arc<awaken_run_executor_acp::FsSessionBlobStore>>,
    /// Host-level memory auxiliary-agent capability. It owns no store: every
    /// recall/extraction operation requires a Session-scoped governed binding.
    pub(crate) memory: Arc<crate::memory::MemoryRuntime>,
    /// Context compaction, when enabled with [`with_compaction`]: the resolved config
    /// plus the `compactor` sub-agent runner, sealed as one [`crate::compact::Compaction`]
    /// so the pair is present-or-absent atomically. The config drives the `compact`
    /// plugin (a `BeforeInference` hook) and a matching `KeepLast` window so summarized
    /// older turns leave the model view.
    compaction: Option<crate::compact::Compaction>,
    /// Runtime-only view of immutable Agent publications.
    pub(crate) agent_publications:
        Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
    /// Current registered Agent bindings used only as intrinsic Resource
    /// reclamation evidence.
    pub(crate) agent_resource_references:
        Option<Arc<dyn awaken_resource_contract::AgentResourceReferenceSource>>,
    /// The host's loopback MCP relay (α-reference resolver), started lazily on the first
    /// sandboxed ACP session that stages an authenticated MCP server. It holds the real
    /// bearers host-side and injects them when forwarding the sandbox's MCP calls, so the
    /// raw token never enters the sandbox. See [`crate::mcp_relay`].
    pub(crate) mcp_relay: tokio::sync::OnceCell<crate::mcp_relay::McpRelay>,
    /// Cold-Worker adapter over the configured Session Runtime. It owns only weak
    /// host wiring plus Resource/credential SPIs, so installing it cannot create
    /// an `Arc<SharedHost>` cycle or a second Vault/materialization path.
    pub(crate) dispatch_session_runtime: std::sync::RwLock<Option<crate::DispatchSessionRuntime>>,
    /// Content-addressed blob store backing the Files API, file-resource mounts, and
    /// collected artifacts. A database-less Worker carries a fail-closed adapter;
    /// immutable claim-scoped reads use `file_content_source` instead.
    pub(crate) file_store: Arc<dyn FileStore>,
    /// Sole per-kind File materialization port. Embedded composition points it at
    /// the local catalog/store pair; a database-less Worker replaces it with the
    /// claim-fenced HTTP adapter before accepting work.
    pub(crate) file_content_source: Arc<dyn crate::FileContentSource>,
    /// Logical Files-API truth: public identity, metadata, Workspace visibility,
    /// Session scope, and harvest idempotency. Bytes remain in `file_store` only.
    pub(crate) file_catalog: Arc<dyn FileCatalog>,
    /// Resources-owned command application. It is absent only on database-less
    /// Workers, which receive immutable content through `file_content_source` and
    /// cannot expose management or artifact-publication commands.
    pub(crate) file_application: Option<Arc<dyn awaken_resource_contract::FileApplicationService>>,
    /// Durable workspace ownership projection for content-addressed resources.
    /// Durable resource-plane lifecycle/reference state. It contains intrinsic
    /// Workspace/resource edges only and is independent of the IAM deployment.
    pub(crate) resource_lifecycle:
        Option<Arc<dyn awaken_resource_contract::ResourceLifecycleRepository>>,
    /// The Resources context's path-addressed Memory backend shared by API,
    /// mounts, recall, and extraction. See [`crate::memory_stores`].
    pub(crate) memory_stores: crate::memory_stores::MemoryStores,
    /// Worker-side realization port for governed MemoryStore mounts. The runtime
    /// host stores only the neutral port; the outer server composition installs
    /// the FUSE/copy adapter.
    pub(crate) memory_mounter:
        std::sync::RwLock<Option<Arc<dyn awaken_provisioning_contract::MemoryMounter>>>,
    /// An optional tool gate that replaces the default authorization gate on every
    /// thread's runtime. Used to exercise scheduled actions (ADR-0020, slice E): a
    /// gate that defers tool calls as `ScheduledAction`s so the durable dispatch
    /// worker performs them out of band.
    pub(crate) gate_override: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    /// The process-level dispatch pool (O2), spawned once by `mount` when durable
    /// ingress is enabled. It is the sole claimer of the one shared queue and drives
    /// every session's runs by routing each claimed run back to the worker that owns
    /// its thread. Held here so background submit / cross-thread relay nudge it and
    /// so it lives for the process's lifetime.
    pub(crate) dispatch_pool: std::sync::OnceLock<Arc<DispatchPool<AnyDispatchStore>>>,
    /// Marks the coordinator-only committed-terminal reconciliation daemon as
    /// started. The detached task owns only a Weak host reference, so this flag
    /// prevents duplicate loops without extending the Host lifetime.
    pub(crate) terminal_reconciliation_started: std::sync::OnceLock<()>,
    /// Wakes a foreground durable submitter the instant the pool settles its run
    /// (event-driven completion), so the durable foreground path never pays a poll
    /// interval. Injected into the pool as its `CompletionSink`.
    pub(crate) completion: Arc<CompletionRegistry>,
    pub(crate) environment_binding_sink:
        std::sync::RwLock<Option<Arc<dyn awaken_session_contract::SessionEnvironmentBindingSink>>>,
    /// The one Host-owned subject-tagged captured-content sink (ADR-0050).
    /// Composition may install it after the shared Host is assembled; sessions
    /// snapshot the current sink when they are created. `None` = spans only.
    pub(crate) capture_sink:
        std::sync::RwLock<Option<Arc<dyn awaken_runtime_contract::CaptureSink>>>,
    /// Deployment-resolved capture ceiling/redactor. Per-request consent and
    /// subject attribution may only narrow or activate this value.
    pub(crate) capture_decision: awaken_runtime_contract::CaptureDecision,
    /// Read-only Control consent port. The default null source preserves open
    /// standalone behavior; managed composition replaces it explicitly.
    pub(crate) data_subject_consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
    /// Globally-registered management tool executables (ADR-0052 D3/D4). Registered
    /// on every thread's runtime (the executor registry stays global); only the
    /// reserved-scope assistant's compiled config *names* them, so no other run can
    /// invoke them. Their ids are also pre-authorized on the gate (read-only tools).
    /// Empty by default.
    pub(crate) admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
}
