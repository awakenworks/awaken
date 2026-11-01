// @formal surface exclude=test_only
// @formal reason: One shared fake records endpoint claims and future lifetimes; it is reachable only from the cfg(test) coordination modules.
//! One remote-control fixture for endpoint routing, exact claims and lifetimes.

use super::*;
use awaken_agent_contract::agent::{run::Id as RunId, thread::Id as ThreadId};
use awaken_session_execution_contract::ClaimedSessionControlError;
use std::sync::Mutex;

#[derive(Default)]
pub(super) struct RemoteControl {
    pub(super) calls: Mutex<Vec<(&'static str, RunClaim)>>,
    pub(super) gate: Option<Arc<AdmissionGate>>,
    pub(super) fail: bool,
    pub(super) model_error: Option<ClaimedSessionControlError>,
}

impl RemoteControl {
    async fn query(
        &self,
        name: &'static str,
        claim: &RunClaim,
    ) -> Result<(), ClaimedSessionControlError> {
        self.calls.lock().unwrap().push((name, claim.clone()));
        if let Some(gate) = &self.gate {
            let _lifetime = AdmissionDropReceipt(gate.clone());
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        if self.fail {
            Err(ClaimedSessionControlError::new(
                "original remote query failure",
            ))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl ClaimedSessionCoordinationControl<RunClaim> for RemoteControl {
    async fn list_session_agents(
        &self,
        claim: &RunClaim,
        _session: &str,
    ) -> Result<Vec<SessionAgentRosterEntry>, ClaimedSessionControlError> {
        self.query("list", claim).await?;
        Ok(Vec::new())
    }

    async fn send_session_agent_message(
        &self,
        claim: &RunClaim,
        command: SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageReceipt, ClaimedSessionControlError> {
        self.query("send", claim).await?;
        Ok(SessionAgentMessageReceipt {
            thread_id: command.source_thread_id,
        })
    }

    async fn admit_session_model_request(
        &self,
        claim: &RunClaim,
        _session: &str,
        _thread: &ThreadId,
        _run: &RunId,
    ) -> Result<bool, ClaimedSessionControlError> {
        self.calls.lock().unwrap().push(("model", claim.clone()));
        match &self.model_error {
            Some(error) => Err(error.clone()),
            None => Ok(true),
        }
    }

    async fn admit_session_run_activity(
        &self,
        claim: &RunClaim,
        _session: &str,
        _agent: &str,
        _run: &RunId,
        _mode: SessionRunActivityAdmissionMode,
    ) -> Result<SessionRunActivityAdmission, ClaimedSessionControlError> {
        self.calls.lock().unwrap().push(("activity", claim.clone()));
        Ok(SessionRunActivityAdmission::Rejected)
    }

    async fn settle_session_agent_boundary(
        &self,
        claim: &RunClaim,
        _command: SessionAgentBoundaryCommand,
    ) -> Result<(), ClaimedSessionControlError> {
        self.calls.lock().unwrap().push(("settle", claim.clone()));
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRealizationControl for RemoteControl {
    async fn begin_session_realization(
        &self,
        _command: awaken_session_execution_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn activate_session_realization(
        &self,
        _command: awaken_session_execution_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn acknowledge_session_realization(
        &self,
        _command: awaken_session_execution_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationDirective,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_execution_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationControlFailure> {
        Err(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn renew_session_realization(
        &self,
        _command: awaken_session_execution_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationLeaseReceipt,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_execution_contract::SessionRealizationControlFailure::NotReady)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ClaimedSessionRealizationControl for RemoteControl {
    async fn resume_frozen(
        &self,
        _claim: &RunClaim,
        _session_id: &str,
        _session_work_authority: Option<
            &awaken_session_application_contract::SessionWorkAuthorityReceipt,
        >,
        _continuation: Option<
            &awaken_session_configuration_contract::resource::SessionRealizationLease,
        >,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionRealizationDirective>,
        awaken_session_execution_contract::ClaimedSessionControlError,
    > {
        Ok(None)
    }
}
