//! Coordinator/AllInOne process startup over canonical domain components.

use super::*;

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
    customize_host: Option<Box<dyn FnOnce(SharedHost) -> SharedHost + Send>>,
) -> Result<ProcessRouters, String> {
    let role = process.role;
    debug_assert!(matches!(
        role,
        config::Role::AllInOne | config::Role::Coordinator
    ));
    let worker_directory = process
        .worker_directory
        .expect("runtime process requires an explicit WorkerDirectory");
    let runtime_authority = process.runtime_authority;
    let worker_observation_wiring = process
        .worker_observations
        .expect("runtime process requires explicit Worker observation wiring");
    let worker_observations = worker_observation_wiring.source;
    let worker_observation_private_router = worker_observation_wiring.private_router;
    let worker_authenticator = process.worker_authenticator.unwrap_or_else(|| {
        Arc::new(awaken_worker_transport_security::HeaderWorkerAuthenticator)
            as Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>
    });
    let worker_placement_policy = process.worker_placement_policy;
    let (
        executable_agent_catalog,
        executable_agent_registrar,
        executable_agent_private_router,
        executable_agent_projection_refresher,
        _remote_coordinator_content_eraser,
    ) = executable_agent_registration::process_parts(process.executable_agent_wiring);
    let content_capture_ceiling = process.content_capture_ceiling;
    let deployment = process.deployment;
    let session_execution_placement = if deployment
        .as_ref()
        .is_some_and(|deployment| deployment.disable_local_pool)
    {
        awaken_session_application::SessionExecutionPlacement::RegisteredWorker
    } else {
        awaken_session_application::SessionExecutionPlacement::LocalWorker
    };
    let cloud_api_base_url = process.cloud_api_base_url;
    let model_capabilities = process.model_supply.clone();
    let cloud_models_enabled = model_capabilities.cloud_models_enabled;
    let injected_brokered_catalog = process.brokered_catalog.clone();
    let org_id = process.org_id.unwrap_or_else(local_org_id);
    let enrollment_signing_key = match (role, process.enrollment_signing_key) {
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
    let managed_rate_limiter = process
        .managed_services
        .request_limiter
        .clone()
        .unwrap_or_else(|| {
            Arc::new(awaken_protocol_managed::ManagedRateLimiter::for_organization(org_id.clone()))
        });
    let mcp_bearer_token = process.mcp_bearer_token;
    // Cause/effect ownership rule: one selected ResourceAuthorities value is moved intact
    // into the Host. The management Skill API borrows the one additional view it
    // needs; no tuple decomposition or parallel Resources reconstruction.
    let coordinator_stores = stores
        .coordinator
        .as_ref()
        .expect("Managed Execution role requires Coordinator stores");
    // Restore the Coordinator-owned Deployment aggregate exactly once before
    // sibling components are prepared. AllInOne Agent lifecycle commands and
    // the Coordinator router/scheduler receive this same instance.
    let deployment_application =
        awaken_coordinator::restore_deployment_application(coordinator_stores.deployments.clone())
            .await
            .map_err(|error| format!("restore Deployment state: {error}"))?;
    let agent_archive_cascade =
        deployment_application.clone() as Arc<dyn awaken_deployment_contract::AgentArchiveCascade>;
    let executable_environment_wiring = executable_environment_registration::require_process_wiring(
        process.executable_environment_wiring,
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
    let resource_application = coordinator_stores.resources.clone();
    let resource_authorities = resource_application.authorities();
    // Resolve the installation's Workspace exactly once, then inject the same
    // coordinate into every adapter prepared below. Durable roots persist it;
    // ephemeral roots receive a process-local generated coordinate.
    let platform_workspace = stores.workspace_root.as_deref().map_or_else(
        SharedHost::provision_local_workspace,
        SharedHost::provision_local_workspace_at,
    );
    let brokered_client = brokered_inference_client(
        cloud_models_enabled && role == config::Role::AllInOne,
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
    let credential_refresh_factory = (role == config::Role::AllInOne)
        .then(|| {
            stores.control.as_ref().map(|control| {
                Arc::new(awaken_credential_materializer::VaultRefreshFactory::new(
                    control.credentials.clone(),
                    control.secrets.clone(),
                ))
                    as Arc<dyn awaken_credential_materializer::CredentialRefreshFactory>
            })
        })
        .flatten();
    let web_search_providers = process
        .web_search_providers
        .unwrap_or_else(awaken_ext_builtin_tools::WebSearchProviderRegistry::builtins);
    let model_services = (role == config::Role::AllInOne).then(|| {
        resolve_model_services(
            model_supply,
            &stores,
            cloud_models_enabled,
            worker_observations.clone(),
        )
    });
    let model_wiring = match (&model_services, &credential_materializer) {
        (Some(process), Some(credentials)) => runtime_model_wiring(
            process.runtime.clone(),
            credentials,
            cloud_models_enabled,
            brokered_client.as_ref(),
        ),
        (None, None) => RuntimeModelWiring {
            executor: Arc::new(awaken_runtime_host::NoModelConfiguredExecutor),
            model_ref: awaken_runtime_host::UNCONFIGURED_MODEL_REF.to_string(),
            materializer: None,
        },
        _ => unreachable!("Control model adapters and Control stores are configured together"),
    };
    let web_search_publication_resolver =
        process.web_search_publication_resolver.unwrap_or_else(|| {
            Arc::new(
                crate::web_search_publication::WebSearchPublicationResolver::new(
                    web_search_providers.clone(),
                ),
            )
        });
    let deployment_iam = iam.clone();
    let deployment_remote_iam = remote_iam.clone();
    let live_runtime_capabilities = stores.control.as_ref().map(|control| {
        Arc::new(LiveRuntimeCapabilities {
            initial: process.local_acp_observations.clone(),
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
            let application =
                Arc::new(awaken_environment_application::EnvironmentApplication::new(
                    control.environments.clone(),
                    executable_environment_registrar,
                    Some(control.sandbox_policies.clone()),
                ));
            let state = Arc::new(awaken_protocol_managed::EnvironmentAuthoringState::new(
                application,
                control.sandbox_policies.clone(),
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
                &process.service_lifecycle,
                &platform_workspace,
                &org_id,
                enrollment_signing_key,
                executable_agent_registrar,
                Some(agent_archive_cascade),
                model_services
                    .as_ref()
                    .expect("AllInOne configures model publication")
                    .publication_resolver
                    .clone(),
                web_search_publication_resolver,
                &web_search_providers,
                brokered_client.clone(),
                injected_brokered_catalog,
                model_capabilities,
                role.mounts_managed_runtime(),
                &process.local_acp_observations,
                live_runtime_capabilities
                    .clone()
                    .expect("AllInOne configures Control runtime capabilities"),
                Some(Arc::new(awaken_control::HostResourceInventory::new(
                    resource_authorities.resource_catalog(),
                    resource_authorities.skill_store(),
                ))),
                environment_authoring
                    .as_ref()
                    .expect("AllInOne owns Environment authoring")
                    .application(),
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
                process.managed_services.tunnel_application.clone(),
                Some(managed_rate_limiter.clone()),
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
            unreachable!("runtime process accepts only AllInOne or Coordinator")
        }
    };
    let ProcessStores {
        workspace_root: _,
        control: control_stores,
        coordinator,
    } = stores;
    let CoordinatorStores {
        resources: _,
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
    let resource_catalog = resource_authorities.resource_catalog();
    let indexed_memory_extractions =
        Arc::new(awaken_coordinator::ReferenceIndexedMemoryExtractions::new(
            memory_extractions,
            resource_authorities.reclamation(),
        ));
    indexed_memory_extractions
        .synchronize_recoverable_references()
        .await
        .map_err(|error| format!("restore Memory extraction references: {error}"))?;
    let memory_extractions: Arc<dyn awaken_ext_memory::MemoryExtractionRepository> =
        indexed_memory_extractions;
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
            let services = process
                .control_service
                .clone()
                .expect("split Coordinator requires Control services");
            (
                Router::new(),
                Router::new(),
                None,
                Vec::new(),
                services.credentials,
                ManagementAuditPlane::from_repository(services.audit),
                None,
                services.consent,
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
    let webhook_notifier: Arc<dyn awaken_session_contract::LifecycleFactNotifier> =
        match local_webhook_stores {
            Some((webhook_store, secrets)) => {
                let delivery = awaken_webhook_managed::config_plane_lifecycle_delivery(
                    webhook_store,
                    secrets,
                    Some(org_id.clone()),
                );
                Arc::new(
                    awaken_webhook_managed::WebhookOutboxNotifier::with_delivery(
                        delivery,
                        sessions.clone(),
                        &process.service_lifecycle,
                    ),
                )
            }
            None => Arc::new(
                awaken_webhook_managed::WebhookOutboxNotifier::with_delivery(
                    process
                        .control_service
                        .as_ref()
                        .expect("split Coordinator requires Control webhook delivery")
                        .webhooks
                        .clone(),
                    sessions.clone(),
                    &process.service_lifecycle,
                ),
            ),
        };
    // The data plane: the host runs the server model, resolves a session's agent to
    // its installed config, and carries the management tool executables so the
    // reserved-scope assistant can call them. It shares the SAME skill store and
    // Resource Catalog the capability inventory reads, so a skill or memory store the
    // host serves is exactly what the assistant enumerates, and identity survives a
    // restart.
    let mut host_builder = match deployment {
        Some(deployment) => SharedHost::new_with_runtime_resources_and_deployment(
            model_wiring.executor,
            model_wiring.model_ref,
            resource_application.file_content_source(),
            resource_authorities.memory_repository(),
            memory_extractions,
            deployment,
        ),
        #[cfg(any(test, feature = "test-support"))]
        None => {
            let host = SharedHost::new_with_resources(
                model_wiring.executor,
                model_wiring.model_ref,
                resource_authorities.clone(),
            );
            host.install_memory_extraction_repository(memory_extractions);
            host
        }
        #[cfg(not(any(test, feature = "test-support")))]
        None => unreachable!("product runtime process requires a resolved deployment"),
    };
    let local_dispatch = runtime_authority
        .as_ref()
        .map(|authority| authority.dispatch_store());
    if let Some(runtime_authority) = runtime_authority {
        host_builder = host_builder.with_runtime_authority(runtime_authority);
    }
    let artifact_publisher = resource_application.artifact_publisher();
    let artifact_publisher = local_dispatch.map_or(artifact_publisher.clone(), |dispatch| {
        Arc::new(awaken_coordinator::ClaimFencedArtifactPublisher::new(
            artifact_publisher,
            dispatch,
        ))
            as Arc<dyn awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::RunClaim>>
    });
    host_builder = host_builder
        .with_artifact_publisher(artifact_publisher)
        .with_skill_bundle_source(resource_application.skill_bundle_source())
        .with_skill_catalog_application(resource_application.skill_catalog_application())
        .with_local_workspace(platform_workspace.clone())
        .with_web_search_provider_registry(web_search_providers)
        .with_acp_tool_exporter(Arc::new(
            awaken_coordinator::mcp_export::SessionToolExporter,
        ))
        .with_remote_attempt_executor(awaken_coordinator::a2a_attempt_executor(
            credential_materializer.clone(),
        ))
        .with_agent_publications(executable_agent_catalog.clone())
        .with_capture_sink(capture_sink)
        .with_data_subject_consent_source(data_subject_consent)
        .with_admin_tools(admin_execs);
    if let Some(credentials) = credential_materializer.clone() {
        host_builder = host_builder.with_credential_materializer(credentials);
    }
    if let Some(materializer) = model_wiring.materializer {
        host_builder = host_builder.with_inference_materializer(materializer);
    }
    // Production ACP wiring (`acp:*` threads): only a process that owns the local
    // claim pool realizes a Session Environment. A split Coordinator admits and
    // projects Sessions to registered Workers; probing or constructing another
    // sandbox here would create a second physical-effect owner.
    let host_builder = if session_execution_placement
        == awaken_session_application::SessionExecutionPlacement::LocalWorker
    {
        let hand_factory = awaken_worker::relay_hand_executor_factory();
        host_builder
            .with_session_environment_from_deployment(Some(hand_factory))
            .await
            .with_acp_from_deployment(credential_materializer.clone(), None)
            .await
    } else {
        host_builder
    };
    // Last-mile backend wiring the standard process does not own, supplied
    // by the process startup (a scenario that serves external-CLI sessions).
    let host_builder = match customize_host {
        Some(customize) => customize(host_builder),
        None => host_builder,
    };
    let host = Arc::new(host_builder);
    let resource_reclaimer = Arc::new(
        awaken_resource_reclaimer::ResourceReclaimer::new(
            format!("awaken-resource-reclaimer:{}", std::process::id()),
            30_000,
            resource_authorities.reclamation(),
            resource_application.physical_cleanup(),
        )
        .expect("construct resource reclaimer")
        .with_guard(resource_application.lifecycle_guard())
        .with_guard(Arc::new(
            awaken_session_application::SessionResourcePurgeGuard::new(
                sessions.clone(),
                resource_authorities.file_catalog(),
            ),
        )),
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
    process
        .service_lifecycle
        .spawn("resources-reclamation", move |cancel| async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or_default();
                if let Err(error) = recurring_resource_reclaimer.reconcile(now, 256).await {
                    eprintln!("resource reclamation retry remains pending: {error}");
                }
            }
            Ok(())
        });
    let mut managed_host = ManagedHost::new(host.clone())
        .with_resource_validator(resource_catalog.clone())
        .with_repository_binding_verifier(Arc::new(
            awaken_resource_application::CatalogRepositoryBindingVerifier::new(
                resource_catalog.clone(),
            ),
        ));
    if let Some(credentials) = credential_materializer {
        managed_host = managed_host.with_credential_materializer(credentials);
    }
    if let Some(factory) = credential_refresh_factory {
        managed_host = managed_host.with_credential_refresh_factory(factory);
    }
    managed_host = managed_host.install_dispatch_session_runtime();
    let managed_host = Arc::new(managed_host);
    let mut session_application =
        awaken_session_application::SessionApplication::new_with_configuration(
            managed_host.clone(),
            managed_host,
            sessions.clone(),
            environment_execution.clone(),
            awaken_session_application::SessionApplicationConfiguration {
                execution_placement: session_execution_placement,
                local_realization_owner: host.dispatch_owner().to_string(),
                ..Default::default()
            },
        );
    session_application.set_credential_source(credential_source);
    session_application.set_resource_catalog(resource_catalog.clone());
    session_application.set_resource_purge_scheduler(resource_application.purge_scheduler());
    session_application.set_resource_reference_authority(
        resource_authorities.reclamation(),
        resource_authorities.file_catalog(),
    );
    // Share the SAME config plane `/v1/agents` reads, so a session inheriting a
    // published agent's model sees the authoritative config-plane truth (M2).
    session_application.set_config_source(executable_agent_catalog.clone());
    session_application.set_lifecycle_notifier(webhook_notifier);
    if let Some(provider) = process.managed_services.list_price_provider.clone() {
        session_application.set_managed_list_price_provider(provider);
    }
    let session_application = Arc::new(session_application);
    let managed_state = Arc::new(ManagedState::from_application(session_application.clone()));
    // Workspace path addressing (ADR-0048 D3 / ADR-0051): wrap the fully-merged flat
    // surface so a `/v1/workspaces/{ws}/…` request is captured, rewritten to its flat
    // `/v1/…` form, and its `{ws}` stamped as the edge scope before it re-enters
    // routing. Flat requests fall through unchanged. The same process returns the
    // DreamApplication it mounted, so scheduling cannot target a parallel instance.
    // AllInOne and split Coordinator expose the same rebuildable runtime
    // projection. Process co-location never grants the Coordinator a second,
    // direct read path into Control catalog or credential authority.
    let model_inventory: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource> =
        executable_agent_catalog.clone();
    let deployment_session_launcher = Arc::new(
        awaken_protocol_managed::ManagedDeploymentSessionLauncher::new(managed_state.clone())
            .with_rate_limiter(managed_rate_limiter.clone()),
    );
    let private_router = executable_agent_private_router
        .merge(executable_environment_private_router)
        .merge(worker_observation_private_router);
    let resource_authorities = resource_application.authorities();
    let resource_management_router =
        awaken_coordinator::resources_router(awaken_coordinator::ResourcesRouterInput {
            files: resource_application.files(),
            memories: resource_authorities.memory_repository(),
            memory_stores: resource_application.memory_stores(),
            skills: Some(resource_authorities.skill_store()),
            purge: resource_application.purge_scheduler(),
        });
    let coordinator = awaken_coordinator::build_coordinator_component(
        awaken_coordinator::CoordinatorDependencies {
            service_lifecycle: process.service_lifecycle.clone(),
            host,
            session_application,
            managed_state,
            resource_catalog,
            resource_management_router,
            memory_stores: resource_application.memory_stores(),
            worker_file_application: resource_application.files(),
            worker_skill_bundles: resource_application.skill_bundle_source(),
            application_access,
            model_inventory,
            dream_process_store,
            worker_authenticator,
            worker_placement_policy,
            worker_directory: worker_directory.clone(),
            deployment_application,
            deployment_session_launcher,
            executable_agents: executable_agent_catalog,
            environments: environment_execution,
            sessions,
            default_workspace: platform_workspace.clone(),
            private_router,
        },
    )
    .await
    .map_err(|error| format!("build Coordinator component: {error}"))?;
    let coordinator_data = awaken_control::protect_runtime_protocol_routers(
        coordinator.router,
        coordinator.application_router,
        deployment_iam.clone(),
        deployment_remote_iam.clone(),
    );
    let coordinator_management = awaken_control::protect_management_router(
        coordinator.management_router,
        deployment_audit_plane.clone(),
        deployment_iam.clone(),
        deployment_remote_iam.clone(),
        Some(managed_rate_limiter.clone()),
    );
    let coordinator_managed = awaken_control::protect_management_router(
        coordinator.managed_router,
        deployment_audit_plane,
        deployment_iam,
        deployment_remote_iam,
        Some(managed_rate_limiter.clone()),
    );
    let data = executable_projection_refresh::layer(
        coordinator_data
            .merge(coordinator_managed)
            .merge(coordinator_management),
        executable_agent_projection_refresher,
        executable_environment_projection_refresher,
    );
    let (flat, mcp_export) = match role {
        config::Role::AllInOne => (data.merge(control), mcp_export),
        config::Role::Coordinator => (data, Router::new()),
        config::Role::Control => unreachable!("Control returned before Coordinator process"),
        config::Role::Worker => unreachable!("Worker has its own process startup"),
    };
    Ok(ProcessRouters::new(
        process_surface::finish(
            flat,
            mcp_export,
            reconciler,
            worker_observations,
            platform_workspace,
        ),
        coordinator.private_router,
        registration_supervisor,
        process.service_lifecycle,
    ))
}
