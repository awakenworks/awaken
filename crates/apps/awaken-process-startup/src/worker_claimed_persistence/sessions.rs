// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The server calls original Session use cases with the final selected claim-bound CAS gate.
//! Three claim-scoped Session verbs and exact terminal settlement observation.
use super::{
    ClaimPurpose, PostgresWorkerClaims, RootGate, RunClaim, RunId, SessionId, ThreadId,
    WorkAdmission, unavailable,
};
use awaken_dispatch_http::{WorkerSessionClaims, WorkerSessionResumeCommand};
use awaken_runtime_contract::runs::RunError;
use awaken_session_application::SessionRootCommit as _;
use awaken_session_execution_contract::{
    BeginSessionRealization, RenewSessionRealization, SessionRealizationDirective,
};
use std::sync::Arc;

#[async_trait::async_trait]
impl WorkerSessionClaims for PostgresWorkerClaims {
    async fn settlement_snapshot(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let mut transaction = self.transaction(ClaimPurpose::Recovery).await?;
        let Some(current) = awaken_dispatch::lock_running_claim_for_commit(&mut transaction, claim)
            .await
            .map_err(super::internal)?
        else {
            transaction.rollback().await.map_err(super::internal)?;
            return Ok(None);
        };
        if !current.physical_slot.permits_settlement() {
            transaction.rollback().await.map_err(super::internal)?;
            return Ok(None);
        }
        // Settlement closes exact Ended work; it grants no execution and must
        // not acquire or renew a Work lease that has already been retired.
        let loaded = self
            .recovery_on(&mut transaction, &current.request, None)
            .await?;
        transaction.rollback().await.map_err(super::internal)?;
        match loaded {
            super::LoadedRunRecovery::Full { snapshot, .. } => Ok(Some(*snapshot)),
            super::LoadedRunRecovery::Reuse { .. } => Err(RunError::internal(
                "settlement cannot reuse an unsupplied prefix",
            )),
        }
    }

    async fn resume(
        &self,
        command: WorkerSessionResumeCommand,
        preparation: &dyn awaken_session_execution_contract::SessionRealizationControl,
    ) -> Result<Option<SessionRealizationDirective>, awaken_dispatch_http::RealizationHttpError>
    {
        // Existing Runtime refusals keep the original selected-PG Boundary
        // envelope. Do not route them through From<RunError>'s Binding variant.
        let (mut gate, expected_resources, receipt) = async {
            // Validate affinity before the original Work renewal can be committed.
            let mut transaction = self.transaction(ClaimPurpose::Resume).await?;
            let current = self
                .observe(&mut transaction, &command.claim, ClaimPurpose::Resume)
                .await?;
            let expected_resources = command.resources_for(&current.request)?;
            let receipt = command.work_receipt()?.clone();
            self.work(
                &mut transaction,
                &current.request,
                &receipt,
                &command.claim.owner,
            )
            .await?;
            command.validate_continuation()?;
            let gate = RootGate {
                selected: Arc::new(self.clone()),
                claim: command.claim.clone(),
                original: current.request,
                purpose: ClaimPurpose::Resume,
                work: WorkAdmission::Asserted(receipt.clone()),
                committed: None,
            };
            transaction.commit().await.map_err(unavailable)?;
            Ok::<_, RunError>((gate, expected_resources, receipt))
        }
        .await
        .map_err(awaken_dispatch_http::RealizationHttpError::worker_boundary)?;
        let control = self.application.realizations();
        if let Some(asserted) = &command.continuation {
            let renewed = control
                .renew_session_realization_with_commit(
                    RenewSessionRealization {
                        session_id: command.session_id.clone(),
                        asserted_lease: asserted.clone(),
                        requested_expires_at_unix_ms: command
                            .target
                            .lease_expires_at_unix_ms
                            .max(asserted.expires_at_unix_ms),
                    },
                    &gate,
                )
                .await
                .map_err(awaken_dispatch_http::RealizationHttpError::Control)?;
            if !awaken_session_configuration_contract::resource::realization_lease_generation_authorizes(&renewed, asserted) {
                return Err(awaken_dispatch_http::RealizationHttpError::Control(
                    awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership,
                ));
            }
        }
        // Match the original HTTP sequence: optional exact renewal precedes
        // the Begin-only cancellation/physical-quiescence exclusion. Both
        // phases use this same Gate and Read Committed transaction policy.
        gate.purpose = ClaimPurpose::ResumeBegin;
        gate.validate_claim()
            .await
            .map_err(awaken_session_execution_contract::SessionRealizationControlFailure::from)
            .map_err(awaken_dispatch_http::RealizationHttpError::Control)?;
        let begin = BeginSessionRealization {
            session_id: command.session_id.clone(),
            expected_resources,
            target: command.target,
        };
        // The product callback holds only this final admission capability.
        // Every credential update and no-change result re-enters the same Gate;
        // no Dispatch connection remains acquired while the issuer is awaited.
        let context = super::claimed_preparation::PostgresClaimedPreparation::new(&gate, &receipt);
        preparation
            .prepare_claimed_session_credentials(&context, &begin)
            .await
            .map_err(awaken_dispatch_http::RealizationHttpError::Control)?;
        gate.validate_claim()
            .await
            .map_err(awaken_session_execution_contract::SessionRealizationControlFailure::from)
            .map_err(awaken_dispatch_http::RealizationHttpError::Control)?;
        let directive = control
            .begin_session_realization_with_commit(begin, &gate)
            .await
            .map_err(awaken_dispatch_http::RealizationHttpError::Control)?;
        if !receipt.binds_realization(
            command.session_id.as_str(),
            &directive.authority.lease.runtime_incarnation,
            directive.projection.baseline.environment.self_hosted,
            &directive.projection.baseline.environment.environment_id,
        ) {
            return Err(awaken_dispatch_http::RealizationHttpError::worker_boundary(
                RunError::bad_request(
                    "Session realization directive does not match claimed Work authority",
                ),
            ));
        }
        if let Some(asserted) = &command.continuation
            && !awaken_session_configuration_contract::resource::realization_lease_generation_authorizes(&directive.authority.lease, asserted)
        {
            return Err(awaken_dispatch_http::RealizationHttpError::Control(
                    awaken_session_execution_contract::SessionRealizationControlFailure::StaleOwnership,
                ));
        }
        Ok(Some(directive))
    }

