//! The write-plane half of the Coordinator/Worker seam: a database-less Worker sends
//! one versioned [`CommitOperation`] under its current claim. The Coordinator cell
//! validates the claim and applies the operation through the authoritative
//! [`OperationCoordinator`].
//!
//! Paired with the dispatch transport: a worker claims a run over
//! the dispatch transport, drives it, then commits its facts here.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use serde_json::{Value, json};

use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_run_ingress::{
    ClaimedCommitRequest, ClaimedRunCommit, DispatchQueue, WorkerDirectory, commit_payload_hash,
};

use crate::host::HostError;
use crate::host::SharedHost;
use crate::worker_http::respond;
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity,
};

#[async_trait::async_trait]
trait CommitApplier: Send + Sync {
    async fn apply_operation(&self, operation: CommitOperation)
    -> Result<CommitReceipt, HostError>;
}

struct CoordinatorCommitApplier(Arc<dyn OperationCoordinator>);

#[async_trait::async_trait]
impl CommitApplier for CoordinatorCommitApplier {
    async fn apply_operation(
        &self,
        operation: CommitOperation,
    ) -> Result<CommitReceipt, HostError> {
        self.0
            .commit_operation(operation)
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }
}

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
impl CommitApplier for HostCommitApplier {
    async fn apply_operation(
        &self,
        operation: CommitOperation,
    ) -> Result<CommitReceipt, HostError> {
        apply_host_operation(&self.0, operation).await
    }
}

/// Explicit application service for the claim-fenced commit boundary.
///
/// The injected coordinator is the committed-truth authority used by the
/// embedding Coordinator cell. The service never constructs a [`SharedHost`] or
/// selects a storage backend.
pub struct ClaimedCommitService {
    dispatch: Arc<dyn DispatchQueue>,
    applier: Arc<dyn CommitApplier>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
}

impl ClaimedCommitService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchQueue>,
        coordinator: Arc<dyn OperationCoordinator>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            dispatch,
            applier: Arc::new(CoordinatorCommitApplier(coordinator)),
            directory,
            authenticator,
        }
    }

    pub fn for_host(
        dispatch: Arc<dyn DispatchQueue>,
        host: Arc<SharedHost>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            dispatch,
            applier: Arc::new(HostCommitApplier(host)),
            directory,
            authenticator,
        }
    }
}

struct CommitIngestState {
    applier: Arc<dyn CommitApplier>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
}

/// Mount the injectable, registered-worker claimed-commit service.
///
/// This is the sole Worker commit surface. It accepts only a versioned
/// [`CommitOperation`] and requires an authenticated, live Worker incarnation.
pub fn claimed_commit_router(service: Arc<ClaimedCommitService>) -> Router {
    commit_ingest_router_from_parts(
        service.applier.clone(),
        service.dispatch.clone(),
        service.authenticator.clone(),
        service.directory.clone(),
    )
}

fn commit_ingest_router_from_parts(
    applier: Arc<dyn CommitApplier>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
) -> Router {
    let state = Arc::new(CommitIngestState {
        applier,
        dispatch,
        authenticator,
        directory,
    });
    Router::new()
        .route(
            "/v1/worker/commit-claimed",
            axum::routing::post(commit_claimed),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(state)
}

/// Atomically validate a remote worker's claim and apply its ThreadCommit while
/// the dispatch authority guard is live. Reclaim/settle/cancel cannot enter the
/// store between the validation and the commit.
async fn commit_claimed(
    State(state): State<Arc<CommitIngestState>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedCommitRequest>,
) -> (StatusCode, Json<Value>) {
    let identity = &request.identity;
    if verify_current_worker_identity(
        state.directory.as_ref(),
        &worker,
        identity,
        unix_now_ms(),
        false,
    )
    .await
    .is_err()
        || request.claim.owner != identity.lease_owner()
    {
        return unauthorized("worker incarnation does not own the claim".to_string());
    }
    let result = async {
        let guard = state
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(_guard) = guard else {
            return Err(HostError::bad_request("run claim is stale"));
        };
        let expected_hash = commit_payload_hash(&request.operation.commit)
            .map_err(|error| HostError::bad_request(error.to_string()))?;
        if expected_hash != request.operation.payload_hash {
            return Err(HostError::bad_request(
                "commit operation payload hash does not match ThreadCommit",
            ));
        }
        let receipt = state.applier.apply_operation(request.operation).await?;
        Ok(serde_json::to_value(receipt).expect("CommitReceipt serializes"))
    }
    .await;
    respond(result)
}

fn unauthorized(message: String) -> (StatusCode, Json<Value>) {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message })))
}

pub(crate) fn remote_claimed_commit(
    upstream: &awaken_worker_transport_security::WorkerUpstream,
) -> Result<Arc<dyn ClaimedRunCommit>, HostError> {
    awaken_worker_runtime::remote_claimed_commit(upstream).map_err(HostError::internal)
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
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
