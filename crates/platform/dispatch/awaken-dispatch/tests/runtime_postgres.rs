// @formal evidence checkpoint.crash_preserves_durable_truth
//! #1 — the durable Postgres commit backend satisfies the runtime contract.
//!
//! ADR-0008 landed `PostgresCommitCoordinator` but nothing drove a real run
//! against it. This proves the durable backend is contract-equivalent to the
//! in-memory one: a run executes and *awaits* through it, a fresh coordinator on
//! the same database *rehydrates* the committed transcript and the active ticket
//! (a restart), then opens only the active Thread's committed projection and
//! resumes to a terminal state through the same `Runtime::execute`/`resume`
//! contract the memory tests use.
//!
//! It runs against a real Postgres (`AWAKEN_TEST_DATABASE_URL`, defaulting to the
//! local dev container) and skips with a notice when none is reachable.

mod harness;

use std::sync::Arc;

use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_store_postgres::PostgresCommitCoordinator;

use harness::{FP, SNAP, THREAD, TICKET, activation, tool_runtime};

fn allow_resume() -> ResumeCommand {
    ResumeCommand {
        operation_id: None,
        correlation_id: TICKET.to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot_id: awaken_runtime_contract::ExecutableAgentSnapshotId(SNAP.to_string()),
        catalog_fingerprint: awaken_runtime_contract::CatalogFingerprint(FP.to_string()),
        result: ResumeResult::allow(),
        context_messages: Vec::new(),
        now_ms: 0,
    }
}

#[tokio::test]
async fn postgres_commit_backs_execute_resume_and_survives_restart() {
    // R1 execute -> input, model Entered/Returned, Requested batch and Awaiting
    // are durable before restart; R2 reopen -> the same ticket and zero tool
    // effects; R3 approved resume -> one tool effect and terminal truth.
    // Commit cardinality includes infrastructure evidence, not just Run phases.
    let Some(pool) = harness::schema_pool("t_rt").await else {
        return;
    };

    let (runtime, ran) = tool_runtime();

    // Execute against the Postgres commit boundary: the run awaits on the gate.
    let commit = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("coordinator"),
    );
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let state = runtime
        .execute(activation("run-1"), context)
        .await
        .expect("execute");
    assert_eq!(state, RunState::Awaiting);
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 0);
    drop(commit); // simulate a process restart

    // A fresh coordinator reads only the global fence at startup; the active
    // Thread's synchronous execution view must be opened from committed SQL.
    let restarted = Arc::new(
        PostgresCommitCoordinator::with_pool(pool.clone())
            .await
            .expect("restart"),
    );
    // C1 model returns a gated tool call; C2 the coordinator restarts before
    // approval. E1 recover input + model enter/return + Requested batch + Awaiting;
    // E2 exactly one invocation, zero tool entries, and the exact active ticket.
    // The shared memory/Postgres oracle reads the canonical Thread state log.
    assert_eq!(
        restarted.commit_count(),
        5,
        "input, model evidence, requested batch, and awaiting approval survive restart"
    );
    let opened = Arc::new(
        restarted
            .open_thread_projection(&ThreadId(THREAD.to_string()))
            .await
            .expect("R2 exact active Thread projection"),
    );
    harness::assert_completed_model_invocations(opened.as_ref(), 1);
    assert!(
        CommittedThreadView::resume_ticket(&*opened, &RunId("run-1".to_string())).is_some(),
        "the active ticket is available from committed SQL after restart"
    );

    // RunResume against the exact scoped reader: the pending tool runs and the
    // same view advances only from commits it sends to PostgreSQL.
    let context = RuntimeRunContext::new().with_commit(opened.clone());
    let state = runtime
        .resume(allow_resume(), opened.as_ref(), context)
        .await
        .expect("resume");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    // R2 resume => reuse the first call's journal and append exactly the second
    // model invocation after the approved tool; never replay the first call.
    harness::assert_completed_model_invocations(opened.as_ref(), 2);

    // The committed run record (the derived cache) equals the latest fact.
    let record =
        CommittedThreadView::run(&*opened, &RunId("run-1".to_string())).expect("run record");
    assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
    assert!(
        CommittedThreadView::resume_ticket(&*opened, &RunId("run-1".to_string())).is_none(),
        "a resumed run clears its ticket"
    );
}
