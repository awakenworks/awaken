//! Canonical mutable Session command and its durable/effect ordering.

use std::collections::BTreeMap;

use awaken_session_contract::{
    IdempotencyRecord, McpAttachmentState, PersistedSession, RunError, SessionExecutionState,
    SessionRevision, SessionToolConfiguration, realization_lease_is_live_at, stable_fingerprint,
};

use super::{
    McpAttachmentCandidate, SessionApplication, SessionMutationError, mutation::repository_failure,
};

/// Protocol-neutral mutable Session command.
#[derive(Clone, Debug)]
pub struct SessionUpdateCommand {
    pub title: Option<SessionFieldUpdate<String>>,
    pub metadata: Option<SessionMetadataUpdate>,
    pub budget: Option<SessionFieldUpdate<u64>>,
    pub tools: Option<SessionToolConfiguration>,
    pub mcp_candidates: Option<Vec<McpAttachmentCandidate>>,
    pub idempotency_key: Option<String>,
    /// Opaque request-equivalence fingerprint compiled by the interface that
    /// owns the request representation.
    pub request_fingerprint: String,
    pub if_match: Option<SessionRevision>,
}

/// Explicit mutation of an optional Session field. `None` on the command means
/// unchanged; the enum distinguishes clear from replace without nested Options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionFieldUpdate<T> {
    Clear,
    Replace(T),
}

/// Session metadata is patched by key rather than replaced wholesale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMetadataUpdate {
    Clear,
    Patch(BTreeMap<String, Option<String>>),
}

/// Which durable projections changed while applying an update.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionUpdateChanges {
    pub title: bool,
    pub metadata: bool,
    pub tools: bool,
    pub mcp: bool,
    pub budget: bool,
}

impl SessionUpdateChanges {
    #[must_use]
    pub fn any(self) -> bool {
        self.title || self.metadata || self.tools || self.mcp || self.budget
    }

    #[must_use]
    pub fn agent(self) -> bool {
        self.tools || self.mcp
    }
}

/// Committed outcome. `command_revision` remains the original receipt revision
/// on a replay even when later root mutations have advanced `session.revision`.
#[derive(Clone, Debug)]
pub struct SessionUpdateOutcome {
    pub session: PersistedSession,
    pub command_revision: SessionRevision,
    pub command_applied: bool,
    pub changes: SessionUpdateChanges,
}

/// Failure from the mutable Session command.
#[derive(Debug, thiserror::Error)]
pub enum SessionUpdateError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session agent updates require an idle Session")]
    NotIdle,
    #[error("Session baseline is not frozen")]
    NotFrozen,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
    #[error("Session update was rejected: {0}")]
    Rejected(#[source] RunError),
    #[error("Session realization failed: {0}")]
    Realization(#[source] super::SessionRealizationError),
    /// Durable truth committed before its disposable Runtime projection failed.
    /// The outcome lets interface adapters project the committed fact once and
    /// an idempotent replay repairs the Runtime from current durable truth.
    #[error("Session Runtime projection failed after commit: {source}")]
    ProjectionAfterCommit {
        outcome: Box<SessionUpdateOutcome>,
        #[source]
        source: RunError,
    },
    #[error("Session update persistence is unavailable: {0}")]
    Unavailable(String),
}

impl SessionUpdateError {
    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::IdempotencyMismatch,
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }

    fn realization(error: super::SessionRealizationError) -> Self {
        match error {
            super::SessionRealizationError::Control(
                awaken_session_contract::SessionRealizationControlFailure::NotFound,
            ) => Self::NotFound,
            super::SessionRealizationError::Control(
                awaken_session_contract::SessionRealizationControlFailure::Conflict,
            ) => Self::Conflict,
            error => Self::Realization(error),
        }
    }
}

impl SessionApplication {
    #[must_use]
    pub fn update_operation_id(session_id: &str, idempotency_key: &str) -> String {
        stable_fingerprint(&(session_id, idempotency_key))
    }

