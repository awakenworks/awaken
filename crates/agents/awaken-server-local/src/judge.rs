//! The goal judge as an ordinary, configurable agent.
//!
//! The outcome loop (`awaken-ext-goal`) grades a deliverable through a
//! `DelegateRunner`; here that runner resolves a `judge` entry from an
//! [`AgentCatalog`] and runs it through [`run_configured_subrun`], exactly like
//! the memory and compact agents. So the judge's model, instructions, and window
//! are configured per-agent rather than hard-coded — the same "just an agent"
//! substrate, now covering evaluation too.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_ext_goal::{DelegateError, DelegateReply, DelegateRequest, DelegateRunner};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_sandbox_local::LocalSandboxProvider;

use crate::agent_catalog::AgentCatalog;

/// Default judge instructions. The outcome loop supplies the goal, rubric, and
/// deliverable in the prompt; the judge returns a JSON verdict the grader parses.
pub const DEFAULT_JUDGE_INSTRUCTIONS: &str = "\
You are a strict evaluator. You are given a goal, its rubric, and a deliverable. \
Judge whether the deliverable satisfies the rubric. Reply with ONLY a JSON object \
of the form {\"result\": \"satisfied\" | \"needs_revision\", \"explanation\": \"...\"} \
and nothing else.";

/// A default judge agent config registered under `agent_id`: no tools, a fresh
/// grading window. A host may override by registering its own config for the id.
pub fn default_judge_agent(model_ref: &str, agent_id: &str, instructions: &str) -> RunnableConfig {
    RunnableConfig::builder(agent_id)
        .instructions(instructions)
        .model(ModelBinding::new("default", model_ref, "default"))
        .max_steps(2)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_judge_agent_carries_its_id_and_instructions() {
        let cfg = default_judge_agent("stub", "judge", DEFAULT_JUDGE_INSTRUCTIONS);
        assert_eq!(cfg.snapshot().root_agent_id.0, "judge");
        assert!(
            cfg.snapshot()
                .resolved_spec
                .instructions
                .contains("strict evaluator")
        );
        // A judge is pure reasoning: no tools.
        assert!(cfg.snapshot().resolved_spec.tool_descriptors.is_empty());
    }
}

/// Runs a judge sub-agent through the kernel for a
/// [`DelegateGrader`](awaken_ext_goal::DelegateGrader): a fresh rooted runtime
/// over the same model, driven to completion; its last assistant line is the
/// judge's reply. The judge sees only its prompt (a fresh window), so its
/// verdict is not biased by the doer's working state.
pub(crate) struct KernelJudgeRunner {
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) provider: LocalSandboxProvider,
    /// The judge is resolved by id from here, so its model/instructions/window are
    /// configured per-agent rather than hard-coded (like memory and compact).
    pub(crate) catalog: Arc<AgentCatalog>,
    pub(crate) seq: AtomicU64,
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
