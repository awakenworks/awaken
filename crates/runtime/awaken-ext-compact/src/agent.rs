//! The compactor agent: id, summary instructions, its default config, and the
//! per-run summarize prompt.

use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::runnable::RunnableConfig;

/// The agent id under which the compactor is registered.
pub const COMPACT_AGENT_ID: &str = "compactor";

/// Default compaction instructions. A host may override by registering its own
/// `compactor` config.
pub const DEFAULT_COMPACT_INSTRUCTIONS: &str = "\
You are a conversation-compaction Agent. You are given the earlier part of a \
conversation that is about to be dropped from the working context. Write a concise \
summary that preserves the durable facts a continuation needs: decisions made, \
constraints, open questions, and important results. Omit small talk and \
already-resolved detail. Reply with only the summary text.";

/// The per-run user prompt appended to the seeded (older) slice.
pub const SUMMARIZE_PROMPT: &str = "Summarize the conversation above per your instructions.";

/// A default `compactor` agent config: no tools, a summary-only prompt.
pub fn default_compact_agent(model_ref: &str, instructions: &str) -> RunnableConfig {
    RunnableConfig::builder(COMPACT_AGENT_ID)
        .instructions(instructions)
        .model(ModelBinding::new("default", model_ref, "default"))
        .max_steps(2)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_carries_id_instructions_and_no_tools() {
        let cfg = default_compact_agent("stub", DEFAULT_COMPACT_INSTRUCTIONS);
        assert_eq!(cfg.snapshot().root_agent_id.0, COMPACT_AGENT_ID);
        let spec = &cfg.snapshot().resolved_spec;
        assert!(spec.instructions.contains("conversation-compaction Agent"));
        assert!(spec.tool_descriptors.is_empty());
    }
}
