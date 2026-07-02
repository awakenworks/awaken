//! Running a fresh, isolated sub-agent to completion.
//!
//! Both native delegation (`agent_run`) and the goal judge run a sub-agent the
//! same way: a fresh sandbox, a runtime over the same model with *no* delegation
//! resolver (so a sub-agent cannot recurse), driven to completion, returning its
//! last assistant line. This is that one operation, shared by both callers.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_sandbox_local::{LocalSandboxProvider, SandboxProvider, SandboxSpec};

use crate::config::{build_runtime, latest_assistant_text, server_config};

/// Run a sub-agent named `name` with `input` to completion and return its last
/// assistant line. `cancellation`, when set, is forwarded so cancelling the parent
/// cancels the sub-run too. Errors are returned as strings for the caller to wrap.
pub(crate) async fn run_subagent(
    llm: Arc<dyn LlmExecutor>,
    model_ref: &str,
    provider: &LocalSandboxProvider,
    name: &str,
    input: &str,
    cancellation: Option<CancellationToken>,
) -> Result<String, String> {
    let env = provider
        .create(&SandboxSpec::new(name))
        .await
        .map_err(|e| e.to_string())?;
    let runtime = build_runtime(llm, &env);
    // A judge/native sub-agent does not offer skills (ADR-0036).
    let config = server_config(model_ref, &HashSet::new(), &HashSet::new(), &[], &[]);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let mut ctx = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_reader(commit.clone());
    if let Some(token) = cancellation {
        ctx = ctx.with_cancellation(token);
    }
    runtime
        .run_to_completion(&config, name, input, ctx, |_| ResumeResult::allow())
        .await
        .map_err(|e| e.to_string())?;
    Ok(latest_assistant_text(
        &commit.committed_messages(&ThreadId(name.to_string())),
    ))
}
