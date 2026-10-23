// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
//! Session-owned Repository participants: the Registry definitions and Vault
//! credentials one Session creates for its own Repository inputs.
//!
//! One use case: configure a participant before a Session root adopts it,
//! compensate one that no root adopted, and retire the ones a root detached or
//! terminally released. The component holds the CAS kernel and Resource commit
//! bracket, the credential authorities, the optional Resource Registry and
//! purge scheduler and the application clock. The Session aggregate decides
//! which generation is retired; the Registry and Vault stay the participant
//! authorities, so every effect is idempotent and replay-safe.

use std::sync::Arc;

use awaken_credential_contract::{
    CredentialMaterialIngressCommand, CredentialMaterialInput, MaterialEntry,
};
use awaken_resource_contract::{
    ResourcePurgeScheduler, ResourceRegistry, SESSION_REPOSITORY_OWNER_KIND,
    SESSION_REPOSITORY_OWNER_SESSION,
};
use awaken_session_configuration_contract::resource::ResolvedInputSource;
use awaken_session_state_contract::SessionAggregate;
use awaken_session_types::RunError;
use awaken_time::WallClockExt as _;

use crate::credentials::internal;
use crate::credentials::{mutation_failure, repository_preparation};
mod repository_reference;

use crate::{
    SessionCredentials, SessionMutationError, SessionMutations, SessionParticipantProvenance,
    SessionPreparationError, SessionResourceReferences,
};
use repository_reference::{
    session_resources_reference_repository, session_resources_reference_repository_generation,
};

/// Closed owner namespace for a Repository definition created exclusively for
/// one Session. Platform and Agent-default Repository definitions never carry
/// this marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionRepositoryOwner {
    Managed { session_id: String },
    Profiled { session_id: String },
}

impl SessionRepositoryOwner {
    #[must_use]
    pub fn from_repository_id(repository_id: &str) -> Option<Self> {
        let (owner, suffix) = repository_id.rsplit_once(":repository:")?;
        if suffix.is_empty() {
            return None;
        }
        if let Some(session_id) = owner.strip_prefix("managed:")
            && !session_id.is_empty()
        {
            return Some(Self::managed(session_id));
        }
        owner
            .strip_prefix("profiled:")
            .filter(|session_id| !session_id.is_empty())
            .map(Self::profiled)
    }

    #[must_use]
    pub fn managed(session_id: impl Into<String>) -> Self {
        Self::Managed {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn profiled(session_id: impl Into<String>) -> Self {
        Self::Profiled {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Managed { session_id } | Self::Profiled { session_id } => session_id,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Managed { .. } => "managed",
            Self::Profiled { .. } => "profiled",
        }
    }

    #[must_use]
    pub fn owns_repository_id(&self, repository_id: &str) -> bool {
        repository_id
            .strip_prefix(&format!(
                "{}:{}:repository:",
                self.kind(),
                self.session_id()
            ))
            .is_some_and(|suffix| !suffix.is_empty())
    }

    #[must_use]
    pub(crate) fn marker(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            (SESSION_REPOSITORY_OWNER_KIND.into(), self.kind().into()),
            (
                SESSION_REPOSITORY_OWNER_SESSION.into(),
                self.session_id().into(),
            ),
        ])
    }

    #[must_use]
    pub(crate) fn matches_definition(
        &self,
        definition: &awaken_resource_contract::RepositoryDefinition,
    ) -> bool {
        self.owns_repository_id(definition.id.as_str())
            && definition
                .metadata
                .get(SESSION_REPOSITORY_OWNER_KIND)
                .map(String::as_str)
                == Some(self.kind())
            && definition
                .metadata
                .get(SESSION_REPOSITORY_OWNER_SESSION)
                .map(String::as_str)
                == Some(self.session_id())
    }
}

/// Application command payload for a Repository attached as a Session input.
pub struct SessionRepositoryResourceInput {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub description: String,
    pub remote_url: String,
    pub credential_material: Option<CredentialMaterialInput>,
    pub credential: Option<awaken_credential_contract::CredentialRef>,
    pub mount_path: String,
    pub initial_branch: Option<String>,
    pub initial_commit: Option<String>,
}

