//! Reusable Repository authoring. Credential material stays in the Vault;
//! Session creation continues to own exact credential revision/holder pinning.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{ClonePolicy, RepositoryConfigVersion, RepositoryDefinition, ResourceRegistryError};

pub const SESSION_REPOSITORY_OWNER_KIND: &str = "awaken.session_repository.owner_kind";
pub const SESSION_REPOSITORY_OWNER_SESSION: &str = "awaken.session_repository.session_id";

/// Fail closed for Session-owned and partially marked legacy records. Such
/// repositories and their credentials must never become reusable Agent inputs.
pub fn is_session_owned_repository(definition: &RepositoryDefinition) -> bool {
    definition.id.as_str().starts_with("managed:")
        || definition.id.as_str().starts_with("profiled:")
        || definition
            .metadata
            .contains_key(SESSION_REPOSITORY_OWNER_KIND)
        || definition
            .metadata
            .contains_key(SESSION_REPOSITORY_OWNER_SESSION)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRepositoryCommand {
    pub workspace_id: String,
    pub name: String,
    pub description: String,
    pub remote_url: String,
    pub credential_binding: Option<String>,
    pub initial_branch: Option<String>,
    pub initial_commit: Option<String>,
    pub clone_policy: ClonePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryDetails {
    pub definition: RepositoryDefinition,
    pub config: RepositoryConfigVersion,
}

/// Metadata-only admission check: active source in this Workspace, correct
/// transport audience/usage, unexpired descriptor. No secret resolution or
/// model-inference probe belongs in Repository authoring.
#[async_trait]
pub trait RepositoryCredentialValidator: Send + Sync {
    async fn validate(
        &self,
        workspace_id: &str,
        source_id: &str,
        remote_url: &str,
    ) -> Result<(), ResourceRegistryError>;
}

/// Explicit fail-closed adapter for isolated Resource fixtures without Control.
pub struct UnavailableRepositoryCredentialValidator;

#[async_trait]
impl RepositoryCredentialValidator for UnavailableRepositoryCredentialValidator {
    async fn validate(&self, _: &str, _: &str, _: &str) -> Result<(), ResourceRegistryError> {
        Err(ResourceRegistryError::Unavailable(
            "Repository authorization service is unavailable".into(),
        ))
    }
}

#[async_trait]
pub trait RepositoryApplicationService: Send + Sync {
    async fn create_idempotent(
        &self,
        command: CreateRepositoryCommand,
        request_key: &str,
    ) -> Result<RepositoryDetails, ResourceRegistryError>;

    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<RepositoryDefinition>, ResourceRegistryError>;

    async fn get(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDetails>, ResourceRegistryError>;
}
