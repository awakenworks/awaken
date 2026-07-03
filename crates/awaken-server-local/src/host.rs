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
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_ext_goal::{
    DelegateError, DelegateGrader, DelegateReply, DelegateRequest, DelegateRunner, GoalPlugin,
    GoalSpec, Grader, KeywordGrader,
};
use awaken_ext_skills::{SkillRegistry, SkillSpec};
use awaken_protocol_a2a::Transport;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{DirectRunIngress, RunIngress, Runtime};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::agent_resolver::AgentResolver;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::ToolOutput;
use awaken_sandbox_local::{Environment, LocalSandboxProvider, SandboxProvider, SandboxSpec};
use awaken_store_fs::FsCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

use awaken_ext_compact::{CompactConfig, CompactPlugin, Summarizer};

use crate::agent_catalog::AgentCatalog;
use crate::background::BackgroundRuns;
use crate::compact::AgentSummarizer;
use crate::config::{build_runtime, server_config, server_gate};
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

/// A filesystem-safe database filename stem for a thread id (durable store).
pub(crate) fn sanitize_thread(thread: &str) -> String {
    thread
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

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
    ingress: Arc<dyn RunIngress>,
    config: RunnableConfig,
    pub(crate) commit: Arc<HostCommit>,
    pub(crate) thread_id: ThreadId,
    /// The thread's sandbox environment, reused to build a goal-enabled runtime
    /// for `define_outcome` (same tools, same environment).
    env: Arc<Environment>,
    /// The thread's skill registry (delivered + workspace), used to expand user
    /// `/skill-name` invocations. `None` when skills are not offered.
    skill_registry: Option<Arc<dyn SkillRegistry>>,
    /// The in-flight run's cancellation token, so a concurrent `interrupt` (a
    /// separate request) can cancel it. A plain `std::sync::Mutex` (brief locks),
    /// held by neither the run loop nor the state lock, so interrupt never blocks
    /// on the loop that holds `state`.
    cancel: std::sync::Mutex<Option<CancellationToken>>,
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
        RuntimeRunContext::new()
            .with_commit(self.commit.clone())
            .with_reader(self.commit.clone())
            .with_cancellation(token)
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

/// Runs a judge sub-agent through the kernel for a [`DelegateGrader`]: a fresh
/// rooted runtime over the same model, driven to completion; its last assistant
/// line is the judge's reply. The judge sees only its prompt (a fresh window), so
/// its verdict is not biased by the doer's working state.
struct KernelJudgeRunner {
    llm: Arc<dyn LlmExecutor>,
    provider: LocalSandboxProvider,
    /// The judge is resolved by id from here, so its model/instructions/window are
    /// configured per-agent rather than hard-coded (like memory and compact).
    catalog: Arc<AgentCatalog>,
    seq: AtomicU64,
}

#[async_trait::async_trait]
impl DelegateRunner for KernelJudgeRunner {
    async fn run(&self, request: DelegateRequest) -> Result<DelegateReply, DelegateError> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        // The judge sees only its prompt (a fresh window); its cancellation is the
        // parent run's, so cancelling the outcome cancels the judge too.
        let name = format!("{}-judge-{n}", request.agent_id);
        let text = crate::subagent::run_configured_subrun(
            &self.catalog,
            &self.provider,
            self.llm.clone(),
            &request.agent_id,
            &name,
            request.prompt,
            Vec::new(),
            request.cancellation,
        )
        .await
        .map_err(DelegateError)?;
        Ok(DelegateReply { text: Some(text) })
    }
}

/// The protocol-neutral, thread-keyed session substrate shared by every adapter.
pub struct SharedHost {
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) model_ref: String,
    pub(crate) provider: LocalSandboxProvider,
    grader: Arc<dyn Grader>,
    client_tools: HashSet<String>,
    /// Skills offered on every thread (ADR-0036). The whole set is fronted by the
    /// single `Skill` tool; the model activates one by id to load its instructions.
    skills: Vec<SkillSpec>,
    pub(crate) delegates: HashSet<String>,
    /// Runtime plugins this host activates on every thread, and their config
    /// sections (e.g. the tool state machine). Empty by default.
    pub(crate) plugin_ids: Vec<String>,
    pub(crate) plugin_config: std::collections::BTreeMap<String, serde_json::Value>,
    sessions: tokio::sync::Mutex<HashMap<String, Arc<SessionCtx>>>,
    hub: Arc<ThreadEventHub>,
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
}

