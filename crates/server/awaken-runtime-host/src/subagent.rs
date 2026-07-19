//! Running an Agent through the ordinary Runtime lifecycle.
//!
//! Native delegation (`agent_run`), the goal judge, and skill forks all use this
//! substrate. A delegated Agent receives a first-class child Run identity, the
//! same durable context and capabilities as a directly initiated Agent, and the
//! same delegation executor, so nested delegation is not a special execution
//! path. Auxiliary Agents may deliberately request transient identity and an
//! isolated context because their work is outside the user-visible Run tree.
//!
//! [`run_configured_agent`] is the parameterized form: it resolves the Agent's
//! own `RunnableConfig` (instructions, model, tools) from an [`AgentCatalog`] by
//! id. [`run_agent`] is the thin wrapper that runs the default
//! `assistant` config with a plain string prompt.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::delegation::DelegationOrigin;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime::RunInput;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::delegation::DelegationExecutor;
use awaken_runtime_contract::llm::{LlmExecutor, ThreadUsage};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

use crate::agent_catalog::AgentCatalog;
use crate::config::{build_runtime, latest_assistant_text, server_config};

/// Where an Agent Run's tools execute. A delegated Agent shares the initiating
/// Agent's sandbox by default, or receives a fresh sandbox when placement policy
/// requests isolation. Sandbox placement does not alter Run semantics.
pub(crate) enum AgentRunSandbox<'a> {
    /// Reuse the parent agent's live sandbox — the default (`与主 agent 共用`).
    Shared(&'a LocalSandbox),
    /// Create a fresh, isolated sandbox for this Agent Run via the given provider.
    Fresh(&'a LocalProvider),
}

/// How an Agent Run's own usage contributes to the initiating Run's accounting
/// projection. The child retains its own committed usage in either case; this
/// policy controls only the additional parent rollup.
/// Making this an explicit argument consumed at the single Agent Run seam keeps the
/// fold decision from being an implicit `let _usage` scattered across call sites: a
/// delegated work folds; out-of-band housekeeping (judge,
/// memory extraction/selection, compaction, skill activation) stays isolated and is
/// never counted against the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageRollup {
    /// Fold the child Run's usage into the parent thread's tally — the caller threads
    /// the returned [`ThreadUsage`] back through its step.
    FoldIntoParent,
    /// Keep auxiliary Agent usage out of the initiating session's tally.
    Isolated,
}

/// Identity used by the Agent Run substrate. Auxiliary work gets a transient
/// thread name; delegation supplies a durable child Run identity.
pub(crate) struct AgentRunIdentity<'a> {
    thread: &'a str,
    run_id: Option<RunId>,
    initiator: Option<DelegationOrigin>,
}

/// Runtime capabilities of an Agent regardless of who initiated its Run.
/// A child Run receives the target Agent's own delegation interface and roster;
/// initiation never copies capabilities from the parent Agent.
pub(crate) struct AgentExecution<'a> {
    pub(crate) agent_id: &'a str,
    pub(crate) model_ref: &'a str,
    pub(crate) delegates: &'a HashSet<String>,
    pub(crate) delegation_executor: Option<Arc<dyn DelegationExecutor>>,
    pub(crate) context: Option<RuntimeRunContext>,
}

impl<'a> AgentRunIdentity<'a> {
    pub(crate) fn transient(thread: &'a str) -> Self {
        Self {
            thread,
            run_id: None,
            initiator: None,
        }
    }

    pub(crate) fn child(run_id: &'a RunId, initiator: DelegationOrigin) -> Self {
        Self {
            thread: &run_id.0,
            run_id: Some(run_id.clone()),
            initiator: Some(initiator),
        }
    }
}

