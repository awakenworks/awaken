//! The goal judge as an ordinary, configurable agent.
//!
//! The outcome loop (`awaken-ext-goal`) grades a deliverable through a
//! `DelegateRunner`; here that runner resolves a `judge` entry from an
//! [`AgentCatalog`] and runs it through the shared Agent Run substrate, exactly like
//! the memory and compact agents. So the judge's model, instructions, and window
//! are configured per-agent rather than hard-coded — the same "just an agent"
//! substrate, now covering evaluation too.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::runnable::RunnableConfig;
use awaken_runtime_contract::subagent_runner::{
    SubagentError, SubagentReply, SubagentRequest, SubagentRunner,
};
use awaken_sandbox_local::LocalProvider;

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

/// Runs a judge sub-agent through the kernel for a
/// [`DelegateGrader`](awaken_ext_goal::DelegateGrader): a fresh rooted runtime
/// over the same model, driven to completion; its last assistant line is the
/// judge's reply. The judge sees only its prompt (a fresh window), so its
/// verdict is not biased by the doer's working state.
pub(crate) struct HostSubagentRunner {
    pub(crate) llm: Arc<dyn LlmExecutor>,
    pub(crate) provider: LocalProvider,
    /// Aux agents (judge, and — as D5 lands — compactor/memory) are resolved by id
    /// from here, so their model/instructions/window are configured per-agent.
    pub(crate) catalog: Arc<AgentCatalog>,
    pub(crate) seq: AtomicU64,
}

#[async_trait::async_trait]
impl SubagentRunner for HostSubagentRunner {
    async fn run(&self, request: SubagentRequest) -> Result<SubagentReply, SubagentError> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        // The sub-agent sees only its seed (a fresh window); its cancellation is the
        // parent run's, so cancelling the parent cancels the sub-run too.
        let name = format!("{}-sub-{n}", request.agent_id);
        // Every sub-run behind this port is out-of-band housekeeping (judge,
        // compaction, memory selection), not the doer's turn — its usage stays
        // isolated on its own sub-thread rather than folding into the parent tally.
        let (text, _usage) = crate::subagent::run_configured_agent(
            &self.catalog,
            crate::subagent::AgentRunSandbox::Fresh(&self.provider),
            self.llm.clone(),
            &request.agent_id,
            &name,
            request.seed,
            Vec::new(),
            request.cancellation,
            None,
            None,
            None,
            None,
            crate::subagent::UsageRollup::Isolated,
        )
        .await
        .map_err(SubagentError)?;
        Ok(SubagentReply { text: Some(text) })
    }
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
