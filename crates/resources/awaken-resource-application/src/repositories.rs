// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: reusable Repository authoring over the single ResourceRegistry and
// @formal reason: RepositoryCredentialValidator. Dynamic path: validate the request key, HTTPS URL and Git
// @formal reason: inputs, admit any credential binding before registration, then delegate exact replay and
// @formal reason: durable config ownership to the registry; scoped list/get exclude Session-owned
// @formal reason: definitions. Adjacent decision-table tests cover rejection before writes, conflicting
// @formal reason: replay, retirement and Workspace isolation. URL parsing, real credential admission and
// @formal reason: database transaction refinement remain unproved by this product classification.
//! One authoring path over the existing registry, independent of Session lifetime.

use std::sync::Arc;

#[cfg(test)]
mod tests;

use async_trait::async_trait;
use awaken_resource_contract::{
    ConfigVersion, CreateRepositoryCommand, RegisterRepository, RepositoryApplicationService,
    RepositoryConfigVersion, RepositoryCredentialValidator, RepositoryDefinition,
    RepositoryDetails, ResourceRegistry, ResourceRegistryError, ResourceState, ResourceTimestamps,
    content_id, is_session_owned_repository,
};

pub struct RepositoryApplication {
    registry: Arc<dyn ResourceRegistry>,
    credentials: Arc<dyn RepositoryCredentialValidator>,
    /// Stamps a registration's creation instant on the registry's timeline.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl RepositoryApplication {
    pub fn new(
        registry: Arc<dyn ResourceRegistry>,
        credentials: Arc<dyn RepositoryCredentialValidator>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            registry,
            credentials,
            clock,
        }
    }

    async fn details(
        &self,
        definition: RepositoryDefinition,
    ) -> Result<RepositoryDetails, ResourceRegistryError> {
        let config = self
            .registry
            .find_repository_config(
                &definition.workspace_id,
                definition.id.as_str(),
                definition.current_config_version,
            )
            .await?
            .ok_or_else(|| {
                ResourceRegistryError::CorruptData("Repository config is missing".into())
            })?;
        Ok(RepositoryDetails { definition, config })
    }
}

fn invalid(message: &str) -> ResourceRegistryError {
    ResourceRegistryError::ConfigurationRejected {
        detail: message.into(),
    }
}

fn validate(command: &CreateRepositoryCommand, key: &str) -> Result<(), ResourceRegistryError> {
    if key.is_empty() || key.len() > 255 || !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(invalid(
            "A visible ASCII request key of 1–255 bytes is required",
        ));
    }
    if command.workspace_id.trim().is_empty() || command.name.trim().is_empty() {
        return Err(invalid("Workspace and repository name are required"));
    }
    let remote = url::Url::parse(&command.remote_url).map_err(|_| {
        invalid("Use an HTTPS repository URL without credentials, query or fragment")
    })?;
    if command.remote_url.chars().any(char::is_whitespace)
        || remote.scheme() != "https"
        || remote.host_str().is_none()
        || !remote.username().is_empty()
        || remote.password().is_some()
        || remote.query().is_some()
        || remote.fragment().is_some()
        || remote.path().trim_matches('/').is_empty()
    {
        return Err(invalid(
            "Use an HTTPS repository URL without credentials, query or fragment",
        ));
    }
    if command.initial_branch.is_some() && command.initial_commit.is_some() {
        return Err(invalid("Choose a branch or an exact commit, not both"));
    }
    if let Some(branch) = &command.initial_branch {
        // git-check-ref-format --branch rules, without spawning Git or allowing
        // shorthand (@{-1}) whose meaning depends on a local checkout.
        if branch.is_empty()
            || branch == "HEAD"
            || branch.starts_with('-')
            || branch.ends_with('.')
            || branch.contains("..")
            || branch.contains("@{")
            || branch
                .chars()
                .any(|c| c.is_control() || " ~^:?*[\\".contains(c))
            || branch
                .split('/')
                .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
        {
            return Err(invalid("Use a valid branch name"));
        }
    }
    if let Some(commit) = &command.initial_commit
        && (!matches!(commit.len(), 40 | 64) || !commit.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid("Use a full 40- or 64-character commit hash"));
    }
    if command.clone_policy.depth == Some(0) {
        return Err(invalid("Clone depth must be positive"));
    }
    if command
        .credential_binding
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.chars().any(char::is_whitespace))
    {
        return Err(invalid(
            "Choose an existing authorization or omit it for a public repository",
        ));
    }
    Ok(())
}

#[async_trait]
impl RepositoryApplicationService for RepositoryApplication {
    async fn create_idempotent(
        &self,
        command: CreateRepositoryCommand,
        request_key: &str,
    ) -> Result<RepositoryDetails, ResourceRegistryError> {
        validate(&command, request_key)?;
        if let Some(source) = &command.credential_binding {
            self.credentials
                .validate(&command.workspace_id, source, &command.remote_url)
                .await?;
        }
        let id = format!(
            "repository_request_{}",
            content_id(
                format!(
                    "repository-create:{}:{}{}",
                    command.workspace_id.len(),
                    command.workspace_id,
                    request_key,
                )
                .as_bytes()
            )
        );
        let definition = self
            .registry
            .register_repository_idempotent(RegisterRepository {
                definition: RepositoryDefinition {
                    id: id.clone().into(),
                    workspace_id: command.workspace_id,
                    name: command.name,
                    description: command.description,
                    metadata: Default::default(),
                    state: ResourceState::Active,
                    current_config_version: ConfigVersion::INITIAL,
                    timestamps: ResourceTimestamps::created(self.clock.now().as_unix_nanos_u64()),
                },
                initial_config: RepositoryConfigVersion {
                    repository_id: id.into(),
                    version: ConfigVersion::INITIAL,
                    remote_url: command.remote_url,
                    credential_binding: command.credential_binding,
                    initial_branch: command.initial_branch,
                    initial_commit: command.initial_commit,
                    clone_policy: command.clone_policy,
                },
            })
            .await?;
        self.details(definition).await
    }

    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<RepositoryDefinition>, ResourceRegistryError> {
        Ok(self
            .registry
            .list_repositories(workspace_id)
            .await?
            .into_iter()
            .filter(|definition| !is_session_owned_repository(definition))
            .collect())
    }

    async fn get(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDetails>, ResourceRegistryError> {
        match self
            .registry
            .find_repository(workspace_id, id)
            .await?
            .filter(|definition| {
                definition.state != ResourceState::Deleted
                    && !is_session_owned_repository(definition)
            }) {
            Some(definition) => self.details(definition).await.map(Some),
            None => Ok(None),
        }
    }
}
