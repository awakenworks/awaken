// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.publication_excludes_staged_retired_generation
// @formal evidence session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
//! Durable realization and terminal-cleanup control state machine.

use super::*;
use crate::mutation::ROOT_CAS_ATTEMPTS;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use awaken_session_configuration_contract::resource::realization_lease_is_live_at;
use std::sync::Arc;
use tracing::Instrument as _;

/// Apply one validated activation to the candidate root: commit the prepared
/// Resource generation, activate every realizing MCP generation, refresh the
/// projection leases and begin obsolete drains. Returns whether the root
/// changed and must be committed; the aggregate decides each transition.
fn apply_activation(
    session: &mut SessionAggregate,
    expected: Vec<StageMcpAttachment>,
    prepared_pending_resources: bool,
    prepared_legacy_resources: bool,
) -> Result<bool, SessionRealizationControlFailure> {
    let needs_activation_commit = prepared_pending_resources
        || prepared_legacy_resources
        || session
            .mcp
            .attachments
            .iter()
            .any(|attachment| attachment.state == McpAttachmentState::Realizing);
    if !needs_activation_commit {
        return Ok(false);
    }
    if prepared_pending_resources {
        session.resources().commit().map_err(unavailable)?;
    }
    if prepared_legacy_resources {
        session.resources().adopt_legacy_active();
    }
    for request in expected {
        let attachment = session
            .mcp
            .attachments
            .iter()
            .find(|attachment| &attachment.generation == request.generation.generation())
            .ok_or(SessionRealizationControlFailure::NotReady)?;
        if attachment.state == McpAttachmentState::Realizing {
            session
                .mcp()
                .activate(request.generation.generation(), &request.realization_id)
                .map_err(unavailable)?;
        }
    }
    // Durable authority advances without rewriting the completed physical
    // receipt. Runtime publication uses that exact receipt under its live
    // same-generation Session lease, so continual heartbeats cannot starve
    // the Stage -> Publish -> Complete transition.
    session
        .assignment()
        .refresh_projection_leases()
        .map_err(unavailable)?;
    session.mcp().begin_obsolete_drains().map_err(unavailable)?;
    // Initial creation remains non-visible until publication acknowledgement.
    // A hot mutation belongs to an already-idle Session, so keep that lifecycle
    // status while its new generation is unacknowledged; a failed replacement
    // must not turn the established Session into a failed create.
    if !session.execution.admits_activity() {
        session
            .lifecycle()
            .transition_execution(SessionExecutionState::Activating)
            .map_err(unavailable)?;
    }
    Ok(true)
}