    async fn admit_model_request(
        &self,
        claim: &RunClaim,
        session: &SessionId,
        thread: &ThreadId,
        run: &RunId,
    ) -> Result<bool, RunError> {
        let mut gate = self
            .context(
                claim,
                ClaimPurpose::Model,
                WorkAdmission::NotRequiredByOperation,
            )
            .await?;
        if gate.original.session_thread_id().0 != session.as_str()
            || gate.original.thread_id() != thread
            || gate.original.run_id() != run
            || run != &claim.run_id
        {
            return Err(RunError::bad_request(
                "model-request admission does not match the claimed Session dispatch",
            ));
        }
        self.freeze_committed_read(&mut gate).await?;
        self.application
            .coordination()
            .admit_session_model_request_with_commit(session, thread, run, &gate)
            .await
    }

    async fn admit_run_activity(
        &self,
        claim: &RunClaim,
        session: &SessionId,
        agent: &str,
        run: &RunId,
        mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<awaken_session_execution_contract::SessionRunActivityAdmission, RunError> {
        let mut gate = self
            .context(
                claim,
                ClaimPurpose::Activity(mode),
                WorkAdmission::NotRequiredByOperation,
            )
            .await?;
        self.validate_activity(&gate, session, agent, run, mode)
            .await?;
        let coordination = self.application.coordination();
        if gate.original.identity_scope
            == awaken_dispatch_contract::DispatchIdentityScope::PreparedSessionCommand
        {
            let revision = coordination
                .prepare_prepared_session_run_activity_with_commit(session, agent, run, mode, &gate)
                .await?;
            let cursor = if revision.is_some() {
                self.freeze_prepared_activity(&mut gate).await?
            } else {
                0
            };
            coordination
                .admit_prepared_session_run_activity_with_commit(
                    session,
                    run,
                    mode,
                    awaken_session_execution_contract::PreparedSessionRunActivity {
                        publication_condition: None,
                        projected_revision: revision,
                        admitted_after_cursor: cursor,
                    },
                    &gate,
                )
                .await
        } else {
            self.freeze_committed_read(&mut gate).await?;
            coordination
                .admit_session_run_activity_with_commit(session, agent, run, mode, &gate)
                .await
        }
    }
}
