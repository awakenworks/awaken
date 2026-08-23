//! Context compaction: the process-startup wiring.
//!
//! The policy half — when to fold ([`fold_point`]), the compactor agent's config
//! and prompts, and the recall-symmetric [`CompactPlugin`] that injects the summary
//! as request-only context — lives in `awaken-ext-compact` (a bounded context).
//! This module wires that onto the host's aux-agent substrate: [`compact_runner`]
//! builds an ordinary stable Agent-backed tool and [`compact_backend`] schedules
//! soft-threshold prefetch while joining the same Run at the hard threshold.
//!
//! Compaction is non-destructive: the committed transcript is never rewritten (G13).
//! The summary is injected request-only and activates a Run-scoped request window;
//! no raw Step is hidden before the summary covers it. Durable truth is never erased.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_ext_builtin_tools::{AuxiliaryAgentInput, invoke_auxiliary_agent};
use awaken_ext_compact::{CompactArtifact, CompactBackend, CompactRequest};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_contract::tool::RawTool;
use awaken_sandbox_local::LocalProvider;

use crate::agent_catalog::AgentCatalog;
use crate::background::{BackgroundRuns, BackgroundWorkClass};
use crate::judge::AuxAgentTool;
use crate::store::HostCommit;

/// A host's enabled compaction policy. Session construction binds its execution
/// backend to that Session's commit/history boundary.
pub(crate) struct Compaction {
    pub(crate) config: awaken_ext_compact::CompactConfig,
}

/// An ordinary tool whose catalog holds the `compactor` Agent.
/// (ADR-0047 D5). The compaction plugin builds the seed (older slice + summarize
/// prompt); this runs the named agent to completion, the read-side counterpart of
/// memory's `AgentSelector`.
pub(crate) fn compact_runner(
    llm: Arc<dyn LlmExecutor>,
    snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
    commit: Arc<HostCommit>,
) -> Arc<dyn RawTool> {
    let catalog = Arc::new(AgentCatalog::new().with_agent(snapshot));
    let base = std::env::temp_dir()
        .join("awaken-coordinator")
        .join(format!("{}-compact", std::process::id()));
    Arc::new(AuxAgentTool {
        llm,
        provider: LocalProvider::new(base),
        catalog,
        execution: commit,
    })
}

type ArtifactCell = Arc<tokio::sync::OnceCell<Option<CompactArtifact>>>;

/// Per-Session asynchronous compaction backend. The index is an optimization;
/// each cell resolves through a stable ordinary Agent Run.
struct HostCompactBackend {
    agent_tool: Arc<dyn RawTool>,
    background: Arc<BackgroundRuns>,
    cells: Mutex<BTreeMap<String, ArtifactCell>>,
}

impl HostCompactBackend {
    fn cell(&self, key: &str) -> ArtifactCell {
        self.cells
            .lock()
            .expect("compact cache lock poisoned")
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    async fn execute(
        agent_tool: Arc<dyn RawTool>,
        request: CompactRequest,
    ) -> Option<CompactArtifact> {
        let reply = invoke_auxiliary_agent(
            agent_tool.as_ref(),
            &request.key,
            AuxiliaryAgentInput {
                agent_id: request.agent_id,
                seed: request.seed,
            },
            None,
        )
        .await
        .ok()?;
        let summary = reply.text();
        if summary.trim().is_empty() {
            return None;
        }
        Some(CompactArtifact {
            scope: request.scope,
            key: request.key,
            covered_messages: request.covered_messages,
            summary,
        })
    }
}

#[async_trait]
impl CompactBackend for HostCompactBackend {
    async fn prefetch(&self, request: CompactRequest) {
        let cell = self.cell(&request.key);
        if cell.get().is_some() {
            return;
        }
        let agent_tool = self.agent_tool.clone();
        self.background
            .spawn(BackgroundWorkClass::EphemeralCache, async move {
                let _ = cell
                    .get_or_init(|| Self::execute(agent_tool, request))
                    .await;
            })
            .await;
    }

    async fn latest_ready(&self, scope: &str, at_most_messages: usize) -> Option<CompactArtifact> {
        self.cells
            .lock()
            .expect("compact cache lock poisoned")
            .values()
            .filter_map(|cell| cell.get().cloned().flatten())
            .filter(|artifact| {
                artifact.scope == scope && artifact.covered_messages <= at_most_messages
            })
            .max_by_key(|artifact| artifact.covered_messages)
    }

