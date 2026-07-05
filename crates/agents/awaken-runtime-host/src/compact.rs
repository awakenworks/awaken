//! Context compaction: the composition-root wiring.
//!
//! The policy half — when to fold ([`fold_point`]), the compactor agent's config
//! and prompts, and the recall-symmetric [`CompactPlugin`] that injects the summary
//! as request-only context — lives in `awaken-ext-compact` (a bounded context).
//! This module wires that onto the host's aux-agent substrate: an [`AgentSummarizer`]
//! that summarizes the older slice through an ordinary `compactor` sub-agent, run at
//! `BeforeInference` (once per run, cached by the plugin), the same shape as memory's
//! [`AgentSelector`](crate::memory::AgentSelector).
//!
//! Compaction is non-destructive: the committed transcript is never rewritten (G13).
//! The summary is injected request-only and the raw older turns drop from the model
//! view via a [`ContextPolicy::KeepLast`](awaken_runtime_contract::resolved::ContextPolicy)
//! on the main agent. Summarize + truncate together bound the window without erasing
//! durable truth.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_compact::{COMPACT_AGENT_ID, SUMMARIZE_PROMPT, Summarizer};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_sandbox_local::LocalSandboxProvider;

use crate::agent_catalog::AgentCatalog;
use crate::subagent::run_configured_subrun;

// The config pieces the host wires (registering the default compactor agent).
pub use awaken_ext_compact::{DEFAULT_COMPACT_INSTRUCTIONS, default_compact_agent};

/// A [`Summarizer`] backed by the `compactor` sub-agent: it seeds the agent with the
/// older slice plus the summarize prompt and returns its reply as the summary. Run
/// synchronously by the compaction plugin at `BeforeInference` (cached once per run),
/// the read-side counterpart of memory's `AgentSelector`.
pub(crate) struct AgentSummarizer {
    llm: Arc<dyn LlmExecutor>,
    provider: Arc<LocalSandboxProvider>,
    catalog: Arc<AgentCatalog>,
    seq: AtomicU64,
}

impl AgentSummarizer {
    pub(crate) fn new(llm: Arc<dyn LlmExecutor>, model_ref: &str) -> Self {
        let catalog = Arc::new(AgentCatalog::new().with_agent(default_compact_agent(
            model_ref,
            DEFAULT_COMPACT_INSTRUCTIONS,
        )));
        let base = std::env::temp_dir()
            .join("awaken-server-local")
            .join(format!("{}-compact", std::process::id()));
        Self {
            llm,
            provider: Arc::new(LocalSandboxProvider::new(base)),
            catalog,
            seq: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl Summarizer for AgentSummarizer {
    async fn summarize(&self, older: &[Message]) -> Option<String> {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let mut seed: Vec<Message> = older.to_vec();
        seed.push(Message {
            id: MessageId(format!("compact-prompt-{n}")),
            role: Role::User,
            content: vec![ContentBlock::text(SUMMARIZE_PROMPT)],
        });
        let summary = run_configured_subrun(
            &self.catalog,
            &self.provider,
            self.llm.clone(),
            COMPACT_AGENT_ID,
            &format!("compact-{n}"),
            seed,
            Vec::new(),
            None,
        )
        .await
        .ok()?;
        (!summary.trim().is_empty()).then_some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, ChatRole, Result as LlmResult,
    };

    /// A stub compactor: replies with a fixed summary that names how many messages
    /// it was asked to fold (proving it received the older slice plus the prompt).
    struct SummaryModel;

    #[async_trait]
    impl LlmExecutor for SummaryModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            let user_msgs = request
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::User)
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
    async fn summarizes_the_older_slice_it_is_given() {
        let summarizer = AgentSummarizer::new(Arc::new(SummaryModel), "stub");
        let older: Vec<Message> = (0..4).map(user).collect();
        // 4 older messages + the appended summarize prompt = 5 user messages.
        assert_eq!(
            summarizer.summarize(&older).await.as_deref(),
            Some("summary of 5 messages")
        );
    }
}