/// Transient participant receipt carried only until a Session root adopts the
/// configured Repository. Registry and Vault provenance remain independent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredSessionRepository {
    pub owner: SessionRepositoryOwner,
    pub workspace_id: String,
    pub repository_id: awaken_resource_contract::RepositoryId,
    pub registry_provenance: SessionParticipantProvenance,
    pub credential: Option<MaterialEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnedRepositoryRetirement {
    Retired,
    NotOwned,
    Pending,
}

impl OwnedRepositoryRetirement {
    const fn is_complete(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One cause projection for the existing Repository retirement reconciler.
/// Ordinary manifest replacement cancels a retirement if the generation is
/// still referenced. Terminal preparation deliberately retains that frozen
/// Resource projection until physical disposal, so its already-persisted plan
/// must instead run to completion through the same participant authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepositoryRetirementCause {
    DetachedGeneration,
    TerminalPreparation,
}

pub struct SessionRepositoryParticipants {
    mutations: Arc<SessionMutations>,
    resource_references: Arc<SessionResourceReferences>,
    credentials: Arc<SessionCredentials>,
    registry: Option<Arc<dyn ResourceRegistry>>,
    purge: Option<Arc<dyn ResourcePurgeScheduler>>,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl SessionRepositoryParticipants {
    pub(crate) fn new(
        mutations: Arc<SessionMutations>,
        resource_references: Arc<SessionResourceReferences>,
        credentials: Arc<SessionCredentials>,
        registry: Option<Arc<dyn ResourceRegistry>>,
        purge: Option<Arc<dyn ResourcePurgeScheduler>>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            mutations,
            resource_references,
            credentials,
            registry,
            purge,
            clock,
        }
    }

