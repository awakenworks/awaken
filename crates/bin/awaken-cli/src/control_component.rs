//! Process adapters for the canonical Control application component.
//!
//! This module maps concrete product stores and provider adapters onto
//! `awaken-control` ports. It contains no Control business implementation.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn control_component_for_process(
    stores: &ProcessStores,
    execution_workspace: &str,
    executable_agent_registrar: Arc<dyn awaken_executable_agent_contract::ExecutableAgentRegistrar>,
    agent_archive_cascade: Option<Arc<dyn awaken_protocol_managed::AgentArchiveCascade>>,
    model_publication_resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
    web_search_providers: &awaken_ext_builtin_tools::WebSearchProviderRegistry,
    brokered_client: Option<
        Arc<awaken_coordinator::brokered_inference::HttpBrokeredInferenceClient>,
    >,
    injected_brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    model_supply: awaken_admin_config_api::ModelSupplyCapabilityView,
    managed_runtime: bool,
    local_acp_observations: &[awaken_acp_application::AcpHostObservation],
    runtimes: Arc<dyn awaken_config_service::RuntimeCapabilitySource>,
    resource_inventory: Option<Arc<dyn awaken_admin_assistant::ResourceInventory>>,
    environment_author: Arc<dyn awaken_admin_assistant::EnvironmentAuthor>,
    environment_application: Arc<awaken_environment_application::EnvironmentApplication>,
    environment_router: Router,
    coordinator_content_eraser: Arc<dyn awaken_runtime_contract::ContentEraser>,
    content_capture_ceiling: awaken_runtime_contract::ContentCapture,
    iam: Option<Arc<ManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
) -> awaken_control::ControlComponent {
    let control = stores
        .control
        .as_ref()
        .expect("Control process requires Control stores");
    let assistant_catalog = control.catalog.snapshot().await.unwrap_or_default();
    let assistant_credentials = control
        .credentials
        .list(execution_workspace)
        .await
        .unwrap_or_default();
    let assistant_model_selection = assistant_selection::select(
        &assistant_catalog,
        &assistant_credentials,
        local_acp_observations,
    );
    awaken_control::build_control_component(awaken_control::ControlDependencies {
        execution_workspace: execution_workspace.to_owned(),
        catalog: control.catalog.clone(),
        credentials: control.credentials.clone(),
        secrets: control.secrets.clone(),
        profiles: control.profiles.clone(),
        webhook_store: control.webhooks.clone(),
        resource_store: control.resources.clone(),
        config_store: control.config.clone(),
        executable_agent_registrar,
        agent_archive_cascade,
        model_publication_resolver,
        plugin_publication_resolvers: vec![web_search_publication_resolver],
        credential_probe: Arc::new(credential_probe::GenaiProbe),
        model_discovery: Arc::new(
            awaken_coordinator::model_discovery::GenaiModelDiscovery::new(control.secrets.clone()),
        ),
        brokered_catalog: injected_brokered_catalog.or_else(|| {
            brokered_client
                .map(|client| client as Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>)
        }),
        model_supply,
        managed_runtime,
        mcp_probe: Some(Arc::new(ExtMcpProbe)),
        assistant_model_selection,
        global_tools: awaken_runtime_host::authorable_tools(),
        platform_plugins: awaken_runtime_host::platform_plugin_capabilities_with_web_search(
            web_search_providers,
        ),
        assistant_plugins: awaken_runtime_host::authorable_config_sections_with_web_search(
            web_search_providers,
        ),
        runtimes,
        resource_inventory,
        environment_author,
        environment_application,
        environment_router,
        data_subjects: control.data_subjects.clone(),
        erasure_jobs: control.erasure_jobs.clone(),
        coordinator_content_eraser,
        resource_content_eraser: None,
        content_capture_ceiling,
        iam,
        local_browser_auth,
        remote_iam,
    })
    .await
}