impl SharedHost {
    /// A host over `llm`. Configure it with the chainable `with_*` builders
    /// (client tools, delegates, a judge grader, a durable store).
    pub fn new(llm: Arc<dyn LlmExecutor>, model_ref: impl Into<String>) -> Self {
        Self {
            llm,
            model_ref: model_ref.into(),
            provider: LocalSandboxProvider::new(sub_base("")),
            grader: Arc::new(KeywordGrader),
            client_tools: HashSet::new(),
            skills: Vec::new(),
            delegates: HashSet::new(),
            plugin_ids: Vec::new(),
            plugin_config: std::collections::BTreeMap::new(),
            sessions: tokio::sync::Mutex::new(HashMap::new()),
            hub: Arc::new(ThreadEventHub::new()),
            // Composition root: the deployment picks durability via the
            // environment. `AWAKEN_STORAGE_DIR` set → each thread commits to a
            // durable SQLite database under it (survives a restart); unset (unit
            // tests, ephemeral use) → in-memory. `with_store_dir` still overrides.
            store_dir: std::env::var("AWAKEN_STORAGE_DIR")
                .ok()
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            remote_agents: HashMap::new(),
            memory: None,
            memory_selector: None,
            compact_config: None,
            compact_summarizer: None,
            config_service: None,
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

    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools only. (Wire-driven file
    /// resources are a later milestone — a Files API plus a `resources` input channel.)
    fn sandbox_spec(&self, thread: &str) -> SandboxSpec {
        SandboxSpec::new(thread)
    }

    /// The registered built-in tools advertised on a managed session's agent object:
    /// each hand-tool id and whether its calls require confirmation. Folded into the
    /// public `agent_toolset` by the adapter. Deterministic from host config.
    pub fn builtin_tools(&self) -> Vec<(String, bool)> {
        crate::config::builtin_hand_tools()
    }

    /// The client-executed (custom) tools advertised on a managed session: their
    /// descriptors, so the adapter can shape each as a `custom` tool definition.
    pub fn custom_tools(&self) -> Vec<ToolDescriptor> {
        self.client_tools
            .iter()
            .map(|id| crate::config::client_tool_descriptor(id))
            .collect()
    }

    /// The skill ids offered on every thread (advertised as the agent's `skills`).
    pub fn skill_ids(&self) -> Vec<String> {
        self.skills.iter().map(|s| s.id.clone()).collect()
    }

    /// The delegate agent ids (advertised as the agent's `multiagent` roster).
    pub fn delegate_ids(&self) -> Vec<String> {
        self.delegates.iter().cloned().collect()
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

    async fn ctx_for(
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
        let thread_id = ThreadId(thread.to_string());
        let commit = Arc::new(self.build_commit(thread).await?);
        let mut runtime = build_runtime(self.llm.clone(), &env);
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
            env.clone(),
            self.llm.clone(),
            &self.model_ref,
            thread,
            server_gate(),
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
        let config = installed.unwrap_or_else(|| {
            server_config(
                &self.model_ref,
                &self.client_tools,
                &self.delegates,
                &plugin_ids,
                &self.plugin_config,
                &skill_descriptors,
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
        // The foreground delivery seam (slice C): a turn's execution goes through
        // `RunIngress::submit` rather than calling `runtime.start_turn` directly.
        // Direct ingress runs inline on the same `runtime`, so behavior is identical;
        // durable dispatch (slice D) swaps this without touching the turn path.
        let ingress: Arc<dyn RunIngress> = Arc::new(DirectRunIngress::new(runtime.clone()));
        let ctx = Arc::new(SessionCtx {
            runtime,
            ingress,
            config,
            commit,
            thread_id,
            env,
            skill_registry,
            cancel: std::sync::Mutex::new(None),
            state: tokio::sync::Mutex::new(state),
        });
        sessions.insert(thread.to_string(), ctx.clone());
        Ok(ctx)
    }

    /// All messages committed on `thread` so far (the source of history). Empty
    /// when the thread has not run yet. Resolves through `ctx_for`, so a durable
    /// thread is hydrated from its store on demand — a fresh process reads a
    /// parked thread's committed transcript even before any session touches it
    /// (ADR-0039), enabling post-restart session rehydration.
    pub async fn committed_messages(&self, thread: &str) -> Vec<Message> {
        match self.ctx_for(thread, None).await {
            Ok(ctx) => ctx.commit.committed_messages(&ctx.thread_id),
            Err(_) => Vec::new(),
        }
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
    pub async fn run_turn(
        &self,
        agent: Option<&str>,
        thread: &str,
        input: Vec<Message>,
    ) -> Result<TurnResult, HostError> {
        let ctx = self.ctx_for(thread, agent).await?;
        let mut st = ctx.state.lock().await;
        if st.parked.is_some() {
            return Err(HostError::bad_request("thread is awaiting a tool decision"));
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
        let (run_id, activation) = ctx
            .runtime
            .prepare(&ctx.config, thread.to_string(), messages)
            .map_err(|e| HostError::internal(e.to_string()))?;
        let phase = ctx
            .ingress
            .submit(activation, ctx.context())
            .await
            .map_err(|e| HostError::internal(e.to_string()))?;
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
mod tests {
    use super::*;
    use crate::config::block_text;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, ChatRole};
    use std::sync::atomic::AtomicUsize;

    /// A model that blocks on its second inference (the first revision round) until
    /// a gate is released, so a concurrent `interrupt` can land while the outcome
    /// loop is mid-run. Its reply never contains the rubric, so the guard steers.
    struct GatedModel {
        gate: Arc<tokio::sync::Notify>,
        reached: Arc<tokio::sync::Notify>,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmExecutor for GatedModel {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.reached.notify_one();
                self.gate.notified().await;
            }
            Ok(ChatResponse {
                output: AssistantOutput::text("a rough draft"),
                usage: None,
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_cancels_the_run_and_reports_interrupted() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let reached = Arc::new(tokio::sync::Notify::new());
        let model = Arc::new(GatedModel {
            gate: gate.clone(),
            reached: reached.clone(),
            calls: AtomicUsize::new(0),
        });
        let host = Arc::new(SharedHost::new(model, "scripted"));

        // Drive an outcome whose rubric is never met, so it would loop; the model
        // blocks it mid second round.
        let driver = host.clone();
        let task =
            tokio::spawn(async move { driver.define_outcome("t1", "finish", "FINAL", 5).await });

        // Once the loop is blocked mid-run, interrupt it, then release the gate.
        reached.notified().await;
        host.interrupt("t1").await.expect("interrupt");
        gate.notify_one();

        let report = task.await.expect("join").expect("define_outcome");
        // Round 1 graded needs_revision; the interrupt ended the run before the
        // second round could conclude, so the outcome reports interrupted.
        assert_eq!(report.iterations[0].result, "needs_revision");
        assert_eq!(
            report.iterations.last().expect("a round").result,
            "interrupted"
        );
    }

    #[tokio::test]
    async fn interrupt_is_a_noop_when_nothing_runs() {
        let host = SharedHost::new(
            Arc::new(GatedModel {
                gate: Arc::new(tokio::sync::Notify::new()),
                reached: Arc::new(tokio::sync::Notify::new()),
                calls: AtomicUsize::new(0),
            }),
            "scripted",
        );
        // No run in flight → interrupt succeeds and does nothing.
        host.interrupt("idle-thread")
            .await
            .expect("interrupt is a no-op");
    }

    /// The main assistant answers plainly; the memory extractor (identified by its
    /// system instructions) saves one memory then reports done.
    struct MemoryHostModel;

    #[async_trait::async_trait]
    impl LlmExecutor for MemoryHostModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            use awaken_runtime_contract::llm::{ChatRole, ToolCall};
            let is_extractor = request.messages.iter().any(|m| {
                m.role == ChatRole::System
                    && m.content.iter().any(|b| match b {
                        ContentBlock::Text { text } => text.contains("memory extraction sub-agent"),
                        _ => false,
                    })
            });
            let output = if is_extractor {
                if request.messages.iter().any(|m| m.role == ChatRole::Tool) {
                    AssistantOutput::text("saved 1 memory")
                } else {
                    AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "w".into(),
                        tool_id: "write_memory".into(),
                        arguments: serde_json::json!({
                            "name": "user prefs",
                            "content": "user likes rust",
                        }),
                    }])
                }
            } else {
                AssistantOutput::text("ok")
            };
            Ok(ChatResponse {
                output,
                usage: None,
            })
        }
    }

    /// The compactor (identified by its instructions) replies with a fixed summary;
    /// the main assistant reports whether it saw a delivered summary in its system
    /// messages, proving the summary reached the next turn's model input.
    struct CompactHostModel;

    #[async_trait::async_trait]
    impl LlmExecutor for CompactHostModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            let system_text: String = request
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::System)
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let reply = if system_text.contains("conversation-compaction sub-agent") {
                "COMPACTED".to_string()
            } else if system_text.contains("Summary of earlier conversation") {
                "seen-summary".to_string()
            } else {
                "no-summary".to_string()
            };
            Ok(ChatResponse {
                output: AssistantOutput::text(reply),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn compaction_summary_reaches_the_same_long_turn() {
        let host = SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction(1, 1);
        let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, "hello")];

        // Turn 1: only the single user message → below threshold, no summary injected.
        let r1 = host.run_turn("t-c", user("u1")).await.expect("turn 1");
        assert!(matches!(r1.phase, Phase::Ended(_)));
        let reply1 = r1
            .new_messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        assert_eq!(reply1, "no-summary", "short turn is not compacted");

        // Turn 2: the conversation now exceeds the threshold, so the compact plugin's
        // BeforeInference hook summarizes the older slice inline and the model sees it.
        let r2 = host.run_turn("t-c", user("u2")).await.expect("turn 2");
        let reply2 = r2
            .new_messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        assert_eq!(reply2, "seen-summary");
    }

    /// The extractor saves "the user prefers tea"; the main agent answers "tea"
    /// only when that memory is present in its system context (recalled).
    struct MemLoopModel;

    #[async_trait::async_trait]
    impl LlmExecutor for MemLoopModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            use awaken_runtime_contract::llm::{ChatRole, ToolCall};
            let system_text: String = request
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::System)
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if system_text.contains("memory extraction sub-agent") {
                let already = request.messages.iter().any(|m| {
                    m.role == ChatRole::Tool
                        && m.content.iter().any(|b| match b {
                            ContentBlock::ToolResult { content, .. } => {
                                block_text(content).contains("saved memory")
                            }
                            _ => false,
                        })
                });
                let output = if already {
                    AssistantOutput::text("done")
                } else {
                    AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "w".into(),
                        tool_id: "write_memory".into(),
                        arguments: serde_json::json!({
                            "name": "beverage-preference",
                            "content": "the user prefers tea",
                        }),
                    }])
                };
                return Ok(ChatResponse {
                    output,
                    usage: None,
                });
            }
            // Main agent: answer from recalled memory when present.
            let reply = if system_text.contains("the user prefers tea") {
                "tea"
            } else {
                "ok"
            };
            Ok(ChatResponse {
                output: AssistantOutput::text(reply),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn memory_written_in_one_thread_is_recalled_and_used_in_another() {
        let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
        let mem_dir = std::env::temp_dir().join(format!("awaken-loop-mem-{stamp}"));
        let host = SharedHost::new(Arc::new(MemLoopModel), "stub").with_memory(&mem_dir);
        let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

        // Thread 1: the user states a preference; extraction saves it.
        host.run_turn("thread-1", user("I really enjoy tea in the morning"))
            .await
            .expect("thread 1 turn");
        assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
        assert!(
            mem_dir.join("beverage-preference.md").exists(),
            "the preference should be saved"
        );

        // Thread 2 (a fresh conversation): the saved memory is recalled into context
        // and the agent uses it to answer.
        let r = host
            .run_turn("thread-2", user("What beverage do I prefer?"))
            .await
            .expect("thread 2 turn");
        let reply = r
            .new_messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| block_text(&m.content))
            .unwrap_or_default();
        assert_eq!(
            reply, "tea",
            "the fresh thread should recall and use the saved memory"
        );
    }

    /// The main agent parks on a `write` (Ask-gated) then finishes on resume; the
    /// extractor saves a memory. Proves resume-ended turns trigger the aux agents.
    struct ResumeMemModel;

    #[async_trait::async_trait]
    impl LlmExecutor for ResumeMemModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            use awaken_runtime_contract::llm::{ChatRole, ToolCall};
            let saw_tool = request.messages.iter().any(|m| m.role == ChatRole::Tool);
            // The extractor's own write_memory succeeded (its result text), distinct
            // from the main turn's `write` result that is also in its seeded context.
            let saved_memory = request.messages.iter().any(|m| {
                m.role == ChatRole::Tool
                    && m.content.iter().any(|b| match b {
                        ContentBlock::ToolResult { content, .. } => {
                            block_text(content).contains("saved memory")
                        }
                        _ => false,
                    })
            });
            let is_extractor = request.messages.iter().any(|m| {
                m.role == ChatRole::System
                    && m.content.iter().any(|b| match b {
                        ContentBlock::Text { text } => text.contains("memory extraction sub-agent"),
                        _ => false,
                    })
            });
            let output = if is_extractor {
                if saved_memory {
                    AssistantOutput::text("extracted")
                } else {
                    AssistantOutput::from_tool_calls(vec![ToolCall {
                        call_id: "mw".into(),
                        tool_id: "write_memory".into(),
                        arguments: serde_json::json!({ "name": "resumed", "content": "after-resume" }),
                    }])
                }
            } else if saw_tool {
                AssistantOutput::text("done")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w1".into(),
                    tool_id: "write".into(),
                    arguments: serde_json::json!({ "path": "note.txt", "content": "x" }),
                }])
            };
            Ok(ChatResponse {
                output,
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn resume_ended_turn_triggers_memory_extraction() {
        let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
        let mem_dir = std::env::temp_dir().join(format!("awaken-resume-mem-{stamp}"));
        let host = SharedHost::new(Arc::new(ResumeMemModel), "stub").with_memory(&mem_dir);

        // Turn 1 parks on the Ask-gated `write`.
        let r1 = host
            .run_turn(
                "t-res",
                vec![Message::text(MessageId("u1".into()), Role::User, "hi")],
            )
            .await
            .expect("turn 1");
        assert!(
            matches!(r1.phase, Phase::Waiting),
            "turn should park on write"
        );
        let pending = r1.pending.expect("a pending tool");

        // Resume approves the write; the turn now ends and extraction fires.
        let r2 = host
            .resume(
                "t-res",
                &pending.tool_use_id,
                HostResume::Confirm {
                    allow: true,
                    note: None,
                },
            )
            .await
            .expect("resume");
        assert!(
            matches!(r2.phase, Phase::Ended(_)),
            "resume should end the turn"
        );

        assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
        let saved = std::fs::read_to_string(mem_dir.join("resumed.md")).expect("memory file");
        assert_eq!(saved, "after-resume");
    }

    /// The extractor writes a `seen.md` whose content is the non-prompt user texts
    /// it was seeded with, so a test can check which messages each extraction saw.
    struct CursorModel;

    #[async_trait::async_trait]
    impl LlmExecutor for CursorModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            use awaken_runtime_contract::llm::{ChatRole, ToolCall};
            let is_extractor = request.messages.iter().any(|m| {
                m.role == ChatRole::System
                    && m.content.iter().any(|b| match b {
                        ContentBlock::Text { text } => text.contains("memory extraction sub-agent"),
                        _ => false,
                    })
            });
            if !is_extractor {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("ok"),
                    usage: None,
                });
            }
            if request.messages.iter().any(|m| m.role == ChatRole::Tool) {
                return Ok(ChatResponse {
                    output: AssistantOutput::text("extracted"),
                    usage: None,
                });
            }
            // Join the user texts it was seeded with, excluding the extraction prompt.
            let seen: Vec<String> = request
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::User)
                .map(|m| block_text(&m.content))
                .filter(|t| !t.contains("Extract durable memories"))
                .collect();
            Ok(ChatResponse {
                output: AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({ "name": "seen", "content": seen.join(",") }),
                }]),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn extraction_cursor_only_processes_new_messages() {
        let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
        let mem_dir = std::env::temp_dir().join(format!("awaken-cursor-mem-{stamp}"));
        let host = SharedHost::new(Arc::new(CursorModel), "stub").with_memory(&mem_dir);
        let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

        host.run_turn("t-cur", user("alpha")).await.expect("turn 1");
        assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
        host.run_turn("t-cur", user("beta")).await.expect("turn 2");
        assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);

        // The second extraction saw only "beta" — turn 1's "alpha" was past the cursor.
        let seen = std::fs::read_to_string(mem_dir.join("seen.md")).expect("seen file");
        assert_eq!(
            seen, "beta",
            "cursor should exclude already-extracted messages"
        );
    }

    #[tokio::test]
    async fn turn_end_fires_background_memory_extraction() {
        let stamp = BASE_SEQ.fetch_add(1, Ordering::SeqCst);
        let mem_dir = std::env::temp_dir().join(format!("awaken-host-mem-{stamp}"));
        let host = SharedHost::new(Arc::new(MemoryHostModel), "stub").with_memory(&mem_dir);

        let input = vec![Message::text(
            MessageId("u1".into()),
            Role::User,
            "I really like rust",
        )];
        let result = host.run_turn("t-mem", input).await.expect("run turn");
        assert!(matches!(result.phase, Phase::Ended(_)), "turn should end");

        let drained = host.drain_memory(std::time::Duration::from_secs(10)).await;
        assert!(drained, "memory extraction should drain");

        let saved =
            std::fs::read_to_string(mem_dir.join("user-prefs.md")).expect("memory file written");
        assert_eq!(saved, "user likes rust");
    }
}
