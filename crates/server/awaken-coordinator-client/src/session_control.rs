// @formal surface exclude=transport_adapter
// @formal owner crates/server/awaken-coordinator-client/src/worker_control_client.rs
// @formal reason: Static owner: Worker-side implementation of neutral Session-control ports. Dynamic path:
// @formal reason: each method forwards the exact Worker identity, claim, and command to
// @formal reason: WorkerControlClient; authentication and state decisions remain server-side. Evidence:
// @formal reason: crates/server/awaken-coordinator-client/src/worker_control_client.rs.
//! Worker-side Session realization control over the authenticated Coordinator transport.

use awaken_dispatch_contract::{RunClaim, WorkerIdentity};
use awaken_session_realization_contract::ClaimedSessionControlError;

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
impl awaken_session_execution_contract::SessionEnvironmentBindingSink
    for WorkerControlSessionClient
{
    async fn authorize(
        &self,
        intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        self.control
            .authorize_session_environment_effect(&self.identity, intent)
            .await
    }

    async fn persist(
        &self,
        receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentState,
        awaken_session_types::RunError,
    > {
        self.control
            .persist_session_environment_receipt(&self.identity, &receipt)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::ClaimedSessionRealizationControl
    for WorkerControlSessionClient
{
    async fn resume_frozen(
        &self,
        claim: &RunClaim,
        session_id: &str,
        session_work_authority: Option<
            &awaken_session_application_contract::SessionWorkAuthorityReceipt,
        >,
        continuation: Option<
            &awaken_session_configuration_contract::resource::SessionRealizationLease,
        >,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRealizationDirective>,
        ClaimedSessionControlError,
    > {
        self.control
            .resume_session(
                &self.identity,
                claim,
                session_id,
                session_work_authority,
                continuation,
            )
            .await
    }
}

#[async_trait::async_trait]
impl
    awaken_session_execution_contract::ClaimedSessionCoordinationControl<
        awaken_dispatch_contract::RunClaim,
    > for WorkerControlSessionClient
{
    async fn list_session_agents(
        &self,
        claim: &RunClaim,
        session_id: &str,
    ) -> Result<
        Vec<awaken_session_execution_contract::SessionAgentRosterEntry>,
        ClaimedSessionControlError,
    > {
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
        mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        ClaimedSessionControlError,
    > {
        self.control
            .admit_session_run_activity(&self.identity, claim, session_id, agent_id, run_id, mode)
            .await
    }

    async fn send_session_agent_message(
        &self,
        claim: &RunClaim,
        command: awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        ClaimedSessionControlError,
    > {
        self.control
            .send_session_agent_message(&self.identity, claim, command)
            .await
    }

    async fn settle_session_agent_boundary(
        &self,
        claim: &RunClaim,
        command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), ClaimedSessionControlError> {
        self.control
            .settle_session_agent_boundary(&self.identity, claim, command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionRealizationControl for WorkerControlSessionClient {
    async fn begin_session_realization(
        &self,
        command: awaken_session_realization_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        let _ = command;
        // Initial assignment/reassignment is returned by the claim-guarded
        // Session resume endpoint. A Worker may drive that directive and renew
        // its exact fence, but cannot open a second unguarded assignment path.
        Err(
            awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                "Worker Session realization begin requires a claimed resume assignment".into(),
            ),
        )
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_realization_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationLeaseReceipt,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.control
            .renew_session_realization(&self.identity, command)
            .await
    }

    async fn activate_session_realization(
        &self,
        command: awaken_session_realization_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.control
            .activate_session_realization(&self.identity, command)
            .await
    }

    async fn acknowledge_session_realization(
        &self,
        command: awaken_session_realization_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_realization_contract::SessionRealizationDirective,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.control
            .acknowledge_session_realization(&self.identity, command)
            .await
    }

    async fn fail_session_realization(
        &self,
        command: awaken_session_realization_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        self.control
            .fail_session_realization(&self.identity, command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalCleanupQueue
    for WorkerControlSessionClient
{
    async fn terminal_cleanup_work(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionTerminalCleanupWork>,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.control
            .terminal_cleanup_work(&self.identity, session_id.as_str(), lease)
            .await
    }

    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_realization_contract::SessionRealizationTarget,
        after_session_id: Option<&awaken_session_types::SessionId>,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionTerminalCleanupAssignment>,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.control
            .claim_next_terminal_cleanup(&self.identity, target, after_session_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalPreparationControl
    for WorkerControlSessionClient
{
    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.control
            .authorize_terminal_cleanup_effect(&self.identity, effect)
            .await
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        preparation: awaken_session_state_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_cleanup_preparation(&self.identity, lease, preparation)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalDisposalControl
    for WorkerControlSessionClient
{
    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, awaken_session_realization_contract::SessionRealizationControlFailure> {
        self.control
            .authorize_terminal_cleanup_disposal(&self.identity, effect)
            .await
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        receipt: awaken_session_state_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_cleanup_disposal(&self.identity, lease, receipt)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionTerminalRepositoryPublicationControl
    for WorkerControlSessionClient
{
    async fn terminal_repository_publication_command(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRepositoryPublicationProjection>,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        self.control
            .terminal_repository_publication_command(&self.identity, session_id.as_str(), lease)
            .await
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        receipt: awaken_session_state_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_repository_publication_receipt(
                &self.identity,
                session_id.as_str(),
                lease,
                receipt,
            )
            .await
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        rejection: awaken_session_state_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationControlFailure> {
        self.control
            .record_terminal_repository_publication_rejection(
                &self.identity,
                session_id.as_str(),
                lease,
                rejection,
            )
            .await
    }
}