    pub async fn configure(
        &self,
        input: SessionRepositoryResourceInput,
    ) -> Result<ConfiguredSessionRepository, RunError> {
        let SessionRepositoryResourceInput {
            id,
            workspace_id,
            name,
            description,
            remote_url,
            credential_material,
            credential,
            mount_path: _,
            initial_branch,
            initial_commit,
        } = input;
        let owner = SessionRepositoryOwner::from_repository_id(&id).ok_or_else(|| {
            RunError::bad_request(
                "Session-owned Repository identity does not match its Managed/Profiled owner",
            )
        })?;
        if !owner.owns_repository_id(&id) {
            return Err(RunError::bad_request(
                "Session-owned Repository identity does not match its Managed/Profiled owner",
            ));
        }
        if credential_material.is_some() && credential.is_some() {
            return Err(RunError::bad_request(
                "repository cannot carry both inline credential material and a credential reference",
            ));
        }
        let repository_id = awaken_resource_contract::RepositoryId::from(id);
        let material_ingress = if let Some(material) = credential_material {
            let credential_target =
                crate::session_resource::repository_transport_credential_target(&remote_url)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
            let ingress = self
                .credentials
                .credential_material_ingress()
                .ok_or_else(|| {
                    RunError::bad_request(
                        "repository authorization requires a configured credential Vault",
                    )
                })?;
            let source_id = awaken_credential_contract::CredentialSourceId(format!(
                "{}:credential",
                repository_id.as_str()
            ));
            Some((ingress, source_id, credential_target, material))
        } else {
            None
        };
        let credential_binding = if let Some((_, source_id, _, _)) = &material_ingress {
            Some(source_id.0.clone())
        } else {
            match credential {
                Some(credential) if !credential.id.trim().is_empty() && credential.revision > 0 => {
                    Some(credential.id)
                }
                Some(_) => {
                    return Err(RunError::bad_request(
                        "repository credential reference is invalid",
                    ));
                }
                None => None,
            }
        };
        let catalog = self.registry.as_ref().ok_or_else(|| {
            RunError::bad_request("repository resources require a configured Resource Registry")
        })?;
        let initial_state = if material_ingress.is_some() {
            awaken_resource_contract::ResourceState::Suspended
        } else {
            awaken_resource_contract::ResourceState::Active
        };
        let definition = awaken_resource_contract::RepositoryDefinition {
            id: repository_id.clone(),
            workspace_id,
            name,
            description,
            metadata: owner.marker(),
            state: initial_state,
            current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
            timestamps: Default::default(),
        };
        let initial_config = awaken_resource_contract::RepositoryConfigVersion {
            repository_id: repository_id.clone(),
            version: awaken_resource_contract::ConfigVersion::INITIAL,
            remote_url,
            credential_binding,
            initial_branch,
            initial_commit,
            clone_policy: awaken_resource_contract::ClonePolicy::default(),
        };
        let map_registry_error = |error| {
            RunError::bad_request(format!(
                "repository resource could not be configured: {error}"
            ))
        };
        // The aggregate is the canonical validation owner. Run that pure
        // admission before either durable participant so invalid Repository
        // configuration cannot create a Vault row or a Registry aggregate.
        awaken_resource_contract::RepositoryAggregate::register(
            definition.clone(),
            initial_config.clone(),
        )
        .map_err(&map_registry_error)?;
        let (registered_state, registry_provenance) = match catalog
            .register_repository(awaken_resource_contract::RegisterRepository {
                definition: definition.clone(),
                initial_config: initial_config.clone(),
            })
            .await
        {
            Ok(()) => (initial_state, SessionParticipantProvenance::Applied),
            Err(awaken_resource_contract::ResourceRegistryError::AlreadyRegistered(_)) => {
                let stored_definition = catalog
                    .find_repository(&definition.workspace_id, definition.id.as_str())
                    .await
                    .map_err(&map_registry_error)?;
                let stored_config = catalog
                    .find_repository_config(
                        &definition.workspace_id,
                        definition.id.as_str(),
                        awaken_resource_contract::ConfigVersion::INITIAL,
                    )
                    .await
                    .map_err(&map_registry_error)?;
                let Some(stored_definition) = stored_definition else {
                    return Err(RunError::bad_request(
                        "repository resource could not be configured: registered Repository is unavailable in this Workspace",
                    ));
                };
                let lifecycle_is_exact = if material_ingress.is_some() {
                    matches!(
                        stored_definition.state,
                        awaken_resource_contract::ResourceState::Suspended
                            | awaken_resource_contract::ResourceState::Active
                    )
                } else {
                    stored_definition.state == awaken_resource_contract::ResourceState::Active
                };
                // State and timestamps are lifecycle progress owned by this
                // Registry saga, not request identity. Normalize only those
                // fields, then retain whole-definition equality for replay.
                let mut stored_registration = stored_definition.clone();
                stored_registration.state = definition.state;
                stored_registration.timestamps = definition.timestamps;
                if !lifecycle_is_exact
                    || stored_registration != definition
                    || stored_config.as_ref() != Some(&initial_config)
                {
                    return Err(RunError::bad_request(
                        "repository resource could not be configured: existing Repository does not exactly match the requested definition and initial config",
                    ));
                }
                (
                    stored_definition.state,
                    SessionParticipantProvenance::Replayed,
                )
            }
            Err(error) => return Err(map_registry_error(error)),
        };
        let Some((ingress, source_id, credential_target, material)) = material_ingress else {
            return Ok(ConfiguredSessionRepository {
                owner,
                workspace_id: definition.workspace_id,
                repository_id,
                registry_provenance,
                credential: None,
            });
        };
        let credential_entry = match ingress
            .enter_material(CredentialMaterialIngressCommand {
                source_id: source_id.clone(),
                workspace_id: definition.workspace_id.clone(),
                target: credential_target,
                usage: crate::session_resource::repository_transport_credential_usage(),
                material,
            })
            .await
        {
            Ok(entry) => entry,
            Err(error) => {
                let first = RunError::bad_request(format!(
                    "repository authorization could not be sealed: {error}"
                ));
                if registry_provenance == SessionParticipantProvenance::Applied
                    && !self
                        .retire_owned(&definition.workspace_id, &owner, repository_id.as_str())
                        .await
                {
                    tracing::warn!(
                        repository = %repository_id,
                        "Session Repository compensation remains pending after credential ingress failure"
                    );
                }
                return Err(first);
            }
        };
        if credential_entry.credential.id != source_id.0
            || credential_entry.credential.revision == 0
        {
            let first = RunError::bad_request(
                "repository authorization returned another credential binding",
            );
            if registry_provenance == SessionParticipantProvenance::Applied
                && !self
                    .retire_owned(&definition.workspace_id, &owner, repository_id.as_str())
                    .await
            {
                tracing::warn!(
                    repository = %repository_id,
                    "Session Repository compensation remains pending after invalid credential ingress"
                );
            }
            return Err(first);
        }
        if registered_state == awaken_resource_contract::ResourceState::Active {
            return Ok(ConfiguredSessionRepository {
                owner,
                workspace_id: definition.workspace_id,
                repository_id,
                registry_provenance,
                credential: Some(credential_entry),
            });
        }
        if let Err(error) = catalog
            .change_repository_state(awaken_resource_contract::ChangeRepositoryState {
                workspace_id: definition.workspace_id.clone(),
                id: repository_id.clone(),
                state: awaken_resource_contract::ResourceState::Active,
            })
            .await
        {
            let first = map_registry_error(error);
            let configured = ConfiguredSessionRepository {
                owner,
                workspace_id: definition.workspace_id,
                repository_id,
                registry_provenance,
                credential: Some(credential_entry),
            };
            if !self
                .abort_unadopted(std::slice::from_ref(&configured))
                .await
            {
                tracing::warn!(
                    repository = %configured.repository_id,
                    "Session Repository compensation remains pending after activation failure"
                );
            }
            return Err(first);
        }
        Ok(ConfiguredSessionRepository {
            owner,
            workspace_id: definition.workspace_id,
            repository_id,
            registry_provenance,
            credential: Some(credential_entry),
        })
    }