/// Standalone Control process assembly. It opens no Managed Execution path and
/// asks the same Control component builder used by AllInOne for the complete
/// authoring application.
pub(super) async fn assemble_control_process_router(
    stores: ProcessStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    model_composition: PublicationModelComposition,
    assembly: ProcessAssemblyOptions,
) -> ProcessRouterAssembly {
    debug_assert_eq!(assembly.role, config::Role::Control);
    let (_, executable_agent_registrar, _, _, coordinator_content_eraser) =
        executable_agent_registration::process_parts(assembly.executable_agent_wiring);
    let executable_environment_registrar = assembly
        .executable_environment_wiring
        .map(|wiring| wiring.registrar)
        .expect("Control process requires executable Environment registrar wiring");
    let content_capture_ceiling = assembly.content_capture_ceiling;
    let execution_workspace = stores.workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let model_supply = assembly.model_supply.clone();
    let cloud_models_enabled = model_supply.cloud_models_enabled;
    let worker_observations = assembly
        .worker_observations
        .expect("Control process requires explicit Worker observation wiring")
        .source;
    let brokered_client = brokered_inference_client(
        cloud_models_enabled,
        remote_iam.as_ref(),
        assembly.cloud_api_base_url.as_deref(),
        &execution_workspace,
    );
    let model_assembly = publication_model_assembly(
        model_composition,
        &stores,
        cloud_models_enabled,
        worker_observations.clone(),
    );
    let web_search_providers = assembly
        .web_search_providers
        .unwrap_or_else(awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins);
    let web_search_publication_resolver =
        assembly.web_search_publication_resolver.unwrap_or_else(|| {
            Arc::new(awaken_config_service::WebSearchPublicationResolver::new(
                web_search_providers.clone(),
            ))
        });
    let runtimes = Arc::new(LiveRuntimeCapabilities {
        initial: assembly.local_acp_observations.clone(),
        workers: worker_observations.clone(),
        credentials: stores
            .control
            .as_ref()
            .expect("Control process requires Control stores")
            .credentials
            .clone(),
        workspace: execution_workspace.clone(),
    });
    let environment_authoring = {
        let control = stores
            .control
            .as_ref()
            .expect("Control process requires Control stores");
        Arc::new(awaken_protocol_managed::EnvironmentAuthoringState::new(
            control.environments.clone(),
            control.sandbox_policies.clone(),
            executable_environment_registrar,
        ))
    };
    let environment_application = environment_authoring.application();
    let component = control_component_for_process(
        &stores,
        &execution_workspace,
        executable_agent_registrar,
        None,
        model_assembly.publication_resolver,
        web_search_publication_resolver,
        &web_search_providers,
        brokered_client,
        assembly.brokered_catalog,
        model_supply,
        assembly.role.exposes_managed_runtime(),
        &assembly.local_acp_observations,
        runtimes,
        None,
        Arc::new(
            awaken_environment_application::EnvironmentApplicationAuthor::new(
                environment_authoring.application(),
            ),
        ),
        environment_application,
        awaken_protocol_managed::environment_authoring_router(environment_authoring.clone()).merge(
            awaken_protocol_awaken::environment_extensions_router(
                environment_authoring.application(),
                environment_authoring.sandbox_policy_store(),
            ),
        ),
        coordinator_content_eraser.unwrap_or_else(test_coordinator_content_eraser),
        content_capture_ceiling,
        iam,
        local_browser_auth,
        remote_iam,
    )
    .await;
    let webhook_delivery = {
        let control = stores
            .control
            .as_ref()
            .expect("Control process requires Control stores");
        awaken_webhook_managed::config_plane_lifecycle_delivery(
            control.webhooks.clone(),
            control.secrets.clone(),
            assembly.org_id.clone(),
        )
    };
    let router = match assembly.control_service_token.as_deref() {
        Some(token) => component.router.merge(
            awaken_coordinator::control_service_boundary::router(
                component.management_audit.clone(),
                component.vault_state.clone(),
                webhook_delivery,
                component.data_subject_consent.clone(),
                token,
            )
            .unwrap_or_else(|error| panic!("build Control service boundary: {error}")),
        ),
        None => component.router,
    };
    std::sync::Arc::new(
        crate::observation_reconcile::WorkerObservationReconcileGate::new(
            worker_observations.clone(),
        ),
    )
    .spawn_periodic(
        component.publication_reconciler.clone(),
        std::time::Duration::from_secs(5),
    );
    let mcp_export = awaken_coordinator::mcp_export::router(
        awaken_admin_assistant::admin_tool_descriptors(),
        component.admin_tools,
        assembly.mcp_bearer_token,
    );
    ProcessRouterAssembly::new(
        process_surface::finish(
            router,
            mcp_export,
            Some(component.publication_reconciler),
            worker_observations,
            execution_workspace,
            Arc::new(
                awaken_protocol_managed::ManagedRateLimiter::for_organization(
                    assembly.org_id.unwrap_or_else(local_org_id),
                ),
            ),
        ),
        Some(component.registration_supervisor),
    )
}

