// @formal entrypoint session.cleanup_completion_requires_exact_identity_and_canonical_artifact_evidence
// @formal entrypoint session.cleanup_operation_advances_exactly_and_completed_is_absorbing
// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
//! Terminal cleanup receipts through the Session root: the Worker-facing
//! projection of one terminal work item, the authorization of its effects and
//! the root CAS that records each preparation, disposal and Repository
//! publication outcome, plus the delete tombstone that ends a deleted Session.

use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_contract::{
    PersistedSession, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionTombstone,
};
use awaken_session_realization_contract::SessionRealizationControlFailure;
use awaken_session_types::SessionRevision;
use awaken_time::WallClockExt as _;

use super::SessionTerminalCleanups;
use super::local_drive::{
    bind_terminal_restore_target, terminal_cleanup_authorization_failure,
    terminal_restore_target_for_thread,
};
use crate::mutation::ROOT_CAS_ATTEMPTS;
use crate::realization::{mutation_control, repository_control};
use crate::{SessionMutationError, SessionPreparationError};

fn terminal_cleanup_lease_matches(
    session: &PersistedSession,
    asserted: &SessionRealizationLease,
) -> bool {
    session.realization.as_ref().is_some_and(|current| {
        awaken_session_realization_contract::realization_lease_generation_authorizes(
            current, asserted,
        )
    })
}

impl SessionTerminalCleanups {
    /// Read one terminal-work projection from one Session-root snapshot.
    ///
    /// Cause/effect table: C1 the asserted generation matches the root's current
    /// lease; C2 the root has no terminal fence, is fenced/requested, or is
    /// completed; C3 the frozen Environment/Resource projection succeeds or
    /// fails. Effects: R1 C1+no fence/completed/legacy-complete => `None`; R2
    /// C1+fenced or publication barrier => `Waiting`; R3 C1+requested+C3
    /// succeeds => one `Prepare` or `Dispose` action from this same snapshot;
    /// R4 stale ownership or projection failure => typed failure before a
    /// Worker effect. The work value owns no queue or receipt.
    pub(crate) async fn terminal_cleanup_work_for_lease(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionTerminalCleanupWork>,
        SessionRealizationControlFailure,
    > {
        let mutations = &self.mutations;
        let session = mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_control)?;
        if !terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        session
            .verified_terminal_cleanup()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        let action = session
            .terminal_cleanup_work_action()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        if action.is_none() {
            return Ok(None);
        }
        // Refresh only after the cheap root preflight identifies terminal work,
        // then re-read the root. The second snapshot, not the preflight value,
        // supplies every assignment and command fact below.
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(SessionRealizationControlFailure::Unavailable)?;
        let session = admission
            .enter(|| mutations.session_aggregate(session_id))
            .await
            .map_err(repository_control)?;
        if !terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        session
            .verified_terminal_cleanup()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        let Some(action) = session
            .terminal_cleanup_work_action()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?
        else {
            return Ok(None);
        };
        let owner_scope = self
            .mutations
            .repository()
            .owner(session_id)
            .await
            .map_err(repository_control)?;
        let action = bind_terminal_restore_target(&owner_scope, &session, action)?;
        let assignment = self
            .terminal_cleanup_assignment_from_snapshot(&owner_scope, &session)
            .await?;
        Ok(Some(
            awaken_session_realization_contract::SessionTerminalCleanupWork { assignment, action },
        ))
    }