    async fn summarize(&self, request: CompactRequest) -> Option<CompactArtifact> {
        let cell = self.cell(&request.key);
        let agent_tool = self.agent_tool.clone();
        cell.get_or_init(|| Self::execute(agent_tool, request))
            .await
            .clone()
    }
}

pub(crate) fn compact_backend(
    agent_tool: Arc<dyn RawTool>,
    background: Arc<BackgroundRuns>,
) -> Arc<dyn CompactBackend> {
    Arc::new(HostCompactBackend {
        agent_tool,
        background,
        cells: Mutex::new(BTreeMap::new()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_ext_compact::{DEFAULT_COMPACT_INSTRUCTIONS, default_compact_agent};
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, Result as LlmResult,
    };
    use std::sync::atomic::AtomicU64;

    /// A stub compactor: replies with a fixed summary that names how many messages
    /// it was asked to fold (proving it received the seed the plugin built).
    struct SummaryModel(AtomicU64);

    #[async_trait]
    impl LlmExecutor for SummaryModel {
        async fn infer(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    struct GatedSummaryModel {
        calls: AtomicU64,
        started: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl LlmExecutor for GatedSummaryModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.started.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            Ok(ChatResponse {
                output: AssistantOutput::text("background summary"),
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
    async fn compact_runner_recovers_the_stable_compactor_run_without_reinference() {
        let model = Arc::new(SummaryModel(AtomicU64::new(0)));
        let runner = compact_runner(
            model.clone(),
            default_compact_agent("stub", DEFAULT_COMPACT_INSTRUCTIONS),
            Arc::new(HostCommit::Local(Arc::new(
                crate::LocalCommitAdapter::projected(
                    awaken_store_inmem::MemoryCommitCoordinator::new(),
                ),
            ))),
        );
        // The plugin builds the seed (older slice + summarize prompt); here that is
        // 5 user messages.
        let seed: Vec<Message> = (0..5).map(user).collect();
        for seed in [seed, vec![user(99)]] {
            let reply = invoke_auxiliary_agent(
                runner.as_ref(),
                "compact-test",
                AuxiliaryAgentInput {
                    agent_id: awaken_ext_compact::COMPACT_AGENT_ID.to_string(),
                    seed,
                },
                None,
            )
            .await
            .unwrap();
            assert_eq!(reply.text(), "summary of 5 messages");
        }
        assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn prefetch_returns_before_inference_and_hard_resolution_joins_it() {
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let model = Arc::new(GatedSummaryModel {
            calls: AtomicU64::new(0),
            started: started.clone(),
            release: release.clone(),
        });
        let runner = compact_runner(
            model.clone(),
            default_compact_agent("stub", DEFAULT_COMPACT_INSTRUCTIONS),
            Arc::new(HostCommit::Local(Arc::new(
                crate::LocalCommitAdapter::projected(
                    awaken_store_inmem::MemoryCommitCoordinator::new(),
                ),
            ))),
        );
        let background = Arc::new(BackgroundRuns::new());
        let backend = compact_backend(runner, background.clone());
        let request = CompactRequest {
            agent_id: awaken_ext_compact::COMPACT_AGENT_ID.to_string(),
            scope: "thread-a".into(),
            key: "stable-prefetch".into(),
            covered_messages: 1,
            seed: vec![user(1)],
        };

        backend.prefetch(request.clone()).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), started.acquire())
            .await
            .expect("background inference started")
            .unwrap()
            .forget();
        assert!(
            backend.latest_ready("thread-a", 1).await.is_none(),
            "pending prefetch is never exposed as ready"
        );

        let resolving = tokio::spawn({
            let backend = backend.clone();
            async move { backend.summarize(request).await }
        });
        tokio::task::yield_now().await;
        release.add_permits(1);
        let artifact = resolving.await.unwrap().unwrap();
        assert_eq!(artifact.summary, "background summary");
        assert_eq!(
            model.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "hard resolution joins the identical in-flight prefetch"
        );
        assert!(background.drain(std::time::Duration::from_secs(1)).await);
    }
}
