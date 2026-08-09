//! The write-plane half of the Coordinator/Worker seam: a database-less Worker sends
//! one versioned [`CommitOperation`] under its current claim. The Coordinator cell
//! validates the claim and applies the operation through the authoritative
//! [`OperationCoordinator`].
//!
//! Paired with the dispatch transport: a worker claims a run over
//! the dispatch transport, drives it, then commits its facts here.

use std::sync::Arc;

use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator as _;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_run_ingress::{ClaimedCommitApplier, ClaimedRunCommit, DispatchQueue};

use crate::host::HostError;
use crate::host::SharedHost;
struct HostCommitApplier(Arc<SharedHost>);

async fn apply_host_operation(
    host: &Arc<SharedHost>,
    operation: CommitOperation,
) -> Result<CommitReceipt, HostError> {
    use awaken_agent_contract::agent::run::RunState;

    let thread = operation.commit.thread_id.clone();
    let terminal = match operation.commit.run_state() {
        RunState::Ended(cause) => Some(awaken_runtime_contract::terminal::CommittedTerminalRun {
            run_id: operation.commit.run_id().clone(),
            thread_id: thread.clone(),
            cause,
        }),
        RunState::Running | RunState::Awaiting => None,
    };
    let ctx = host.ctx_for(&thread.0, None).await?;
    let receipt = ctx
        .commit
        .commit_operation(operation)
        .await
        .map_err(|error| HostError::internal(error.to_string()))?;

    if let Some(terminal) = terminal {
        // Delivery is deliberately after the authoritative commit. Failure cannot
        // roll back the Run; operation replay redelivers the same stable identity
        // and the extraction outbox makes that redelivery idempotent.
        for failure in awaken_runtime_contract::terminal::deliver_committed_terminal(
            &ctx.terminal_observers,
            &terminal,
        )
        .await
        {
            tracing::warn!(
                observer.id = %failure.observer_id,
                awaken.run.id = %terminal.run_id.0,
                error = %failure.error,
                "remote committed-terminal observer failed; commit replay may redeliver"
            );
        }
    }
    Ok(receipt)
}

#[async_trait::async_trait]
impl ClaimedCommitApplier for HostCommitApplier {
    async fn apply(
        &self,
        operation: CommitOperation,
    ) -> Result<CommitReceipt, awaken_run_ingress::ApplicationError> {
        apply_host_operation(&self.0, operation)
            .await
            .map_err(map_host_error)
    }
}

fn map_host_error(error: HostError) -> awaken_run_ingress::ApplicationError {
    match error.kind {
        crate::HostErrorKind::BadRequest => {
            awaken_run_ingress::ApplicationError::invalid(error.message)
        }
        crate::HostErrorKind::Conflict => {
            awaken_run_ingress::ApplicationError::conflict(error.message)
        }
        crate::HostErrorKind::Internal => {
            awaken_run_ingress::ApplicationError::internal(error.message)
        }
        crate::HostErrorKind::Unavailable => {
            awaken_run_ingress::ApplicationError::unavailable(error.message)
        }
    }
}

#[must_use]
pub fn claimed_commit_service(
    dispatch: Arc<dyn DispatchQueue>,
    host: Arc<SharedHost>,
) -> awaken_run_ingress::ClaimedCommitService {
    awaken_run_ingress::ClaimedCommitService::with_applier(
        dispatch,
        Arc::new(HostCommitApplier(host)),
    )
}

