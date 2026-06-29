//! Cross-mechanism GWT scenarios from docs/design/runtime-scenario-validation.md.
//!
//! ## Coverage map
//!
//! | Scenario id | Test |
//! |---|---|
//! | RS-ING-001  | direct_ingress_durable_path_fails_closed |
//! | RS-EVT-001  | live_stream_is_not_replay_truth |
//! | RS-CTRL-001 | cancel_commits_terminal_outcome (cancel→terminal-commit slice) |
//!
//! The long scenario text lives in the design doc; tests carry a short
//! Given/When/Then for readability.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Lifecycle};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::event::Kind as StreamKind;
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamSink, replay_latest_lifecycle};
use awaken_runtime::{DirectRunIngress, RunIngress, Runtime};
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation, RunOptions};
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use tokio_util::sync::CancellationToken;

struct TextLlm;

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::Text("done".to_string()),
            usage: None,
        })
    }
}

fn runtime() -> Arc<Runtime> {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm)));
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
    runtime
}

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                model_binding: ModelBinding {
                    provider_instance_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: "hi".to_string(),
        }],
        options: RunOptions {
            persistence: PersistenceMode::ReadWrite,
        },
        trace: Default::default(),
    }
}

/// RS-ING-001: Given direct ingress, When a durable-only submission is
/// requested, Then it fails closed with a typed error before execution.
#[tokio::test]
async fn direct_ingress_durable_path_fails_closed() {
    let ingress = DirectRunIngress::new(runtime());
    assert!(matches!(
        ingress.submit_background(activation()).await,
        Err(Error::Execution(_))
    ));
}

/// RS-EVT-001: Given a run that streams live events and commits, When replay
/// reads history, Then it derives from committed facts, not the live stream.
#[tokio::test]
async fn live_stream_is_not_replay_truth() {
    let runtime = runtime();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite)
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    runtime.execute(activation(), context).await.expect("runs");

    // Live stream carries best-effort progress.
    let live = sink.events();
    assert!(
        live.iter()
            .any(|e| matches!(e.kind, StreamKind::RunFinished))
    );

    // Replay is reconstructed from committed facts alone.
    let committed = commit.committed();
    assert_eq!(
        replay_latest_lifecycle(&committed, &RunId("run-1".to_string())),
        Some(Lifecycle::Completed)
    );
}

/// RS-CTRL-001 (cancel→terminal slice): Given a pre-cancelled run, When it
/// executes, Then a terminal Cancelled outcome is committed and no assistant
/// message is produced.
#[tokio::test]
async fn cancel_commits_terminal_outcome() {
    let runtime = runtime();
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let token = CancellationToken::new();
    token.cancel();
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite)
        .with_commit(commit.clone())
        .with_cancellation(token);

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome.lifecycle, Lifecycle::Cancelled);

    let committed = commit.committed();
    assert!(
        committed.messages.is_empty(),
        "a cancelled run must not commit assistant messages"
    );
}
