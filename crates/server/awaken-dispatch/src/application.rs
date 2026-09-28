// @formal entrypoint ingress.commit_epoch_atomic_remote_custom
// @formal entrypoint remote_worker.committed_truth_is_coordinator_owned
// @formal entrypoint sandbox.k8s_process_observation_requires_complete_exit
// @formal evidence session_execution.one_physical_attempt_per_thread
//! Protocol-neutral application ports used by Coordinator HTTP adapters.

use awaken_dispatch_contract::ControlDispatchQueue;
use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::thread::commit::coordinator::OperationCoordinator;
use awaken_agent_contract::thread::commit::operation::{CommitReceipt, commit_payload_hash};

use crate::commit_fence::validate_commit_dispatch_binding;
use crate::{ClaimedCommitRequest, RunDispatch};

/// Trusted Coordinator composition resolves an exact running execution domain
/// before physical admission. Worker request JSON is never an implementation.
/// Implementations must verify provider provenance and the full incarnation;
/// returning a coordinate is not a stop receipt or permission to clear a slot.
/// Lookup is read-only and retryable: it must not launch or stop execution.
#[async_trait::async_trait]
pub trait WorkerExecutionBindingSource: Send + Sync {
    async fn binding_for(
        &self,
        worker: &crate::WorkerIdentity,
    ) -> Result<crate::AttemptExecutionBinding, ApplicationError>;
}

/// Coordinator-authorized whole-Worker stop, not user cancellation of one Run.
/// Only the application below can construct it, after exact Registry closure.
/// It carries the original persisted binding, never a newly observed replacement.
///
/// ```compile_fail
/// use awaken_dispatch::{AttemptExecutionBinding, ClosedWorkerExecution};
/// fn forge(binding: AttemptExecutionBinding) {
///     let _ = ClosedWorkerExecution { binding };
/// }
/// ```
pub struct ClosedWorkerExecution {
    binding: crate::AttemptExecutionBinding,
}

impl ClosedWorkerExecution {
    pub fn binding(&self) -> &crate::AttemptExecutionBinding {
        &self.binding
    }

    /// Trusted adapters call this only after observing exact local process exit.
    /// This provider reference supports audit/reobservation, not remote replay.
    pub fn observed_exit(
        &self,
        evidence_ref: impl Into<String>,
    ) -> Result<WorkerProcessStopStatus, ApplicationError> {
        let evidence_ref = evidence_ref.into();
        if evidence_ref.trim().is_empty() {
            return Err(ApplicationError::invalid(
                "process exit requires provider evidence",
            ));
        }
        Ok(WorkerProcessStopStatus::Exited(ObservedWorkerProcessExit {
            binding: self.binding.clone(),
            evidence_ref,
        }))
    }
}

/// Subject-bound LOCAL exit evidence. No wire deserialization and no conversion
/// to whole-attempt acknowledgement. The provider still owns the physical fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedWorkerProcessExit {
    binding: crate::AttemptExecutionBinding,
    evidence_ref: String,
}

impl ObservedWorkerProcessExit {
    pub fn binding(&self) -> &crate::AttemptExecutionBinding {
        &self.binding
    }
    pub fn evidence_ref(&self) -> &str {
        &self.evidence_ref
    }
}

/// Local process observation only. Neither value authorizes Dispatch retirement
/// or replay: independent model, MCP and Sandbox participants may remain active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerProcessStopStatus {
    Unknown,
    Exited(ObservedWorkerProcessExit),
}

/// One exact predecessor, after independent local exit and irreversible Worker
/// closure. Only the Coordinator application constructs this observation subject;
/// neither a Worker request nor a Run's logical terminal status can construct it.
///
/// ```compile_fail
/// use awaken_dispatch::{ClosedAttemptExecution, PhysicalAttempt, ObservedWorkerProcessExit};
/// fn forge(attempt: PhysicalAttempt, process_exit: ObservedWorkerProcessExit) {
///     let _ = ClosedAttemptExecution { attempt, process_exit };
/// }
/// ```
pub struct ClosedAttemptExecution {
    attempt: crate::PhysicalAttempt,
    process_exit: Option<ObservedWorkerProcessExit>,
}