    /// Apply one mutable Session command through the sole root CAS and then
    /// reconcile its disposable Runtime tool projection.
    pub async fn update_session(
        &self,
        session_id: &str,
        command: SessionUpdateCommand,
    ) -> Result<SessionUpdateOutcome, SessionUpdateError> {
        let command_record = command
            .idempotency_key
            .as_ref()
            .map(|key| IdempotencyRecord {
                key: format!(
                    "managed:update-command:{session_id}:{}",
                    Self::update_operation_id(session_id, key)
                ),
                payload_hash: command.request_fingerprint.clone(),
            });

        if let Some(record) = &command_record
            && let Some(receipt) = self
                .session_repository()
                .idempotency_receipt(session_id, &record.key)
                .await
                .map_err(repository_failure)
                .map_err(SessionUpdateError::mutation)?
        {
            if receipt.payload_hash != record.payload_hash {
                return Err(SessionUpdateError::IdempotencyMismatch);
            }
            let session = self
                .session_repository()
                .get(session_id)
                .await
                .map_err(repository_failure)
                .map_err(SessionUpdateError::mutation)?;
            let outcome = SessionUpdateOutcome {
                session,
                command_revision: receipt.committed_revision,
                command_applied: false,
                changes: SessionUpdateChanges::default(),
            };
            return self
                .reconcile_update_runtime(
                    outcome,
                    command.tools.is_some(),
                    command.budget.is_some(),
                )
                .await;
        }

        if command.mcp_candidates.is_some() {
            self.refresh_executable_projections()
                .await
                .map_err(SessionUpdateError::Unavailable)?;
        }

        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            match self
                .update_session_once(session_id, &command, command_record.clone())
                .await
            {
                Err(SessionUpdateError::Conflict)
                    if command.if_match.is_none() && attempt + 1 < Self::ROOT_CAS_ATTEMPTS =>
                {
                    continue;
                }
                Ok(outcome) => {
                    return self
                        .reconcile_update_runtime(
                            outcome,
                            command.tools.is_some(),
                            command.budget.is_some(),
                        )
                        .await;
                }
                result => return result,
            }
        }
        Err(SessionUpdateError::Conflict)
    }

    async fn reconcile_update_runtime(
        &self,
        mut outcome: SessionUpdateOutcome,
        tools_in_request: bool,
        budget_in_request: bool,
    ) -> Result<SessionUpdateOutcome, SessionUpdateError> {
        if tools_in_request
            && (outcome.changes.tools || !outcome.command_applied)
            && let Err(source) = self
                .runtime()
                .replace_session_tools(&outcome.session.session_id, outcome.session.tools.clone())
                .await
        {
            return Err(SessionUpdateError::ProjectionAfterCommit {
                outcome: Box::new(outcome),
                source,
            });
        }
        if budget_in_request && outcome.session.budget.can_admit_model_request() {
            outcome = self.resume_budget_pauses(outcome).await?;
        }
        Ok(outcome)
    }

    async fn resume_budget_pauses(
        &self,
        mut outcome: SessionUpdateOutcome,
    ) -> Result<SessionUpdateOutcome, SessionUpdateError> {
        use awaken_session_contract::{
            SessionBudgetResumeDelivery, SessionBudgetResumeDisposition,
        };

        let session_id = outcome.session.session_id.clone();
        let tickets = self
            .runtime()
            .session_budget_resume_tickets(&session_id)
            .await
            .map_err(|source| SessionUpdateError::ProjectionAfterCommit {
                outcome: Box::new(outcome.clone()),
                source,
            })?;

        for pause in tickets {
            let ticket = pause.ticket;
            let operation_id = format!(
                "budget-resume:{}",
                stable_fingerprint(&(
                    "session-budget-resume-v2",
                    session_id.as_str(),
                    ticket.thread_id.0.as_str(),
                    ticket.run_id.0.as_str(),
                    ticket.correlation_id.as_str(),
                    pause.pause_generation,
                ))
            );
            let (_, session_activity_epoch) = self
                .transfer_committed_activity_for_operation(
                    &session_id,
                    &operation_id,
                    pause.prior_session_activity_epoch,
                )
                .await
                .map_err(|error| SessionUpdateError::ProjectionAfterCommit {
                    outcome: Box::new(outcome.clone()),
                    source: error.run_error(),
                })?;
            let disposition = match self
                .runtime()
                .resume_budget_reached(SessionBudgetResumeDelivery {
                    session_id: session_id.clone(),
                    thread_id: ticket.thread_id,
                    run_id: ticket.run_id,
                    correlation_id: ticket.correlation_id,
                    pause_generation: pause.pause_generation,
                    prior_session_activity_epoch: pause.prior_session_activity_epoch,
                    session_activity_epoch,
                })
                .await
            {
                Ok(disposition) => disposition,
                Err(source) if source.kind == awaken_session_contract::RunErrorKind::BadRequest => {
                    // The runtime definitively rejected this exact delivery
                    // before executing it. Reclaim the just-opened activity;
                    // dependency/internal failures remain active because their
                    // staging outcome is ambiguous and update replay repairs it.
                    self.settle_activity(&session_id, session_activity_epoch)
                        .await
                        .map_err(|error| SessionUpdateError::ProjectionAfterCommit {
                            outcome: Box::new(outcome.clone()),
                            source: error.run_error(),
                        })?;
                    return Err(SessionUpdateError::ProjectionAfterCommit {
                        outcome: Box::new(outcome),
                        source,
                    });
                }
                Err(source) => {
                    return Err(SessionUpdateError::ProjectionAfterCommit {
                        outcome: Box::new(outcome),
                        source,
                    });
                }
            };
            if disposition == SessionBudgetResumeDisposition::Stale {
                self.settle_activity(&session_id, session_activity_epoch)
                    .await
                    .map_err(|error| SessionUpdateError::ProjectionAfterCommit {
                        outcome: Box::new(outcome.clone()),
                        source: error.run_error(),
                    })?;
            }
        }
        outcome.session = self
            .session_repository()
            .get(&session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionUpdateError::mutation)?;
        Ok(outcome)
    }

    async fn update_session_once(
        &self,
        session_id: &str,
        command: &SessionUpdateCommand,
        command_record: Option<IdempotencyRecord>,
    ) -> Result<SessionUpdateOutcome, SessionUpdateError> {
        let owner_scope = self
            .owner(session_id)
            .await
            .map_err(SessionUpdateError::mutation)?;
        let mut session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionUpdateError::mutation)?;
        if session.execution != SessionExecutionState::Idle {
            return Err(SessionUpdateError::NotIdle);
        }
        if command
            .if_match
            .is_some_and(|expected| expected != session.revision)
        {
            return Err(SessionUpdateError::Conflict);
        }

        let initial_title = session.title.clone();
        let initial_metadata = session.metadata.clone();
        let initial_tools = session.tools.clone();
        let initial_budget = session.budget.clone();
        let command_receipt_key = command_record.as_ref().map(|record| record.key.clone());
        let mut mcp_changed = false;
        if let Some(candidates) = command.mcp_candidates.clone() {
            let baseline = session
                .frozen_baseline()
                .ok_or(SessionUpdateError::NotFrozen)?
                .clone();
            let drafts = self
                .normalize_mcp_drafts(
                    &owner_scope,
                    candidates,
                    &baseline.mcp_authoring.ordered_vault_ids,
                )
                .await
                .map_err(SessionUpdateError::Rejected)?;
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default();
            let lease_is_current = session.realization.as_ref().is_some_and(|lease| {
                lease.runtime_incarnation == self.runtime_incarnation()
                    && realization_lease_is_live_at(lease.expires_at_unix_ms, now_unix_ms)
            });
            let projection_requires_completion = session.mcp.attachments.iter().any(|attachment| {
                matches!(
                    attachment.state,
                    McpAttachmentState::Requested
                        | McpAttachmentState::Realizing
                        | McpAttachmentState::Draining
                ) || (attachment.state == McpAttachmentState::Active
                    && !attachment.publication_acknowledged)
            });
            let active_requires_new_owner = !lease_is_current
                && session
                    .mcp
                    .attachments
                    .iter()
                    .any(|attachment| attachment.state == McpAttachmentState::Active);
            if projection_requires_completion || active_requires_new_owner {
                session = self
                    .realize_session_after_refresh(session_id)
                    .await
                    .map_err(SessionUpdateError::realization)?;
                mcp_changed = true;
            }

            let plan = session
                .mcp
                .request_full_replacement(
                    drafts,
                    Some(baseline.environment.credential_realization.mcp_holder),
                )
                .map_err(|error| {
                    SessionUpdateError::Rejected(RunError::bad_request(error.to_string()))
                })?;
            if plan.changed {
                mcp_changed = true;
                self.commit_session_snapshot(
                    &owner_scope,
                    session,
                    "mcp-replacement-intent",
                    Vec::new(),
                )
                .await
                .map_err(SessionUpdateError::mutation)?;
                session = self
                    .realize_session_after_refresh(session_id)
                    .await
                    .map_err(SessionUpdateError::realization)?;
            }
        }

        if let Some(title) = &command.title {
            session.title = match title {
                SessionFieldUpdate::Clear => None,
                SessionFieldUpdate::Replace(title) => Some(title.clone()),
            };
        }
        if let Some(patch) = &command.metadata {
            match patch {
                SessionMetadataUpdate::Clear => session.metadata.clear(),
                SessionMetadataUpdate::Patch(patch) => {
                    for (key, value) in patch {
                        match value {
                            Some(value) => {
                                session.metadata.insert(key.clone(), value.clone());
                            }
                            None => {
                                session.metadata.remove(key);
                            }
                        }
                    }
                }
            }
        }
        if let Some(tools) = &command.tools {
            session.tools = tools.clone();
        }
        if let Some(requested) = &command.budget {
            use awaken_session_contract::SessionBudgetState;
            session.budget = match (session.budget, requested) {
                (
                    SessionBudgetState::Active {
                        consumed_numerator,
                        usage_cursor,
                        snapshot,
                        reach_transitions,
                        ..
                    },
                    SessionFieldUpdate::Replace(max_list_cost_minor),
                ) => {
                    let threshold = u128::from(*max_list_cost_minor)
                        * SessionBudgetState::MICROS_PER_MINOR_USD
                        * SessionBudgetState::COST_DENOMINATOR;
                    if threshold <= consumed_numerator {
                        return Err(SessionUpdateError::Rejected(RunError::bad_request(
                            "updated max_list_cost must be greater than consumed list cost",
                        )));
                    }
                    SessionBudgetState::Active {
                        max_list_cost_minor: *max_list_cost_minor,
                        consumed_numerator,
                        usage_cursor,
                        snapshot,
                        reach_transitions,
                    }
                }
                (
                    SessionBudgetState::Active {
                        consumed_numerator,
                        usage_cursor,
                        snapshot,
                        reach_transitions,
                        ..
                    },
                    SessionFieldUpdate::Clear,
                ) => SessionBudgetState::Removed {
                    consumed_numerator,
                    usage_cursor,
                    snapshot,
                    reach_transitions,
                },
                (SessionBudgetState::Absent, _) => {
                    return Err(SessionUpdateError::Rejected(RunError::bad_request(
                        "a budget cannot be added to a Session created without one",
                    )));
                }
                (removed @ SessionBudgetState::Removed { .. }, SessionFieldUpdate::Clear) => {
                    removed
                }
                (SessionBudgetState::Removed { .. }, SessionFieldUpdate::Replace(_)) => {
                    return Err(SessionUpdateError::Rejected(RunError::bad_request(
                        "a removed Session budget cannot be re-added",
                    )));
                }
            };
        }

        let changes = SessionUpdateChanges {
            title: session.title != initial_title,
            metadata: session.metadata != initial_metadata,
            tools: session.tools != initial_tools,
            mcp: mcp_changed,
            budget: session.budget != initial_budget,
        };
        let mut command_applied = true;
        if changes.title
            || changes.metadata
            || changes.tools
            || changes.budget
            || command_record.is_some()
        {
            session = match command_record {
                Some(record) => {
                    let (committed, applied) = self
                        .commit_session_snapshot_with_record(
                            &owner_scope,
                            session,
                            record,
                            Vec::new(),
                        )
                        .await
                        .map_err(SessionUpdateError::mutation)?;
                    command_applied = applied;
                    committed
                }
                None => self
                    .commit_session_snapshot(&owner_scope, session, "update", Vec::new())
                    .await
                    .map_err(SessionUpdateError::mutation)?,
            };
        }
        let command_revision = if command_applied {
            session.revision
        } else {
            let key = command_receipt_key.ok_or_else(|| {
                SessionUpdateError::Unavailable(
                    "replayed Session update has no idempotency key".into(),
                )
            })?;
            self.session_repository()
                .idempotency_receipt(session_id, &key)
                .await
                .map_err(repository_failure)
                .map_err(SessionUpdateError::mutation)?
                .ok_or_else(|| {
                    SessionUpdateError::Unavailable(
                        "replayed Session update has no idempotency receipt".into(),
                    )
                })?
                .committed_revision
        };
        Ok(SessionUpdateOutcome {
            session,
            command_revision,
            command_applied,
            changes,
        })
    }
}
