//! `SharedHost` — the protocol-neutral, thread-keyed session substrate.
//!
//! This is the "waist" both protocol adapters (Managed Agents, AI SDK) drive. It
//! owns one sandboxed runtime + commit coordinator per **thread id**, and exposes
//! neutral operations — `run_turn`, `resume`, `committed_messages` — over that
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
use awaken_agent_contract::store::stream_checkpoint::StreamCheckpointStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_goal::{DelegateGrader, GoalPlugin, GoalSpec, Grader, KeywordGrader};
use awaken_ext_skills::{SkillRegistry, SkillSpec};
use awaken_protocol_a2a::Transport;
use awaken_run_ingress::{
    AnyDispatchStore, DispatchService, DispatchServiceConfig, DurableRunIngress, SystemClock,
};
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamCheckpointStore};
use awaken_runtime::{DirectRunIngress, RunIngress, Runtime};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::agent_resolver::AgentResolver;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::{ToolExecutor, ToolOutput};
use awaken_sandbox_local::{
    Environment, FileStore, InMemoryFileStore, LocalSandboxProvider, SandboxProvider,
};
use awaken_store_fs::{FsCommitCoordinator, FsStreamCheckpointStore};
use awaken_store_sqlite::SqliteCommitCoordinator;

use awaken_ext_compact::{CompactConfig, CompactPlugin, Summarizer};

use crate::agent_catalog::AgentCatalog;
use crate::background::BackgroundRuns;
use crate::compact::AgentSummarizer;
use crate::config::{build_runtime, server_config};
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
    std::env::temp_dir().join("awaken-server-local").join(name)
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

/// A tool a run parked on: its id, model-visible name/input, and whether it is
/// client-executed (the caller runs it and returns a result) or a built-in tool
/// awaiting a permission decision.
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// The neutral result of one step (a turn or a resume): the messages committed
/// during the step, the terminal phase, and the pending tool when the run parked.
pub struct TurnResult {
    pub new_messages: Vec<Message>,
    pub phase: Phase,
    pub pending: Option<PendingTool>,
}

/// The neutral resume command: answer a built-in tool's permission gate, or
/// deliver a client-executed tool's result.
pub enum HostResume {
    /// Built-in tool awaiting approval (Managed `user.tool_confirmation`; AI SDK
    /// `approval-responded` / `output-denied`).
    Confirm { allow: bool, note: Option<String> },
    /// Client-executed tool result (Managed `user.custom_tool_result`; AI SDK
    /// `output-available` / `output-error` on a client tool part).
    ClientResult { content: String, is_error: bool },
}

impl HostResume {
    fn wants_client(&self) -> bool {
        matches!(self, HostResume::ClientResult { .. })
    }
}

/// A host failure classified by fault: `BadRequest` is the caller's (bad id,
/// wrong binding, no park), `Internal` is the runtime's. Each adapter maps this
/// to its own public error shape.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    pub message: String,
    pub kind: HostErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorKind {
    Internal,
    BadRequest,
}

impl HostError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Internal,
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::BadRequest,
        }
    }
}

/// One evaluation round of a goal (neutral): the revision messages committed this
/// round, the round index, the classification token, and the grader explanation.
pub struct HostOutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The neutral outcome report: the ordered evaluation rounds.
pub struct HostOutcomeReport {
    pub iterations: Vec<HostOutcomeIteration>,
}

/// A thread's mutable position: the run awaiting a decision (if any) and the
/// system messages buffered for the next turn.
#[derive(Default)]
struct SessionState {
    parked: Option<RunId>,
    pending_system: Vec<String>,
    /// How many committed `Continuation` (outcome) rounds have already been
    /// projected, so a second `define_outcome` on the thread reports only its own.
    consumed_rounds: usize,
    /// Cursor for out-of-band memory extraction: the committed-message count that
    /// has already been handed to the extractor, so each turn extracts only the
    /// new messages instead of re-processing (and re-billing) the whole history.
    last_extracted_len: usize,
}

/// One thread's live state: an isolated runtime, its config, its commit
/// coordinator (the source of committed truth), its sandbox root, and its
/// position.
pub(crate) struct SessionCtx {
    pub(crate) runtime: Arc<Runtime>,
    /// The delivery seam a turn's execution goes through (slice C): `DirectRunIngress`
    /// by default; a `DurableRunIngress` when durable dispatch is enabled (slice D).
    /// Both drive the same `runtime`/`commit`; only the delivery guarantees differ.
    pub(crate) ingress: Arc<dyn RunIngress>,
    /// True when `ingress` is durable: a turn is submitted through the dispatch
    /// queue (`submit_background`) rather than executed inline (slice D).
    pub(crate) durable: bool,
    /// The concrete durable ingress, present iff `durable`. Kept alongside the
    /// boxed `ingress` so the ADR-0009 operational verbs (recover / reap /
    /// dead-letter GC / superseding submit — slice E) stay reachable; the boxed
    /// trait object erases them.
    pub(crate) durable_ingress: Option<Arc<DurableRunIngress<AnyDispatchStore>>>,
    /// The standing dispatch daemon for this session (ADR-0011), present when
    /// durable + `AWAKEN_DISPATCH_DAEMON=1`. Held here to keep the background task
    /// alive for the session's lifetime; it drains the shared queue autonomously.
    pub(crate) dispatch_service: Option<DispatchService<AnyDispatchStore>>,
    config: RunnableConfig,
    pub(crate) commit: Arc<HostCommit>,
    /// This thread's interrupted-stream checkpoint store (Phase 3), wired into
    /// every run context so an inference drop flushes durably at its boundary.
    pub(crate) stream_checkpoint: Arc<dyn StreamCheckpointStore>,
    /// The host's remote hand (ADR-0044), cloned from `SharedHost::remote_hand` at
    /// session creation. When set, every run context routes tool calls to it.
    pub(crate) remote_hand: Option<Arc<dyn ToolExecutor>>,
    pub(crate) thread_id: ThreadId,
    /// The thread's sandbox environment, reused to build a goal-enabled runtime
    /// for `define_outcome` (same tools, same environment).
    pub(crate) env: Arc<Environment>,
    /// The thread's skill registry (delivered + workspace), used to expand user
    /// `/skill-name` invocations. `None` when skills are not offered.
    skill_registry: Option<Arc<dyn SkillRegistry>>,
    /// The in-flight run's cancellation token, so a concurrent `interrupt` (a
    /// separate request) can cancel it. A plain `std::sync::Mutex` (brief locks),
    /// held by neither the run loop nor the state lock, so interrupt never blocks
    /// on the loop that holds `state`.
    cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// The in-flight run's live inbox plus the previous attempt's unconsumed
    /// leftovers. Same locking discipline as `cancel`; lifecycle and lookup
    /// live in [`crate::live_inbox`].
    pub(crate) live_inbox: std::sync::Mutex<crate::live_inbox::LiveInboxSlot>,
    state: tokio::sync::Mutex<SessionState>,
}

impl SessionCtx {
    /// A run context carrying a fresh cancellation token, registered on this ctx so
    /// a concurrent `interrupt` can cancel the run it drives. Only one run is in
    /// flight per thread at a time (the `state` lock serializes them), so the slot
    /// always holds the current run's token.
    pub(crate) fn context(&self) -> RuntimeRunContext {
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex poisoned") = Some(token.clone());
        let mut ctx = RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
            .with_stream_checkpoint(self.stream_checkpoint.clone())
            .with_cancellation(token);
        // ADR-0044: route this run's tool calls to the host's remote hand, if one
        // is wired; otherwise the kernel's in-process LocalToolExecutor runs them.
        if let Some(hand) = &self.remote_hand {
            ctx = ctx.with_tool_executor(hand.clone());
        }
        ctx
    }
}

