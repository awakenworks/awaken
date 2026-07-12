//! A projection is derived from committed event records and the run-store read
//! port — never from the live stream sink (G1/G13).

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::kind::Kind as EventKind;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_runtime::Runtime;
use awaken_runtime::memory::{
    CommittedThread, MemoryCommitCoordinator, MemoryStreamSink, replay_latest_phase,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

struct TextLlm;

#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done".to_string()),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A public-style projection built only from committed event records.
fn project_phase_events(committed: &CommittedThread) -> Vec<String> {
    committed
        .events
        .iter()
        .filter(|record| matches!(record.kind, EventKind::RunPhaseChanged))
        .map(|record| record.payload.to_string())
        .collect()
}

async fn run() -> (MemoryCommitCoordinator, MemoryStreamSink) {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm));
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint.clone(),
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let sink = Arc::new(MemoryStreamSink::new());
    let context = RuntimeRunContext::new()
        .with_commit(commit.clone())
        .with_stream_sink(sink.clone());

    let activation = RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                model_binding: ModelBinding {
                    provider_identity_ref: "p".to_string(),
                    model_ref: "m".to_string(),
                    backend_ref: "b".to_string(),
                },
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("hi")],
        }],
        trace: Default::default(),
    };
    runtime.execute(activation, context).await.expect("runs");
    (
        Arc::try_unwrap(commit).unwrap_or_default(),
        Arc::try_unwrap(sink).unwrap_or_default(),
    )
}

#[tokio::test]
async fn projection_derives_from_committed_events_not_the_live_stream() {
    let (commit, _sink) = run().await;
    let committed = commit.committed();

    // The projection is built from committed event records: the transition
    // into Running at the first step boundary, then the terminal phase.
    let events = project_phase_events(&committed);
    assert_eq!(events.len(), 2);
    assert!(events[0].contains("Running"));
    assert!(events[1].contains("NaturalEnd"));

    // The same truth is reachable through the RunStore read port.
    let record = commit.get(&RunId("run-1".to_string())).expect("run record");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));
    assert!(commit.get(&RunId("missing".to_string())).is_none());

    // The record is a derived cache: it equals what replay derives from the
    // committed fact log, which is the authority (ADR-0006 D1/D2).
    assert_eq!(
        replay_latest_phase(&committed, &RunId("run-1".to_string())),
        Some(record.phase)
    );
}
