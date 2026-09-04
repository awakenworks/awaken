//! Host adapters for the configurable WebSearch extension.
//!
//! The extension owns provider discovery and execution. This module supplies
//! the exact Vault materialization effect it deliberately cannot own. ACP tool
//! transport is supplied through the neutral Host export port.

use awaken_ext_builtin_tools::WebSearchCredentialResolver;
use awaken_runtime_contract::{
    CredentialAccess, CredentialRealizationKind, PlaintextBoundary, PlaintextHolder,
};
use std::sync::Arc;

use awaken_credential_materializer::PinnedCredentialMaterializer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpWebToolDisposition {
    ExportHostTool,
    UseAcpBuiltin,
}

/// One closed Session-level realization decision for an ACP-visible web tool.
/// The variants make it impossible to both export the canonical capability
/// through Awaken MCP and enable a provider-owned ACP builtin.
pub(crate) enum AcpWebToolRealization {
    NotConfigured,
    HostTool {
        descriptor: awaken_runtime_contract::resolved::ToolDescriptor,
        tool: Arc<dyn awaken_runtime_contract::tool::RawTool>,
    },
    ProviderServer(awaken_runtime_contract::resolved::ProviderServerTool),
}

fn acp_web_tool_disposition(
    acp_cli_id: &str,
    descriptor: &awaken_runtime_contract::resolved::ToolDescriptor,
    display_name: &str,
) -> Result<AcpWebToolDisposition, crate::HostError> {
    let Some(projection) = descriptor.provider_server_tool.as_ref() else {
        return Ok(AcpWebToolDisposition::ExportHostTool);
    };
    let adapter = awaken_run_executor_acp::acp_cli(acp_cli_id).ok_or_else(|| {
        crate::HostError::bad_request(format!("unknown ACP adapter `{acp_cli_id}`"))
    })?;
    if adapter.realizes_provider_server_tool(projection) {
        return Ok(AcpWebToolDisposition::UseAcpBuiltin);
    }
    Err(crate::HostError::bad_request(format!(
        "ACP `{acp_cli_id}` cannot realize the selected provider-server {display_name}"
    )))
}

impl crate::host::SharedHost {
    /// Build the one configured WebSearch plugin used by both root and delegated
    /// Native runtimes. Provider selection stays in the immutable publication;
    /// this adapter supplies only the Worker-held credential materialization edge.
    pub(crate) fn web_search_plugin(
        &self,
        thread: &str,
        execution_configuration: Option<awaken_ext_builtin_tools::WebSearchExecutionConfiguration>,
    ) -> Arc<awaken_ext_builtin_tools::WebSearchPlugin> {
        let credentials = self.credential_materializer.clone().map(|materializer| {
            Arc::new(HostWebSearchCredentialResolver::new(
                materializer,
                self.thread_workspace(thread),
            )) as Arc<dyn awaken_ext_builtin_tools::WebSearchCredentialResolver>
        });
        Arc::new(
            awaken_ext_builtin_tools::WebSearchPlugin::new(
                self.web_search_providers.clone(),
                credentials,
            )
            .with_execution_configuration(execution_configuration),
        )
    }

    /// Build the configured WebFetch plugin from the same provider catalog and
    /// credential boundary as WebSearch. There is no static fetch fallback.
    pub(crate) fn web_fetch_plugin(
        &self,
        thread: &str,
        execution_configuration: Option<awaken_ext_builtin_tools::WebFetchExecutionConfiguration>,
    ) -> Arc<awaken_ext_builtin_tools::WebFetchPlugin> {
        let credentials = self.credential_materializer.clone().map(|materializer| {
            Arc::new(HostWebSearchCredentialResolver::new(
                materializer,
                self.thread_workspace(thread),
            )) as Arc<dyn awaken_ext_builtin_tools::WebSearchCredentialResolver>
        });
        Arc::new(
            awaken_ext_builtin_tools::WebFetchPlugin::new(
                self.web_search_providers.clone(),
                credentials,
            )
            .with_execution_configuration(execution_configuration),
        )
    }