/// Read a string field from an opaque round detail, defaulting to empty.
fn detail_str(detail: &serde_json::Value, key: &str) -> String {
    detail
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

use crate::judge::KernelJudgeRunner;
use crate::provisioning::StagedResources;

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
    pub(crate) provider: LocalSandboxProvider,
    grader: Arc<dyn Grader>,
    pub(crate) client_tools: HashSet<String>,
    /// Skills offered on every thread (ADR-0036). The whole set is fronted by the
    /// single `Skill` tool; the model activates one by id to load its instructions.
    pub(crate) skills: Vec<SkillSpec>,
    /// An optional durable delivered-skill catalog (resources plane). When set, its
    /// `SKILL.md`s are offered alongside the static `skills` and survive a restart, so
    /// a catalog configured through `/v1/skills` outlives the process. The host reads
    /// the bytes and feeds them to the extension's `SkillSource`, so the runtime stays
    /// store-unaware.
    pub(crate) skill_store: Option<Arc<awaken_skill_store::SkillStore>>,
    pub(crate) delegates: HashSet<String>,
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
    /// Delegate agents fulfilled over A2A (agent id → transport) instead of a local
    /// sub-run. `run_delegate` routes to these first.
    pub(crate) remote_agents: HashMap<String, Arc<dyn Transport>>,
    /// Out-of-band memory extraction, when enabled with [`with_memory`]. After a
    /// turn reaches a natural end it fires a background `memory-extractor` sub-run.
    memory: Option<Arc<MemoryExtraction>>,
    /// The relevance selector for recall (a `memory-selector` sub-agent), wired
    /// into the memory recall plugin when memory is enabled.
    memory_selector: Option<Arc<dyn awaken_ext_memory::RecallSelector>>,
    /// Context compaction, when enabled with [`with_compaction`]. The config is
    /// installed on the `compact` plugin (a `BeforeInference` hook); the summarizer
    /// is a `compactor` sub-agent the plugin calls to fold the older slice. The main
    /// agent runs a matching `KeepLast` window so those raw turns leave the model view.
    compact_config: Option<CompactConfig>,
    compact_summarizer: Option<Arc<dyn Summarizer>>,
    /// The config data plane, when the server exposes `/v1/config/*`. A thread's
    /// runtime config is the installed (published) config for its agent, if any,
    /// else the built-in default (slice A).
    pub(crate) config_service: Option<Arc<crate::config_plane::ConfigService>>,
    /// Per-thread MCP servers staged by a session's `prepare_session` (ADR-0043
    /// Phase 3), consumed when the thread's context is first built. Keyed by
    /// thread id; a thread with no entry connects to no MCP server.
    thread_mcp: std::sync::Mutex<HashMap<String, Vec<PreparedMcpServer>>>,
    /// Per-thread staged resource mounts + prompt fragments (ADR-0038), set by a
    /// session's `prepare_session` and consumed by `sandbox_spec` (mounts) and the
    /// run's system prompt (fragments). A thread with no entry mounts nothing.
    pub(crate) thread_resources: std::sync::Mutex<HashMap<String, StagedResources>>,
    /// Per-thread network-egress denial, set by a session's `prepare_session` from its
    /// environment's networking policy. A thread with no entry (or `false`) shares the
    /// host network; `true` runs its `bash` under a `bwrap --unshare-net` namespace
    /// with no egress. A shared handle, so a sandboxed ACP channel source can follow
    /// the same registrations (see [`crate::SandboxChannelSource`]).
    pub(crate) thread_egress: crate::sandbox_source::ThreadEgress,
    /// Content-addressed blob store backing the Files API, file-resource mounts, and
    /// collected artifacts. In-memory by default (one server process).
    pub(crate) file_store: Arc<dyn FileStore>,
    /// Mutable, id-keyed memory stores (ADR-0038 MemoryStore family): unlike the
    /// content-addressed `file_store`, a memory store keeps a stable id whose bytes a
    /// session mounts read-write and the host harvests back after a turn. Backed by
    /// the durable [`awaken_memory_store::MemoryBlobStore`] — under the storage dir it
    /// survives a restart, so memory written in one process is visible to the next; an
    /// ephemeral per-process dir when the host has no storage dir (unit tests).
    pub(crate) memory_stores: awaken_memory_store::MemoryBlobStore,
    /// An optional tool gate that replaces the default authorization gate on every
    /// thread's runtime. Used to exercise scheduled actions (ADR-0020, slice E): a
    /// gate that defers tool calls as `ScheduledAction`s so the durable dispatch
    /// worker performs them out of band.
    pub(crate) gate_override: Option<Arc<dyn awaken_runtime_contract::permission::ToolGateHook>>,
    /// When true (durable + `AWAKEN_DISPATCH_DAEMON=1`), each durable session runs
    /// a standing `DispatchService` daemon (ADR-0011): it drains the queue on a
    /// nudge/timer and reaps + relays autonomously, so background-submitted runs
    /// complete without a foreground request driving them (slice E follow-up).
    pub(crate) dispatch_daemon: bool,
    /// When set (ADR-0044), every run's tool calls are routed through this remote
    /// hand instead of the in-process registry. The host owns no placement policy:
    /// a caller connects a hand and injects the executor via [`with_remote_hand`].
    /// `None` is the in-process default (`LocalToolExecutor`), untouched.
    pub(crate) remote_hand: Option<Arc<dyn ToolExecutor>>,
}