    pub(crate) async fn reconcile_retirements(
        &self,
        owner_scope: &str,
        mut session: SessionAggregate,
        cause: RepositoryRetirementCause,
    ) -> Result<SessionAggregate, SessionPreparationError> {
        let terminal_plan = (cause == RepositoryRetirementCause::TerminalPreparation)
            .then(|| session.resources.repository_retirements().to_vec());
        let mut terminal_index = 0;
        loop {
            let retirement = terminal_plan
                .as_ref()
                .and_then(|plan| plan.get(terminal_index))
                .cloned()
                .or_else(|| {
                    (terminal_plan.is_none())
                        .then(|| session.resources.repository_retirements().first().cloned())
                        .flatten()
                });
            let Some(retirement) = retirement else {
                return Ok(session);
            };
            let ResolvedInputSource::Repository { repository_id, .. } = &retirement.source else {
                return Err(internal(
                    "Session Repository retirement intent contains another Resource kind",
                ));
            };
            if cause == RepositoryRetirementCause::DetachedGeneration
                && session_resources_reference_repository_generation(
                    &session.resources.active,
                    repository_id.as_str(),
                )
            {
                let mut candidate = session.clone();
                candidate
                    .resources()
                    .complete_repository_retirement(&retirement);
                session = self
                    .resource_references
                    .commit(
                        owner_scope,
                        candidate,
                        "repository-retirement-cancelled",
                        Vec::new(),
                    )
                    .await
                    .map_err(mutation_failure)?;
                continue;
            }
            if cause == RepositoryRetirementCause::DetachedGeneration
                && session.resources.pending.as_ref().is_some_and(|pending| {
                    session_resources_reference_repository_generation(
                        pending,
                        repository_id.as_str(),
                    )
                })
            {
                return Ok(session);
            }
            if !self
                .retire_input(owner_scope, session.session_id.as_str(), &retirement.source)
                .await
            {
                return Err(internal(
                    "Session-scoped Repository cleanup remains pending",
                ));
            }
            if terminal_plan.is_some() {
                // Keep the full frozen plan until the preparation receipt and
                // root Runtime receipt enter Disposing in one CAS. A crash or
                // response loss replays all participant effects through their
                // existing idempotency authorities; removing entries here
                // would erase the plan identity the receipt must bind.
                terminal_index += 1;
                continue;
            }
            let mut candidate = session.clone();
            if !candidate
                .resources()
                .complete_repository_retirement(&retirement)
            {
                return Err(internal(
                    "Session Repository retirement intent disappeared before completion",
                ));
            }
            match self
                .resource_references
                .commit(
                    owner_scope,
                    candidate,
                    "repository-retirement-complete",
                    Vec::new(),
                )
                .await
            {
                Ok(committed) => session = committed,
                Err(SessionMutationError::Conflict) => {
                    let latest = self
                        .mutations
                        .repository()
                        .get(&session.session_id)
                        .await
                        .map_err(repository_preparation)?;
                    if latest.is_terminal()
                        != (cause == RepositoryRetirementCause::TerminalPreparation)
                    {
                        // A lifecycle transition won the root CAS. Never carry
                        // an ordinary replacement cause into a newly terminal
                        // plan (or vice versa); the owning lifecycle driver
                        // must re-enter with the cause derived from that root.
                        return Err(SessionPreparationError::Conflict);
                    }
                    session = crate::mutation::aggregate_from_snapshot(latest)
                        .map_err(repository_preparation)?;
                }
                Err(error) => return Err(mutation_failure(error)),
            }
        }
    }

