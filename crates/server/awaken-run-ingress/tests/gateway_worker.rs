//! Durable/worker-path cloud-managed gateway honor (ADR-0004, P1b): a worker-driven
//! run carrying a `ModelAccessGrant::CloudManagedGateway` is routed through the
//! gateway executor built from the grant — the secretless-worker path — and a
//! gateway grant a worker cannot honor fails the drive CLOSED (never degrading to
//! local credentials).

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Phase};
use awaken_run_ingress::{
    DispatchQueue, DispatchWorker, DurableRunIngress, GatewayExecutorFn, MemoryDispatchStore,
    RunExecutionRequest,
};
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::model_access::ModelAccessGrant;

use harness::{activation, text_runtime};

/// A stand-in gateway executor: whatever the request, it replies with a fixed marker,
/// so a test can prove THIS executor (not the runtime's bound default) drove the run.
struct MarkerLlm(&'static str);
#[async_trait::async_trait]
impl LlmExecutor for MarkerLlm {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.0),
            usage: None,
            stop_reason: None,
        })
    }
}

/// A gateway-grant activation for `run`. The materialized endpoint's fields are what
/// a real gateway build would read; the marker executor ignores them.
fn gateway_activation(run: &str) -> RunExecutionRequest {
    let mut a = activation(run);
    a.model_access = ModelAccessGrant::CloudManagedGateway {
        gateway_base_url: "https://gw.internal".into(),
        dialect: "anthropic".into(),
        model_ref: "claude".into(),
        lease_token: "lease-xyz".into(), // awaken-allow: secret
    };
    RunExecutionRequest::new(a)
}

/// A worker-driven run carrying a gateway grant is executed through the executor the
/// gateway builder returns — NOT the runtime's bound default ("done") — so a
/// secretless worker honors the grant without a local provider credential.
#[tokio::test]
async fn a_gateway_granted_run_is_driven_through_the_gateway_executor() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    // The runtime's BOUND executor replies "done"; the gateway builder returns one
    // that replies "GATEWAY". If the grant is honored, the transcript carries GATEWAY.
    let gateway: GatewayExecutorFn = Arc::new(|_endpoint| {
        Some(Arc::new(MarkerLlm("GATEWAY")) as Arc<dyn LlmExecutor>)
    });
    let worker = DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "w")
        .with_gateway_executor(gateway);

    store.enqueue(gateway_activation("run-gw")).await.unwrap();
    let processed = worker.tick(0).await.expect("drive");
    assert_eq!(
        processed.map(|(_, phase)| phase),
        Some(Phase::Ended(EndCause::NaturalEnd)),
        "the gateway-granted run reached a terminal phase"
    );
    let messages = commit.committed().messages;
    assert!(
        messages.iter().any(|m| m.text_content() == "GATEWAY"),
        "the run was driven through the GATEWAY executor, not the bound default"
    );
    assert!(
        !messages.iter().any(|m| m.text_content() == "done"),
        "the runtime's bound default executor was never used for a gateway grant"
    );
}

/// A gateway grant a worker cannot honor (no gateway builder installed) fails the
/// drive CLOSED — the worker never falls back to its local/bound executor for a
/// gateway run, which would defeat the custody the grant enforces.
#[tokio::test]
async fn a_gateway_grant_with_no_builder_fails_closed() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    // No `with_gateway_executor`: this worker cannot honor a gateway grant.
    let worker = DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "w");

    store.enqueue(gateway_activation("run-gw")).await.unwrap();
    let result = worker.tick(0).await;
    assert!(
        result.is_err(),
        "a gateway grant with no builder must fail the drive, not run on local credentials"
    );
    // Nothing was committed for the run — it did not execute.
    assert!(
        commit.committed().messages.iter().all(|m| m.text_content() != "done"),
        "the run never fell back to the bound executor"
    );
}

/// `DurableRunIngress::with_owner_and_gateway` wires the gateway builder into the
/// ingress's worker, so a run driven through the ingress honors a gateway grant —
/// the composition path the host uses to make a worker secretless.
#[tokio::test]
async fn durable_ingress_wires_the_gateway_builder_into_its_worker() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let gateway: GatewayExecutorFn =
        Arc::new(|_endpoint| Some(Arc::new(MarkerLlm("GATEWAY")) as Arc<dyn LlmExecutor>));
    let ingress = DurableRunIngress::with_owner_and_gateway(
        text_runtime(),
        store.clone(),
        commit.clone(),
        "node-1",
        None,
        Some(gateway),
    );

    store.enqueue(gateway_activation("run-gw")).await.unwrap();
    ingress.worker().tick(0).await.expect("drive").expect("a run");
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content() == "GATEWAY"),
        "the ingress's worker honored the gateway grant via the wired builder"
    );
}

/// A LOCAL grant (the default) is unaffected: the runtime's bound executor drives it,
/// exactly as before — the gateway path is opt-in per run and never disturbs the
/// common local-credential case.
#[tokio::test]
async fn a_local_grant_still_uses_the_bound_executor() {
    let store = Arc::new(MemoryDispatchStore::new());
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let gateway: GatewayExecutorFn = Arc::new(|_endpoint| {
        Some(Arc::new(MarkerLlm("GATEWAY")) as Arc<dyn LlmExecutor>)
    });
    let worker = DispatchWorker::new(text_runtime(), store.clone(), commit.clone(), "w")
        .with_gateway_executor(gateway);

    // A default (local) grant: the bound "done" executor drives it, NOT the gateway.
    store
        .enqueue(RunExecutionRequest::new(activation("run-local")))
        .await
        .unwrap();
    worker.tick(0).await.expect("drive");
    let messages = commit.committed().messages;
    assert!(
        messages.iter().any(|m| m.text_content() == "done"),
        "a local grant uses the runtime's bound executor"
    );
    assert!(
        !messages.iter().any(|m| m.text_content() == "GATEWAY"),
        "the gateway builder is never consulted for a local grant"
    );
}
