//! Context compaction: the composition-root wiring.
//!
//! The policy half — when to fold ([`fold_point`]), the compactor agent's config
//! and prompts, and the recall-symmetric [`CompactPlugin`] that injects the summary
//! as request-only context — lives in `awaken-ext-compact` (a bounded context).
//! This module wires that onto the host's aux-agent substrate: [`compact_runner`]
//! builds an ordinary Agent-backed tool over the `compactor` Agent,
//! also used by the goal judge). The plugin seeds it with the older slice at
//! `BeforeInference` (once per run, cached), the same shape as memory's
//! [`AgentSelector`](crate::memory::AgentSelector).
//!
//! Compaction is non-destructive: the committed transcript is never rewritten (G13).
//! The summary is injected request-only and the raw older turns drop from the model
//! view via a [`ContextPolicy::KeepLast`](awaken_runtime_contract::resolved::ContextPolicy)
//! on the main agent. Summarize + truncate together bound the window without erasing
//! durable truth.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::judge::HostAgentTool;

// The config pieces the host wires (registering the default compactor agent).
pub use awaken_ext_compact::{DEFAULT_COMPACT_INSTRUCTIONS, default_compact_agent};

/// A host's enabled compaction: the resolved config plus the `compactor` sub-agent
/// runner that applies it. Sealed as one type so the pair is present-or-absent
/// atomically — `SharedHost` holds a single `Option<Compaction>`, replacing the two
/// separate `Option`s whose tuple-match had to fold three impossible mixes
/// (`config` without `runner`, and the reverse) into one catch-all arm.
pub(crate) struct Compaction {
    pub(crate) config: awaken_ext_compact::CompactConfig,
    pub(crate) agent_tool: Arc<dyn RawTool>,
}

/// An ordinary tool whose catalog holds the `compactor` Agent.
/// (ADR-0047 D5). The compaction plugin builds the seed (older slice + summarize
/// prompt); this runs the named agent to completion, the read-side counterpart of
/// memory's `AgentSelector`.
pub(crate) fn compact_runner(llm: Arc<dyn LlmExecutor>, model_ref: &str) -> Arc<dyn RawTool> {
    let catalog = Arc::new(AgentCatalog::new().with_agent(default_compact_agent(
        model_ref,
        DEFAULT_COMPACT_INSTRUCTIONS,
    )));
    let base = std::env::temp_dir()
        .join("awaken-server")
        .join(format!("{}-compact", std::process::id()));
    Arc::new(HostAgentTool {
        llm,
        provider: LocalProvider::new(base),
        catalog,
        seq: AtomicU64::new(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_ext_builtin_tools::{AgentRunArgs, invoke_agent_tool};
    use awaken_ext_compact::COMPACT_AGENT_ID;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult,
    };

    /// A stub compactor: replies with a fixed summary that names how many messages
    /// it was asked to fold (proving it received the seed the plugin built).
    struct SummaryModel;

    #[async_trait]
    impl LlmExecutor for SummaryModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let user_msgs = request
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .count();
            Ok(ChatResponse {
                output: AssistantOutput::text(format!("summary of {user_msgs} messages")),
                usage: None,
                stop_reason: None,
            })
        }
    }

    fn user(n: usize) -> Message {
        Message {
            id: MessageId(format!("m{n}")),
            role: Role::User,
            content: vec![ContentBlock::text(format!("msg {n}"))],
        }
    }

    #[tokio::test]
    async fn compact_runner_runs_the_compactor_on_its_seed() {
        let runner = compact_runner(Arc::new(SummaryModel), "stub");
        // The plugin builds the seed (older slice + summarize prompt); here that is
        // 5 user messages.
        let seed: Vec<Message> = (0..5).map(user).collect();
        let reply = invoke_agent_tool(
            runner.as_ref(),
            "compact-test",
            AgentRunArgs {
                agent_id: COMPACT_AGENT_ID.to_string(),
                seed,
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(reply.content, "summary of 5 messages");
    }
}