    pub(crate) async fn authorize_terminal_cleanup_disposal_from_root(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure> {
        let session = self
            .mutations
            .repository()
            .get(&effect.command.session_id)
            .await
            .map_err(repository_control)?;
        crate::require_live_root_lease(&session, self.clock.now_ms())?;
        let owner_scope = self
            .mutations
            .repository()
            .owner(&effect.command.session_id)
            .await
            .map_err(repository_control)?;
        if effect.command.restore_target
            != terminal_restore_target_for_thread(
                &owner_scope,
                &session,
                effect.command.session_id.as_str(),
            )
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal disposal restore target differs from durable Environment authority"
                    .into(),
            ));
        }
        session
            .authorize_terminal_cleanup_disposal_effect(&owner_scope, effect)
            .map_err(terminal_cleanup_authorization_failure)?;
        Ok(owner_scope)
    }

    /// Project the root-only Repository publication effect under the exact
    /// realization lease that already owns terminal cleanup. The command and
    /// immutable Workspace owner are read from the Session authority on every
    /// poll; no application queue or receipt registry exists beside
    /// `SessionCleanupOperation`.
    pub(crate) async fn terminal_repository_publication_command_for_lease(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_realization_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        let mutations = &self.mutations;
        let session =
            mutations
                .session_aggregate(session_id)
                .await
                .map_err(|error| match error {
                    awaken_session_contract::SessionRepositoryError::NotFound => {
                        SessionRealizationControlFailure::NotFound
                    }
                    error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                })?;
        // The asserted publication generation may predate a same-generation
        // renewal. Starting a new credential/source effect still requires the
        // current root lease to be live; its exact readback travels with the
        // immutable command so transport adapters never reconstruct it.
        crate::require_live_root_lease(&session, self.clock.now_ms())?;
        if !terminal_cleanup_lease_matches(&session, lease) {
            return Err(SessionRealizationControlFailure::StaleOwnership);
        }
        let terminal_cleanup = session
            .verified_terminal_cleanup()
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        if terminal_cleanup.is_completed() || !terminal_cleanup.is_requested() {
            return Ok(None);
        }
        let command = terminal_cleanup
            .publication_command(session_id)
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        let Some(command) = command else {
            return Ok(None);
        };
        let workspace_id = mutations
            .owner(session_id)
            .await
            .map_err(|error| match error {
                SessionMutationError::NotFound => SessionRealizationControlFailure::NotFound,
                error => SessionRealizationControlFailure::Unavailable(error.to_string()),
            })?;
        let current_lease = session.realization.clone().ok_or_else(|| {
            SessionRealizationControlFailure::Invalid(
                "terminal Repository publication has no current realization".into(),
            )
        })?;
        awaken_session_realization_contract::SessionRepositoryPublicationProjection::try_new(
            workspace_id,
            command,
            current_lease,
        )
        .map(Some)
    }

    /// Verify and persist one exact Repository publication receipt in the same
    /// root CAS for local composition and remote Workers. Physical disposal
    /// remains a later action, so response loss can replay this receipt without
    /// losing the working tree that produced it. The current root lease, not
    /// topology, is the sole admission authority.
    pub(crate) async fn record_terminal_repository_publication_receipt_from_root(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_effect_from_root(
            session_id,
            lease,
            awaken_session_contract::SessionRepositoryPublicationEffect::Published(receipt),
        )
        .await
    }

    pub(crate) async fn record_terminal_repository_publication_rejection_from_root(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_effect_from_root(
            session_id,
            lease,
            awaken_session_contract::SessionRepositoryPublicationEffect::Rejected(rejection),
        )
        .await
    }

    async fn record_terminal_repository_publication_effect_from_root(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
        effect: awaken_session_contract::SessionRepositoryPublicationEffect,
    ) -> Result<(), SessionRealizationControlFailure> {
        let mutations = &self.mutations;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations.owner(session_id).await.map_err(|error| {
                SessionRealizationControlFailure::Unavailable(error.to_string())
            })?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(|error| match error {
                    awaken_session_contract::SessionRepositoryError::NotFound => {
                        SessionRealizationControlFailure::NotFound
                    }
                    error => SessionRealizationControlFailure::Unavailable(error.to_string()),
                })?;
            if !session.is_terminal() {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session is not terminal".into(),
                ));
            }
            if !terminal_cleanup_lease_matches(&session, lease) {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            let changed = match effect.clone() {
                awaken_session_contract::SessionRepositoryPublicationEffect::Published(receipt) => {
                    session
                        .cleanup()
                        .record_repository_publication_receipt(receipt)
                }
                awaken_session_contract::SessionRepositoryPublicationEffect::Rejected(
                    rejection,
                ) => session
                    .lifecycle()
                    .record_repository_publication_rejection(rejection),
            }
            .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
            if changed {
                match self
                    .resource_references
                    .commit(
                        &owner_scope,
                        session,
                        "terminal-repository-publication-worker-outcome",
                        Vec::new(),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                        continue;
                    }
                    Err(SessionMutationError::Conflict) => {
                        return Err(SessionRealizationControlFailure::Conflict);
                    }
                    Err(error) => {
                        return Err(SessionRealizationControlFailure::Unavailable(
                            error.to_string(),
                        ));
                    }
                }
            }
            self.lifecycle.request_reconciliation();
            return Ok(());
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    /// Persist one source-dependent preparation through the Session root. The
    /// last root preparation may enter `Disposing` only after every
    /// Session-owned Repository participant has been retired while the
    /// aggregate still retains its canonical Resource projection.
    pub(crate) async fn record_terminal_cleanup_preparation_from_root(
        &self,
        lease: &SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        let mutations = &self.mutations;
        let session_id = preparation.effect.command.session_id.clone();
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations
                .owner(&session_id)
                .await
                .map_err(mutation_control)?;
            let mut session = mutations
                .session_aggregate(&session_id)
                .await
                .map_err(repository_control)?;
            if !session.is_terminal() {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session is not terminal".into(),
                ));
            }
            let repository_preparation = if let Some(durable) =
                session.terminal_cleanup.repository_preparation().cloned()
            {
                // A delayed child or root retry may arrive after the last
                // preparation CAS already entered Disposing. Every exact replay
                // reuses the same aggregate-owned Repository proof.
                Some(durable)
            } else if preparation.effect.command.thread_id == session_id {
                // Root preparation is projected only after every child and
                // optional publication barrier. Run the canonical
                // Repository/Vault participant owner before admitting this
                // last preparation; its receipt and the Runtime receipt
                // enter Disposing in the same root CAS.
                let _inherited_provider_disposal = session
                    .authorize_terminal_cleanup_effect(&preparation.effect)
                    .map_err(terminal_cleanup_authorization_failure)?;
                let prepared = self
                    .repository_participants
                    .prepare_terminal(&owner_scope, session)
                    .await;
                let (next_session, receipt) = match prepared {
                    Ok(prepared) => prepared,
                    Err(SessionPreparationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                        continue;
                    }
                    Err(SessionPreparationError::Conflict) => {
                        return Err(SessionRealizationControlFailure::Conflict);
                    }
                    Err(SessionPreparationError::NotFound) => {
                        return Err(SessionRealizationControlFailure::NotFound);
                    }
                    Err(error) => {
                        return Err(SessionRealizationControlFailure::Unavailable(
                            error.to_string(),
                        ));
                    }
                };
                session = next_session;
                Some(receipt)
            } else {
                None
            };
            let changed = session
                .cleanup()
                .record_terminal_cleanup_preparation(
                    &owner_scope,
                    lease,
                    preparation.clone(),
                    repository_preparation,
                )
                .map_err(terminal_cleanup_authorization_failure)?;
            if !changed {
                self.lifecycle.request_reconciliation();
                return Ok(());
            }
            match self
                .resource_references
                .commit(
                    &owner_scope,
                    session,
                    "terminal-cleanup-preparation",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => {
                    self.lifecycle.request_reconciliation();
                    return Ok(());
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(mutation_control(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    /// Persist the one physical-disposal receipt and atomically retire the
    /// aggregate's Environment/Resource projection. Exact response-loss replay
    /// is absorbed by the existing Completed fingerprint.
    pub(crate) async fn record_terminal_cleanup_disposal_from_root(
        &self,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        let mutations = &self.mutations;
        let session_id = receipt.session_id.clone();
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations
                .owner(&session_id)
                .await
                .map_err(mutation_control)?;
            let mut session = mutations
                .session_aggregate(&session_id)
                .await
                .map_err(repository_control)?;
            let changed = session
                .cleanup()
                .record_terminal_cleanup_disposal(
                    &owner_scope,
                    lease,
                    receipt.clone(),
                    "Session terminated before activation completed",
                )
                .map_err(terminal_cleanup_authorization_failure)?;
            if !changed {
                self.lifecycle.request_reconciliation();
                return Ok(());
            }
            match self
                .resource_references
                .commit(
                    &owner_scope,
                    session,
                    "terminal-cleanup-disposal",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => {
                    self.lifecycle.request_reconciliation();
                    return Ok(());
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(mutation_control(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    /// Commit the terminal delete tombstone through the canonical Session CAS.
    /// The `session.deleted` lifecycle fact belongs to the earlier atomic Delete
    /// fence; emitting it again here could duplicate delivery after an outbox
    /// consumer acknowledged the first fact.
    pub(super) async fn commit_delete_tombstone(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionMutationError> {
        let deleted_revision =
            SessionRevision(session.revision.0.checked_add(1).ok_or_else(|| {
                SessionMutationError::Internal("Session revision exhausted".into())
            })?);
        let payload = SessionMutationPayload::Delete(SessionTombstone {
            session_id: session.session_id.clone(),
            deleted_revision,
            // The tombstone's deletion instant, in Unix seconds, on the
            // application clock.
            deleted_at: self
                .clock
                .now()
                .duration_since_epoch()
                .as_secs()
                .to_string(),
        });
        let payload_hash = payload.stable_hash();
        let mutation = SessionMutation {
            expected_revision: session.revision,
            idempotency: awaken_session_contract::IdempotencyRecord {
                key: format!(
                    "session:delete:{}:{}:{payload_hash}",
                    session.session_id, session.revision.0
                ),
                payload_hash,
            },
            payload,
            lifecycle_facts: Vec::new(),
        };
        match self
            .mutations
            .commit_mutation(owner_scope, mutation)
            .await?
        {
            SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. } => {
                Ok(())
            }
            SessionMutationResult::Conflict { .. } => Err(SessionMutationError::Conflict),
            SessionMutationResult::IdempotencyMismatch => {
                Err(SessionMutationError::IdempotencyMismatch)
            }
        }
    }
}
