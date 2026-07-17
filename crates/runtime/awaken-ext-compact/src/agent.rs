//! The compactor agent: id, summary instructions, its default config, and the
//! per-run summarize prompt.

use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;

/// The agent id under which the compactor is registered.
pub const COMPACT_AGENT_ID: &str = "compactor";

/// Default compaction instructions. A host may override by registering its own
/// `compactor` config.
///
/// The two explicit DROP rules (superseded decisions, completed work) are not
/// decoration: `scripts/runtime-prompt-eval.py` showed the terse original kept
/// superseded/finished detail ~35% of the time on the weakest model (drop_rate 65%),
/// which misleads a continuation about which decision is final. Naming the two cases
/// lifted drop_rate to 88% with preserve_recall unchanged at 98% (K=8, adversarial
/// fixtures incl. a reversed decision). Kept short on purpose — length dilutes the gate.
pub const DEFAULT_COMPACT_INSTRUCTIONS: &str = "\
You are a conversation-compaction Agent. Summarize the earlier conversation into the \
durable facts a continuation needs: decisions still in force, active constraints, \
UNRESOLVED open questions, and results that still matter. Preserve every such fact — do \
not lose them.\n\n\
Two hard DROP rules:\n\
1. If a decision was later changed or reversed, keep ONLY the final choice — never mention \
the superseded one.\n\
2. If work was already completed and needs no follow-up, leave it out — never restate a \
finished fix or task as if it were still pending.\n\
Also drop small talk and any line that would not change what the next turn does.\n\n\
Reply with only the summary text.";

/// The per-run user prompt appended to the seeded (older) slice.
pub const SUMMARIZE_PROMPT: &str = "Summarize the conversation above per your instructions.";

/// A default `compactor` agent config: no tools, a summary-only prompt.
pub fn default_compact_agent(model_ref: &str, instructions: &str) -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder(COMPACT_AGENT_ID)
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
        assert_eq!(cfg.root_agent_id.0, COMPACT_AGENT_ID);
        let spec = &cfg.resolved_spec;
        assert!(spec.instructions.contains("conversation-compaction Agent"));
        assert!(spec.tool_descriptors.is_empty());
    }
}
