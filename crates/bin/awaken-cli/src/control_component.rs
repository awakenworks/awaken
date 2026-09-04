//! Process adapters for the canonical Control application component.
//!
//! This module maps concrete product stores and provider adapters onto
//! `awaken-control` ports. It contains no Control business implementation.

use super::*;

fn managed_runtime_available_at_control_origin(
    role: config::Role,
    managed_services: &ManagedServiceAdapters,
) -> bool {
    role.mounts_managed_runtime() || managed_services.same_origin_managed_runtime
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn control_component_for_process(
    stores: &ProcessStores,
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    execution_workspace: &str,
    data_subject_org: &str,
    enrollment_signing_key: [u8; 32],
    executable_agent_registrar: Arc<dyn awaken_executable_agent_contract::ExecutableAgentRegistrar>,
    agent_archive_cascade: Option<Arc<dyn awaken_deployment_contract::AgentArchiveCascade>>,
    model_publication_resolver: Arc<dyn awaken_config_service::ModelPublicationResolver>,
    web_search_publication_resolver: Arc<dyn awaken_config_service::PluginPublicationResolver>,
    web_search_providers: &awaken_ext_builtin_tools::WebSearchProviderRegistry,
    brokered_client: Option<
        Arc<awaken_credential_materializer::brokered_inference::HttpBrokeredInferenceClient>,
    >,
    injected_brokered_catalog: Option<Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>>,
    model_supply: awaken_admin_config_api::ModelSupplyCapabilityView,
    managed_runtime_available: bool,
    local_acp_observations: &[awaken_acp_application::AcpHostObservation],
    runtimes: Arc<dyn awaken_config_service::RuntimeCapabilitySource>,
    resource_inventory: Option<Arc<dyn awaken_admin_assistant::ResourceInventory>>,
    environment_author: Arc<dyn awaken_environment_contract::EnvironmentAuthor>,
    environment_application: Arc<awaken_environment_application::EnvironmentApplication>,
    environment_router: Router,
    managed_tunnel_application: Option<Arc<dyn awaken_protocol_managed::ManagedTunnelApplication>>,
    inference_geo_policy: Option<Arc<dyn awaken_protocol_managed::ManagedInferenceGeoPolicy>>,
    managed_request_limiter: Option<Arc<dyn awaken_protocol_managed::ManagedRequestLimiter>>,
    credential_material_delivery: Option<awaken_credential_contract::CredentialMaterialDelivery>,
    coordinator_content_eraser: Arc<dyn awaken_runtime_contract::ContentEraser>,
    content_capture_ceiling: awaken_runtime_contract::ContentCapture,
    iam: Option<Arc<ManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    cloud_login: Option<Arc<dyn awaken_admin_config_api::CloudLoginApplication>>,
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
        service_lifecycle: service_lifecycle.clone(),
        execution_workspace: execution_workspace.to_owned(),
        data_subject_org: data_subject_org.to_owned(),
        enrollment_signing_key,
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
        plugin_publication_resolvers: vec![
            web_search_publication_resolver,
            Arc::new(
                crate::web_search_publication::WebFetchPublicationResolver::new(
                    web_search_providers.clone(),
                    control.credentials.clone(),
                ),
            ),
        ],
        credential_probe: Arc::new(credential_probe::GenaiProbe),
        model_discovery: Arc::new(awaken_control::model_discovery::GenaiModelDiscovery::new(
            control.secrets.clone(),
        )),
        brokered_catalog: injected_brokered_catalog.or_else(|| {
            brokered_client
                .map(|client| client as Arc<dyn awaken_admin_config_api::BrokeredCatalogDiscovery>)
        }),
        model_supply,
        managed_runtime_available,
        mcp_probe: Some(Arc::new(ExtMcpProbe)),
        assistant_model_selection,
        global_tools: awaken_runtime_host::authorable_tools(),
        platform_plugins: awaken_runtime_host::platform_plugin_capabilities_with_web_search(
            web_search_providers,
        ),
        assistant_plugins: awaken_runtime_host::platform_plugin_capabilities_with_web_search(
            web_search_providers,
        ),
        runtimes,
        resource_inventory,
        environment_author,
        environment_application,
        environment_router,
        managed_tunnel_application,
        inference_geo_policy,
        managed_request_limiter,
        credential_material_delivery,
        data_subjects: control.data_subjects.clone(),
        erasure_jobs: control.erasure_jobs.clone(),
        coordinator_content_eraser,
        resource_content_eraser: None,
        content_capture_ceiling,
        iam,
        local_browser_auth,
        remote_iam,
        cloud_login,
    })
    .await
}