impl SharedHost {
    /// A host over `llm`. Configure it with the chainable `with_*` builders
    /// (client tools, delegates, a judge grader, a durable store).
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        // Composition root: durability is picked from the environment.
        // `AWAKEN_STORAGE_DIR` set → durable SQLite commit store + a durable memory
        // blob store under it (both survive a restart); unset → ephemeral.
        let store_dir = std::env::var("AWAKEN_STORAGE_DIR")
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        // The ADR-0038 memory_store family persists under the storage dir when set so
        // a harvested write-back outlives the process; otherwise a per-process temp dir
        // (unit tests / ephemeral use) keeps it in-run only.
        let memory_store_root = match &store_dir {
            Some(dir) => dir.join("memory_stores"),
            None => std::env::temp_dir().join(format!("awaken-memstore-{}", std::process::id())),
        };
        let memory_stores = awaken_memory_store::MemoryBlobStore::open(&memory_store_root)
            .expect("open durable memory-store root");
        Self {
            llm,
            model_ref: model_ref.into(),
            model_route: crate::model_route::ThreadModelBinding::new(),
            acp: None,
            provider: LocalSandboxProvider::new(sub_base("")),
            grader: Arc::new(KeywordGrader),
            client_tools: HashSet::new(),
            skills: Vec::new(),
            skill_store: None,
            delegates: HashSet::new(),
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            hub: Arc::new(ThreadEventHub::new()),
            // `with_store_dir` still overrides this environment-derived default.
            store_dir,
            remote_agents: HashMap::new(),
            memory: None,
            memory_selector: None,
            compact_config: None,
            compact_summarizer: None,
            config_service: None,
            thread_mcp: std::sync::Mutex::new(HashMap::new()),
            thread_resources: std::sync::Mutex::new(HashMap::new()),
            thread_egress: crate::sandbox_source::ThreadEgress::new(),
            file_store: Arc::new(InMemoryFileStore::new()),
            memory_stores,
            gate_override: None,
            dispatch_daemon: std::env::var("AWAKEN_DISPATCH_DAEMON").is_ok_and(|v| v == "1"),
            remote_hand: None,
        }
    }

    /// Enable context compaction. Once a turn's conversation exceeds `threshold`
    /// messages, the `compact` plugin's `BeforeInference` hook summarizes everything
    /// but the last `keep_last` messages (through a `compactor` sub-agent) and injects
    /// the summary as request-only context; the main agent runs a matching `KeepLast`
    /// window so those older raw turns drop from the model view. Non-destructive:
    /// committed truth is never rewritten (G13). The bounds are also exposed as the
    /// `compact` config section, so a per-run `plugin_config` can override them.
    pub fn with_compaction(mut self, threshold: usize, keep_last: usize) -> Self {
        self.compact_config = Some(CompactConfig {
            threshold,
            keep_last,
        });
        self.compact_summarizer = Some(Arc::new(AgentSummarizer::new(
            self.llm.clone(),
            &self.model_ref,
        )));
        self
    }

    /// Enable out-of-band memory extraction, writing memories under `mem_dir`. After
    /// each turn that reaches a natural end, a background `memory-extractor` sub-agent
    /// reads the conversation and saves durable memories via `write_memory` (scoped
    /// to `mem_dir`), without blocking the turn. The extractor runs the default
    /// memory agent over this host's model; drain it before shutdown with
    /// [`drain_memory`](Self::drain_memory).
    pub fn with_memory(mut self, mem_dir: impl Into<PathBuf>) -> Self {
        let catalog = Arc::new(AgentCatalog::new().with_agent(default_memory_agent(
            &self.model_ref,
            DEFAULT_MEMORY_INSTRUCTIONS,
        )));
        let extraction = MemoryExtraction::new(
            self.llm.clone(),
            Arc::new(LocalSandboxProvider::new(sub_base("mem"))),
            catalog,
            Arc::new(BackgroundRuns::new()),
            mem_dir.into(),
        );
        self.memory = Some(Arc::new(extraction));
        // The recall plugin uses this selector once the store grows: a single-step
        // `memory-selector` sub-agent picks the memories relevant to the user's
        // message.
        self.memory_selector = Some(Arc::new(crate::memory::AgentSelector::new(
            self.llm.clone(),
            &self.model_ref,
        )));
        self
    }

    /// Await in-flight background memory extractions up to `timeout` (shutdown
    /// flush). Returns `true` if all finished. A no-op returning `true` when memory
    /// is disabled.
    pub async fn drain_memory(&self, timeout: std::time::Duration) -> bool {
        match &self.memory {
            Some(mem) => mem.drain(timeout).await,
            None => true,
        }
    }

    /// Replace the default authorization gate on every thread with `gate` (slice
    /// E): the scheduled-action server uses a gate that defers tool calls so the
    /// durable worker performs them (ADR-0020).
    pub fn with_gate_override(
        mut self,
        gate: Arc<dyn awaken_runtime_contract::permission::ToolGateHook>,
    ) -> Self {
        self.gate_override = Some(gate);
        self
    }

    /// Wire the config data plane, so a session's agent resolves to its installed
    /// (published) config (slice A).
    pub fn with_config_service(mut self, service: Arc<crate::config_plane::ConfigService>) -> Self {
        self.config_service = Some(service);
        self
    }

    /// Add client-executed tools: those ids are model-visible but unregistered, so
    /// a call parks and the client supplies the result.
    pub fn with_client_tools(mut self, client_tools: HashSet<String>) -> Self {
        self.client_tools.extend(client_tools);
        self
    }

    /// Add local delegate agents callable via `agent_run`.
    pub fn with_delegates(mut self, delegates: HashSet<String>) -> Self {
        self.delegates.extend(delegates);
        self
    }

    /// Activate the tool state machine on every thread with `config` (its
    /// `{"machines":[…]}` section). The plugin gates and advances tool calls per
    /// the declared transitions (ADR tool-state-machine).
    pub fn with_state_machine(mut self, config: serde_json::Value) -> Self {
        self.plugin_ids
            .push(awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.to_string());
        self.plugin_config.insert(
            awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.to_string(),
            config,
        );
        self
    }

    /// Offer skills on every thread (ADR-0036): they are fronted by the single
    /// `Skill` tool, whose catalog lists them and whose invocation returns the
    /// activated skill's instructions. The host stays out of skill
    /// authoring/collection — it only carries the offered set.
    pub fn with_skills(mut self, skills: Vec<SkillSpec>) -> Self {
        self.skills.extend(skills);
        self
    }

    /// Back the delivered skill catalog with a durable [`awaken_skill_store::SkillStore`]
    /// rooted at `dir`. Its `SKILL.md`s are offered on every thread alongside any
    /// static [`with_skills`](Self::with_skills) set and survive a restart, so a skill
    /// added through `/v1/skills` is still offered by a later process over the same
    /// dir. The extension never learns of the store — the host scans it into the
    /// `SkillSource` port as plain file data.
    pub fn with_skill_store(mut self, dir: impl Into<PathBuf>) -> Self {
        let store = awaken_skill_store::SkillStore::open(dir.into())
            .expect("open durable skill store root");
        self.skill_store = Some(Arc::new(store));
        self
    }

    /// Grade outcomes with a real judge sub-agent (`judge_agent_id`) run through the
    /// kernel, instead of the deterministic keyword grader. The judge grades in its
    /// own fresh context.
    pub fn with_judge(mut self, judge_agent_id: impl Into<String>) -> Self {
        let id = judge_agent_id.into();
        let catalog = Arc::new(AgentCatalog::new().with_agent(default_judge_agent(
            &self.model_ref,
            &id,
            DEFAULT_JUDGE_INSTRUCTIONS,
        )));
        let runner = Arc::new(KernelJudgeRunner {
            llm: self.llm.clone(),
            provider: LocalSandboxProvider::new(sub_base("judge")),
            catalog,
            seq: AtomicU64::new(0),
        });
        self.grader = Arc::new(DelegateGrader::new(runner, id));
        self
    }

    /// Persist every thread's committed truth to a durable SQLite database under
    /// `dir` (one file per thread). A run parked on a thread survives a restart:
    /// a host rebuilt over the same directory recovers the parked position and can
    /// resume it. Without this, sessions are in-memory and lost on restart.
    pub fn with_store_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.store_dir = Some(dir.into());
        self
    }

    /// Install the model→executor resolver (R1). Without one, every thread uses `llm`.
    pub fn with_executor_provider(
        mut self,
        provider: Arc<dyn crate::model_route::ExecutorProvider>,
    ) -> Self {
        self.model_route.set_provider(provider);
        self
    }

    /// Bind `model_ref` to `thread` (R2/R5), staged before its first turn.
    pub fn register_thread_model(&self, thread: &str, model_ref: impl Into<String>) {
        self.model_route.register(thread, model_ref);
    }

    /// Deny network egress for `thread`'s sandbox (from its environment's networking
    /// policy), staged before its first turn and consumed by `sandbox_spec` (and by a
    /// sandboxed ACP channel source wired via [`SharedHost::thread_egress`]).
    pub fn register_thread_egress(&self, thread: &str, deny: bool) {
        self.thread_egress.set(thread, deny);
    }

    /// The shared per-thread egress-registration handle, for wiring a
    /// [`crate::SandboxChannelSource`] before `with_acp` consumes the builder.
    pub fn thread_egress(&self) -> crate::sandbox_source::ThreadEgress {
        self.thread_egress.clone()
    }

    /// Stage MCP servers for `thread`, to be connected when the thread's context
    /// is first built (its first turn) — the Managed session-create path calls
    /// this from `prepare_session`, so the credential is materialized before the
    /// session exists but the network connect happens lazily (ADR-0043 Phase 3).
    /// Re-registering replaces the thread's staged set.
    pub fn register_thread_mcp(&self, thread: &str, servers: Vec<PreparedMcpServer>) {
        self.thread_mcp
            .lock()
            .expect("thread mcp mutex poisoned")
            .insert(thread.to_string(), servers);
    }

    /// The model id echoed by adapters in their session/agent objects.
    pub fn model(&self) -> String {
        self.model_ref.clone()
    }

    /// The set of client-executed tool ids (model-visible, host-unregistered).
    pub fn client_tools(&self) -> &HashSet<String> {
        &self.client_tools
    }

    /// The shared per-thread live observation hub.
    pub fn hub(&self) -> &Arc<ThreadEventHub> {
        &self.hub
    }

    /// Register a delegate agent fulfilled over A2A: `agent_run` calls naming it
    /// are routed to `transport` (a remote agent). The id joins the advertised
    /// roster so the model can delegate to it.
    /// Route every run's tool calls through `hand` — a remote `ToolExecutor`
    /// (ADR-0044) — instead of the in-process registry. The brain still commits
    /// the hand's returned output. `None` (the default) keeps in-process execution.
    pub fn with_remote_hand(mut self, hand: Arc<dyn ToolExecutor>) -> Self {
        self.remote_hand = Some(hand);
        self
    }

    pub fn with_remote_a2a(
        mut self,
        agent_id: impl Into<String>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        let agent_id = agent_id.into();
        self.delegates.insert(agent_id.clone());
        self.remote_agents.insert(agent_id, transport);
        self
    }

    /// Build the delegation resolver from the configured roster and remotes, or
    /// `None` when the host has no delegates. Injected into each thread's runtime.
    fn agent_resolver(&self) -> Option<Arc<dyn AgentResolver>> {
        if self.delegates.is_empty() {
            return None;
        }
        // Native delegates are the roster ids that are not remotes.
        let native: HashSet<String> = self
            .delegates
            .iter()
            .filter(|id| !self.remote_agents.contains_key(*id))
            .cloned()
            .collect();
        Some(Arc::new(DelegationResolver::new(
            self.llm.clone(),
            self.model_ref.clone(),
            LocalSandboxProvider::new(sub_base("deleg")),
            native,
            self.remote_agents.clone(),
        )))
    }

    /// Build a thread's commit boundary under the configured store directory: a
    /// durable SQLite database (default) or the filesystem append-log backend when
    /// `AWAKEN_STORE=fs`, or an in-memory coordinator when no store dir is set.
    async fn build_commit(&self, thread: &str) -> Result<HostCommit, HostError> {
        let Some(dir) = &self.store_dir else {
            return Ok(HostCommit::Memory(MemoryCommitCoordinator::new()));
        };
        std::fs::create_dir_all(dir).map_err(|e| HostError::internal(e.to_string()))?;
        let fs_backend = std::env::var("AWAKEN_STORE").is_ok_and(|value| value == "fs");
        if fs_backend {
            let thread_dir = dir.join(sanitize_thread(thread));
            let fs = FsCommitCoordinator::open(&thread_dir)
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
            Ok(HostCommit::Fs(fs))
        } else {
            let path = dir.join(format!("{}.db", sanitize_thread(thread)));
            let sqlite = SqliteCommitCoordinator::open(&path.to_string_lossy())
                .map_err(|e| HostError::internal(e.to_string()))?;
            Ok(HostCommit::Sqlite(sqlite))
        }
    }

    /// Build a thread's interrupted-stream checkpoint store, mirroring
    /// `build_commit`'s durability choice: a filesystem store under the configured
    /// directory (so a partial survives a process crash and resumes), or an
    /// in-memory store when no store dir is set. Always filesystem when durable —
    /// the checkpoint is a small `run_id`-keyed blob, so it needs no SQLite/fs
    /// backend axis; it simply follows the commit boundary's durability.
    fn build_stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, HostError> {
        let Some(dir) = &self.store_dir else {
            return Ok(Arc::new(MemoryStreamCheckpointStore::new()));
        };
        let checkpoint_dir = dir.join(sanitize_thread(thread)).join("stream-checkpoints");
        let store = FsStreamCheckpointStore::open(&checkpoint_dir)
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(Arc::new(store))
    }

    /// Build a thread's run-delivery ingress. Default is direct in-process
    /// execution (`DirectRunIngress`, slice C). With `AWAKEN_INGRESS=durable` the
    /// turn is delivered through a `DurableRunIngress`: every accepted run is
    /// persisted to a dispatch queue before it executes (so it survives a crash),
    /// and on session (re)build any dispatch a prior process crashed on is
    /// recovered (slice D). The durable ingress shares this thread's `runtime` and
    /// `commit`, so execution and committed truth are identical to the direct path
    /// (G6) — only the delivery guarantee differs. Returns the ingress plus the
    /// flag that tells `run_turn` to submit through the durable (queued) path.
    async fn build_ingress(
        &self,
        thread: &str,
        runtime: Arc<Runtime>,
        commit: Arc<HostCommit>,
        stream_checkpoint: Arc<dyn StreamCheckpointStore>,
    ) -> Result<
        (
            Arc<dyn RunIngress>,
            Option<Arc<DurableRunIngress<AnyDispatchStore>>>,
        ),
        HostError,
    > {
        let durable = std::env::var("AWAKEN_INGRESS").is_ok_and(|value| value == "durable");
        if !durable {
            return Ok((Arc::new(DirectRunIngress::new(runtime)), None));
        }
        // The durable dispatch backend (SQLite per-thread, or the shared Postgres
        // pool for a multi-node fleet) plus this process's unique claim owner — both
        // live in `dispatch_backend`, which owns backend selection (ADR-0019/0024).
        let store = crate::dispatch_backend::open_durable_store(self.store_dir.as_deref(), thread)?;
        // The recovered dispatch a crash left mid-flight is re-executed by this
        // worker; giving it the same checkpoint store lets that re-execution resume
        // the interrupted step from its flushed partial (Phase 3 cross-process).
        let ingress = Arc::new(DurableRunIngress::with_owner(
            runtime,
            store,
            commit,
            crate::dispatch_backend::dispatch_owner(),
            Some(stream_checkpoint),
        ));
        // Startup recovery: reclaim any dispatch a prior process crashed on
        // mid-flight, driving it to completion against committed truth.
        ingress
            .recover(now_ms())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        let boxed: Arc<dyn RunIngress> = ingress.clone();
        Ok((boxed, Some(ingress)))
    }

    pub(crate) async fn ctx_for(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<Arc<SessionCtx>, HostError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(ctx) = sessions.get(thread) {
            return Ok(ctx.clone());
        }
        let env = Arc::new(
            self.provider
                .create(&self.sandbox_spec(thread))
                .await
                .map_err(|e| HostError::internal(e.to_string()))?,
        );
        // Clone any staged github_repository resources into the fresh environment,
        // host-side (ADR-0038); fail-closed so a bad repo aborts session start.
        self.provision_thread_repos(thread, &env)?;
        let thread_id = ThreadId(thread.to_string());
        let commit = Arc::new(self.build_commit(thread).await?);
        // Durable interrupted-stream checkpoints follow the commit's durability
        // (Phase 3): a mid-recovery crash resumes from the flushed partial.
        let stream_checkpoint = self.build_stream_checkpoint(thread)?;
        // This thread's staged MCP servers (ADR-0043 Phase 3), registered by the
        // managed adapter's `prepare_session` before the first turn; the wire
        // composition (connect + discover, fail closed) lives in `crate::mcp`.
        // Read, not removed, so a retry re-attempts (and re-fails) the connect.
        let staged_mcp: Vec<PreparedMcpServer> = self
            .thread_mcp
            .lock()
            .expect("thread mcp mutex poisoned")
            .get(thread)
            .cloned()
            .unwrap_or_default();
        let mcp = crate::mcp::connect_staged(&staged_mcp).await?;
        // MCP tools are pre-authorized on this thread's gate: the session creator
        // explicitly configured the server (with its credential), which is the
        // authorization decision — the ask-gate keeps covering the built-in
        // mutation tools. `server_gate_allowing(&[])` is the plain server gate,
        // so threads without MCP keep the exact default policy.
        let base_gate = crate::config::server_gate_allowing(&mcp.tool_ids);
        // R1/R2: per-session executor resolved from the thread's bound model.
        let exec = self
            .model_route
            .resolve_executor(thread, &self.model_ref, &self.llm);
        let mut runtime = build_runtime(exec, &env);
        if !mcp.tool_ids.is_empty() {
            runtime = runtime.with_gate(base_gate.clone());
        }
        // Register the discovered MCP tools; their descriptors join the advertised
        // config below so the model sees them.
        for tool in mcp.tools {
            runtime = runtime.with_tool(tool);
        }
        let mcp_descriptors = mcp.descriptors;
        // A gate override (slice E) replaces the default authorization gate — e.g.
        // a scheduling gate that defers tool calls as `ScheduledAction`s so the
        // durable worker performs them out of band (ADR-0020).
        if let Some(gate) = &self.gate_override {
            runtime = runtime.with_gate(gate.clone());
        }
        // Delegation is a runtime concern: inject the resolver so the kernel runs
        // `agent_run` as a sub-agent (native or remote), not the tool registry.
        if let Some(resolver) = self.agent_resolver() {
            runtime = runtime.with_resolver(resolver);
        }
        // Skills are fronted by two stable tools (ADR-0036); all skill behavior is
        // in `awaken-ext-skills`. The host only wires the pieces it alone owns —
        // the sandbox env, the sub-run capability, and the base gate — via
        // `skills::wire_skills`.
        let mut skill_descriptors = Vec::new();
        let mut skill_registry: Option<Arc<dyn SkillRegistry>> = None;
        if let Some(wiring) = crate::skills::wire_skills(
            &self.skills,
            self.skill_store.clone(),
            env.clone(),
            self.llm.clone(),
            &self.model_ref,
            thread,
            // The MCP-aware base gate, so a skill-wrapped gate keeps the thread's
            // pre-authorized MCP tools (identical to `server_gate()` without MCP).
            base_gate.clone(),
            sub_base("skill-fork"),
        ) {
            runtime = runtime
                .with_gate(wiring.gate)
                .with_tool(wiring.list_tool)
                .with_tool(wiring.activate_tool);
            skill_descriptors = wiring.descriptors;
            skill_registry = Some(wiring.registry);
        }
        // Memory recall is a plugin: it contributes a BeforeInference hook that
        // injects bounded recall as request-only context (never committed). Install
        // it and list its id so it is active for the run (G30).
        // Seed with host-registered plugins (e.g. the tool state machine via
        // `with_state_machine`), then append the per-run memory/compact plugins.
        let mut plugin_ids: Vec<String> = self.plugin_ids.clone();
        if let Some(mem) = &self.memory {
            let mut plugin = awaken_ext_memory::MemoryPlugin::new(mem.store(), mem.bounds());
            if let Some(selector) = &self.memory_selector {
                plugin = plugin.with_selector(selector.clone());
            }
            runtime = runtime.with_plugin(Arc::new(plugin));
            plugin_ids.push(awaken_ext_memory::MEMORY_PLUGIN_ID.to_string());
        }
        // Compaction is a plugin too: a BeforeInference hook that folds the older
        // slice into a summary and injects it request-only. The main agent runs a
        // rolling window matching the config's `keep_last`, so summarized older turns
        // leave the model view.
        let context_policy = match (&self.compact_config, &self.compact_summarizer) {
            (Some(config), Some(summarizer)) => {
                let keep_last = config.keep_last;
                let plugin = CompactPlugin::new(config.clone()).with_summarizer(summarizer.clone());
                runtime = runtime.with_plugin(Arc::new(plugin));
                plugin_ids.push(awaken_ext_compact::COMPACT_PLUGIN_ID.to_string());
                awaken_runtime_contract::resolved::ContextPolicy::KeepLast { keep_last }
            }
            _ => awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        };
        // A published agent runs with its own installed config (slice A); an
        // unknown/unpublished agent falls back to the server's built-in default,
        // carrying the run's dynamic plugin list and context policy.
        let installed = self
            .config_service
            .as_ref()
            .zip(agent)
            .and_then(|(svc, agent)| svc.installed(agent));
        // Everything dynamically provisioned on this thread that the config must
        // advertise: the skill tools plus the discovered MCP tools.
        let mut dynamic_descriptors = skill_descriptors;
        dynamic_descriptors.extend(mcp_descriptors);
        let config = installed.unwrap_or_else(|| {
            server_config(
                &self.model_route.model_ref(thread, &self.model_ref),
                &self.client_tools,
                &self.delegates,
                &plugin_ids,
                &self.plugin_config,
                &dynamic_descriptors,
                context_policy,
            )
        });
        // Recover the session's position from committed truth: a durable store may
        // already hold this thread's history and a parked run (e.g. after a
        // restart). `consumed_rounds` starts past any prior outcome rounds so a new
        // `define_outcome` reports only the rounds it produces.
        let mut state = SessionState {
            consumed_rounds: commit.continuation_payloads(&thread_id).len(),
            ..SessionState::default()
        };
        if let Some((run_id, _)) = commit.open_wait_for_thread(&thread_id) {
            // Prime the fresh runtime so the parked run's snapshot resolves on
            // resume — `start_turn` would normally have installed it.
            runtime
                .install_for_resume(&config)
                .map_err(|e| HostError::internal(e.to_string()))?;
            state.parked = Some(run_id);
        }
        let runtime = Arc::new(runtime);
        // The foreground delivery seam (slice C/D): a turn's execution goes through
        // `RunIngress` rather than calling `runtime.start_turn` directly. Direct
        // ingress runs inline on the same `runtime`; durable ingress queues the run
        // through a dispatch store first. Both share this thread's `runtime`/`commit`.
        let (ingress, durable_ingress) = self
            .build_ingress(
                thread,
                runtime.clone(),
                commit.clone(),
                stream_checkpoint.clone(),
            )
            .await?;
        let durable = durable_ingress.is_some();
        // Spawn the standing dispatch daemon for this session when enabled: it
        // drains the shared queue on a nudge/timer and reaps + relays autonomously
        // (ADR-0011). Runs on the same worker/store as the ingress.
        let dispatch_service = if self.dispatch_daemon {
            durable_ingress.as_ref().map(|ing| {
                // Enable the lease-renewal heartbeat (ADR-0024): a long run must not
                // be reclaimed by a peer while this daemon is still executing it.
                let config = DispatchServiceConfig {
                    lease_renewal_interval: Some(crate::dispatch_backend::LEASE_RENEWAL),
                    ..DispatchServiceConfig::default()
                };
                ing.spawn_service(Arc::new(SystemClock), config)
            })
        } else {
            None
        };
        let ctx = Arc::new(SessionCtx {
            runtime,
            ingress,
            durable,
            durable_ingress,
            dispatch_service,
            config,
            commit,
            stream_checkpoint,
            remote_hand: self.remote_hand.clone(),
            thread_id,
            env,
            skill_registry,
            cancel: std::sync::Mutex::new(None),
            live_inbox: std::sync::Mutex::new(crate::live_inbox::LiveInboxSlot::default()),
            state: tokio::sync::Mutex::new(state),
        });
        sessions.insert(thread.to_string(), ctx.clone());
        // Deliver the session's staged resource prompts (ADR-0038 A3a) as system
        // context on the first turn, so the model knows what it has mounted and where.
        let prompts = self.thread_resource_prompts(thread);
        if !prompts.is_empty() {
            let mut st = ctx.state.lock().await;
            for prompt in prompts {
                st.pending_system.push(prompt);
            }
        }
        Ok(ctx)
    }

    /// All messages committed on `thread` so far (the source of history). Empty
    /// when the thread has not run yet. Resolves through `ctx_for`, so a durable
    /// thread is hydrated from its store on demand — a fresh process reads a
    /// parked thread's committed transcript even before any session touches it
    /// (ADR-0039), enabling post-restart session rehydration.
    /// True when the durable store already holds `thread` — WITHOUT building a
    /// session context (the layout probe lives with the commit boundary in
    /// [`crate::store`]).
    pub fn has_durable_thread(&self, thread: &str) -> bool {
        crate::store::durable_thread_exists(self.store_dir.as_deref(), thread)
    }

    pub async fn committed_messages(&self, thread: &str) -> Vec<Message> {
        match self.ctx_for(thread, None).await {
            Ok(ctx) => ctx.commit.committed_messages(&ctx.thread_id),
            Err(_) => Vec::new(),
        }
    }

    /// A thread's accumulated token usage, attributed per model (the run loop records
    /// it as committed thread state under [`THREAD_USAGE_STATE_KEY`]; each write is the
    /// running cumulative, so the last `Set` is the whole tally). Empty for a thread
    /// that has never run a real turn or whose provider reported no usage (the
    /// deterministic models). Callers use `.total()` for the session-level sum.
    pub async fn thread_usage(&self, thread: &str) -> awaken_runtime_contract::llm::ThreadUsage {
        use awaken_agent_contract::agent::state::{Action, Scope};
        use awaken_runtime_contract::llm::{THREAD_USAGE_STATE_KEY, ThreadUsage};
        let Ok(ctx) = self.ctx_for(thread, None).await else {
            return ThreadUsage::default();
        };
        let mut usage = ThreadUsage::default();
        for cmd in ctx.commit.committed_state(&ctx.thread_id) {
            if cmd.scope == Scope::Thread && cmd.key.0 == THREAD_USAGE_STATE_KEY {
                if let Action::Set(value) = &cmd.action {
                    if let Ok(parsed) = serde_json::from_value::<ThreadUsage>(value.clone()) {
                        usage = parsed;
                    }
                }
            }
        }
        usage
    }

    /// True when `thread` has a run parked awaiting a decision.
    pub async fn is_parked(&self, thread: &str) -> bool {
        let ctx = match self.ctx_for(thread, None).await {
            Ok(ctx) => ctx,
            Err(_) => return false,
        };
        ctx.state.lock().await.parked.is_some()
    }

    /// The tool a parked run on `thread` is waiting on, if any.
    pub async fn pending_tool(&self, thread: &str) -> Option<PendingTool> {
        let ctx = self.ctx_for(thread, None).await.ok()?;
        let st = ctx.state.lock().await;
        let run_id = st.parked.clone()?;
        pending_from_ticket(&ctx.commit.waiting_ticket(&run_id)?, &self.client_tools)
    }

    /// Buffer a system message; it is prepended to the next turn's input.
    pub async fn add_system(&self, thread: &str, text: &str) -> Result<(), HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        ctx.state.lock().await.pending_system.push(text.to_string());
        Ok(())
    }

    /// Interrupt the run in flight on `thread`, if any: cancel its token so the
    /// runtime observes it at the next step boundary and ends the run `Cancelled`
    /// (an outcome loop then reports `interrupted`). A no-op when nothing is
    /// running. Never blocks on the run's own state lock — it only touches the
    /// separate cancel slot — so it works from a concurrent request.
    pub async fn interrupt(&self, thread: &str) -> Result<(), HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        if let Some(token) = ctx.cancel.lock().expect("cancel mutex poisoned").as_ref() {
            token.cancel();
        }
        Ok(())
    }

    /// Run one turn on `thread`: buffered system messages first, then `input`.
    /// Runs to the first pause (a parked tool) or the natural end.
    #[tracing::instrument(
        name = "host.run_turn",
        skip_all,
        fields(awaken.thread.id = %thread)
    )]
    pub async fn run_turn(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<TurnResult, HostError> {
        self.deliver_turn(agent, thread, input, false).await
    }

    /// Submit a turn that *supersedes* the thread's prior pending/parked work
    /// (ADR-0022, slice E): the newest submission wins, stale dispatches are marked
    /// superseded and never claimed again, then the new run is driven. Requires
    /// durable ingress. Unlike `run_turn` it does not fail closed on a parked
    /// thread — superseding a parked run is the point.
    pub(crate) async fn supersede_turn(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<TurnResult, HostError> {
        self.deliver_turn(agent, thread, input, true).await
    }

    async fn deliver_turn(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
        supersede: bool,
    ) -> Result<TurnResult, HostError> {
        let ctx = self.ctx_for(thread, agent).await?;
        let mut st = ctx.state.lock().await;
        if st.parked.is_some() && !supersede {
            return Err(HostError::bad_request("thread is awaiting a tool decision"));
        }
        if supersede && ctx.durable_ingress.is_none() {
            return Err(HostError::bad_request(
                "supersede requires durable ingress (set AWAKEN_INGRESS=durable)",
            ));
        }
        // Recall is injected by the memory plugin's BeforeInference hook (request-only,
        // never committed), so the host does not touch it here.
        let mut messages: Vec<Message> = Vec::new();
        messages.extend(
            std::mem::take(&mut st.pending_system)
                .into_iter()
                .map(|text| {
                    Message::text(
                        MessageId(format!("sys-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst))),
                        Role::System,
                        text,
                    )
                }),
        );
        // Expand a user `/skill-name` into the skill's instructions before the turn.
        let input = match &ctx.skill_registry {
            Some(registry) => {
                awaken_ext_skills::expand_slash_commands(registry.as_ref(), thread, input)
            }
            None => input,
        };
        messages.extend(input);
        let before = ctx.commit.committed_messages(&ctx.thread_id).len();
        // Prepare the activation (install catalog + register snapshot + mint ids),
        // then deliver it through the ingress seam. Direct ingress executes inline,
        // so this is behavior-identical to the former `start_turn` call.
        let (mut run_id, mut activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages)
            .map_err(|e| HostError::internal(e.to_string()))?;
        if ctx.durable {
            // The durable path needs a run id that is unique across a restart: the
            // runtime's in-process id counter resets to 1 on restart and would
            // collide with an already-committed terminal run, which the dispatch
            // worker's terminal-run guard then skips (never re-running a finished
            // run) — silently dropping the turn. A wall-clock + sequence id cannot
            // collide with a prior process's ids.
            let uid = RunId(format!(
                "run-{}-{}",
                now_ms(),
                BASE_SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            activation.run_id = uid.clone();
            run_id = uid;
        }
        // Durable ingress queues the run through the dispatch store and drives it
        // via the worker (`submit_background`); a superseding submit first marks the
        // thread's stale pending/parked dispatches superseded (ADR-0022); direct
        // ingress runs it inline (`submit`). All drive to the same terminal/parked
        // phase and commit through the same boundary, so `finish_step` is identical.
        // R3/R4: route to the ACP executor for acp:* threads, else the native
        // ingress (direct / durable / superseding). See `crate::turn_exec`.
        let phase = self
            .execute_activation(&ctx, thread, activation, supersede)
            .await?;
        let result = self.finish_step(&ctx, &mut st, run_id, phase, before, thread);
        drop(st);
        self.run_aux_after_step(&ctx, thread, &result.phase).await;
        Ok(result)
    }

    /// Fire the out-of-band auxiliary agents (memory extraction) after a step reaches
    /// a terminal phase. Shared by `run_turn` and `resume`, so a turn that ended via a
    /// tool/delegation resume gets the same treatment as one that ended directly.
    /// No-op while the run is still parked. (Compaction is not out-of-band: it runs
    /// inline as the `compact` plugin's `BeforeInference` hook.)
    async fn run_aux_after_step(&self, ctx: &Arc<SessionCtx>, thread: &str, phase: &Phase) {
        self.maybe_extract_memory(ctx, thread, phase).await;
    }

    /// Fire out-of-band memory extraction when a turn reaches a terminal phase
    /// (not parked) and memory is enabled. Seeds the extractor with only the
    /// messages committed since the last extraction (a per-thread cursor), so a
    /// long conversation is not re-processed every turn. Fire-and-forget (drained
    /// at shutdown). The cursor advances optimistically on trigger.
    async fn maybe_extract_memory(&self, ctx: &SessionCtx, thread: &str, phase: &Phase) {
        if matches!(phase, Phase::Waiting) {
            return;
        }
        let Some(mem) = &self.memory else {
            return;
        };
        let committed = ctx.commit.committed_messages(&ctx.thread_id);
        let mut st = ctx.state.lock().await;
        let cursor = st.last_extracted_len.min(committed.len());
        if committed.len() <= cursor {
            return; // no new messages since the last extraction
        }
        let delta = committed[cursor..].to_vec();
        st.last_extracted_len = committed.len();
        drop(st);
        mem.trigger(thread, delta).await;
    }

    /// The durable ingress for `thread`, building the session if needed. Errors
    /// unless the server runs in durable mode (`AWAKEN_INGRESS=durable`). This is
    /// the operational entry for the ADR-0009 follow-on verbs (slice E): recover
    /// (ADR-0011), reap / dead-letter GC (ADR-0015), and superseding submit
    /// (ADR-0022).
    pub(crate) async fn durable_ingress(
        &self,
        thread: &str,
    ) -> Result<Arc<DurableRunIngress<AnyDispatchStore>>, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        ctx.durable_ingress.clone().ok_or_else(|| {
            HostError::bad_request("durable ingress not enabled (set AWAKEN_INGRESS=durable)")
        })
    }

    /// Reconcile `thread`'s dispatch queue (ADR-0011, slice E): reclaim and re-run
    /// any dispatch left runnable by a crash. Returns the recovered run ids.
    pub(crate) async fn reconcile(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let processed = self
            .durable_ingress(thread)
            .await?
            .recover(now_ms())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(processed.into_iter().map(|(id, _)| id.0).collect())
    }

    /// Reap crashed dispatches on `thread` that have exhausted `max_attempts`
    /// crash-recoveries as of `now_ms` (ADR-0015, slice E). Returns how many were
    /// dead-lettered. `now_ms` is an as-of cutoff so an operator (or a test) can
    /// reap against a chosen clock.
    pub(crate) async fn reap(
        &self,
        thread: &str,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, HostError> {
        self.durable_ingress(thread)
            .await?
            .reap(max_attempts, now_ms)
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }

    /// The run ids currently dead-lettered on `thread` (ADR-0015, slice E).
    pub(crate) async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let ids = self
            .durable_ingress(thread)
            .await?
            .dead_letters()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(ids.into_iter().map(|id| id.0).collect())
    }

    /// Operator GC: purge every dead-lettered dispatch on `thread` (ADR-0015,
    /// slice E). Returns how many were removed.
    pub(crate) async fn purge_dead_letters(&self, thread: &str) -> Result<usize, HostError> {
        self.durable_ingress(thread)
            .await?
            .purge_dead_letters()
            .await
            .map_err(|e| HostError::internal(e.to_string()))
    }

    /// An operational snapshot of `thread`'s dispatch queue (ADR-0025): every row
    /// in enqueue order with its status and attempt count — the monitoring surface.
    pub(crate) async fn list_dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<(String, String, u64)>, HostError> {
        let rows = self
            .durable_ingress(thread)
            .await?
            .list_dispatches()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|d| (d.run_id.0, format!("{:?}", d.status), d.attempt_count))
            .collect())
    }

    /// The run ids superseded by a newer submission on `thread` (ADR-0022,
    /// slice E).
    pub(crate) async fn superseded(&self, thread: &str) -> Result<Vec<String>, HostError> {
        let ids = self
            .durable_ingress(thread)
            .await?
            .superseded()
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(ids.into_iter().map(|id| id.0).collect())
    }

    /// Enqueue a run for the standing dispatch daemon to drive autonomously
    /// (ADR-0011, slice E follow-up): prepare the activation and hand it to the
    /// daemon via `DispatchService::submit` (durable enqueue + wake), returning
    /// immediately with the run id. The daemon drains it out of band — no
    /// foreground request drives it — so the caller observes completion by polling
    /// committed truth. Requires `AWAKEN_DISPATCH_DAEMON=1`.
    pub(crate) async fn submit_background_async(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<String, HostError> {
        let ctx = self.ctx_for(thread, agent).await?;
        let service = ctx.dispatch_service.as_ref().ok_or_else(|| {
            HostError::bad_request("dispatch daemon not enabled (set AWAKEN_DISPATCH_DAEMON=1)")
        })?;
        let mut messages: Vec<Message> = {
            let mut st = ctx.state.lock().await;
            std::mem::take(&mut st.pending_system)
                .into_iter()
                .map(|text| {
                    Message::text(
                        MessageId(format!("sys-{}", BASE_SEQ.fetch_add(1, Ordering::SeqCst))),
                        Role::System,
                        text,
                    )
                })
                .collect()
        };
        messages.extend(input);
        let (_run_id, mut activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages)
            .map_err(|e| HostError::internal(e.to_string()))?;
        // Restart-unique run id, same rationale as the foreground durable path.
        let uid = RunId(format!(
            "run-{}-{}",
            now_ms(),
            BASE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        activation.run_id = uid.clone();
        service
            .submit(activation)
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        Ok(uid.0)
    }

    /// Resume the run parked on `thread`, answering `tool_use_id` with `resume`.
    /// Fails closed unless `tool_use_id` names the pending tool and its binding
    /// (built-in vs client-executed) matches the resume variant.
    pub async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: HostResume,
    ) -> Result<TurnResult, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        let mut st = ctx.state.lock().await;
        let run_id = st
            .parked
            .clone()
            .ok_or_else(|| HostError::bad_request("no parked run to resume"))?;
        let ticket = ctx
            .commit
            .waiting_ticket(&run_id)
            .ok_or_else(|| HostError::internal("parked run has no waiting ticket"))?;

        // A parked delegation resumes through the kernel resolver with the user's
        // input; the kernel routes it (not the tool registry) and the run continues
        // or re-parks.
        if ticket.reason == WaitingReason::Delegation {
            if ticket.call_id.as_deref() != Some(tool_use_id) {
                return Err(HostError::bad_request(format!(
                    "tool_use_id {tool_use_id:?} does not match the pending delegate"
                )));
            }
            let input = match resume {
                HostResume::ClientResult { content, .. } => content,
                HostResume::Confirm { note, .. } => note.unwrap_or_default(),
            };
            let before = ctx.commit.committed_messages(&ctx.thread_id).len();
            let command = ResumeCommand::from_ticket(&ticket, ResumeResult::Input(input), 0);
            let phase = ctx
                .runtime
                .resume(command, &*ctx.commit, ctx.context())
                .await
                .map_err(|e| HostError::internal(e.to_string()))?;
            let result = self.finish_step(&ctx, &mut st, run_id, phase, before, thread);
            drop(st);
            self.run_aux_after_step(&ctx, thread, &result.phase).await;
            return Ok(result);
        }

        self.check_pending(&ticket, tool_use_id, resume.wants_client())?;
        let result = match resume {
            HostResume::Confirm { allow, note } => {
                if allow {
                    ResumeResult::allow()
                } else {
                    ResumeResult::deny(note)
                }
            }
            HostResume::ClientResult { content, is_error } => {
                let call_id = ticket.call_id.clone().unwrap_or_default();
                let output = if is_error {
                    ToolOutput::error(call_id, content)
                } else {
                    ToolOutput::ok(call_id, content)
                };
                ResumeResult::ToolResult(output)
            }
        };
        let before = ctx.commit.committed_messages(&ctx.thread_id).len();
        let command = ResumeCommand::from_ticket(&ticket, result, 0);
        let phase = ctx
            .runtime
            .resume(command, &*ctx.commit, ctx.context())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
        let result = self.finish_step(&ctx, &mut st, run_id, phase, before, thread);
        drop(st);
        self.run_aux_after_step(&ctx, thread, &result.phase).await;
        Ok(result)
    }

    /// Define an outcome and drive the grade->revise loop over `thread`, bounded
    /// by `max_iterations`. Revision rounds auto-approve tools (the goal loop
    /// drives to a deliverable).
    pub async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<HostOutcomeReport, HostError> {
        let ctx = self.ctx_for(thread, None).await?;
        let mut st = ctx.state.lock().await;
        let goal = GoalSpec::new(description, rubric, max_iterations);

        // The runtime owns the grade->revise loop: a goal-enabled runtime whose
        // run-end guard steers revisions until the goal is met or the budget is
        // spent. The host drives one run and projects the rounds it committed. The
        // guard shares the thread's committed history and sandbox root.
        let goal_runtime = build_runtime(self.llm.clone(), &ctx.env)
            .with_plugin(Arc::new(GoalPlugin::new(goal, self.grader.clone())));
        // The goal run auto-approves tools to drive to a deliverable, so it does not
        // advertise `agent_run` (which parks and is host-fulfilled, not auto-run).
        // The outcome/goal run does not offer skills (ADR-0036): it auto-approves
        // tools to drive a deliverable and does not register the `Skill` tool.
        let config = server_config(
            &self.model_ref,
            &self.client_tools,
            &HashSet::new(),
            &["goal".to_string()],
            &self.plugin_config,
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );

        // One run: the guard re-derives and grades the deliverable, then steers
        // revisions. Outcome rounds auto-approve tools. Empty input re-infers over
        // the committed history. A concurrent `interrupt` cancels this run.
        let phase = goal_runtime
            .run_to_completion(
                &config,
                thread,
                Vec::<Message>::new(),
                ctx.context(),
                |_| ResumeResult::allow(),
            )
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;

        // Project from DURABLE truth: the committed `Continuation` events the run
        // recorded, each carrying the round's opaque detail (result + explanation).
        // A `consumed_rounds` cursor scopes this to the rounds this call produced.
        let rounds: Vec<serde_json::Value> = ctx.commit.continuation_payloads(&ctx.thread_id);
        let fresh = &rounds[st.consumed_rounds.min(rounds.len())..];
        st.consumed_rounds = rounds.len();

        let outcome_id = format!("outc_{thread}");
        let all = ctx.commit.committed_messages(&ctx.thread_id);
        let mut iterations: Vec<HostOutcomeIteration> = fresh
            .iter()
            .enumerate()
            .map(|(i, detail)| HostOutcomeIteration {
                messages: if i == 0 { all.clone() } else { Vec::new() },
                outcome_id: outcome_id.clone(),
                iteration: i as u32 + 1,
                result: detail_str(detail, "result"),
                explanation: detail_str(detail, "explanation"),
            })
            .collect();
        // An interrupted run ends `Cancelled` before the guard can conclude, so
        // no terminal `Continuation` was committed. Report the outcome as
        // `interrupted` — distinct from satisfied/failed/max_iterations.
        if matches!(phase, Phase::Ended(EndCause::Cancelled)) {
            iterations.push(HostOutcomeIteration {
                messages: Vec::new(),
                outcome_id: outcome_id.clone(),
                iteration: iterations.len() as u32 + 1,
                result: "interrupted".to_string(),
                explanation: "the outcome was interrupted".to_string(),
            });
        }
        Ok(HostOutcomeReport { iterations })
    }

    /// Project the step's delta, update the parked position, and publish the
    /// delta to the thread hub for any observing protocol.
    fn finish_step(
        &self,
        ctx: &SessionCtx,
        st: &mut SessionState,
        run_id: RunId,
        phase: Phase,
        before: usize,
        thread: &str,
    ) -> TurnResult {
        let all = ctx.commit.committed_messages(&ctx.thread_id);
        let new_messages = all[before.min(all.len())..].to_vec();
        let (pending, waiting) = match &phase {
            Phase::Waiting => {
                st.parked = Some(run_id.clone());
                let pending = ctx
                    .commit
                    .waiting_ticket(&run_id)
                    .and_then(|t| pending_from_ticket(&t, &self.client_tools));
                (pending, true)
            }
            _ => {
                st.parked = None;
                (None, false)
            }
        };
        if !new_messages.is_empty() {
            self.hub
                .publish(thread, ThreadEvent::Committed(new_messages.clone()));
        }
        self.hub.publish(thread, ThreadEvent::StepEnded { waiting });
        TurnResult {
            new_messages,
            phase,
            pending,
        }
    }

    /// Fail closed before resuming: the asserted `tool_use_id` must name the
    /// run's pending tool, and that tool's binding must match the inbound resume
    /// — a client result may only answer a client-executed tool, a confirmation
    /// only a built-in one.
    fn check_pending(
        &self,
        ticket: &WaitingTicket,
        tool_use_id: &str,
        want_client: bool,
    ) -> Result<(), HostError> {
        if ticket.call_id.as_deref() != Some(tool_use_id) {
            return Err(HostError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending tool"
            )));
        }
        let pending_tool_id = ticket
            .pending_tool
            .as_ref()
            .map(|t| t.tool_id.as_str())
            .ok_or_else(|| HostError::internal("parked run has no pending tool"))?;
        let is_client = self.client_tools.contains(pending_tool_id);
        if is_client != want_client {
            let (got, expected) = if want_client {
                ("built-in", "a confirmation")
            } else {
                ("client-executed", "a client tool result")
            };
            return Err(HostError::bad_request(format!(
                "pending tool is {got}; answer it with {expected}"
            )));
        }
        Ok(())
    }
}

/// Read the pending tool off a waiting ticket, classifying it client-executed
/// when its id is in `client_tools`.
fn pending_from_ticket(
    ticket: &WaitingTicket,
    client_tools: &HashSet<String>,
) -> Option<PendingTool> {
    let tool_use_id = ticket.call_id.clone()?;
    let tool = ticket.pending_tool.clone()?;
    // A parked delegation is client-executed from the caller's view: the user
    // supplies the input, delivered back through `resume`.
    let client_executed =
        ticket.reason == WaitingReason::Delegation || client_tools.contains(&tool.tool_id);
    Some(PendingTool {
        tool_use_id,
        name: tool.tool_id,
        input: tool.arguments,
        client_executed,
    })
}

#[cfg(test)]
mod tests;
