//! Running a fresh, isolated sub-agent to completion.
//!
//! Both native delegation (`agent_run`) and the goal judge run a sub-agent the
//! same way: a fresh sandbox, a runtime over the same model with *no* delegation
//! resolver (so a sub-agent cannot recurse), driven to completion, returning its
//! last assistant line. This is that one operation.
//!
//! [`run_configured_subrun`] is the parameterized form: it resolves the agent's
//! own `RunnableConfig` (instructions, model, tools) from an [`AgentCatalog`] by
//! id, so each sub-run carries its own configuration rather than a single shared
//! one. [`run_subagent`] is the thin back-compat wrapper that runs the default
//! `assistant` config with a plain string prompt.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::RunInput;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::{LocalSandboxProvider, SandboxProvider, SandboxSpec};

use crate::agent_catalog::AgentCatalog;
use crate::config::{build_runtime, latest_assistant_text, server_config};

/// Run the agent identified by `agent_id` — its config resolved from `catalog` —
/// on an isolated `thread`, seeded with `seed`, to completion, returning its last
/// assistant line. The sub-run gets a fresh sandbox, a runtime over `llm` with no
/// delegation resolver (no recursion), and an isolated commit store (its history
/// never joins the parent transcript). `cancellation`, when set, is forwarded so
/// cancelling the parent cancels the sub-run too.
///
/// Errors (unknown agent, sandbox/runtime failure) are returned as strings for
/// the caller to wrap.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_subrun(
    catalog: &AgentCatalog,
    provider: &LocalSandboxProvider,
    llm: Arc<dyn LlmExecutor>,
    agent_id: &str,
    thread: &str,
    seed: impl Into<RunInput>,
    extra_tools: Vec<Arc<dyn RawTool>>,
    cancellation: Option<CancellationToken>,
) -> Result<String, String> {
    let config = catalog
        .resolve(agent_id)
        .ok_or_else(|| format!("unknown agent {agent_id:?}"))?
        .clone();
    let env = provider
        .create(&SandboxSpec::new(thread))
        .await
        .map_err(|e| e.to_string())?;
    let mut runtime = build_runtime(llm, &env);
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
    runtime
        .run_to_completion(&config, thread, seed, ctx, |_| ResumeResult::allow())
        .await
        .map_err(|e| e.to_string())?;
    Ok(latest_assistant_text(
        &commit.committed_messages(&ThreadId(thread.to_string())),
    ))
}

/// Run a default `assistant` sub-agent named `name` with `input` to completion and
/// return its last assistant line. Thin wrapper over [`run_configured_subrun`]: it
/// builds a one-entry catalog holding the shared `assistant` config (no skills,
/// ADR-0036). Callers that need a differently-configured agent resolve it from a
/// richer catalog through [`run_configured_subrun`] instead.
pub(crate) async fn run_subagent(
    llm: Arc<dyn LlmExecutor>,
    model_ref: &str,
    provider: &LocalSandboxProvider,
    name: &str,
    input: &str,
    cancellation: Option<CancellationToken>,
) -> Result<String, String> {
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
        provider,
        llm,
        "assistant",
        name,
        input,
        Vec::new(),
        cancellation,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, ChatRole, Result as LlmResult,
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
                .find(|m| m.role == ChatRole::System)
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
        let provider = LocalSandboxProvider::new(&base);
        let catalog = AgentCatalog::new()
            .with_agent(agent("memory-extractor", "MEMORY INSTRUCTIONS"))
            .with_agent(agent("judge", "JUDGE INSTRUCTIONS"));
        let llm = Arc::new(InstructionEchoModel);

        let mem = run_configured_subrun(
            &catalog,
            &provider,
            llm.clone(),
            "memory-extractor",
            "t-mem",
            vec![user("go")],
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(mem, "MEMORY INSTRUCTIONS");

        let judge = run_configured_subrun(
            &catalog,
            &provider,
            llm.clone(),
            "judge",
            "t-judge",
            vec![user("go")],
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(judge, "JUDGE INSTRUCTIONS");
    }

    #[tokio::test]
    async fn unknown_agent_is_an_error() {
        let base = std::env::temp_dir().join("awaken-subrun-test-unknown");
        let provider = LocalSandboxProvider::new(&base);
        let catalog = AgentCatalog::new().with_agent(agent("assistant", "hi"));
        let err = run_configured_subrun(
            &catalog,
            &provider,
            Arc::new(InstructionEchoModel),
            "nope",
            "t",
            vec![user("go")],
            Vec::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("unknown agent"), "got: {err}");
    }
}
