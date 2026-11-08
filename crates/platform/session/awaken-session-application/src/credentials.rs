// @formal surface obligation=credential_reference.exact_revision_never_resolves_another_revision
// @formal surface obligation=worker.durable_dispatch_contains_no_plaintext_secret
// @formal reason: Freezes secret-free exact credential inputs for a Session.
//! Exact Repository credential pinning at the Session application boundary.

use crate::mutation::ROOT_CAS_ATTEMPTS;
use awaken_agent_contract::{
    credential::CredentialExecutionPolicy, credential::ModelExposurePolicy,
    credential::PlaintextHolder,
};
use awaken_credential_contract::{
    CredentialMaterialIngress, CredentialSourceId, SessionCredentialAccessRequest,
    SessionCredentialSource,
};
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::resource::{
    ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential, ResolvedSessionResources,
};
use awaken_session_contract::{PersistedSession, SessionAggregate};
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
    // Request-path state machines in this file return their future already
    // boxed. An unoptimized poll frame reserves a slot for every inline callee
    // it awaits and never reuses it; nested inline, one Managed request needed
    // more than a 2 MiB thread stack (see `tests/stack_boundaries.rs`).
    /// Advance each MCP credential pin of `session` to the exact successor an
    /// OAuth refresh published of it, on the working aggregate only; the
    /// caller commits it in its own root CAS. The revision is the Vault law's
    /// ([`SessionCredentialSource::refresh_successor`]): only the receipt
    /// whose predecessor is exactly the pin. Each advance goes through the
    /// credential-lifecycle replacement law (same source, monotonic, no
    /// topology change), one source at a time. Returns whether a pin moved.
    ///
    /// | Rule | credential source | pinned revision | refresh receipt | effect |
    /// |---|---|---|---|---|
    /// | P1 | absent | any | any | unchanged |
    /// | P2 | present | current | - | unchanged |
    /// | P3 | present | superseded | predecessor = pin | the attachment re-pinned to that successor |
    /// | P4 | present | superseded | none, or another predecessor | unchanged: staging refuses the stale pin |
    pub(crate) async fn adopt_mcp_refresh_successors(
        &self,
        owner_scope: &str,
        session: &mut SessionAggregate,
    ) -> Result<bool, RunError> {
        let Some(source) = self.credential_source.as_ref() else {
            return Ok(false);
        };
        let Some(baseline) = session.frozen_baseline() else {
            return Ok(false);
        };
        let holder = baseline
            .environment
            .credential_realization
            .mcp_holder
            .clone();
        let vault_ids = baseline.mcp_authoring.ordered_vault_ids.clone();
        let mut successors = std::collections::BTreeMap::new();
        for attachment in session.mcp.desired_attachments() {
            let Some(access) = attachment.credential.as_ref() else {
                continue;
            };
            if successors.contains_key(&access.credential.id) {
                continue;
            }
            let found = source
                .refresh_successor(owner_scope, &access.credential)
                .await;
            if let Some(revision) = found.map_err(RunError::unavailable)? {
                successors.insert(access.credential.id.clone(), revision);
            }
        }
        for (source_id, revision) in &successors {
            let candidates = session
                .mcp
                .desired_attachments()
                .into_iter()
                .map(|attachment| crate::McpAttachmentCandidate {
                    name: attachment.name.clone(),
                    target: crate::McpAttachmentCandidateTarget::Normalized(
                        attachment.target.clone(),
                    ),
                    prompts_as_skills: attachment.prompts_as_skills,
                    published_credential: attachment.credential.as_ref().map(|access| {
                        if &access.credential.id == source_id {
                            (source_id.clone(), *revision)
                        } else {
                            (access.credential.id.clone(), access.credential.revision)
                        }
                    }),
                    origin: attachment.origin,
                })
                .collect();
            let drafts = self
                .normalize_mcp_drafts(owner_scope, candidates, &vault_ids, &holder)
                .await?;
            crate::update::validate_credential_lifecycle_replacement(
                &session.mcp,
                source_id,
                false,
                &drafts,
            )?;
            session
                .mcp()
                .request_full_replacement(drafts, Some(holder.clone()))
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        Ok(!successors.is_empty())
    }

    pub fn ensure_repository_credentials_pinned<'a>(
        &'a self,
        owner_scope: &'a str,
        session: PersistedSession,
    ) -> futures_util::future::BoxFuture<'a, Result<PersistedSession, SessionPreparationError>>
    {
        Box::pin(async move {
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
        })
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
        SessionMutationError::Unavailable(message) | SessionMutationError::NotApplied(message) => {
            SessionPreparationError::Unavailable(message)
        }
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