/// Standalone Control process. It opens no Managed Execution path and
/// asks the same Control component builder used by AllInOne for the complete
/// authoring application.
pub(super) async fn prepare_control_routers(
    stores: ProcessStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    model_supply: PublicationModelSupply,
    process: ProcessStartup,
) -> Result<ProcessRouters, String> {
    debug_assert_eq!(process.role, config::Role::Control);
    let (
        _,
        executable_agent_registrar,
        _,
        _,
        coordinator_content_eraser,
        credential_rollout_target,
        _service_authenticator,
    ) = executable_agent_registration::process_parts(process.executable_agent_wiring);
    let executable_environment_registrar = process
        .executable_environment_wiring
        .map(|wiring| wiring.registrar)
        .expect("Control process requires executable Environment registrar wiring");
    let content_capture_ceiling = process.content_capture_ceiling;
    let data_subject_org = process.org_id.clone().unwrap_or_else(local_org_id);
    let enrollment_signing_key = match process.enrollment_signing_key {
        Some(key) => key,
        #[cfg(any(test, feature = "test-support"))]
        None => [0xA5; 32],
        #[cfg(not(any(test, feature = "test-support")))]
        None => panic!("Control process requires a derived enrollment signing key"),
    };
    // Store startup already resolved the one installation coordinate after all
    // owned authorities opened; Control consumes it without a second publisher.
    let execution_workspace = stores.platform_workspace.clone();
    let model_capabilities = process.model_supply.clone();
    let cloud_login = process.cloud_login.clone();
    let cloud_models_enabled = model_capabilities.cloud_models_enabled;
    let worker_observations = process
        .worker_observations
        .expect("Control process requires explicit Worker observation wiring")
        .source;
    let brokered_client = brokered_inference_client(
        cloud_models_enabled && model_supply.needs_interactive_brokered_client(),
        remote_iam.as_ref(),
        process.cloud_developer_key_file.as_deref(),
        process.cloud_api_base_url.as_deref(),
        &execution_workspace,
        &local_client_instance_id(stores.workspace_root.as_deref()),
    );
    let model_services = resolve_model_services(
        model_supply,
        &stores,
        cloud_models_enabled,
        worker_observations.clone(),
    );
    let mut web_search_providers = process
        .web_search_providers
        .unwrap_or_else(awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins);
    if let Some(client) = brokered_client.as_ref()
        && let Err(error) = client
            .install_managed_web_routes(&mut web_search_providers)
            .await
    {
        eprintln!("Awaken Cloud Web tools are unavailable: {error}");
    }
    let web_search_publication_resolver =
        process.web_search_publication_resolver.unwrap_or_else(|| {
            Arc::new(
                crate::web_search_publication::WebSearchPublicationResolver::new(
                    web_search_providers.clone(),
                    stores
                        .control
                        .as_ref()
                        .expect("Control process requires Control stores")
                        .credentials
                        .clone(),
                ),
            )
        });
    let runtimes = Arc::new(LiveRuntimeCapabilities {
        initial: process.local_acp_observations.clone(),
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
        let application = Arc::new(awaken_environment_application::EnvironmentApplication::new(
            control.environments.clone(),
            executable_environment_registrar,
            Some(control.sandbox_policies.clone()),
        ));
        Arc::new(awaken_protocol_managed::EnvironmentAuthoringState::new(
            application,
            control.sandbox_policies.clone(),
        ))
    };
    let environment_application = environment_authoring.application();
    let managed_rate_limiter = process
        .managed_services
        .request_limiter
        .clone()
        .unwrap_or_else(|| {
            Arc::new(
                awaken_protocol_managed::ManagedRateLimiter::for_organization(
                    data_subject_org.clone(),
                ),
            )
        });
    let component = control_component_for_process(
        &stores,
        &process.service_lifecycle,
        &execution_workspace,
        &data_subject_org,
        enrollment_signing_key,
        executable_agent_registrar,
        None,
        model_services.publication_resolver,
        web_search_publication_resolver,
        &web_search_providers,
        brokered_client,
        process.brokered_catalog,
        model_capabilities,
        managed_runtime_available_at_control_origin(process.role, &process.managed_services),
        &process.local_acp_observations,
        runtimes,
        None,
        environment_application.clone(),
        environment_application,
        awaken_protocol_managed::environment_authoring_router(environment_authoring.clone()).merge(
            awaken_protocol_awaken::environment_extensions_router(
                environment_authoring.application(),
                environment_authoring.sandbox_policy_store(),
            ),
        ),
        process.managed_services.tunnel_application.clone(),
        process.managed_services.inference_geo_policy.clone(),
        Some(managed_rate_limiter),
        process
            .managed_services
            .credential_material_delivery
            .clone(),
        coordinator_content_eraser.unwrap_or_else(test_coordinator_content_eraser),
        content_capture_ceiling,
        iam,
        local_browser_auth,
        remote_iam,
        cloud_login,
    )
    .await;
    // Hosted compositions may replace the transport/target mechanism while
    // preserving Control's sole durable outbox and acknowledgement authority.
    // Self-hosted split deployments keep the canonical Coordinator target.
    let credential_rollout_target = process
        .managed_services
        .credential_rollout_target
        .clone()
        .or(credential_rollout_target);
    if let Some(target) = credential_rollout_target {
        component.vault_state.set_rollout_target(target);
    }
    let webhook_delivery: Arc<dyn awaken_session_contract::LifecycleFactDelivery> = {
        let control = stores
            .control
            .as_ref()
            .expect("Control process requires Control stores");
        let webhook = awaken_webhook_managed::config_plane_lifecycle_delivery(
            control.webhooks.clone(),
            control.secrets.clone(),
            process.org_id.clone(),
        );
        match process.additional_lifecycle_delivery {
            Some(additional) => Arc::new(
                awaken_session_contract::CompositeLifecycleFactDelivery::new(vec![
                    additional, webhook,
                ]),
            ),
            None => webhook,
        }
    };
    let private_router = match process.control_service_authenticator {
        Some(authenticator) => {
            awaken_coordinator::control_service_boundary::router_with_authenticator(
                component.management_audit.clone(),
                component.vault_state.clone(),
                webhook_delivery,
                component.data_subject_resolver.clone(),
                component.organization_privacy.clone(),
                authenticator,
            )
        }
        None => Router::new(),
    };
    std::sync::Arc::new(
        crate::observation_reconcile::WorkerObservationReconcileGate::new(
            worker_observations.clone(),
        ),
    )
    .register_periodic(
        component.publication_reconciler.clone(),
        std::time::Duration::from_secs(5),
        &process.service_lifecycle,
    );
    let admin_tools = component.admin_tools.clone();
    let mcp_export = awaken_coordinator::mcp_export::router(
        awaken_admin_assistant::admin_tool_descriptors(),
        component.admin_tools,
        process.mcp_bearer_token,
    )?;
    Ok(ProcessRouters::new(
        process_surface::finish(
            component.router,
            mcp_export,
            Some(component.publication_reconciler),
            worker_observations,
            execution_workspace,
        ),
        private_router,
        Some(component.registration_supervisor),
        process.service_lifecycle,
        None,
        admin_tools,
    ))
}

#[cfg(test)]
#[test]
fn managed_runtime_capability_distinguishes_mount_from_hosted_reachability() {
    // Cause/effect graph: C1 local runtime mount and C2 an explicit hosted
    // same-origin facade independently make the canonical Coordinator surface
    // browser-reachable. Neither fact changes which Router the process mounts.
    //
    // Decision table:
    // | rule | role       | hosted facade | capability |
    // | R1   | AllInOne   | no            | true       |
    // | R2   | Control    | no            | false      |
    // | R3   | Control    | yes           | true       |
    let local = ManagedServiceAdapters::default();
    let hosted = ManagedServiceAdapters::default().with_same_origin_managed_runtime();
    assert!(managed_runtime_available_at_control_origin(
        config::Role::AllInOne,
        &local
    ));
    assert!(!managed_runtime_available_at_control_origin(
        config::Role::Control,
        &local
    ));
    assert!(managed_runtime_available_at_control_origin(
        config::Role::Control,
        &hosted
    ));
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
    let app = prepare_control_routers(
        in_memory_control_stores(),
        None,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        ProcessStartup {
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
    .expect("Control MCP exports have matching descriptors and executors")
    .public_router;
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
    // those three authoring capabilities. Constraint: ProcessStartup has
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
        let app = prepare_control_routers(
            in_memory_control_stores(),
            None,
            None,
            None,
            PublicationModelSupply::PublishedProviders,
            ProcessStartup {
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
        .expect("Control MCP exports have matching descriptors and executors")
        .public_router;
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
    // startup failure before any authoring router can return a fake success.
    // Decision rule W1 = C1+C3 -> E1. The positive configured rules are owned by
    // the adjacent standalone Control tests.
    let _ = prepare_control_routers(
        in_memory_control_stores(),
        None,
        None,
        None,
        PublicationModelSupply::PublishedProviders,
        ProcessStartup {
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
    .await
    .expect("Control MCP exports have matching descriptors and executors");
}

#[cfg(test)]
fn test_coordinator_content_eraser() -> Arc<dyn awaken_runtime_contract::ContentEraser> {
    Arc::new(awaken_captured_content_store::InMemoryCapturedContentStore::new())
}

#[cfg(not(test))]
fn test_coordinator_content_eraser() -> Arc<dyn awaken_runtime_contract::ContentEraser> {
    panic!("split Control requires Coordinator content-erasure adapter")
}
