// @formal surface exclude=test_only
// @formal owner crates/control/awaken-control/src/managed_agents.rs
// @formal reason: Static owner: managed-agent test fixtures. Dynamic path: the module is reachable
// @formal reason: only through the exact #[cfg(test)] declaration in managed_agents.rs and never
// @formal reason: enters the product graph. Evidence:
// @formal reason: crates/control/awaken-control/src/managed_agents.rs.
//! Fixtures shared by the managed-agent tests.
//!
//! `create_params` and the `plane*` builders are used by tests in this module's
//! parent and in `roster`, so they live here rather than in either test module:
//! a second copy would drift, and moving the tests to sit beside one copy would
//! separate them from the behavior they verify.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_agent_config::ModelSelection;
use awaken_config_service::{
    ConfigService, ModelPublicationResolver, ResolvedPublicationModels, StaticToolCatalog,
};
use awaken_config_store::SqliteConfigStore;
use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
use awaken_protocol_managed::types::agent::{AgentCreateParams, AgentUpdateParams, ModelInput};
use awaken_runtime_contract::agent_bindings::InferenceGeography;
use awaken_runtime_contract::resolved::{
    InferenceEndpoint, InferencePlacement, InferencePlacementMechanism, ModelBinding,
    ResolvedModelCandidate, ToolDescriptor, ToolKind,
};
use serde_json::json;

use super::ConfigPlane;

pub(super) struct TestModelResolver;
pub(super) struct RejectModelResolver;
#[async_trait::async_trait]
impl ModelPublicationResolver for TestModelResolver {
    async fn resolve_models(
        &self,
        workspace: &awaken_tenancy::ScopeId,
        selection: &ModelSelection,
        candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, awaken_config_service::PublicationResolutionError> {
        let primary = selection
            .resolved()
            .cloned()
            .or_else(|| {
                selection.target().map(|(target, backend_ref)| {
                    ModelBinding::new(
                        target.provider_id.as_deref().unwrap_or("test-provider"),
                        &target.model_id,
                        backend_ref,
                    )
                })
            })
            .ok_or_else(|| "test requires a pinned model".to_string())?;
        let resolved = |binding: ModelBinding| {
            let model = binding.identity().1.to_owned();
            ResolvedModelCandidate::try_provider(
                binding,
                "test-provider",
                format!("test-route:{model}"),
                workspace.clone(),
                None,
                InferenceEndpoint {
                    adapter_kind: "anthropic_messages".into(),
                    api_dialect: "anthropic_messages".into(),
                    base_url: "https://provider.example.test".into(),
                    upstream_model: model,
                    processing_placement: Some(InferencePlacement {
                        geography: InferenceGeography::Us,
                        mechanism: InferencePlacementMechanism::AnthropicRequestBody,
                    }),
                },
            )
        };
        Ok(ResolvedPublicationModels {
            primary: resolved(primary).map_err(|error| error.to_string())?,
            candidates: candidates
                .iter()
                .cloned()
                .map(resolved)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?,
            context_window: None,
            max_output_tokens: None,
        })
    }
}
#[async_trait::async_trait]
impl ModelPublicationResolver for RejectModelResolver {
    async fn resolve_models(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        _selection: &ModelSelection,
        _candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, awaken_config_service::PublicationResolutionError> {
        Err(awaken_config_service::PublicationResolutionError::MissingPrimary)
    }
}
pub(super) fn create_params(name: &str) -> AgentCreateParams {
    AgentCreateParams {
        name: name.into(),
        model: ModelInput::Id("model-a".into()),
        description: None,
        system: Some("be helpful".into()),
        metadata: BTreeMap::new(),
        mcp_servers: vec![
            serde_json::from_value(json!({
                "type": "url",
                "name": "docs",
                "url": "https://mcp.example.test"
            }))
            .unwrap(),
        ],
        skills: vec![
            serde_json::from_value(json!({
                "type": "custom",
                "skill_id": "skill-docs"
            }))
            .unwrap(),
        ],
        tools: vec![
            serde_json::from_value(json!({
                "type": "mcp_toolset",
                "mcp_server_name": "docs"
            }))
            .unwrap(),
        ],
        multiagent: None,
    }
}
pub(super) fn update_params(version: u64) -> AgentUpdateParams {
    AgentUpdateParams {
        version: Some(version),
        name: Some("renamed".into()),
        model: None,
        description: None,
        system: None,
        metadata: None,
        mcp_servers: None,
        skills: None,
        tools: None,
        multiagent: None,
    }
}
pub(super) async fn plane(path: &str) -> ConfigPlane {
    plane_with_catalog(path).await.0
}
pub(super) async fn plane_with_catalog(path: &str) -> (ConfigPlane, Arc<ExecutableAgentCatalog>) {
    plane_with_store(Arc::new(
        SqliteConfigStore::open(path).await.expect("config store"),
    ))
}

pub(super) fn plane_with_store(
    store: Arc<dyn awaken_agent_config::ScopedConfigRegistry>,
) -> (ConfigPlane, Arc<ExecutableAgentCatalog>) {
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    (
        ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(TestModelResolver),
                Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                Vec::new(),
            )),
            store,
            Arc::new(StaticToolCatalog(Vec::new())),
        ),
        catalog,
    )
}
pub(super) async fn plane_with_delegation(path: &str) -> ConfigPlane {
    plane_with_delegation_and_catalog(path).await.0
}
pub(super) async fn plane_with_delegation_and_catalog(
    path: &str,
) -> (ConfigPlane, Arc<ExecutableAgentCatalog>) {
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    (
        ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(TestModelResolver),
                Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                Vec::new(),
            )),
            Arc::new(SqliteConfigStore::open(path).await.expect("config store")),
            Arc::new(StaticToolCatalog(vec![
                ToolDescriptor::pinned(
                    "managed",
                    "agent_run",
                    "Run an exact roster Agent",
                    json!({"type": "object"}),
                )
                .with_kind(ToolKind::AgentDelegation),
            ])),
        ),
        catalog,
    )
}
pub(super) async fn rejecting_plane(path: &str) -> ConfigPlane {
    ConfigPlane::new(
        Arc::new(ConfigService::new(
            Arc::new(RejectModelResolver),
            Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
                ExecutableAgentCatalog::new(),
            ))),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            Vec::new(),
        )),
        Arc::new(SqliteConfigStore::open(path).await.expect("config store")),
        Arc::new(StaticToolCatalog(Vec::new())),
    )
}
