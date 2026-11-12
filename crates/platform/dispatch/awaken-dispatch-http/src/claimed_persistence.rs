// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Product-selected claimed operations retain the existing claim and application decisions.
//! Process-selected persistence for the existing claimed Worker verbs.
//! The HTTP owner retains identity authentication. These ports are selected
//! once by the store composition; they neither open a pool nor own retry state.
use std::sync::Arc;

use awaken_dispatch_contract::{ClaimedCheckpointTransport, RunDispatch};
use awaken_runtime_contract::runs::RunError;
use awaken_session_application_contract::SessionWorkAuthorityReceipt;
use awaken_session_configuration_contract::resource::{
    SessionRealizationLease, SessionResourceManifest,
};
use awaken_session_execution_contract::{SessionRealizationDirective, SessionRealizationTarget};
use awaken_session_types::SessionId;
use awaken_worker_contract::RunClaim;

/// The store assembly must state the actual connection relationship. This is
/// not an optional capability: PostgreSQL dispatch always selects its composed
/// operations, including when its Session/Work stores are independent.
#[derive(Clone)]
pub enum WorkerClaimPersistence {
    Independent,
    Postgres {
        checkpoints: Arc<dyn ClaimedCheckpointTransport<RunError>>,
        sessions: Arc<dyn WorkerSessionClaims>,
    },
}

/// Inputs already bound to the verified registered Worker at the HTTP edge.
/// Session resources still come exclusively from the exact Dispatch observation.
#[derive(Clone)]
pub struct WorkerSessionResumeCommand {
    pub claim: RunClaim,
    pub session_id: SessionId,
    pub work_authority: Option<SessionWorkAuthorityReceipt>,
    pub continuation: Option<SessionRealizationLease>,
    pub target: SessionRealizationTarget,
    pub registry_expires_at_unix_ms: u64,
}

impl WorkerSessionResumeCommand {
    /// The same binding checks apply to the independent and borrowed paths.
    /// These facts authorize nothing after the protecting claim is released.
    pub fn resources_for(
        &self,
        dispatch: &RunDispatch,
    ) -> Result<Option<SessionResourceManifest>, RunError> {
        if dispatch.run_id() != &self.claim.run_id {
            return Err(RunError::bad_request(
                "guarded dispatch does not match the Session resume claim",
            ));
        }
        if dispatch.session_thread_id().0 != self.session_id.as_str() {
            return Err(RunError::bad_request(
                "Session resume target does not match the claimed Run",
            ));
        }
        dispatch
            .session_resources
            .as_ref()
            .map(|envelope| envelope.decode_with(SessionResourceManifest::decode_resources))
            .transpose()
            .map_err(|error| RunError::bad_request(error.to_string()))
    }
    pub fn work_receipt(&self) -> Result<&SessionWorkAuthorityReceipt, RunError> {
        let receipt = self
            .work_authority
            .as_ref()
            .ok_or_else(|| RunError::bad_request("Session resume requires exact Work authority"))?;
        if receipt.session_id() != self.session_id
            || receipt.worker_owner() != self.claim.owner
            || self.target.runtime_incarnation != self.claim.owner
        {
            return Err(RunError::bad_request(
                "Session resume Work authority identity does not match the claim",
            ));
        }
        Ok(receipt)
    }

    pub fn validate_continuation(&self) -> Result<(), RunError> {
        if let Some(asserted) = &self.continuation
            && (asserted.owner != self.target.owner
                || asserted.runtime_incarnation != self.target.runtime_incarnation
                || asserted.expires_at_unix_ms > self.registry_expires_at_unix_ms)
        {
            return Err(RunError::bad_request(
                "Session continuation exceeds authenticated Worker authority",
            ));
        }
        Ok(())
    }
}

/// Server-side implementation of the existing one Resume use case. It stays
/// beside Worker HTTP composition; no Runtime SDK role or generic transaction
/// interface is introduced by this boundary.
#[async_trait::async_trait]
pub trait WorkerSessionClaims: Send + Sync {
    /// Terminal observation grants no execution and must not renew a retired
    /// Work lease. The original settlement owner verifies Ended and quiescence.
    async fn settlement_snapshot(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>;

    async fn resume(
        &self,
        command: WorkerSessionResumeCommand,
    ) -> Result<Option<SessionRealizationDirective>, RunError>;

    async fn admit_model_request(
        &self,
        claim: &RunClaim,
        session: &SessionId,
        thread: &awaken_agent_contract::ThreadId,
        run: &awaken_agent_contract::RunId,
    ) -> Result<bool, RunError>;

    async fn admit_run_activity(
        &self,
        claim: &RunClaim,
        session: &SessionId,
        agent: &str,
        run: &awaken_agent_contract::RunId,
        mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<awaken_session_execution_contract::SessionRunActivityAdmission, RunError>;
}

/// The one reservation-to-Session activity binding rule, over the protected row.
pub fn require_worker_run_activity(
    observation: &awaken_dispatch_contract::CommitEpochObservation,
    claim: &RunClaim,
    session: &SessionId,
    agent: &str,
    run: &awaken_agent_contract::RunId,
    mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    now_ms: u64,
) -> Result<(), RunError> {
    use awaken_session_execution_contract::SessionRunActivityAdmissionMode as Mode;
    let dispatch = &observation.request;
    let expected = if observation.cancellation_requested {
        Mode::RecoverOnly
    } else {
        Mode::RecoverOrAdmit
    };
    if !awaken_time::Expiry::from_epoch_ms(observation.expires_ms)
        .is_live_at(awaken_time::UtcTime::from_epoch_ms(now_ms))
        || run != &claim.run_id
        || run != dispatch.run_id()
        || dispatch.thread_id() != dispatch.session_thread_id()
        || dispatch.thread_id().0 != session.as_str()
        || dispatch.session_activity_epoch.is_some()
        || dispatch.activation.snapshot.root_agent_id.0 != agent
        || mode != expected
    {
        return Err(RunError::bad_request(
            "Session Run activity admission does not match the claimed reservation",
        ));
    }
    Ok(())
}
