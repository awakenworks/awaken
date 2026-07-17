//! `SharedHost` — the protocol-neutral, thread-keyed session substrate.
//!
//! This is the "waist" both protocol adapters (Managed Agents, AI SDK) drive. It
//! owns one sandboxed runtime + commit coordinator per **thread id**, and exposes
//! neutral operations — `run`, `resume`, `committed_messages` — over that
//! shared state. Because both adapters key by the same thread id and mutate the
//! same coordinator and parked-run position, a turn started through one protocol
//! can be observed or resumed through the other on the *same thread*.
//!
//! It names no protocol vocabulary: outcomes are the neutral [`Phase`] plus an
//! optional [`PendingTool`]; each adapter maps those onto its own wire shape.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::{WaitingReason, WaitingTicket};
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_ext_goal::{DelegateGrader, GoalPlugin, GoalSpec, Grader, KeywordGrader};
use awaken_ext_skills::{SkillRegistry, SkillSpec};
use awaken_file_store::{FileStore, InMemoryFileStore};
use awaken_run_ingress::{
    AnyDispatchStore, CompletionSink, DEFAULT_LEASE_MS, DispatchPool, DispatchQueue,
    DispatchServiceConfig, DurableRunIngress, RunExecutionRequest, SubmitOptions, SystemClock,
    WorkerResolver,
};
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamCheckpointStore};
use awaken_runtime::{DirectRunIngress, RunIngress, Runtime};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::RuntimeCatalogInstaller;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::agent_resolver::AgentResolver;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::{ToolExecutor, ToolExecutorProvider, ToolOutput};
// The Workdir-tier sandbox realized through the neutral provisioning contract:
// `LocalProvider::create_sandbox` yields a `LocalSandbox` whose host-tier helpers
// (rooted tools, repos, artifacts) the host composes into each session's runtime.
use awaken_sandbox_local::{LocalProvider, LocalSandbox};
use awaken_store_fs::{FsCommitCoordinator, FsStreamCheckpointStore};
use awaken_store_sqlite::SqliteCommitCoordinator;

use awaken_ext_compact::{CompactConfig, CompactPlugin};

use crate::agent_catalog::AgentCatalog;
use crate::background::BackgroundRuns;
use crate::compact::compact_runner as build_compact_runner;
use crate::config::{build_runtime, config_permission_ruleset, server_config, server_gate_with};
use crate::delegate::DelegationResolver;
use crate::hub::{ThreadEvent, ThreadEventHub};
use crate::judge::{DEFAULT_JUDGE_INSTRUCTIONS, default_judge_agent};
use crate::memory::{DEFAULT_MEMORY_INSTRUCTIONS, MemoryExtraction, default_memory_agent};
use crate::store::HostCommit;

pub(crate) static BASE_SEQ: AtomicU64 = AtomicU64::new(0);

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
    std::env::temp_dir().join("awaken-server").join(name)
}

pub(crate) use crate::store::sanitize_thread;

/// Wall-clock milliseconds since the Unix epoch — the dispatch queue's lease and
/// recovery clock (slice D). Falls back to `0` if the clock is before the epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub use crate::mcp::PreparedMcpServer;

use crate::judge::HostSubagentRunner;
use crate::provisioning::StagedResources;

