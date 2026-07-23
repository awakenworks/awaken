//! Publication policy for exact credential references in typed Agent bindings.
//!
//! This module owns only the orchestration rule: every authored reference is
//! validated once against the trusted execution Workspace before publication.
//! Credential storage and lifecycle remain behind the injected port.

use std::sync::Arc;

use awaken_config_store::AgentConfig;
use awaken_runtime_contract::credential::CredentialRef;
use awaken_tenancy::ScopeId;

/// Publication-time integrity check for one exact, secret-free revision.
#[async_trait::async_trait]
pub trait CredentialReferenceValidator: Send + Sync {
    async fn validate(&self, workspace: &ScopeId, credential: &CredentialRef)
    -> Result<(), String>;
}

pub(crate) async fn validate_credential_references(
    validator: Option<&Arc<dyn CredentialReferenceValidator>>,
    workspace: &ScopeId,
    config: &AgentConfig,
) -> Result<(), String> {
    for credential in config
        .mcp_servers
        .iter()
        .filter_map(|server| server.credential.as_ref())
    {
        validator
            .ok_or_else(|| "MCP credential references require a publication validator".to_string())?
            .validate(workspace, credential)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;

    struct ExactValidator;

    #[async_trait::async_trait]
    impl CredentialReferenceValidator for ExactValidator {
        async fn validate(
            &self,
            workspace: &ScopeId,
            credential: &CredentialRef,
        ) -> Result<(), String> {
            (workspace.as_str() == "workspace-a"
                && credential.id == "cred:workspace-a:docs"
                && credential.revision == 3)
                .then_some(())
                .ok_or_else(|| "credential mismatch".to_string())
        }
    }

    fn config() -> AgentConfig {
        AgentConfig {
            mcp_servers: vec![AgentMcpServerBinding {
                name: "docs".into(),
                url: "https://mcp.example.test".into(),
                credential: Some(CredentialRef {
                    id: "cred:workspace-a:docs".into(),
                    revision: 3,
                }),
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn references_fail_closed_without_a_validator_and_accept_exact_match() {
        let workspace = ScopeId::from("workspace-a");
        assert!(
            validate_credential_references(None, &workspace, &config())
                .await
                .unwrap_err()
                .contains("publication validator")
        );

        let validator: Arc<dyn CredentialReferenceValidator> = Arc::new(ExactValidator);
        assert!(
            validate_credential_references(Some(&validator), &workspace, &config())
                .await
                .is_ok()
        );
        assert!(
            validate_credential_references(
                Some(&validator),
                &ScopeId::from("workspace-b"),
                &config(),
            )
            .await
            .is_err()
        );
    }
}
