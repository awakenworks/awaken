//! Canonical Scenario platform startup.
//!
//! This module owns the one test-only Scenario startup path for Resource catalogs,
//! Environment state, and immutable backend-owned Agent publications. Keeping
//! these together prevents individual ACP scenarios from recreating Managed
//! state or publication projections.

use std::sync::Arc;

use awaken_runtime_contract::agent_bindings::AgentBindings;
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::snapshot::{AgentId, ExecutableAgentSnapshot};
use awaken_runtime_contract::{PublishedAgentSnapshotSource, StaticPublishedAgentSnapshots};
use axum::Router;

use super::{EchoModel, SharedHost, resource_host};
use crate::deployment::ScenarioPlatform;

pub(super) fn test_environment_components() -> (
    Arc<awaken_protocol_managed::EnvironmentAuthoringState>,
    Arc<awaken_environment_execution_application::EnvironmentExecutionApplication>,
) {
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;

    let work: Arc<dyn awaken_session_contract::work_queue::WorkQueue> =
        Arc::new(awaken_work_store::InMemoryWorkQueue::new());
    let executable =
        Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
    executable
        .install_seed(awaken_environment_application::default_environment_registration())
        .expect("install built-in Environment");
    let registrar: Arc<dyn ExecutableEnvironmentRegistrar> = Arc::new(
        awaken_environment_execution_application::CoordinatorEnvironmentRegistrar::new(
            Arc::new(
                awaken_executable_environment_catalog::LocalExecutableEnvironmentRegistrar::new(
                    executable.clone(),
                ),
            ),
            work.clone(),
        ),
    );
    let policies =
        Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
    let application = Arc::new(awaken_environment_application::EnvironmentApplication::new(
        Arc::new(awaken_env_store::InMemoryEnvRegistry::new()),
        registrar,
        Some(policies.clone()),
    ));
    let authoring = awaken_protocol_managed::EnvironmentAuthoringState::new(application, policies);
    (
        Arc::new(authoring),
        Arc::new(
            awaken_environment_execution_application::EnvironmentExecutionApplication::new(
                work, executable,
            ),
        ),
    )
}

/// Scenario equivalent of the production service wiring: one secret-free
/// Resource Registry is shared by the Memory API, Managed ACL, and runtime
/// activation. Authorization remains outside this helper.
pub(super) fn mount(platform: ScenarioPlatform) -> Router {
    let (host, resources) = platform.into_parts();
    mount_parts(Arc::new(host), resources)
}

pub(super) fn mount_parts(
    host: Arc<SharedHost>,
    resources: awaken_resource_application::ResourcesApplication,
) -> Router {
    let catalog = resources.authorities().resource_registry();
    let managed = awaken_coordinator::local_managed_state(host.clone(), catalog.clone());
    awaken_coordinator::mount_with_managed_and_resource_registry_and_dreams(host, managed, catalog)
        .0
}

pub(super) fn mount_parts_with_model_publication_resolver(
    host: Arc<SharedHost>,
    resources: awaken_resource_application::ResourcesApplication,
    resolver: Arc<dyn awaken_session_contract::SessionModelPublicationResolver>,
) -> Router {
    let catalog = resources.authorities().resource_registry();
    let managed = awaken_coordinator::local_managed_state_with_model_publication_resolver(
        host.clone(),
        catalog.clone(),
        resolver.clone(),
    );
    let (router, dreams) = awaken_coordinator::mount_with_managed_and_resource_registry_and_dreams(
        host, managed, catalog,
    );
    dreams.bind_model_readiness(Arc::new(ScenarioDreamModelReadiness { resolver }));
    router
}

struct ScenarioDreamModelReadiness {
    resolver: Arc<dyn awaken_session_contract::SessionModelPublicationResolver>,
}

