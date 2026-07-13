//! A database-less worker's commit path end to end: a `SharedHost` configured with
//! `.with_upstream(server)` runs a fresh turn and commits its facts to the cell
//! server's commit ingest — the worker holds no store, the server is the single
//! writer. The committed messages then read back from the SERVER's store.
//!
//! (The dispatch control plane — claim/settle over HTTP — is covered separately by
//! `dispatch_transport_client`; wiring both into one pool is a two-process e2e,
//! since the dispatch store accessor is process-global.)

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_host::{SharedHost, commit_ingest_router};

/// A model that answers with one short text turn (a natural end, no tool calls).
struct OkModel;

#[async_trait::async_trait]
impl LlmExecutor for OkModel {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("done by a db-less worker"),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_db_less_worker_runs_and_commits_its_facts_to_the_server() {
    // The cell server: store-owning (in-memory here), exposing commit ingest.
    let server = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let router = commit_ingest_router(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // The worker: no store of its own — every thread commits to the server.
    let worker = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub").with_upstream(format!("http://{addr}")),
    );

    // Drive one fresh turn on the worker. Its commit boundary is remote, so the
    // facts land on the SERVER, not the worker.
    worker
        .run(
            None,
            "t1",
            vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        )
        .await
        .expect("the worker runs the turn and commits remotely");

    // The committed truth lives on the server's store, readable back there.
    let committed = server.committed_messages("t1").await;
    assert!(
        committed
            .iter()
            .any(|m| m.text_content().contains("db-less worker")),
        "the worker's turn committed on the server: {committed:?}"
    );

    // The worker holds nothing — its own view of the thread is empty (no store).
    let worker_view = worker.committed_messages("t1").await;
    assert!(
        worker_view.is_empty(),
        "the db-less worker keeps no committed truth locally: {worker_view:?}"
    );
}
