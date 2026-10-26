// @formal entrypoint assembly_role.local_managed_mount_requires_exact_local_worker_component
// @formal entrypoint executable_projection.runtime_admission_requires_both_refreshes
// @formal evidence resource_reclamation.physical_purge_requires_zero_references_leases_and_fence
//! Coordinator/AllInOne process startup over canonical domain components.

use super::*;

use cloud_plane::{CloudPlane, assemble_cloud_plane};

mod cloud_plane;
mod local_execution;
mod reclamation;
mod startup;
mod surfaces;

#[cfg(test)]
mod route_inventory;
#[cfg(test)]
mod tests;

fn broker_router(
    transport: &worker_transport_security::WorkerTransport,
    directory: &awaken_coordinator::WorkerDirectoryHandle,
    clock: &Arc<dyn awaken_time::WallClock>,
) -> Option<Router> {
    #[cfg(feature = "container-k8s")]
    {
        transport.create_broker_router(directory.clone(), clock.clone())
    }
    #[cfg(not(feature = "container-k8s"))]
    {
        let _ = (transport, directory, clock);
        None
    }
}

fn resource_management_router(
    resources: &awaken_resource_application::ResourcesApplication,
    credentials: Arc<dyn awaken_resource_contract::RepositoryCredentialValidator>,
) -> Router {
    awaken_coordinator::resources_router(awaken_coordinator::ResourcesRouterInput::new(
        resources,
        credentials,
    ))
}

/// Assemble the runtime process, one stage per component family, in the order
/// the components depend on each other: process facts, Deployment and Resources,
/// cloud/model/credential plane, Control, execution stores, execution Host,
/// Session application, Coordinator, then the protected route surfaces.
pub(super) async fn prepare_runtime_routers(
    stores: ProcessStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    model_supply: PublicationModelSupply,
    process: ProcessStartup,
    // An optional last-mile hook on the prepared data-plane host, applied before it is
    // shared. The process startup uses it to supply a runtime backend the standard process
    // does not own (e.g. an ACP executor for `acp:*` threads) without this
    // module naming that backend's crate. `None` in production; `Some` in a scenario that
    // serves external-CLI sessions.
    customize_host: Option<
        Box<dyn FnOnce(SessionExecutorBuilder) -> SessionExecutorBuilder + Send>,
    >,
) -> Result<ProcessRouters, String> {
    let mut base = open_process_base(stores, iam, remote_iam, process).await?;
    let resources = assemble_deployment_and_resources(&mut base).await?;
    let mut cloud = assemble_cloud_plane(&mut base, model_supply).await?;
    let control_component =
        assemble_control_component(&base, &resources, &cloud, local_browser_auth).await;
    let execution = assemble_execution_stores(&mut base, &resources).await?;
    let mut control = assemble_control_surface(&mut base, control_component)?;
    let mut host = assemble_execution_host(
        &mut base,
        &resources,
        &mut cloud,
        &control,
        &execution,
        customize_host,
    )
    .await?;
    let session = assemble_session_plane(
        &mut base,
        &resources,
        &cloud,
        &mut control,
        &execution,
        &mut host,
    )?;
    let event_batch_cutover_validation = session.event_batch_cutover_validation.clone();
    let coordinator = assemble_coordinator(
        &mut base, &resources, &mut cloud, &control, &execution, session,
    )
    .await?;
    Ok(finish_process_routers(
        base,
        &resources,
        &execution,
        control,
        coordinator,
        event_batch_cutover_validation,
    )
    .await)
}

/// Process-scope facts every assembly stage reads: the opened stores, the
/// startup selection, the role and the wiring the startup validated.
struct ProcessBase {
    stores: ProcessStores,
    process: ProcessStartup,
    role: config::Role,
    org_id: String,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    managed_rate_limiter: Arc<dyn awaken_tenancy::org_admission::OrgRequestAdmission>,
    runtime_authority: Option<Arc<dyn awaken_dispatch_erasure::RuntimeAuthority>>,
    placement: awaken_session_application::SessionExecutionPlacement,
    worker_directory: awaken_coordinator::WorkerDirectoryHandle,
    worker_transport: worker_transport_security::WorkerTransport,
    worker_observations: worker_observation_wiring::WorkerObservationWiring,
    agents: executable_agent_registration::ExecutableAgentWiring,
    environments: executable_environment_registration::ExecutableEnvironmentWiring,
    enrollment_signing_key: [u8; 32],
}