#[async_trait::async_trait]
impl awaken_dream_application::DreamModelReadiness for ScenarioDreamModelReadiness {
    async fn is_ready(&self, workspace_id: &str, model_id: &str) -> Result<bool, String> {
        let publication = match self
            .resolver
            .resolve_session_model(workspace_id, model_id)
            .await
        {
            Ok(publication) => publication,
            Err(awaken_session_contract::SessionModelResolutionError::Invalid(_)) => {
                return Ok(false);
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(!matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(
                &publication.primary.binding.backend_ref
            ),
            awaken_runtime_contract::resolved::Backend::Remote { .. }
        ))
    }
}

pub(super) fn mount_with_agent_source(
    platform: ScenarioPlatform,
    agent_source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
) -> Router {
    let (host, resources) = platform.into_parts();
    let host = Arc::new(host);
    let catalog = resources.authorities().resource_registry();
    let managed = awaken_coordinator::local_managed_state_with_agent_source(
        host.clone(),
        catalog.clone(),
        agent_source,
    );
    awaken_coordinator::mount_with_managed_and_resource_registry(host, managed, catalog)
}

/// Scenario platform with the Environment API and the same Resource Registry,
/// credential plane, and Session repository used by [`mount`].
pub(super) fn mount_with_environments(platform: ScenarioPlatform) -> Router {
    mount_with_environments_and_agent_source(platform, None)
}

pub(super) fn mount_with_environments_and_agent_source(
    platform: ScenarioPlatform,
    agent_source: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
) -> Router {
    let (host, resources) = platform.into_parts();
    let host = Arc::new(host);
    let catalog = resources.authorities().resource_registry();
    let (environment_authoring, environment_execution) = test_environment_components();
    let managed = match agent_source {
        Some(source) => awaken_coordinator::local_managed_state_with_environments_and_agent_source(
            host.clone(),
            catalog.clone(),
            environment_execution.clone(),
            source,
        ),
        None => awaken_coordinator::local_managed_state_with_environments(
            host.clone(),
            catalog.clone(),
            environment_execution.clone(),
        ),
    };
    awaken_coordinator::mount_with_managed_and_resource_registry(host, managed, catalog)
        .merge(awaken_protocol_managed::environment_authoring_router(
            environment_authoring.clone(),
        ))
        .merge(awaken_protocol_managed::environment_work_router(
            environment_execution,
        ))
        .merge(awaken_protocol_awaken::environment_extensions_router(
            environment_authoring.application(),
            environment_authoring.sandbox_policy_store(),
        ))
}

pub(super) struct FixedAgentPublication {
    snapshots: StaticPublishedAgentSnapshots,
    resources: Vec<awaken_resource_contract::InputBinding>,
}

impl FixedAgentPublication {
    fn host_backend(
        id: &str,
        backend_ref: &str,
        skills: Vec<awaken_agent_contract::AgentSkillBinding>,
    ) -> Self {
        Self::host_backend_with_acp_mcp(id, backend_ref, skills, Vec::new())
    }

    fn host_backend_with_acp_mcp(
        id: &str,
        backend_ref: &str,
        skills: Vec<awaken_agent_contract::AgentSkillBinding>,
        mcp_servers: Vec<awaken_runtime_contract::resolved::AcpMcpServer>,
    ) -> Self {
        // Cause graph: deterministic scenario launch -> host-installed executor ->
        // HostExecutor placement. BackendOwned instead implies a discovered,
        // revision-pinned WorkerLocal identity, which these fake CLIs do not own.
        //
        // Decision table:
        // S1 fixed scenario launch -> HostExecutor, no credential observation
        // S2 product local CLI     -> BackendOwned + exact WorkerLocal observation
        let plugin_config = awaken_runtime_contract::resolved::AcpSpec {
            compact_window: None,
            mcp_servers,
        }
        .into_plugin_config(Default::default());
        let snapshot = ExecutableAgentSnapshot::builder(id)
            .resolved_model(ResolvedModelCandidate::host(ModelBinding::new(
                "",
                "",
                backend_ref,
            )))
            .agent_bindings(AgentBindings {
                skills,
                ..Default::default()
            })
            .plugin_config(plugin_config)
            .build();
        Self {
            snapshots: StaticPublishedAgentSnapshots::try_new([snapshot])
                .expect("valid fixed scenario Agent publication"),
            resources: Vec::new(),
        }
    }