/// Run the agent identified by `agent_id` — its config resolved from `catalog` —
/// on its own `thread`, seeded with `seed`, to completion, returning its last
/// assistant line **and its accumulated token usage**. A child Run receives the
/// parent's resolved Runtime context and delegation executor, so it follows the
/// ordinary Agent lifecycle; auxiliary work may request an isolated context.
/// `cancellation`, when set, is forwarded so cancelling the parent cancels the
/// child Run too.
///
/// The returned [`ThreadUsage`] follows `rollup`: under
/// [`FoldIntoParent`](UsageRollup::FoldIntoParent) it is read from the child Run's own
/// committed thread state so the caller can fold it into a parent thread's total;
/// under [`Isolated`](UsageRollup::Isolated) the return is empty, so an auxiliary Agent's tokens are never
/// counted against the session. Also empty when the model reported no usage.
///
/// Errors (unknown agent, sandbox/runtime failure) are returned as strings for
/// the caller to wrap.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_agent(
    catalog: &AgentCatalog,
    sandbox: AgentRunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    seed: impl Into<RunInput>,
    extra_tools: Vec<Arc<dyn RawTool>>,
    cancellation: Option<CancellationToken>,
    context: Option<RuntimeRunContext>,
    delegation_executor: Option<Arc<dyn DelegationExecutor>>,
    run_id: Option<RunId>,
    initiator: Option<DelegationOrigin>,
    rollup: UsageRollup,
) -> Result<(String, ThreadUsage), String> {
    let config = catalog
        .resolve(agent_id)
        .ok_or_else(|| format!("unknown agent {agent_id:?}"))?
        .clone();
    // Reuse the parent's sandbox by default; a `Fresh` Agent Run gets its own root. The
    // created sandbox (if any) is bound here so the borrow lives for the whole run.
    let created = match &sandbox {
        AgentRunSandbox::Fresh(provider) => Some(
            provider
                .create_sandbox(&crate::provisioning::agent_run_sandbox_spec(thread))
                .await
                .map_err(|e| e.to_string())?,
        ),
        AgentRunSandbox::Shared(_) => None,
    };
    let env: &LocalSandbox = match &sandbox {
        AgentRunSandbox::Shared(shared) => shared,
        AgentRunSandbox::Fresh(_) => created
            .as_ref()
            .expect("a Fresh Agent Run created a sandbox"),
    };
    let mut runtime = build_runtime(llm, env);
    // Tools the caller provisions on top of the sandbox's own (e.g. a
    // persistent write_memory scoped outside the ephemeral sandbox).
    for tool in extra_tools {
        runtime = runtime.with_tool(tool);
    }
    if let Some(executor) = delegation_executor {
        runtime = runtime.with_delegation_executor(executor);
    }
    let mut ctx = context.unwrap_or_else(|| {
        let commit = Arc::new(MemoryCommitCoordinator::new());
        RuntimeRunContext::new()
            .with_commit(commit.clone())
            .with_reader(commit)
    });
    if let Some(token) = cancellation {
        ctx = ctx.with_cancellation(token);
    }
    if ctx.commit.is_none() || ctx.reader.is_none() {
        return Err("an Agent Run requires commit and history wiring".to_string());
    }
    let reader = ctx.reader.clone().expect("checked above");
    let thread_id = ThreadId(thread.to_string());
    match run_id {
        Some(run_id) => match initiator {
            Some(origin) => {
                runtime
                    .run_delegated_to_completion(&config, run_id, thread, seed, ctx, origin, |_| {
                        ResumeResult::allow()
                    })
                    .await
            }
            None => {
                runtime
                    .run_to_completion_with_id(&config, run_id, thread, seed, ctx, |_| {
                        ResumeResult::allow()
                    })
                    .await
            }
        },
        None => {
            runtime
                .run_to_completion(&config, thread, seed, ctx, |_| ResumeResult::allow())
                .await
        }
    }
    .map_err(|e| e.to_string())?;
    let text = latest_assistant_text(&reader.committed_messages(&thread_id));
    // The rollup decision lives here; it never changes the child Run's committed
    // usage, only whether the initiating Run receives an additional projection.
    let usage = match rollup {
        UsageRollup::FoldIntoParent => usage_from_committed(reader.as_ref(), &thread_id),
        UsageRollup::Isolated => ThreadUsage::default(),
    };
    Ok((text, usage))
}

/// Read a finished Agent Run's accumulated [`ThreadUsage`] out of committed
/// thread state. The run loop writes the running cumulative under
/// `THREAD_USAGE_STATE_KEY` each step, so the last `Set` is the whole tally
/// (mirrors `SharedHost::thread_usage`).
fn usage_from_committed(reader: &dyn ThreadReader, thread_id: &ThreadId) -> ThreadUsage {
    ThreadUsage::from_committed_state(&reader.committed_state(thread_id))
}

