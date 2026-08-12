//! Worker-side Session realization control over the authenticated Coordinator transport.

use awaken_run_ingress_contract::{ClaimedSessionControl, ClaimedSessionControlError};
use awaken_run_ingress_contract::{RunClaim, WorkerIdentity};

/// Standard client using the same registered identity-bound Worker transport as
/// lifecycle, dispatch, recovery, and claimed commits.
pub struct WorkerControlSessionClient {
    control: crate::WorkerControlClient,
    identity: WorkerIdentity,
}

impl WorkerControlSessionClient {
    #[must_use]
    pub fn new(control: crate::WorkerControlClient, identity: WorkerIdentity) -> Self {
        Self { control, identity }
    }
}

#[async_trait::async_trait]
impl ClaimedSessionControl for WorkerControlSessionClient {
    async fn resume_frozen(
        &self,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionRealizationDirective>,
        ClaimedSessionControlError,
    > {
        self.control
            .resume_session(&self.identity, claim, session_id)
            .await
            .map_err(ClaimedSessionControlError::new)
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for WorkerControlSessionClient {
    async fn begin_session_realization(
        &self,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .begin_session_realization(&self.identity, command)
            .await
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .activate_session_realization(&self.identity, command)
            .await
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .acknowledge_session_realization(&self.identity, command)
            .await
    }

    async fn fail_session_realization(
        &self,
        command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.control
            .fail_session_realization(&self.identity, command)
            .await
    }
}
