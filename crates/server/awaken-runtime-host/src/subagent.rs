//! Running a sub-agent to completion.
//!
//! Native delegation (`agent_run`), the goal judge, and skill forks run a sub-agent
//! the same way: a sandbox ([shared with the parent by default](SubrunSandbox), or a
//! fresh isolated one), a runtime over the same model with *no* delegation resolver
//! (so a sub-agent cannot recurse), driven to completion, returning its last
//! assistant line. This is that one operation.
//!
//! [`run_configured_subrun`] is the parameterized form: it resolves the agent's
//! own `RunnableConfig` (instructions, model, tools) from an [`AgentCatalog`] by
//! id, so each sub-run carries its own configuration rather than a single shared
//! one. [`run_subagent`] is the thin back-compat wrapper that runs the default
//! `assistant` config with a plain string prompt.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_runtime::RunInput;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::llm::{LlmExecutor, ThreadUsage};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

use crate::agent_catalog::AgentCatalog;
use crate::config::{build_runtime, latest_assistant_text, server_config};

/// Where a sub-run's tools execute. A subagent **shares the parent agent's sandbox by
/// default** ([`Shared`](SubrunSandbox::Shared)) — same root, same workspace, so the
/// two collaborate on one set of files — or runs in a **fresh, isolated** sandbox
/// ([`Fresh`](SubrunSandbox::Fresh)) when the subagent is configured for isolation or
/// no parent sandbox exists (out-of-band housekeeping: judge / memory / compaction).
pub(crate) enum SubrunSandbox<'a> {
    /// Reuse the parent agent's live sandbox — the default (`与主 agent 共用`).
    Shared(&'a LocalSandbox),
    /// Create a fresh, isolated sandbox for this sub-run via the given provider.
    Fresh(&'a LocalProvider),
}

/// What becomes of an auxiliary sub-run's token usage. A sub-run commits its usage
/// to its own isolated store (dropped when the run returns), so the caller must
/// choose whether that tally folds into the parent thread's total or stays isolated.
/// Making this an explicit argument consumed at the single sub-run seam keeps the
/// fold decision from being an implicit `let _usage` scattered across call sites: a
/// turn-work sub-run (native delegation) folds; out-of-band housekeeping (judge,
/// memory extraction/selection, compaction, skill activation) stays isolated and is
/// never counted against the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageRollup {
    /// Fold the sub-run's usage into the parent thread's tally — the caller threads
    /// the returned [`ThreadUsage`] back through its step.
    FoldIntoParent,
    /// Keep the sub-run's usage on its own sub-thread: the returned tally is empty,
    /// so the sub-run's tokens cannot be folded into the session total.
    Isolated,
}

/// Run the agent identified by `agent_id` — its config resolved from `catalog` —
/// on an isolated `thread`, seeded with `seed`, to completion, returning its last
/// assistant line **and its accumulated token usage**. The sub-run runs on `sandbox`
/// (the parent's, shared, or a fresh one), a runtime over `llm` with no delegation
/// resolver (no recursion), and an isolated commit store (history never joins the
/// parent transcript).
/// `cancellation`, when set, is forwarded so cancelling the parent cancels the
/// sub-run too.
///
/// The returned [`ThreadUsage`] follows `rollup`: under
/// [`FoldIntoParent`](UsageRollup::FoldIntoParent) it is read from the sub-run's own
/// committed thread state before its isolated store is dropped, so the caller can
/// fold it into a parent thread's total (otherwise the delegate's tokens would vanish
/// with the store); under [`Isolated`](UsageRollup::Isolated) the tally is dropped at
/// this seam and the return is empty, so an out-of-band sub-run's tokens are never
/// counted against the session. Also empty when the model reported no usage.
///
/// Errors (unknown agent, sandbox/runtime failure) are returned as strings for
/// the caller to wrap.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_subrun(
    catalog: &AgentCatalog,
    sandbox: SubrunSandbox<'_>,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    seed: impl Into<RunInput>,
    extra_tools: Vec<Arc<dyn RawTool>>,
    cancellation: Option<CancellationToken>,
    rollup: UsageRollup,
) -> Result<(String, ThreadUsage), String> {
    let config = catalog
        .resolve(agent_id)
        .ok_or_else(|| format!("unknown agent {agent_id:?}"))?
        .clone();
    // Reuse the parent's sandbox by default; a `Fresh` sub-run gets its own root. The
    // created sandbox (if any) is bound here so the borrow lives for the whole run.
    let created = match &sandbox {
        SubrunSandbox::Fresh(provider) => Some(
            provider
                .create_sandbox(&crate::provisioning::subrun_sandbox_spec(thread))
                .await
                .map_err(|e| e.to_string())?,
        ),
        SubrunSandbox::Shared(_) => None,
    };
    let env: &LocalSandbox = match &sandbox {
        SubrunSandbox::Shared(shared) => shared,
        SubrunSandbox::Fresh(_) => created.as_ref().expect("a Fresh sub-run created a sandbox"),
    };
    let mut runtime = build_runtime(llm, env);
    // Tools the caller provisions on top of the sandbox's own (e.g. a
    // persistent write_memory scoped outside the ephemeral sandbox).
    for tool in extra_tools {
        runtime = runtime.with_tool(tool);
    }
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let mut ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());
    if let Some(token) = cancellation {
        ctx = ctx.with_cancellation(token);
    }
    let thread_id = ThreadId(thread.to_string());
    runtime
        .run_to_completion(&config, thread, seed, ctx, |_| ResumeResult::allow())
        .await
        .map_err(|e| e.to_string())?;
    let text = latest_assistant_text(&commit.committed_messages(&thread_id));
    // The fold decision lives here, at the single seam: an isolated sub-run's tally
    // is dropped so no caller can fold housekeeping tokens into the session total.
    let usage = match rollup {
        UsageRollup::FoldIntoParent => usage_from_committed(&commit, &thread_id),
        UsageRollup::Isolated => ThreadUsage::default(),
    };
    Ok((text, usage))
}