impl ClosedAttemptExecution {
    pub fn attempt(&self) -> &crate::PhysicalAttempt {
        &self.attempt
    }

    pub fn process_exit(&self) -> Option<&ObservedWorkerProcessExit> {
        self.process_exit.as_ref()
    }

    pub fn local_evidence_ref(&self) -> String {
        self.process_exit.as_ref().map_or_else(
            || {
                format!(
                    "dispatch-entry-closed:{}:{}",
                    self.attempt.claim.run_id.0, self.attempt.claim.epoch
                )
            },
            |receipt| receipt.evidence_ref().to_owned(),
        )
    }

    /// Trusted provider assertion, NOT a test for local completion. Call only
    /// after proving complete pre-effect enrollment AND terminal/no-restart
    /// evidence for ALL work belonging to this exact attempt, including SDK
    /// retries, hooks, nested tools and independently hosted Sandboxes. Missing
    /// records, latest-value projections and a closed HTTP connection are not
    /// evidence of absence. Unsupported/legacy execution must remain Unknown.
    /// The reference identifies repeatably queryable provider evidence; it is
    /// not a signature and this constructor cannot verify provider honesty.
    pub fn observed_all_execution_stopped(
        &self,
        evidence_ref: impl Into<String>,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        self.observed_closure(AttemptClosureKind::AllExecutionStopped, evidence_ref.into())
    }

    /// Prove closure of all governed effects after exact Worker closure, with
    /// complete pre-effect scope enrollment. Response-only provider computation
    /// may remain unknown; no business outcome or provider stop is fabricated.
    pub fn observed_execution_domain_closed(
        &self,
        evidence_ref: impl Into<String>,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        self.observed_closure(
            AttemptClosureKind::ControlledExecutionClosed,
            evidence_ref.into(),
        )
    }
    fn observed_closure(
        &self,
        kind: AttemptClosureKind,
        evidence_ref: String,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        if !self.attempt.closing || evidence_ref.trim().is_empty() {
            return Err(ApplicationError::invalid(
                "complete closure requires a durably closed entry and provider evidence",
            ));
        }
        Ok(AttemptStopStatus::Stopped(Box::new(ObservedAttemptStop {
            attempt: self.attempt.clone(),
            evidence_ref,
            kind,
        })))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptClosureKind {
    AllExecutionStopped,
    ControlledExecutionClosed,
}

/// Subject-bound assertion from the trusted stop source, never deserialized
/// from Worker/user JSON. Covers this physical attempt only, not Session cleanup,
/// business outcome, side-effect replay safety, or a successor's authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAttemptStop {
    attempt: crate::PhysicalAttempt,
    evidence_ref: String,
    kind: AttemptClosureKind,
}

impl ObservedAttemptStop {
    pub fn kind(&self) -> AttemptClosureKind {
        self.kind
    }
    pub fn attempt(&self) -> &crate::PhysicalAttempt {
        &self.attempt
    }

