//! Coordinator/AllInOne process composition over canonical domain components.

use super::*;

pub(super) async fn assemble_runtime_process_router(
    stores: ProcessStores,
    iam: Option<Arc<ManagementAuthz>>,
    remote_iam: Option<Arc<RemoteManagementAuthz>>,
    local_browser_auth: Option<awaken_control::LocalBrowserAuth>,
    model_composition: PublicationModelComposition,
    assembly: ProcessAssemblyOptions,
    // An optional last-mile hook on the assembled data-plane host, applied before it is
    // shared. The composition root uses it to wire a runtime backend the standard process
    // does not assemble itself (e.g. an ACP executor for `acp:*` threads) without this
    // module naming that backend's crate. `None` in production; `Some` in a scenario that
    // serves external-CLI sessions.
    customize_host: Option<Box<dyn FnOnce(SharedHost) -> SharedHost + Send>>,
) -> Result<ProcessRouterAssembly, String> {
    let role = assembly.role;
    debug_assert!(matches!(
        role,
        config::Role::AllInOne | config::Role::Coordinator
    ));
    let worker_directory = assembly
        .worker_directory
        .expect("runtime process requires an explicit WorkerDirectory");
    let worker_observation_wiring = assembly
        .worker_observations
        .expect("runtime process requires explicit Worker observation wiring");
    let worker_observations = worker_observation_wiring.source;
    let worker_observation_private_router = worker_observation_wiring.private_router;
    let worker_authenticator = assembly.worker_authenticator.unwrap_or_else(|| {
        Arc::new(awaken_worker_transport_security::HeaderWorkerAuthenticator)
            as Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>
    });
    let (
        executable_agent_catalog,
        executable_agent_registrar,
        executable_agent_private_router,
        executable_agent_projection_refresher,
        _remote_coordinator_content_eraser,
    ) = executable_agent_registration::process_parts(assembly.executable_agent_wiring);
    let content_capture_ceiling = assembly.content_capture_ceiling;
    let deployment = assembly.deployment;
    let session_execution_placement = if deployment
        .as_ref()
        .is_some_and(|deployment| deployment.disable_local_pool)
    {
        awaken_session_application::SessionExecutionPlacement::RegisteredWorker
    } else {
        awaken_session_application::SessionExecutionPlacement::LocalWorker
    };
    let cloud_api_base_url = assembly.cloud_api_base_url;
    let model_supply = assembly.model_supply.clone();
    let cloud_models_enabled = model_supply.cloud_models_enabled;
    let injected_brokered_catalog = assembly.brokered_catalog.clone();
    let org_id = assembly.org_id.unwrap_or_else(local_org_id);
    let enrollment_signing_key = match (role, assembly.enrollment_signing_key) {
        (config::Role::AllInOne, Some(key)) => key,
        #[cfg(any(test, feature = "test-support"))]
        (config::Role::AllInOne, None) => [0xA5; 32],
        #[cfg(not(any(test, feature = "test-support")))]
        (config::Role::AllInOne, None) => {
            panic!("AllInOne requires a derived enrollment signing key")
        }
        (config::Role::Coordinator, _) => [0; 32],
        _ => unreachable!(),
    };
    let managed_rate_limiter =
        Arc::new(awaken_protocol_managed::ManagedRateLimiter::for_organization(org_id.clone()));
    let mcp_bearer_token = assembly.mcp_bearer_token;
    // Cause/effect composition rule: one selected ResourceComponent is moved intact
    // into the Host. The management Skill API borrows the one additional view it
    // needs; no tuple decomposition or parallel Resources reconstruction.
    let coordinator_stores = stores
        .coordinator
        .as_ref()
        .expect("Managed Execution role requires Coordinator stores");
    // Restore the Coordinator-owned Deployment aggregate exactly once before
    // sibling components are assembled. AllInOne Agent lifecycle commands and
    // the Coordinator router/scheduler receive this same instance.
    let deployment_application =
        awaken_coordinator::restore_deployment_application(coordinator_stores.deployments.clone())
            .await
            .map_err(|error| format!("restore Deployment state: {error}"))?;
    let agent_archive_cascade =
        deployment_application.clone() as Arc<dyn awaken_deployment_contract::AgentArchiveCascade>;
    let executable_environment_wiring = executable_environment_registration::require_process_wiring(
        assembly.executable_environment_wiring,
    );
    let executable_environment_catalog = executable_environment_wiring.catalog;
    let executable_environment_registrar = executable_environment_wiring.registrar;
    let executable_environment_projection_refresher =
        executable_environment_wiring.projection_refresher;
    let executable_environment_private_router = executable_environment_wiring.private_router;
    let executable_environment_image_builds = executable_environment_wiring.image_builds;
    let coordinator_content_eraser =
        awaken_coordinator::data_subject_boundary::coordinator_content_eraser(
            coordinator_stores.captured_content_eraser.clone(),
            deployment
                .as_ref()
                .and_then(|deployment| deployment.acp_session_blob_root.clone()),
        );
    let resource_component = coordinator_stores.resource_component.clone();
    let resource_application =
        awaken_resource_application::ResourcesApplication::new(resource_component.clone());
    // Resolve the installation's Workspace exactly once, then inject the same
    // coordinate into every adapter assembled below. Durable roots persist it;
    // ephemeral roots receive a process-local generated coordinate.
    let platform_workspace = stores.workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let brokered_client = brokered_inference_client(
        cloud_models_enabled,
        remote_iam.as_ref(),
        cloud_api_base_url.as_deref(),
        &platform_workspace,
    );
    let credential_materializer = (role == config::Role::AllInOne)
        .then(|| {
            stores.control.as_ref().map(|control| {
                awaken_credential_materializer::PinnedCredentialMaterializer::new(
                    control.credentials.clone(),
                    control.secrets.clone(),
                )
            })
        })
        .flatten();
    let web_search_providers = assembly
        .web_search_providers
        .unwrap_or_else(awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins);
    let model_assembly = (role == config::Role::AllInOne).then(|| {
        publication_model_assembly(
            model_composition,
            &stores,
            cloud_models_enabled,
            worker_observations.clone(),
        )
    });
    let model_wiring = match (&model_assembly, &credential_materializer) {
        (Some(assembly), Some(credentials)) => runtime_model_wiring(
            assembly.runtime.clone(),
            credentials,
            cloud_models_enabled,
            brokered_client.as_ref(),
        ),
        (None, None) => RuntimeModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: None,
        },
        _ => unreachable!("Control model adapters and Control stores are composed together"),
    };
    let web_search_publication_resolver =
        assembly.web_search_publication_resolver.unwrap_or_else(|| {
            Arc::new(
                crate::web_search_publication::WebSearchPublicationResolver::new(
                    web_search_providers.clone(),
                ),
            )
        });
    // Keep the IAM handles for the sibling resource PEP. The authoring router owns
    // its PEP; File/Memory/Skill routes are wrapped independently after the data
    // router is assembled, so neither plane depends on the other's services.
    let resource_iam = iam.clone();
    let resource_remote_iam = remote_iam.clone();
    let deployment_iam = iam.clone();
    let deployment_remote_iam = remote_iam.clone();
    let live_runtime_capabilities = stores.control.as_ref().map(|control| {
        Arc::new(LiveRuntimeCapabilities {
            initial: assembly.local_acp_observations.clone(),
            workers: worker_observations.clone(),
            credentials: control.credentials.clone(),
            workspace: platform_workspace.clone(),
        })
    });
    let environment_authoring = match role {
        config::Role::AllInOne => {
            let control = stores
                .control
                .as_ref()
                .expect("AllInOne owns Control Environment stores");
            let state = Arc::new(awaken_protocol_managed::EnvironmentAuthoringState::new(
                control.environments.clone(),
                control.sandbox_policies.clone(),
                executable_environment_registrar,
            ));
            Some(state)
        }
        config::Role::Coordinator => None,
        config::Role::Control | config::Role::Worker => unreachable!(),
    };

    // Control owns this complete component. Standalone Control and AllInOne
    // supply different process adapters but call the same domain builder;
    // Coordinator never constructs ConfigService or a Control router.
    let control_component = match role {
        config::Role::AllInOne => Some(
            control_component_for_process(
                &stores,
                &platform_workspace,
                &org_id,
                enrollment_signing_key,
                executable_agent_registrar,
                Some(agent_archive_cascade),
                model_assembly
                    .as_ref()
                    .expect("AllInOne composes model publication")
                    .publication_resolver
                    .clone(),
                web_search_publication_resolver,
                &web_search_providers,
                brokered_client.clone(),
                injected_brokered_catalog,
                model_supply,
                role.exposes_managed_runtime(),
                &assembly.local_acp_observations,
                live_runtime_capabilities
                    .clone()
                    .expect("AllInOne composes Control runtime capabilities"),
                Some(Arc::new(awaken_control::HostResourceInventory::new(
                    resource_component.resource_catalog(),
                    resource_component.skill_store(),
                    &platform_workspace,
                ))),
                Arc::new(
                    awaken_environment_application::EnvironmentApplicationAuthor::new(
                        environment_authoring
                            .as_ref()
                            .expect("AllInOne owns Environment authoring")
                            .application(),
                    ),
                ),
                environment_authoring
                    .as_ref()
                    .expect("AllInOne owns Environment authoring")
                    .application(),
                awaken_protocol_managed::environment_authoring_router(
                    environment_authoring
                        .clone()
                        .expect("AllInOne owns Environment authoring"),
                )
                .merge(awaken_protocol_awaken::environment_extensions_router(
                    environment_authoring
                        .as_ref()
                        .expect("AllInOne owns Environment authoring")
                        .application(),
                    environment_authoring
                        .as_ref()
                        .expect("AllInOne owns Environment authoring")
                        .sandbox_policy_store(),
                )),
                coordinator_content_eraser,
                content_capture_ceiling,
                iam.clone(),
                local_browser_auth,
                remote_iam.clone(),
            )
            .await,
        ),
        config::Role::Coordinator => None,
        config::Role::Control | config::Role::Worker => {
            unreachable!("runtime process assembly accepts only AllInOne or Coordinator")
        }
    };
    let ProcessStores {
        workspace_root: _,
        control: control_stores,
        coordinator,
    } = stores;
    let CoordinatorStores {
        resource_component,
        sessions,
        deployments: _,
        dream_process_store,
        memory_extractions,
        capture_sink,
        captured_content_eraser: _,
        environment_work,
    } = coordinator.expect("Managed Execution role requires Coordinator stores");
    let environment_execution =
        awaken_environment_execution_application::EnvironmentExecutionApplication::new(
            environment_work,
            executable_environment_catalog,
        );
    let environment_execution = match executable_environment_image_builds {
        Some(builds) => environment_execution.with_image_readiness(builds),
        None => environment_execution,
    };
    let environment_execution = Arc::new(environment_execution);
    let resource_catalog = resource_component.resource_catalog();
    let (
        control,
        mcp_export,
        reconciler,
        admin_execs,
        credential_source,
        deployment_audit_plane,
        local_webhook_stores,
        data_subject_consent,
        registration_supervisor,
    ) = match control_component {
        Some(component) => {
            let control_stores = control_stores
                .as_ref()
                .expect("AllInOne Control component has Control stores");
            let mcp_export = awaken_coordinator::mcp_export::router(
                awaken_admin_assistant::admin_tool_descriptors(),
                component.admin_tools.clone(),
                mcp_bearer_token,
            );
            (
                component.router,
                mcp_export,
                Some(component.publication_reconciler),
                component.admin_tools,
                component.vault_state.clone()
                    as Arc<dyn awaken_session_application::SessionCredentialSource>,
                component.management_audit,
                Some((
                    control_stores.webhooks.clone(),
                    control_stores.secrets.clone(),
                )),
                component.data_subject_consent,
                Some(component.registration_supervisor),
            )
        }
        None => {
            let ports = assembly
                .control_service
                .clone()
                .expect("split Coordinator requires Control service ports");
            (
                Router::new(),
                Router::new(),
                None,
                Vec::new(),
                ports.credentials,
                ManagementAuditPlane::from_repository(ports.audit),
                None,
                ports.consent,
                None,
            )
        }
    };

    // Only Coordinator and AllInOne can reach this point. Their one execution
    // store group owns Session, Deployment/Run, Dream, extraction work and the
    // lifecycle outbox; Control has no fallback implementation of that group.
    // Application credentials and the protocol guards that consume them must
    // live in the same Coordinator process. The issuer validates bindings
    // against this Coordinator's sole Managed Session repository; split Control
    // neither mirrors that repository nor owns a second token directory.
    let application_access = Arc::new(awaken_authz_enforce::ApplicationAccessStore::new());
    let webhook_sink: Arc<dyn awaken_session_contract::SessionLifecycleSink> =
        match local_webhook_stores {
            Some((webhook_store, secrets)) => {
                awaken_webhook_managed::assemble_with_session_repo(
                    webhook_store,
                    secrets,
                    Some(org_id.clone()),
                    sessions.clone(),
                )
                .0
            }
            None => Arc::new(awaken_webhook_managed::WebhookLifecycleSink::with_delivery(
                assembly
                    .control_service
                    .as_ref()
                    .expect("split Coordinator requires Control webhook delivery")
                    .webhooks
                    .clone(),
                sessions.clone(),
            )),
        };
    // The data plane: the host runs the server model, resolves a session's agent to
    // its installed config, and carries the management tool executables so the
    // reserved-scope assistant can call them. It shares the SAME skill store and
    // Resource Catalog the capability inventory reads, so a skill or memory store the
    // host serves is exactly what the assistant enumerates, and identity survives a
    // restart.
    let mut host_builder = match deployment {
        Some(deployment) => SharedHost::new_with_resource_component_and_deployment(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_component.clone(),
            memory_extractions,
            deployment,
        ),
        #[cfg(any(test, feature = "test-support"))]
        None => {
            let host = SharedHost::new_with_resource_component(
                model_wiring.executor,
                model_wiring.model_ref,
                resource_component.clone(),
            );
            host.install_memory_extraction_repository(memory_extractions);
            host
        }
        #[cfg(not(any(test, feature = "test-support")))]
        None => unreachable!("product runtime assembly requires a resolved deployment"),
    };
    host_builder = host_builder
        .with_file_application(resource_application.files())
        .with_local_workspace(platform_workspace.clone())
        .with_web_search_provider_registry(web_search_providers)
        .with_acp_tool_exporter(Arc::new(
            awaken_coordinator::mcp_export::SessionToolExporter,
        ))
        .with_remote_attempt_executor(awaken_coordinator::a2a_attempt_executor(
            credential_materializer.clone(),
        ))
        .with_agent_publications(executable_agent_catalog.clone())
        .with_agent_resource_references(executable_agent_catalog.clone())
        .with_capture_sink(capture_sink)
        .with_data_subject_consent_source(data_subject_consent)
        .with_admin_tools(admin_execs);
    if let Some(credentials) = credential_materializer.clone() {
        host_builder = host_builder.with_credential_materializer(credentials);
    }
    if let Some(materializer) = model_wiring.materializer {
        host_builder = host_builder.with_inference_materializer(materializer);
    }
    // Production ACP wiring (`acp:*` threads): the environment advertises only the
    // installed CLI/sandbox capability. Provider coordinates and credentials are
    // realized from the same publication-pinned DB facts as native inference.
    let hand_factory = awaken_coordinator::relay_hand_executor_factory();
    let host_builder = host_builder
        .with_session_environment_from_deployment(Some(hand_factory))
        .await
        .with_acp_from_deployment(credential_materializer.clone())
        .await;
    // Last-mile backend wiring the standard process does not assemble itself, injected
    // by the composition root (a scenario that serves external-CLI sessions).
    let host_builder = match customize_host {
        Some(customize) => customize(host_builder),
        None => host_builder,
    };
    let host = Arc::new(host_builder);
    let resource_reclamation = Arc::new(awaken_runtime_host::HostResourceReclamation::new(
        host.clone(),
        resource_catalog.clone(),
    ));
    let resource_reclaimer = Arc::new(
        awaken_resource_reclaimer::ResourceReclaimer::new(
            format!("awaken-resource-reclaimer:{}", std::process::id()),
            30_000,
            host.resource_lifecycle()
                .expect("resource-plane composition installs lifecycle repository"),
            resource_reclamation.clone(),
        )
        .expect("construct resource reclaimer")
        .with_guard(resource_reclamation),
    );
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default()
    };
    if let Ok(summary) = resource_reclaimer.reconcile(now_ms(), 256).await
        && summary.completed > 0
    {
        eprintln!("reclaimed {} durable resource(s)", summary.completed);
    }
    let recurring_resource_reclaimer = resource_reclaimer.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            if let Err(error) = recurring_resource_reclaimer.reconcile(now, 256).await {
                eprintln!("resource reclamation retry remains pending: {error}");
            }
        }
    });
    let mut managed_host =
        ManagedHost::new(host.clone()).with_resource_validator(resource_catalog.clone());
    if let Some(credentials) = credential_materializer {
        managed_host = managed_host.with_credential_materializer(credentials);
    }
    let managed_host = Arc::new(managed_host);
    let session_application =
        awaken_session_application::SessionApplication::new_with_configuration(
            managed_host.clone(),
            managed_host,
            sessions.clone(),
            environment_execution.clone(),
            awaken_session_application::SessionApplicationConfiguration {
                execution_placement: session_execution_placement,
            },
        );
    let mut managed_state = ManagedState::from_application(session_application)
        .with_credential_source(credential_source)
        .with_resource_catalog(resource_catalog.clone())
        .with_resource_purge_scheduler(resource_application.purge_scheduler())
        // Share the SAME config plane `/v1/agents` reads, so a session inheriting a
        // published agent's model sees the authoritative config-plane truth (M2).
        .with_config_source(executable_agent_catalog.clone());
    managed_state = managed_state.with_lifecycle_sink(webhook_sink);
    let managed_state = Arc::new(managed_state);
    // Workspace path addressing (ADR-0048 D3 / ADR-0051): wrap the fully-merged flat
    // surface so a `/v1/workspaces/{ws}/…` request is captured, rewritten to its flat
    // `/v1/…` form, and its `{ws}` stamped as the edge scope before it re-enters
    // routing. Flat requests fall through unchanged. The same assembly returns the
    // DreamApplication it mounted, so scheduling cannot target a parallel instance.
    let model_directory: Arc<dyn awaken_coordinator::ModelDirectory> = match control_stores.as_ref()
    {
        Some(control) => Arc::new(
            awaken_coordinator::model_directory::CatalogModelDirectory::with_source(
                control.catalog.clone(),
                control.credentials.clone(),
                live_runtime_capabilities.expect("AllInOne composes live Control capabilities"),
            ),
        ),
        None => Arc::new(
            awaken_coordinator::model_directory::ExecutableAgentModelDirectory::new(
                executable_agent_catalog.clone(),
            ),
        ),
    };
    let registration_router = executable_agent_private_router
        .merge(executable_environment_private_router)
        .merge(worker_observation_private_router);
    let resource_ports = resource_application.ports();
    let resource_management_router =
        awaken_coordinator::resources_router(awaken_coordinator::ResourcesRouterInput {
            files: resource_application.files(),
            memories: resource_ports.memory_repository(),
            memory_stores: resource_application.memory_stores(),
            skills: Some(resource_ports.skill_store()),
            purge: resource_application.purge_scheduler(),
        });
    let coordinator = awaken_coordinator::build_coordinator_component(
        awaken_coordinator::CoordinatorDependencies {
            host,
            managed_state,
            resource_catalog,
            resource_management_router,
            application_access,
            model_directory,
            dream_process_store,
            worker_authenticator,
            worker_directory: worker_directory.clone(),
            deployment_application,
            executable_agents: executable_agent_catalog,
            rate_limiter: managed_rate_limiter.clone(),
            environments: environment_execution,
            sessions,
            default_workspace: platform_workspace.clone(),
            registration_router,
        },
    )
    .await
    .map_err(|error| format!("build Coordinator component: {error}"))?;
    let coordinator_management = awaken_control::protect_management_router(
        coordinator.management_router,
        deployment_audit_plane,
        deployment_iam,
        deployment_remote_iam,
    );
    let mut data = executable_projection_refresh::layer(
        coordinator.router.merge(coordinator_management),
        executable_agent_projection_refresher,
        executable_environment_projection_refresher,
    );
    if let Some(iam) = resource_iam {
        data = data.layer(axum::middleware::from_fn_with_state(
            iam,
            awaken_control::authz::resource_guard,
        ));
    } else if let Some(remote_iam) = resource_remote_iam {
        data = data.layer(axum::middleware::from_fn_with_state(
            remote_iam,
            awaken_control::authz::cloud_resource_guard,
        ));
    }
    let (flat, mcp_export) = match role {
        config::Role::AllInOne => (data.merge(control), mcp_export),
        config::Role::Coordinator => (data, Router::new()),
        config::Role::Control => unreachable!("Control returned before Coordinator assembly"),
        config::Role::Worker => unreachable!("Worker has its own process composition"),
    };
    Ok(ProcessRouterAssembly::new(
        process_surface::finish(
            flat,
            mcp_export,
            reconciler,
            worker_observations,
            platform_workspace,
            managed_rate_limiter,
        ),
        registration_supervisor,
    ))
}