/// Read a finished sub-run's accumulated [`ThreadUsage`] out of its committed
/// thread state. The run loop writes the running cumulative under
/// `THREAD_USAGE_STATE_KEY` each step, so the last `Set` is the whole tally
/// (mirrors `SharedHost::thread_usage`, but over the sub-run's isolated commit).
fn usage_from_committed(commit: &MemoryCommitCoordinator, thread_id: &ThreadId) -> ThreadUsage {
    ThreadUsage::from_committed_state(&commit.committed_state(thread_id))
}

/// Run a default `assistant` sub-agent named `name` with `input` to completion and
/// return its last assistant line. Thin wrapper over [`run_configured_subrun`]: it
/// builds a one-entry catalog holding the shared `assistant` config (no skills,
/// ADR-0036). Callers that need a differently-configured agent resolve it from a
/// richer catalog through [`run_configured_subrun`] instead.
pub(crate) async fn run_subagent(
    llm: Arc<dyn LlmExecutor>,
    model_ref: &str,
    sandbox: SubrunSandbox<'_>,
    name: &str,
    input: impl Into<RunInput>,
    cancellation: Option<CancellationToken>,
    rollup: UsageRollup,
) -> Result<(String, ThreadUsage), String> {
    // A judge/native sub-agent does not offer skills (ADR-0036).
    let config = server_config(
        model_ref,
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let catalog = AgentCatalog::new().with_agent(config);
    run_configured_subrun(
        &catalog,
        sandbox,
        llm,
        "assistant",
        name,
        input,
        Vec::new(),
        cancellation,
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

        let (mem, _) = run_configured_subrun(
            &catalog,
            SubrunSandbox::Fresh(&provider),
            llm.clone(),
            "memory-extractor",
            "t-mem",
            vec![user("go")],
            Vec::new(),
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap();
        assert_eq!(mem, "MEMORY INSTRUCTIONS");

        let (judge, _) = run_configured_subrun(
            &catalog,
            SubrunSandbox::Fresh(&provider),
            llm.clone(),
            "judge",
            "t-judge",
            vec![user("go")],
            Vec::new(),
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
    async fn subrun_returns_its_accumulated_usage() {
        let base = std::env::temp_dir().join(format!(
            "awaken-subrun-usage-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = LocalProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("worker", "WORK"));

        let (_text, usage) = run_configured_subrun(
            &catalog,
            SubrunSandbox::Fresh(&provider),
            Arc::new(UsageModel),
            "worker",
            "t-usage",
            vec![user("go")],
            Vec::new(),
            None,
            UsageRollup::FoldIntoParent,
        )
        .await
        .unwrap();

        // The sub-run's model reported 13/5 on its one step; that must survive the
        // isolated store's drop as the returned tally.
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
    async fn isolated_rollup_drops_the_subruns_usage() {
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

        let (_text, usage) = run_configured_subrun(
            &catalog,
            SubrunSandbox::Fresh(&provider),
            Arc::new(UsageModel),
            "worker",
            "t-isolated",
            vec![user("go")],
            Vec::new(),
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
        let err = run_configured_subrun(
            &catalog,
            SubrunSandbox::Fresh(&provider),
            Arc::new(InstructionEchoModel),
            "nope",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
            UsageRollup::Isolated,
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[tokio::test]
    async fn a_shared_subrun_reuses_the_parent_sandbox_while_fresh_makes_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let parent_base = tmp.path().join("parent");
        let fresh_base = tmp.path().join("fresh");
        // The parent agent's live sandbox (root = parent_base/main).
        let parent = LocalProvider::new(&parent_base)
            .create_sandbox(&crate::provisioning::subrun_sandbox_spec("main"))
            .await
            .unwrap();
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let fresh_provider = LocalProvider::new(&fresh_base);

        // Shared: the sub-run runs on the parent's sandbox — no new root is created
        // under the fresh provider's base (`默认共用`).
        run_configured_subrun(
            &catalog,
            SubrunSandbox::Shared(&parent),
            Arc::new(InstructionEchoModel),
            "assistant",
            "sub-a",
            vec![user("go")],
            Vec::new(),
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
        run_configured_subrun(
            &catalog,
            SubrunSandbox::Fresh(&fresh_provider),
            Arc::new(InstructionEchoModel),
            "assistant",
            "sub-b",
            vec![user("go")],
            Vec::new(),
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
