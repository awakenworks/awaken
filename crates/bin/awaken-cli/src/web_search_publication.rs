//! Control-side adapter for WebSearch publication validation.

use std::sync::Arc;

use awaken_config_service::PluginPublicationResolver;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{ExactCredentialAccessRequest, compile_exact_credential_access};
use awaken_runtime_contract::credential::CredentialPurpose;
use awaken_runtime_contract::{
    CredentialExecutionPolicy, CredentialMaterialBinding, CredentialTarget, ModelExposurePolicy,
    PlaintextBoundary, PlaintextHolder,
};

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn compile_target_access(
    credentials: &dyn CredentialRepo,
    workspace: &awaken_tenancy::ScopeId,
    target: &mut awaken_ext_builtin_tools::WebProviderTarget,
    requirement: awaken_ext_builtin_tools::WebSearchCredentialRequirement,
) -> Result<(), String> {
    let awaken_ext_builtin_tools::WebSearchCredentialRequirement::Exact(usage) = requirement else {
        return Ok(());
    };
    let reference = target
        .credential
        .take()
        .ok_or_else(|| "web provider credential source pin is missing".to_string())?;
    let source = credentials
        .get(&CredentialSourceId(reference.id.clone()))
        .await
        .map_err(|_| "web provider credential is unavailable in this Workspace".to_string())?;
    if u64::try_from(source.version).ok() != Some(reference.revision) {
        return Err("web provider credential revision changed before publication".into());
    }
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    let provider_target = CredentialTarget::new(
        CredentialPurpose::WebProviderAuthorization,
        target.provider_id.clone(),
    );
    let binding = CredentialMaterialBinding::for_target(
        workspace.as_str(),
        &(&target.provider_id, &usage),
        &usage,
    );
    let access = compile_exact_credential_access(
        &source,
        ExactCredentialAccessRequest {
            workspace_id: Some(workspace.as_str()),
            target: Some(provider_target),
            usage,
            policy: CredentialExecutionPolicy::exact(
                holder.clone(),
                ModelExposurePolicy::Forbidden,
            ),
            selected_holder: &holder,
            binding: &binding,
            now_unix_ms: unix_time_ms(),
        },
    )
    .map_err(|_| "web provider credential is not authorized for this provider".to_string())?;
    target.credential_access = Some(access);
    Ok(())
}

async fn compile_config_accesses(
    credentials: &dyn CredentialRepo,
    workspace: &awaken_tenancy::ScopeId,
    providers: &awaken_ext_builtin_tools::WebSearchProviderRegistry,
    config: &mut awaken_ext_builtin_tools::WebSearchConfig,
    fetch: bool,
) -> Result<(), String> {
    let requirement = if fetch {
        providers.fetch_credential_requirement(&config.provider_id)
    } else {
        providers.search_credential_requirement(&config.provider_id)
    };
    if let Some(requirement) = requirement {
        let mut primary = awaken_ext_builtin_tools::WebProviderTarget {
            provider_id: config.provider_id.clone(),
            credential: config.credential.take(),
            credential_access: config.credential_access.take(),
            options: std::mem::take(&mut config.options),
        };
        compile_target_access(credentials, workspace, &mut primary, requirement).await?;
        config.credential = primary.credential;
        config.credential_access = primary.credential_access;
        config.options = primary.options;
    }
    for target in &mut config.fallbacks {
        let requirement = if fetch {
            providers.fetch_credential_requirement(&target.provider_id)
        } else {
            providers.search_credential_requirement(&target.provider_id)
        }
        .ok_or_else(|| "web provider disappeared during publication".to_string())?;
        compile_target_access(credentials, workspace, target, requirement).await?;
    }
    Ok(())
}

pub(crate) struct WebSearchPublicationResolver {
    providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    credentials: Arc<dyn CredentialRepo>,
}

impl WebSearchPublicationResolver {
    pub(crate) fn new(
        providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
        credentials: Arc<dyn CredentialRepo>,
    ) -> Self {
        Self {
            providers,
            credentials,
        }
    }
}