    fn host_backend_with_mcp(
        id: &str,
        backend_ref: &str,
        skills: Vec<awaken_agent_contract::AgentSkillBinding>,
        mcp_servers: Vec<awaken_runtime_contract::agent_bindings::AgentMcpServerBinding>,
    ) -> Self {
        let toolsets = mcp_servers
            .iter()
            .map(|server| awaken_agent_contract::ToolsetPolicy {
                source: awaken_agent_contract::ToolsetSource::Mcp {
                    server_name: server.name.clone(),
                },
                default: awaken_agent_contract::ToolExecutionPolicy::default(),
                overrides: Vec::new(),
            })
            .collect();
        let snapshot = ExecutableAgentSnapshot::builder(id)
            .resolved_model(ResolvedModelCandidate::host(ModelBinding::new(
                "",
                "",
                backend_ref,
            )))
            .agent_bindings(AgentBindings {
                skills,
                mcp_servers,
                toolsets,
                ..Default::default()
            })
            .build();
        Self {
            snapshots: StaticPublishedAgentSnapshots::try_new([snapshot])
                .expect("valid fixed scenario Agent publication"),
            resources: Vec::new(),
        }
    }
}

/// Install the deterministic Memory probe through the same immutable Agent
/// publication path used by production. The published `/memory` slot is a
/// replaceable identity: the Managed Session attachment supplies the actual
/// Store while retaining this binding id for the plugin configuration.
pub(super) fn mount_with_memory_publication(
    platform: ScenarioPlatform,
    model_ref: &str,
    skills: Vec<awaken_agent_contract::AgentSkillBinding>,
) -> Router {
    let snapshot = ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(ResolvedModelCandidate::host(ModelBinding::new(
            "scenario", model_ref, "default",
        )))
        .plugins([awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()])
        .plugin_config(std::collections::BTreeMap::from([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            serde_json::json!({ "binding_id": "memory" }),
        )]))
        .agent_bindings(AgentBindings {
            skills,
            ..Default::default()
        })
        .build();
    let publication = Arc::new(FixedAgentPublication {
        snapshots: StaticPublishedAgentSnapshots::try_new([snapshot])
            .expect("valid fixed Memory scenario Agent publication"),
        resources: vec![awaken_resource_contract::InputBinding {
            binding_id: awaken_resource_contract::BindingId::from("memory"),
            target: awaken_resource_contract::InputResourceId::MemoryStore(
                awaken_resource_contract::MemoryStoreId::from("scenario-memory-placeholder"),
            ),
            mount_path: "/memory".into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        }],
    });
    let platform = platform.map_host(|host| host.with_agent_publications(publication.clone()));
    mount_with_agent_source(platform, publication)
}

pub(super) fn fixed_host_backend_publication_with_mcp(
    id: &str,
    backend_ref: &str,
    skills: Vec<awaken_agent_contract::AgentSkillBinding>,
    mcp_servers: Vec<awaken_runtime_contract::agent_bindings::AgentMcpServerBinding>,
) -> Arc<FixedAgentPublication> {
    Arc::new(FixedAgentPublication::host_backend_with_mcp(
        id,
        backend_ref,
        skills,
        mcp_servers,
    ))
}

pub(super) fn fixed_host_backend_publication_with_acp_mcp(
    id: &str,
    backend_ref: &str,
    skills: Vec<awaken_agent_contract::AgentSkillBinding>,
    mcp_servers: Vec<awaken_runtime_contract::resolved::AcpMcpServer>,
) -> Arc<FixedAgentPublication> {
    Arc::new(FixedAgentPublication::host_backend_with_acp_mcp(
        id,
        backend_ref,
        skills,
        mcp_servers,
    ))
}

pub(super) fn fixed_host_backend_publication(
    id: &str,
    backend_ref: &str,
    skills: Vec<awaken_agent_contract::AgentSkillBinding>,
) -> Arc<FixedAgentPublication> {
    Arc::new(FixedAgentPublication::host_backend(id, backend_ref, skills))
}

pub(super) fn fixed_host_model_publication(
    id: &str,
    primary: ResolvedModelCandidate,
    candidates: Vec<ResolvedModelCandidate>,
) -> Arc<FixedAgentPublication> {
    let snapshot = ExecutableAgentSnapshot::builder(id)
        .resolved_model(primary)
        .model_candidates(candidates.into_iter().map(|candidate| candidate.binding))
        .build();
    Arc::new(FixedAgentPublication {
        snapshots: StaticPublishedAgentSnapshots::try_new([snapshot])
            .expect("valid fixed scenario model publication"),
        resources: Vec::new(),
    })
}

