//! Background context compaction: an ordinary sub-agent that summarizes the older
//! part of a long conversation, so future turns carry a short summary instead of
//! the full history.
//!
//! Like memory extraction it is "just an agent" (a `compactor` entry in the
//! [`AgentCatalog`]) run out-of-band through [`BackgroundRuns`] after a turn — it
//! never blocks the turn. Unlike memory it feeds a result back: when the summary
//! is ready it is delivered (as a system message prepended to the next turn) via a
//! caller-supplied closure, so the compactor stays decoupled from the host.
//!
//! Compaction is non-destructive: the committed transcript is never rewritten
//! (G13). The summary is *appended* and the raw older turns are dropped from the
//! model view by a [`ContextPolicy::KeepLast`](awaken_runtime_contract::resolved::ContextPolicy)
//! on the main agent. Summarize + truncate together bound the window without
//! erasing durable truth.

use std::future::Future;
use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_ext_compact::{COMPACT_AGENT_ID, SUMMARIZE_PROMPT, fold_point};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_sandbox_local::LocalSandboxProvider;

use crate::agent_catalog::AgentCatalog;
use crate::background::BackgroundRuns;
use crate::subagent::run_configured_subrun;

// The config pieces the host wires (registering the default compactor agent).
pub use awaken_ext_compact::{DEFAULT_COMPACT_INSTRUCTIONS, default_compact_agent};

/// Triggers out-of-band context compaction after a main turn.
pub struct Compaction {
    llm: Arc<dyn LlmExecutor>,
    provider: Arc<LocalSandboxProvider>,
    catalog: Arc<AgentCatalog>,
    background: Arc<BackgroundRuns>,
    /// Compact only once the committed message count exceeds this.
    threshold: usize,
    /// How many most-recent messages to leave out of the summarized slice (they
    /// stay verbatim in the window).
    keep_last: usize,
}

impl Compaction {
    pub fn new(
        llm: Arc<dyn LlmExecutor>,
        provider: Arc<LocalSandboxProvider>,
        catalog: Arc<AgentCatalog>,
        background: Arc<BackgroundRuns>,
        threshold: usize,
        keep_last: usize,
    ) -> Self {
        Self {
            llm,
            provider,
            catalog,
            background,
            threshold,
            keep_last,
        }
    }

    /// If `committed` exceeds the threshold, summarize the older slice (everything
    /// but the last `keep_last` messages) in the background and hand the summary to
    /// `deliver` when ready. Fire-and-forget; the run is drained at shutdown. No-op
    /// (does not spawn) when the conversation is short or the compactor is absent.
    pub async fn trigger<F, Fut>(&self, thread: &str, committed: Vec<Message>, deliver: F)
    where
        F: FnOnce(String) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if self.catalog.resolve(COMPACT_AGENT_ID).is_none() {
            return;
        }
        let Some(fold_to) = fold_point(committed.len(), self.threshold, self.keep_last) else {
            return;
        };
        let mut seed: Vec<Message> = committed.into_iter().take(fold_to).collect();
        seed.push(Message {
            id: MessageId(format!("{thread}-compact-prompt")),
            role: Role::User,
            content: vec![ContentBlock::text(SUMMARIZE_PROMPT)],
        });

        let llm = self.llm.clone();
        let provider = self.provider.clone();
        let catalog = self.catalog.clone();
        let compact_thread = format!("{thread}::compact");

        self.background
            .spawn(async move {
                match run_configured_subrun(
                    &catalog,
                    &provider,
                    llm,
                    COMPACT_AGENT_ID,
                    &compact_thread,
                    seed,
                    Vec::new(),
                    None,
                )
                .await
                {
                    Ok(summary) if !summary.trim().is_empty() => deliver(summary).await,
                    _ => {}
                }
            })
            .await;
    }

    /// The number of most-recent messages kept verbatim (the window size the main
    /// agent's [`ContextPolicy::KeepLast`] should mirror).
    pub fn keep_last(&self) -> usize {
        self.keep_last
    }

    /// Await in-flight compactions up to `timeout` (shutdown flush).
    pub async fn drain(&self, timeout: std::time::Duration) -> bool {
        self.background.drain(timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, ChatRole, Result as LlmResult,
    };
    use std::sync::Mutex;

    /// A stub compactor: replies with a fixed summary that names how many messages
    /// it was asked to fold (proving it received the older slice).
    struct SummaryModel;

    #[async_trait::async_trait]
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

    fn compaction(threshold: usize, keep_last: usize) -> Compaction {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Compaction::new(
            Arc::new(SummaryModel),
            Arc::new(LocalSandboxProvider::new(
                std::env::temp_dir().join(format!("awaken-compact-{stamp}")),
            )),
            Arc::new(
                AgentCatalog::new()
                    .with_agent(default_compact_agent("stub", DEFAULT_COMPACT_INSTRUCTIONS)),
            ),
            Arc::new(BackgroundRuns::new()),
            threshold,
            keep_last,
        )
    }

    #[tokio::test]
    async fn summarizes_the_older_slice_and_delivers_it() {
        let comp = compaction(3, 1);
        let delivered: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = delivered.clone();
        let committed: Vec<Message> = (0..5).map(user).collect();

        comp.trigger("t1", committed, move |summary| async move {
            *sink.lock().unwrap() = Some(summary);
        })
        .await;
        assert!(comp.drain(std::time::Duration::from_secs(10)).await);

        // 5 committed, keep_last 1 → folds 4, plus the compaction prompt = 5 users.
        assert_eq!(
            delivered.lock().unwrap().as_deref(),
            Some("summary of 5 messages")
        );
    }

    #[tokio::test]
    async fn short_conversations_are_not_compacted() {
        let comp = compaction(10, 1);
        let delivered: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = delivered.clone();
        let committed: Vec<Message> = (0..4).map(user).collect();

        comp.trigger("t1", committed, move |summary| async move {
            *sink.lock().unwrap() = Some(summary);
        })
        .await;
        assert!(comp.drain(std::time::Duration::from_secs(2)).await);
        assert!(delivered.lock().unwrap().is_none());
    }
}
