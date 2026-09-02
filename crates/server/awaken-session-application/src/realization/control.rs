//! Durable realization and terminal-cleanup control state machine.

use super::*;

enum TerminalCleanupWorkerRevalidation {
    Admitted,
    Ineligible,
    Failed(SessionRealizationControlFailure),
    Abort(SessionRealizationControlFailure),
}

impl TerminalCleanupWorkerRevalidation {
    fn rejection(self) -> Option<TerminalCleanupRootClaim> {
        match self {
            Self::Admitted => None,
            Self::Ineligible => Some(TerminalCleanupRootClaim::Skip),
            Self::Failed(error) => Some(TerminalCleanupRootClaim::Failed(error)),
            Self::Abort(error) => Some(TerminalCleanupRootClaim::Abort(error)),
        }
    }
}

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
            let owner_scope = self
                .owner(&command.session_id)
                .await
                .map_err(mutation_control)?;
            let mut session = self
                .session_repository()
                .get(&command.session_id)
                .await
                .map_err(repository_control)?;
            if session.frozen_baseline().is_none() {
                return Err(SessionRealizationControlFailure::NotReady);
            }
            if !session.is_terminal()
                && !environment_admits_realization_effects(&session.environment)
            {
                return Err(SessionRealizationControlFailure::NotReady);
            }
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
            let assignment = awaken_session_contract::session_realization_assignment(
                session.realization.as_ref().map(|lease| lease.epoch),
                existing_live,
                same_owner,
                same_incarnation,
                command.target.reassign_existing_lease,
            );
            if assignment == awaken_session_contract::SessionRealizationAssignment::StaleOwnership {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            if assignment == awaken_session_contract::SessionRealizationAssignment::EpochExhausted {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session realization lease epoch is exhausted".into(),
                ));
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
            let needs_assignment = matches!(
                assignment,
                awaken_session_contract::SessionRealizationAssignment::Assign { .. }
            );
            if !needs_assignment && requested.is_empty() {
                return self.next_action(owner_scope, &session, false, true).await;
            }

            let lease =
                if let awaken_session_contract::SessionRealizationAssignment::Assign { epoch } =
                    assignment
                {
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

    async fn activate_session_realization_after_refresh(
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

    async fn acknowledge_session_realization_after_refresh(
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

    async fn fail_session_realization_after_refresh(
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

    /// Build the only terminal assignment projection from one already-read
    /// Session root. Claim recovery and warm work polling share this adapter so
    /// neither can select a lease or frozen projection independently.
    pub(crate) async fn terminal_cleanup_assignment_from_snapshot(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<SessionTerminalCleanupAssignment, RunError> {
        let lease = session.realization.clone().ok_or_else(|| {
            RunError::unavailable_classified(
                "session_terminal_cleanup_realization_missing",
                "terminal Session snapshot has no current realization lease",
            )
        })?;
        let projection = self
            .active_frozen_session_projection(owner_scope.to_string(), session, false)
            .await?;
        Ok(SessionTerminalCleanupAssignment {
            session_id: session.session_id.clone(),
            projection,
            lease,
        })
    }

    /// Resolve the exact current registry incarnation that may participate in
    /// an external cleanup claim. HTTP authentication is an adapter boundary;
    /// the root-CAS owner repeats this read so direct callers and a stale
    /// transport snapshot cannot mint a durable Session lease.
    async fn terminal_cleanup_worker_for_target(
        &self,
        target: &awaken_session_contract::SessionRealizationTarget,
    ) -> Result<Option<awaken_worker_contract::WorkerSnapshot>, SessionRealizationControlFailure>
    {
        let observations = self.worker_observation_source().ok_or_else(|| {
            SessionRealizationControlFailure::Unavailable(
                "Worker observation source is not installed for external terminal cleanup".into(),
            )
        })?;
        let workers = observations.list().await.map_err(unavailable)?;
        let mut matching = workers
            .into_iter()
            .filter(|registered| registered.snapshot.identity.worker_id == target.owner);
        let Some(registered) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            return Err(SessionRealizationControlFailure::Unavailable(
                "Worker observation source returned duplicate logical owners".into(),
            ));
        }
        let worker = registered.snapshot;
        let fingerprint = worker.manifest.fingerprint().map_err(unavailable)?;
        let now = now_unix_ms();
        if worker.identity.lease_owner() != target.runtime_incarnation
            || worker.expires_at_ms <= now
            || target.lease_expires_at_unix_ms > worker.expires_at_ms
            || !matches!(
                worker.state,
                awaken_worker_contract::WorkerState::Ready
                    | awaken_worker_contract::WorkerState::Draining
            )
            || fingerprint != worker.capability_fingerprint
            || !worker.manifest.explicitly_supports_terminal_cleanup_v2()
        {
            return Ok(None);
        }
        Ok(Some(worker))
    }

    /// Recompute the complete claim gate from one current Registry read and
    /// one current Session root. The outer scan may use an earlier observation
    /// only as a coarse wake-up hint; this is the sole eligibility decision at
    /// the root-CAS/assignment boundary.
    async fn terminal_cleanup_worker_admits_root(
        &self,
        target: &awaken_session_contract::SessionRealizationTarget,
        owner_scope: &str,
        session: &PersistedSession,
        action: &awaken_session_contract::SessionTerminalCleanupAction,
    ) -> Result<bool, SessionRealizationControlFailure> {
        let Some(worker) = self.terminal_cleanup_worker_for_target(target).await? else {
            return Ok(false);
        };
        let requirements = terminal_cleanup_worker_requirements(owner_scope, session, action)?;
        Ok(awaken_worker_contract::can_claim(&worker.manifest, &requirements).is_ok())
    }

    /// Undo only this invocation's exact uncertain claim after Registry
    /// authority disappears. Each retry reloads the aggregate and preserves
    /// every concurrent field; a different realization proves that another
    /// root writer already won and must never be overwritten.
    async fn compensate_terminal_cleanup_claim(
        &self,
        conflict: &TerminalCleanupClaimConflict,
        initial: Option<PersistedSession>,
    ) -> Result<(), SessionRealizationControlFailure> {
        let mut current = match initial {
            Some(session) => session,
            None => match self.session_repository().get(&conflict.session_id).await {
                Ok(session) => session,
                Err(SessionRepositoryError::NotFound) => return Ok(()),
                Err(error) => return Err(repository_control(error)),
            },
        };
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            if current.session_id != conflict.session_id
                || current.realization.as_ref() != Some(&conflict.attempted_lease)
            {
                return Ok(());
            }
            current.realization = conflict.previous_realization.clone();
            match self
                .commit_session_snapshot(
                    &conflict.owner_scope,
                    current,
                    "revoke-terminal-cleanup-recovery",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    current = match self.session_repository().get(&conflict.session_id).await {
                        Ok(session) => session,
                        Err(SessionRepositoryError::NotFound) => return Ok(()),
                        Err(error) => return Err(repository_control(error)),
                    };
                }
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(mutation_control(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn revalidate_terminal_cleanup_worker(
        &self,
        target: &awaken_session_contract::SessionRealizationTarget,
        owner_scope: &str,
        session: &PersistedSession,
        action: &awaken_session_contract::SessionTerminalCleanupAction,
        conflict: Option<&TerminalCleanupClaimConflict>,
    ) -> TerminalCleanupWorkerRevalidation {
        let admission = self
            .terminal_cleanup_worker_admits_root(target, owner_scope, session, action)
            .await;
        match admission {
            Ok(true) => TerminalCleanupWorkerRevalidation::Admitted,
            Ok(false) => {
                if let Some(conflict) = conflict
                    && let Err(error) = self
                        .compensate_terminal_cleanup_claim(conflict, Some(session.clone()))
                        .await
                {
                    return TerminalCleanupWorkerRevalidation::Abort(error);
                }
                TerminalCleanupWorkerRevalidation::Ineligible
            }
            Err(error) => {
                if let Some(conflict) = conflict
                    && let Err(compensation_error) = self
                        .compensate_terminal_cleanup_claim(conflict, Some(session.clone()))
                        .await
                {
                    return TerminalCleanupWorkerRevalidation::Abort(compensation_error);
                }
                TerminalCleanupWorkerRevalidation::Failed(error)
            }
        }
    }

    async fn claim_terminal_cleanup_root(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        target: &awaken_session_contract::SessionRealizationTarget,
        scope: TerminalCleanupClaimScope<'_>,
        replay_after_conflict: Option<&TerminalCleanupClaimConflict>,
    ) -> TerminalCleanupRootClaim {
        if !scope.admits(self, &session)
            || !session.is_terminal()
            || !session.terminal_cleanup.is_requested()
        {
            return TerminalCleanupRootClaim::Skip;
        }
        // A realization lease is also the only durable logical-Worker affinity
        // for Local/Namespace storage and the Container/Kubernetes realization
        // namespace. Static manifest compatibility can narrow a candidate but
        // can never authorize a foreign Worker to reopen that physical source.
        if scope.is_external()
            && session
                .realization
                .as_ref()
                .is_some_and(|current| current.owner != target.owner)
        {
            return TerminalCleanupRootClaim::Skip;
        }
        // With no historical lease, only an Environment that has never acquired
        // a physical binding is eligible for first placement. Resident and
        // continuation states without an affinity fact are legacy/inconsistent
        // evidence and must not be guessed from the current Worker manifest.
        if scope.is_external()
            && session.realization.is_none()
            && !matches!(
                &session.environment,
                awaken_session_contract::SessionEnvironmentState::Unmaterialized
            )
        {
            return TerminalCleanupRootClaim::Failed(SessionRealizationControlFailure::Invalid(
                "terminal Session has physical Environment state without a realization owner"
                    .into(),
            ));
        }
        let worker_action = match session.terminal_cleanup_work_action() {
            Ok(action) => action,
            Err(error) => {
                return TerminalCleanupRootClaim::Failed(
                    SessionRealizationControlFailure::Invalid(error.to_string()),
                );
            }
        };
        let Some(worker_action) = worker_action else {
            return TerminalCleanupRootClaim::Skip;
        };
        if scope.is_external()
            && let Some(rejection) = self
                .revalidate_terminal_cleanup_worker(
                    target,
                    owner_scope,
                    &session,
                    &worker_action,
                    replay_after_conflict,
                )
                .await
                .rejection()
        {
            return rejection;
        }

        let current_is_live = session.realization.as_ref().is_some_and(|current| {
            awaken_session_contract::realization_lease_is_live_at(
                current.expires_at_unix_ms,
                now_unix_ms(),
            )
        });
        if current_is_live {
            let current = session
                .realization
                .clone()
                .expect("a live realization lease was checked");
            let exact_incarnation = current.owner == target.owner
                && current.runtime_incarnation == target.runtime_incarnation;
            if exact_incarnation && (scope.is_exact_local() || replay_after_conflict.is_some()) {
                let assignment = self
                    .terminal_cleanup_assignment_from_snapshot(owner_scope, &session)
                    .await;
                if scope.is_external()
                    && let Some(rejection) = self
                        .revalidate_terminal_cleanup_worker(
                            target,
                            owner_scope,
                            &session,
                            &worker_action,
                            replay_after_conflict,
                        )
                        .await
                        .rejection()
                {
                    return rejection;
                }
                return match assignment {
                    Ok(assignment) => TerminalCleanupRootClaim::Assignment(Box::new(assignment)),
                    Err(error) => TerminalCleanupRootClaim::Failed(unavailable(error)),
                };
            }
            // External recovery may advance an exact current incarnation only
            // when the authenticated heartbeat carries a strictly newer
            // expiry. The resulting root CAS increments the epoch exactly once;
            // its equal-expiry replay is skipped unless this invocation is
            // recovering an ambiguous committed CAS above. A live predecessor
            // incarnation or foreign logical owner remains provider-affined and
            // is never preempted by manifest compatibility.
            let exact_external_renewal = scope.is_external()
                && exact_incarnation
                && current.expires_at_unix_ms < target.lease_expires_at_unix_ms;
            if !exact_external_renewal {
                return if scope.is_exact_local() {
                    TerminalCleanupRootClaim::Failed(
                        SessionRealizationControlFailure::StaleOwnership,
                    )
                } else {
                    TerminalCleanupRootClaim::Skip
                };
            }
        }

        let epoch = match session.realization.as_ref() {
            Some(current) => match current.epoch.checked_add(1) {
                Some(epoch) => epoch,
                None => {
                    return TerminalCleanupRootClaim::Failed(
                        SessionRealizationControlFailure::Invalid(
                            "Session realization lease epoch is exhausted".into(),
                        ),
                    );
                }
            },
            None => 1,
        };
        let claim = TerminalCleanupClaimConflict {
            owner_scope: owner_scope.to_string(),
            session_id: session.session_id.clone(),
            attempted_lease: SessionRealizationLease {
                owner: target.owner.clone(),
                runtime_incarnation: target.runtime_incarnation.clone(),
                epoch,
                expires_at_unix_ms: target.lease_expires_at_unix_ms,
            },
            previous_realization: session.realization.clone(),
        };
        session.realization = Some(claim.attempted_lease.clone());
        let committed = match self
            .commit_session_snapshot(
                owner_scope,
                session,
                "claim-terminal-cleanup-recovery",
                Vec::new(),
            )
            .await
        {
            Ok(committed) => committed,
            Err(SessionMutationError::Conflict) => {
                return TerminalCleanupRootClaim::Conflict(Box::new(claim));
            }
            Err(error) => return TerminalCleanupRootClaim::Failed(unavailable(error)),
        };
        let assignment = self
            .terminal_cleanup_assignment_from_snapshot(owner_scope, &committed)
            .await;
        if scope.is_external() {
            let committed_action = match committed.terminal_cleanup_work_action() {
                Ok(Some(action)) => action,
                Ok(None) => {
                    return TerminalCleanupRootClaim::Failed(
                        SessionRealizationControlFailure::Invalid(
                            "claimed terminal Session has no remaining cleanup action".into(),
                        ),
                    );
                }
                Err(error) => {
                    return TerminalCleanupRootClaim::Failed(
                        SessionRealizationControlFailure::Invalid(error.to_string()),
                    );
                }
            };
            if let Some(rejection) = self
                .revalidate_terminal_cleanup_worker(
                    target,
                    owner_scope,
                    &committed,
                    &committed_action,
                    Some(&claim),
                )
                .await
                .rejection()
            {
                return rejection;
            }
        }
        match assignment {
            Ok(assignment) => TerminalCleanupRootClaim::Assignment(Box::new(assignment)),
            // The claim is already durable. An exact local retry reuses this
            // lease; an external scan skips it until expiry.
            Err(error) => TerminalCleanupRootClaim::Failed(unavailable(error)),
        }
    }

    async fn claim_terminal_cleanup_after_refresh(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure> {
        validate_realization_target(&target)?;
        if target.reassign_existing_lease {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup recovery claims cannot reassign a live logical owner".into(),
            ));
        }
        let mut conflicted_claim: Option<Box<TerminalCleanupClaimConflict>> = None;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            // Re-read the canonical Worker registry on every root-CAS attempt.
            // A conflict never carries eligibility from the older Session or
            // Worker snapshot into the retry.
            match self.terminal_cleanup_worker_for_target(&target).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Some(conflict) = conflicted_claim.as_deref() {
                        self.compensate_terminal_cleanup_claim(conflict, None)
                            .await?;
                    }
                    return Ok(None);
                }
                Err(error) => {
                    if let Some(conflict) = conflicted_claim.as_deref() {
                        self.compensate_terminal_cleanup_claim(conflict, None)
                            .await?;
                    }
                    return Err(error);
                }
            };
            let mut first_projection_failure = None;
            let mut retry_after_conflict = false;
            let mut cursor = None;
            loop {
                let scan = self
                    .session_repository()
                    .reconcilable_sessions_page(cursor.as_ref())
                    .await
                    .map_err(repository_control)?;
                let next_cursor = scan.next_cursor;
                for scoped in scan.sessions {
                    let replay_after_conflict = conflicted_claim
                        .as_deref()
                        .filter(|conflict| conflict.session_id == scoped.session.session_id);
                    match self
                        .claim_terminal_cleanup_root(
                            &scoped.workspace_id,
                            scoped.session,
                            &target,
                            TerminalCleanupClaimScope::External,
                            replay_after_conflict,
                        )
                        .await
                    {
                        TerminalCleanupRootClaim::Assignment(assignment) => {
                            return Ok(Some(*assignment));
                        }
                        TerminalCleanupRootClaim::Skip => {}
                        TerminalCleanupRootClaim::Conflict(conflict)
                            if attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                        {
                            conflicted_claim = Some(conflict);
                            retry_after_conflict = true;
                            break;
                        }
                        TerminalCleanupRootClaim::Conflict(_) => {
                            return Err(SessionRealizationControlFailure::Conflict);
                        }
                        TerminalCleanupRootClaim::Abort(error) => return Err(error),
                        TerminalCleanupRootClaim::Failed(error) => {
                            first_projection_failure.get_or_insert(error);
                        }
                    }
                }
                if retry_after_conflict {
                    break;
                }
                let Some(next_cursor) = next_cursor else {
                    break;
                };
                if cursor
                    .as_ref()
                    .is_some_and(|cursor| next_cursor.session_id() <= cursor.session_id())
                {
                    return Err(SessionRealizationControlFailure::Unavailable(
                        "Session reconciliation cursor did not advance".into(),
                    ));
                }
                cursor = Some(next_cursor);
            }
            if retry_after_conflict {
                continue;
            }
            return match first_projection_failure {
                Some(error) => Err(error),
                None => Ok(None),
            };
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    pub(crate) async fn claim_local_terminal_cleanup_assignment(
        &self,
        session_id: &str,
    ) -> Result<SessionTerminalCleanupAssignment, SessionRealizationControlFailure> {
        self.refresh_executable_projections()
            .await
            .map_err(unavailable)?;
        let lease_expires_at_unix_ms = now_unix_ms()
            .checked_add(LOCAL_SESSION_REALIZATION_LEASE_MS)
            .ok_or_else(|| {
                SessionRealizationControlFailure::Unavailable(
                    "terminal cleanup lease expiry overflow".into(),
                )
            })?;
        let target = awaken_session_contract::SessionRealizationTarget {
            owner: self.local_realization_owner().to_string(),
            runtime_incarnation: self.runtime_incarnation().to_string(),
            lease_expires_at_unix_ms,
            reassign_existing_lease: false,
        };
        validate_realization_target(&target)?;
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self.owner(session_id).await.map_err(mutation_control)?;
            let session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_control)?;
            match self
                .claim_terminal_cleanup_root(
                    &owner_scope,
                    session,
                    &target,
                    TerminalCleanupClaimScope::LocalSession(session_id),
                    None,
                )
                .await
            {
                TerminalCleanupRootClaim::Assignment(assignment) => return Ok(*assignment),
                TerminalCleanupRootClaim::Skip => {
                    return Err(SessionRealizationControlFailure::NotReady);
                }
                TerminalCleanupRootClaim::Conflict(_) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {}
                TerminalCleanupRootClaim::Conflict(_) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                TerminalCleanupRootClaim::Abort(error) => return Err(error),
                TerminalCleanupRootClaim::Failed(error) => return Err(error),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn terminal_cleanup_work_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupWork>,
        SessionRealizationControlFailure,
    > {
        self.terminal_cleanup_work_for_lease(session_id, lease)
            .await
    }

    async fn record_terminal_cleanup_preparation_after_refresh(
        &self,
        lease: &SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_cleanup_preparation_from_root(lease, preparation)
            .await
    }

    async fn record_terminal_cleanup_disposal_after_refresh(
        &self,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_cleanup_disposal_from_root(lease, receipt)
            .await
    }

    async fn terminal_repository_publication_command_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        SessionRealizationControlFailure,
    > {
        self.terminal_repository_publication_command_for_lease(session_id, lease)
            .await
    }

    async fn record_terminal_repository_publication_receipt_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_receipt_from_root(session_id, lease, receipt)
            .await
    }

    async fn record_terminal_repository_publication_rejection_after_refresh(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_repository_publication_rejection_from_root(
            session_id, lease, rejection,
        )
        .await
    }
}

/// Local application drivers cross an executable-refresh boundary before they
/// enter the multi-phase protocol. This adapter reuses that proof for the
/// Stage/Activate/Acknowledge calls without adding a token, cache, or cursor.
pub(super) struct RefreshedSessionRealizationControl<'a>(pub(super) &'a SessionApplication);

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
    }

    async fn renew_session_realization(
        &self,
        command: RenewSessionRealization,
    ) -> Result<SessionRealizationLease, SessionRealizationControlFailure> {
        self.0.renew_session_realization_after_load(command).await
    }
}

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
        self.claim_terminal_cleanup_after_refresh(target).await
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &str,
        lease: &SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupWork>,
        SessionRealizationControlFailure,
    > {
        self.terminal_cleanup_work_after_refresh(session_id, lease)
            .await
    }

    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
        SessionRealizationControlFailure,
    > {
        self.authorize_terminal_cleanup_effect_from_root(effect)
            .await
    }

    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, SessionRealizationControlFailure> {
        self.authorize_terminal_cleanup_disposal_from_root(effect)
            .await
    }

    async fn authorize_checkpoint_release_artifact_effect(
        &self,
        session_id: &str,
        operation: &awaken_session_contract::SessionEnvironmentOperation,
    ) -> Result<String, SessionRealizationControlFailure> {
        self.authorize_checkpoint_release_artifact_effect_from_root(session_id, operation)
            .await
    }

    async fn authorize_terminal_memory_intent(
        &self,
        intent: &awaken_session_contract::SessionTerminalMemoryIntent,
    ) -> Result<
        awaken_session_contract::SessionTerminalMemoryTarget,
        SessionRealizationControlFailure,
    > {
        self.authorize_terminal_memory_intent_from_root(intent)
            .await
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_cleanup_preparation_after_refresh(lease, preparation)
            .await
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.record_terminal_cleanup_disposal_after_refresh(lease, receipt)
            .await
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
}
