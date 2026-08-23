//! Canonical durable Session creation protocol.
//!
//! Public protocols lower their wire inputs into [`SessionCreationIntent`].
//! This module alone owns the subsequent insert, finalization, realization,
//! activation, lifecycle-fact commit, and WorkQueue projection ordering.

use awaken_session_contract::{
    CompiledSessionCreation, IdempotencyRecord, PersistedSession, RunError, SessionBudgetState,
    SessionCreationIntent, SessionMcpAttachmentSet, SessionMutationPayload,
    SessionToolConfiguration,
};

use super::{SessionApplication, SessionMutationError, SessionRealizationError};

/// Protocol-neutral command accepted by the one durable Session creation driver.
pub struct CreateSessionCommand {
    pub owner_scope: String,
    pub session_id: String,
    pub intent: SessionCreationIntent,
    pub title: Option<String>,
    pub metadata: std::collections::BTreeMap<String, String>,
    pub tools: SessionToolConfiguration,
    pub budget: SessionBudgetState,
    /// Optional complete create-time Event plan. It is compiled by the protocol
    /// boundary before this command and installed in the original Session root.
    pub initial_events: Option<awaken_session_contract::SessionInitialEventPlan>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionCreationError {
    #[error("Session identity already exists")]
    Conflict,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
    #[error(transparent)]
    Rejected(#[from] RunError),
    #[error("Session creation dependency is unavailable: {0}")]
    Unavailable(String),
}

impl SessionCreationError {
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
        let CreateSessionCommand {
            owner_scope,
            session_id,
            intent,
            title,
            metadata,
            tools,
            budget,
            initial_events,
        } = command;
        // Compile the complete intent before insert so invalid input can never
        // strand a durable Session waiting for a second authoring path.
        let compiled = intent
            .finalize()
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        let mut persisted = Self::compile_session_root(
            session_id.clone(),
            title,
            metadata,
            tools,
            budget,
            compiled,
        )?;
        if let Some(initial_events) = initial_events {
            persisted
                .install_initial_event_plan(initial_events)
                .map_err(|error| {
                    SessionCreationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
        }
        let payload = SessionMutationPayload::Replace(persisted.clone());
        let payload_hash = payload.stable_hash();
        persisted = self
            .create_session_root(
                &owner_scope,
                persisted,
                IdempotencyRecord {
                    key: format!("session:create:{session_id}:{payload_hash}"),
                    payload_hash,
                },
                Vec::new(),
            )
            .await
            .map_err(SessionCreationError::mutation)?;

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
