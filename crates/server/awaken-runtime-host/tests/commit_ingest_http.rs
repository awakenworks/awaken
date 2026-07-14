//! The write-plane worker seam over real HTTP: a database-less worker's
//! `RemoteCoordinator` pushes a staged `ThreadCommit` to the cell server's
//! `commit_ingest_router`, which applies it through the thread's single writer.
//! The committed facts are then readable from the server's store, and a redelivery
//! is idempotent (at-least-once → exactly-once effect).

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::commit::coordinator::Coordinator;
use awaken_agent_contract::commit::staged::ThreadCommit;
use awaken_agent_contract::fact::run::Fact;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_host::{RemoteCoordinator, SharedHost, commit_ingest_router};

struct OkModel;

#[async_trait::async_trait]
impl LlmExecutor for OkModel {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("ok"),
            usage: None,
            stop_reason: None,
        })
    }
}

fn thread_commit() -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId("t1".into()),
        run_fact: Fact {
            run_id: RunId("run-A".into()),
            phase: Phase::Ended(EndCause::NaturalEnd),
        },
        messages: vec![Message::text(
            MessageId("a1".into()),
            Role::Assistant,
            "committed by a db-less worker",
        )],
        state: vec![],
        events: vec![],
        waiting: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn db_less_worker_pushes_facts_and_the_server_commits_them() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let router = commit_ingest_router(host.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // The worker holds only this HTTP client — no store, no coordinator of its own.
    let coordinator = RemoteCoordinator::new(format!("http://{addr}"));

    let record = coordinator
        .commit(thread_commit())
        .await
        .expect("commit over http");
    assert!(
        record.sequence >= 1,
        "the server sequenced the commit: {record:?}"
    );

    // The fact is now committed truth on the SERVER's store, readable back.
    let committed = host.committed_messages("t1").await;
    assert_eq!(
        committed.len(),
        1,
        "the worker's message committed on the server"
    );
    assert!(
        committed[0].text_content().contains("db-less worker"),
        "the committed message is the one the worker pushed"
    );

    // At-least-once redelivery is idempotent: the same commit does not duplicate.
    coordinator
        .commit(thread_commit())
        .await
        .expect("redelivered commit accepted");
    let after = host.committed_messages("t1").await;
    assert_eq!(
        after.len(),
        1,
        "a redelivered commit is a no-op (idempotent), not a duplicate: {after:?}"
    );
}