#[async_trait::async_trait]
impl PluginPublicationResolver for WebSearchPublicationResolver {
    fn plugin_id(&self) -> &str {
        awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID
    }

    async fn resolve(
        &self,
        workspace: &awaken_tenancy::ScopeId,
        toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
        config: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        awaken_ext_builtin_tools::WebSearchPlugin::new(self.providers.clone(), None)
            .with_execution_configuration(
                awaken_ext_builtin_tools::web_search_execution_configuration(toolsets)?,
            )
            .validate_config(config)
            .map_err(|error| error.to_string())?;
        let mut config: awaken_ext_builtin_tools::WebSearchConfig = serde_json::from_value(
            config
                .cloned()
                .expect("validated WebSearch config is present"),
        )
        .map_err(|error| error.to_string())?;
        compile_config_accesses(
            self.credentials.as_ref(),
            workspace,
            &self.providers,
            &mut config,
            false,
        )
        .await?;
        serde_json::to_value(config).map_err(|error| error.to_string())
    }
}

pub(crate) struct WebFetchPublicationResolver {
    providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
    credentials: Arc<dyn CredentialRepo>,
}

impl WebFetchPublicationResolver {
    pub(crate) fn new(
        providers: awaken_ext_builtin_tools::WebSearchProviderRegistry,
        credentials: Arc<dyn CredentialRepo>,
    ) -> Self {
        Self {
            providers,
            credentials,
        }
    }
}

#[async_trait::async_trait]
impl PluginPublicationResolver for WebFetchPublicationResolver {
    fn plugin_id(&self) -> &str {
        awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID
    }

