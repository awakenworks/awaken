//! Deployment-selected runtime process and migration process.

use super::*;

pub(super) async fn prepare_runtime_process(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
    role: config::Role,
    model_supply: PublicationModelSupply,
) -> Result<PreparedProcess, String> {
    debug_assert!(matches!(
        role,
        config::Role::AllInOne | config::Role::Coordinator
    ));
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let identity = identity_wiring(
        deployment.identity_mode,
        Some(&deployment.data_dir),
        &deployment.org_id,
        &deployment.iam_workspaces,
        &deployment.cloud_iam,
    )?;
    let postgres_schema = match deployment.mode {
        config::OperatingMode::Local => PostgresSchemaMode::Migrate,
        config::OperatingMode::Server => PostgresSchemaMode::Verify,
    };
    let persistence = match deployment.mode {
        config::OperatingMode::Local => {
            awaken_coordinator::open_coordinator_persistence(&deployment.runtime).await?
        }
        config::OperatingMode::Server => {
            awaken_coordinator::open_existing_coordinator_persistence(&deployment.runtime).await?
        }
    };
    let worker_directory = persistence.worker_directory.clone();
    let worker_observations = worker_observation_wiring::WorkerObservationWiring::runtime(
        role,
        deployment,
        worker_directory.clone(),
    )?;
    let resources =
        open_resources_application(deployment.resources.clone(), postgres_schema).await?;
    let stores = open_process_stores(ProcessStoreOpenOptions {
        control: deployment.control.clone(),
        coordinator: deployment.coordinator.clone(),
        resources: Some(resources),
        workspace_root: deployment.data_dir.clone(),
        seal_key: key,
        role,
        postgres_schema,
    })
    .await?;
    let executable_environment_wiring = executable_environment_registration::for_runtime_role(
        role,
        deployment,
        postgres_schema,
        stores
            .coordinator
            .as_ref()
            .expect("Managed Execution owns WorkQueue")
            .environment_work
            .clone(),
        &service_lifecycle,
    )
    .await?;
    let executable_agent_wiring = executable_agent_registration::for_runtime_role(
        role,
        deployment,
        postgres_schema,
        stores
            .coordinator
            .as_ref()
            .expect("Managed Execution owns captured content")
            .captured_content_eraser
            .clone(),
    )
    .await?;
    let worker_authenticator = worker_transport_security::authenticator(deployment)?;
    let control_service = if role == config::Role::Coordinator {
        let (url, token_source) = deployment.control_service.coordinator_credentials()?;
        Some(ControlServices::remote(Arc::new(
            awaken_coordinator::control_service_boundary::HttpControlServiceClient::with_token_source(
                url,
                token_source,
            )?,
        )))
    } else {
        None
    };
    let prepared = prepare_runtime_routers(
        stores,
        identity.iam,
        identity.remote_iam,
        identity.local_browser_auth,
        model_supply,
        ProcessStartup {
            service_lifecycle: service_lifecycle.clone(),
            deployment: Some(deployment.runtime.clone()),
            content_capture_ceiling: deployment.runtime.content_capture.level,
            org_id: Some(deployment.org_id.clone()),
            enrollment_signing_key: key
                .map(awaken_data_subject_application::derive_enrollment_signing_key),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            role,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            model_supply: local_model_supply(deployment.cloud_models.is_enabled()),
            brokered_catalog: None,
            local_acp_observations: deployment.local_acp_observations.clone(),
            web_search_providers: None,
            web_search_publication_resolver: None,
            executable_agent_wiring: Some(executable_agent_wiring),
            executable_environment_wiring: Some(executable_environment_wiring),
            worker_authenticator: Some(worker_authenticator),
            worker_directory: Some(worker_directory),
            runtime_authority: Some(persistence.runtime_authority),
            worker_observations: Some(worker_observations),
            control_service_authenticator: None,
            control_service,
            additional_lifecycle_delivery: None,
        },
        None,
    )
    .await?;
    Ok(PreparedProcess {
        public_router: prepared.public_router,
        private_router: prepared.private_router,
        local_setup: identity.local_setup,
        registration_supervisor: prepared.registration_supervisor,
        service_lifecycle: prepared.service_lifecycle,
    })
}

/// Explicit deployment migration phase for every role-owned store.
/// Local SQLite startup retains auto-migration; managed PostgreSQL deployments
/// invoke this command before starting application Pods.
pub async fn migrate_deployment_schema(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
) -> Result<(), String> {
    let manifest = migration_manifest(deployment.role);
    let resources = if manifest.contains(&MigrationComponent::Resources) {
        Some(
            open_resources_application(deployment.resources.clone(), PostgresSchemaMode::Migrate)
                .await?,
        )
    } else {
        None
    };
    if manifest.contains(&MigrationComponent::Control)
        || manifest.contains(&MigrationComponent::Coordinator)
    {
        open_process_stores(ProcessStoreOpenOptions {
            control: deployment.control.clone(),
            coordinator: deployment.coordinator.clone(),
            resources,
            workspace_root: deployment.data_dir.clone(),
            seal_key: key,
            role: deployment.role,
            postgres_schema: PostgresSchemaMode::Migrate,
        })
        .await
        .map(drop)?;
    }
    if manifest.contains(&MigrationComponent::Coordinator) {
        executable_agent_registration::migrate(deployment).await?;
        executable_environment_registration::migrate(deployment).await?;
        awaken_coordinator::migrate_postgres_coordinator_schema(&deployment.runtime).await?;
    }
    Ok(())
}
