// @formal entrypoint session_resources.failed_replacement_preserves_active_manifest
//! The Session Resource desired-manifest command: its idempotent replay, its
//! root-CAS replacement, and the convergence that follows an accepted
//! generation.
//!
//! Recovery and terminal reclamation own the durable sweep; this module owns
//! only the caller-issued manifest command and the exact receipt it returns.

use awaken_agent_contract::stable_fingerprint;
use awaken_lease_contract::WallClockExt as _;
use awaken_session_error_contract::RunError;
use awaken_session_state_contract::{IdempotencyRecord, SessionExecutionState};

use super::{
    ReplaceSessionResourceManifest, RepositoryRetirementCause, SessionResourceManifestError,
    SessionResourceManifestOutcome,
};
use crate::mutation::repository_failure;
use crate::{SessionApplication, SessionPreparationError};

impl SessionApplication {
    #[must_use]
    pub fn resource_manifest_operation_id(session_id: &str, idempotency_key: &str) -> String {
        stable_fingerprint(&(session_id, idempotency_key))
    }

    fn resource_manifest_idempotency_record(
        session_id: &str,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> IdempotencyRecord {
        IdempotencyRecord {
            key: format!(
                "managed:resource-manifest-command:{session_id}:{}",
                Self::resource_manifest_operation_id(session_id, idempotency_key)
            ),
            payload_hash: request_fingerprint.to_string(),
        }
    }

    /// Resolve an exact command replay before an interface performs lowering
    /// side effects such as Repository/Vault configuration. The main command
    /// repeats this check to close the race with a concurrent first writer.
    pub async fn replay_session_resource_manifest(
        &self,
        session_id: &awaken_session_types::SessionId,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<SessionResourceManifestOutcome>, SessionResourceManifestError> {
        let record = Self::resource_manifest_idempotency_record(
            session_id.as_str(),
            idempotency_key,
            request_fingerprint,
        );
        let Some(receipt) = self
            .session_repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?
        else {
            return Ok(None);
        };
        if receipt.payload_hash != record.payload_hash {
            return Err(SessionResourceManifestError::IdempotencyMismatch);
        }
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        self.converge_resource_manifest(SessionResourceManifestOutcome {
            session,
            command_revision: receipt.committed_revision,
            command_applied: false,
        })
        .await
        .map(Some)
    }

    /// Persist one complete desired Resource manifest through the Session root
    /// CAS, then ask the existing local reconciler or WorkQueue projection to
    /// realize that exact generation. No external effect runs before this
    /// method has a durable, queryable desired manifest.
    pub async fn replace_session_resource_manifest(
        &self,
        session_id: &awaken_session_types::SessionId,
        command: ReplaceSessionResourceManifest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let command_record = command.idempotency_key.as_ref().map(|key| {
            Self::resource_manifest_idempotency_record(
                session_id.as_str(),
                key,
                &command.request_fingerprint,
            )
        });

        if let Some(key) = &command.idempotency_key
            && let Some(outcome) = self
                .replay_session_resource_manifest(session_id, key, &command.request_fingerprint)
                .await?
        {
            return Ok(outcome);
        }

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            match self
                .replace_session_resource_manifest_once(
                    session_id,
                    &command,
                    command_record.clone(),
                )
                .await
            {
                Err(SessionResourceManifestError::Conflict)
                    if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Ok(outcome) => return self.converge_resource_manifest(outcome).await,
                result => return result,
            }
        }
        Err(SessionResourceManifestError::Conflict)
    }

    async fn replace_session_resource_manifest_once(
        &self,
        session_id: &awaken_session_types::SessionId,
        command: &ReplaceSessionResourceManifest,
        command_record: Option<IdempotencyRecord>,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let owner_scope = self
            .owner(session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let mut session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        if session.is_terminal() {
            return Err(SessionResourceManifestError::Terminal);
        }
        if command
            .expected_resource_revision
            .is_some_and(|expected| expected != session.resources.revision)
        {
            return Err(SessionResourceManifestError::Conflict);
        }
        let mut desired = command.resources.clone();
        desired.observe_input_times(session.resources.desired(), self.clock.now_ms());
        if let Some(baseline) = session.frozen_baseline()
            && !baseline
                .mutation_policy
                .admits_resource_replacement(session.resources.desired(), &desired)
        {
            return Err(SessionResourceManifestError::Rejected(
                RunError::bad_request(
                    "profiled Session resource mutation is outside its immutable policy",
                ),
            ));
        }

        let unchanged = session.resources.desired() == &desired;
        if !unchanged {
            let result = if session.resources.pending.is_some() {
                session
                    .resources
                    .revise_unattempted_pending(&session.session_id, desired.clone())
            } else {
                session.resources.prepare(&session.session_id, desired)
            };
            result.map_err(|error| match error {
                awaken_session_state_contract::ResourceActivationError::Pending => {
                    SessionResourceManifestError::Conflict
                }
                error => {
                    SessionResourceManifestError::Rejected(RunError::bad_request(error.to_string()))
                }
            })?;
        }

        let command_receipt_key = command_record.as_ref().map(|record| record.key.clone());
        let (session, command_applied) = match command_record {
            Some(record) => {
                let commit = self
                    .commit_resource_snapshot_with_record(&owner_scope, session, record)
                    .await
                    .map_err(SessionResourceManifestError::mutation)?;
                let applied = commit.transitioned();
                (commit.into_session(), applied)
            }
            None if unchanged => {
                let revision = session.revision;
                return Ok(SessionResourceManifestOutcome {
                    session,
                    command_revision: revision,
                    command_applied: false,
                });
            }
            None => (
                self.commit_resource_snapshot(
                    &owner_scope,
                    session,
                    "resource-manifest-intent",
                    Vec::new(),
                )
                .await
                .map_err(SessionResourceManifestError::mutation)?,
                true,
            ),
        };
        let command_revision = if command_applied {
            session.revision
        } else {
            let key = command_receipt_key.ok_or_else(|| {
                SessionResourceManifestError::Unavailable(
                    "replayed Resource manifest has no idempotency key".into(),
                )
            })?;
            self.session_repository()
                .idempotency_receipt(session_id, &key)
                .await
                .map_err(repository_failure)
                .map_err(SessionResourceManifestError::mutation)?
                .ok_or_else(|| {
                    SessionResourceManifestError::Unavailable(
                        "replayed Resource manifest has no idempotency receipt".into(),
                    )
                })?
                .committed_revision
        };
        Ok(SessionResourceManifestOutcome {
            session,
            command_revision,
            command_applied,
        })
    }

    async fn converge_resource_manifest(
        &self,
        mut outcome: SessionResourceManifestOutcome,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        if outcome.session.resources.pending.is_none()
            && !outcome.session.resources.has_repository_retirements()
        {
            return Ok(outcome);
        }
        let owner_scope = self
            .owner(&outcome.session.session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let convergence = if outcome.session.resources.pending.is_none() {
            self.reconcile_repository_retirements(
                &owner_scope,
                outcome.session.clone(),
                RepositoryRetirementCause::DetachedGeneration,
            )
            .await
        } else if self.requires_external_realization(&outcome.session) {
            self.dispatch_session_work(&outcome.session)
                .await
                .map(|_| outcome.session.clone())
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))
        } else if outcome.session.execution == SessionExecutionState::Idle {
            self.reconcile_persisted_resources(&owner_scope, outcome.session.clone())
                .await
        } else {
            Ok(outcome.session.clone())
        };
        match convergence {
            Ok(session) => {
                outcome.session = session;
                Ok(outcome)
            }
            Err(source) => Err(SessionResourceManifestError::ProjectionAfterCommit {
                outcome: Box::new(outcome),
                source,
            }),
        }
    }
}