/// Run a default `assistant` Agent with `input` to completion and return its last
/// assistant line. Thin wrapper over [`run_configured_agent`]: it
/// builds a one-entry catalog holding the shared `assistant` config (no skills,
/// ADR-0036). Callers that need a differently-configured agent resolve it from a
/// richer catalog through [`run_configured_agent`] instead.
pub(crate) async fn run_agent(
    llm: Arc<dyn LlmExecutor>,
    execution: AgentExecution<'_>,
    sandbox: AgentRunSandbox<'_>,
    identity: AgentRunIdentity<'_>,
    input: impl Into<RunInput>,
    cancellation: Option<CancellationToken>,
    rollup: UsageRollup,
) -> Result<(String, ThreadUsage), String> {
    // This compatibility wrapper does not offer skills (ADR-0036).
    let config = server_config(
        execution.agent_id,
        execution.model_ref,
        &HashSet::new(),
        execution.delegates,
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let catalog = AgentCatalog::new().with_agent(config);
    run_configured_agent(
        &catalog,
        sandbox,
        llm,
        execution.agent_id,
        identity.thread,
        input,
        Vec::new(),
        cancellation,
        execution.context,
        execution.delegation_executor,
        identity.run_id,
        identity.initiator,
        rollup,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult,
    };
    use awaken_runtime_contract::resolved::ModelBinding;
    use awaken_runtime_contract::runnable::RunnableConfig;

    /// A model that replies with the leading system instruction it was given, so a
    /// test can prove the sub-run resolved that agent's own config.
    struct InstructionEchoModel;

    #[async_trait::async_trait]
    impl LlmExecutor for InstructionEchoModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let system = request
                .messages
                .iter()
                .find(|m| m.role == Role::System)
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            Ok(ChatResponse {
                output: AssistantOutput::text(system),
                usage: None,
                stop_reason: None,
            })
        }
    }

    fn agent(id: &str, instructions: &str) -> RunnableConfig {
        RunnableConfig::builder(id)
            .instructions(instructions)
            .model(ModelBinding::new("default", "stub", "default"))
            .max_steps(4)
            .build()
    }

    fn user(text: &str) -> Message {
        Message {
            id: MessageId("u1".into()),
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    #[tokio::test]
    async fn resolves_agent_config_by_id() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new()
            .with_agent(agent("memory-extractor", "MEMORY INSTRUCTIONS"))
            .with_agent(agent("judge", "JUDGE INSTRUCTIONS"));
        let llm = Arc::new(InstructionEchoModel);

        let (mem, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "memory-extractor",
            "t-mem",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap();
        assert_eq!(mem, "MEMORY INSTRUCTIONS");

        let (judge, _) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            llm.clone(),
            "judge",
            "t-judge",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap();
        assert_eq!(judge, "JUDGE INSTRUCTIONS");
    }

    /// A model that reports fixed token usage per inference, so a test can prove the
    /// sub-run's usage is read back out of its isolated commit store (rather than
    /// discarded with it).
    struct UsageModel;

    #[async_trait::async_trait]
    impl LlmExecutor for UsageModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: Some(awaken_runtime_contract::llm::TokenUsage {
                    prompt_tokens: 13,
                    completion_tokens: 5,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn agent_run_returns_its_accumulated_usage() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-usage-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));

        let (_text, usage) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(UsageModel),
            "worker",
            "t-usage",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            UsageRollup::FoldIntoParent,
        )
        .await
        .unwrap();

        // The sub-run's model reported 13/5 on its one step; that must survive the
        // committed Agent Run state as the returned tally.
        let total = usage.total();
        assert_eq!(total.prompt_tokens, 13);
        assert_eq!(total.completion_tokens, 5);
        assert_eq!(
            usage
                .by_model
                .get("stub")
                .copied()
                .unwrap_or_default()
                .prompt_tokens,
            13
        );
    }

    #[tokio::test]
    async fn isolated_rollup_excludes_auxiliary_agent_usage() {
        // The same UsageModel reports 13/5, but under `Isolated` the seam must return
        // an empty tally so an out-of-band sub-run's tokens are never folded into the
        // session — the governance invariant that keeps housekeeping off the bill.
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-isolated-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));

        let (_text, usage) = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(UsageModel),
            "worker",
            "t-isolated",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap();

        assert_eq!(
            usage.total().prompt_tokens,
            0,
            "an isolated sub-run's usage must not survive the seam"
        );
        assert!(usage.by_model.is_empty());
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let base = std::env::temp_dir().join("awaken-subrun-test-unknown");
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let err = run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&provider),
            Arc::new(InstructionEchoModel),
            "nope",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[tokio::test]
    async fn a_shared_agent_run_reuses_the_parent_sandbox_while_fresh_makes_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let parent_base = tmp.path().join("parent");
        let fresh_base = tmp.path().join("fresh");
        // The parent agent's live sandbox (root = parent_base/main).
        let parent = LocalProvider::new(&parent_base)
            .create_sandbox(&crate::provisioning::agent_run_sandbox_spec("main"))
            .await
            .unwrap();
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let fresh_provider = LocalProvider::new(&fresh_base);

        // Shared: the sub-run runs on the parent's sandbox — no new root is created
        // under the fresh provider's base (`默认共用`).
        run_configured_agent(
            &catalog,
            AgentRunSandbox::Shared(&parent),
            Arc::new(InstructionEchoModel),
            "assistant",
            "sub-a",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap();
        assert!(
            !fresh_base.join("sub-a").exists(),
            "a shared sub-run must not create its own sandbox root"
        );

        // Fresh: the sub-run creates its own isolated root under the provider base.
        run_configured_agent(
            &catalog,
            AgentRunSandbox::Fresh(&fresh_provider),
            Arc::new(InstructionEchoModel),
            "assistant",
            "sub-b",
            vec![user("go")],
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap();
        assert!(
            fresh_base.join("sub-b").exists(),
            "a fresh sub-run creates its own isolated sandbox root"
        );
    }
}