#[cfg(test)]
#[tokio::test]
async fn standalone_control_uses_the_authored_capture_ceiling() {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt as _;

    // Causes: C1 standalone Control has no Runtime deployment object, C2 its
    // authored ceiling is Off, C3 the request asks for Full, C4 the subject is
    // absent. Effects: E1 the endpoint is available, E2 effective capture is Off.
    // Constraint: consent may only narrow the authored ceiling.
    // Decision rule R1 = C1+C2+C3+C4 -> E1+E2. This pins the policy input that
    // standalone Control must carry independently of Runtime deployment state.
    let app = assemble_control_process_router(
        in_memory_control_stores(),
        None,
        None,
        None,
        PublicationModelComposition::PublishedProviders,
        ProcessAssemblyOptions {
            role: config::Role::Control,
            content_capture_ceiling: awaken_runtime_contract::ContentCapture::Off,
            executable_environment_wiring: Some(
                executable_environment_registration::local_test_wiring(),
            ),
            executable_agent_wiring: Some(
                executable_agent_registration::ExecutableAgentWiring::local(),
            ),
            worker_observations: Some(worker_observation_wiring::WorkerObservationWiring::local(
                awaken_coordinator::test_worker_directory(),
            )),
            ..Default::default()
        },
    )
    .await
    .router;
    let response = app
        .oneshot(
            Request::get("/v1/user_profiles/unknown/capture-decision?requested=full")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["effective"], "off", "R1 + R2");
}

#[cfg(test)]
#[tokio::test]
async fn standalone_control_projects_the_exact_model_supply_posture() {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt as _;

    // Cause/effect graph: C1 posture={local, hosted}; C2 both use the same
    // canonical Control component. Effects: E1 local exposes catalog/BYOK/profile
    // authoring without Cloud models; E2 hosted exposes Cloud models and denies
    // those three authoring capabilities. Constraint: ProcessAssemblyOptions has
    // one ModelSupplyCapabilityView source of truth and no parallel mode flag.
    // Decision table: R1 C1=local+C2 -> E1; R2 C1=hosted+C2 -> E2.
    let cases = [
        (
            "R1",
            awaken_admin_config_api::ModelSupplyCapabilityView::default(),
        ),
        (
            "R2",
            awaken_admin_config_api::ModelSupplyCapabilityView {
                local_catalog_enabled: false,
                byok_enabled: false,
                cloud_models_enabled: true,
                profile_authoring_enabled: false,
            },
        ),
    ];

    for (rule, expected) in cases {
        let app = assemble_control_process_router(
            in_memory_control_stores(),
            None,
            None,
            None,
            PublicationModelComposition::PublishedProviders,
            ProcessAssemblyOptions {
                role: config::Role::Control,
                model_supply: expected.clone(),
                executable_environment_wiring: Some(
                    executable_environment_registration::local_test_wiring(),
                ),
                executable_agent_wiring: Some(
                    executable_agent_registration::ExecutableAgentWiring::local(),
                ),
                worker_observations: Some(
                    worker_observation_wiring::WorkerObservationWiring::local(
                        awaken_coordinator::test_worker_directory(),
                    ),
                ),
                ..Default::default()
            },
        )
        .await
        .router;
        let response = app
            .oneshot(
                Request::get("/v1/config/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body["models"],
            serde_json::to_value(expected).unwrap(),
            "{rule}"
        );
    }
}

#[cfg(test)]
#[tokio::test]
#[should_panic(expected = "Control process requires executable Environment registrar wiring")]
async fn standalone_control_rejects_missing_environment_registration_wiring() {
    // Cause/effect graph: C1 role=Control; C2 executable Agent wiring is explicit;
    // C3 executable Environment wiring is absent. Effect E1 is a
    // composition failure before any authoring router can return a fake success.
    // Decision rule W1 = C1+C3 -> E1. The positive configured rules are owned by
    // the adjacent standalone Control tests.
    let _ = assemble_control_process_router(
        in_memory_control_stores(),
        None,
        None,
        None,
        PublicationModelComposition::PublishedProviders,
        ProcessAssemblyOptions {
            role: config::Role::Control,
            executable_agent_wiring: Some(
                executable_agent_registration::ExecutableAgentWiring::local(),
            ),
            worker_observations: Some(worker_observation_wiring::WorkerObservationWiring::local(
                awaken_coordinator::test_worker_directory(),
            )),
            ..Default::default()
        },
    )
    .await;
}

#[cfg(test)]
fn test_coordinator_content_eraser() -> Arc<dyn awaken_runtime_contract::ContentEraser> {
    Arc::new(awaken_captured_content_store::InMemoryCapturedContentStore::new())
}

#[cfg(not(test))]
fn test_coordinator_content_eraser() -> Arc<dyn awaken_runtime_contract::ContentEraser> {
    panic!("split Control requires Coordinator content-erasure adapter")
}