    /// Select one realization for an already-configured ACP web tool. Host
    /// execution keeps its process-local export alive for the Session lifetime;
    /// provider execution returns only the closed launch projection.
    pub(crate) fn realize_web_tool_for_acp(
        &self,
        acp_cli_id: &str,
        display_name: &str,
        configured: Option<(
            awaken_runtime_contract::resolved::ToolDescriptor,
            Arc<dyn awaken_runtime_contract::tool::RawTool>,
        )>,
    ) -> Result<AcpWebToolRealization, crate::HostError> {
        let Some((descriptor, tool)) = configured else {
            return Ok(AcpWebToolRealization::NotConfigured);
        };
        if acp_web_tool_disposition(acp_cli_id, &descriptor, display_name)?
            == AcpWebToolDisposition::UseAcpBuiltin
        {
            tracing::info!(
                acp_cli = acp_cli_id,
                tool = display_name,
                provider = descriptor
                    .provider_server_tool
                    .as_ref()
                    .map_or("unknown", |tool| tool.provider_kind()),
                realization = "acp_builtin",
                "selected one ACP web-tool execution owner"
            );
            return Ok(AcpWebToolRealization::ProviderServer(
                descriptor
                    .provider_server_tool
                    .expect("builtin disposition requires a provider projection"),
            ));
        }
        tracing::info!(
            acp_cli = acp_cli_id,
            tool = display_name,
            realization = "awaken_host_mcp",
            "selected one ACP web-tool execution owner"
        );
        Ok(AcpWebToolRealization::HostTool { descriptor, tool })
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
        access: &CredentialAccess,
        provider_id: &str,
    ) -> Result<awaken_credential::Credential, String> {
        let holder = PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        );
        let resolved = self
            .materializer
            .resolve_for_workspace_and_provider(
                access,
                &holder,
                CredentialRealizationKind::WorkerProviderAdapter,
                &self.workspace,
                provider_id,
                &(provider_id, &access.usage),
            )
            .await
            .map_err(|error| error.to_string())?;
        awaken_credential_materializer::materialized_http_credential(access, &resolved.material)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use awaken_runtime_contract::tool::{ToolCall, ToolError};
    use awaken_runtime_contract::{
        CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef, CredentialUsage,
        ModelExposurePolicy,
    };

