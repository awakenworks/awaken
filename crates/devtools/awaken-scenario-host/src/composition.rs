//! Shared scenario composition helpers.
//!
//! This module owns the one test-only assembly path for resource catalogs,
//! Environment state, and immutable backend-owned Agent publications. Keeping
//! these together prevents individual ACP scenarios from recreating Managed
//! state or publication projections.

use std::sync::Arc;

use awaken_runtime_contract::agent_bindings::AgentBindings;
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use awaken_runtime_contract::{PublishedAgentSnapshotSource, StaticPublishedAgentSnapshots};
use axum::Router;

use super::{
    EchoModel, SharedHost, files_router, memory_stores_router_with_catalog, resource_host,
    scenario_resource_catalog, skills_router,
};

/// Scenario equivalent of the production composition root: one secret-free
/// Resource Catalog is shared by the Memory API, Managed ACL, and runtime
/// activation. Authorization remains outside this helper.
pub(super) fn mount(host: Arc<SharedHost>) -> Router {
    let catalog = scenario_resource_catalog();
    let managed = awaken_server::local_managed_state(host.clone(), catalog.clone());
    awaken_server::mount_with_managed_and_resource_catalog(host, managed, catalog)
}

fn mount_with_agent_source(
    host: Arc<SharedHost>,
    agent_source: Arc<dyn awaken_protocol_managed::AgentConfigSource>,
) -> Router {
    let catalog = scenario_resource_catalog();
    let managed = awaken_server::local_managed_state_with_agent_source(
        host.clone(),
        catalog.clone(),
        agent_source,
    );
    awaken_server::mount_with_managed_and_resource_catalog(host, managed, catalog)
}

/// Scenario composition with the Environment API and the same Resource Catalog,
/// credential plane, and Session repository used by [`mount`].
pub(super) fn mount_with_environments(host: Arc<SharedHost>) -> Router {
    mount_with_environments_and_agent_source(host, None)
}

pub(super) fn mount_with_environments_and_agent_source(
    host: Arc<SharedHost>,
    agent_source: Option<Arc<dyn awaken_protocol_managed::AgentConfigSource>>,
) -> Router {
    let catalog = scenario_resource_catalog();
    let environments = Arc::new(
        awaken_protocol_managed::EnvironmentState::new().with_sandbox_policies(Arc::new(
            awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default(),
        )),
    );
    let managed = match agent_source {
        Some(source) => awaken_server::local_managed_state_with_environments_and_agent_source(
            host.clone(),
            catalog.clone(),
            environments.clone(),
            source,
        ),
        None => awaken_server::local_managed_state_with_environments(
            host.clone(),
            catalog.clone(),
            environments.clone(),
        ),
    };
    awaken_server::mount_with_managed_and_resource_catalog(host, managed, catalog)
        .merge(awaken_protocol_managed::environments_router(environments))
}

pub(super) struct FixedAgentPublication {
    snapshots: StaticPublishedAgentSnapshots,
}

impl FixedAgentPublication {
    fn host_backend(id: &str, backend_ref: &str, skill_ids: Vec<String>) -> Self {
        // Cause graph: deterministic scenario launch -> host-installed executor ->
        // HostExecutor placement. BackendOwned instead implies a discovered,
        // revision-pinned WorkerLocal identity, which these fake CLIs do not own.
        //
        // Decision table:
        // S1 fixed scenario launch -> HostExecutor, no credential observation
        // S2 product local CLI     -> BackendOwned + exact WorkerLocal observation
        let snapshot = ExecutableAgentSnapshot::builder(id)
            .resolved_model(ResolvedModelCandidate::host(ModelBinding::new(
                "",
                "",
                backend_ref,
            )))
            .agent_bindings(AgentBindings {
                skill_ids,
                ..Default::default()
            })
            .build();
        Self {
            snapshots: StaticPublishedAgentSnapshots::try_new([snapshot])
                .expect("valid fixed scenario Agent publication"),
        }
    }
}

pub(super) fn fixed_host_backend_publication(
    id: &str,
    backend_ref: &str,
    skill_ids: Vec<String>,
) -> Arc<FixedAgentPublication> {
    Arc::new(FixedAgentPublication::host_backend(
        id,
        backend_ref,
        skill_ids,
    ))
}

/// Install one immutable Agent as the sole backend authority for deterministic
/// ACP scenarios. Keeping this assembly in one place prevents scenario tests
/// from reviving metadata-based runtime selection as a second source of truth.
pub(super) fn mount_with_host_backend_publication(
    host: SharedHost,
    agent_id: &str,
    backend_ref: &str,
) -> Router {
    let publication = fixed_host_backend_publication(agent_id, backend_ref, Vec::new());
    let host = host.with_agent_publications(publication.clone());
    mount_with_agent_source(Arc::new(host), publication)
}

impl PublishedAgentSnapshotSource for FixedAgentPublication {
    fn current(&self, workspace: &str, agent_id: &AgentId) -> Option<ExecutableAgentSnapshot> {
        self.snapshots.current(workspace, agent_id)
    }

    fn exact(
        &self,
        workspace: &str,
        fingerprint: &awaken_runtime_contract::resolved::CatalogFingerprint,
    ) -> Option<ExecutableAgentSnapshot> {
        self.snapshots.exact(workspace, fingerprint)
    }
}

impl awaken_protocol_managed::AgentConfigSource for FixedAgentPublication {
    fn agent_view_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_protocol_managed::AgentConfigView> {
        let snapshot = self.current(workspace_id, &AgentId(agent_id.to_string()))?;
        Some(awaken_protocol_managed::AgentConfigView {
            model: Some(snapshot.resolved_spec.model_binding.model_ref.clone()),
            backend_ref: snapshot.resolved_spec.model_binding.backend_ref.clone(),
            system: None,
            tool_ids: Vec::new(),
            toolsets: Vec::new(),
            client_tools: Vec::new(),
            mcp_servers: Vec::new(),
            skill_ids: snapshot.resolved_spec.plugin_config.agent.skill_ids.clone(),
            delegate_ids: Vec::new(),
            resources: Vec::new(),
            environment: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::ModelProvisioning;

    #[test]
    fn fixed_scenario_backend_uses_the_installed_host_executor() {
        let publication = fixed_host_backend_publication("acp-agent", "acp:claude", Vec::new());
        let snapshot = publication
            .current("workspace", &AgentId("acp-agent".into()))
            .expect("fixed publication");
        assert!(matches!(
            snapshot.resolved_spec.model_binding.provisioning,
            ModelProvisioning::HostExecutor
        ));
        assert_eq!(
            snapshot.resolved_spec.model_binding.binding.backend_ref,
            "acp:claude"
        );
    }
}

/// Resource HTTP adapters without the product composition root's local Workspace
/// injector. This intentionally incomplete test composition proves that File,
/// MemoryStore, and Skill routes fail closed instead of deriving a Workspace from
/// the Host. Production always supplies either the local default-scope layer or an
/// authenticated PEP before these routers.
pub fn build_unscoped_resource_router() -> Router {
    let host = Arc::new(resource_host(Arc::new(EchoModel), "unscoped-resource"));
    let purge: Arc<dyn awaken_protocol_managed::resource_plane::ResourcePurgeScheduler> =
        host.clone();
    Router::new()
        .merge(files_router(host.clone()))
        .merge(memory_stores_router_with_catalog(
            host.memory_repository(),
            scenario_resource_catalog(),
            purge.clone(),
        ))
        .merge(skills_router(host.skill_store(), purge))
}
