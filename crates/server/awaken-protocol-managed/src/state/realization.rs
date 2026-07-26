//! Control-owned Session realization phase commands.

use std::collections::BTreeSet;

use super::*;
use awaken_session_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, McpAttachmentState, McpGenerationRef, SessionRealizationAction,
    SessionRealizationControl, SessionRealizationControlFailure, SessionRealizationDirective,
    SessionRealizationLease, StageMcpAttachment,
};

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

struct LocalProjectionSynchronizer<'a> {
    runtime: &'a dyn SessionRuntime,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionProjectionSynchronizer for LocalProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_session_contract::FrozenSessionProjection,
        _lease: &SessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), RunError> {
        if !prepare_session {
            return Ok(());
        }
        let baseline = &projection.baseline;
        self.runtime
            .prepare_session(
                session_id,
                SessionInit {
                    workspace_id: projection.workspace_id.clone(),
                    agent_id: baseline.agent_id.clone(),
                    delegate_ids: baseline.delegate_ids.clone(),
                    resources: projection.resources.clone(),
                    model: Some(baseline.model.clone()),
                    runtime: baseline.runtime.clone(),
                    environment: baseline.environment.clone(),
                },
            )
            .await
    }
}

impl ManagedState {
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
                super::sessions::stage_mcp_request(owner_scope, &session.session_id, attachment)
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
                super::sessions::mcp_generation_ref(&session.session_id, attachment)
                    .map_err(unavailable)
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
                super::sessions::mcp_generation_ref(&session.session_id, attachment)
                    .map_err(unavailable)
            })
            .collect()
    }

    fn realization_directive(
        owner_scope: String,
        session: &PersistedSession,
        action: SessionRealizationAction,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        Ok(SessionRealizationDirective {
            projection: Self::frozen_session_projection(owner_scope, session)
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
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let stages = Self::realization_stage_requests(&owner_scope, session)?;
        let prepare_session = session.resources.pending.is_some();
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
            .resolve_owner(session_id)
            .await
            .ok_or(SessionRealizationControlFailure::NotFound)?;
        let session = self
            .sessions_repo
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
impl SessionRealizationControl for ManagedState {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        validate_target(&command)?;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
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
                return Self::next_action(owner_scope, &session);
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
                Ok(session) => return Self::next_action(owner_scope, &session),
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => continue,
                Err(StateError::Conflict) => {
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
                StateError::Conflict => SessionRealizationControlFailure::Conflict,
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
                    && super::sessions::mcp_generation_ref(&session.session_id, attachment)
                        .is_ok_and(|current| current == *generation)
            })
        });
        let replayed_drain = command.drained.iter().all(|generation| {
            session.mcp.attachments.iter().any(|attachment| {
                attachment.state == McpAttachmentState::Removed
                    && super::sessions::mcp_generation_ref(&session.session_id, attachment)
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
                StateError::Conflict => SessionRealizationControlFailure::Conflict,
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
            StateError::Conflict => SessionRealizationControlFailure::Conflict,
            error => unavailable(error),
        })?;
        Ok(())
    }
}

impl ManagedState {
    fn map_realization_failure(error: SessionRealizationControlFailure) -> StateError {
        match error {
            SessionRealizationControlFailure::NotFound => StateError::NotFound,
            SessionRealizationControlFailure::Conflict => StateError::Conflict,
            SessionRealizationControlFailure::Invalid(message) => {
                StateError::Run(RunError::bad_request(message))
            }
            error => StateError::Run(RunError::internal(error.to_string())),
        }
    }

    /// Sole in-process topology adapter for the Control-owned realization
    /// protocol. Create, hot replacement, and recovery all call this driver;
    /// it owns Runtime I/O but never mutates Session desired state directly.
    pub(super) async fn realize_session_locally(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, StateError> {
        let lease_expires_at_unix_ms = now_unix_ms()
            .checked_add(300_000)
            .ok_or_else(|| StateError::Run(RunError::internal("lease expiry overflow")))?;
        let directive = self
            .begin_session_realization(BeginSessionRealization {
                session_id: session_id.to_string(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: "managed-runtime".into(),
                    runtime_incarnation: self.runtime_incarnation.clone(),
                    lease_expires_at_unix_ms,
                    renew_existing_lease: false,
                },
            })
            .await
            .map_err(Self::map_realization_failure)?;
        self.drive_local_realization(session_id, directive).await?;
        self.sessions_repo
            .get(session_id)
            .await
            .ok_or(StateError::NotFound)
    }

    async fn drive_local_realization(
        &self,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
    ) -> Result<(), StateError> {
        awaken_session_contract::drive_session_realization(
            session_id,
            self,
            &LocalProjectionSynchronizer {
                runtime: self.runtime.as_ref(),
            },
            self.mcp_realizer.as_ref(),
            directive,
        )
        .await
        .map_err(|error| match error {
            awaken_session_contract::SessionRealizationDriveError::Effect(error) => {
                StateError::Run(error)
            }
            awaken_session_contract::SessionRealizationDriveError::Control(error) => {
                Self::map_realization_failure(error)
            }
            awaken_session_contract::SessionRealizationDriveError::DidNotConverge => {
                StateError::Run(RunError::internal(error.to_string()))
            }
        })
    }

    /// Renew due local projections through the same root-CAS phase protocol.
    /// Composition roots call this periodically; it owns no second registry or
    /// relay-specific timer.
    pub async fn renew_due_session_realizations(
        &self,
        now_unix_ms: u64,
    ) -> Result<usize, StateError> {
        const RENEW_BEFORE_MS: u64 = 150_000;
        const LEASE_MS: u64 = 300_000;
        let renew_before = now_unix_ms.saturating_add(RENEW_BEFORE_MS);
        let requested_expiry = now_unix_ms.saturating_add(LEASE_MS);
        let sessions = self.sessions_repo.reconcilable_sessions().await;
        let mut renewed = 0;
        for scoped in sessions {
            let Some(lease) = scoped.session.realization.clone() else {
                continue;
            };
            if scoped.session.status == "deleted"
                || lease.owner != "managed-runtime"
                || lease.runtime_incarnation != self.runtime_incarnation
                || lease.expires_at_unix_ms > renew_before
                || !scoped
                    .session
                    .mcp
                    .attachments
                    .iter()
                    .any(|attachment| attachment.state == McpAttachmentState::Active)
            {
                continue;
            }
            let directive = self
                .begin_session_realization(BeginSessionRealization {
                    session_id: scoped.session.session_id.clone(),
                    target: awaken_session_contract::SessionRealizationTarget {
                        owner: lease.owner,
                        runtime_incarnation: lease.runtime_incarnation,
                        lease_expires_at_unix_ms: requested_expiry,
                        renew_existing_lease: true,
                    },
                })
                .await
                .map_err(Self::map_realization_failure)?;
            self.drive_local_realization(&scoped.session.session_id, directive)
                .await?;
            renewed += 1;
        }
        Ok(renewed)
    }

    /// Start the local realization-lease supervisor when a Tokio runtime is
    /// available. Composition roots call this once for the canonical
    /// `ManagedState`; repeated MCP-specific timers are forbidden.
    #[must_use]
    pub fn spawn_realization_lease_supervisor(
        self: &Arc<Self>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let state = self.clone();
        Some(runtime.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            loop {
                interval.tick().await;
                let now = now_unix_ms();
                if let Err(error) = state.renew_due_session_realizations(now).await {
                    tracing::warn!(
                        error = ?error,
                        "Session realization lease renewal remains pending"
                    );
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_credential_contract::{
        CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_contract::{
        ApplicationContributionReceipt, EnvironmentFingerprint, EnvironmentSnapshot,
        IdempotencyRecord, McpAttachmentDraft, McpAttachmentOrigin, McpRealizationReceipt,
        McpTarget, OutcomeReport, SessionBaseline, SessionBaselineInputs, SessionBaselineState,
        SessionMcpAttachmentSet, SessionNetworkPolicy, SessionResourceState, SessionRevision,
        SessionRuntime, StepOutcome, ToolPermissionDecision,
    };

    struct NoopRuntime;

    #[async_trait::async_trait]
    impl SessionRuntime for NoopRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: &str,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            unreachable!()
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<OutcomeReport, RunError> {
            unreachable!()
        }

        fn model(&self) -> String {
            "test-model".into()
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for NoopRuntime {
        async fn stage_mcp_attachment(
            &self,
            request: StageMcpAttachment,
        ) -> Result<McpRealizationReceipt, RunError> {
            Ok(McpRealizationReceipt {
                generation: request.generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), RunError> {
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), RunError> {
            Ok(())
        }
    }

    fn holder() -> PlaintextHolder {
        PlaintextHolder::new(PlaintextBoundary::Worker, "test.worker")
    }

    fn persisted_session(id: &str) -> PersistedSession {
        let application_input = awaken_session_contract::ApplicationSessionInput::default();
        let baseline = SessionBaseline::compile(SessionBaselineInputs {
            environment: EnvironmentSnapshot {
                environment_id: "env".into(),
                revision: awaken_session_contract::env_registry::EnvironmentRevision(1),
                config_fingerprint: EnvironmentFingerprint("env-fingerprint".into()),
                sandbox: serde_json::json!({}),
                network: SessionNetworkPolicy::Unrestricted,
                credential_realization: CredentialRealizationProfile {
                    inference_holder: holder(),
                    mcp_holder: holder(),
                    resource_holder: holder(),
                },
            },
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            model: "model".into(),
            runtime: None,
            application: Some(ApplicationContributionReceipt::from_input(
                "application-plan".into(),
                &application_input,
            )),
            delegate_ids: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
        });
        let mut resources = SessionResourceState::default();
        resources
            .prepare(id, Default::default())
            .expect("prepare generation 1 resources");
        let mcp = SessionMcpAttachmentSet::from_initial(
            vec![McpAttachmentDraft {
                name: "docs".into(),
                target: McpTarget::parse_http("https://mcp.example.test").unwrap(),
                credential: None,
                origin: McpAttachmentOrigin::Application,
            }],
            Some(holder()),
        )
        .unwrap();
        PersistedSession {
            session_id: id.into(),
            revision: SessionRevision(0),
            baseline: SessionBaselineState::Frozen(baseline),
            title: None,
            metadata: Default::default(),
            agent_tools: None,
            environment_binding: None,
            mcp,
            resources,
            realization: None,
            status: "preparing".into(),
            archived_at: None,
        }
    }

    async fn harness_for(
        session: PersistedSession,
    ) -> (ManagedState, Arc<dyn ManagedSessionRepository>) {
        let repo: Arc<dyn ManagedSessionRepository> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let id = session.session_id.clone();
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        repo.create(
            "workspace-a",
            session,
            IdempotencyRecord {
                key: format!("test:create:{id}"),
                payload_hash,
            },
            Vec::new(),
        )
        .await
        .unwrap();
        (
            ManagedState::new_with_mcp(NoopRuntime).with_session_repo(repo.clone()),
            repo,
        )
    }

    async fn harness(id: &str) -> (ManagedState, Arc<dyn ManagedSessionRepository>) {
        harness_for(persisted_session(id)).await
    }

    fn exact_receipts(action: &SessionRealizationAction) -> Vec<McpRealizationReceipt> {
        let SessionRealizationAction::Stage { mcp_stages, .. } = action else {
            panic!("expected stage action")
        };
        mcp_stages
            .iter()
            .map(|request| McpRealizationReceipt {
                generation: request.generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: request.selected_plaintext_holder.clone(),
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            })
            .collect()
    }

    #[tokio::test]
    async fn realization_phase_cases_are_generated_from_the_decision_table() {
        // Cause graph:
        // Frozen Session + valid target -> durable lease/claims -> exact complete
        // receipts -> durable activation -> exact publish/drain acknowledgement ->
        // Complete. A stale owner, missing/mismatched receipt, or wrong ack set
        // terminates before the next root mutation. Exact retries replay.
        //
        // | Rule | Phase | lease | receipt set | ack set | Effect |
        // |---|---|---|---|---|---|
        // | Q1 | begin | new valid | - | - | Stage + durable claims |
        // | Q2 | begin replay | exact live | - | - | same Stage/no revision |
        // | Q3 | begin | other live owner | - | - | stale/no mutation |
        // | Q4 | activate | exact | missing | - | invalid/no mutation |
        // | Q5 | activate | exact | fingerprint mismatch | - | invalid/no mutation |
        // | Q6 | activate | exact | exact complete | - | Publish + durable Active |
        // | Q7 | activate replay | exact | exact complete | - | same Publish/no revision |
        // | Q8 | acknowledge | exact | - | missing | invalid/no mutation |
        // | Q9 | acknowledge | exact | - | duplicate | invalid/no mutation |
        // | Q10 | acknowledge | exact | - | exact | Complete + durable idle |
        // | Q11 | acknowledge replay | exact | - | exact | Complete/no revision |
        // | Q12 | renew | same owner/incarnation | - | later expiry | Stage same generation |
        // | Q13 | renew complete | exact | exact | exact | Complete + extended fence |
        let (state, repo) = harness("session-phase").await;
        let target = awaken_session_contract::SessionRealizationTarget {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/incarnation-1".into(),
            lease_expires_at_unix_ms: u64::MAX - 1,
            renew_existing_lease: false,
        };
        let begin = BeginSessionRealization {
            session_id: "session-phase".into(),
            target: target.clone(),
        };
        let staged = state
            .begin_session_realization(begin.clone())
            .await
            .expect("Q1");
        assert!(matches!(
            staged.action,
            SessionRealizationAction::Stage { .. }
        ));
        let after_begin = repo.get("session-phase").await.unwrap();
        assert_eq!(
            after_begin.mcp.attachments[0].state,
            McpAttachmentState::Realizing,
            "Q1"
        );
        assert!(
            after_begin.resources.activations.is_empty(),
            "Q1 empty resources"
        );

        let replay = state.begin_session_realization(begin).await.expect("Q2");
        assert_eq!(replay, staged, "Q2");
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_begin.revision,
            "Q2"
        );

        let stale = state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-phase".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: "worker-b".into(),
                    ..target
                },
            })
            .await;
        assert_eq!(
            stale.unwrap_err(),
            SessionRealizationControlFailure::StaleOwnership,
            "Q3"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_begin.revision,
            "Q3"
        );

        let missing = state
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: Vec::new(),
            })
            .await;
        assert!(
            matches!(missing, Err(SessionRealizationControlFailure::Invalid(_))),
            "Q4"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_begin.revision,
            "Q4"
        );

        let receipts = exact_receipts(&staged.action);
        let mut mismatched = receipts.clone();
        mismatched[0].receipt_fingerprint = "another-request".into();
        let mismatch = state
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: mismatched,
            })
            .await;
        assert!(
            matches!(mismatch, Err(SessionRealizationControlFailure::Invalid(_))),
            "Q5"
        );

        let activated = state
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: receipts.clone(),
            })
            .await
            .expect("Q6");
        let (publish, drain) = match &activated.action {
            SessionRealizationAction::Publish { publish, drain } => {
                (publish.clone(), drain.clone())
            }
            _ => panic!("Q6 expected publish"),
        };
        assert_eq!(publish.len(), 1, "Q6");
        assert!(drain.is_empty(), "Q6");
        let after_activate = repo.get("session-phase").await.unwrap();
        assert_eq!(
            after_activate.mcp.attachments[0].state,
            McpAttachmentState::Active,
            "Q6"
        );
        assert!(after_activate.resources.pending.is_none(), "Q6");

        let activate_replay = state
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                mcp_receipts: receipts,
            })
            .await
            .expect("Q7");
        assert_eq!(activate_replay, activated, "Q7");
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_activate.revision,
            "Q7"
        );

        let wrong_ack = state
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                published: Vec::new(),
                drained: Vec::new(),
            })
            .await;
        assert!(
            matches!(wrong_ack, Err(SessionRealizationControlFailure::Invalid(_))),
            "Q8"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_activate.revision,
            "Q8"
        );

        let duplicate_ack = state
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-phase".into(),
                lease: staged.lease.clone(),
                published: vec![publish[0].clone(), publish[0].clone()],
                drained: Vec::new(),
            })
            .await;
        assert!(
            matches!(
                duplicate_ack,
                Err(SessionRealizationControlFailure::Invalid(_))
            ),
            "Q9"
        );
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_activate.revision,
            "Q9"
        );

        let acknowledgement = AcknowledgeSessionRealization {
            session_id: "session-phase".into(),
            lease: staged.lease,
            published: publish,
            drained: drain,
        };
        let complete = state
            .acknowledge_session_realization(acknowledgement.clone())
            .await
            .expect("Q10");
        assert_eq!(complete.action, SessionRealizationAction::Complete, "Q10");
        let after_ack = repo.get("session-phase").await.unwrap();
        assert_eq!(after_ack.status, "idle", "Q10");
        assert!(after_ack.mcp.attachments[0].publication_acknowledged, "Q10");

        let ack_replay = state
            .acknowledge_session_realization(acknowledgement)
            .await
            .expect("Q11");
        assert_eq!(ack_replay.action, SessionRealizationAction::Complete, "Q11");
        assert_eq!(
            repo.get("session-phase").await.unwrap().revision,
            after_ack.revision,
            "Q11"
        );

        let renewal = state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-phase".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: "worker-a".into(),
                    runtime_incarnation: "worker-a/incarnation-1".into(),
                    lease_expires_at_unix_ms: u64::MAX,
                    renew_existing_lease: true,
                },
            })
            .await
            .expect("Q12");
        let SessionRealizationAction::Stage { mcp_stages, .. } = &renewal.action else {
            panic!("Q12 expected renewal stage")
        };
        assert_eq!(mcp_stages.len(), 1, "Q12");
        assert_eq!(
            mcp_stages[0].generation.generation,
            awaken_session_contract::McpGeneration(1),
            "Q12"
        );
        assert_eq!(
            mcp_stages[0].generation.lease_expires_at_unix_ms,
            u64::MAX,
            "Q12"
        );
        let publish = state
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-phase".into(),
                lease: renewal.lease.clone(),
                mcp_receipts: exact_receipts(&renewal.action),
            })
            .await
            .expect("Q13 publish");
        let SessionRealizationAction::Publish { publish, drain } = publish.action else {
            panic!("Q13 expected publish")
        };
        let complete = state
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-phase".into(),
                lease: renewal.lease,
                published: publish,
                drained: drain,
            })
            .await
            .expect("Q13");
        assert_eq!(complete.action, SessionRealizationAction::Complete, "Q13");
        let renewed = repo.get("session-phase").await.unwrap();
        assert_eq!(
            renewed.realization.unwrap().expires_at_unix_ms,
            u64::MAX,
            "Q13"
        );
        assert!(renewed.mcp.attachments[0].publication_acknowledged, "Q13");
    }

    #[tokio::test]
    async fn local_lease_supervision_cases_follow_the_decision_table() {
        // Cause graph: active generation AND local owner/incarnation AND expiry
        // within the renewal window -> run the canonical phase driver with a
        // later exact fence. A false due-window cause is a no-op; there is no
        // relay-local timer or second desired-state mutation.
        //
        // | Rule | Active | Owner/incarnation | Due | Effect |
        // |---|---|---|---|---|
        // | S1 | yes | exact local | yes | same generation/epoch, later expiry |
        // | S2 | yes | exact local | no | no mutation |
        let (state, repo) = harness("session-supervised").await;
        state
            .realize_session_locally("session-supervised")
            .await
            .expect("S1 setup");
        let before = repo.get("session-supervised").await.unwrap();
        let lease = before.realization.clone().unwrap();
        let original_generation = before.mcp.attachments[0].generation;
        let original_epoch = lease.epoch;
        let supervision_now = lease.expires_at_unix_ms.saturating_sub(100_000);
        assert_eq!(
            state
                .renew_due_session_realizations(supervision_now)
                .await
                .expect("S1"),
            1,
            "S1"
        );
        let renewed = repo.get("session-supervised").await.unwrap();
        let renewed_lease = renewed.realization.as_ref().unwrap();
        assert!(
            renewed_lease.expires_at_unix_ms > lease.expires_at_unix_ms,
            "S1"
        );
        assert_eq!(renewed_lease.epoch, original_epoch, "S1");
        assert_eq!(
            renewed.mcp.attachments[0].generation, original_generation,
            "S1"
        );
        assert!(renewed.mcp.attachments[0].publication_acknowledged, "S1");
        let revision = renewed.revision;
        assert_eq!(
            state
                .renew_due_session_realizations(supervision_now)
                .await
                .expect("S2"),
            0,
            "S2"
        );
        assert_eq!(
            repo.get("session-supervised").await.unwrap().revision,
            revision,
            "S2 no mutation"
        );
    }

    #[tokio::test]
    async fn live_lease_takeover_follows_owner_and_incarnation_decision_table() {
        // Cause-effect graph:
        // live lease + exact owner + exact incarnation -> replay;
        // live lease + exact owner + new incarnation -> fence old process and
        // allocate epoch N+1; live lease + different owner -> stale.
        //
        // | Rule | lease | owner | incarnation | Effect |
        // | O1 | live | same | same | replay epoch N |
        // | O2 | live | same | new | claim epoch N+1 |
        // | O3 | live | other | any | StaleOwnership |
        let (state, repo) = harness("session-owner-restart").await;
        let target = awaken_session_contract::SessionRealizationTarget {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/incarnation-1".into(),
            lease_expires_at_unix_ms: u64::MAX,
            renew_existing_lease: false,
        };
        let first = state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: target.clone(),
            })
            .await
            .expect("O1 initial claim");
        let replay = state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: target.clone(),
            })
            .await
            .expect("O1 replay");
        assert_eq!(replay.lease, first.lease, "O1");

        let restarted = state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    runtime_incarnation: "worker-a/incarnation-2".into(),
                    ..target.clone()
                },
            })
            .await
            .expect("O2");
        assert_eq!(restarted.lease.epoch, first.lease.epoch + 1, "O2");
        assert_eq!(
            restarted.lease.runtime_incarnation, "worker-a/incarnation-2",
            "O2"
        );
        let persisted = repo.get("session-owner-restart").await.unwrap();
        assert_eq!(
            persisted.mcp.attachments[0]
                .realization
                .as_ref()
                .unwrap()
                .runtime_incarnation,
            "worker-a/incarnation-2",
            "O2 exact generation was reclaimed"
        );

        let other = state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-owner-restart".into(),
                target: awaken_session_contract::SessionRealizationTarget {
                    owner: "worker-b".into(),
                    runtime_incarnation: "worker-b/incarnation-1".into(),
                    ..target
                },
            })
            .await;
        assert_eq!(
            other.unwrap_err(),
            SessionRealizationControlFailure::StaleOwnership,
            "O3"
        );
    }

    #[tokio::test]
    async fn empty_and_failure_cases_are_generated_from_the_decision_table() {
        // Cause graph: pending Resources without MCP still requires
        // Stage/activation/ack. An exact failure terminalizes Realizing MCP and
        // records retryable Resource evidence once; re-delivery is a no-op.
        //
        // | Rule | Pending resources | MCP | Command | Effect |
        // |---|---|---|---|---|
        // | F1 | T | empty | begin | Stage(prepare=true) |
        // | F2 | T | empty | activate+ack | idle, one commit per phase |
        // | F3 | T | Realizing | fail | Failed + activation_failed |
        // | F4 | T | Failed | same fail | replay/no revision |
        let mut empty = persisted_session("session-empty");
        empty.mcp = SessionMcpAttachmentSet::default();
        let (empty_state, empty_repo) = harness_for(empty).await;
        let target = awaken_session_contract::SessionRealizationTarget {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a/incarnation-1".into(),
            lease_expires_at_unix_ms: u64::MAX,
            renew_existing_lease: false,
        };
        let staged = empty_state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-empty".into(),
                target: target.clone(),
            })
            .await
            .expect("F1");
        assert!(
            matches!(
                staged.action,
                SessionRealizationAction::Stage {
                    prepare_session: true,
                    ref mcp_stages,
                } if mcp_stages.is_empty()
            ),
            "F1"
        );
        let activated = empty_state
            .activate_session_realization(ActivateSessionRealization {
                session_id: "session-empty".into(),
                lease: staged.lease.clone(),
                mcp_receipts: Vec::new(),
            })
            .await
            .expect("F2 activate");
        assert!(
            matches!(
                activated.action,
                SessionRealizationAction::Publish {
                    ref publish,
                    ref drain,
                } if publish.is_empty() && drain.is_empty()
            ),
            "F2"
        );
        empty_state
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: "session-empty".into(),
                lease: staged.lease,
                published: Vec::new(),
                drained: Vec::new(),
            })
            .await
            .expect("F2 acknowledge");
        assert_eq!(
            empty_repo.get("session-empty").await.unwrap().status,
            "idle",
            "F2"
        );

        let (failed_state, failed_repo) = harness("session-failed").await;
        let staged = failed_state
            .begin_session_realization(BeginSessionRealization {
                session_id: "session-failed".into(),
                target,
            })
            .await
            .unwrap();
        let failure = FailSessionRealization {
            session_id: "session-failed".into(),
            lease: staged.lease,
            reason: "stage failed".into(),
        };
        failed_state
            .fail_session_realization(failure.clone())
            .await
            .expect("F3");
        let after_failure = failed_repo.get("session-failed").await.unwrap();
        assert_eq!(after_failure.status, "activation_failed", "F3");
        assert_eq!(
            after_failure.mcp.attachments[0].state,
            McpAttachmentState::Failed,
            "F3"
        );
        failed_state
            .fail_session_realization(failure)
            .await
            .expect("F4");
        assert_eq!(
            failed_repo.get("session-failed").await.unwrap().revision,
            after_failure.revision,
            "F4"
        );
    }
}