    async fn resolve(
        &self,
        workspace: &awaken_tenancy::ScopeId,
        toolsets: &[awaken_runtime_contract::agent_bindings::ToolsetPolicy],
        config: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        awaken_ext_builtin_tools::WebFetchPlugin::new(self.providers.clone(), None)
            .with_execution_configuration(
                awaken_ext_builtin_tools::web_fetch_execution_configuration(toolsets)?,
            )
            .validate_config(config)
            .map_err(|error| error.to_string())?;
        let mut config: awaken_ext_builtin_tools::WebSearchConfig = serde_json::from_value(
            config
                .cloned()
                .unwrap_or_else(awaken_ext_builtin_tools::WebFetchPlugin::default_config),
        )
        .map_err(|error| error.to_string())?;
        compile_config_accesses(
            self.credentials.as_ref(),
            workspace,
            &self.providers,
            &mut config,
            true,
        )
        .await?;
        serde_json::to_value(config).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential_described};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialKind, CredentialStatus, InMemorySecretStore,
    };

    struct PaidFetch;

    #[async_trait::async_trait]
    impl awaken_ext_builtin_tools::WebFetchProvider for PaidFetch {
        fn descriptor(&self) -> awaken_ext_builtin_tools::WebFetchProviderDescriptor {
            awaken_ext_builtin_tools::WebFetchProviderDescriptor {
                id: "brave".into(),
                label: "Paid fetch probe".into(),
                credential: awaken_ext_builtin_tools::WebSearchCredentialRequirement::Exact(
                    awaken_runtime_contract::CredentialUsage::HttpHeader {
                        name: "X-Subscription-Token".into(),
                        scheme: None,
                    },
                ),
                options_schema: serde_json::json!({"type":"object"}),
            }
        }

        async fn fetch(
            &self,
            _request: awaken_ext_builtin_tools::WebFetchRequest,
            _credential: Option<&awaken_credential::Credential>,
            _domain_filter: Option<&awaken_ext_builtin_tools::WebDomainFilter>,
        ) -> Result<String, awaken_runtime_contract::tool::ToolError> {
            unreachable!("publication never executes a provider")
        }
    }

    async fn described_web_credential(
        credentials: Arc<dyn CredentialRepo>,
        workspace: &str,
        provider: &str,
    ) -> awaken_credential_vault::CredentialSource {
        let usage = awaken_runtime_contract::CredentialUsage::HttpHeader {
            name: "X-Subscription-Token".into(),
            scheme: None,
        };
        enter_credential_described(
            CredentialCreateParams {
                workspace_id: workspace.into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("never-published")),
                oauth_command: None,
            },
            awaken_runtime_contract::credential::CredentialDescriptor::new(
                provider,
                awaken_runtime_contract::credential::CredentialMaterialDescriptor::secret(
                    awaken_runtime_contract::credential::OPAQUE_SECRET_MATERIAL_TYPE,
                ),
                [
                    awaken_runtime_contract::credential::CredentialTargetContract::new(
                        CredentialTarget::new(
                            CredentialPurpose::WebProviderAuthorization,
                            provider,
                        ),
                        usage,
                    ),
                ],
            ),
            &InMemorySecretStore::new(),
            credentials.as_ref(),
        )
        .await
        .unwrap()
    }

    fn agent_policy(
        name: &str,
        configuration: serde_json::Value,
    ) -> awaken_runtime_contract::agent_bindings::ToolsetPolicy {
        awaken_runtime_contract::agent_bindings::ToolsetPolicy {
            source: awaken_runtime_contract::agent_bindings::ToolsetSource::Agent,
            default: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy::default(),
            overrides: vec![awaken_runtime_contract::agent_bindings::ToolPolicyOverride::with_optional_configuration(
                name,
                awaken_runtime_contract::agent_bindings::ToolExecutionPolicy::default(),
                Some(configuration),
            )],
        }
    }

    #[tokio::test]
    async fn publication_validation_follows_provider_semantics() {
        // Causes: C1 host provider with complete config; C2 paid host provider
        // lacks a credential pin; C3 provider-server route plus a restrictive
        // Agent execution policy. Effects: E1 unchanged frozen config; E2/E3
        // rejection before publication. R1=C1=>E1; R2=C2=>E2; R3/R4=C3 for
        // Search/Fetch=>E3. The extension's one plugin validator owns both
        // publication and runtime semantics; no credential is materialized.
        //
        // | Rule | realization | policy | Effect |
        // | R1 | host/free | none | frozen unchanged |
        // | R2 | host/paid | missing credential | reject |
        // | R3 | provider Search | user location | reject |
        // | R4 | provider Fetch | content cap | reject |
        let resolver = WebSearchPublicationResolver::new(
            awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
            Arc::new(InMemoryCredentialRepo::new()),
        );
        assert_eq!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[],
                    Some(&serde_json::json!({ "provider_id": "duckduckgo", "options": {} })),
                )
                .await,
            Ok(serde_json::json!({ "provider_id": "duckduckgo", "options": {} })),
            "R1"
        );
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[],
                    Some(&serde_json::json!({ "provider_id": "brave", "options": {} })),
                )
                .await
                .is_err(),
            "R2"
        );
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[agent_policy(
                        "web_search",
                        serde_json::json!({
                            "type": "web_search",
                            "user_location": {"country": "US"}
                        }),
                    )],
                    Some(&serde_json::json!({ "provider_id": "openrouter", "options": {} })),
                )
                .await
                .is_err(),
            "R3"
        );
        let fetch = WebFetchPublicationResolver::new(
            awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
            Arc::new(InMemoryCredentialRepo::new()),
        );
        assert!(
            fetch
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[agent_policy(
                        "web_fetch",
                        serde_json::json!({"type": "web_fetch", "max_content_tokens": 512}),
                    )],
                    Some(&serde_json::json!({ "provider_id": "openrouter", "options": {} })),
                )
                .await
                .is_err(),
            "R4"
        );
    }

    #[tokio::test]
    async fn publication_compiles_exact_web_access_and_rejects_authority_drift() {
        // Cause/effect graph: C1 exact active source revision in the publishing
        // Workspace with the provider's target/usage; C2 stale revision; C3
        // foreign Workspace; C4 inactive source; C5 provider-target mismatch.
        // Effects: E1 replace the authoring ref with one secret-free exact
        // CredentialAccess; E2 reject before a snapshot exists and publish no
        // secret/header value. Decision table: P1=C1=>E1;
        // P2=C2=>E2; P3=C3=>E2; P4=C4=>E2; P5=C5=>E2.
        //
        // | Rule | revision | workspace | state | target | Effect |
        // | P1 | exact | same | active | brave | exact access only |
        // | P2 | stale | same | active | brave | reject |
        // | P3 | exact | other | active | brave | reject |
        // | P4 | exact | same | inactive | brave | reject |
        // | P5 | exact | same | active | other | reject |
        let credentials: Arc<dyn CredentialRepo> = Arc::new(InMemoryCredentialRepo::new());
        let source = described_web_credential(credentials.clone(), "workspace-a", "brave").await;
        let resolver = WebSearchPublicationResolver::new(
            awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
            credentials.clone(),
        );
        let authored = serde_json::json!({
            "provider_id": "brave",
            "credential": {"id": source.id.0, "revision": 1},
            "options": {"safesearch": "strict"}
        });
        let published = resolver
            .resolve(
                &awaken_tenancy::ScopeId::from("workspace-a"),
                &[],
                Some(&authored),
            )
            .await
            .expect("P1");
        assert!(published.get("credential").is_none(), "P1/E1");
        let access: awaken_runtime_contract::CredentialAccess =
            serde_json::from_value(published["credential_access"].clone()).unwrap();
        assert_eq!(access.credential.revision, 1, "P1/E1");
        assert_eq!(
            access.target,
            Some(CredentialTarget::new(
                CredentialPurpose::WebProviderAuthorization,
                "brave"
            )),
            "P1/E1"
        );
        assert_eq!(access.policy.model_exposure, ModelExposurePolicy::Forbidden);
        assert!(!published.to_string().contains("never-published"));

        let mut fetch_providers = awaken_ext_builtin_tools::WebSearchProviderRegistry::default();
        fetch_providers.register_fetch(Arc::new(PaidFetch)).unwrap();
        let fetch_published =
            WebFetchPublicationResolver::new(fetch_providers, credentials.clone())
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[],
                    Some(&authored),
                )
                .await
                .expect("P1 applies identically to WebFetch");
        assert!(fetch_published.get("credential").is_none());
        assert!(fetch_published.get("credential_access").is_some());

        let mut stale = authored.clone();
        stale["credential"]["revision"] = 2.into();
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[],
                    Some(&stale)
                )
                .await
                .is_err(),
            "P2/E2"
        );
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-b"),
                    &[],
                    Some(&authored)
                )
                .await
                .is_err(),
            "P3/E2"
        );

        let mut inactive = source.clone();
        inactive.status = CredentialStatus::Disabled;
        credentials.put(inactive).await.unwrap();
        assert!(
            resolver
                .resolve(
                    &awaken_tenancy::ScopeId::from("workspace-a"),
                    &[],
                    Some(&authored)
                )
                .await
                .is_err(),
            "P4/E2"
        );

        let other_credentials: Arc<dyn CredentialRepo> = Arc::new(InMemoryCredentialRepo::new());
        let other =
            described_web_credential(other_credentials.clone(), "workspace-a", "other-provider")
                .await;
        let other_authored = serde_json::json!({
            "provider_id": "brave",
            "credential": {"id": other.id.0, "revision": 1},
            "options": {}
        });
        assert!(
            WebSearchPublicationResolver::new(
                awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins(),
                other_credentials,
            )
            .resolve(
                &awaken_tenancy::ScopeId::from("workspace-a"),
                &[],
                Some(&other_authored)
            )
            .await
            .is_err(),
            "P5/E2"
        );
    }
}
