//! The execution loop runs one model step, commits durable facts, and streams
//! live progress that is independent of committed truth (G1/G13).

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{Id as RunId, Lifecycle};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::event::Kind as StreamKind;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{MemoryCommitCoordinator, MemoryStreamSink, replay_latest_lifecycle};
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

/// A deterministic provider that always answers with fixed text.
struct TextLlm(&'static str);

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::Text(self.0.to_string()),
            usage: None,
        })
    }
}

fn install(runtime: &Runtime, fingerprint: &str) {
    let fingerprint = CatalogFingerprint(fingerprint.to_string());
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
        .expect("catalog installs");
}

fn activation(fingerprint: &str) -> RunActivation {
    let fingerprint = CatalogFingerprint(fingerprint.to_string());
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                model_binding: ModelBinding {
                    provider_instance_ref: "provider-1".to_string(),
                    model_ref: "model-1".to_string(),
                    backend_ref: "backend-1".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("message-1".to_string()),
            role: Role::User,
            content: "hello".to_string(),
        }],
        options: RunOptions {
            persistence: PersistenceMode::ReadWrite,
        },
        trace: Default::default(),
    }
}

#[tokio::test]
async fn one_model_step_commits_facts_and_streams_progress() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi there")));
    install(&runtime, "catalog-a");

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite)
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    let outcome = runtime
        .execute(activation("catalog-a"), context)
        .await
        .expect("run executes");
    assert_eq!(outcome.lifecycle, Lifecycle::Completed);

    // Committed truth: one commit, the assistant message, and a run fact.
    assert_eq!(commit.commit_count(), 1);
    let committed = commit.committed();
    assert_eq!(committed.messages.len(), 1);
    assert_eq!(committed.messages[0].content, "hi there");
    assert_eq!(committed.messages[0].role, Role::Assistant);

    // Replay reads committed facts, not the live stream.
    assert_eq!(
        replay_latest_lifecycle(&committed, &RunId("run-1".to_string())),
        Some(Lifecycle::Completed)
    );
    assert_eq!(committed.events.len(), 1);

    // Live stream order is RunStarted -> OutputText -> RunFinished.
    let kinds = sink.events();
    assert!(matches!(kinds[0].kind, StreamKind::RunStarted));
    assert!(matches!(kinds[1].kind, StreamKind::OutputText { .. }));
    assert!(matches!(kinds[2].kind, StreamKind::RunFinished));
}

#[tokio::test]
async fn execution_fails_closed_on_fingerprint_mismatch() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi")));
    install(&runtime, "catalog-a");

    let result = runtime
        .execute(
            activation("catalog-b"),
            RuntimeRunContext::new(PersistenceMode::ReadWrite),
        )
        .await;
    assert!(matches!(result, Err(Error::Resolution(_))));
}

#[tokio::test]
async fn execution_fails_without_a_catalog() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("hi")));
    let result = runtime
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new(PersistenceMode::ReadWrite),
        )
        .await;
    assert!(matches!(result, Err(Error::Resolution(_))));
}

#[tokio::test]
async fn execution_requires_a_model_provider() {
    let runtime = Runtime::new();
    install(&runtime, "catalog-a");
    let result = runtime
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new(PersistenceMode::ReadWrite),
        )
        .await;
    assert!(matches!(result, Err(Error::Execution(_))));
}

#[tokio::test]
async fn run_without_commit_coordinator_still_completes() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm("ok")));
    install(&runtime, "catalog-a");
    let outcome = runtime
        .execute(
            activation("catalog-a"),
            RuntimeRunContext::new(PersistenceMode::Disabled),
        )
        .await
        .expect("runs");
    assert_eq!(outcome.lifecycle, Lifecycle::Completed);
}