    pub fn evidence_ref(&self) -> &str {
        &self.evidence_ref
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptStopStatus {
    Unknown,
    Stopped(Box<ObservedAttemptStop>),
}

/// Trusted, independent infrastructure control, selected by Coordinator startup.
/// Implementations must validate the provider and exact frozen process identity,
/// stop idempotently and preserve evidence. The two methods report DIFFERENT
/// facts; a local-only implementation must retain the default Unknown below.
#[async_trait::async_trait]
pub trait WorkerExecutionStopSource: Send + Sync {
    async fn stop_closed_worker(
        &self,
        execution: &ClosedWorkerExecution,
    ) -> Result<WorkerProcessStopStatus, ApplicationError>;

    /// Release provider-retained process history only after the Registry has
    /// durably stored this exact exit receipt. Implementations must target the
    /// original binding idempotently; failure leaves Dispatch occupied and is
    /// retried through the same blocked-successor path.
    async fn release_closed_worker_evidence(
        &self,
        _execution: &ClosedWorkerExecution,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }

    /// Close/observe the exact governed domains after durable local entry
    /// closure. The persisted closing phase prevents handoff and ordinary ACK
    /// from releasing them during IO. Adapters must target frozen bindings;
    /// absent enrollment/capability remains Unknown, with no initiating replay.
    async fn close_attempt_execution(
        &self,
        _execution: &ClosedAttemptExecution,
        _recovery: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        Ok(AttemptStopStatus::Unknown)
    }
}

/// Progress a blocked successor through the existing authorities, without a
/// recovery queue or a second copy of slot state. The caller bounds provider IO
/// and retries through ordinary admission. `None` means no stop was authorized.
/// The returned observation remains LOCAL only. Retirement additionally requires
/// a separate complete assertion from `close_attempt_execution`; the existing
/// Dispatch slot is the sole retirement authority, not this return value.
pub async fn stop_expired_predecessor(
    dispatch: &dyn ControlDispatchQueue,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    recovery: Option<&dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>,
    successor: &crate::RunClaim,
    now_ms: u64,
) -> Result<Option<WorkerProcessStopStatus>, ApplicationError> {
    if !dispatch
        .claim_is_current(successor)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    {
        return Ok(None);
    }
    let Some(mut predecessor) = dispatch
        .load_physical_attempt(&successor.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
    else {
        return Ok(None);
    };
    let Some(binding) = predecessor.binding.clone() else {
        return Ok(None);
    };
    binding
        .validate_for(&predecessor.claim)
        .map_err(|error| ApplicationError::internal(error.to_string()))?;
    let process_exit = if predecessor.closing {
        persisted_worker_exit(directory, &binding).await?
    } else {
        if predecessor.claim.owner == successor.owner {
            return Ok(None);
        }
        let observation = independently_close_worker(
            dispatch,
            directory,
            source,
            &predecessor,
            &binding,
            successor,
            now_ms,
        )
        .await?;
        let exit = match observation {
            Some(WorkerProcessStopStatus::Exited(exit)) => exit,
            Some(WorkerProcessStopStatus::Unknown) => {
                return Ok(Some(WorkerProcessStopStatus::Unknown));
            }
            None => return Ok(None),
        };
        if dispatch
            .close_attempt(&predecessor.claim)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            != crate::SettleOutcome::Applied
        {
            return Ok(Some(WorkerProcessStopStatus::Exited(exit)));
        }
        predecessor.closing = true;
        Some(exit)
    };
    let observation = process_exit
        .as_ref()
        .map_or(WorkerProcessStopStatus::Unknown, |exit| {
            WorkerProcessStopStatus::Exited(exit.clone())
        });
    if dispatch
        .load_physical_attempt(&successor.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .as_ref()
        != Some(&predecessor)
    {
        return Ok(Some(observation));
    }
    let (Some(recovery), Some(thread_id)) = (recovery, predecessor.thread_id.as_ref()) else {
        return Ok(Some(observation));
    };
    let session_thread_id = predecessor.session_thread_id.as_ref().unwrap_or(thread_id);
    let snapshot = recovery
        .recovery_snapshot_in_session(session_thread_id, thread_id, &predecessor.claim.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
    if snapshot.thread_id != *thread_id || snapshot.claimed_run_id != predecessor.claim.run_id {
        return Err(ApplicationError::internal(
            "recovery source returned another physical attempt subject",
        ));
    }
    let stopped = source
        .close_attempt_execution(
            &ClosedAttemptExecution {
                attempt: predecessor.clone(),
                process_exit,
            },
            &snapshot,
        )
        .await?;
    if let AttemptStopStatus::Stopped(receipt) = &stopped {
        if receipt.attempt() != &predecessor {
            return Err(ApplicationError::internal(
                "closure evidence names another physical attempt",
            ));
        }
        // The closing phase denies normal return/handoff during provider IO.
        // Exact Coordinator closure still cannot clear a successor's slot.
        let retirement = dispatch
            .finish_attempt_closure(&predecessor.claim)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
        tracing::debug!(?retirement, kind = ?receipt.kind(), "independently closed physical attempt");
    }
    Ok(Some(observation))
}

async fn independently_close_worker(
    dispatch: &dyn ControlDispatchQueue,
    directory: &dyn crate::WorkerDirectory,
    source: &dyn WorkerExecutionStopSource,
    predecessor: &crate::PhysicalAttempt,
    binding: &crate::AttemptExecutionBinding,
    successor: &crate::RunClaim,
    now_ms: u64,
) -> Result<Option<WorkerProcessStopStatus>, ApplicationError> {
    let expiry = directory
        .expire_incarnation(binding.worker(), now_ms)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
    if expiry != crate::RegistryMutation::Applied
        && !directory
            .incarnation(binding.worker())
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            .is_some_and(|record| {
                matches!(
                    record.snapshot.state,
                    crate::WorkerState::Quiesced | crate::WorkerState::Dead
                )
            })
    {
        return Ok(None);
    }
    if dispatch
        .load_physical_attempt(&successor.run_id)
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .as_ref()
        != Some(predecessor)
    {
        return Ok(None);
    }
    let closed = ClosedWorkerExecution {
        binding: binding.clone(),
    };
    let observation = if let Some(exit) = persisted_worker_exit(directory, binding).await? {
        WorkerProcessStopStatus::Exited(exit)
    } else {
        source.stop_closed_worker(&closed).await?
    };
    let WorkerProcessStopStatus::Exited(exit) = observation else {
        return Ok(Some(WorkerProcessStopStatus::Unknown));
    };
    if exit.binding() != binding {
        return Err(ApplicationError::internal(
            "Worker exit names another execution",
        ));
    }
    let receipt = crate::WorkerProcessExit::new(
        binding.provider_id(),
        binding.execution_ref(),
        exit.evidence_ref(),
    )
    .map_err(|error| ApplicationError::internal(error.to_string()))?;
    if directory
        .record_process_exit(binding.worker(), receipt.clone())
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        != crate::RegistryMutation::Applied
    {
        return Err(ApplicationError::internal(
            "Worker exit could not be committed",
        ));
    }
    let committed = directory
        .incarnation(binding.worker())
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .and_then(|record| record.process_exit);
    if committed.as_ref() != Some(&receipt) {
        return Err(ApplicationError::internal(
            "Worker exit readback did not match",
        ));
    }
    source.release_closed_worker_evidence(&closed).await?;
    Ok(Some(WorkerProcessStopStatus::Exited(exit)))
}

async fn persisted_worker_exit(
    directory: &dyn crate::WorkerDirectory,
    binding: &crate::AttemptExecutionBinding,
) -> Result<Option<ObservedWorkerProcessExit>, ApplicationError> {
    let receipt = directory
        .incarnation(binding.worker())
        .await
        .map_err(|error| ApplicationError::unavailable(error.to_string()))?
        .and_then(|record| record.process_exit);
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    if receipt.provider_id() != binding.provider_id()
        || receipt.execution_ref() != binding.execution_ref()
    {
        return Err(ApplicationError::internal(
            "persisted Worker exit names another execution",
        ));
    }
    let closed = ClosedWorkerExecution {
        binding: binding.clone(),
    };
    let WorkerProcessStopStatus::Exited(exit) = closed.observed_exit(receipt.evidence_ref())?
    else {
        unreachable!()
    };
    Ok(Some(exit))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplicationErrorKind {
    InvalidRequest,
    Conflict,
    Internal,
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApplicationError {
    pub kind: ApplicationErrorKind,
    pub message: String,
}

impl ApplicationError {
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::InvalidRequest,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::invalid(message)
    }

    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::Conflict,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::Internal,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ApplicationErrorKind::Unavailable,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableSupersedeResult {
    pub state: String,
    pub superseded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableDispatchStatus {
    pub run_id: String,
    pub status: String,
    pub attempts: u64,
    pub sandbox_bound: bool,
}

/// Coordinator application dependency for the private durable operations API.
#[async_trait::async_trait]
pub trait DurableRunOperations: Send + Sync {
    async fn submit_background(
        &self,
        agent: Option<&str>,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<String, ApplicationError>;
    async fn cancel(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError>;
    async fn pause(&self, thread: &str, run_id: Option<&str>) -> Result<String, ApplicationError>;
    async fn resume(&self, thread: &str, text: String) -> Result<String, ApplicationError>;
    async fn wake(&self, thread: &str, run_id: &str) -> Result<(), ApplicationError>;
    async fn deliver(&self, thread: &str, allow: bool) -> Result<String, ApplicationError>;
    async fn supersede(
        &self,
        agent: Option<&str>,
        thread: &str,
        messages: Vec<Message>,
    ) -> Result<DurableSupersedeResult, ApplicationError>;
    async fn messages(&self, thread: &str) -> Result<Vec<Message>, ApplicationError>;
    async fn superseded(&self, thread: &str) -> Result<Vec<String>, ApplicationError>;
    async fn dispatches(
        &self,
        thread: &str,
    ) -> Result<Vec<DurableDispatchStatus>, ApplicationError>;
    async fn reconcile(&self, thread: &str) -> Result<Vec<String>, ApplicationError>;
    async fn quarantine_retry_exhausted(
        &self,
        thread: &str,
        max_attempts: u64,
        now_ms: u64,
    ) -> Result<usize, ApplicationError>;
    async fn dead_letters(&self, thread: &str) -> Result<Vec<String>, ApplicationError>;
    async fn requeue_dead_letter(
        &self,
        thread: &str,
        run_id: &str,
    ) -> Result<bool, ApplicationError>;
    async fn purge_dead_letters(&self, thread: &str) -> Result<usize, ApplicationError>;
}

#[async_trait::async_trait]
pub trait ClaimedOperationCoordinatorResolver: Send + Sync {
    /// Resolve the physical Coordinator selected by the claim-guarded
    /// dispatch. `dispatch` is trusted queue truth, never Worker input. The
    /// resolver may select an authority but cannot replace the one canonical
    /// `OperationCoordinator::commit_operation` write boundary below.
    async fn resolve(
        &self,
        trusted_dispatch: &RunDispatch,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError>;
}

struct FixedOperationCoordinatorResolver(Arc<dyn OperationCoordinator>);

#[async_trait::async_trait]
impl ClaimedOperationCoordinatorResolver for FixedOperationCoordinatorResolver {
    async fn resolve(
        &self,
        _dispatch: &RunDispatch,
    ) -> Result<Arc<dyn OperationCoordinator>, ApplicationError> {
        Ok(self.0.clone())
    }
}

/// Exact-epoch committed-truth application service. Authentication and HTTP
/// status mapping are deliberately outside this owner.
pub struct ClaimedCommitService {
    dispatch: Arc<dyn ControlDispatchQueue>,
    coordinator: Arc<dyn ClaimedOperationCoordinatorResolver>,
}

impl ClaimedCommitService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn ControlDispatchQueue>,
        coordinator: Arc<dyn OperationCoordinator>,
    ) -> Self {
        Self {
            dispatch,
            coordinator: Arc::new(FixedOperationCoordinatorResolver(coordinator)),
        }
    }

    #[must_use]
    pub fn with_coordinator_resolver(
        dispatch: Arc<dyn ControlDispatchQueue>,
        coordinator: Arc<dyn ClaimedOperationCoordinatorResolver>,
    ) -> Self {
        Self {
            dispatch,
            coordinator,
        }
    }

    pub async fn apply_claimed(
        &self,
        request: ClaimedCommitRequest,
    ) -> Result<CommitReceipt, ApplicationError> {
        let guard = self
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?
            .ok_or_else(|| ApplicationError::invalid("run claim is stale"))?;
        let trusted_dispatch = guard.request();
        validate_commit_dispatch_binding(trusted_dispatch, &request.operation.commit)
            .map_err(ApplicationError::invalid)?;
        if request.operation.operation_id.run_id != *trusted_dispatch.run_id() {
            return Err(ApplicationError::invalid(format!(
                "commit operation run {} does not match claimed dispatch run {}",
                request.operation.operation_id.run_id.0,
                trusted_dispatch.run_id().0
            )));
        }
        let expected_hash = commit_payload_hash(&request.operation.commit)
            .map_err(|error| ApplicationError::invalid(error.to_string()))?;
        if expected_hash != request.operation.payload_hash {
            return Err(ApplicationError::invalid(
                "commit operation payload hash does not match ThreadCommit",
            ));
        }
        let coordinator = self.coordinator.resolve(trusted_dispatch).await?;
        let committed = coordinator
            .commit_operation(request.operation)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()));
        match guard.release_after(committed).await {
            Ok(committed) => committed,
            Err(error) => Err(ApplicationError::unavailable(format!(
                "commit claim lock release failed: {error}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_receipt_freezes_all_physical_identity_coordinates() {
        // R28 exact typed assertion => equal; independently vary Run, owner,
        // epoch, optional binding, logical/Session Thread, Worker
        // id/incarnation/generation, provider and execution reference =>
        // unequal. This is the same derived equality the
        // application uses before finish_attempt, not string concatenation or a
        // Run-only comparison. Blank evidence rejects. Constructor privacy is
        // covered by the compile-fail doctest; provider truth is outside this test.
        let worker = crate::WorkerIdentity::new("worker", "boot", 1);
        let binding =
            crate::AttemptExecutionBinding::new(worker.clone(), "provider", "instance").unwrap();
        let attempt = crate::PhysicalAttempt {
            closing: true,
            claim: crate::RunClaim {
                run_id: awaken_agent_contract::agent::run::Id("run".into()),
                owner: worker.lease_owner(),
                epoch: 1,
            },
            binding: Some(binding.clone()),
            thread_id: Some(awaken_agent_contract::agent::thread::Id("thread".into())),
            session_thread_id: Some(awaken_agent_contract::agent::thread::Id("session".into())),
        };
        let command = ClosedAttemptExecution {
            attempt: attempt.clone(),
            process_exit: Some(ObservedWorkerProcessExit {
                binding,
                evidence_ref: "local-exit".into(),
            }),
        };
        assert!(command.observed_all_execution_stopped("\n\t").is_err());
        let AttemptStopStatus::Stopped(receipt) =
            command.observed_all_execution_stopped("complete").unwrap()
        else {
            panic!("positive witness")
        };
        assert_eq!(receipt.attempt(), &attempt);
        for coordinate in [
            "run",
            "owner",
            "epoch",
            "missing-binding",
            "thread",
            "session-thread",
            "worker",
            "boot",
            "generation",
            "provider",
            "instance",
        ] {
            let mut other = attempt.clone();
            let mut other_worker = worker.clone();
            match coordinate {
                "run" => other.claim.run_id.0.push('2'),
                "owner" => other.claim.owner.push('2'),
                "epoch" => other.claim.epoch += 1,
                "missing-binding" => other.binding = None,
                "thread" => other.thread_id.as_mut().unwrap().0.push('2'),
                "session-thread" => other.session_thread_id.as_mut().unwrap().0.push('2'),
                "worker" => other_worker.worker_id.push('2'),
                "boot" => other_worker.incarnation_id.push('2'),
                "generation" => other_worker.generation += 1,
                _ => {}
            }
            if ![
                "run",
                "owner",
                "epoch",
                "missing-binding",
                "thread",
                "session-thread",
            ]
            .contains(&coordinate)
            {
                other.binding = Some(
                    crate::AttemptExecutionBinding::new(
                        other_worker,
                        if coordinate == "provider" {
                            "provider2"
                        } else {
                            "provider"
                        },
                        if coordinate == "instance" {
                            "instance2"
                        } else {
                            "instance"
                        },
                    )
                    .unwrap(),
                );
            }
            assert_ne!(receipt.attempt(), &other, "{coordinate}");
        }
    }

    #[test]
    fn application_error_kinds_preserve_http_mapping_causes() {
        // Cause/effect graph: C1 caller input/claim is invalid -> E1 4xx;
        // C2 state conflicts -> E2 409; C3 permanent internal failure -> E3 500;
        // C4 temporary dependency failure -> E4 retryable 503.
        // The application port preserves these causes without importing HTTP.
        //
        // | Rule | cause            | effect kind     |
        // | R1   | invalid request  | InvalidRequest  |
        // | R2   | state conflict   | Conflict        |
        // | R3   | internal fault   | Internal        |
        // | R4   | dependency down  | Unavailable     |
        assert_eq!(
            ApplicationError::invalid("x").kind,
            ApplicationErrorKind::InvalidRequest
        );
        assert_eq!(
            ApplicationError::conflict("x").kind,
            ApplicationErrorKind::Conflict
        );
        assert_eq!(
            ApplicationError::internal("x").kind,
            ApplicationErrorKind::Internal
        );
        assert_eq!(
            ApplicationError::unavailable("x").kind,
            ApplicationErrorKind::Unavailable
        );
    }
}
