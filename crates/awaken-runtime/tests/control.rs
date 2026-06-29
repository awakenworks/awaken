//! Cancellation produces a terminal Cancelled outcome and DirectRunIngress is
//! the direct delivery seam; durable-only operations fail closed (G5).

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime::{DirectRunIngress, RunIngress, Runtime};
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation, RunOptions};
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

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
        })
    }
}

/// Blocks the first inference until released, so a live cancel can land mid-run.
struct GatedLlm {
    started: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait::async_trait]
impl LlmExecutor for GatedLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(ChatResponse {
            output: AssistantOutput::text("late".to_string()),
            usage: None,
        })
    }
}

fn install(runtime: &Runtime) {
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
        .expect("catalog installs");
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
                instructions: String::new(),
                max_steps: 16,
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
            content: vec![ContentBlock::text("hi")],
        }],
        options: RunOptions {
            persistence: PersistenceMode::ReadWrite,
        },
        trace: Default::default(),
    }
}

#[tokio::test]
async fn pre_cancelled_run_commits_a_terminal_cancelled_outcome() {
    let runtime = Runtime::new().with_llm(Arc::new(TextLlm));
    install(&runtime);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let token = CancellationToken::new();
    token.cancel();
    let context = RuntimeRunContext::new(PersistenceMode::ReadWrite)
        .with_commit(commit.clone())
        .with_cancellation(token);

    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::Cancelled));
    assert_eq!(
        commit.committed().latest_run.unwrap().phase,
        Phase::Ended(EndCause::Cancelled)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_cancel_steers_an_in_flight_run() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(GatedLlm {
        started: started.clone(),
        release: release.clone(),
    })));
    install(&runtime);

    let token = CancellationToken::new();
    let context = RuntimeRunContext::new(PersistenceMode::Disabled).with_cancellation(token);

    let runtime_for_run = runtime.clone();
    let handle = tokio::spawn(async move { runtime_for_run.execute(activation(), context).await });

    // Wait until the first inference is in-flight, then cancel via live control.
    started.notified().await;
    runtime
        .deliver(LiveCommand::Cancel {
            run_id: RunId("run-1".to_string()),
        })
        .expect("cancel delivered");
    release.notify_one();

    let outcome = handle.await.expect("join").expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::Cancelled));
}

#[test]
fn cancel_on_unknown_run_is_not_active() {
    let runtime = Runtime::new();
    assert_eq!(
        runtime.deliver(LiveCommand::Cancel {
            run_id: RunId("ghost".to_string()),
        }),
        Err(ControlError::NotActive)
    );
}

#[tokio::test]
async fn direct_ingress_runs_inline_and_rejects_durable() {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm)));
    install(&runtime);
    let ingress = DirectRunIngress::new(runtime);

    let outcome = ingress
        .submit(
            activation(),
            RuntimeRunContext::new(PersistenceMode::ReadWrite),
        )
        .await
        .expect("inline run");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));

    // Durable submission fails closed on direct ingress (G5).
    assert!(matches!(
        ingress.submit_background(activation()).await,
        Err(Error::Execution(_))
    ));
}

#[tokio::test]
async fn direct_ingress_cancel_on_unknown_run_is_not_active() {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm)));
    let ingress = DirectRunIngress::new(runtime);
    assert_eq!(
        ingress.cancel(&RunId("ghost".to_string())),
        Err(ControlError::NotActive)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wake_on_an_active_run_is_accepted() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(GatedLlm {
        started: started.clone(),
        release: release.clone(),
    })));
    install(&runtime);

    let token = CancellationToken::new();
    let context = RuntimeRunContext::new(PersistenceMode::Disabled).with_cancellation(token);
    let runtime_for_run = runtime.clone();
    let handle = tokio::spawn(async move { runtime_for_run.execute(activation(), context).await });

    started.notified().await;
    // A wake on a live run is accepted (no-op in the MVP) rather than failing.
    assert_eq!(
        runtime.deliver(LiveCommand::Wake {
            run_id: RunId("run-1".to_string()),
            reason: "nudge".to_string(),
        }),
        Ok(())
    );
    release.notify_one();
    let outcome = handle.await.expect("join").expect("runs");
    assert_eq!(outcome, Phase::Ended(EndCause::NaturalEnd));
}