    /// Prepare every Session-owned Repository participant through the one
    /// ordinary retirement reconciler, while retaining the frozen Resource
    /// projection required by later physical-disposal authorization. The
    /// returned receipt is committed with the root Runtime preparation; it is
    /// never a second effect log or retirement cursor.
    pub(crate) async fn prepare_terminal(
        &self,
        owner_scope: &str,
        mut session: SessionAggregate,
    ) -> Result<
        (
            SessionAggregate,
            awaken_session_state_contract::SessionCleanupRepositoryPreparation,
        ),
        SessionPreparationError,
    > {
        if session.resources().ensure_terminal_repository_retirements() {
            session = self
                .resource_references
                .commit(
                    owner_scope,
                    session,
                    "terminal-repository-retirement-plan",
                    Vec::new(),
                )
                .await
                .map_err(mutation_failure)?;
        }
        session = self
            .reconcile_retirements(
                owner_scope,
                session,
                RepositoryRetirementCause::TerminalPreparation,
            )
            .await?;
        // Terminal mode intentionally retains the complete canonical plan in
        // the Session root. The receipt below binds that plan; participant
        // effects are replay-safe through their existing Vault/Registry
        // authorities until the same root accepts physical disposal.
        let receipt = awaken_session_state_contract::SessionCleanupRepositoryPreparation::new(
            &session.session_id,
            owner_scope,
            &session.resources,
        )
        .map_err(internal)?;
        Ok((session, receipt))
    }

    /// Compensate only participants created by a command that failed before a
    /// durable Session root adopted them. Exact replays belong to an earlier
    /// command and are never inferred to be disposable.
    pub async fn abort_unadopted(&self, configured: &[ConfiguredSessionRepository]) -> bool {
        let mut retired = true;
        for configured in configured.iter().rev() {
            let adopted = match self
                .mutations
                .repository()
                .get(&awaken_session_types::SessionId::from(
                    configured.owner.session_id(),
                ))
                .await
            {
                Ok(session) => session_resources_reference_repository(
                    &session.resources,
                    configured.repository_id.as_str(),
                ),
                Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => false,
                Err(_) => {
                    // An unavailable/corrupt root read cannot prove that a
                    // concurrent exact command did not adopt this participant.
                    retired = false;
                    continue;
                }
            };
            if adopted {
                continue;
            }
            if configured.registry_provenance == SessionParticipantProvenance::Applied {
                retired &= self
                    .retire_owned(
                        &configured.workspace_id,
                        &configured.owner,
                        configured.repository_id.as_str(),
                    )
                    .await;
            }
            if let Some(credential) = &configured.credential
                && credential.created
            {
                retired &= self
                    .credentials
                    .retire_owned_repository_credential(
                        &configured.workspace_id,
                        &credential.credential,
                    )
                    .await;
            }
        }
        retired
    }

