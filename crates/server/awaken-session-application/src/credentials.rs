//! Exact Repository credential pinning at the Session application boundary.

use awaken_credential_contract::{
    CredentialExecutionPolicy, CredentialSourceId, ModelExposurePolicy, PlaintextHolder,
};
use awaken_session_contract::{
    PersistedSession, ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential,
    ResolvedSessionResources, RunError,
};

use super::{
    SessionApplication, SessionMutationError, mutation::repository_failure,
    resource_reconciliation::mutation_failure,
};

/// Failure from compiling or migrating exact Session credential pins.
#[derive(Debug, thiserror::Error)]
pub enum SessionPreparationError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session preparation was rejected: {0}")]
    Rejected(#[source] RunError),
    #[error("Session preparation is unavailable: {0}")]
    Unavailable(String),
}

impl SessionApplication {
    pub fn resource_plaintext_holder(
        &self,
        session: &PersistedSession,
    ) -> Result<PlaintextHolder, SessionPreparationError> {
        session
            .frozen_baseline()
            .map(|baseline| {
                baseline
                    .environment
                    .credential_realization
                    .resource_holder
                    .clone()
            })
            .ok_or_else(|| {
                SessionPreparationError::Rejected(RunError::bad_request(
                    "Session resource mutation requires a frozen Environment baseline",
                ))
            })
    }

    /// Compile or verify the exact Repository credential execution pin before
    /// an input enters the durable Session aggregate.
    pub async fn pin_repository_credential(
        &self,
        owner_scope: &str,
        selected_holder: &PlaintextHolder,
        input: &mut ResolvedInput,
    ) -> Result<(), SessionPreparationError> {
        let ResolvedInputSource::Repository {
            repository_id,
            config,
            credential,
            ..
        } = &mut input.source
        else {
            return Ok(());
        };
        let Some(binding) = config.credential_binding.as_deref() else {
            if credential.is_some() {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "Repository without a Vault binding carries a credential pin",
                )));
            }
            return Ok(());
        };
        if let Some(existing) = credential {
            existing.validate_for_binding(binding).map_err(|error| {
                SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
            })?;
            if &existing.selected_plaintext_holder != selected_holder {
                return Err(SessionPreparationError::Rejected(RunError::bad_request(
                    "Repository credential pin selects another Environment holder",
                )));
            }
            return Ok(());
        }
        let credentials = self.credential_source().ok_or_else(|| {
            SessionPreparationError::Rejected(RunError::bad_request(
                "Repository credential requires a configured credential vault",
            ))
        })?;
        let usage = awaken_session_contract::repository_transport_credential_usage();
        let material_binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
            owner_scope,
            &(repository_id, config.version),
            &usage,
        );
        let access = credentials
            .credential_access_for_source(
                &CredentialSourceId(binding.to_string()),
                owner_scope,
                usage,
                CredentialExecutionPolicy::exact(
                    selected_holder.clone(),
                    ModelExposurePolicy::Forbidden,
                ),
                selected_holder,
                &material_binding,
            )
            .await
            .map_err(|error| {
                SessionPreparationError::Rejected(RunError::bad_request(format!(
                    "Repository credential could not be pinned exactly: {error}"
                )))
            })?;
        *credential = Some(Box::new(ResolvedRepositoryCredential {
            access,
            selected_plaintext_holder: selected_holder.clone(),
        }));
        Ok(())
    }

    /// Apply the canonical per-input compiler to a complete Resource generation.
    pub async fn pin_repository_credentials(
        &self,
        owner_scope: &str,
        selected_holder: &PlaintextHolder,
        resources: &mut ResolvedSessionResources,
    ) -> Result<bool, SessionPreparationError> {
        let before = resources.clone();
        for input in &mut resources.inputs {
            self.pin_repository_credential(owner_scope, selected_holder, input)
                .await?;
        }
        Ok(*resources != before)
    }

    /// Root-CAS migration for retained Session rows written before exact
    /// Repository credential pins existed.
    pub async fn ensure_repository_credentials_pinned(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let holder = self.resource_plaintext_holder(&session)?;
            let mut changed = self
                .pin_repository_credentials(owner_scope, &holder, &mut session.resources.active)
                .await?;
            if let Some(pending) = &mut session.resources.pending {
                changed |= self
                    .pin_repository_credentials(owner_scope, &holder, pending)
                    .await?;
            }
            if !changed {
                return Ok(session);
            }
            let session_id = session.session_id.clone();
            match self
                .commit_session_snapshot(
                    owner_scope,
                    session,
                    "repository-credential-pin-migration",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => return Ok(session),
                Err(SessionMutationError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => {
                    session = self
                        .session_repository()
                        .get(&session_id)
                        .await
                        .map_err(repository_failure)
                        .map_err(mutation_failure)?;
                }
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionPreparationError::Conflict);
                }
                Err(SessionMutationError::NotFound) => {
                    return Err(SessionPreparationError::NotFound);
                }
                Err(SessionMutationError::IdempotencyMismatch) => {
                    return Err(SessionPreparationError::Unavailable(
                        "credential pin idempotency mismatch".into(),
                    ));
                }
                Err(SessionMutationError::Unavailable(message)) => {
                    return Err(SessionPreparationError::Unavailable(message));
                }
            }
        }
        Err(SessionPreparationError::Conflict)
    }
}
