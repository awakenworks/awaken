//! Canonical durable Session realization phase commands.

use std::collections::BTreeSet;

use awaken_session_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, McpAttachmentState, McpGenerationRef, PersistedSession,
    SessionRealizationAction, SessionRealizationControl, SessionRealizationControlFailure,
    SessionRealizationDirective, SessionRealizationLease, StageMcpAttachment,
};

use super::{SessionApplication, SessionMutationError};
use crate::projection;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn unavailable(error: impl std::fmt::Display) -> SessionRealizationControlFailure {
    SessionRealizationControlFailure::Unavailable(error.to_string())
}

fn validate_target(
    command: &BeginSessionRealization,
) -> Result<(), SessionRealizationControlFailure> {
    if command.session_id.trim().is_empty()
        || command.target.owner.trim().is_empty()
        || command.target.runtime_incarnation.trim().is_empty()
        || !awaken_session_contract::realization_lease_is_live_at(
            command.target.lease_expires_at_unix_ms,
            now_unix_ms(),
        )
    {
        return Err(SessionRealizationControlFailure::Invalid(
            "Session id, Runtime owner/incarnation, and a future lease expiry are required".into(),
        ));
    }
    Ok(())
}

fn verify_lease(
    session: &PersistedSession,
    asserted: &SessionRealizationLease,
) -> Result<(), SessionRealizationControlFailure> {
    if session.realization.as_ref() != Some(asserted)
        || !awaken_session_contract::realization_lease_is_live_at(
            asserted.expires_at_unix_ms,
            now_unix_ms(),
        )
    {
        return Err(SessionRealizationControlFailure::StaleOwnership);
    }
    Ok(())
}

fn exact_generation_key(generation: &McpGenerationRef) -> String {
    awaken_session_contract::stable_fingerprint(generation)
}

impl SessionApplication {
    fn realization_stage_requests(
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<Vec<StageMcpAttachment>, SessionRealizationControlFailure> {
        session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| {
                attachment.state == McpAttachmentState::Realizing
                    || (attachment.state == McpAttachmentState::Active
                        && !attachment.publication_acknowledged)
            })
            .map(|attachment| {
                projection::stage_mcp_request(owner_scope, &session.session_id, attachment)
                    .map_err(unavailable)
            })
            .collect()
    }

    fn publication_generations(
        session: &PersistedSession,
    ) -> Result<Vec<McpGenerationRef>, SessionRealizationControlFailure> {
        session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| {
                attachment.state == McpAttachmentState::Active
                    && !attachment.publication_acknowledged
            })
            .map(|attachment| {
                projection::mcp_generation_ref(&session.session_id, attachment).map_err(unavailable)
            })
            .collect()
    }

    fn draining_generations(
        session: &PersistedSession,
    ) -> Result<Vec<McpGenerationRef>, SessionRealizationControlFailure> {
        session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| attachment.state == McpAttachmentState::Draining)
            .map(|attachment| {
                projection::mcp_generation_ref(&session.session_id, attachment).map_err(unavailable)
            })
            .collect()
    }

    fn realization_directive(
        owner_scope: String,
        session: &PersistedSession,
        action: SessionRealizationAction,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        Ok(SessionRealizationDirective {
            projection: SessionApplication::frozen_session_projection(owner_scope, session)
                .map_err(|error| unavailable(error.to_string()))?,
            lease: session
                .realization
                .clone()
                .ok_or(SessionRealizationControlFailure::NotReady)?,
            action,
        })
    }

    fn next_action(
        owner_scope: String,
        session: &PersistedSession,
        prepare_projection: bool,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let stages = Self::realization_stage_requests(&owner_scope, session)?;
        // A new runtime incarnation has no process-local baseline even when the
        // Session has zero Resources/MCP. Synchronize the complete frozen
        // projection on assignment; pending Resources independently require the
        // same idempotent synchronization before their external effects.
        let prepare_session = prepare_projection || session.resources.pending.is_some();
        if prepare_session || !stages.is_empty() {
            return Self::realization_directive(
                owner_scope,
                session,
                SessionRealizationAction::Stage {
                    prepare_session,
                    mcp_stages: stages,
                },
            );
        }
        let publish = Self::publication_generations(session)?;
        let drain = Self::draining_generations(session)?;
        let action = if publish.is_empty() && drain.is_empty() {
            SessionRealizationAction::Complete
        } else {
            SessionRealizationAction::Publish { publish, drain }
        };
        Self::realization_directive(owner_scope, session, action)
    }

    async fn session_for_realization(
        &self,
        session_id: &str,
    ) -> Result<(String, PersistedSession), SessionRealizationControlFailure> {
        let owner_scope = self
            .owner(session_id)
            .await
            .ok_or(SessionRealizationControlFailure::NotFound)?;
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .ok_or(SessionRealizationControlFailure::NotFound)?;
        if session.frozen_baseline().is_none() {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        let session = self
            .ensure_repository_credentials_pinned(&owner_scope, session)
            .await
            .map_err(unavailable)?;
        Ok((owner_scope, session))
    }
}

