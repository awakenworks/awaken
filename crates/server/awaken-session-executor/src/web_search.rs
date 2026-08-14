//! Host adapters for the configurable WebSearch extension.
//!
//! The extension owns provider discovery and execution. This module supplies
//! the exact Vault materialization effect it deliberately cannot own. ACP tool
//! transport is supplied through the neutral Host export port.

use awaken_ext_builtin_tools::WebSearchCredentialResolver;
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterial, CredentialMaterialSource,
    CredentialRealizationKind, CredentialRef, CredentialUsage, ModelExposurePolicy,
    PlaintextBoundary, PlaintextHolder,
};
use std::sync::Arc;

use awaken_credential_materializer::PinnedCredentialMaterializer;

impl crate::host::SharedHost {
    /// Build the one configured WebSearch plugin used by both root and delegated
    /// Native runtimes. Provider selection stays in the immutable publication;
    /// this adapter supplies only the Worker-held credential materialization edge.
    pub(crate) fn web_search_plugin(
        &self,
        thread: &str,
    ) -> Arc<awaken_ext_builtin_tools::WebSearchPlugin> {
        let credentials = self.credential_materializer.clone().map(|materializer| {
            Arc::new(HostWebSearchCredentialResolver::new(
                materializer,
                self.thread_workspace(thread),
            )) as Arc<dyn awaken_ext_builtin_tools::WebSearchCredentialResolver>
        });
        Arc::new(awaken_ext_builtin_tools::WebSearchPlugin::new(
            self.web_search_providers.clone(),
            credentials,
        ))
    }
}

#[derive(Clone)]
pub(crate) struct HostWebSearchCredentialResolver {
    materializer: PinnedCredentialMaterializer,
    workspace: String,
}

impl HostWebSearchCredentialResolver {
    pub(crate) fn new(materializer: PinnedCredentialMaterializer, workspace: String) -> Self {
        Self {
            materializer,
            workspace,
        }
    }
}

#[async_trait::async_trait]
impl WebSearchCredentialResolver for HostWebSearchCredentialResolver {
    async fn resolve(
        &self,
        credential: &CredentialRef,
        provider_id: &str,
        usage: &CredentialUsage,
    ) -> Result<CredentialMaterial, String> {
        let holder = PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        );
        let access = CredentialAccess::new(
            credential.clone(),
            CredentialMaterialSource::ControlPlaneReference,
            usage.clone(),
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        );
        self.materializer
            .resolve_for_workspace_and_provider(
                &access,
                &holder,
                CredentialRealizationKind::WorkerProviderAdapter,
                &self.workspace,
                provider_id,
                &(provider_id, usage),
            )
            .await
            .map(|resolved| resolved.material)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use awaken_runtime_contract::tool::{ToolCall, ToolError};

    struct PaidProbe;

    #[async_trait::async_trait]
    impl awaken_ext_builtin_tools::WebSearchProvider for PaidProbe {
        fn descriptor(&self) -> awaken_ext_builtin_tools::WebSearchProviderDescriptor {
            awaken_ext_builtin_tools::WebSearchProviderDescriptor {
                id: "brave".into(),
                label: "Paid probe".into(),
                credential: awaken_ext_builtin_tools::WebSearchCredentialRequirement::Exact(
                    CredentialUsage::HttpHeader {
                        name: "X-Subscription-Token".into(),
                        scheme: None,
                    },
                ),
                options_schema: serde_json::json!({ "type": "object" }),
            }
        }

        async fn search(
            &self,
            request: awaken_ext_builtin_tools::WebSearchRequest,
            credential: Option<&CredentialMaterial>,
        ) -> Result<Vec<awaken_ext_builtin_tools::WebSearchResult>, ToolError> {
            let secret = credential
                .ok_or_else(|| ToolError::Execution("missing paid material".into()))?
                .single_secret()
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            if secret.expose_secret() != "paid-search-key" {
                return Err(ToolError::Execution("wrong paid material".into()));
            }
            Ok(vec![awaken_ext_builtin_tools::WebSearchResult {
                title: request.query,
                url: "https://paid.test/result".into(),
                snippet: "vault material reached the selected provider".into(),
            }])
        }
    }

    #[tokio::test]
    async fn exact_web_search_credential_is_fenced_before_plaintext() {
        use awaken_credential_vault::repo::{
            CredentialRepo, InMemoryCredentialRepo, enter_credential,
        };
        use awaken_credential_vault::{
            CredentialCreateParams, CredentialKind, InMemorySecretStore, SecretStore,
        };

        // Cause/effect decision table:
        // R1 exact workspace+revision+provider -> secret; R2 stale revision ->
        // reject; R3 wrong Workspace -> reject; R4 provider mismatch -> reject.
        // Every rejection occurs in the materializer, before an HTTP provider can
        // receive plaintext.
        let repo: Arc<dyn CredentialRepo> = Arc::new(InMemoryCredentialRepo::new());
        let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("brave".into()),
                env_key: None,
                secret: Some(awaken_agent_contract::RedactedString::new(
                    "paid-search-key",
                )),
                oauth_command: None,
            },
            secrets.as_ref(),
            repo.as_ref(),
        )
        .await
        .unwrap();
        let materializer = PinnedCredentialMaterializer::new(repo, secrets);
        let exact = CredentialRef {
            id: source.id.0,
            revision: 1,
        };
        let resolver = Arc::new(HostWebSearchCredentialResolver::new(
            materializer.clone(),
            "workspace-a".into(),
        ));
        assert_eq!(
            resolver
                .resolve(
                    &exact,
                    "brave",
                    &CredentialUsage::HttpHeader {
                        name: "X-Subscription-Token".into(),
                        scheme: None,
                    },
                )
                .await
                .unwrap()
                .single_secret()
                .unwrap()
                .expose_secret(),
            "paid-search-key"
        );
        let providers =
            awaken_ext_builtin_tools::WebSearchProviderRegistry::try_new([
                Arc::new(PaidProbe) as Arc<dyn awaken_ext_builtin_tools::WebSearchProvider>
            ])
            .unwrap();
        let plugin =
            awaken_ext_builtin_tools::WebSearchPlugin::new(providers, Some(resolver.clone()));
        let (_, paid_tool) = plugin
            .configured_tool(Some(&serde_json::json!({
                "provider_id": "brave",
                "credential": { "id": exact.id.clone(), "revision": exact.revision },
                "options": {},
            })))
            .unwrap();
        let output = paid_tool
            .invoke(ToolCall {
                call_id: "paid-e2e".into(),
                tool_id: awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID.into(),
                arguments: serde_json::json!({ "query": "ddd" }),
            })
            .await
            .unwrap();
        assert!(output.text().contains("https://paid.test/result"));
        let stale = CredentialRef {
            revision: 2,
            ..exact.clone()
        };
        assert!(
            resolver
                .resolve(
                    &stale,
                    "brave",
                    &CredentialUsage::HttpHeader {
                        name: "X-Key".into(),
                        scheme: None
                    },
                )
                .await
                .is_err()
        );
        assert!(
            HostWebSearchCredentialResolver::new(materializer.clone(), "workspace-b".into())
                .resolve(
                    &exact,
                    "brave",
                    &CredentialUsage::HttpHeader {
                        name: "X-Key".into(),
                        scheme: None
                    },
                )
                .await
                .is_err()
        );
        assert!(
            resolver
                .resolve(
                    &exact,
                    "another-provider",
                    &CredentialUsage::HttpHeader {
                        name: "X-Key".into(),
                        scheme: None
                    },
                )
                .await
                .is_err()
        );
    }
}
