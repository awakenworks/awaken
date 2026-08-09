//! Canonical durable Session creation protocol.
//!
//! Public protocols lower their wire inputs into [`SessionCreationIntent`].
//! This module alone owns the subsequent insert, finalization, realization,
//! activation, lifecycle-fact commit, and WorkQueue projection ordering.

use awaken_session_contract::{
    ApplicationContributionState, ApplicationSessionContributionFailure, IdempotencyRecord,
    ManagedLifecycleFact, PersistedSession, RunError, SessionCreationIntent,
    SessionMutationPayload, SessionToolConfiguration,
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
    /// Facts are already neutral durable values; protocol adapters may choose
    /// which public lifecycle vocabulary, if any, they commit beside creation.
    pub lifecycle_facts: Vec<ManagedLifecycleFact>,
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

    fn contribution(error: ApplicationSessionContributionFailure) -> Self {
        match error {
            ApplicationSessionContributionFailure::Conflict => Self::Conflict,
            ApplicationSessionContributionFailure::Invalid(message) => {
                Self::Rejected(RunError::bad_request(message))
            }
            error => Self::Unavailable(error.to_string()),
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
            lifecycle_facts,
        } = command;
        let application_required =
            matches!(intent.application, ApplicationContributionState::Required);
        // Compile before insert so invalid no-application input cannot strand a
        // Preparing row. Required applications finalize only after contribution.
        let compiled = if application_required {
            None
        } else {
            Some(
                intent
                    .clone()
                    .finalize(Vec::new())
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
            )
        };
        let mut persisted =
            PersistedSession::preparing(session_id.clone(), intent, title, metadata, tools);
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

        if let Some(compiled) = compiled {
            persisted = self
                .commit_compiled_session_creation(&owner_scope, persisted, compiled)
                .await
                .map_err(SessionCreationError::contribution)?;
            let realized = if self.requires_external_realization(&persisted) {
                self.install_dispatch_projection(&owner_scope, &persisted)
                    .await
                    .map(|()| persisted.clone())
            } else {
                self.realize_session(&session_id).await
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
        }

        persisted = self
            .commit_session_snapshot(
                &owner_scope,
                persisted,
                if application_required {
                    "record-preparing-config"
                } else {
                    "activate"
                },
                lifecycle_facts,
            )
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
        Ok(persisted)
    }
}