async fn open_process_base(
    stores: ProcessStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    process: ProcessStartup,
) -> Result<ProcessBase, String> {
    let startup::RuntimeStartup {
        mut process,
        worker_directory,
        worker_observation_wiring,
        worker_transport,
        executable_agent_wiring,
        executable_environment_wiring,
        enrollment_signing_key,
    } = startup::prepare(&stores, process)?;
    let role = process.role;
    let runtime_authority = selected_runtime_authority(&mut process, &stores).await?;
    let placement = if process
        .deployment
        .as_ref()
        .is_some_and(|deployment| deployment.disable_local_pool)
    {
        awaken_session_application::SessionExecutionPlacement::RegisteredWorker
    } else {
        awaken_session_application::SessionExecutionPlacement::LocalWorker
    };
    let org_id = process.org_id.take().unwrap_or_else(local_org_id);
    let managed_rate_limiter = process
        .managed_services
        .request_limiter
        .clone()
        .unwrap_or_else(|| {
            Arc::new(awaken_authz_enforce::OrgRateLimiter::for_organization(
                org_id.clone(),
                stores.clock.clone(),
            ))
        });
    Ok(ProcessBase {
        stores,
        process,
        role,
        org_id,
        iam,
        remote_iam,
        managed_rate_limiter,
        runtime_authority,
        placement,
        worker_directory,
        worker_transport,
        worker_observations: worker_observation_wiring,
        agents: executable_agent_wiring,
        environments: executable_environment_wiring,
        enrollment_signing_key,
    })
}

