// @formal surface obligation=credential_reference.exact_revision_never_resolves_another_revision
// @formal surface obligation=worker.durable_dispatch_contains_no_plaintext_secret
// @formal reason: Freezes secret-free exact credential inputs for a Session.
//! Exact Repository credential pinning at the Session application boundary.

use crate::mutation::ROOT_CAS_ATTEMPTS;
use awaken_credential_contract::{
    CredentialExecutionPolicy, CredentialMaterialIngress, CredentialSourceId, ModelExposurePolicy,
    PlaintextHolder, SessionCredentialAccessRequest, SessionCredentialSource,
};
use awaken_session_configuration_contract::resource::{
    ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential, ResolvedSessionResources,
};
use awaken_session_contract::{PersistedSession, SessionAggregate};
use awaken_session_types::RunError;
use std::sync::Arc;

use super::{SessionMutationError, SessionMutations, mutation::repository_failure};

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

/// Exact credential preparation and Session Repository binding commands.
///
/// The Builder supplies the same mandatory root-CAS and Clock handles as the
/// other Session use cases. Source and ingress retain their original optional
/// feature configuration; this owner introduces no durable credential state.
pub struct SessionCredentials {
    mutations: Arc<SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    credential_source: Option<Arc<dyn SessionCredentialSource>>,
    credential_material_ingress: Option<Arc<dyn CredentialMaterialIngress>>,
}

impl SessionCredentials {
    pub(crate) fn new(
        mutations: Arc<SessionMutations>,
        clock: Arc<dyn awaken_time::WallClock>,
        credential_source: Option<Arc<dyn SessionCredentialSource>>,
        credential_material_ingress: Option<Arc<dyn CredentialMaterialIngress>>,
    ) -> Self {
        Self {
            mutations,
            clock,
            credential_source,
            credential_material_ingress,
        }
    }

    pub(crate) fn mutations(&self) -> &SessionMutations {
        &self.mutations
    }

    pub(crate) fn clock(&self) -> &dyn awaken_time::WallClock {
        self.clock.as_ref()
    }

    #[must_use]
    pub(crate) fn credential_source(&self) -> Option<&dyn SessionCredentialSource> {
        self.credential_source.as_deref()
    }

