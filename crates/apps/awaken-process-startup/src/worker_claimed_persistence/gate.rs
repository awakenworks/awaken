// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The final claim/Work/Root check composes existing kernels on the selected connection.
//! Final Root CAS and exact observation boundary; no cold application I/O owns D.
use super::{
    ClaimConnectionUse, ClaimPurpose, LoadedRunRecovery, RootGate, SessionId, WorkAdmission,
    mutation, unavailable,
};
use awaken_runtime_contract::runs::RunError;
use awaken_session_application::{SessionMutationError, SessionRootCommit};
use awaken_session_contract::{SessionMutation, SessionMutationResult};
use awaken_session_types::SessionRevision;
use sqlx::PgConnection;

impl RootGate {
    pub(super) async fn check(&self, connection: &mut PgConnection) -> Result<(), RunError> {
        let observation = self
            .selected
            .observe(connection, &self.claim, self.purpose)
            .await?;
        if !observation.request.same_original_dispatch(&self.original) {
            return Err(RunError::bad_request(
                "guarded dispatch changed during Session preparation",
            ));
        }
        if let ClaimPurpose::Activity(mode) = self.purpose {
            awaken_dispatch_http::require_worker_run_activity(
                &observation,
                &self.claim,
                &SessionId::from(self.original.session_thread_id().0.clone()),
                &self.original.activation.snapshot.root_agent_id.0,
                &self.claim.run_id,
                mode,
                awaken_time::WallClockExt::now_ms(self.selected.clock.as_ref()),
            )?;
            if !observation.physical_slot.permits_settlement() {
                return Err(RunError::internal(
                    "prepared reservation requires a live claim and empty physical slot",
                ));
            }
        }
        if let WorkAdmission::Asserted(receipt) = &self.work {
            self.selected
                .work(connection, &observation.request, receipt, &self.claim.owner)
                .await?;
        }
        if let Some(frozen) = &self.committed {
            if &frozen.thread != self.original.thread_id() || &frozen.run != self.original.run_id()
            {
                return Err(RunError::internal(
                    "committed read fence differs from its claimed scope",
                ));
            }
            let observed = self
                .selected
                .recovery_on(connection, &self.original, Some(frozen.prefix))
                .await?;
            if !matches!(observed, LoadedRunRecovery::Reuse { .. }) {
                return Err(RunError::conflict(
                    "committed Thread changed during Session admission",
                ));
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionRootCommit for RootGate {
    async fn validate_claim(&self) -> Result<(), SessionMutationError> {
        #[cfg(test)]
        if matches!(self.purpose, ClaimPurpose::Activity(_))
            && let Some(cut) = &self.selected.cut
        {
            cut.pause(super::test_cut::FinalAdmissionCutKind::AbsentActivityReceipt)
                .await;
        }
        let mut transaction = self
            .selected
            .transaction(self.purpose)
            .await
            .map_err(SessionMutationError::admission)?;
        self.check(&mut transaction)
            .await
            .map_err(SessionMutationError::admission)?;
        if matches!(self.purpose, ClaimPurpose::Activity(_)) {
            let session = SessionId::from(self.original.session_thread_id().0.clone());
            // H9: a cold negative receipt observation is not final rejection.
            // Serialize this exact re-read with the original Root commit lock.
            match self.selected.roots {
                ClaimConnectionUse::Borrowed => {
                    awaken_session_store::lock_postgres_session_root_on(&mut transaction, &session)
                        .await
                        .map_err(mutation)?;
                }
                ClaimConnectionUse::Independent => {
                    self.selected
                        .views
                        .roots
                        .get(&session)
                        .await
                        .map_err(mutation)?;
                }
            }
            let operation = awaken_session_execution_contract::session_run_activity_operation_id(
                &session,
                &self.claim.run_id,
            );
            let accepted = awaken_session_application::SessionActivities::activity_operation_record(
                session.as_str(),
                &operation,
            );
            let rejected = awaken_session_application::SessionActivities::budget_rejection_record(
                session.as_str(),
                &operation,
            );
            for (record, accepted) in [(accepted, true), (rejected, false)] {
                let receipt = match self.selected.roots {
                    ClaimConnectionUse::Borrowed => awaken_session_store::PostgresManagedSessionRepository::idempotency_receipt_on(
                        &mut transaction, &session, &record.key,
                    ).await,
                    ClaimConnectionUse::Independent => self.selected.application.session_repository_handle().idempotency_receipt(&session, &record.key).await,
                }.map_err(mutation)?;
                if let Some(receipt) = receipt {
                    if receipt.payload_hash != record.payload_hash {
                        return Err(SessionMutationError::IdempotencyMismatch);
                    }
                    if accepted {
                        transaction
                            .rollback()
                            .await
                            .map_err(|error| SessionMutationError::admission(unavailable(error)))?;
                        // The admission owner re-reads the SAME receipt once;
                        // this is not a stale-claim error or a new retry loop.
                        let result = Err(SessionMutationError::Conflict);
                        #[cfg(test)]
                        if let Some(cut) = &self.selected.cut {
                            cut.observe_claim_result(&result);
                        }
                        return result;
                    }
                }
            }
        }
        transaction
            .commit()
            .await
            .map_err(|error| SessionMutationError::admission(unavailable(error)))
    }

    async fn validate(
        &self,
        session: &SessionId,
        revision: SessionRevision,
    ) -> Result<bool, SessionMutationError> {
        #[cfg(test)]
        if matches!(self.purpose, ClaimPurpose::Resume)
            && let Some(cut) = &self.selected.cut
        {
            cut.pause(super::test_cut::FinalAdmissionCutKind::PreparedResumeReadValidation)
                .await;
        }
        let mut transaction = self
            .selected
            .transaction(self.purpose)
            .await
            .map_err(SessionMutationError::admission)?;
        self.check(&mut transaction)
            .await
            .map_err(SessionMutationError::admission)?;
        let current = self
            .selected
            .root(&mut transaction, session)
            .await
            .map_err(mutation)?;
        let matches = current.revision == revision;
        if matches {
            transaction
                .commit()
                .await
                .map_err(|error| SessionMutationError::admission(unavailable(error)))?;
        } else {
            transaction
                .rollback()
                .await
                .map_err(|error| SessionMutationError::admission(unavailable(error)))?;
        }
        Ok(matches)
    }

    async fn commit(
        &self,
        owner: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionMutationError> {
        if mutation.payload.session_id().as_str() != self.original.session_thread_id().0 {
            return Err(SessionMutationError::admission(RunError::bad_request(
                "Session mutation differs from its claimed physical Session",
            )));
        }
        #[cfg(test)]
        if matches!(self.purpose, ClaimPurpose::Resume)
            && let Some(cut) = &self.selected.cut
        {
            cut.pause(super::test_cut::FinalAdmissionCutKind::PreparedResumeRootCommit)
                .await;
        }
        let mut transaction = self
            .selected
            .transaction(self.purpose)
            .await
            .map_err(SessionMutationError::admission)?;
        self.check(&mut transaction)
            .await
            .map_err(SessionMutationError::admission)?;
        let staged = match self.selected.roots {
            ClaimConnectionUse::Borrowed => {
                awaken_session_store::PostgresManagedSessionRepository::commit_on(
                    &mut transaction,
                    owner,
                    mutation,
                )
                .await
            }
            ClaimConnectionUse::Independent => {
                self.selected
                    .application
                    .session_repository_handle()
                    .commit(owner, mutation)
                    .await
            }
        };
        match staged {
            Ok(
                result @ (SessionMutationResult::Applied { .. }
                | SessionMutationResult::AcknowledgedNoOp { .. }),
            ) => {
                // Only this owner has invoked final COMMIT; transport failure
                // here is unknown, never a fabricated NotApplied certificate.
                transaction
                    .commit()
                    .await
                    .map_err(|error| SessionMutationError::admission(unavailable(error)))?;
                Ok(result)
            }
            Ok(result) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|error| SessionMutationError::admission(unavailable(error)))?;
                Ok(result)
            }
            Err(error) => {
                transaction
                    .rollback()
                    .await
                    .map_err(|rollback| SessionMutationError::admission(unavailable(rollback)))?;
                Err(error.into())
            }
        }
    }
}