impl SessionRealizations {
    /// Monotonic, idempotent demand on the one Session root. Product callers
    /// can request Worker preparation after creation or after an upgrade; the
    /// Worker polls only this retained intent and never receives a model Run.
    pub async fn request_run_free_initial_preparation(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionRealizationControlFailure> {
        let mutations = &self.mutations;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = mutations
                .owner(session_id)
                .await
                .map_err(mutation_control)?;
            let mut session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_control)?;
            if session
                .realization_progress()
                .run_free_initial_preparation()
            {
                return Ok(session.into_snapshot());
            }
            let external = session.requires_external_environment_owner()
                && session
                    .frozen_baseline()
                    .is_some_and(|baseline| !baseline.environment.self_hosted);
            if !external {
                return Err(SessionRealizationControlFailure::Invalid(
                    "run-free preparation requires a non-self-hosted registered Worker Session"
                        .into(),
                ));
            }
            if session.is_terminal() || session.has_active_activities() {
                return Err(SessionRealizationControlFailure::NotReady);
            }
            if !session.needs_initial_realization_recovery() {
                // An already-completed historical Session needs no new work;
                // the caller must assess its actual Environment/MCP facts.
                return Ok(session.into_snapshot());
            }
            session
                .assignment()
                .request_run_free_preparation()
                .map_err(unavailable)?;
            match mutations
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "request-run-free-initial-preparation",
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => {
                    self.lifecycle.request_reconciliation();
                    return Ok(commit.into_session());
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
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

    #[cfg(feature = "test-support")]
    async fn observe_realization_root(
        &self,
        point: &'static str,
        id: &awaken_session_types::SessionId,
    ) {
        use awaken_session_execution_contract::realization_observation as observation;
        if observation::enabled() {
            let root = match self.mutations.repository().get(id).await {
                Ok(root) => root,
                Err(error) => {
                    observation::checkpoint(
                        point,
                        id,
                        serde_json::json!({"read_error": error.to_string()}),
                    )
                    .await;
                    return;
                }
            };
            let attachments = root
                .mcp
                .attachments
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "key": observation::generation_observation_key(&a.generation),
                        "state": a.state, "ack": a.publication_acknowledged,
                        "claim": a.realization,
                    })
                })
                .collect::<Vec<_>>();
            observation::checkpoint(
                point,
                id,
                serde_json::json!({
                    "revision": root.revision.0, "created": root.created_at_unix_ms,
                    "updated": root.updated_at_unix_ms, "lease": root.realization,
                    "attachments": attachments,
                }),
            )
            .await;
        }
    }

    pub(super) async fn begin_session_realization_after_refresh(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        #[cfg(feature = "test-support")]
        let id = command.session_id.clone();
        #[cfg(feature = "test-support")]
        self.observe_realization_root("begin-before", &id).await;
        let result = self.begin_session_realization_inner(command).await;
        #[cfg(feature = "test-support")]
        self.observe_realization_root(
            if result.is_ok() {
                "begin-after"
            } else {
                "begin-error"
            },
            &id,
        )
        .await;
        result
    }

    pub(super) async fn activate_session_realization_after_refresh(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        #[cfg(feature = "test-support")]
        let id = command.session_id.clone();
        #[cfg(feature = "test-support")]
        self.observe_realization_root("activate-before", &id).await;
        let result = self.activate_session_realization_inner(command).await;
        #[cfg(feature = "test-support")]
        self.observe_realization_root(
            if result.is_ok() {
                "activate-after"
            } else {
                "activate-error"
            },
            &id,
        )
        .await;
        result
    }

    pub(super) async fn acknowledge_session_realization_after_refresh(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        #[cfg(feature = "test-support")]
        let id = command.session_id.clone();
        #[cfg(feature = "test-support")]
        self.observe_realization_root("ack-before", &id).await;
        let result = self.acknowledge_session_realization_inner(command).await;
        #[cfg(feature = "test-support")]
        self.observe_realization_root(
            if result.is_ok() {
                "ack-after"
            } else {
                "ack-error"
            },
            &id,
        )
        .await;
        result
    }

    /// Extend only the existing root-owned realization fence. Renewal never
    /// resolves executable catalogs, pins credentials, materializes transcript
    /// context, or advances pending MCP/Resource desired state.
    #[tracing::instrument(name = "session.realization.root_renewal", skip_all, fields(session_id = %command.session_id, epoch = command.asserted_lease.epoch))]
    pub(super) async fn renew_session_realization_after_load(
        &self,
        command: RenewSessionRealization,
    ) -> Result<SessionRealizationLease, SessionRealizationControlFailure> {
        if command.session_id.as_str().trim().is_empty()
            || command.asserted_lease.owner.trim().is_empty()
            || command.asserted_lease.runtime_incarnation.trim().is_empty()
            || command.requested_expires_at_unix_ms < command.asserted_lease.expires_at_unix_ms
            || !realization_lease_is_live_at(
                command.requested_expires_at_unix_ms,
                self.clock.now_ms(),
            )
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "Session id, exact asserted lease, and a monotonic future expiry are required"
                    .into(),
            ));
        }
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let attempt_span = tracing::info_span!("session.realization.root_cas", attempt);
            let (owner_scope, session) = self
                .load_realization_root(&command.session_id)
                .instrument(attempt_span.clone())
                .await?;
            let mut session =
                crate::mutation::aggregate_from_snapshot(session).map_err(repository_control)?;
            if session.frozen_baseline().is_none() {
                return Err(SessionRealizationControlFailure::NotReady);
            }
            transition_environment(
                SessionEnvironmentFacts::project(
                    session.snapshot(),
                    self.clock.now_ms(),
                    Some(&command.asserted_lease),
                ),
                SessionEnvironmentEvent::RenewRealization,
            )?;
            let current = session
                .realization
                .clone()
                .ok_or(SessionRealizationControlFailure::StaleOwnership)?;
            if command.requested_expires_at_unix_ms <= current.expires_at_unix_ms {
                return Ok(current);
            }
            let mut renewed = current;
            renewed.expires_at_unix_ms = command.requested_expires_at_unix_ms;
            session.assignment().renew(renewed).map_err(unavailable)?;
            match self
                .mutations
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "renew-session-realization-lease",
                    Vec::new(),
                )
                .instrument(
                    tracing::info_span!(parent: &attempt_span, "session.realization.root_commit"),
                )
                .await
            {
                Ok(commit) => {
                    return commit
                        .into_session()
                        .realization
                        .ok_or(SessionRealizationControlFailure::NotReady);
                }
                // The existing bounded loop owns every retry; exhaustion
                // returns the same Conflict below, without a second last-row path.
                Err(SessionMutationError::Conflict) => {}
                Err(error) => return Err(unavailable(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    pub(super) async fn begin_session_realization_inner(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        validate_target(&command, self.clock.now_ms())?;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let (owner_scope, mut session) =
                self.session_for_realization(&command.session_id).await?;
            // Retained execution carries its original generation even when a
            // later replacement is pending. Choose under this same root CAS;
            // never stage a different generation and rely on Worker rejection.
            let activate_pending_resources = match command.expected_resources.as_ref() {
                None => true,
                Some(expected) => {
                    if expected.workspace_id != owner_scope {
                        return Err(SessionRealizationControlFailure::Invalid(
                            "claimed Resource Workspace does not match the Session".into(),
                        ));
                    }
                    if session.resources.retained_generation(expected.revision)
                        != Some(&expected.resources)
                    {
                        return Err(SessionRealizationControlFailure::Invalid(
                            "claimed Resource generation is neither active nor pending".into(),
                        ));
                    }
                    expected.revision != session.resources.active_revision()
                }
            };
            let now = self.clock.now_ms();
            let existing_live = session
                .realization
                .as_ref()
                .is_some_and(|lease| realization_lease_is_live_at(lease.expires_at_unix_ms, now));
            let same_owner = session
                .realization
                .as_ref()
                .is_some_and(|lease| lease.owner == command.target.owner);
            let same_incarnation = session.realization.as_ref().is_some_and(|lease| {
                lease.runtime_incarnation == command.target.runtime_incarnation
            });
            let assignment = awaken_session_execution_contract::session_realization_assignment(
                session.realization.as_ref().map(|lease| lease.epoch),
                existing_live,
                same_owner,
                same_incarnation,
                command.target.reassign_existing_lease,
            );
            if assignment
                == awaken_session_execution_contract::SessionRealizationAssignment::StaleOwnership
            {
                return Err(SessionRealizationControlFailure::StaleOwnership);
            }
            if assignment
                == awaken_session_execution_contract::SessionRealizationAssignment::EpochExhausted
            {
                return Err(SessionRealizationControlFailure::Invalid(
                    "Session realization lease epoch is exhausted".into(),
                ));
            }

            let requested = session
                .mcp
                .attachments
                .iter()
                .filter(|attachment| attachment.state == McpAttachmentState::Requested)
                .map(|attachment| attachment.generation.clone())
                .collect::<Vec<_>>();
            // One authenticated logical owner may immediately fence its prior
            // process incarnation after restart. A different owner can do so
            // only when the claim-authenticated topology edge explicitly asks
            // Control to reassign this otherwise independent Session lease.
            let needs_assignment = matches!(
                assignment,
                awaken_session_execution_contract::SessionRealizationAssignment::Assign { .. }
            );
            if !needs_assignment && requested.is_empty() {
                return self
                    .next_action(owner_scope, &session, false, activate_pending_resources)
                    .await;
            }

            let lease =
                if let awaken_session_execution_contract::SessionRealizationAssignment::Assign {
                    epoch,
                } = assignment
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
            if activate_pending_resources && session.resources.pending.is_some() {
                session.resources().start_attempt().map_err(unavailable)?;
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
                    .map(|attachment| attachment.generation.clone())
                    .collect::<Vec<_>>()
            } else {
                requested
            };
            for generation in to_claim {
                let realization_id = awaken_agent_contract::stable_fingerprint(&(
                    &session.session_id,
                    &generation,
                    &lease.runtime_incarnation,
                    lease.epoch,
                ));
                let claim = crate::session_mcp::McpRealizationClaim {
                    realization_id,
                    runtime_incarnation: lease.runtime_incarnation.clone(),
                    lease_epoch: lease.epoch,
                    lease_expires_at_unix_ms: lease.expires_at_unix_ms,
                    stage_idempotency_key: format!(
                        "stage:{}:{}:{}:{}",
                        session.session_id,
                        generation.attachment_id().0,
                        generation.version(),
                        lease.epoch
                    ),
                };
                let result = if needs_assignment {
                    session.mcp().claim_recovery(&generation, claim)
                } else {
                    session.mcp().claim_realization(&generation, claim)
                };
                result.map_err(unavailable)?;
            }
            let counted = needs_assignment
                && matches!(
                    session.execution,
                    SessionExecutionState::Preparing | SessionExecutionState::Activating
                );
            session
                .assignment()
                .claim(lease, counted)
                .map_err(unavailable)?;
            match self
                .mutations
                .commit_session_snapshot_with_outcome(
                    &owner_scope,
                    session,
                    "begin-session-realization",
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => {
                    return self
                        .next_action(
                            owner_scope,
                            &commit.into_session(),
                            needs_assignment,
                            activate_pending_resources,
                        )
                        .await;
                }
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
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

    async fn activate_session_realization_inner(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease, self.clock.now_ms())?;
        let mut expected = Self::realization_stage_requests(&owner_scope, &session)?;
        let receipt_generations = command
            .mcp_receipts
            .iter()
            .map(|receipt| receipt.generation.clone())
            .collect::<Vec<_>>();
        // Recovery can Stage an old Active generation together with its
        // replacement. After activation commits, that old claim is Draining.
        // An exact activation retry may still carry its completed receipt;
        // reconstruct and verify it from the same durable claim, without
        // requiring unrelated drains or authorizing their publication.
        for attachment in &session.mcp.attachments {
            if attachment.state == McpAttachmentState::Draining
                && receipt_generations
                    .iter()
                    .any(|generation| generation.generation() == &attachment.generation)
            {
                expected.push(
                    projection::stage_mcp_request(&owner_scope, &session.session_id, attachment)
                        .map_err(unavailable)?,
                );
            }
        }
        let expected_generations = expected
            .iter()
            .map(|request| request.generation.clone())
            .collect::<Vec<_>>();
        if !generation_set_authorizes(&expected_generations, &receipt_generations) {
            return Err(SessionRealizationControlFailure::Invalid(
                "MCP realization receipt set does not match the authorized generation set".into(),
            ));
        }
        // Renewal changes authority duration, not which Resource generation
        // this Stage prepared. Never consume an unreported pending mutation.
        let prepared_pending_resources = match command.prepared_resource_revision {
            Some(revision)
                if session.resources.pending.is_some()
                    && revision == session.resources.revision =>
            {
                true
            }
            Some(revision) if revision == session.resources.active_revision() => false,
            Some(_) => {
                return Err(SessionRealizationControlFailure::Invalid(
                    "prepared Resource generation is neither active nor pending".into(),
                ));
            }
            None => false,
        };
        for receipt in &command.mcp_receipts {
            let request = expected
                .iter()
                .find(|request| {
                    awaken_session_execution_contract::realization_generation_authorizes(
                        &request.generation,
                        &receipt.generation,
                    )
                })
                .ok_or_else(|| {
                    SessionRealizationControlFailure::Invalid(
                        "MCP realization receipt names an unclaimed generation".into(),
                    )
                })?;
            // Reconstruct the exact admitted request using only its older
            // expiry. Every immutable field and the complete fingerprint still
            // pass the existing verifier. A live same-generation root lease was
            // checked above; no receipt grants or extends execution authority.
            let mut admitted = request.clone();
            admitted.generation = receipt.generation.clone();
            receipt
                .verify(&admitted)
                .map_err(|error| SessionRealizationControlFailure::Invalid(error.to_string()))?;
        }
        let prepared_legacy_resources = if session.resources.pending.is_none()
            && session.resources.activations.is_empty()
            && !session.resources.active.inputs().is_empty()
        {
            match command.prepared_resource_revision {
                Some(revision) if revision == session.resources.active_revision() => true,
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
        if apply_activation(
            &mut session,
            expected,
            prepared_pending_resources,
            prepared_legacy_resources,
        )? {
            session = self
                .resource_references
                .commit(
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
        }
        // Activation can supersede a previously unacknowledged generation and
        // move it to Draining. Publish only receipts still named by the durable
        // publication set, keeping their actual (possibly older) lease expiry.
        // Obsolete staged receipts belong exclusively to the drain below.
        let action = publication_action(
            session.session_id.as_str(),
            &session.mcp.attachments,
            receipt_generations,
        )?;
        self.realization_directive(owner_scope, &session, action, false)
            .await
    }

    async fn acknowledge_session_realization_inner(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease, self.clock.now_ms())?;
        let expected_publish =
            publication_generations(&session.session_id, &session.mcp.attachments)?;
        let expected_drain = draining_generations(&session.session_id, &session.mcp.attachments)?;
        let keys = |items: &[McpGenerationRef]| {
            items
                .iter()
                .map(exact_generation_key)
                .collect::<BTreeSet<_>>()
        };
        if !generation_set_authorizes(&command.published, &command.published)
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
                    && projection::mcp_generation_ref(&session.session_id, attachment).is_ok_and(
                        |current| {
                            awaken_session_execution_contract::realization_generation_authorizes(
                                &current, generation,
                            )
                        },
                    )
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
            && matches!(
                session.execution,
                SessionExecutionState::Idle | SessionExecutionState::Running
            )
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
        let publish_mismatch = !generation_set_authorizes(&expected_publish, &command.published);
        let drain_mismatch = keys(&expected_drain) != keys(&command.drained);
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
                .find(|attachment| &attachment.generation == generation.generation())
                .and_then(|attachment| attachment.realization.as_ref())
                .map(|claim| claim.realization_id.clone())
                .ok_or(SessionRealizationControlFailure::NotReady)?;
            session
                .mcp()
                .acknowledge_publication(generation.generation(), &realization_id)
                .map_err(unavailable)?;
        }
        for generation in expected_drain {
            session
                .mcp()
                .finish_drain(generation.generation())
                .map_err(unavailable)?;
        }
        let initial_ready = session
            .activity()
            .realization_ready(self.clock.now_ms())
            .map_err(unavailable)?;
        let ready_fact = initial_ready.then(|| {
            initial_idle_fact(
                &owner_scope,
                command.session_id.as_str(),
                self.clock.now_ms(),
            )
        });
        let session = self
            .mutations
            .commit_session_snapshot_with_outcome(
                &owner_scope,
                session,
                "acknowledge-session-realization",
                ready_fact.iter().cloned().collect(),
            )
            .await
            .map_err(|error| match error {
                SessionMutationError::Conflict => SessionRealizationControlFailure::Conflict,
                error => unavailable(error),
            })?
            .into_session();
        if ready_fact.is_some() {
            self.lifecycle.notify_fact();
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
        let mutations = &self.mutations;
        if command.reason.trim().is_empty() {
            return Err(SessionRealizationControlFailure::Invalid(
                "realization failure reason is empty".into(),
            ));
        }
        // Failure delivery is idempotent even though activation_failed is
        // terminal for every new phase command. Handle that exact replay before
        // the common terminal guard used by begin/activate/acknowledge.
        match mutations.session_aggregate(&command.session_id).await {
            Ok(session) if session.execution == SessionExecutionState::ActivationFailed => {
                return Ok(());
            }
            Ok(_) | Err(SessionRepositoryError::NotFound) => {}
            Err(error) => return Err(repository_control(error)),
        }
        let (owner_scope, mut session) = self.session_for_realization(&command.session_id).await?;
        verify_lease(&session, &command.lease, self.clock.now_ms())?;
        if command
            .prepared_resource_revision
            .is_some_and(|revision| session.resources.retained_generation(revision).is_none())
        {
            return Err(SessionRealizationControlFailure::Invalid(
                "failed Resource generation is neither active nor pending".into(),
            ));
        }
        if command.prepared_resource_revision == Some(session.resources.revision)
            && session.resources.pending.is_some()
        {
            session
                .resources()
                .note_retryable_failure(command.reason.clone())
                .map_err(unavailable)?;
        }
        session.assignment().record_failure(
            command.reason.clone(),
            self.clock.now_ms(),
            command.source_run_id.clone(),
        );
        command.record_bounded_failure(&mut session, self.realization_retry_budget)?;
        let initial_realization = session.execution != SessionExecutionState::Idle;
        let retry_initial = {
            match command
                .initial_realization_failure_plan(&session, self.realization_retry_budget)?
            {
                awaken_session_execution_contract::RealizationFailureOutcome::Retry {
                    refund_attempt,
                } => {
                    if refund_attempt {
                        session
                            .assignment()
                            .refund_capacity_attempt()
                            .map_err(unavailable)?;
                    }
                    true
                }
                awaken_session_execution_contract::RealizationFailureOutcome::Absorb => false,
            }
        };
        if retry_initial || !initial_realization {
            // The driver closes this exact local authority generation after an
            // effect failure. Any nonterminal recovery must obtain a new epoch;
            // keeping Idle's lease live would replay a permanently closed Host
            // projection after a failed hot replacement. Active MCP/Resources
            // remain retained; only this disposable assignment expires.
            session.assignment().expire();
        }
        if retry_initial {
            // The retry intent's response carries no applied/replayed
            // distinction: a replay observes the same realization progress.
            let _outcome = mutations
                .commit_session_snapshot_with_outcome(
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
                    attachment.generation.clone(),
                    attachment
                        .realization
                        .as_ref()
                        .map(|claim| claim.realization_id.clone()),
                )
            })
            .collect::<Vec<_>>();
        for (generation, realization_id) in realizing {
            session
                .mcp()
                .fail_realization(
                    &generation,
                    realization_id
                        .as_deref()
                        .ok_or(SessionRealizationControlFailure::NotReady)?,
                    command.reason.clone(),
                )
                .map_err(unavailable)?;
        }
        let emitted_runtime_interval = session.running_interval.is_some();
        if session.execution != SessionExecutionState::Idle {
            session
                .lifecycle()
                .transition_execution_at(
                    SessionExecutionState::ActivationFailed,
                    self.clock.now_ms(),
                )
                .map_err(unavailable)?;
        }
        // The aggregate compiler derives the closed interval fact from its
        // exact closing revision together with this terminal transition.
        let lifecycle_facts = Vec::new();
        // Failure is absorbing; the caller reports no applied/replayed bit.
        let _outcome = mutations
            .commit_session_snapshot_with_outcome(
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
            self.lifecycle.notify_fact();
        }
        Ok(())
    }

    /// Resolve the exact current registry incarnation that may participate in
    /// an external cleanup claim. HTTP authentication is an adapter boundary;
    /// the root-CAS owner repeats this read so direct callers and a stale
    /// transport snapshot cannot mint a durable Session lease.
    pub(crate) async fn worker_for_target(
        &self,
        target: &awaken_session_execution_contract::SessionRealizationTarget,
    ) -> Result<Option<awaken_worker_contract::WorkerSnapshot>, SessionRealizationControlFailure>
    {
        let observations = self.execution_admission.workers.get().ok_or_else(|| {
            SessionRealizationControlFailure::Unavailable(
                "Worker observation source is not installed for external terminal cleanup".into(),
            )
        })?;
        let workers = observations.list().await.map_err(unavailable)?;
        target.worker_from_registry_observation(workers, self.clock.now_ms())
    }
}

/// Local application drivers cross an executable-refresh boundary before they
/// enter the multi-phase protocol. This adapter reuses that proof for the
/// Stage/Activate/Acknowledge calls without adding a token, cache, or cursor.
pub(super) struct RefreshedSessionRealizationControl<'a>(pub(super) &'a SessionRealizations);

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
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationLeaseReceipt,
        SessionRealizationControlFailure,
    > {
        // Lease-only renewal performs no executable refresh. Reuse its public
        // entry so local and remote callers share one receipt-publication rule.
        self.0.renew_session_realization(command).await
    }
}

#[async_trait::async_trait]
impl SessionRealizationControl for SessionRealizations {
    async fn begin_session_realization(
        &self,
        command: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        validate_target(&command, self.clock.now_ms())?;
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(unavailable)?;
        admission
            .enter(|| self.begin_session_realization_after_refresh(command))
            .await
    }

    async fn renew_session_realization(
        &self,
        command: RenewSessionRealization,
    ) -> Result<
        awaken_session_execution_contract::SessionRealizationLeaseReceipt,
        SessionRealizationControlFailure,
    > {
        awaken_session_execution_contract::SessionRealizationLeaseReceipt::issued_at(
            self.renew_session_realization_after_load(command).await?,
            self.clock.now_ms(),
        )
    }

    async fn activate_session_realization(
        &self,
        command: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(unavailable)?;
        admission
            .enter(|| self.activate_session_realization_after_refresh(command))
            .await
    }

    async fn acknowledge_session_realization(
        &self,
        command: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(unavailable)?;
        admission
            .enter(|| self.acknowledge_session_realization_after_refresh(command))
            .await
    }

    async fn fail_session_realization(
        &self,
        command: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.fail_session_realization_after_refresh(command).await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionPreparationControl for SessionRealizations {
    async fn claim_next_session_preparation(
        &self,
        target: awaken_session_execution_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionPreparationAssignment>,
        SessionRealizationControlFailure,
    > {
        validate_realization_target(&target, self.clock.now_ms())?;
        if target.reassign_existing_lease {
            return Err(SessionRealizationControlFailure::Invalid(
                "initial Session preparation cannot reassign a live lease".into(),
            ));
        }
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(unavailable)?;
        admission
            .enter(|| async {
                let Some(worker) = self.worker_for_target(&target).await? else {
                    return Ok(None);
                };
                if worker.state != awaken_worker_contract::WorkerState::Ready {
                    return Ok(None);
                }
                let cursor_state = {
                    let mut cursors = self.session_preparation_cursors.lock().await;
                    let key = target.runtime_incarnation.clone();
                    if cursors.len() >= 4096 && !cursors.contains_key(&key) {
                        // Positions are disposable hints. Cap abandoned Worker
                        // incarnations without dropping Session/root authority.
                        if let Some(evicted_key) = cursors.keys().next().cloned() {
                            cursors.remove(&evicted_key);
                        }
                    }
                    Arc::clone(
                        cursors
                            .entry(key)
                            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))),
                    )
                };
                // Serialize this incarnation's disposable position through
                // selection. Advance only past examined roots, not the whole
                // fetched page: an unfinished head must not starve its peers.
                let mut cursor = cursor_state.lock().await;
                let scan = self
                    .mutations
                    .repository()
                    .preparation_candidates(cursor.as_ref())
                    .await
                    .map_err(repository_control)?;
                if let (Some(previous), Some(next)) = (cursor.as_ref(), scan.next_cursor.as_ref())
                    && next.session_id() == previous.session_id()
                {
                    return Err(SessionRealizationControlFailure::Unavailable(
                        "Session preparation cursor did not advance".into(),
                    ));
                }
                for scoped in scan.sessions {
                    let session_id = scoped.session.session_id;
                    *cursor = Some(
                        awaken_session_contract::SessionScanCursor::after_session_id(
                            session_id.as_str(),
                        ),
                    );
                    // The scan is a wake hint only. Eligibility and the
                    // directive are both re-derived from the current root.
                    let current = match self.mutations.repository().get(&session_id).await {
                        Ok(session) => session,
                        Err(SessionRepositoryError::NotFound) => continue,
                        Err(error) => return Err(repository_control(error)),
                    };
                    if !current.is_run_free_preparation_candidate() {
                        continue;
                    }
                    let compiler = self.session_preparation_placement().ok_or_else(|| {
                        SessionRealizationControlFailure::Unavailable(
                            "Session preparation placement compiler is not installed".into(),
                        )
                    })?;
                    let requirements = compiler.requirements(&current)?;
                    if awaken_worker_contract::can_claim(&worker.manifest, &requirements).is_err() {
                        continue;
                    }
                    let directive = match self
                        .begin_session_realization_after_refresh(BeginSessionRealization {
                            session_id: session_id.clone(),
                            target: target.clone(),
                            expected_resources: None,
                        })
                        .await
                    {
                        Ok(directive) => directive,
                        Err(
                            SessionRealizationControlFailure::StaleOwnership
                            | SessionRealizationControlFailure::NotReady
                            | SessionRealizationControlFailure::Conflict,
                        ) => continue,
                        Err(error) => return Err(error),
                    };
                    // A withdrawn Worker cannot start physical effects from
                    // a lease minted just before its Registry transition. The
                    // refused directive never leaves this process, so its lease
                    // is closed rather than left live on a retiring Worker.
                    let current_root = self
                        .mutations
                        .repository()
                        .get(&session_id)
                        .await
                        .map_err(repository_control)?;
                    let admitted = match self.worker_for_target(&target).await? {
                        Some(worker) => {
                            worker.state == awaken_worker_contract::WorkerState::Ready
                                && current_root.revision == directive.projection.revision
                                && current_root.is_run_free_preparation_candidate()
                                && awaken_worker_contract::can_claim(
                                    &worker.manifest,
                                    &compiler.requirements(&current_root)?,
                                )
                                .is_ok()
                        }
                        None => false,
                    };
                    if !admitted {
                        self.mutations
                            .expire_exact_assignment(&session_id, &directive.authority.lease)
                            .await?;
                        return Ok(None);
                    }
                    return Ok(Some(
                        awaken_session_execution_contract::SessionPreparationAssignment {
                            session_id,
                            directive,
                        },
                    ));
                }
                // Every returned row was examined. Retain the repository's
                // continuation (including an empty filtered page), or wrap at
                // the proven end of this index snapshot on the next poll.
                *cursor = scan.next_cursor;
                Ok(None)
            })
            .await
    }
}