    #[must_use]
    pub(crate) fn credential_material_ingress(&self) -> Option<&dyn CredentialMaterialIngress> {
        self.credential_material_ingress.as_deref()
    }
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
            existing
                .validate_for_repository(binding, &config.remote_url)
                .map_err(|error| {
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
        let usage = crate::session_resource::repository_transport_credential_usage();
        let target =
            crate::session_resource::repository_transport_credential_target(&config.remote_url)
                .map_err(|error| {
                    SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
        let material_binding = crate::session_resource::repository_transport_material_binding(
            owner_scope,
            repository_id,
            config.version,
        );
        let access = credentials
            .credential_access_for_source(
                &CredentialSourceId(binding.to_string()),
                owner_scope,
                SessionCredentialAccessRequest {
                    target,
                    usage,
                    policy: CredentialExecutionPolicy::exact(
                        selected_holder.clone(),
                        ModelExposurePolicy::Forbidden,
                    ),
                    selected_holder: selected_holder.clone(),
                    binding: material_binding,
                },
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
        let (mut inputs, skills) = resources.clone().into_parts();
        for input in &mut inputs {
            self.pin_repository_credential(owner_scope, selected_holder, input)
                .await?;
        }
        let next = ResolvedSessionResources::try_new(inputs, skills).map_err(|error| {
            SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
        })?;
        let changed = next != *resources;
        *resources = next;
        Ok(changed)
    }

    /// Root-CAS migration for retained Session rows written before exact
    /// Repository credential pins existed.
    pub async fn ensure_repository_credentials_pinned(
        &self,
        owner_scope: &str,
        session: PersistedSession,
    ) -> Result<PersistedSession, SessionPreparationError> {
        let mutations = &self.mutations;
        let mut session = crate::mutation::aggregate_from_snapshot(session)
            .map_err(repository_failure)
            .map_err(mutation_failure)?;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let holder = self.resource_plaintext_holder(&session)?;
            let mut active = session.resources.active.clone();
            let mut pending = session.resources.pending.clone();
            self.pin_repository_credentials(owner_scope, &holder, &mut active)
                .await?;
            if let Some(pending) = &mut pending {
                self.pin_repository_credentials(owner_scope, &holder, pending)
                    .await?;
            }
            let changed = session
                .resources()
                .migrate_repository_credential_pins(active, pending)
                .map_err(|error| {
                    SessionPreparationError::Rejected(RunError::bad_request(error.to_string()))
                })?;
            if !changed {
                return Ok(session.into_snapshot());
            }
            let session_id = session.session_id.clone();
            match mutations
                .commit_session_snapshot_with_outcome(
                    owner_scope,
                    session,
                    "repository-credential-pin-migration",
                    Vec::new(),
                )
                .await
            {
                Ok(commit) => return Ok(commit.into_session()),
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    session = mutations
                        .session_aggregate(&session_id)
                        .await
                        .map_err(repository_failure)
                        .map_err(mutation_failure)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
        Err(SessionPreparationError::Conflict)
    }
}

pub(crate) fn mutation_failure(error: SessionMutationError) -> SessionPreparationError {
    match error {
        SessionMutationError::NotFound => SessionPreparationError::NotFound,
        SessionMutationError::Conflict => SessionPreparationError::Conflict,
        SessionMutationError::IdempotencyMismatch => SessionPreparationError::Rejected(
            SessionMutationError::Internal(
                "Session idempotency key was reused with another payload".into(),
            )
            .into(),
        ),
        SessionMutationError::Unavailable(message) => SessionPreparationError::Unavailable(message),
        error @ (SessionMutationError::Internal(_) | SessionMutationError::Invalid(_)) => {
            SessionPreparationError::Rejected(error.into())
        }
    }
}

pub(crate) fn internal(error: impl std::fmt::Display) -> SessionPreparationError {
    SessionPreparationError::Rejected(RunError::internal(error.to_string()))
}

pub(crate) fn repository_preparation(
    error: awaken_session_contract::SessionRepositoryError,
) -> SessionPreparationError {
    mutation_failure(repository_failure(error))
}

pub(crate) fn preflight_repository_credential_rotation(
    root: &SessionAggregate,
    input: ResolvedInput,
) -> Result<(), SessionPreparationError> {
    root.clone()
        .resources()
        .replace_desired_repository_credential(input)
        .map_err(|_| SessionPreparationError::Conflict)
}

#[cfg(test)]
pub(crate) mod tests {
    /// O1: adding a command holder must preserve the one Mutations/Clock
    /// allocation. O2: a Builder without credential authorities remains valid;
    /// it does not acquire a source or ingress by extracting this service.
    /// The original construction test exercises create/replay and shared sink,
    /// repository, counter and Notify identities after these private checks.
    pub(crate) fn assert_composed_identity(app: &crate::SessionApplication) {
        let credentials = app.credentials();
        assert!(std::sync::Arc::ptr_eq(&app.credentials, &credentials));
        assert!(std::sync::Arc::ptr_eq(
            &app.mutations,
            &credentials.mutations
        ));
        assert!(std::sync::Arc::ptr_eq(&app.clock, &credentials.clock));
        let role: std::sync::Arc<dyn crate::SessionCredentialCommands> = credentials.clone();
        assert_eq!(
            std::sync::Arc::as_ptr(&role) as *const (),
            std::sync::Arc::as_ptr(&credentials) as *const (),
            "O1 the Managed credential role is the original component allocation"
        );
        assert!(credentials.credential_source.is_none());
        assert!(credentials.credential_material_ingress.is_none());
    }
}
