//! Canonical durable Session creation protocol.
//!
//! Public protocols lower their wire inputs into [`SessionCreationIntent`].
//! This module alone owns the subsequent insert, finalization, realization,
//! activation, lifecycle-fact commit, and WorkQueue projection ordering.

use awaken_session_contract::{
    CompiledSessionCreation, IdempotencyRecord, PersistedSession, RunError, SessionBudgetState,
    SessionCreateResult, SessionCreationIntent, SessionExecutionState, SessionMcpAttachmentSet,
    SessionMutationPayload, SessionToolConfiguration,
};

use super::{SessionApplication, SessionMutationError, SessionRealizationError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionCreationCompletion {
    AwaitRealization,
    AcceptDurableRoot,
}

/// Classify the repository-owned durable result at both create-replay entry
/// points. Awaiting callers retain the historical terminal conflict, while an
/// accepting caller receives the same failed asynchronous-operation root rather
/// than starting or projecting a replacement.
pub(super) fn validated_create_replay(
    session: PersistedSession,
    completion: SessionCreationCompletion,
) -> Result<PersistedSession, SessionCreationError> {
    if completion == SessionCreationCompletion::AwaitRealization
        && session.execution == SessionExecutionState::ActivationFailed
    {
        return Err(SessionCreationError::Tombstoned);
    }
    Ok(session)
}

/// Protocol-neutral command accepted by the one durable Session creation driver.
pub struct CreateSessionCommand {
    pub owner_scope: String,
    pub session_id: String,
    pub created_at_unix_ms: u64,
    pub intent: SessionCreationIntent,
    pub title: Option<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub tools: SessionToolConfiguration,
    pub budget: SessionBudgetState,
    /// Transient external participants configured before the root insert. The
    /// root adopts them on Applied/Replayed; they are never serialized here.
    pub repository_configurations: Vec<super::ConfiguredSessionRepository>,
    /// Optional caller-compiled request identity. Private product protocols use
    /// the same repository receipt as every later Session command instead of
    /// storing a replay fingerprint in mutable metadata. Ordinary callers keep
    /// the historical aggregate-payload identity by leaving this absent.
    pub idempotency: Option<IdempotencyRecord>,
    /// Optional complete create-time Event plan. It is compiled by the protocol
    /// boundary before this command and installed in the original Session root.
    pub initial_events: Option<awaken_session_contract::SessionInitialEventPlan>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionCreationError {
    #[error("Session identity already exists")]
    Conflict,
    #[error("Session identity is terminally occupied")]
    Tombstoned,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
    #[error(transparent)]
    Rejected(#[from] RunError),
    #[error("Session creation dependency is unavailable: {0}")]
    Unavailable(String),
    #[error("Session creation failed because durable state is invalid: {0}")]
    Internal(String),
}

impl SessionCreationError {
    pub(crate) fn repository(error: awaken_session_contract::SessionRepositoryError) -> Self {
        match error {
            awaken_session_contract::SessionRepositoryError::NotFound => {
                Self::Internal("newly created Session disappeared".into())
            }
            awaken_session_contract::SessionRepositoryError::Conflict(conflict) => match conflict {
                awaken_session_contract::SessionRepositoryConflict::IdempotencyMismatch => {
                    Self::IdempotencyMismatch
                }
                awaken_session_contract::SessionRepositoryConflict::Tombstoned => Self::Tombstoned,
                awaken_session_contract::SessionRepositoryConflict::AlreadyExists => Self::Conflict,
            },
            awaken_session_contract::SessionRepositoryError::Unavailable(message) => {
                Self::Unavailable(message)
            }
            awaken_session_contract::SessionRepositoryError::Corrupt(message)
            | awaken_session_contract::SessionRepositoryError::InvalidMutation(message) => {
                Self::Internal(message)
            }
        }
    }

    fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => {
                Self::Unavailable("newly created Session disappeared".into())
            }
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::IdempotencyMismatch,
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
        }
    }

    fn realization(error: SessionRealizationError) -> Self {
        match error {
            SessionRealizationError::Effect(error) => Self::Rejected(error),
            SessionRealizationError::Control(error) => Self::Unavailable(error.to_string()),
            SessionRealizationError::DidNotConverge => {
                Self::Unavailable("Session realization did not converge".into())
            }
        }
    }
}

impl SessionApplication {
    fn compile_session_root(
        session_id: String,
        created_at_unix_ms: u64,
        title: Option<String>,
        metadata: std::collections::BTreeMap<String, String>,
        tools: SessionToolConfiguration,
        budget: SessionBudgetState,
        compiled: CompiledSessionCreation,
    ) -> Result<PersistedSession, SessionCreationError> {
        let holder = compiled
            .baseline
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        let mut resources = awaken_session_contract::SessionResourceState::default();
        resources
            .prepare(&session_id, compiled.initial_resources)
            .map_err(|error| SessionCreationError::Unavailable(error.to_string()))?;
        let mcp = SessionMcpAttachmentSet::from_initial(compiled.initial_mcp, Some(holder))
            .map_err(|error| SessionCreationError::Unavailable(error.to_string()))?;
        Ok(PersistedSession::frozen_with_budget(
            session_id,
            created_at_unix_ms,
            compiled.baseline,
            resources,
            mcp,
            title,
            metadata,
            tools,
            budget,
        ))
    }

    /// Create one Session through the only durable creation protocol.
    pub async fn create_session(
        &self,
        command: CreateSessionCommand,
    ) -> Result<PersistedSession, SessionCreationError> {
        Box::pin(
            self.create_session_with_completion(
                command,
                SessionCreationCompletion::AwaitRealization,
            ),
        )
        .await
    }

    /// Accept one Session as soon as its complete root is durable. Physical
    /// realization, activation, and disposable Work projection are resumed by
    /// the existing lifecycle supervisor from that same aggregate.
    pub async fn accept_session(
        &self,
        command: CreateSessionCommand,
    ) -> Result<PersistedSession, SessionCreationError> {
        Box::pin(
            self.create_session_with_completion(
                command,
                SessionCreationCompletion::AcceptDurableRoot,
            ),
        )
        .await
    }

    pub(super) async fn create_session_with_completion(
        &self,
        command: CreateSessionCommand,
        completion: SessionCreationCompletion,
    ) -> Result<PersistedSession, SessionCreationError> {
        let CreateSessionCommand {
            owner_scope,
            session_id,
            created_at_unix_ms,
            intent,
            title,
            metadata,
            tools,
            budget,
            repository_configurations,
            idempotency,
            initial_events,
        } = command;
        // Compile the complete intent before insert so invalid input can never
        // strand a durable Session waiting for a second authoring path.
        let compiled = match intent.finalize() {
            Ok(compiled) => compiled,
            Err(error) => {
                let first =
                    SessionCreationError::Rejected(RunError::bad_request(error.to_string()));
                if !self
                    .abort_unadopted_session_repositories(&repository_configurations)
                    .await
                {
                    tracing::warn!(
                        session = %session_id,
                        "Session Repository compensation remains pending after compile rejection"
                    );
                }
                return Err(first);
            }
        };
        let layout = awaken_session_contract::SessionSandboxLayout {
            workspace_id: owner_scope.clone(),
            agent_id: compiled.baseline.agent_id.clone(),
            agent_revision: compiled.baseline.agent_revision,
            runtime_placement: compiled.baseline.runtime_placement,
            model_override: compiled.baseline.model_override.clone(),
            runtime: compiled.baseline.runtime.clone(),
            mounts: compiled.baseline.mounts.clone(),
            env: compiled.baseline.env.clone(),
            environment: compiled.baseline.environment.clone(),
            resources: compiled.initial_resources.sandbox_layout_bindings(),
        };
        if let Err(error) = self.validate_session_sandbox_layout(&session_id, &layout) {
            let first = SessionCreationError::Rejected(error);
            if !self
                .abort_unadopted_session_repositories(&repository_configurations)
                .await
            {
                tracing::warn!(
                    session = %session_id,
                    "Session Repository compensation remains pending after Sandbox-layout rejection"
                );
            }
            return Err(first);
        }
        let mut persisted = match Self::compile_session_root(
            session_id.clone(),
            created_at_unix_ms,
            title,
            metadata,
            tools,
            budget,
            compiled,
        ) {
            Ok(persisted) => persisted,
            Err(first) => {
                if !self
                    .abort_unadopted_session_repositories(&repository_configurations)
                    .await
                {
                    tracing::warn!(
                        session = %session_id,
                        "Session Repository compensation remains pending after root compilation"
                    );
                }
                return Err(first);
            }
        };
        if let Some(initial_events) = initial_events
            && let Err(error) = persisted.install_initial_event_plan(initial_events)
        {
            let first = SessionCreationError::Rejected(RunError::bad_request(error.to_string()));
            if !self
                .abort_unadopted_session_repositories(&repository_configurations)
                .await
            {
                tracing::warn!(
                    session = %session_id,
                    "Session Repository compensation remains pending after Event-plan rejection"
                );
            }
            return Err(first);
        }
        let payload = SessionMutationPayload::Replace(persisted.clone());
        let payload_hash = payload.stable_hash();
        let idempotency = idempotency.unwrap_or_else(|| IdempotencyRecord {
            key: format!("session:create:{session_id}:{payload_hash}"),
            payload_hash,
        });
        let create_result = match self
            .create_session_root(&owner_scope, persisted, idempotency.clone(), Vec::new())
            .await
        {
            Ok(result) => result,
            Err(first) => {
                match self
                    .replay_session_create(&owner_scope, &session_id, &idempotency)
                    .await
                {
                    Ok(Some(session)) => return validated_create_replay(session, completion),
                    Ok(None) => {
                        if !self
                            .abort_unadopted_session_repositories(&repository_configurations)
                            .await
                        {
                            tracing::warn!(
                                session = %session_id,
                                "Session Repository compensation remains pending after create failure"
                            );
                        }
                    }
                    Err(
                        SessionCreationError::Unavailable(_) | SessionCreationError::Internal(_),
                    ) => {
                        // An unavailable/corrupt replay query cannot prove the
                        // atomic root transaction absent. Preserve participants
                        // until exact durable truth can classify adoption.
                    }
                    Err(_) => {
                        // Conflict/tombstone/idempotency mismatch proves this
                        // exact create receipt was not adopted. The compensation
                        // helper still rereads the root and preserves any
                        // participant referenced by a concurrent winner.
                        if !self
                            .abort_unadopted_session_repositories(&repository_configurations)
                            .await
                        {
                            tracing::warn!(
                                session = %session_id,
                                "Session Repository compensation remains pending after deterministic create rejection"
                            );
                        }
                    }
                }
                return Err(first);
            }
        };
        persisted = match create_result {
            SessionCreateResult::Applied(session) => session,
            // The repository receipt and aggregate are the sole replay truth.
            // Returning here also prevents duplicate external installation,
            // activation, WorkQueue dispatch, or cleanup from a newly lowered
            // candidate that was never committed.
            SessionCreateResult::Replayed(session) => {
                return validated_create_replay(session, completion);
            }
        };

        if completion == SessionCreationCompletion::AcceptDurableRoot {
            // The root contains the frozen baseline, initial Event plan, and
            // idempotency receipt. Waking the sole recovery driver is only an
            // optimization: startup scanning recovers the same Preparing root
            // after process loss, without a Job row or a second lifecycle.
            self.wake_lifecycle_supervisor();
            return Ok(persisted);
        }

        let realized = if self.requires_external_realization(&persisted) {
            self.install_dispatch_projection(&owner_scope, &persisted)
                .await
                .map(|()| persisted.clone())
        } else {
            self.realize_session_after_refresh(&session_id).await
        };
        persisted = match realized {
            Ok(session) => session,
            Err(error) => {
                let _ = self
                    .release_terminal_resources(&owner_scope, &session_id)
                    .await;
                return Err(SessionCreationError::realization(error));
            }
        };

        persisted = self
            .commit_session_snapshot(&owner_scope, persisted, "activate", Vec::new())
            .await
            .map_err(SessionCreationError::mutation)?;

        // The aggregate is already the authoritative dispatch intent. An
        // ambiguous create failure here could induce a duplicate client create;
        // the canonical reconciler therefore retries this disposable projection.
        if let Err(error) = self.dispatch_session_work(&persisted).await {
            tracing::warn!(
                session = %session_id,
                error = ?error,
                "Session WorkQueue dispatch remains pending after create"
            );
        }
        if persisted.needs_event_reconciliation() {
            self.wake_lifecycle_supervisor();
        }
        Ok(persisted)
    }
}
