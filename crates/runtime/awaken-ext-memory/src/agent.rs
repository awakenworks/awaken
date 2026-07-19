//! The memory-extractor agent: id, instructions (Claude Code's memory taxonomy),
//! its default config, and the per-run extraction prompt.

use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::runnable::RunnableConfig;

use crate::tool::write_memory_descriptor;

/// The agent id under which the memory extractor is registered.
pub const MEMORY_AGENT_ID: &str = "memory-extractor";

/// Default extraction instructions. Adapted from Claude Code's memory taxonomy
/// (four types + a what-NOT-to-save gate), mapped onto the single-file
/// `write_memory` tool.
pub const DEFAULT_MEMORY_INSTRUCTIONS: &str = "\
You are the memory extraction sub-agent. Analyze the conversation you are given \
and update a persistent memory so future conversations understand who the user \
is, how they want you to work, and the context behind their tasks.\n\n\
## Types of memory to save\n\
- user: the user's role, goals, responsibilities, preferences, and knowledge — \
so you can tailor future behavior to them specifically.\n\
- feedback: guidance on how to approach work — corrections (\"no, not that\", \
\"stop doing X\") AND confirmations (\"yes, exactly\"). Lead with the rule, then a \
Why: line (the reason given) and a How to apply: line (when it kicks in).\n\
- project: ongoing work, goals, decisions, or incidents not derivable from the \
code or git history. Convert relative dates to absolute. Lead with the fact, \
then Why: and How to apply: lines.\n\
- reference: pointers to where information lives in external systems (a Linear \
project, a Slack channel, a dashboard) and their purpose.\n\n\
## What NOT to save\n\
- Code patterns, conventions, architecture, file paths, project structure — \
derivable by reading the project.\n\
- Git history or who-changed-what — git log/blame are authoritative.\n\
- Debugging solutions or fix recipes — the fix is in the code.\n\
- Ephemeral task state, current-conversation context, or anything trivial or \
easily re-derived.\n\n\
## How to save\n\
Save each memory with the write_memory tool: a short kebab-case slug name and \
the memory text. Prefer one memory per distinct fact. Be specific — the text is \
what a future conversation reads. When done, reply with a one-line summary of \
what you saved (or that nothing was worth saving).";

/// The per-run user prompt appended to the seeded conversation.
pub const EXTRACT_PROMPT: &str =
    "Extract durable memories from the conversation above and save each via write_memory.";

/// A default `memory-extractor` agent config: advertises only `write_memory` and
/// carries the extraction instructions. A host may override by registering its own
/// config under [`MEMORY_AGENT_ID`].
pub fn default_memory_agent(model_ref: &str, instructions: &str) -> RunnableConfig {
    RunnableConfig::builder(MEMORY_AGENT_ID)
        .instructions(instructions)
        .model(ModelBinding::new("default", model_ref, "default"))
        .max_steps(6)
        .tools([write_memory_descriptor()])
        .build()
}

/// The agent id of the relevance selector (a single-step, tool-free agent).
pub const SELECTOR_AGENT_ID: &str = "memory-selector";

/// Default selector instructions. The host seeds the query + memory manifest as
/// input; the agent replies with the relevant bracketed indices.
pub const DEFAULT_SELECTOR_INSTRUCTIONS: &str = "\
You select which of a user's saved memories are relevant to their current message. \
Reply with ONLY the bracketed indices of the relevant memories (e.g. `[0], [3]`), \
comma-separated, at most the requested count. If none are relevant, reply NONE. \
Do not explain, do not use tools.";

/// A default `memory-selector` agent config: no tools, a single step (it replies
/// once), and no plugins — its Agent config therefore cannot invoke memory recall.
pub fn default_selector_agent(model_ref: &str, instructions: &str) -> RunnableConfig {
    RunnableConfig::builder(SELECTOR_AGENT_ID)
        .instructions(instructions)
        .model(ModelBinding::new("default", model_ref, "default"))
        .max_steps(1)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_agent_carries_id_instructions_and_the_write_tool() {
        let cfg = default_memory_agent("stub", DEFAULT_MEMORY_INSTRUCTIONS);
        assert_eq!(cfg.snapshot().root_agent_id.0, MEMORY_AGENT_ID);
        let spec = &cfg.snapshot().resolved_spec;
        assert!(spec.instructions.contains("memory extraction sub-agent"));
        assert_eq!(spec.tool_descriptors.len(), 1);
        assert_eq!(spec.tool_descriptors[0].id, "write_memory");
    }
}
