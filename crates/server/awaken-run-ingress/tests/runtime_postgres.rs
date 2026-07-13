//! #1 — the durable Postgres commit backend satisfies the runtime contract.
//!
//! ADR-0008 landed `PostgresCommitCoordinator` but nothing drove a real run
//! against it. This proves the durable backend is contract-equivalent to the
//! in-memory one: a run executes and *parks* through it, a fresh coordinator on
//! the same database *rehydrates* the committed transcript and the active ticket
//! (a restart), and the run *resumes* to a terminal phase against the rehydrated
//! reader — all through the same `Runtime::execute`/`resume` the memory tests use.
//!
//! It runs against a real Postgres (`AWAKEN_TEST_DATABASE_URL`, defaulting to the
//! local dev container) and skips with a notice when none is reachable.

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_store_postgres::PostgresCommitCoordinator;

use harness::{FP, SNAP, THREAD, TICKET, activation, tool_runtime};

fn allow_resume() -> ResumeCommand {
    ResumeCommand {
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAP.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FP.to_string()),
        result: ResumeResult::Decision {
            allow: true,
            note: None,
        },
        now_ms: 0,
    }
}

#[tokio::test]
async fn postgres_commit_backs_execute_resume_and_survives_restart() {
    let Some(pool) = harness::schema_pool("t_rt").await else {
        return;
    };

    let (runtime, ran) = tool_runtime();

    // Execute against the Postgres commit boundary: the run parks on the gate.
    let commit = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("coordinator"),
    );
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime
        .execute(activation("run-1"), context)
        .await
        .expect("execute");
    assert_eq!(phase, Phase::Waiting);
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 0);
    drop(commit); // simulate a process restart

    // A fresh coordinator on the same database rehydrates committed truth.
    let restarted = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("restart"),
    );
    assert_eq!(restarted.commit_count(), 1, "the park survives restart");
    assert!(
        ThreadReader::waiting_ticket(&*restarted, &RunId("run-1".to_string())).is_some(),
        "the active ticket rehydrated"
    );

    // Resume against the rehydrated reader: the pending tool runs and the run ends.
    let context = RuntimeRunContext::new().with_commit(restarted.clone());
    let phase = runtime
        .resume(allow_resume(), restarted.as_ref(), context)
        .await
        .expect("resume");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The committed run record (the derived cache) equals the latest fact.
    let record = RunStore::get(&*restarted, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(record.phase, Phase::Ended(EndCause::NaturalEnd));
    assert!(
        ThreadReader::waiting_ticket(&*restarted, &RunId("run-1".to_string())).is_none(),
        "a resumed run clears its ticket"
    );
}
