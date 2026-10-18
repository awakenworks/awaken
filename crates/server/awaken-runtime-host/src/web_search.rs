// @formal surface requirement=runtime.federated_tools.perception_not_authority
// @formal reason: This source implements the repository-controlled federated product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Host adapters for the configurable WebSearch extension.
//!
//! The extension owns provider discovery and execution. This module supplies
//! the exact Vault materialization effect it deliberately cannot own. ACP tool
//! transport is supplied through the neutral Host export port.

use awaken_credential_contract::CredentialRealizationKind;
use awaken_ext_builtin_tools::WebSearchCredentialResolver;
use awaken_runtime_contract::{CredentialAccess, PlaintextBoundary, PlaintextHolder};
use std::sync::Arc;

use awaken_credential_materializer::PinnedCredentialMaterializer;

pub(crate) type ConfiguredWebTool = (
    awaken_runtime_contract::resolved::ToolDescriptor,
    Arc<dyn awaken_tool_contract::tool::RawTool>,
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpWebToolDisposition {
    ExportHostTool,
    UseAcpBuiltin,
}

/// One closed Session-level realization decision for an ACP-visible web tool.
/// The variants make it impossible to both export the canonical capability
/// through Awaken MCP and enable a provider-owned ACP builtin.
enum AcpWebToolRealization {
    NotConfigured,
    HostTool {
        descriptor: awaken_runtime_contract::resolved::ToolDescriptor,
        tool: Arc<dyn awaken_tool_contract::tool::RawTool>,
    },
    ProviderServer(awaken_runtime_contract::resolved::ProviderServerTool),
}

fn acp_web_tool_disposition(
    policy: &dyn awaken_acp_contract::AcpAdapterPolicy<
        awaken_runtime_contract::resolved::ProviderServerTool,
    >,
    acp_cli_id: &str,
    descriptor: &awaken_runtime_contract::resolved::ToolDescriptor,
    display_name: &str,
) -> Result<AcpWebToolDisposition, crate::HostError> {
    let Some(projection) = descriptor.provider_server_tool.as_ref() else {
        return Ok(AcpWebToolDisposition::ExportHostTool);
    };
    if !policy.contains(acp_cli_id) {
        return Err(crate::HostError::bad_request(format!(
            "unknown ACP adapter `{acp_cli_id}`"
        )));
    }
    if policy.realizes_provider_server_tool(acp_cli_id, projection) {
        return Ok(AcpWebToolDisposition::UseAcpBuiltin);
    }
    Err(crate::HostError::bad_request(format!(
        "ACP `{acp_cli_id}` cannot realize the selected provider-server {display_name}"
    )))
}

impl crate::RunCapabilities {
    /// Both web plugins consume the same Workspace-pinned credential boundary.
    /// The extension registry and the exact materializer remain their existing
    /// owners; assembling this resolver twice would let their custody drift.
    fn web_credentials(
        &self,
        thread: &str,
    ) -> Option<Arc<dyn awaken_ext_builtin_tools::WebSearchCredentialResolver>> {
        self.exec
            .credential_materializer
            .clone()
            .map(|materializer| {
                Arc::new(HostWebSearchCredentialResolver::new(
                    materializer,
                    self.thread_workspace(thread),
                )) as Arc<dyn awaken_ext_builtin_tools::WebSearchCredentialResolver>
            })
    }

    /// Build the one configured WebSearch plugin used by both root and delegated
    /// Native runtimes. Provider selection stays in the immutable publication;
    /// this adapter supplies only the Worker-held credential materialization edge.
    pub(crate) fn web_search_plugin(
        &self,
        thread: &str,
        execution_configuration: Option<awaken_ext_builtin_tools::WebSearchExecutionConfiguration>,
    ) -> Arc<awaken_ext_builtin_tools::WebSearchPlugin> {
        Arc::new(
            awaken_ext_builtin_tools::WebSearchPlugin::new(
                self.tools.web_search_providers.clone(),
                self.web_credentials(thread),
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
        Arc::new(
            awaken_ext_builtin_tools::WebFetchPlugin::new(
                self.tools.web_search_providers.clone(),
                self.web_credentials(thread),
            )
            .with_execution_configuration(execution_configuration),
        )
    }

    /// Apply the one published Web selection to a Session runtime. Native
    /// execution installs the configured plugin; ACP retains the same
    /// configured tool for its later, single-owner realization decision.
    pub(crate) fn configure_web_tools_for_session(
        &self,
        thread: &str,
        spec: &awaken_runtime_contract::resolved::ResolvedSpec,
        toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
        is_acp: bool,
        mut runtime: awaken_runtime::Runtime,
    ) -> Result<
        (
            awaken_runtime::Runtime,
            Option<ConfiguredWebTool>,
            Option<ConfiguredWebTool>,
        ),
        crate::HostError,
    > {
        let search = if spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID)
        {
            let execution_configuration =
                awaken_ext_builtin_tools::web_search_execution_configuration(toolsets)
                    .map_err(crate::HostError::bad_request)?;
            let plugin = self.web_search_plugin(thread, execution_configuration);
            if is_acp {
                Some(
                    plugin
                        .configured_tool(
                            spec.plugin_config
                                .get(awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID),
                        )
                        .map_err(|error| crate::HostError::bad_request(error.to_string()))?,
                )
            } else {
                runtime = runtime.with_plugin(plugin);
                None
            }
        } else {
            None
        };
        let fetch = if spec
            .plugin_ids
            .iter()
            .any(|id| id == awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID)
        {
            let plugin = Arc::new(
                (*self.web_fetch_plugin(
                    thread,
                    awaken_ext_builtin_tools::web_fetch_execution_configuration(toolsets)
                        .map_err(crate::HostError::bad_request)?,
                ))
                .clone()
                .with_tool_source_catalogue(&spec.tool_descriptors),
            );
            if is_acp {
                Some(
                    plugin
                        .configured_tool(
                            spec.plugin_config
                                .get(awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID),
                        )
                        .map_err(|error| crate::HostError::bad_request(error.to_string()))?,
                )
            } else {
                runtime = runtime.with_plugin(plugin);
                None
            }
        } else {
            None
        };
        Ok((runtime, search, fetch))
    }

    /// Select one realization for an already-configured ACP web tool. Host
    /// execution keeps its process-local export alive for the Session lifetime;
    /// provider execution returns only the closed launch projection.
    fn realize_web_tool_for_acp(
        &self,
        acp_cli_id: &str,
        display_name: &str,
        configured: Option<ConfiguredWebTool>,
    ) -> Result<AcpWebToolRealization, crate::HostError> {
        let Some((descriptor, tool)) = configured else {
            return Ok(AcpWebToolRealization::NotConfigured);
        };
        if acp_web_tool_disposition(
            self.acp_adapter_policy()?,
            acp_cli_id,
            &descriptor,
            display_name,
        )? == AcpWebToolDisposition::UseAcpBuiltin
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

    /// Resolve the configured Search/Fetch pair for the exact ACP adapter.
    /// Provider-owned execution cannot satisfy a per-call Awaken approval;
    /// each configured tool then uses the same single-owner disposition above.
    pub(crate) fn realize_configured_web_tools_for_acp(
        &self,
        acp_cli_id: &str,
        agent_bindings: &awaken_runtime_contract::agent_bindings::AgentBindings,
        search: Option<ConfiguredWebTool>,
        fetch: Option<ConfiguredWebTool>,
    ) -> Result<
        (
            Vec<ConfiguredWebTool>,
            Vec<awaken_runtime_contract::resolved::ProviderServerTool>,
        ),
        crate::HostError,
    > {
        let mut host_tools = Vec::new();
        let mut provider_server_tools = Vec::new();
        for (tool_id, label, configured) in [
            ("web_search", "WebSearch", search),
            ("web_fetch", "WebFetch", fetch),
        ] {
            if configured.as_ref().is_some_and(|(descriptor, _)| {
                descriptor.provider_server_tool.is_some()
                    && agent_bindings.tool_policy(tool_id).is_some_and(|policy| {
                        policy.enabled
                            && matches!(
                                policy.permission,
                                awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk
                            )
                    })
            }) {
                return Err(crate::HostError::bad_request(format!(
                    "PROVIDER_SERVER_TOOL_APPROVAL_UNSUPPORTED: {label} is executed inside the model provider and cannot satisfy per-call Awaken approval; select an Awaken-hosted provider or change the permission to always_allow"
                )));
            }
            match self.realize_web_tool_for_acp(acp_cli_id, label, configured)? {
                AcpWebToolRealization::NotConfigured => {}
                AcpWebToolRealization::HostTool { descriptor, tool } => {
                    host_tools.push((descriptor, tool));
                }
                AcpWebToolRealization::ProviderServer(tool) => {
                    if !provider_server_tools.contains(&tool) {
                        provider_server_tools.push(tool);
                    }
                }
            }
        }
        Ok((host_tools, provider_server_tools))
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
    ) -> Result<awaken_agent_contract::credential::Credential, String> {
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_runtime_contract::{
        CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef, CredentialUsage,
        ModelExposurePolicy,
    };
    use awaken_tool_contract::tool::{ToolCall, ToolError};

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
            acp_web_tool_disposition(
                awaken_run_executor_acp::acp_adapter_policy(),
                "codex",
                &host,
                "WebFetch"
            )
            .unwrap(),
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
                acp_web_tool_disposition(
                    awaken_run_executor_acp::acp_adapter_policy(),
                    cli,
                    &native,
                    "WebSearch"
                )
                .unwrap(),
                AcpWebToolDisposition::UseAcpBuiltin,
                "native search must not also be exported through MCP"
            );
        }
        let wrong_cli = awaken_ext_builtin_tools::web_search_descriptor()
            .with_provider_server_tool(
                awaken_runtime_contract::resolved::ProviderServerTool::AnthropicWebSearch,
            );
        assert!(
            acp_web_tool_disposition(
                awaken_run_executor_acp::acp_adapter_policy(),
                "codex",
                &wrong_cli,
                "WebSearch"
            )
            .is_err(),
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
                acp_web_tool_disposition(
                    awaken_run_executor_acp::acp_adapter_policy(),
                    cli,
                    &unverified,
                    "WebSearch"
                )
                .is_err(),
                "an ACP builtin without a launch projection must fail closed"
            );
        }
        let provider = awaken_ext_builtin_tools::web_fetch_descriptor().with_provider_server_tool(
            awaken_runtime_contract::resolved::ProviderServerTool::openrouter_web_fetch(
                awaken_runtime_contract::resolved::OpenRouterWebFetchParameters::default(),
            ),
        );
        let error = match acp_web_tool_disposition(
            awaken_run_executor_acp::acp_adapter_policy(),
            "codex",
            &provider,
            "WebFetch",
        ) {
            Ok(_) => panic!("A2 unsupported provider-server placeholder must not be exported"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("cannot realize"),
            "A2/E2: {error}"
        );
    }

    struct ProbeTool(&'static str);

    #[async_trait::async_trait]
    impl awaken_tool_contract::tool::RawTool for ProbeTool {
        fn id(&self) -> &str {
            self.0
        }

        async fn invoke(
            &self,
            call: ToolCall,
        ) -> Result<awaken_tool_contract::tool::ToolOutput, ToolError> {
            Ok(awaken_tool_contract::tool::ToolOutput::ok(
                call.call_id,
                "probe",
            ))
        }
    }

    #[tokio::test]
    async fn configured_acp_web_pair_keeps_one_owner_and_rejects_provider_approval() {
        // Causes: C1 configured pair/owner, C2 exact tool permission, C3 the
        // one installed ACP factory supplies CLI projection support. Effects: Host exports, provider launch tools,
        // or a closed configuration error. A failed pair returns no partial
        // ServerTool result, so one capability never has two execution owners.
        //
        // | Rule | C1 pair/owner              | C2         | C3       | Effect |
        // | R1   | absent                     | any        | any      | none   |
        // | R2   | HostFetch+ProviderSearch   | Allow      | exact    | 1+1    |
        // | R3   | HostFetch+ProviderSearch   | Search Ask | exact    | error  |
        // | R4   | HostFetch                  | Fetch Ask  | n/a      | Host   |
        // | R5   | ProviderSearch             | Allow      | mismatch | error  |
        use awaken_runtime_contract::agent_bindings::{
            AgentBindings, ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride,
            ToolsetPolicy, ToolsetSource,
        };
        let host = (crate::acp_backend::with_fixture_acp(
            crate::HostDependencies::ephemeral(
                Arc::new(crate::no_model::NoModelConfiguredExecutor),
                "stub",
            )
            .await,
        ))
        .build()
        .expect("validated Host dependencies");
        let host_fetch = (
            awaken_ext_builtin_tools::web_fetch_descriptor(),
            Arc::new(ProbeTool(awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID))
                as Arc<dyn awaken_tool_contract::tool::RawTool>,
        );
        let provider_search = (
            awaken_ext_builtin_tools::web_search_descriptor().with_provider_server_tool(
                awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch,
            ),
            Arc::new(ProbeTool(awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID))
                as Arc<dyn awaken_tool_contract::tool::RawTool>,
        );
        let empty = host
            .run_assembly
            .capabilities
            .realize_configured_web_tools_for_acp("codex", &AgentBindings::default(), None, None)
            .expect("R1 absent pair");
        assert!(empty.0.is_empty() && empty.1.is_empty(), "R1");
        let (host_tools, provider_tools) = host
            .run_assembly
            .capabilities
            .realize_configured_web_tools_for_acp(
                "codex",
                &AgentBindings::default(),
                Some(provider_search.clone()),
                Some(host_fetch.clone()),
            )
            .expect("R2 exact pair");
        assert_eq!(host_tools.len(), 1, "R2 Host export");
        assert_eq!(
            host_tools[0].0.id,
            awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID
        );
        assert_eq!(
            host_tools[0].1.id(),
            awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID,
            "R2 exact host executor"
        );
        assert_eq!(
            provider_tools,
            [awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch],
            "R2 provider owns only WebSearch"
        );
        let ask = ToolExecutionPolicy {
            permission: ToolPermissionRequirement::AlwaysAsk,
            ..ToolExecutionPolicy::default()
        };
        let policy = |id| AgentBindings {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::new(id, ask)],
            }],
            ..AgentBindings::default()
        };
        let provider_error = match host
            .run_assembly
            .capabilities
            .realize_configured_web_tools_for_acp(
                "codex",
                &policy(awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID),
                Some(provider_search.clone()),
                Some(host_fetch.clone()),
            ) {
            Ok(_) => panic!("R3 provider-owned call cannot promise approval"),
            Err(error) => error,
        };
        assert!(
            provider_error
                .to_string()
                .contains("PROVIDER_SERVER_TOOL_APPROVAL_UNSUPPORTED"),
            "R3"
        );
        let host_ask = host
            .run_assembly
            .capabilities
            .realize_configured_web_tools_for_acp(
                "codex",
                &policy(awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID),
                None,
                Some(host_fetch),
            )
            .expect("R4 Host approval remains possible");
        assert_eq!(host_ask.0.len(), 1, "R4");
        assert!(host_ask.1.is_empty(), "R4");
        let unsupported = (
            awaken_ext_builtin_tools::web_search_descriptor().with_provider_server_tool(
                awaken_runtime_contract::resolved::ProviderServerTool::AnthropicWebSearch,
            ),
            provider_search.1,
        );
        assert!(
            host.run_assembly
                .capabilities
                .realize_configured_web_tools_for_acp(
                    "codex",
                    &AgentBindings::default(),
                    Some(unsupported),
                    None,
                )
                .is_err(),
            "R5 incompatible CLI"
        );
    }

    fn paid_provider_descriptor() -> (
        String,
        String,
        awaken_ext_builtin_tools::WebSearchCredentialRequirement,
        serde_json::Value,
    ) {
        (
            "brave".into(),
            "Paid probe".into(),
            awaken_ext_builtin_tools::WebSearchCredentialRequirement::Exact(
                CredentialUsage::HttpHeader {
                    name: "X-Subscription-Token".into(),
                    scheme: None,
                },
            ),
            serde_json::json!({ "type": "object" }),
        )
    }

    fn verify_paid_header(
        credential: Option<&awaken_agent_contract::credential::Credential>,
    ) -> Result<(), ToolError> {
        let header = credential
            .and_then(awaken_agent_contract::credential::Credential::header)
            .ok_or_else(|| ToolError::DefinitiveFailure("missing paid wire credential".into()))?;
        if header != ("X-Subscription-Token".into(), "paid-search-key".into()) {
            return Err(ToolError::DefinitiveFailure("wrong paid material".into()));
        }
        Ok(())
    }

    struct PaidProbe {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl awaken_ext_builtin_tools::WebSearchProvider for PaidProbe {
        fn descriptor(&self) -> awaken_ext_builtin_tools::WebSearchProviderDescriptor {
            let (id, label, credential, options_schema) = paid_provider_descriptor();
            awaken_ext_builtin_tools::WebSearchProviderDescriptor {
                id,
                label,
                credential,
                options_schema,
            }
        }

        async fn search(
            &self,
            request: awaken_ext_builtin_tools::WebSearchRequest,
            credential: Option<&awaken_agent_contract::credential::Credential>,
        ) -> Result<Vec<awaken_ext_builtin_tools::WebSearchResult>, ToolError> {
            verify_paid_header(credential)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![awaken_ext_builtin_tools::WebSearchResult {
                title: request.query,
                url: "https://paid.test/result".into(),
                snippet: "vault material reached the selected provider".into(),
            }])
        }
    }

    /// Test-only Fetch consumer of the same exact paid wire requirement.
    struct PaidFetchProbe {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl awaken_ext_builtin_tools::WebFetchProvider for PaidFetchProbe {
        fn descriptor(&self) -> awaken_ext_builtin_tools::WebFetchProviderDescriptor {
            let (id, label, credential, options_schema) = paid_provider_descriptor();
            awaken_ext_builtin_tools::WebFetchProviderDescriptor {
                id,
                label,
                credential,
                options_schema,
            }
        }

        async fn fetch(
            &self,
            request: awaken_ext_builtin_tools::WebFetchRequest,
            credential: Option<&awaken_agent_contract::credential::Credential>,
            domain_filters: &[awaken_ext_builtin_tools::WebDomainFilter],
        ) -> Result<String, ToolError> {
            assert!(domain_filters.is_empty(), "R7 Fetch has no domain filter");
            verify_paid_header(credential)?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!("paid fetch: {}", request.url))
        }
    }

    #[tokio::test]
    async fn web_plugins_share_exact_credential_custody_without_sharing_tool_grants() {
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
        // C7 plugin={Search,Fetch}; C8 materializer={installed,missing}; C9
        // tool grant={Search only,Fetch only}; C10 domain filters={empty}.
        // R7 exact+installed grants each plugin one invocation, with no Fetch
        // domain filter; R8 missing/foreign denies both before provider
        // I/O; R9 either single-tool grant blocks the other for Native and ACP.
        // The shared resolver changes no tool surface or permission authority.
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
            awaken_credential_contract::CredentialDescriptor::new(
                "brave",
                awaken_credential_contract::CredentialMaterialDescriptor::secret(
                    awaken_credential_contract::OPAQUE_SECRET_MATERIAL_TYPE,
                ),
                [
                    awaken_credential_contract::CredentialTargetContract::new(
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
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        let materializer = PinnedCredentialMaterializer::new(
            Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                repo.clone(),
                secrets,
                awaken_time::system_clock(),
            )),
            awaken_time::system_clock(),
        );
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
        let search_calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        let mut providers =
            awaken_ext_builtin_tools::WebSearchProviderRegistry::try_new([Arc::new(PaidProbe {
                calls: search_calls.clone(),
            })
                as Arc<dyn awaken_ext_builtin_tools::WebSearchProvider>])
            .unwrap();
        providers
            .register_fetch(Arc::new(PaidFetchProbe {
                calls: fetch_calls.clone(),
            }))
            .unwrap();
        let host = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await
        .with_local_workspace("workspace-a")
        .with_credential_materializer(materializer.clone())
        .with_web_search_provider_registry(providers.clone()))
        .build()
        .expect("validated Host dependencies");
        let resolver = host
            .run_assembly
            .capabilities
            .web_credentials("bound-thread")
            .unwrap();
        assert_eq!(
            resolver.resolve(&access, "brave").await.unwrap().header(),
            Some(("X-Subscription-Token".into(), "paid-search-key".into()))
        );

        let published = serde_json::json!({
            "provider_id": "brave",
            "credential_access": access,
            "options": {},
        });
        let without_materializer = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await
        .with_local_workspace("workspace-a")
        .with_web_search_provider_registry(providers.clone()))
        .build()
        .expect("validated Host dependencies");
        assert!(
            without_materializer
                .run_assembly
                .capabilities
                .web_search_plugin("bound-thread", None)
                .configured_tool(Some(&published))
                .is_err(),
            "R8 Search requires a materializer"
        );
        assert!(
            without_materializer
                .run_assembly
                .capabilities
                .web_fetch_plugin("bound-thread", None)
                .configured_tool(Some(&published))
                .is_err(),
            "R8 Fetch requires a materializer"
        );

        let foreign = (crate::HostDependencies::ephemeral(
            Arc::new(crate::no_model::NoModelConfiguredExecutor),
            "stub",
        )
        .await
        .with_local_workspace("workspace-b")
        .with_credential_materializer(materializer.clone())
        .with_web_search_provider_registry(providers))
        .build()
        .expect("validated Host dependencies");
        let (_, foreign_search) = foreign
            .run_assembly
            .capabilities
            .web_search_plugin("bound-thread", None)
            .configured_tool(Some(&published))
            .unwrap();
        let (_, foreign_fetch) = foreign
            .run_assembly
            .capabilities
            .web_fetch_plugin("bound-thread", None)
            .configured_tool(Some(&published))
            .unwrap();
        for (tool, call) in [
            (
                foreign_search,
                ToolCall {
                    call_id: "foreign-search".into(),
                    tool_id: awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID.into(),
                    arguments: serde_json::json!({ "query": "ddd" }),
                },
            ),
            (
                foreign_fetch,
                ToolCall {
                    call_id: "foreign-fetch".into(),
                    tool_id: awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID.into(),
                    arguments: serde_json::json!({ "url": "https://paid.test/result" }),
                },
            ),
        ] {
            assert!(tool.invoke(call).await.is_err(), "R8 foreign Workspace");
        }
        assert_eq!(search_calls.load(Ordering::SeqCst), 0, "R8/E2 Search");
        assert_eq!(fetch_calls.load(Ordering::SeqCst), 0, "R8/E2 Fetch");

        let (_, paid_search) = host
            .run_assembly
            .capabilities
            .web_search_plugin("bound-thread", None)
            .configured_tool(Some(&published))
            .unwrap();
        let (_, paid_fetch) = host
            .run_assembly
            .capabilities
            .web_fetch_plugin("bound-thread", None)
            .configured_tool(Some(&published))
            .unwrap();
        let search_output = paid_search
            .invoke(ToolCall {
                call_id: "paid-search".into(),
                tool_id: awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID.into(),
                arguments: serde_json::json!({ "query": "ddd" }),
            })
            .await
            .unwrap();
        let fetch_output = paid_fetch
            .invoke(ToolCall {
                call_id: "paid-fetch".into(),
                tool_id: awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID.into(),
                arguments: serde_json::json!({ "url": "https://paid.test/result" }),
            })
            .await
            .unwrap();
        assert!(search_output.text().contains("https://paid.test/result"));
        assert!(fetch_output.text().contains("paid fetch"));
        assert_eq!(search_calls.load(Ordering::SeqCst), 1, "R7/E1 Search");
        assert_eq!(fetch_calls.load(Ordering::SeqCst), 1, "R7/E1 Fetch");

        use awaken_runtime_contract::agent_bindings::{
            ResolvedConfiguration, ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };
        use awaken_runtime_contract::permission::GateOutcomeKind;
        for (granted, blocked) in [
            (
                awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID,
                awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID,
            ),
            (
                awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID,
                awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID,
            ),
        ] {
            let allowed = ToolExecutionPolicy::default();
            let authorization = crate::config::effective_tool_authorization(
                &ResolvedConfiguration::default(),
                &[],
                &[ToolsetPolicy {
                    source: ToolsetSource::Agent,
                    default: ToolExecutionPolicy {
                        enabled: false,
                        ..allowed
                    },
                    overrides: vec![ToolPolicyOverride::new(granted, allowed)],
                }],
            );
            for (tool_id, expected) in [
                (granted, GateOutcomeKind::Allow),
                (blocked, GateOutcomeKind::Block),
            ] {
                let call = ToolCall {
                    call_id: format!("grant-{tool_id}"),
                    tool_id: tool_id.into(),
                    arguments: serde_json::json!({}),
                };
                let native = authorization
                    .gate
                    .gate(&call, &awaken_agent_contract::agent::state::Store::new())
                    .await;
                let acp = authorization.policy.evaluate(&call).await;
                assert_eq!(native.outcome.kind(), expected, "R9 Native {tool_id}");
                assert_eq!(
                    acp.verdict.kind().gate_outcome_kind(),
                    expected,
                    "R9 ACP {tool_id}"
                );
            }
        }
        assert_eq!(search_calls.load(Ordering::SeqCst), 1, "R9/E3 Search");
        assert_eq!(fetch_calls.load(Ordering::SeqCst), 1, "R9/E3 Fetch");

        let mut stale = access.clone();
        stale.credential.revision = 2;
        assert!(resolver.resolve(&stale, "brave").await.is_err());
        assert!(
            foreign
                .run_assembly
                .capabilities
                .web_credentials("bound-thread")
                .unwrap()
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
