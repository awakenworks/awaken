//! Worker-side Session realization control over the authenticated Coordinator transport.

use awaken_run_ingress_contract::{ClaimedSessionControl, ClaimedSessionControlError};
use awaken_run_ingress_contract::{RunClaim, WorkerIdentity};

/// Standard client using the same registered identity-bound Worker transport as
/// lifecycle, dispatch, recovery, and claimed commits.
#[derive(Clone)]
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
impl awaken_session_contract::SessionEnvironmentBindingSink for WorkerControlSessionClient {
    async fn authorize(
        &self,
        intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_contract::RunError,
    > {
        self.control
            .authorize_session_environment_effect(&self.identity, intent)
            .await
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        self.control
            .persist_session_environment_receipt(&self.identity, &receipt)
            .await
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
    }

    async fn list_session_agents(
        &self,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::SessionAgentRosterEntry>, ClaimedSessionControlError>
    {
        self.control
            .list_session_agents(&self.identity, claim, session_id)
            .await
    }

    async fn admit_session_model_request(
        &self,
        claim: &RunClaim,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, ClaimedSessionControlError> {
        self.control
            .admit_session_model_request(&self.identity, claim, session_id, thread_id, run_id)
            .await
    }

    async fn admit_session_run_activity(
        &self,
        claim: &RunClaim,
        session_id: &str,
        agent_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
        mode: awaken_session_contract::SessionRunActivityAdmissionMode,
    ) -> Result<awaken_session_contract::SessionRunActivityAdmission, ClaimedSessionControlError>
    {
        self.control
            .admit_session_run_activity(&self.identity, claim, session_id, agent_id, run_id, mode)
            .await
    }

    async fn send_session_agent_message(
        &self,
        claim: &RunClaim,
        command: awaken_session_contract::SessionAgentMessageCommand,
    ) -> Result<awaken_session_contract::SessionAgentMessageReceipt, ClaimedSessionControlError>
    {
        self.control
            .send_session_agent_message(&self.identity, claim, command)
            .await
    }

    async fn settle_session_agent_boundary(
        &self,
        claim: &RunClaim,
        command: awaken_session_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), ClaimedSessionControlError> {
        self.control
            .settle_session_agent_boundary(&self.identity, claim, command)
            .await
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
        let _ = command;
        // Initial assignment/reassignment is returned by the claim-guarded
        // Session resume endpoint. A Worker may drive that directive and renew
        // its exact fence, but cannot open a second unguarded assignment path.
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "Worker Session realization begin requires a claimed resume assignment".into(),
            ),
        )
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationLease,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .renew_session_realization(&self.identity, command)
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

    async fn terminal_cleanup_work(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupWork>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .terminal_cleanup_work(&self.identity, session_id, lease)
            .await
    }

    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .authorize_terminal_cleanup_effect(&self.identity, effect)
            .await
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_cleanup_preparation(&self.identity, lease, preparation)
            .await
    }

    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        self.control
            .authorize_terminal_cleanup_disposal(&self.identity, effect)
            .await
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_cleanup_disposal(&self.identity, lease, receipt)
            .await
    }

    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupAssignment>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .claim_next_terminal_cleanup(&self.identity, target)
            .await
    }

    async fn terminal_repository_publication_command(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.control
            .terminal_repository_publication_command(&self.identity, session_id, lease)
            .await
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_repository_publication_receipt(
                &self.identity,
                session_id,
                lease,
                receipt,
            )
            .await
    }
    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_repository_publication_rejection(
                &self.identity,
                session_id,
                lease,
                rejection,
            )
            .await
    }
}
