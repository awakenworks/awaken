//! Host-side wiring for the management assistant (ADR-0052): the real adapters behind
//! the admin tools' ports, plus the startup seeding that publishes the assistant as an
//! ordinary agent in the reserved scope.
//!
//! The assistant is authored, compiled, and published like any agent (D1) — the
//! seeding here is exactly a `put` + `publish` through `ConfigService`, in the reserved
//! scope (D2), where the scope-keyed catalog makes the four admin tools nameable (D3).

use awaken_admin_assistant::{
    ADMIN_ASSISTANT_AGENT_ID, CapabilityReader, DraftValidator, PlatformCapabilities, PluginInfo,
    admin_assistant_config,
};
use awaken_config_store::AgentConfig;
use awaken_model_catalog::ProviderCatalog;
use awaken_runtime_contract::capability::PluginCapability;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_host::{ConfigPlane, RESERVED_ADMIN_SCOPE};
use awaken_tenancy::ScopeId;

/// Publish the management assistant into the reserved scope through the ordinary
/// publish path (D1/D2), via the scope edge ([`ConfigPlane`]). Idempotent —
/// re-seeding recompiles to the same content address. Returns the publish error
/// verbatim so a caller can surface a bad setup (e.g. no provider-backed model).
pub async fn seed_admin_assistant(plane: &ConfigPlane) -> Result<(), String> {
    let scope = ScopeId::from(RESERVED_ADMIN_SCOPE);
    plane.put(&scope, &admin_assistant_config()).await?;
    plane
        .publish(&scope, ADMIN_ASSISTANT_AGENT_ID)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Reads the redacted, org-shared capability snapshot (D4) from a provider-catalog
/// snapshot, the advertised (global) tool ids, and the platform plugin capabilities.
/// Carries only ids/names — never a key, endpoint, or header.
pub struct CatalogCapabilityReader {
    models: Vec<String>,
    providers: Vec<String>,
    tools: Vec<String>,
    plugins: Vec<PluginInfo>,
}

impl CatalogCapabilityReader {
    pub fn new(
        catalog: &ProviderCatalog,
        global_tools: &[ToolDescriptor],
        plugins: &[PluginCapability],
    ) -> Self {
        // Deduplicate model ids across offerings, keep catalog order.
        let mut models = Vec::new();
        for offering in &catalog.offerings {
            if !models.contains(&offering.model_id) {
                models.push(offering.model_id.clone());
            }
        }
        Self {
            models,
            providers: catalog.providers.keys().cloned().collect(),
            tools: global_tools.iter().map(|d| d.id.clone()).collect(),
            plugins: plugins
                .iter()
                .map(|p| PluginInfo {
                    id: p.id.clone(),
                    schema_keys: p.schema_keys.clone(),
                })
                .collect(),
        }
    }
}

impl CapabilityReader for CatalogCapabilityReader {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            agents: Vec::new(),
            models: self.models.clone(),
            providers: self.providers.clone(),
            tools: self.tools.clone(),
            plugins: self.plugins.clone(),
            skills: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }
}

/// Validates a drafted config through the ordinary config-plane validate in a target
/// scope (the tenant/default scope — a draft is an ordinary agent, so it is validated
/// against the global catalog, and naming an admin tool in a draft is correctly
/// rejected). This is the same check `/v1/config/agents/validate` runs. It holds the
/// scope edge ([`ConfigPlane`]) so the scope-free service stays untouched.
pub struct ConfigServiceDraftValidator {
    plane: ConfigPlane,
    scope: ScopeId,
}

impl ConfigServiceDraftValidator {
    pub fn new(plane: ConfigPlane, scope: impl Into<ScopeId>) -> Self {
        Self {
            plane,
            scope: scope.into(),
        }
    }
}

impl DraftValidator for ConfigServiceDraftValidator {
    fn validate(&self, draft: &AgentConfig) -> Result<(), String> {
        self.plane.validate(&self.scope, draft)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_config_store::{DEFAULT_SCOPE, ModelSelection, SqliteConfigStore};
    use awaken_model_catalog::{ApiDialect, Offering, ProtocolEndpointId, Provider, ProviderId};
    use awaken_runtime_host::{ConfigPlane, ConfigService, ScopedToolCatalog, StaticToolCatalog};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::pinned("t", id, "d", serde_json::json!({"type": "object"}))
    }

    fn catalog(model: &str) -> ProviderCatalog {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_string(),
            Provider {
                id: ProviderId::new("anthropic"),
                slug: "anthropic".into(),
                display_name: "Anthropic".into(),
                version: 1,
            },
        );
        ProviderCatalog {
            providers,
            offerings: vec![Offering {
                model_id: model.to_string(),
                provider_id: ProviderId::new("anthropic"),
                protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
                dialect: ApiDialect::AnthropicMessages,
                upstream_model: None,
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn seeding_publishes_the_assistant_into_the_reserved_scope_only() {
        // A scope-keyed catalog + a resolver so the Auto assistant can publish.
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let tools = Arc::new(ScopedToolCatalog::new(
            Vec::new(),
            RESERVED_ADMIN_SCOPE,
            awaken_admin_assistant::admin_tool_descriptors(),
        ));
        let service = Arc::new(ConfigService::new().with_model_resolver(Arc::new(
            crate::model_resolver::CatalogModelResolver::new(catalog("m-1")),
        )));
        let plane = ConfigPlane::new(service.clone(), store, tools);

        seed_admin_assistant(&plane).await.expect("seed");

        // Published + installed under the reserved id, model auto-resolved.
        let installed = service
            .installed(ADMIN_ASSISTANT_AGENT_ID)
            .expect("installed");
        let spec = &installed.snapshot().resolved_spec;
        assert_eq!(spec.model_binding.model_ref, "m-1");
        // It carries the four admin tool descriptors (nameable because it published in
        // the reserved scope).
        assert_eq!(spec.tool_descriptors.len(), 4);
    }

    #[test]
    fn capability_reader_reports_only_ids_no_secrets() {
        let reader = CatalogCapabilityReader::new(&catalog("m-1"), &[tool("read")], &[]);
        let caps = reader.capabilities();
        assert_eq!(caps.models, vec!["m-1"]);
        assert_eq!(caps.tools, vec!["read"]);
        assert_eq!(caps.providers, vec!["anthropic"]);
        let json = serde_json::to_string(&caps).unwrap();
        assert!(!json.contains("key") && !json.contains("secret"));
    }

    #[tokio::test]
    async fn draft_validator_rejects_an_unknown_tool_in_a_tenant_draft() {
        let store = Arc::new(SqliteConfigStore::open_in_memory().unwrap());
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new()),
            store,
            Arc::new(StaticToolCatalog(vec![tool("read")])),
        );
        let validator = ConfigServiceDraftValidator::new(plane, DEFAULT_SCOPE);

        let mut good = admin_assistant_config();
        good.model_binding = ModelSelection::pinned("p", "m", "b");
        good.tool_ids = vec!["read".to_string()];
        assert!(validator.validate(&good).is_ok());

        let mut bad = good.clone();
        bad.tool_ids = vec!["ghost".to_string()];
        assert!(validator.validate(&bad).is_err());
    }
}
