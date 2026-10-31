//! ACP's terminal commit under the cause of its attempt's cancellation.
//!
//! An ACP Step interrupted by the cancellation token ends
//! `TerminationReason::Cancelled`, which `end_cause` maps to
//! `Ended(Cancelled)`. Every ACP terminal goes through `commit::commit`, which
//! applies the shared admission:
//!
//! | Row | Recorded cause | Disposition | Effect |
//! |---|---|---|---|
//! | A1 | none / `Requested` | `Ended(Cancelled)` | committed |
//! | A2 | `ProofExpired`, `ClaimLost`, `WorkerRevoked`, `HostDrain` | `Ended(Cancelled)` | abandoned error, nothing committed |
//! | A3 | any authority loss | `Ended(NaturalEnd)` | committed (real outcome) |
//!
//! The recovered ACP Run stays classified by Dispatch recovery (Indeterminate
//! for an opaque predecessor), unchanged by this table.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_protocol_acp::TerminationReason;
use awaken_runtime_contract::cancellation::{
    CancellationCause, CancellationCauseCell, abandoned_attempt_cause,
};
use awaken_runtime_contract::{CancellationToken, RuntimeRunContext};

#[derive(Default)]
struct CountingCoordinator {
    commits: Mutex<usize>,
}

#[async_trait]
impl Coordinator for CountingCoordinator {
    async fn commit(&self, _commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        let mut commits = self.commits.lock().unwrap();
        *commits += 1;
        Ok(CommitRecord {
            sequence: *commits as u64,
        })
    }
}

fn context(
    coordinator: &Arc<CountingCoordinator>,
    cause: Option<CancellationCause>,
) -> RuntimeRunContext {
    let token = CancellationToken::new();
    let cell = CancellationCauseCell::new();
    match cause {
        Some(cause) => cell.cancel(&token, cause),
        None => token.cancel(),
    }
    RuntimeRunContext::new()
        .with_commit(coordinator.clone())
        .with_cancellation(token)
        .with_cancellation_cause(cell)
}

async fn commit_end(context: &RuntimeRunContext, cause: EndCause) -> crate::Result<()> {
    crate::commit::commit(
        context,
        Vec::new(),
        None,
        &ThreadId("acp-cause-thread".into()),
        RunDisposition::ended(RunId("acp-cause-run".into()), cause),
        Vec::new(),
        Vec::new(),
    )
    .await
}

#[tokio::test]
async fn acp_cancelled_step_commits_only_for_a_cause_that_ends_the_run() {
    assert_eq!(
        crate::end_cause(TerminationReason::Cancelled),
        EndCause::Cancelled,
        "the Step mapping is unchanged; the commit boundary decides"
    );
    for cause in [None, Some(CancellationCause::Requested)] {
        let coordinator = Arc::new(CountingCoordinator::default());
        commit_end(&context(&coordinator, cause), EndCause::Cancelled)
            .await
            .expect("A1 commits Cancelled");
        assert_eq!(*coordinator.commits.lock().unwrap(), 1, "A1 {cause:?}");
    }
    for cause in [
        CancellationCause::ProofExpired,
        CancellationCause::ClaimLost,
        CancellationCause::WorkerRevoked,
        CancellationCause::HostDrain,
    ] {
        let coordinator = Arc::new(CountingCoordinator::default());
        let context = context(&coordinator, Some(cause));
        let error = commit_end(&context, EndCause::Cancelled)
            .await
            .expect_err("A2 authority loss never commits Cancelled");
        assert_eq!(abandoned_attempt_cause(&error), Some(cause), "A2");
        assert_eq!(*coordinator.commits.lock().unwrap(), 0, "A2 {cause:?}");

        commit_end(&context, EndCause::NaturalEnd)
            .await
            .expect("A3 a real outcome still commits");
        assert_eq!(*coordinator.commits.lock().unwrap(), 1, "A3 {cause:?}");
    }
}