    /// Retire one detached Repository input only when both its canonical
    /// Session namespace and durable owner marker agree. Platform/shared
    /// Repository inputs are intentionally a no-op.
    async fn retire_input(
        &self,
        owner_scope: &str,
        session_id: &str,
        source: &ResolvedInputSource,
    ) -> bool {
        let ResolvedInputSource::Repository {
            repository_id,
            config,
            credential,
        } = source
        else {
            return true;
        };
        let Some(owner) = SessionRepositoryOwner::from_repository_id(repository_id.as_str())
            .filter(|owner| owner.session_id() == session_id)
        else {
            return true;
        };
        let owned_credential_binding = format!("{}:credential", repository_id.as_str());
        let owned_credential = credential
            .as_deref()
            .filter(|_| config.credential_binding.as_deref() == Some(&owned_credential_binding))
            .map(|credential| &credential.access.credential);
        self.retire_participants(
            owner_scope,
            &owner,
            repository_id.as_str(),
            owned_credential,
        )
        .await
        .is_complete()
    }

    pub(crate) async fn retire_owned(
        &self,
        owner_scope: &str,
        owner: &SessionRepositoryOwner,
        repository_id: &str,
    ) -> bool {
        self.retire_participants(owner_scope, owner, repository_id, None)
            .await
            .is_complete()
    }

    async fn retire_participants(
        &self,
        owner_scope: &str,
        owner: &SessionRepositoryOwner,
        repository_id: &str,
        credential: Option<&awaken_credential_contract::CredentialRef>,
    ) -> OwnedRepositoryRetirement {
        let Some(catalog) = self.registry.as_deref() else {
            return OwnedRepositoryRetirement::Pending;
        };
        let definition = match catalog.find_repository(owner_scope, repository_id).await {
            Ok(Some(definition)) => definition,
            // The prior idempotent attempt may already have retired and purged
            // the Registry participant before its Session-root completion CAS.
            // Absence therefore completes without inferring credential authority.
            Ok(None) => return OwnedRepositoryRetirement::NotOwned,
            Err(_) => return OwnedRepositoryRetirement::Pending,
        };
        if !owner.matches_definition(&definition) {
            // Namespace resemblance alone never grants delete authority. This
            // is a successful no-op for platform/shared definitions so one
            // terminal Session cannot be held open by a resource it does not
            // own.
            return OwnedRepositoryRetirement::NotOwned;
        }
        if let Some(credential) = credential
            && !self
                .credentials
                .retire_owned_repository_credential(owner_scope, credential)
                .await
        {
            return OwnedRepositoryRetirement::Pending;
        }
        let now = self.clock.now_ms();
        if let Some(scheduler) = self.purge.as_deref()
            && scheduler
                .schedule_purge(
                    awaken_resource_contract::ResourceTarget::new(
                        owner_scope,
                        awaken_resource_contract::ResourceKind::Repository,
                        repository_id,
                    ),
                    Some(definition.current_config_version.0),
                    now,
                    now,
                )
                .await
                .is_err()
        {
            return OwnedRepositoryRetirement::Pending;
        }
        if catalog
            .change_repository_state(awaken_resource_contract::ChangeRepositoryState {
                workspace_id: owner_scope.into(),
                id: repository_id.into(),
                state: awaken_resource_contract::ResourceState::Deleted,
            })
            .await
            .is_ok()
        {
            OwnedRepositoryRetirement::Retired
        } else {
            OwnedRepositoryRetirement::Pending
        }
    }
}
