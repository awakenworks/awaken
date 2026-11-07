// @formal entrypoint compaction.artifact_identity_coverage_and_content_fail_closed
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

use awaken_sync::InvariantLock;
use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_builtin_tools::{AuxiliaryAgentInput, invoke_auxiliary_agent};
use awaken_ext_compact::{CompactArtifact, CompactBackend, CompactRequest};
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_tool_contract::tool::RawTool;

use crate::agent_catalog::AgentCatalog;
use crate::judge::AuxAgentTool;
use awaken_session_run_delivery::SessionCommit;
use awaken_session_run_delivery::{BackgroundRuns, BackgroundWorkClass};

/// An ordinary tool whose catalog holds the `compactor` Agent.
/// (ADR-0047 D5). The compaction plugin builds the seed (older slice + summarize
/// prompt); this runs the named agent to completion, the read-side counterpart of
/// memory's `AgentSelector`.
pub(crate) fn compact_runner(
    llm: Arc<dyn LlmExecutor>,
    snapshot: awaken_runtime_contract::ExecutableAgentSnapshot,
    commit: Arc<SessionCommit>,
    clock: Arc<dyn awaken_time::WallClock>,
    sandboxes: Arc<dyn awaken_session_environment_contract::AuxiliarySandboxProvider>,
) -> Arc<dyn RawTool> {
    let catalog = Arc::new(AgentCatalog::new().with_agent(snapshot));
    Arc::new(AuxAgentTool {
        llm,
        provider: sandboxes,
        catalog,
        execution: commit,
        clock,
    })
}

type ArtifactCell = Arc<tokio::sync::OnceCell<Option<CompactArtifact>>>;

/// Per-Session asynchronous compaction backend. The index is an optimization;
/// each cell resolves through a stable ordinary Agent Run.
struct HostCompactBackend {
    agent_tool: Arc<dyn RawTool>,
    background: Arc<BackgroundRuns>,
    cells: InvariantLock<BTreeMap<String, ArtifactCell>>,
}

/// Every compaction failure already means "no artifact": the caller keeps the
/// uncompacted transcript. A poisoned artifact index is one more such
/// failure, reported once per read instead of unwinding through the Run.
///
/// | Rule | index lock | `prefetch` | `summarize` | `latest_ready` |
/// |---|---|---|---|---|
/// | K1 | healthy  | schedules once per key | resolves the key's cell | best ready artifact |
/// | K2 | poisoned | no effect, reported   | `None`, reported        | `None`, reported   |
fn compaction_index_poisoned() {
    tracing::error!(
        code = crate::session_execution_error::EXECUTOR_LOCK_POISONED_CODE,
        "compaction artifact index lock poisoned; the Run keeps its uncompacted transcript"
    );
}

impl HostCompactBackend {
    fn cell(&self, key: &str) -> Option<ArtifactCell> {
        let Ok(mut cells) = self.cells.lock() else {
            compaction_index_poisoned();
            return None;
        };
        Some(cells.entry(key.to_string()).or_default().clone())
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
        let Some(cell) = self.cell(&request.key) else {
            return;
        };
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
        let Ok(cells) = self.cells.lock() else {
            compaction_index_poisoned();
            return None;
        };
        cells
            .values()
            .filter_map(|cell| cell.get().cloned().flatten())
            .filter(|artifact| {
                artifact.scope == scope && artifact.covered_messages <= at_most_messages
            })
            .max_by_key(|artifact| artifact.covered_messages)
    }

    async fn summarize(&self, request: CompactRequest) -> Option<CompactArtifact> {
        let cell = self.cell(&request.key)?;
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
        cells: InvariantLock::new(BTreeMap::new()),
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
        let clock = awaken_time::system_clock();
        let model = Arc::new(SummaryModel(AtomicU64::new(0)));
        let runner = compact_runner(
            model.clone(),
            default_compact_agent("stub", DEFAULT_COMPACT_INSTRUCTIONS)
                .expect("literal model reference"),
            Arc::new(SessionCommit::Local(Arc::new(
                crate::LocalCommitAdapter::projected(
                    awaken_store_inmem::MemoryCommitCoordinator::new(clock.clone()),
                ),
            ))),
            clock.clone(),
            awaken_session_environment_contract::AuxiliarySandboxFactory::provider(
                &awaken_sandbox_local::LocalAuxiliarySandboxes::in_process_temp_dir(),
                "compact",
            ),
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
    async fn a_poisoned_artifact_index_yields_no_artifact_instead_of_unwinding() {
        // Row K2 of the compaction backend table.
        let clock = awaken_time::system_clock();
        let runner = compact_runner(
            Arc::new(SummaryModel(AtomicU64::new(0))),
            default_compact_agent("stub", DEFAULT_COMPACT_INSTRUCTIONS)
                .expect("literal model reference"),
            Arc::new(SessionCommit::Local(Arc::new(
                crate::LocalCommitAdapter::projected(
                    awaken_store_inmem::MemoryCommitCoordinator::new(clock.clone()),
                ),
            ))),
            clock,
            awaken_session_environment_contract::AuxiliarySandboxFactory::provider(
                &awaken_sandbox_local::LocalAuxiliarySandboxes::in_process_temp_dir(),
                "compact",
            ),
        );
        let backend = HostCompactBackend {
            agent_tool: runner,
            background: Arc::new(BackgroundRuns::new()),
            cells: InvariantLock::new(BTreeMap::new()),
        };
        crate::session_execution_error::poison_for_test(&backend.cells);
        let request = CompactRequest {
            agent_id: awaken_ext_compact::COMPACT_AGENT_ID.to_string(),
            scope: "thread-a".into(),
            key: "poisoned".into(),
            covered_messages: 1,
            seed: vec![user(1)],
        };
        backend.prefetch(request.clone()).await;
        assert!(backend.summarize(request).await.is_none(), "K2 summarize");
        assert!(
            backend.latest_ready("thread-a", 10).await.is_none(),
            "K2 latest"
        );
    }

    #[tokio::test]
    async fn prefetch_returns_before_inference_and_hard_resolution_joins_it() {
        let clock = awaken_time::system_clock();
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let model = Arc::new(GatedSummaryModel {
            calls: AtomicU64::new(0),
            started: started.clone(),
            release: release.clone(),
        });
        let runner = compact_runner(
            model.clone(),
            default_compact_agent("stub", DEFAULT_COMPACT_INSTRUCTIONS)
                .expect("literal model reference"),
            Arc::new(SessionCommit::Local(Arc::new(
                crate::LocalCommitAdapter::projected(
                    awaken_store_inmem::MemoryCommitCoordinator::new(clock.clone()),
                ),
            ))),
            clock.clone(),
            awaken_session_environment_contract::AuxiliarySandboxFactory::provider(
                &awaken_sandbox_local::LocalAuxiliarySandboxes::in_process_temp_dir(),
                "compact",
            ),
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
        // The bound only turns a lost start into a failure. The auxiliary
        // sandbox is created on the blocking pool before inference starts, so
        // its fsyncs count against this wall clock under writeback pressure.
        tokio::time::timeout(std::time::Duration::from_secs(30), started.acquire())
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