/// The Runtime authority the process selected. Scenario/test startup selects
/// one complete reference authority before either Host or SessionApplication
/// can observe Dispatch; a Host-only fallback would give them different
/// authority views.
async fn selected_runtime_authority(
    process: &mut ProcessStartup,
    stores: &ProcessStores,
) -> Result<Option<Arc<dyn awaken_dispatch_erasure::RuntimeAuthority>>, String> {
    #[cfg(any(test, feature = "test-support"))]
    if process.runtime_authority.is_none() {
        let selected = process
            .deployment
            .clone()
            .unwrap_or_else(awaken_deployment_config::DeploymentConfig::ephemeral);
        process.runtime_authority = Some(
            crate::scenario_runtime_authority(
                &selected.coordinator_storage(),
                stores.clock.clone(),
            )
            .await
            .map_err(|error| error.to_string())?,
        );
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = stores;
    Ok(process.runtime_authority.take())
}

/// The Deployment aggregate and the Resources authorities every sibling
/// component shares.
struct DeploymentAndResources {
    deployment_application: Arc<awaken_deployment_application::DeploymentApplication>,
    projection_refresh:
        Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
    content_eraser: Arc<dyn awaken_data_governance_contract::ContentEraser>,
    resource_application: awaken_resource_application::ResourcesApplication,
    memory_invalidator: Arc<awaken_resource_contract::LocalInvalidator>,
    resource_authorities: awaken_resource_application::ResourceAuthorities,
    resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
}

async fn assemble_deployment_and_resources(
    base: &mut ProcessBase,
) -> Result<DeploymentAndResources, String> {
    // Cause/effect ownership rule: one selected ResourceAuthorities value is moved intact
    // into the Host. The management Skill API borrows the one additional view it
    // needs; no tuple decomposition or parallel Resources reconstruction.
    let coordinator_stores = base
        .stores
        .coordinator
        .as_ref()
        .expect("Managed Execution role requires Coordinator stores");
    // Restore the Coordinator-owned Deployment aggregate exactly once before
    // sibling components are prepared. AllInOne Agent lifecycle commands and
    // the Coordinator router/scheduler receive this same instance.
    let deployment_application = awaken_coordinator::restore_deployment_application(
        coordinator_stores.deployments.clone(),
        base.stores.clock.clone(),
    )
    .await
    .map_err(|error| format!("restore Deployment state: {error}"))?;
    let projection_refresh = executable_projection_refresh::shared(
        base.agents.projection_refresher.take(),
        base.environments.projection_refresher.take(),
    )?;
    let content_eraser = awaken_coordinator::data_subject_boundary::coordinator_content_eraser(
        coordinator_stores.captured_content_eraser.clone(),
        base.process
            .deployment
            .as_ref()
            .and_then(|deployment| deployment.acp_session_blob_root.clone())
            .map(|root| {
                Arc::new(awaken_run_executor_acp::FsSessionBlobStore::new(root))
                    as Arc<dyn awaken_data_governance_contract::ContentEraser>
            }),
    );
    let resource_application = coordinator_stores.resources.application.clone();
    let memory_invalidator = coordinator_stores.resources.invalidator.clone();
    let resource_authorities = resource_application.authorities();
    let resource_registry = resource_authorities.resource_registry();
    Ok(DeploymentAndResources {
        deployment_application,
        projection_refresh,
        content_eraser,
        resource_application,
        memory_invalidator,
        resource_authorities,
        resource_registry,
    })
}

/// Control owns this complete component. Standalone Control and AllInOne
/// supply different process adapters but call the same domain builder;
/// Coordinator never constructs ConfigService or a Control router.
async fn assemble_control_component(
    base: &ProcessBase,
    resources: &DeploymentAndResources,
    cloud: &CloudPlane,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
) -> Option<awaken_control::ControlComponent> {
    let environment_authoring = match base.role {
        config::Role::AllInOne => {
            let control = base
                .stores
                .control
                .as_ref()
                .expect("AllInOne owns Control Environment stores");
            Some(Arc::new(control.environments.application(
                base.environments.registrar.clone(),
                control.sandbox_policies.clone(),
            )))
        }
        config::Role::Coordinator => None,
        config::Role::Control | config::Role::Worker => unreachable!(),
    };
    match base.role {
        config::Role::AllInOne => {
            let authoring = environment_authoring
                .as_ref()
                .expect("AllInOne owns Environment authoring");
            Some(
                control_component_for_process(
                    &base.stores,
                    &base.process.service_lifecycle,
                    &base.stores.platform_workspace,
                    &base.org_id,
                    base.enrollment_signing_key,
                    base.agents.registrar.clone(),
                    Some(resources.deployment_application.clone()
                        as Arc<dyn awaken_deployment_contract::AgentArchiveCascade>),
                    cloud
                        .model_services
                        .as_ref()
                        .expect("AllInOne configures model publication")
                        .publication_resolver
                        .clone(),
                    cloud
                        .web_search_publication_resolver
                        .clone()
                        .expect("AllInOne requires a Web publication resolver"),
                    &cloud.web_search_providers,
                    cloud.brokered_client.clone(),
                    base.process.brokered_catalog.clone(),
                    base.process.model_supply.clone(),
                    base.role.mounts_managed_runtime(),
                    &base.process.local_acp_observations,
                    cloud
                        .live_runtime_capabilities
                        .clone()
                        .expect("AllInOne configures Control runtime capabilities"),
                    Some(Arc::new(awaken_control::HostResourceInventory::new(
                        resources.resource_authorities.resource_registry(),
                        resources.resource_authorities.skill_store(),
                    ))),
                    authoring.clone(),
                    authoring.clone(),
                    awaken_protocol_managed::environment_authoring_router(authoring.clone()).merge(
                        awaken_protocol_awaken::environment_extensions_router(
                            authoring.clone(),
                            Some(
                                base.stores
                                    .control
                                    .as_ref()
                                    .expect("AllInOne owns Control Environment stores")
                                    .sandbox_policies
                                    .clone(),
                            ),
                        ),
                    ),
                    base.process.managed_services.tunnel_application.clone(),
                    base.process.managed_services.inference_geo_policy.clone(),
                    Some(base.managed_rate_limiter.clone()),
                    base.process
                        .managed_services
                        .credential_material_delivery
                        .clone(),
                    resources.content_eraser.clone(),
                    base.process.content_capture_ceiling,
                    base.iam.clone(),
                    local_browser_auth,
                    base.remote_iam.clone(),
                    base.process.cloud_login.clone(),
                )
                .await,
            )
        }
        config::Role::Coordinator => None,
        config::Role::Control | config::Role::Worker => {
            unreachable!("runtime process accepts only AllInOne or Coordinator")
        }
    }
}

/// The Coordinator-owned stores an execution process builds over: Session,
/// application capabilities, Dream, extraction work and the Environment work
/// queue. They share the exact durable store opened by process startup; split
/// Control neither mirrors it nor owns a fallback token directory.
struct ExecutionStores {
    sessions: Arc<dyn awaken_session_repository_contract::SessionStore>,
    application_access: Arc<awaken_coordinator::application_access_store::ApplicationAccessStore>,
    dream_process_store: Arc<dyn awaken_session_dream_contract::DreamProcessStore>,
    capture_sink: Arc<dyn awaken_runtime_contract::CaptureSink>,
    environment_execution:
        Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository>,
}

async fn assemble_execution_stores(
    base: &mut ProcessBase,
    resources: &DeploymentAndResources,
) -> Result<ExecutionStores, String> {
    let CoordinatorStores {
        resources: _,
        sessions,
        application_access,
        deployments: _,
        dream_process_store,
        memory_extractions,
        capture_sink,
        captured_content_eraser: _,
        environment_work,
    } = base
        .stores
        .coordinator
        .take()
        .expect("Managed Execution role requires Coordinator stores");
    let environment_execution =
        awaken_environment_application::execution::EnvironmentExecutionApplication::new(
            environment_work,
            base.environments.catalog.clone(),
        );
    let environment_execution = match base.process.cloud_native_credential_realization.take() {
        Some(profile) => environment_execution.with_cloud_native_credential_realization(profile),
        None => environment_execution,
    };
    let environment_execution = match base.environments.image_builds.as_ref() {
        Some(builds) => environment_execution.with_image_readiness(builds.clone()),
        None => environment_execution,
    };
    let indexed_memory_extractions =
        Arc::new(awaken_coordinator::ReferenceIndexedMemoryExtractions::new(
            memory_extractions,
            resources.resource_authorities.reclamation(),
        ));
    indexed_memory_extractions
        .synchronize_recoverable_references()
        .await
        .map_err(|error| format!("restore Memory extraction references: {error}"))?;
    Ok(ExecutionStores {
        sessions,
        application_access,
        dream_process_store,
        capture_sink,
        environment_execution: Arc::new(environment_execution),
        memory_extractions: indexed_memory_extractions,
    })
}

/// The Control component's contribution to the execution process. A split
/// Coordinator has none of Control's routers and reaches its services over the
/// authenticated private boundary instead.
struct ControlSurface {
    router: Router,
    mcp_export: Router,
    reconciler: Option<Arc<dyn awaken_config_service::PublicationBindingReconciler>>,
    admin_execs: Vec<Arc<dyn awaken_tool_contract::RawTool>>,
    credential_source: Arc<dyn awaken_credential_contract::SessionCredentialSource>,
    repository_credentials: Arc<dyn awaken_resource_contract::RepositoryCredentialValidator>,
    audit_plane: ManagementAuditPlane,
    webhook_stores: Option<(
        Arc<dyn awaken_config_resolver::WebhookStore>,
        Arc<dyn awaken_credential_vault::SecretStore>,
    )>,
    capture_policy: awaken_data_governance_contract::CapturePolicySources,
    registration_supervisor: Option<Arc<awaken_control::StaticRegistrationSupervisor>>,
    vault_state: Option<Arc<awaken_protocol_managed::VaultState>>,
}

fn assemble_control_surface(
    base: &mut ProcessBase,
    component: Option<awaken_control::ControlComponent>,
) -> Result<ControlSurface, String> {
    match component {
        Some(component) => {
            let control_stores = base
                .stores
                .control
                .as_ref()
                .expect("AllInOne Control component has Control stores");
            let mcp_export = awaken_coordinator::mcp_export::router(
                awaken_admin_assistant::admin_tool_descriptors(),
                component.admin_tools.clone(),
                base.process.mcp_bearer_token.take(),
            )?;
            Ok(ControlSurface {
                router: component.router,
                mcp_export,
                reconciler: Some(component.publication_reconciler),
                admin_execs: component.admin_tools,
                credential_source: component.vault_state.application().credential_access()
                    as Arc<dyn awaken_credential_contract::SessionCredentialSource>,
                repository_credentials: component.repository_credentials.clone(),
                audit_plane: component.management_audit,
                webhook_stores: Some((
                    control_stores.webhooks.clone(),
                    control_stores.secrets.clone(),
                )),
                capture_policy: component.capture_policy,
                registration_supervisor: Some(component.registration_supervisor),
                vault_state: Some(component.vault_state),
            })
        }
        None => {
            let services = base
                .process
                .control_service
                .clone()
                .expect("split Coordinator requires Control services");
            Ok(ControlSurface {
                router: Router::new(),
                mcp_export: Router::new(),
                reconciler: None,
                admin_execs: Vec::new(),
                credential_source: services.credentials,
                repository_credentials: services.repository_credentials,
                audit_plane: ManagementAuditPlane::from_repository(services.audit),
                webhook_stores: None,
                capture_policy: services.capture_policy,
                registration_supervisor: None,
                vault_state: None,
            })
        }
    }
}

/// The execution Host and the Session persistence it records Environment
/// bindings through.
struct ExecutionHost {
    host: Arc<awaken_session_executor::ExecutorComponents>,
    /// Consumed once by the Session application that shares these exact handles.
    session_persistence: Option<awaken_session_application::SessionPersistence>,
    artifact_fence: Option<Arc<awaken_coordinator::ClaimFencedArtifactPublisher>>,
}

/// The data plane: the host runs the server model, resolves a session's agent to its
/// installed config, and carries the management tool executables so the reserved-scope
/// assistant can call them. It shares the SAME skill store and Resource Registry the
/// capability inventory reads, so a skill or memory store the host serves is exactly
/// what the assistant enumerates, and identity survives a restart.
async fn assemble_execution_host(
    base: &mut ProcessBase,
    resources: &DeploymentAndResources,
    cloud: &mut CloudPlane,
    control: &ControlSurface,
    execution: &ExecutionStores,
    customize_host: Option<
        Box<dyn FnOnce(SessionExecutorBuilder) -> SessionExecutorBuilder + Send>,
    >,
) -> Result<ExecutionHost, String> {
    let clock = base.stores.clock.clone();
    let runtime_authority = base
        .runtime_authority
        .take()
        .ok_or_else(|| "execution process requires its selected Runtime authority".to_string())?;
    let session_persistence = awaken_session_application::SessionPersistence::new(
        execution.sessions.clone(),
        clock.clone(),
    );
    // The process keeps the Deployment it resolved: the execution placement
    // below installs the Session environment and ACP routes from it.
    let deployment = base
        .process
        .deployment
        .take()
        .ok_or_else(|| "runtime process requires a resolved deployment".to_string())?;
    let host_builder = SessionExecutorBuilder::for_coordinator(
        cloud.model_wiring.executor.clone(),
        cloud.model_wiring.model_ref.clone(),
        awaken_session_executor::WorkerContentAdapters {
            file_content_source: resources.resource_application.file_content_source(),
            memory: resources.resource_authorities.memory_execution_content(),
        },
        execution.memory_extractions.clone(),
        awaken_session_executor::SessionExecutionProcess::from_deployment(
            &deployment,
            base.stores.platform_workspace.clone(),
            clock.clone(),
        )?,
        runtime_authority.clone(),
    );
    let host_builder =
        host_builder.with_environment_binding_sink(session_persistence.environment_binding_sink());
    let local_dispatch = Some(runtime_authority.dispatch_store());
    let artifact_publisher = resources.resource_application.artifact_publisher();
    let artifact_fence = local_dispatch.map(|dispatch| {
        Arc::new(awaken_coordinator::ClaimFencedArtifactPublisher::new(
            artifact_publisher.clone(),
            dispatch,
            clock.clone(),
        ))
    });
    let artifact_publisher = artifact_fence
        .as_ref()
        .map_or(artifact_publisher, |publisher| {
            publisher.clone()
                as Arc<
                    dyn awaken_resource_contract::ArtifactPublisher<
                            awaken_session_realization_contract::ResourceOperationFence,
                        >,
                >
        });
    let mut host_builder = host_builder
        .with_artifact_publisher(artifact_publisher)
        .with_skill_bundle_source(resources.resource_application.skill_bundle_source())
        .with_skill_catalog_application(resources.resource_application.skill_catalog_application())
        .with_web_search_provider_registry(cloud.web_search_providers.clone())
        .with_acp_tool_exporter(Arc::new(awaken_protocol_mcp::SessionToolExporter))
        .with_remote_attempt_executor(crate::a2a_attempt_executor(
            cloud.credential_materializer.clone(),
        ))
        .with_agent_publications(base.agents.catalog.clone())
        .with_capture_sink(execution.capture_sink.clone())
        .with_capture_policy_sources(control.capture_policy.clone())
        .with_admin_tools(control.admin_execs.clone());
    if let Some(credentials) = cloud.credential_materializer.clone() {
        host_builder = host_builder.with_credential_materializer(credentials);
    }
    if let Some(materializer) = cloud.model_wiring.materializer.take() {
        host_builder = host_builder.with_inference_materializer(materializer);
    }
    let host_builder = local_execution::prepare(
        host_builder,
        base.placement,
        base.process.colocated_execution.take(),
        crate::ColocatedExecutionInputs {
            deployment: deployment.clone(),
            namespace_hand_path: base.process.namespace_hand_path.take(),
            credentials: cloud.credential_materializer.clone(),
            memory_source: resources.resource_authorities.memory_mount_source(),
            memory_invalidator: resources.memory_invalidator.clone(),
        },
    )
    .await?;
    // Last-mile backend wiring the standard process does not own, supplied
    // by the process startup (a scenario that serves external-CLI sessions).
    let host_builder = match customize_host {
        Some(customize) => customize(host_builder),
        None => host_builder,
    };
    let repository_bindings = Arc::new(
        awaken_resource_application::RegistryRepositoryBindingVerifier::new(
            resources.resource_registry.clone(),
        ),
    );
    let mut session_execution = awaken_session_executor::SessionExecutionDependencies::default()
        .with_resource_validator(resources.resource_registry.clone())
        .with_repository_binding_verifier(repository_bindings);
    if let Some(factory) = cloud.credential_refresh_factory.take() {
        session_execution = session_execution.with_credential_refresh_factory(factory);
    }
    let host = host_builder
        .with_session_execution(session_execution)
        .build()
        .map_err(|error| format!("construct execution Host: {error}"))?;
    Ok(ExecutionHost {
        host,
        session_persistence: Some(session_persistence),
        artifact_fence,
    })
}

/// The Session application, the Managed state over it and the lifecycle
/// delivery they share.
struct SessionPlane {
    /// The executor as the Coordinator consumes it, selected once the
    /// Session application is bound into it.
    execution: awaken_coordinator::ExecutionPorts,
    session_application: Arc<awaken_session_application::SessionApplication>,
    managed_state: Arc<ManagedState>,
    lifecycle_delivery: awaken_coordinator::PreparedManagedLifecycleDelivery,
    event_batch_cutover_validation:
        Arc<awaken_session_application::SessionEventBatchCutoverValidationSource>,
}

fn assemble_session_plane(
    base: &mut ProcessBase,
    resources: &DeploymentAndResources,
    cloud: &CloudPlane,
    control: &mut ControlSurface,
    execution: &ExecutionStores,
    host: &mut ExecutionHost,
) -> Result<SessionPlane, String> {
    let webhook_delivery = lifecycle_webhook_delivery(base, control.webhook_stores.take());
    // The canonical Coordinator component owns Runtime background drain.
    // This process does not register a second drain for the same Host.
    let credentials = match control.vault_state.as_ref() {
        Some(vault) => awaken_session_application::SessionCredentialAuthorities::read_write(
            vault.application().credential_access(),
        ),
        None => awaken_session_application::SessionCredentialAuthorities::read_only(
            control.credential_source.clone(),
        ),
    };
    let session_resources =
        awaken_session_application::SessionResources::catalog(resources.resource_registry.clone())
            .with_retention(
                resources.resource_authorities.reclamation(),
                resources.resource_authorities.file_catalog(),
                resources.resource_application.purge_scheduler(),
            );
    let mut builder = awaken_session_application::SessionApplicationBuilder::new(
        host.host.session_execution_roles(),
        host.session_persistence
            .take()
            .ok_or("Session persistence was already consumed")?,
        execution.environment_execution.clone(),
        awaken_session_application::SessionExecutionAdmission::new(
            host.host
                .coordination()
                .dispatch_observation_source()
                .map_err(|error| error.to_string())?,
            base.worker_directory.clone(),
            host.host.dispatch().preparation_placement(),
        ),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(
        awaken_session_application::SessionApplicationConfiguration {
            execution_placement: base.placement,
            local_realization_owner: host
                .host
                .dispatch()
                .topology()
                .lease_owner
                .as_str()
                .to_owned(),
            ..Default::default()
        },
    )
    .credentials(credentials)
    .resources(session_resources)
    .agent_profiles(base.agents.catalog.clone());
    if let Some(refresh) = resources.projection_refresh.clone() {
        builder = builder.executable_projection_refresh(refresh);
    }
    if let Some(model_services) = &cloud.model_services {
        builder = builder.model_overrides(Arc::new(
            awaken_config_service::ConfigSessionModelPublicationResolver::new(
                model_services.publication_resolver.clone(),
            ),
        ));
    }
    if let Some(provider) = base.process.managed_services.list_price_provider.clone() {
        builder = builder.managed_prices(provider);
    }
    let session_application = builder.build().map_err(|error| error.to_string())?;
    let session_application = Arc::new(session_application);
    bind_session_lifecycle_hooks(base, host, &session_application)?;
    let event_batch_cutover_validation = session_application
        .recovery()
        .session_event_batch_cutover_validation_source();
    let coordinator_execution = crate::coordinator_execution(&host.host, &session_application)
        .map_err(|error| error.to_string())?;
    // The Session `content_capture` projection decides with the Host's own
    // ceiling and the very policy sources its Run boundary reads.
    let mut managed_state = ManagedState::from_application(
        session_application.clone(),
        host.host.content_capture_policy(),
    );
    if let Some(policy) = base.process.managed_services.inference_geo_policy.clone() {
        managed_state = managed_state.with_inference_geo_policy(policy);
    }
    let managed_state = Arc::new(managed_state);
    let lifecycle_delivery = awaken_coordinator::prepare_managed_lifecycle_delivery(
        &managed_state,
        &session_application,
        Some(webhook_delivery),
        Some(resources.deployment_application.as_ref()),
    )
    .expect("Managed lifecycle delivery binds before Coordinator serves traffic");
    install_credential_rollout_target(base, control, &session_application, &managed_state);
    Ok(SessionPlane {
        execution: coordinator_execution,
        session_application,
        managed_state,
        lifecycle_delivery,
        event_batch_cutover_validation,
    })
}

/// Lifecycle facts reach webhooks through the Control-owned subscription
/// source in AllInOne, and through the authenticated Control service when the
/// Coordinator is split.
fn lifecycle_webhook_delivery(
    base: &ProcessBase,
    local_stores: Option<(
        Arc<dyn awaken_config_resolver::WebhookStore>,
        Arc<dyn awaken_credential_vault::SecretStore>,
    )>,
) -> Arc<dyn awaken_session_application_contract::LifecycleFactDelivery> {
    match local_stores {
        Some((webhook_store, secrets)) => {
            awaken_webhook::lifecycle::config_plane_lifecycle_delivery(
                Arc::new(
                    awaken_control::webhook_authoring::ConfigPlaneSubscriptionSource::new(
                        webhook_store,
                        secrets,
                    ),
                ),
                Some(base.org_id.clone()),
            )
        }
        None => base
            .process
            .control_service
            .as_ref()
            .expect("split Coordinator requires Control webhook delivery")
            .webhooks
            .clone(),
    }
}

/// Wire the Environment image-build transition hint and the terminal Artifact
/// authority to the shared Session application.
fn bind_session_lifecycle_hooks(
    base: &ProcessBase,
    host: &ExecutionHost,
    session_application: &Arc<awaken_session_application::SessionApplication>,
) -> Result<(), String> {
    if let Some(builds) = base.environments.image_builds.as_ref() {
        let application = session_application.clone();
        builds
            .install_transition_hint(Arc::new(move || {
                application.lifecycle().request_reconciliation();
            }))
            .map_err(|error| error.to_string())?;
        // Cover a fast build transition that completed between starting the
        // image worker and installing this process-local hint. Notify retains
        // one permit until the lifecycle supervisor begins waiting.
        session_application.lifecycle().request_reconciliation();
    }
    if let Some(publisher) = host.artifact_fence.as_ref() {
        publisher
            .install_session_control(
                session_application.terminal_cleanups(),
                session_application.resource_effect_authorization(),
            )
            .map_err(|error| format!("install terminal Artifact authority: {error}"))?;
    }
    Ok(())
}

/// Credential rollout reaches the Coordinator's Session adoption over the local
/// Managed state, conjoined with any external target the hosted composition
/// supplies.
fn install_credential_rollout_target(
    base: &mut ProcessBase,
    control: &ControlSurface,
    session_application: &Arc<awaken_session_application::SessionApplication>,
    managed_state: &Arc<ManagedState>,
) {
    if let Some(vault_state) = control.vault_state.as_ref() {
        let local_target: Arc<dyn awaken_credential_contract::ManagedCredentialRolloutTarget> =
            awaken_coordinator::SessionCredentialRolloutAdoption::over_managed(
                session_application,
                managed_state,
            );
        if let Some(rollout_target) = managed_platform::credential_rollout_target(
            Some(local_target),
            base.process
                .managed_services
                .credential_rollout_target
                .clone(),
        ) {
            vault_state.application().set_rollout_target(rollout_target);
        }
    }
}

/// Build the Coordinator component over the shared Host and Session plane.
async fn assemble_coordinator(
    base: &mut ProcessBase,
    resources: &DeploymentAndResources,
    cloud: &mut CloudPlane,
    control: &ControlSurface,
    execution: &ExecutionStores,
    session: SessionPlane,
) -> Result<awaken_coordinator::CoordinatorComponent, String> {
    // AllInOne and split Coordinator expose the same rebuildable runtime
    // projection. Process co-location never grants the Coordinator a second,
    // direct read path into Control catalog or credential authority.
    let model_inventory: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource> =
        base.agents.catalog.clone();
    let deployment_session_launcher = Arc::new(
        awaken_protocol_managed::ManagedDeploymentSessionLauncher::new(
            session.managed_state.clone(),
        )
        .with_rate_limiter(base.managed_rate_limiter.clone()),
    );
    let private_router = base
        .agents
        .private_router
        .clone()
        .merge(base.environments.private_router.clone())
        .merge(base.worker_observations.private_router.clone());
    let resource_management_router = resource_management_router(
        &resources.resource_application,
        control.repository_credentials.clone(),
    );
    awaken_coordinator::build_coordinator_component(awaken_coordinator::CoordinatorDependencies {
        service_lifecycle: base.process.service_lifecycle.clone(),
        lifecycle_delivery: session.lifecycle_delivery,
        execution: session.execution,
        session_application: session.session_application,
        managed_state: session.managed_state,
        resource_registry: resources.resource_registry.clone(),
        resource_management_router,
        resource_authorities: resources.resource_authorities.clone(),
        memory_stores: resources.resource_application.memory_stores(),
        worker_file_application: resources.resource_application.files(),
        worker_skill_bundles: resources.resource_application.skill_bundle_source(),
        application_access: execution.application_access.clone(),
        ai_sdk_browser_cors: std::mem::take(&mut base.process.ai_sdk_browser_cors),
        model_inventory,
        dream_process_store: execution.dream_process_store.clone(),
        worker_authenticator: base.worker_transport.authenticator(),
        worker_placement_policy: base.process.worker_placement_policy.take(),
        worker_execution_bindings: base.worker_transport.binding_source(),
        worker_execution_stops: base.worker_transport.stop_source(),
        worker_relay_targets: base.worker_transport.relay_target_source(),
        worker_create_broker_router: broker_router(
            &base.worker_transport,
            &base.worker_directory,
            &base.stores.clock,
        ),
        credential_deliveries: cloud.attempt_credential_deliveries.take(),
        repository_transport_authorizer: base.process.repository_transport_authorizer.take(),
        worker_directory: base.worker_directory.clone(),
        deployment_application: resources.deployment_application.clone(),
        deployment_session_launcher,
        executable_agents: base.agents.catalog.clone(),
        executable_projection_refresh: resources.projection_refresh.clone(),
        environments: execution.environment_execution.clone(),
        sessions: execution.sessions.clone(),
        default_workspace: base.stores.platform_workspace.clone(),
        private_router,
        service_authenticator: base.agents.service_authenticator.clone(),
        relay_destination_authenticator: base.process.relay_destination_authenticator.take(),
        clock: base.stores.clock.clone(),
        org_admission: base.managed_rate_limiter.clone(),
    })
    .await
    .map_err(|error| format!("build Coordinator component: {error}"))
}

/// Resource reclamation over the Resources authorities and the Session
/// purge guard.
fn resource_reclaimer(
    resources: &DeploymentAndResources,
    execution: &ExecutionStores,
) -> Arc<awaken_resource_reclaimer::ResourceReclaimer> {
    Arc::new(
        awaken_resource_reclaimer::ResourceReclaimer::new(
            format!("awaken-resource-reclaimer:{}", std::process::id()),
            30_000,
            resources.resource_authorities.reclamation(),
            resources.resource_application.physical_cleanup(),
        )
        .expect("construct resource reclaimer")
        .with_preparation(resources.resource_application.purge_preparation())
        .with_guard(resources.resource_application.lifecycle_guard())
        .with_guard(Arc::new(
            awaken_session_application::SessionResourcePurgeGuard::new(
                execution.sessions.clone(),
                resources.resource_authorities.file_catalog(),
            ),
        )),
    )
}

/// Protect the Coordinator surfaces with the process IAM/audit/admission edge,
/// merge Control for AllInOne, wrap the flat surface in Workspace path
/// addressing (ADR-0048 D3 / ADR-0051), and start reclamation.
async fn finish_process_routers(
    base: ProcessBase,
    resources: &DeploymentAndResources,
    execution: &ExecutionStores,
    control: ControlSurface,
    coordinator: awaken_coordinator::CoordinatorComponent,
    event_batch_cutover_validation: Arc<
        awaken_session_application::SessionEventBatchCutoverValidationSource,
    >,
) -> ProcessRouters {
    let resource_reclaimer = resource_reclaimer(resources, execution);
    let reclamation_clock = resources.resource_authorities.clock();
    let surfaces::ProtectedSurfaces { data, private } = surfaces::protect(
        coordinator,
        base.iam.clone(),
        base.remote_iam.clone(),
        control.audit_plane,
        base.managed_rate_limiter.clone(),
    );
    let (flat, mcp_export) = match base.role {
        config::Role::AllInOne => (data.merge(control.router), control.mcp_export),
        config::Role::Coordinator => (data, Router::new()),
        config::Role::Control => unreachable!("Control returned before Coordinator process"),
        config::Role::Worker => unreachable!("Worker has its own process startup"),
    };
    let worker_authenticator = base.worker_transport.authenticator();
    let routers = ProcessRouters::new(
        process_surface::finish(
            flat,
            mcp_export,
            control.reconciler,
            base.worker_observations.source,
            base.stores.platform_workspace,
        ),
        private,
        control.registration_supervisor,
        base.process.service_lifecycle,
        Some(event_batch_cutover_validation),
        control.admin_execs,
    )
    .with_worker_authenticator(worker_authenticator);
    reclamation::install(
        &routers.service_lifecycle,
        resource_reclaimer,
        reclamation_clock,
    )
    .await;
    routers
}
