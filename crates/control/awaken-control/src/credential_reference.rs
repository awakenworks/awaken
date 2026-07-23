//! Credential aggregate adapter for Agent publication.
//!
//! The config domain supplies the trusted execution Workspace and an exact
//! secret-free revision. This adapter reads the existing credential aggregate
//! and enforces ownership, lifecycle, and revision without materializing secret
//! bytes.

use std::sync::Arc;

use awaken_config_service::CredentialReferenceValidator;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{CredentialSourceId, CredentialStatus};
use awaken_runtime_contract::credential::CredentialRef;
use awaken_tenancy::ScopeId;

pub struct CredentialRevisionValidator {
    credentials: Arc<dyn CredentialRepo>,
}

impl CredentialRevisionValidator {
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialRepo>) -> Self {
        Self { credentials }
    }
}

#[async_trait::async_trait]
impl CredentialReferenceValidator for CredentialRevisionValidator {
    async fn validate(
        &self,
        workspace: &ScopeId,
        credential: &CredentialRef,
    ) -> Result<(), String> {
        let source = self
            .credentials
            .get(&CredentialSourceId(credential.id.clone()))
            .await
            .map_err(|_| "MCP credential is unavailable in this Workspace".to_string())?;
        let revision = u64::try_from(source.version).ok();
        if source.workspace_id != workspace.as_str()
            || source.status != CredentialStatus::Active
            || revision != Some(credential.revision)
        {
            return Err(
                "MCP credential is unavailable in this Workspace at the requested revision"
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};

    #[tokio::test]
    async fn validator_accepts_only_the_exact_active_workspace_revision() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = InMemorySecretStore::new();
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("secret")),
                oauth_command: None,
            },
            &secrets,
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let validator = CredentialRevisionValidator::new(credentials);
        let exact = CredentialRef {
            id: source.id.0,
            revision: 1,
        };

        assert!(
            validator
                .validate(&ScopeId::from("workspace-a"), &exact)
                .await
                .is_ok()
        );
        assert!(
            validator
                .validate(&ScopeId::from("workspace-b"), &exact)
                .await
                .is_err()
        );
        assert!(
            validator
                .validate(
                    &ScopeId::from("workspace-a"),
                    &CredentialRef {
                        revision: 2,
                        ..exact
                    },
                )
                .await
                .is_err()
        );
    }
}