/// Install one immutable Agent as the sole backend authority for deterministic
/// ACP scenarios. Keeping this platform preparation in one place prevents scenario tests
/// from reviving metadata-based runtime selection as a second source of truth.
pub(super) fn mount_with_host_backend_publication(
    platform: ScenarioPlatform,
    agent_id: &str,
    backend_ref: &str,
) -> Router {
    let publication = fixed_host_backend_publication(agent_id, backend_ref, Vec::new());
    let platform = platform.map_host(|host| host.with_agent_publications(publication.clone()));
    mount_with_agent_source(platform, publication)
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

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for FixedAgentPublication {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        let snapshot = self.current(workspace_id, &AgentId(agent_id.to_string()))?;
        let bindings = &snapshot.resolved_spec.plugin_config.agent;
        let mcp_servers = bindings
            .mcp_servers
            .iter()
            .map(|server| {
                Ok(awaken_executable_agent_contract::ExecutableAgentMcpServer {
                    name: server.name.clone(),
                    target: server.transport.normalize()?,
                    prompts_as_skills: server.prompts_as_skills,
                    credential_source_id: server
                        .credential
                        .as_ref()
                        .map(|credential| credential.id.clone()),
                    credential_revision: server
                        .credential
                        .as_ref()
                        .map(|credential| credential.revision),
                })
            })
            .collect::<Result<Vec<_>, String>>()
            .ok()?;
        Some(
            awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                name: None,
                description: None,
                source_revision: snapshot.metadata.source.revision,
                model: Some(snapshot.resolved_spec.model_binding.model_ref.clone()),
                inference: snapshot.resolved_spec.plugin_config.inference.clone(),
                execution_model_ref: Some(snapshot.resolved_spec.model_binding.model_ref.clone()),
                backend_ref: snapshot.resolved_spec.model_binding.backend_ref.clone(),
                system: None,
                tool_ids: Vec::new(),
                toolsets: bindings.toolsets.clone(),
                client_tools: Vec::new(),
                mcp_servers,
                skills: bindings.skills.clone(),
                delegates: Vec::new(),
                advisor_model: bindings
                    .advisor
                    .as_ref()
                    .map(|advisor| advisor.model.clone()),
                resources: self.resources.clone(),
                environment: None,
            },
        )
    }
}

/// Resource HTTP adapters without the product service wiring's local Workspace
/// injector. This intentionally incomplete Scenario platform proves that File,
/// MemoryStore, and Skill routes fail closed instead of deriving a Workspace from
/// the Host. Production always supplies either the local default-scope layer or an
/// authenticated PEP before these routers.
pub fn build_unscoped_resource_router() -> Router {
    let platform = resource_host(Arc::new(EchoModel), "unscoped-resource");
    let (_host, resources) = platform.into_parts();
    let authorities = resources.authorities();
    awaken_protocol_managed::resources_router(awaken_protocol_managed::ResourcesRouterInput {
        files: resources.files(),
        memories: authorities.memory_repository(),
        memory_stores: resources.memory_stores(),
        skills: Some(authorities.skill_store()),
        purge: resources.purge_scheduler(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::resolved::{
        AcpMcpServer, AcpMcpTransport, AcpSpec, ModelProvisioning,
    };

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

    #[test]
    fn fixed_publication_preserves_a_secret_free_stdio_mcp_route() {
        let publication = fixed_host_backend_publication_with_acp_mcp(
            "acp-agent",
            "acp:fixture",
            Vec::new(),
            vec![AcpMcpServer {
                name: "playwright".into(),
                transport: AcpMcpTransport::Stdio {
                    command: "playwright-mcp".into(),
                    args: vec!["--headless".into()],
                },
            }],
        );
        let snapshot = publication
            .current("workspace", &AgentId("acp-agent".into()))
            .expect("fixed publication");
        let servers =
            AcpSpec::from_plugin_config(snapshot.resolved_spec.plugin_config.plugins()).mcp_servers;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "playwright");
        assert!(matches!(
            &servers[0].transport,
            AcpMcpTransport::Stdio { command, args }
                if command == "playwright-mcp" && args == &["--headless"]
        ));
    }

    #[test]
    fn fixed_native_mcp_publication_projects_server_and_permission_toolset() {
        // Decision rule N1: a fixed Native publication with one sandbox-stdio
        // binding exposes that server through the canonical executable profile
        // and grants the matching MCP toolset; no legacy Agent view participates.
        let publication = fixed_host_backend_publication_with_mcp(
            "native-agent",
            "native",
            Vec::new(),
            vec![
                awaken_runtime_contract::agent_bindings::AgentMcpServerBinding {
                    name: "playwright".into(),
                    transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::sandbox_stdio(
                        "playwright-mcp",
                        vec!["--headless".into()],
                    ),
                    credential: None,
                    prompts_as_skills: false,
                },
            ],
        );
        let view =
            awaken_executable_agent_contract::ExecutableAgentProfileSource::session_profile_in(
                publication.as_ref(),
                "workspace",
                "native-agent",
            )
            .expect("fixed publication view");
        assert_eq!(view.mcp_servers.len(), 1);
        assert_eq!(view.toolsets.len(), 1);
        assert_eq!(
            view.toolsets[0].default.permission,
            awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow
        );
    }
}
