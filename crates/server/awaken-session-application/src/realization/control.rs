use super::*;



#[async_trait::async_trait]
impl SessionRealizationControl for RefreshedSessionRealizationControl<'_> {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.0
            .begin_session_realization_after_refresh(command)
            .await
    }

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.0
            .activate_session_realization_after_refresh(command)
            .await
    }

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.0
            .acknowledge_session_realization_after_refresh(command)
            .await
    }

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.0.fail_session_realization_after_refresh(command).await
    }}


#[async_trait::async_trait]
impl SessionRealizationControl for SessionApplication {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        validate_target(&command)?;
        self.refresh_executable_projections()
            .await
            .map_err(unavailable)?;
        self.begin_session_realization_after_refresh(command).await
    }

    async fn renew_session_realization(
        &self,
        command: RenewSessionRealization,
    ) -> Result<SessionRealizationLease, SessionRealizationControlFailure> {
        self.renew_session_realization_after_load(command).await
    }

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.refresh_executable_projections()
            .await
            .map_err(unavailable)?;
        self.activate_session_realization_after_refresh(command)
            .await
    }

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.refresh_executable_projections()
            .await
            .map_err(unavailable)?;
        self.acknowledge_session_realization_after_refresh(command)
            .await
    }

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.fail_session_realization_after_refresh(command).await
    }

    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure> {
        validate_realization_target(&target)?;
        if target.reassign_existing_lease {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup recovery claims cannot reassign a live logical owner".into(),
            ));
        }
        self.refresh_executable_projections()
            .await
            .map_err(unavailable)?;
        self.claim_next_terminal_cleanup_after_refresh(target).await
    }

    async fn terminal_repository_publication_command(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        self.terminal_repository_publication_command_after_refresh(session_id, lease)
            .await
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_receipt_after_refresh(
            session_id, lease, receipt,
        )
        .await
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_rejection_after_refresh(
            session_id, lease, rejection,
        )
        .await
    }

    async fn terminal_cleanup_commands(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<Vec<awaken_session_contract::SessionCleanupCommand>>,
        SessionRealizationControlFailure,
    > {
        self.terminal_cleanup_commands_after_refresh(session_id, lease)
            .await
    }

    async fn record_terminal_cleanup_completion(
        &self,
        lease: &SessionRealizationLease,
        completion: awaken_session_contract::SessionCleanupCompletion,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_cleanup_completion_after_refresh(lease, completion)
            .await
    }}