mod build;
mod completion;
mod run;
mod session;
mod session_ctx;
#[cfg(test)]
mod tests;
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
    /// The host DEFAULT executor: used by auxiliary sub-agents (judge, compactor,
    /// memory) and as the fallback when no [`ExecutorProvider`] resolves a thread's
    /// model. The main run resolves its executor per thread via `resolve_executor`.
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    /// Per-thread model→executor routing (R1/R2). See [`crate::model_route`].
    pub(crate) model_route: crate::model_route::ThreadModelBinding,
    /// ACP runtime backend (R3/R4): serves `acp:*` sessions on an external CLI.
    pub(crate) acp: Option<Arc<crate::acp_backend::AcpBackend>>,
    pub(crate) provider: LocalProvider,
    grader: Arc<dyn Grader>,
    pub(crate) client_tools: HashSet<String>,
    /// The skill offering (ADR-0036): the static configured set, the optional durable
    /// `/v1/skills` catalog, and its sync-read cache — grouped behind one type that
    /// owns the cache↔store coherence invariant. See [`crate::skill_catalog`].
    pub(crate) skills: crate::skill_catalog::SkillCatalog,
    /// The delegate agent roster (local + A2A-remote) behind one type owning the
    /// `remotes ⊆ advertised` invariant. See [`crate::delegate::Delegates`].
    pub(crate) delegates: crate::delegate::Delegates,
    /// Whether a native subagent (delegation / skill fork) reuses the parent agent's
    /// sandbox (`true`, the default — `默认共用`) or runs in a fresh, isolated one.
    /// The workspace-sharing knob for subagents; out-of-band housekeeping sub-runs
    /// (judge / memory / compaction) are always isolated regardless.
    pub(crate) subagent_reuse_sandbox: bool,
    /// Runtime plugins this host activates on every thread, and their config
    /// sections (e.g. the tool state machine). Empty by default.
    pub(crate) plugin_ids: Vec<String>,
    pub(crate) plugin_config: std::collections::BTreeMap<String, serde_json::Value>,
    pub(crate) sessions: tokio::sync::Mutex<HashMap<String, Arc<SessionCtx>>>,
    pub(crate) hub: Arc<ThreadEventHub>,
    /// When set, each thread commits to a durable SQLite database at
    /// `store_dir/<thread>.db`, so a parked run survives a process restart. When
    /// `None`, sessions use an in-memory coordinator (ephemeral).
    pub(crate) store_dir: Option<PathBuf>,
    /// The cell server this host is a database-less **worker** of, if any. When set,
    /// every thread's commit boundary is a [`HostCommit::Remote`] that posts facts to
    /// the server's commit ingest — the worker holds no store. Set via
    /// [`with_upstream`](Self::with_upstream); `None` is a store-owning server/host.
    pub(crate) upstream: Option<String>,
    /// The deployment axes (store/dispatch backend, durable ingress, wake), parsed
    /// once from the environment at construction. The runtime reads this typed
    /// config instead of reaching into process env at each call site.
    pub(crate) deployment: crate::deployment_config::DeploymentConfig,
    /// When set, an ACP CLI's session is harvested/restored under this durable root
    /// (keyed by thread+adapter) so it survives a move to another directory or
    /// worker. Point it at a **shared** location for cross-machine recovery; leave
    /// `None` on a single machine (the per-thread config home is already stable).
    pub(crate) session_blob_root: Option<PathBuf>,
    /// Out-of-band memory extraction, when enabled with [`with_memory`]. After a
    /// turn reaches a natural end it fires a background `memory-extractor` sub-run.
    memory: Option<Arc<MemoryExtraction>>,
    /// The relevance selector for recall (a `memory-selector` sub-agent), wired
    /// into the memory recall plugin when memory is enabled.
    memory_selector: Option<Arc<dyn awaken_ext_memory::RecallSelector>>,
    /// Context compaction, when enabled with [`with_compaction`]: the resolved config
    /// plus the `compactor` sub-agent runner, sealed as one [`crate::compact::Compaction`]
    /// so the pair is present-or-absent atomically. The config drives the `compact`
    /// plugin (a `BeforeInference` hook) and a matching `KeepLast` window so summarized
    /// older turns leave the model view.
    compaction: Option<crate::compact::Compaction>,
    /// The config data plane, when the server exposes `/v1/config/*`. A thread's
    /// runtime config is the installed (published) config for its agent, if any,
    /// else the built-in default (slice A).
    pub(crate) config_service: Option<Arc<crate::config_plane::ConfigService>>,
    /// Per-thread MCP servers staged by a session's `prepare_session` (ADR-0043
    /// Phase 3), consumed when the thread's context is first built. Keyed by
    /// thread id; a thread with no entry connects to no MCP server.
    pub(crate) thread_mcp: std::sync::Mutex<HashMap<String, Vec<PreparedMcpServer>>>,
    /// The host's loopback MCP relay (α-reference resolver), started lazily on the first
    /// sandboxed ACP session that stages an authenticated MCP server. It holds the real
    /// bearers host-side and injects them when forwarding the sandbox's MCP calls, so the
    /// raw token never enters the sandbox. See [`crate::mcp_relay`].
    pub(crate) mcp_relay: tokio::sync::OnceCell<crate::mcp_relay::McpRelay>,
    /// Per-thread staged resource mounts + prompt fragments (ADR-0038), set by a
    /// session's `prepare_session` and consumed by `sandbox_spec` (mounts) and the
    /// run's system prompt (fragments). A thread with no entry mounts nothing. A shared
    /// handle (like [`Self::thread_egress`]) so a sandboxed ACP channel source can carry
    /// the same resource mounts into the bwrap/container sandbox it launches the CLI in.
    pub(crate) thread_resources: std::sync::Arc<std::sync::Mutex<HashMap<String, StagedResources>>>,
    /// Per-thread network-egress denial, set by a session's `prepare_session` from its
    /// environment's networking policy. A thread with no entry (or `false`) shares the
    /// host network; `true` runs its `bash` under a `bwrap --unshare-net` namespace
    /// with no egress. A shared handle, so a sandboxed ACP channel source can follow
    /// the same registrations (see [`crate::SandboxChannelSource`]).
    pub(crate) thread_egress: crate::sandbox_source::ThreadEgress,
    /// Content-addressed blob store backing the Files API, file-resource mounts, and
    /// collected artifacts. In-memory by default (one server process).
    pub(crate) file_store: Arc<dyn FileStore>,
    /// The memory resource plane's *content* backends (ADR-0038/0053): the id-keyed
    /// read-write blob store + the path-addressed CAS `/memories` store, both governed
    /// by one storage-dir durability rule and grouped behind one type that owns that
    /// construction invariant. See [`crate::memory_stores`].
    pub(crate) memory_stores: crate::memory_stores::MemoryStores,
    /// The control-plane registry of memory-store **identity** (id/name/description/
    /// metadata/archived), ADR-0038. A different bounded context from the content
    /// stores above: it lives beside its siblings (`McpStore`, `WebhookStore`) in
    /// `awaken-config-resolver` and is injected by the composition root with the durable
    /// admin backend so a store's identity survives a restart and the admin assistant
    /// can enumerate stores. Defaults to a process-lifetime in-memory registry.
    pub(crate) memory_registry: Arc<dyn awaken_config_resolver::MemoryStoreRegistry>,
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
    /// Wakes a foreground durable submitter the instant the pool settles its run
    /// (event-driven completion), so the durable foreground path never pays a poll
    /// interval. Injected into the pool as its `CompletionSink`.
    pub(crate) completion: Arc<CompletionRegistry>,
    /// Where this host's runs execute tool calls (ADR-0044/0046): the session-wide
    /// remote hand + the per-run placement provider, sealed behind one type owning
    /// their precedence (provider placement overrides the hand). Cloned into each
    /// `SessionCtx`. See [`crate::hand_placement`].
    pub(crate) hand_placement: crate::hand_placement::HandPlacement,
    /// Subject-tagged captured-content sink (ADR-0050): when set, a run whose
    /// capture level permits content writes it here (attributed to the
    /// `AWAKEN_CONTENT_SUBJECT` on the open surface). `None` = spans only.
    pub(crate) capture_sink: Option<Arc<dyn awaken_runtime_contract::CaptureSink>>,
    /// Globally-registered management tool executables (ADR-0052 D3/D4). Registered
    /// on every thread's runtime (the executor registry stays global); only the
    /// reserved-scope assistant's compiled config *names* them, so no other run can
    /// invoke them. Their ids are also pre-authorized on the gate (read-only tools).
    /// Empty by default.
    pub(crate) admin_tools: Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>>,
    /// Whether this host launches its ACP CLI as a **trusted-local** (non-sandboxed)
    /// process, so a staged MCP server's raw bearer may be handed to the CLI inline (β)
    /// rather than as a secretless reference (α). Default `false` (α) — the fail-closed
    /// choice a managed/multi-tenant host keeps; a single-machine trusted deployment
    /// opts into β via [`SharedHost::with_trusted_acp_mcp`].
    pub(crate) mcp_trusted_inline: bool,
}