    #[test]
    fn acp_web_tools_are_exported_or_native_but_never_both() {
        // Cause/effect table: E1 a HostExecuted descriptor crosses the ACP MCP
        // exporter; E2 an exact CLI/provider builtin is not exported; E3 an
        // incompatible server projection fails before a placeholder RawTool or
        // lease is created. Exactly one execution owner is visible.
        //
        // | Rule | provider_server_tool | Effect |
        // | A1 | absent | E1 |
        // | A2 | exact CLI builtin | E2/no MCP export |
        // | A3 | incompatible builtin | E3/fail closed |
        let host = awaken_ext_builtin_tools::web_fetch_descriptor();
        assert_eq!(
            acp_web_tool_disposition("codex", &host, "WebFetch").unwrap(),
            AcpWebToolDisposition::ExportHostTool,
            "A1/E1"
        );
        for (cli, projection) in [
            (
                "codex",
                awaken_runtime_contract::resolved::ProviderServerTool::DeepSeekResponsesWebSearch,
            ),
            (
                "codex",
                awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch,
            ),
        ] {
            let native = awaken_ext_builtin_tools::web_search_descriptor()
                .with_provider_server_tool(projection);
            assert_eq!(
                acp_web_tool_disposition(cli, &native, "WebSearch").unwrap(),
                AcpWebToolDisposition::UseAcpBuiltin,
                "native search must not also be exported through MCP"
            );
        }
        let wrong_cli = awaken_ext_builtin_tools::web_search_descriptor()
            .with_provider_server_tool(
                awaken_runtime_contract::resolved::ProviderServerTool::AnthropicWebSearch,
            );
        assert!(
            acp_web_tool_disposition("codex", &wrong_cli, "WebSearch").is_err(),
            "an ACP protocol match must not inherit another CLI's builtin"
        );
        for (cli, projection) in [
            (
                "codex",
                awaken_runtime_contract::resolved::ProviderServerTool::openrouter_web_search(
                    awaken_runtime_contract::resolved::OpenRouterWebSearchParameters::default(),
                ),
            ),
            (
                "claude",
                awaken_runtime_contract::resolved::ProviderServerTool::AnthropicWebSearch,
            ),
            (
                "gemini",
                awaken_runtime_contract::resolved::ProviderServerTool::GeminiWebSearch,
            ),
        ] {
            let unverified = awaken_ext_builtin_tools::web_search_descriptor()
                .with_provider_server_tool(projection);
            assert!(
                acp_web_tool_disposition(cli, &unverified, "WebSearch").is_err(),
                "an ACP builtin without a launch projection must fail closed"
            );
        }
        let provider = awaken_ext_builtin_tools::web_fetch_descriptor().with_provider_server_tool(
            awaken_runtime_contract::resolved::ProviderServerTool::openrouter_web_fetch(
                awaken_runtime_contract::resolved::OpenRouterWebFetchParameters::default(),
            ),
        );
        let error = match acp_web_tool_disposition("codex", &provider, "WebFetch") {
            Ok(_) => panic!("A2 unsupported provider-server placeholder must not be exported"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("cannot realize"),
            "A2/E2: {error}"
        );
    }

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
            credential: Option<&awaken_credential::Credential>,
        ) -> Result<Vec<awaken_ext_builtin_tools::WebSearchResult>, ToolError> {
            let header = credential
                .and_then(awaken_credential::Credential::header)
                .ok_or_else(|| ToolError::Execution("missing paid wire credential".into()))?;
            if header != ("X-Subscription-Token".into(), "paid-search-key".into()) {
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
            CredentialRepo, InMemoryCredentialRepo, enter_credential_described,
        };
        use awaken_credential_vault::{
            CredentialCreateParams, CredentialKind, InMemorySecretStore, SecretStore,
        };

        // Cause/effect decision table:
        // R1 exact workspace+revision+provider -> one wire credential; R2 stale
        // requested revision; R3 wrong Workspace; R4 provider mismatch; R5
        // source rotated after publication; R6 source disabled after rotation.
        // R2-R6 reject in the materializer before a Provider receives a header.
        let repo: Arc<dyn CredentialRepo> = Arc::new(InMemoryCredentialRepo::new());
        let secrets: Arc<dyn SecretStore> = Arc::new(InMemorySecretStore::new());
        let usage = CredentialUsage::HttpHeader {
            name: "X-Subscription-Token".into(),
            scheme: None,
        };
        let source = enter_credential_described(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(awaken_agent_contract::RedactedString::new(
                    "paid-search-key",
                )),
                oauth_command: None,
            },
            awaken_runtime_contract::credential::CredentialDescriptor::new(
                "brave",
                awaken_runtime_contract::credential::CredentialMaterialDescriptor::secret(
                    awaken_runtime_contract::credential::OPAQUE_SECRET_MATERIAL_TYPE,
                ),
                [
                    awaken_runtime_contract::credential::CredentialTargetContract::new(
                        awaken_runtime_contract::CredentialTarget::new(
                            awaken_runtime_contract::credential::CredentialPurpose::WebProviderAuthorization,
                            "brave",
                        ),
                        usage.clone(),
                    ),
                ],
            ),
            secrets.as_ref(),
            repo.as_ref(),
        )
        .await
        .unwrap();
        let materializer = PinnedCredentialMaterializer::new(repo.clone(), secrets);
        let exact = CredentialRef {
            id: source.id.0.clone(),
            revision: 1,
        };
        let holder = PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        );
        let access = CredentialAccess::new(
            exact.clone(),
            CredentialMaterialSource::ControlPlaneReference,
            usage.clone(),
            CredentialExecutionPolicy::exact(holder, ModelExposurePolicy::Forbidden),
        )
        .with_target(awaken_runtime_contract::CredentialTarget::new(
            awaken_runtime_contract::credential::CredentialPurpose::WebProviderAuthorization,
            "brave",
        ));
        let resolver = Arc::new(HostWebSearchCredentialResolver::new(
            materializer.clone(),
            "workspace-a".into(),
        ));
        assert_eq!(
            resolver.resolve(&access, "brave").await.unwrap().header(),
            Some(("X-Subscription-Token".into(), "paid-search-key".into()))
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
                "credential_access": access,
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
        let mut stale = access.clone();
        stale.credential.revision = 2;
        assert!(resolver.resolve(&stale, "brave").await.is_err());
        assert!(
            HostWebSearchCredentialResolver::new(materializer.clone(), "workspace-b".into())
                .resolve(&access, "brave")
                .await
                .is_err()
        );
        assert!(resolver.resolve(&access, "another-provider").await.is_err());

        let mut rotated = source.clone();
        rotated.version = 2;
        repo.put(rotated.clone()).await.unwrap();
        assert!(resolver.resolve(&access, "brave").await.is_err(), "R5");
        rotated.status = awaken_credential_vault::CredentialStatus::Disabled;
        repo.put(rotated).await.unwrap();
        let mut rotated_access = access;
        rotated_access.credential.revision = 2;
        assert!(
            resolver.resolve(&rotated_access, "brave").await.is_err(),
            "R6"
        );
    }
}