impl SessionApplication {
    /// Extend only the existing root-owned realization fence. Renewal never
    /// resolves executable catalogs, pins credentials, materializes transcript
    /// context, or advances pending MCP/Resource desired state.
    pub(super) async fn renew_session_realization_after_load(
        &self,
        command: RenewSessionRealization,
    ) -> Result<SessionRealizationLease, SessionRealizationControlFailure> {
        if command.session_id.trim().is_empty()
            || command.asserted_lease.owner.trim().is_empty()
            || command.asserted_lease.runtime_incarnation.trim().is_empty()
            || command.requested_expires_at_unix_ms < command.asserted_lease.expires_at_unix_ms
            || !awaken_session_contract::realization_lease_is_live_at(
                command.requested_expires_at_unix_ms,
                now_unix_ms(),
            )
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "Session id, exact asserted lease, and a monotonic future expiry are required"
                    .into(),
            ));
        }
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let (owner_scope, mut session) = self
                .session_for_realization_without_credential_migration(&command.session_id)
                .await?;
            let current = session
                .realization
                .clone()
                .ok_or(SessionRealizationControlFailure::StaleOwnership)?;
            if !awaken_session_contract::realization_lease_authorizes(
                &current,
                &command.asserted_lease,
                now_unix_ms(),
            ) {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            if command.requested_expires_at_unix_ms <= current.expires_at_unix_ms {
                return Ok(current);
            }
            let mut renewed = current;
            renewed.expires_at_unix_ms = command.requested_expires_at_unix_ms;
            session
                .mcp
                .extend_active_realization_leases(
                    &renewed.runtime_incarnation,
                    renewed.epoch,
                    renewed.expires_at_unix_ms,
                )
                .map_err(unavailable)?;
            session.realization = Some(renewed);
            match self
                .commit_session_snapshot(
                    &owner_scope,
                    session,
                    "renew-session-realization-lease",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => {
                    return session
                        .realization
                        .ok_or(SessionRealizationControlFailure::NotReady);
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

    pub(super) async fn begin_session_realization_after_refresh(
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
            if existing_live && !same_owner && !command.target.reassign_existing_lease {
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
            // process incarnation after restart. A different owner can do so
            // only when the claim-authenticated topology edge explicitly asks
            // Control to reassign this otherwise independent Session lease.
            let needs_assignment = !existing_live
                || !same_incarnation
                || (command.target.reassign_existing_lease && !same_owner);
            if !needs_assignment && requested.is_empty() {
                return self.next_action(owner_scope, &session, false, true).await;
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
                session
                    .realization
                    .clone()
                    .expect("a live assignment was checked")
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
            if needs_assignment
                && matches!(
                    session.execution,
                    SessionExecutionState::Preparing | SessionExecutionState::Activating
                )
            {
                session.realization_progress.attempts = session
                    .realization_progress
                    .attempts
                    .checked_add(1)
                    .ok_or_else(|| {
                        SessionRealizationControlFailure::Invalid(
                            "Session realization attempt counter is exhausted".into(),
                        )
                    })?;
                session.realization_progress.last_error = None;
                session.realization_progress.failure_source_run_id = None;
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
                    return self
                        .next_action(owner_scope, &session, needs_assignment, true)
                        .await;
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

    pub(super) async fn activate_session_realization_after_refresh(
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
        }
        let expected_generations = expected
            .iter()
            .map(|request| request.generation.clone())
            .collect::<Vec<_>>();
        let receipt_generations = command
            .mcp_receipts
            .iter()
            .map(|receipt| receipt.generation.clone())
            .collect::<Vec<_>>();
        if generation_set_is_renewed_successor(&expected_generations, &receipt_generations)
            && command.mcp_receipts.iter().all(|receipt| {
                expected.iter().any(|request| {
                    awaken_session_contract::realization_generation_authorizes(
                        &request.generation,
                        &receipt.generation,
                    ) && request.realization_id == receipt.realization_id
                        && request.selected_plaintext_holder == receipt.selected_plaintext_holder
                })
            })
        {
            // A heartbeat advanced only the exact lease expiry while this Stage
            // was in flight. The predecessor receipt commits nothing; return the
            // latest Stage so the one driver catches up under current authority.
            return self.next_action(owner_scope, &session, false, false).await;
        }
        for receipt in &command.mcp_receipts {
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
        let prepared_pending_resources = match command.prepared_resource_revision {
            Some(revision)
                if session.resources.pending.is_some()
                    && revision == session.resources.revision =>
            {
                true
            }
            Some(_) if session.resources.pending.is_some() => {
                return Err(SessionRealizationControlFailure::Invalid(
                    "prepared Resource generation does not match the pending Session generation"
                        .into(),
                ));
            }
            Some(_) | None => false,
        };
        let prepared_legacy_resources = if session.resources.pending.is_none()
            && session.resources.activations.is_empty()
            && !session.resources.active.inputs().is_empty()
        {
            match command.prepared_resource_revision {
                Some(revision) if revision == session.resources.revision => true,
                Some(_) => {
                    return Err(SessionRealizationControlFailure::Invalid(
                        "prepared legacy Resource generation does not match the active Session generation"
                            .into(),
                    ));
                }
                None => false,
            }
        } else {
            false
        };
        let needs_activation_commit = prepared_pending_resources
            || prepared_legacy_resources
            || session
                .mcp
                .attachments
                .iter()
                .any(|attachment| attachment.state == McpAttachmentState::Realizing);
        if !needs_activation_commit {
            let publish = Self::publication_generations(&session)?;
            let drain = Self::draining_generations(&session)?;
            return self
                .realization_directive(
                    owner_scope,
                    &session,
                    SessionRealizationAction::Publish { publish, drain },
                    false,
                )
                .await;
        }
        if prepared_pending_resources {
            session.resources.commit().map_err(unavailable)?;
        }
        if prepared_legacy_resources {
            session.resources.adopt_legacy_active(&command.session_id);
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
        // A heartbeat may have extended the aggregate lease while the Runtime
        // was staging a previously admitted request. Promote the newly Active
        // durable claim to that current lease before publication. The Runtime's
        // lease-only port already updated the same resident projection, so no
        // second Stage or credential materialization is required.
        let current_lease = session
            .realization
            .clone()
            .ok_or(SessionRealizationControlFailure::NotReady)?;
        session
            .mcp
            .extend_active_realization_leases(
                &current_lease.runtime_incarnation,
                current_lease.epoch,
                current_lease.expires_at_unix_ms,
            )
            .map_err(unavailable)?;
        session.mcp.begin_obsolete_drains().map_err(unavailable)?;
        // Initial creation remains non-visible until publication acknowledgement.
        // A hot mutation belongs to an already-idle Session, so keep that lifecycle
        // status while its new generation is unacknowledged; a failed replacement
        // must not turn the established Session into a failed create.
        if !session.execution.admits_activity() {
            session
                .transition_execution(SessionExecutionState::Activating)
                .map_err(unavailable)?;
        }
        let session = self
            .commit_resource_snapshot(
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
        self.realization_directive(
            owner_scope,
            &session,
            SessionRealizationAction::Publish { publish, drain },
            false,
        )
        .await
    }

    pub(super) async fn acknowledge_session_realization_after_refresh(
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
            && session.execution == SessionExecutionState::Idle
        {
            return self
                .realization_directive(
                    owner_scope,
                    &session,
                    SessionRealizationAction::Complete,
                    false,
                )
                .await;
        }
        let publish_mismatch = keys(&expected_publish) != keys(&command.published);
        let drain_mismatch = keys(&expected_drain) != keys(&command.drained);
        if publish_mismatch
            && !drain_mismatch
            && generation_set_is_renewed_successor(&expected_publish, &command.published)
        {
            // Publication happened under a shorter same-epoch lease while a
            // heartbeat advanced durable authority. Do not acknowledge the old
            // fence and do not fail the Session: return the latest Stage/Publish
            // work to the same canonical driver.
            return self.next_action(owner_scope, &session, false, false).await;
        }
        if publish_mismatch || drain_mismatch {
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
        let initial_ready = !session.has_active_activities()
            && session.activity_epoch == 0
            && session.execution != SessionExecutionState::Idle;
        // Realization publication settles physical readiness, not the activity
        // fence. An initially claimed Worker may acknowledge while the driving
        // activity is still recorded under Preparing/Activating; promote that
        // same activity to Running and open its one interval. A replacement may
        // already observe Running. Only the final activity settlement may close
        // the interval and return the Session to Idle.
        if session.has_active_activities() {
            if session.execution != SessionExecutionState::Running {
                if session.execution != SessionExecutionState::Idle {
                    session
                        .transition_execution(SessionExecutionState::Idle)
                        .map_err(unavailable)?;
                }
                session
                    .transition_execution(SessionExecutionState::Running)
                    .map_err(unavailable)?;
            }
            session.begin_runtime_interval(now_unix_ms());
        } else if session.execution != SessionExecutionState::Running {
            session
                .transition_execution(SessionExecutionState::Idle)
                .map_err(unavailable)?;
        }
        let ready_fact =
            initial_ready.then(|| initial_idle_fact(&owner_scope, &command.session_id));
        let session = self
            .commit_session_snapshot(
                &owner_scope,
                session,
                "acknowledge-session-realization",
                ready_fact.iter().cloned().collect(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?;
        if ready_fact.is_some() {
            self.notify_lifecycle_fact();
        }
        self.realization_directive(
            owner_scope,
            &session,
            SessionRealizationAction::Complete,
            false,
        )
        .await
    }

    pub(super) async fn fail_session_realization_after_refresh(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        if command.reason.trim().is_empty() {
            return Err(SessionRealizationControlFailure::Invalid(
                "realization failure reason is empty".into(),
            ));
        }
        // Failure delivery is idempotent even though activation_failed is
        // terminal for every new phase command. Handle that exact replay before
        // the common terminal guard used by begin/activate/acknowledge.
        match self.session_repository().get(&command.session_id).await {
            Ok(session) if session.execution == SessionExecutionState::ActivationFailed => {
                return Ok(());
            }
            Ok(_) | Err(SessionRepositoryError::NotFound) => {}
            Err(error) => return Err(repository_control(error)),
        }
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease)?;
        if command.prepared_resource_revision.is_some_and(|revision| {
            session.resources.pending.is_some() && revision != session.resources.revision
        }) {
            return Err(SessionRealizationControlFailure::Invalid(
                "failed Resource generation does not match the pending Session generation".into(),
            ));
        }
        if command.prepared_resource_revision == Some(session.resources.revision)
            && session.resources.pending.is_some()
        {
            session
                .resources
                .note_retryable_failure(command.reason.clone())
                .map_err(unavailable)?;
        }
        session.realization_progress.last_error = Some(command.reason.clone());
        session.realization_progress.failure_source_run_id =
            command.source_run_id.clone().map(Box::new);
        let initial_realization = session.execution != SessionExecutionState::Idle;
        if command.retryable
            && initial_realization
            && session.realization_progress.attempts < self.realization_retry_budget()
        {
            // Retain the exact MCP/Resource generation for recovery, but expire
            // this assignment immediately. The next fenced claim increments the
            // lease epoch and the persisted attempt counter before any effect.
            if let Some(lease) = &mut session.realization {
                lease.expires_at_unix_ms = 0;
            }
            self.commit_session_snapshot(
                &owner_scope,
                session,
                "retry-session-realization",
                Vec::new(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?;
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
        let failed_running_activity = session.execution == SessionExecutionState::Running;
        if session.execution != SessionExecutionState::Idle {
            session
                .transition_execution(SessionExecutionState::ActivationFailed)
                .map_err(unavailable)?;
        }
        // A terminal realization failure ends an admitted driving activity.
        // Close its aggregate-owned interval in the same root CAS and emit the
        // same pricing-neutral lifecycle fact as ordinary/terminal settlement.
        // Retryable failures below budget remain Running and retain the open
        // interval through the earlier return above.
        let lifecycle_facts = failed_running_activity
            .then(now_unix_ms)
            .and_then(|ended_at_unix_ms| session.close_runtime_interval(ended_at_unix_ms))
            .map(|interval| runtime_interval_fact(&owner_scope, &command.session_id, interval))
            .into_iter()
            .collect::<Vec<_>>();
        let emitted_runtime_interval = !lifecycle_facts.is_empty();
        self.commit_session_snapshot(
            &owner_scope,
            session,
            "fail-session-realization",
            lifecycle_facts,
        )
        .await
        .map_err(|error| match error {
            SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
            error => unavailable(error),
        })?;
        if emitted_runtime_interval {
            self.notify_lifecycle_fact();
        }
        Ok(())
    }

    pub(super) async fn terminal_repository_publication_command_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        self.external_terminal_repository_publication_command(session_id, lease)
            .await
    }

    pub(super) async fn record_terminal_repository_publication_receipt_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_external_terminal_repository_publication_receipt(session_id, lease, receipt)
            .await
    }

    pub(super) async fn record_terminal_repository_publication_rejection_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_external_terminal_repository_publication_rejection(session_id, lease, rejection)
            .await
    }}
