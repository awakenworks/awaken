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
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::awaiting::{
    AwaitReason, AwaitTarget, PermissionDecision, ResumeTicket,
};
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Scope, StateKey, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunRecoverySource};
use awaken_ext_skills::{SkillRegistry, SkillSpec};
#[cfg(any(test, feature = "test-support"))]
use awaken_resource_contract::{FileCatalog, FileStore};
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
// The Workdir-tier sandbox realized through the neutral provisioning contract:
// `LocalProvider::create_sandbox` yields a `LocalSandbox` whose host-tier helpers
// (rooted tools, repos, artifacts) the host configures into each session's runtime.
use awaken_sandbox_local::LocalProvider;
use awaken_session_contract::{DelegatedRun, Pending};

use awaken_ext_compact::{CompactConfig, CompactPlugin};

use crate::background::BackgroundRuns;
use crate::compact::{
    compact_backend as build_compact_backend, compact_runner as build_compact_runner,
};
use crate::config::{
    build_runtime_with_authorization, config_permission_ruleset, effective_tool_authorization,
    server_config,
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

mod build;
mod completion;
mod coordination;
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
mod tests;
#[cfg(test)]
pub(crate) use tests::install_test_memory_mounter;
#[cfg(test)]
pub(crate) use tests::{MemoryHostModel, test_resource_validator};
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

/// The protocol-neutral, thread-keyed session substrate shared by every adapter.
pub struct SharedHost {
    /// Platform-managed workspace for flat, no-login local requests. It is
    /// generated (and persisted with durable storage), never compiled in.
    pub(crate) local_workspace: String,
    /// Process-local projections materialized for prepared Sessions. The private
    /// slot makes their lifecycle atomic while the durable manifest stays authoritative.
    pub(crate) session_slots: crate::session_slot::SessionRuntimeSlots,
    /// The host DEFAULT executor: used by auxiliary sub-agents (judge, compactor,
    /// memory) and by an explicitly local deployment with no
    /// [`InferenceExecutorMaterializer`]. Once a materializer is installed, a rejected
    /// publication pin fails closed instead of falling back to this executor.
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    /// Per-thread model→executor routing (R1/R2). See [`crate::inference_routing`].
    pub(crate) inference_routing: crate::inference_routing::InferenceRouting,
    /// ACP runtime backend (R3/R4): serves `acp:*` sessions on an external CLI.
    pub(crate) acp: Option<Arc<crate::acp_backend::AcpBackend>>,
    /// Higher-layer transport adapter for Session-owned tools exposed to ACP.
    /// The Host names only this contract; concrete MCP server setup remains in
    /// `awaken-coordinator` and does not add a protocol dependency to the substrate.
    pub(crate) acp_tool_exporter: Option<Arc<dyn crate::AcpToolExporter>>,
    /// Remote attempt adapter injected by the process startup. The neutral host
    /// owns only the `RunAttemptExecutor` contract and never names the A2A protocol.
    pub(crate) remote_attempt_executor:
        Option<Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>>,
    pub(crate) remote_credential_realization:
        awaken_runtime_contract::CredentialRealizationCapabilities,
    /// Optional application wrapper around the complete per-Session attempt
    /// router. It cannot replace or bypass the built-in backend registry.
    pub(crate) attempt_decorator: Option<AttemptExecutorDecorator>,
    /// Outbound claim-fenced Control client used to realize an already-frozen
    /// Session projection on this Worker.
    pub(crate) session_control: Option<Arc<dyn awaken_run_ingress_contract::ClaimedSessionControl>>,
    pub(crate) provider: LocalProvider,
    /// Provider for the Session-owned environment shared by Native/ACP/children.
    /// Kept separate from deliberately-fresh housekeeping sandboxes.
    pub(crate) session_provider: crate::session_environment::SessionEnvironmentProvider,
    /// Deployment-owned byte custody for full-Environment checkpoints. Desired
    /// lifecycle state remains in the Session aggregate; this port stores bytes
    /// only. Hosted startup injects the regional encrypted adapter.
    pub(crate) environment_checkpoint_store:
        Option<Arc<dyn awaken_provisioning_contract::SandboxCheckpointStore>>,
    /// Product-plane single-flight preparation for caller-owned CacheVolume
    /// paths. The selected Sandbox provider remains an opaque-path consumer.
    pub(crate) cache_volume_prewarmer: crate::cache_volume::CacheVolumePrewarmer,
    /// Trusted-host environment selected only for BackendOwned provisioning.
    /// It is a policy branch over the same Session owner, not a second executor.
    pub(crate) backend_owned_session_provider:
        Option<crate::session_environment::SessionEnvironmentProvider>,
    /// The process startup installed the authoritative Session provider. ACP
    /// ACP execution must reuse it instead of constructing a deployment-derived peer.
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
    /// Native/ACP WebSearch calls. External deployments extend this registry;
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
    /// [`with_worker_upstream`](Self::with_worker_upstream); `None` is a
    /// store-owning server/host.
    pub(crate) upstream: Option<awaken_worker_transport_security::WorkerUpstream>,
    /// Optional Resource-transport encoder installed by a remote Worker. The
    /// runtime knows only the neutral Resources contract, never an HTTP wire type.
    pub(crate) memory_reference_encoder: Option<
        Arc<
            dyn awaken_resource_contract::MemoryMaterializationReferenceEncoder<
                    awaken_run_ingress::RunClaim,
                >,
        >,
    >,
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
    /// dispatch authority.
    pub(crate) dispatch_store_override: Option<Arc<awaken_run_ingress::AnyDispatchStore>>,
    /// Coordinator-owned durable capability. Runtime and Worker builds contain
    /// no concrete Store acquisition; a database-less Worker leaves this absent.
    pub(crate) authority: Option<Arc<dyn crate::RuntimeAuthority>>,
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
    /// Runtime-only view of immutable Agent publications.
    pub(crate) agent_publications:
        Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
    /// The host's loopback MCP relay (α-reference resolver), started lazily on the first
    /// sandboxed ACP session that stages an authenticated MCP server. It holds the real
    /// bearers host-side and injects them when forwarding the sandbox's MCP calls, so the
    /// raw token never enters the sandbox. See [`crate::mcp_relay`].
    pub(crate) mcp_relay: tokio::sync::OnceCell<crate::mcp_relay::McpRelay>,
    /// Cold-Worker adapter over the configured Session Runtime. It owns only weak
    /// host wiring plus Resource/credential SPIs, so installing it cannot create
    /// an `Arc<SharedHost>` cycle or a second Vault/materialization path.
    pub(crate) dispatch_session_runtime: std::sync::RwLock<Option<crate::DispatchSessionRuntime>>,
    /// Weak composition edge to the Session application's sole coordination
    /// admission owner. This is executable wiring only; it contains no roster,
    /// Thread relationship, operation receipt, or lifecycle state.
    pub(crate) agent_coordination: std::sync::RwLock<
        Option<std::sync::Weak<dyn awaken_session_contract::SessionAgentCoordination>>,
    >,
    /// Weak composition edge to the existing Session Run admission owner.
    /// BackgroundTask completion uses this port only to publish a deterministic
    /// same-Thread attention Run; task lifecycle and result truth remain in
    /// committed Thread State.
    pub(crate) session_background_runs: std::sync::RwLock<
        Option<std::sync::Weak<dyn awaken_session_contract::SessionRunBackgroundApplication>>,
    >,
    /// Content-addressed blob store backing the Files API, file-resource mounts, and
    /// collected artifacts. A database-less Worker carries a fail-closed adapter;
    /// immutable claim-scoped reads use `file_content_source` instead.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_store: Arc<dyn FileStore>,
    /// Sole per-kind File materialization service. An embedded process points it at
    /// the local catalog/store pair; a database-less Worker replaces it with the
    /// claim-fenced HTTP adapter before accepting work.
    pub(crate) file_content_source: Arc<dyn crate::FileContentSource<awaken_run_ingress::RunClaim>>,
    /// Logical Files-API truth: public identity, metadata, Workspace visibility,
    /// Session scope, and harvest idempotency. Bytes remain in `file_store` only.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_catalog: Arc<dyn FileCatalog>,
    /// Full Resources-owned File application retained for local management and
    /// test-support composition. Product Coordinator and database-less Worker
    /// processes receive only the narrower content and publication ports.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) file_application: Option<Arc<dyn awaken_resource_contract::FileApplicationService>>,
    /// Sole Runtime-to-Resources artifact command edge. Embedded deployments
    /// install the local application adapter; database-less Workers install the
    /// claim-fenced HTTP client.
    pub(crate) artifact_publisher:
        Arc<dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>>,
    /// Durable workspace ownership projection for content-addressed resources.
    /// Durable resource-plane lifecycle/reference state. It contains intrinsic
    /// Workspace/resource edges only and is independent of the IAM deployment.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) resource_reclamation:
        Option<Arc<dyn awaken_resource_contract::ResourceReclamationRepository>>,
    /// The Resources context's path-addressed Memory backend shared by API,
    /// mounts, recall, and extraction. See [`crate::memory_stores`].
    pub(crate) memory_stores: crate::memory_stores::MemoryStores,
    /// Worker-side service for governed MemoryStore mounts. The Runtime Host
    /// stores only the neutral contract; the outer server process installs
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
    /// The run-ingress-owned maintenance loop used by a coordinator-only Host.
    /// Local pools already own the same canonical maintenance mechanism.
    pub(crate) dispatch_maintenance: std::sync::OnceLock<awaken_run_ingress::DispatchMaintenance>,
    /// Owns temporary run-id registrations for event-driven durable completion,
    /// foreground stream relay, and publication into the existing Thread hub.
    /// Injected into the pool as its `CompletionSink` and into each local Session
    /// worker as its `StreamSink`.
    pub(crate) completion: Arc<CompletionRegistry>,
    /// Database-less Workers replace the process-local relay with the one
    /// authenticated, claim-fenced Coordinator publisher.
    pub(crate) worker_stream_publisher: Option<Arc<dyn awaken_run_ingress::ClaimedStreamPublisher>>,
    pub(crate) environment_binding_sink:
        std::sync::RwLock<Option<Arc<dyn awaken_session_contract::SessionEnvironmentBindingSink>>>,
    /// The one Host-owned subject-tagged captured-content sink (ADR-0050).
    /// Process startup may install it after the shared Host is created; sessions
    /// snapshot the current sink when they are created. `None` = spans only.
    pub(crate) capture_sink:
        std::sync::RwLock<Option<Arc<dyn awaken_runtime_contract::CaptureSink>>>,
    /// Deployment-resolved capture ceiling/redactor. Per-request consent and
    /// subject attribution may only narrow or activate this value.
    pub(crate) capture_decision: awaken_runtime_contract::CaptureDecision,
    /// Read-only Control consent service. The default null source preserves open
    /// standalone behavior; a managed process replaces it explicitly.
    pub(crate) data_subject_consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
    /// Globally-registered management tool executables (ADR-0052 D3/D4). Registered
    /// on every thread's runtime (the executor registry stays global); only the
    /// reserved-scope assistant's compiled config *names* them, so no other run can
    /// invoke them. Their ids are also pre-authorized on the gate (read-only tools).
    /// Empty by default.
    pub(crate) admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
}