pub(crate) fn remote_claimed_commit(
    upstream: &awaken_worker_transport_security::WorkerUpstream,
) -> Result<Arc<dyn ClaimedRunCommit>, HostError> {
    awaken_worker_runtime::remote_claimed_commit(upstream).map_err(HostError::internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::RunDisposition;
    use awaken_agent_contract::thread::commit::operation::CommitOperationId;
    use awaken_agent_contract::thread::commit::staged::ThreadCommit;

    #[tokio::test]
    async fn remote_terminal_commit_observation_is_coordinator_owned_and_idempotent() {
        // Cause/effect graph: C1 commit is nonterminal/terminal; C2 terminal operation
        // is fresh/replayed; C3 execution Host is Coordinator/remote Worker. Effects:
        // E1 committed truth advances once; E2 one durable extraction intent exists;
        // E3 Worker owns no terminal observer/outbox; E4 nonterminal commits create no
        // extraction. Constraint: observation occurs only after commit_operation.
        //
        // | Rule | C1          | C2       | C3          | effects   |
        // | R1   | nonterminal | fresh    | Coordinator | E1,E4     |
        // | R2   | terminal    | fresh    | Coordinator | E1,E2     |
        // | R3   | terminal    | replay   | Coordinator | E1,E2     |
        // | R4   | terminal    | any      | Worker      | E3        |
        let thread = "remote-memory-terminal";
        let run = RunId("remote-memory-run".into());
        let host = Arc::new(SharedHost::new(
            Arc::new(crate::host::MemoryHostModel),
            "stub",
        ));
        crate::host::bind_test_memory(&host, thread, "remote-memory-store", true);
        let coordinator_ctx = host
            .ctx_for(thread, None)
            .await
            .expect("Coordinator context");
        assert_eq!(coordinator_ctx.terminal_observers.len(), 1, "R1/R2 owner");

        let operation = |ordinal, expected_thread_version, commit: ThreadCommit| CommitOperation {
            operation_id: CommitOperationId::new(run.clone(), ordinal),
            expected_thread_version,
            payload_hash: awaken_run_ingress::commit_payload_hash(&commit).expect("commit hash"),
            commit,
        };
        let running = operation(
            0,
            0,
            ThreadCommit::assemble(
                ThreadId(thread.into()),
                RunDisposition::running(run.clone()),
                true,
                vec![Message::text(
                    MessageId("remote-user".into()),
                    Role::User,
                    "remember rust",
                )],
                Vec::new(),
                Vec::new(),
            ),
        );
        apply_host_operation(&host, running)
            .await
            .expect("R1 running commit");
        let intent_id = format!("memory-extraction:{thread}:{}", run.0);
        assert!(
            host.memory
                .extraction_repository()
                .get_extraction(&intent_id)
                .await
                .expect("R1 query")
                .is_none(),
            "R1/E4"
        );

        let terminal = operation(
            1,
            1,
            ThreadCommit::assemble(
                ThreadId(thread.into()),
                RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
                true,
                vec![Message::text(
                    MessageId("remote-assistant".into()),
                    Role::Assistant,
                    "done",
                )],
                Vec::new(),
                Vec::new(),
            ),
        );
        let first = apply_host_operation(&host, terminal.clone())
            .await
            .expect("R2 terminal commit");
        let replay = apply_host_operation(&host, terminal)
            .await
            .expect("R3 terminal replay");
        assert!(!first.duplicate, "R2/E1");
        assert!(replay.duplicate, "R3/E1");
        assert!(
            host.memory
                .extraction_repository()
                .get_extraction(&intent_id)
                .await
                .expect("R2/R3 query")
                .is_some(),
            "R2/R3 E2"
        );

        let worker = SharedHost::new(Arc::new(crate::host::MemoryHostModel), "stub")
            .with_worker_upstream(
                awaken_worker_transport_security::WorkerUpstream::new("http://127.0.0.1:1")
                    .with_worker_identity(awaken_run_ingress::WorkerIdentity::new(
                        "worker", "boot", 1,
                    )),
            );
        crate::host::bind_test_memory(&worker, "remote-worker-thread", "remote-worker-store", true);
        let worker_ctx = worker
            .ctx_for("remote-worker-thread", None)
            .await
            .expect("Worker context");
        assert!(worker_ctx.terminal_observers.is_empty(), "R4/E3");
    }
}
