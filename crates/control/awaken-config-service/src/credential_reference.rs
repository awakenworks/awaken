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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CredentialReferenceIssue {
    pub(crate) path: String,
    pub(crate) message: String,
}

fn plugin_credential_references(
    value: &serde_json::Value,
    path: &str,
    references: &mut Vec<(String, CredentialRef)>,
) -> Result<(), CredentialReferenceIssue> {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                let field_path = format!("{path}.{name}");
                if name == "credential" && !value.is_null() {
                    let credential = serde_json::from_value(value.clone()).map_err(|error| {
                        CredentialReferenceIssue {
                            path: field_path.clone(),
                            message: format!(
                                "credential must be an exact id/revision reference: {error}"
                            ),
                        }
                    })?;
                    references.push((field_path, credential));
                } else {
                    plugin_credential_references(value, &field_path, references)?;
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                plugin_credential_references(item, &format!("{path}.{index}"), references)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) async fn validate_credential_references(
    validator: Option<&Arc<dyn CredentialReferenceValidator>>,
    workspace: &ScopeId,
    config: &AgentConfig,
) -> Result<(), CredentialReferenceIssue> {
    for credential in config
        .mcp_servers
        .iter()
        .filter_map(|server| server.credential.as_ref())
    {
        validator
            .ok_or_else(|| CredentialReferenceIssue {
                path: "mcp_servers".into(),
                message: "MCP credential references require a publication validator".into(),
            })?
            .validate(workspace, credential)
            .await
            .map_err(|message| CredentialReferenceIssue {
                path: "mcp_servers".into(),
                message,
            })?;
    }
    // Open plugin extensions use one structural convention: every field named
    // `credential` is an exact `CredentialRef`. Traversal, rather than a
    // WebSearch-specific DTO, lets an external plugin bind Vault material without
    // adding a new config-service dependency or validation path.
    let mut plugin_references = Vec::new();
    for plugin_id in &config.plugin_ids {
        if let Some(section) = config.plugin_config.get(plugin_id) {
            plugin_credential_references(
                section,
                &format!("plugin_config.{plugin_id}"),
                &mut plugin_references,
            )?;
        }
    }
    for (path, credential) in plugin_references {
        validator
            .ok_or_else(|| CredentialReferenceIssue {
                path: path.clone(),
                message: "plugin credential references require a publication validator".into(),
            })?
            .validate(workspace, &credential)
            .await
            .map_err(|message| CredentialReferenceIssue { path, message })?;
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
                prompts_as_skills: false,
            }],
            ..Default::default()
        }
    }

    /// Cause/effect table: C1 MCP exact pin and C2 plugin exact pin each require
    /// the validator; C3 matching workspace/id/revision succeeds; C4 wrong
    /// workspace fails; C5 malformed plugin pin reports its exact field path.
    #[tokio::test]
    async fn references_fail_closed_without_a_validator_and_accept_exact_match() {
        let workspace = ScopeId::from("workspace-a");
        assert!(
            validate_credential_references(None, &workspace, &config())
                .await
                .unwrap_err()
                .message
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

        let mut plugin = AgentConfig {
            plugin_ids: vec!["web_search".into()],
            plugin_config: [(
                "web_search".into(),
                serde_json::json!({
                    "provider_id": "paid",
                    "credential": { "id": "cred:workspace-a:docs", "revision": 3 }
                }),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert!(
            validate_credential_references(Some(&validator), &workspace, &plugin)
                .await
                .is_ok()
        );
        plugin.plugin_config.insert(
            "web_search".into(),
            serde_json::json!({ "credential": { "id": "missing-revision" } }),
        );
        assert_eq!(
            validate_credential_references(Some(&validator), &workspace, &plugin)
                .await
                .unwrap_err()
                .path,
            "plugin_config.web_search.credential"
        );
    }
}