#[async_trait::async_trait]
impl SessionRealizationControl for SessionApplication {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        validate_target(&command)?;
        for attempt in 0..SessionApplication::ROOT_CAS_ATTEMPTS {
            let (owner_scope, mut session) =
                self.session_for_realization(&command.session_id).await?;
            let now = now_unix_ms();
            let existing_live = session.realization.as_ref().is_some_and(|lease| {
                awaken_session_contract::realization_lease_is_live_at(lease.expires_at_unix_ms, now)
            });
            let same_owner = session
                .realization
                .as_ref()
                .is_some_and(|lease| lease.owner == command.target.owner);
            let same_incarnation = session.realization.as_ref().is_some_and(|lease| {
                lease.runtime_incarnation == command.target.runtime_incarnation
            });
            if existing_live && !same_owner {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }

            let requested = session
                .mcp
                .attachments
                .iter()
                .filter(|attachment| attachment.state == McpAttachmentState::Requested)
                .map(|attachment| (attachment.attachment_id.clone(), attachment.generation))
                .collect::<Vec<_>>();
            // One authenticated logical owner may immediately fence its prior
            // process incarnation after restart. A different owner still waits
            // for expiry or an explicit Control reassignment.
            let needs_assignment = !existing_live || !same_incarnation;
            let renews_assignment = command.target.renew_existing_lease
                && !needs_assignment
                && session.realization.as_ref().is_some_and(|lease| {
                    command.target.lease_expires_at_unix_ms > lease.expires_at_unix_ms
                });
            if !needs_assignment && !renews_assignment && requested.is_empty() {
                return Self::next_action(owner_scope, &session, false);
            }

            let lease = if needs_assignment {
                let epoch = match session.realization.as_ref() {
                    Some(lease) => lease.epoch.checked_add(1).ok_or_else(|| {
                        SessionRealizationControlFailure::Invalid(
                            "Session realization lease epoch is exhausted".into(),
                        )
                    })?,
                    None => 1,
                };
                SessionRealizationLease {
                    owner: command.target.owner.clone(),
                    runtime_incarnation: command.target.runtime_incarnation.clone(),
                    epoch,
                    expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                }
            } else {
                let mut lease = session
                    .realization
                    .clone()
                    .expect("a live assignment was checked");
                if renews_assignment {
                    lease.expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
                }
                lease
            };
            if session.resources.pending.is_some() {
                session.resources.start_attempt().map_err(unavailable)?;
            }
            let to_claim = if needs_assignment {
                session
                    .mcp
                    .attachments
                    .iter()
                    .filter(|attachment| {
                        matches!(
                            attachment.state,
                            McpAttachmentState::Requested
                                | McpAttachmentState::Realizing
                                | McpAttachmentState::Active
                        )
                    })
                    .map(|attachment| (attachment.attachment_id.clone(), attachment.generation))
                    .collect::<Vec<_>>()
            } else {
                requested
            };
            for (attachment_id, generation) in to_claim {
                let realization_id = awaken_session_contract::stable_fingerprint(&(
                    &session.session_id,
                    &attachment_id,
                    generation,
                    &lease.runtime_incarnation,
                    lease.epoch,
                ));
                let claim = awaken_session_contract::McpRealizationClaim {
                    realization_id,
                    runtime_incarnation: lease.runtime_incarnation.clone(),
                    lease_epoch: lease.epoch,
                    lease_expires_at_unix_ms: lease.expires_at_unix_ms,
                    stage_idempotency_key: format!(
                        "stage:{}:{}:{}:{}",
                        session.session_id, attachment_id.0, generation.0, lease.epoch
                    ),
                };
                let result = if needs_assignment {
                    session
                        .mcp
                        .claim_recovery(&attachment_id, generation, claim)
                } else {
                    session
                        .mcp
                        .claim_realization(&attachment_id, generation, claim)
                };
                result.map_err(unavailable)?;
            }
            if renews_assignment {
                session
                    .mcp
                    .renew_active_realizations(
                        &lease.runtime_incarnation,
                        lease.epoch,
                        lease.expires_at_unix_ms,
                    )
                    .map_err(unavailable)?;
            }
            session.realization = Some(lease);
            match self
                .commit_session_snapshot(
                    &owner_scope,
                    session,
                    "begin-session-realization",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => {
                    return Self::next_action(owner_scope, &session, needs_assignment);
                }
                Err(SessionMutationError::Conflict)
                    if attempt + 1 < SessionApplication::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(unavailable(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease)?;
        let expected = Self::realization_stage_requests(&owner_scope, &session)?;
        if expected.len() != command.mcp_receipts.len() {
            return Err(SessionRealizationControlFailure::Invalid(
                "MCP realization receipt set is incomplete or contains extras".into(),
            ));
        }
        let mut receipt_keys = BTreeSet::new();
        for receipt in &command.mcp_receipts {
            if !receipt_keys.insert(exact_generation_key(&receipt.generation)) {
                return Err(SessionRealizationControlFailure::Invalid(
                    "MCP realization receipt set contains a duplicate generation".into(),
                ));
            }
            let request = expected
                .iter()
                .find(|request| request.generation == receipt.generation)
                .ok_or_else(|| {
                    SessionRealizationControlFailure::Invalid(
                        "MCP realization receipt names an unclaimed generation".into(),
                    )
                })?;
            receipt
                .verify(request)
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        }
        let needs_activation_commit = session.resources.pending.is_some()
            || session
                .mcp
                .attachments
                .iter()
                .any(|attachment| attachment.state == McpAttachmentState::Realizing);
        if !needs_activation_commit {
            let publish = Self::publication_generations(&session)?;
            let drain = Self::draining_generations(&session)?;
            return Self::realization_directive(
                owner_scope,
                &session,
                SessionRealizationAction::Publish { publish, drain },
            );
        }
        if session.resources.pending.is_some() {
            session.resources.commit().map_err(unavailable)?;
        }
        for request in expected {
            let attachment = session
                .mcp
                .attachments
                .iter()
                .find(|attachment| {
                    attachment.attachment_id == request.generation.attachment_id
                        && attachment.generation == request.generation.generation
                })
                .ok_or(SessionRealizationControlFailure::NotReady)?;
            if attachment.state == McpAttachmentState::Realizing {
                session
                    .mcp
                    .activate(
                        &request.generation.attachment_id,
                        request.generation.generation,
                        &request.realization_id,
                    )
                    .map_err(unavailable)?;
            }
        }
        session.mcp.begin_obsolete_drains().map_err(unavailable)?;
        // Initial creation remains non-visible until publication acknowledgement.
        // A hot mutation belongs to an already-idle Session, so keep that lifecycle
        // status while its new generation is unacknowledged; a failed replacement
        // must not turn the established Session into a failed create.
        if session.status != "idle" {
            session.status = "activating".into();
        }
        let session = self
            .commit_session_snapshot(
                &owner_scope,
                session,
                "activate-session-realization",
                Vec::new(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?;
        let publish = Self::publication_generations(&session)?;
        let drain = Self::draining_generations(&session)?;
        Self::realization_directive(
            owner_scope,
            &session,
            SessionRealizationAction::Publish { publish, drain },
        )
    }

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease)?;
        let expected_publish = Self::publication_generations(&session)?;
        let expected_drain = Self::draining_generations(&session)?;
        let keys = |items: &[McpGenerationRef]| {
            items
                .iter()
                .map(exact_generation_key)
                .collect::<BTreeSet<_>>()
        };
        if keys(&command.published).len() != command.published.len()
            || keys(&command.drained).len() != command.drained.len()
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "publication acknowledgement contains a duplicate generation".into(),
            ));
        }
        let replayed_publish = command.published.iter().all(|generation| {
            session.mcp.attachments.iter().any(|attachment| {
                attachment.state == McpAttachmentState::Active
                    && attachment.publication_acknowledged
                    && projection::mcp_generation_ref(&session.session_id, attachment)
                        .is_ok_and(|current| current == *generation)
            })
        });
        let replayed_drain = command.drained.iter().all(|generation| {
            session.mcp.attachments.iter().any(|attachment| {
                attachment.state == McpAttachmentState::Removed
                    && projection::mcp_generation_ref(&session.session_id, attachment)
                        .is_ok_and(|current| current == *generation)
            })
        });
        if expected_publish.is_empty()
            && expected_drain.is_empty()
            && replayed_publish
            && replayed_drain
            && session.status == "idle"
        {
            return Self::realization_directive(
                owner_scope,
                &session,
                SessionRealizationAction::Complete,
            );
        }
        if keys(&expected_publish) != keys(&command.published)
            || keys(&expected_drain) != keys(&command.drained)
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "publication acknowledgement does not match the durable generation set".into(),
            ));
        }
        for generation in expected_publish {
            let realization_id = session
                .mcp
                .attachments
                .iter()
                .find(|attachment| {
                    attachment.attachment_id == generation.attachment_id
                        && attachment.generation == generation.generation
                })
                .and_then(|attachment| attachment.realization.as_ref())
                .map(|claim| claim.realization_id.clone())
                .ok_or(SessionRealizationControlFailure::NotReady)?;
            session
                .mcp
                .acknowledge_publication(
                    &generation.attachment_id,
                    generation.generation,
                    &realization_id,
                )
                .map_err(unavailable)?;
        }
        for generation in expected_drain {
            session
                .mcp
                .finish_drain(&generation.attachment_id, generation.generation)
                .map_err(unavailable)?;
        }
        session.status = "idle".into();
        let session = self
            .commit_session_snapshot(
                &owner_scope,
                session,
                "acknowledge-session-realization",
                Vec::new(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?;
        Self::realization_directive(owner_scope, &session, SessionRealizationAction::Complete)
    }

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        if command.reason.trim().is_empty() {
            return Err(SessionRealizationControlFailure::Invalid(
                "realization failure reason is empty".into(),
            ));
        }
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease)?;
        if session.status == "activation_failed" {
            return Ok(());
        }
        let realizing = session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| attachment.state == McpAttachmentState::Realizing)
            .map(|attachment| {
                (
                    attachment.attachment_id.clone(),
                    attachment.generation,
                    attachment
                        .realization
                        .as_ref()
                        .map(|claim| claim.realization_id.clone()),
                )
            })
            .collect::<Vec<_>>();
        for (attachment_id, generation, realization_id) in realizing {
            session
                .mcp
                .fail_realization(
                    &attachment_id,
                    generation,
                    realization_id
                        .as_deref()
                        .ok_or(SessionRealizationControlFailure::NotReady)?,
                    command.reason.clone(),
                )
                .map_err(unavailable)?;
        }
        if session.resources.pending.is_some() {
            session
                .resources
                .note_retryable_failure(command.reason.clone())
                .map_err(unavailable)?;
        }
        if session.status != "idle" {
            session.status = "activation_failed".into();
        }
        self.commit_session_snapshot(
            &owner_scope,
            session,
            "fail-session-realization",
            Vec::new(),
        )
        .await
        .map_err(|error| match error {
            SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
            error => unavailable(error),
        })?;
        Ok(())
    }
}
