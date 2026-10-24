// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: This source implements the repository-controlled assembly product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Deployment-selected runtime process and migration process.

use super::*;

pub(super) async fn prepare_runtime_process(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
    role: config::Role,
    model_supply: PublicationModelSupply,
    managed_services: ManagedServiceAdapters,
) -> Result<PreparedProcess, String> {
    prepare_runtime_process_with_coordinator_services(
        deployment,
        key,
        role,
        model_supply,
        managed_services,
        CoordinatorServiceAdapters::default(),
    )
    .await
}

pub(super) async fn prepare_runtime_process_with_coordinator_services(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
    role: config::Role,
    model_supply: PublicationModelSupply,
    managed_services: ManagedServiceAdapters,
    coordinator_services: CoordinatorServiceAdapters,
) -> Result<PreparedProcess, String> {
    let installations = installation_binding::verify_deployment_installations(deployment).await?;
    prepare_runtime_process_with_coordinator_services_and_installations(
        deployment,
        key,
        role,
        model_supply,
        (managed_services, coordinator_services),
        installations,
        process_clock_for(Some(deployment)),
    )
    .await
}

/// Direct embedding chooses a process clock here; an authored Workspace data
/// guard already carries the clock selected when its lease was admitted.
pub(super) fn process_clock_for(
    deployment: Option<&config::ResolvedDeployment>,
) -> std::sync::Arc<dyn awaken_time::WallClock> {
    deployment
        .and_then(|deployment| deployment.workspace_data_lease_guard.as_ref())
        .map(config::WorkspaceDataLeaseGuard::clock)
        .unwrap_or_else(awaken_time::system_clock)
}

pub(super) async fn prepare_runtime_process_with_coordinator_services_and_installations(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
    role: config::Role,
    model_supply: PublicationModelSupply,
    services: (ManagedServiceAdapters, CoordinatorServiceAdapters),
    installations: installation_binding::PreparedDeploymentInstallations,
    clock: std::sync::Arc<dyn awaken_time::WallClock>,
) -> Result<PreparedProcess, String> {
    let (mut managed_services, coordinator_services) = services;
    debug_assert!(matches!(
        role,
        config::Role::AllInOne | config::Role::Coordinator
    ));
    // The process's one wall clock: Worker assertions, stores and every
    // Coordinator component built below judge time with this handle.
    let process_clock = clock.clone();
    let worker_transport = worker_transport_security::select(
        deployment,
        coordinator_services.worker_authenticator,
        clock.clone(),
    )
    .await?;
    let publish_local_workspace = installations.publishes_local_workspace();
    let platform_workspace = installations.platform_workspace_before_write()?;
    let service_lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let credential_material_resolver = managed_services.install_credential_envelope(
        deployment.worker.credential_envelope_keys_file.as_deref(),
        clock.clone(),
    )?;
    let background_services = std::mem::take(&mut managed_services.background_services);
    let postgres_schema = match deployment.mode {
        config::OperatingMode::Local => PostgresSchemaMode::Migrate,
        config::OperatingMode::Server => PostgresSchemaMode::Verify,
    };
    let storage = deployment.runtime.coordinator_storage();
    let persistence = match deployment.mode {
        config::OperatingMode::Local => {
            awaken_coordinator::open_coordinator_persistence(&storage, clock.clone()).await?
        }
        config::OperatingMode::Server => {
            awaken_coordinator::open_existing_coordinator_persistence(&storage, clock.clone())
                .await?
        }
    };
    let mut postgres_pools = crate::postgres_pools::ProcessPostgresPools::new(
        deployment.runtime.postgres_max_connections.get(),
    );
    if let Some(pool) = persistence.postgres_pool.clone() {
        postgres_pools.seed(
            deployment
                .runtime
                .database_url
                .as_deref()
                .expect("persistence requires database URL"),
            pool,
        )?;
    }
    let worker_directory = persistence.worker_directory.clone();
    let worker_observations = worker_observation_wiring::WorkerObservationWiring::runtime(
        role,
        deployment,
        worker_directory.clone(),
    )?;
    let resources = crate::process_stores::ProcessResources::open(
        deployment.resources.clone(),
        postgres_schema,
        clock.clone(),
        &mut postgres_pools,
    )
    .await?;
    let stores = open_process_stores(ProcessStoreOpenOptions {
        control: deployment.control.clone(),
        coordinator: deployment.coordinator.clone(),
        resources: Some(resources),
        postgres_pools: &mut postgres_pools,
        workspace_root: deployment.data_dir.clone(),
        platform_workspace,
        publish_local_workspace,
        seal_key: key,
        role,
        postgres_schema,
        clock,
    })
    .await?;
    // Store startup has already opened the canonical Session authority and
    // resolved the one installation Workspace. Identity wiring consumes that
    // coordinate; it is no longer a second publisher with mode-dependent timing.
    let identity = identity_wiring(
        deployment,
        &stores.platform_workspace,
        awaken_iam_client::CredentialCache::open(),
        managed_services.entitlement_provider.take(),
        stores.clock.clone(),
    )
    .await?;
    let coordinator_authorities =
        stores
            .coordinator
            .as_ref()
            .map(|stores| CoordinatorAuthorityHandles {
                runtime_authority: persistence.runtime_authority.clone(),
                worker_directory: persistence.worker_directory.clone(),
                sessions: stores.sessions.clone(),
                postgres_pool: persistence.postgres_pool.clone(),
                run_recovery: persistence.run_recovery.clone(),
                run_lifecycle: persistence.run_lifecycle.clone(),
            });
    let coordinator_stores = stores
        .coordinator
        .as_ref()
        .expect("Managed Execution owns WorkQueue");
    let executable_environment_wiring = executable_environment_registration::for_runtime_role(
        role,
        deployment,
        postgres_schema,
        coordinator_stores.environment_work.clone(),
        &service_lifecycle,
        &mut postgres_pools,
        stores.clock.clone(),
    )
    .await?;
    let executable_agent_wiring = executable_agent_registration::for_runtime_role(
        role,
        deployment,
        postgres_schema,
        coordinator_stores.captured_content_eraser.clone(),
        coordinator_stores
            .resources
            .application
            .authorities()
            .reclamation(),
        &mut postgres_pools,
    )
    .await?;
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
            namespace_hand_path: None,
            content_capture_ceiling: deployment.runtime.content_capture.level,
            org_id: Some(deployment.org_id.clone()),
            enrollment_signing_key: key
                .map(awaken_data_subject_application::derive_enrollment_signing_key),
            mcp_bearer_token: deployment.mcp_bearer_token.clone(),
            ai_sdk_browser_cors: deployment.ai_sdk_browser_cors.clone(),
            role,
            cloud_api_base_url: Some(deployment.cloud_iam.inference_base_url.clone()),
            cloud_developer_key_file: deployment.cloud_iam.developer_key_file.clone(),
            model_supply: local_model_supply(deployment.cloud_models.is_enabled()),
            brokered_catalog: None,
            cloud_login: identity.cloud_login,
            local_acp_observations: deployment.local_acp_observations.clone(),
            web_search_providers: None,
            web_search_publication_resolver: None,
            executable_agent_wiring: Some(executable_agent_wiring),
            executable_environment_wiring: Some(executable_environment_wiring),
            worker_transport: Some(worker_transport),
            worker_placement_policy: coordinator_services.worker_placement_policy,
            cloud_native_credential_realization: coordinator_services
                .cloud_native_credential_realization,
            repository_transport_authorizer: coordinator_services.repository_transport_authorizer,
            inference_materializer: coordinator_services.inference_materializer,
            worker_directory: Some(worker_directory),
            runtime_authority: Some(persistence.runtime_authority.clone()),
            worker_observations: Some(worker_observations),
            control_service_authenticator: None,
            relay_destination_authenticator: coordinator_services.relay_destination_authenticator,
            control_service,
            additional_lifecycle_delivery: None,
            credential_material_resolver,
            managed_services,
        },
        None,
    )
    .await?;
    managed_platform::install_background_services(
        &prepared.service_lifecycle,
        &background_services,
    );
    Ok(PreparedProcess {
        public_router: prepared.public_router,
        private_router: prepared.private_router,
        local_setup: identity.local_setup,
        registration_supervisor: prepared.registration_supervisor,
        service_lifecycle: prepared.service_lifecycle,
        event_batch_cutover_validation: prepared.event_batch_cutover_validation,
        coordinator_authorities,
        worker_authenticator: prepared.worker_authenticator,
        admin_tools: prepared.admin_tools,
        clock: process_clock,
    })
}

/// Explicit deployment migration phase for every role-owned store.
/// Local SQLite startup retains auto-migration; managed PostgreSQL deployments
/// invoke this command before starting application Pods.
pub async fn migrate_deployment_schema(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
) -> Result<(), String> {
    let prepared = installation_binding::prepare_deployment_installations(
        deployment,
        &installation_binding::InstallationAuthorization::default(),
    )
    .await?;
    migrate_deployment_schema_prepared(deployment, key, prepared).await
}

/// Apply role-owned migrations after the exact installation preflight has
/// already succeeded. Consuming the opaque proof keeps the authorized CLI path
/// from re-entering ordinary/default admission and losing its explicit grant.
pub(crate) async fn migrate_deployment_schema_prepared(
    deployment: &config::ResolvedDeployment,
    key: Option<&[u8; 32]>,
    prepared: installation_binding::PreparedDeploymentInstallations,
) -> Result<(), String> {
    let publish_local_workspace = prepared.publishes_local_workspace();
    let platform_workspace = prepared.platform_workspace_before_write()?;
    let manifest = migration_manifest(deployment.role);
    // The migration process's one wall clock.
    let clock = awaken_time::system_clock();
    let mut postgres_pools = crate::postgres_pools::ProcessPostgresPools::new(
        deployment.runtime.postgres_max_connections.get(),
    );
    let resources = if manifest.contains(&MigrationComponent::Resources) {
        Some(
            crate::process_stores::ProcessResources::open(
                deployment.resources.clone(),
                PostgresSchemaMode::Migrate,
                clock.clone(),
                &mut postgres_pools,
            )
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
            postgres_pools: &mut postgres_pools,
            workspace_root: deployment.data_dir.clone(),
            platform_workspace,
            publish_local_workspace,
            seal_key: key,
            role: deployment.role,
            postgres_schema: PostgresSchemaMode::Migrate,
            clock,
        })
        .await
        .map(drop)?;
    }
    if manifest.contains(&MigrationComponent::Coordinator) {
        executable_agent_registration::migrate(deployment, &mut postgres_pools).await?;
        executable_environment_registration::migrate(deployment, &mut postgres_pools).await?;
        if deployment.runtime.dispatch_backend
            == awaken_deployment_config::DispatchBackend::Postgres
            || deployment.runtime.store == awaken_deployment_config::StoreKind::Postgres
        {
            let url = deployment
                .runtime
                .database_url
                .as_deref()
                .ok_or("PostgreSQL Coordinator migration requires runtime.database_url")?;
            let pool = postgres_pools.get(url).await?;
            awaken_coordinator::migrate_postgres_coordinator_schema_with_pool(
                &deployment.runtime.coordinator_storage(),
                pool,
            )
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migration_fences_changed_local_proof_before_opening_any_store() {
        /* Cause/effect graph: C1 canonical Session storage and marker A pass the
         * one installation preflight; C2 the marker becomes B before the opaque
         * proof reaches the migration opener. Effects: E1 the opener reports
         * local_installation_changed; E2 no Session/ledger/schema byte changes;
         * E3 B is not overwritten. Decision rule M1: C1+C2 => E1+E2+E3. The
         * assertion enters the production migrate seam, so helper correctness and
         * caller ordering are covered together. */
        fn storage_bytes(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
            std::fs::read_dir(root)
                .unwrap()
                .filter_map(|entry| {
                    let entry = entry.unwrap();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    (name != "platform-workspace-id")
                        .then(|| (name, std::fs::read(entry.path()).unwrap()))
                })
                .collect()
        }

        let directory = tempfile::tempdir().unwrap();
        let sessions = directory.path().join("sessions.db");
        awaken_session_store::SqliteManagedSessionRepository::open(&sessions.to_string_lossy())
            .await
            .unwrap()
            .close()
            .await;
        awaken_installation_workspace::publish_at(directory.path(), "workspace-a").unwrap();
        let deployment = config::local_test_deployment(directory.path().to_path_buf());
        let prepared = installation_binding::prepare_deployment_installations(
            &deployment,
            &installation_binding::InstallationAuthorization::default(),
        )
        .await
        .expect("M1 exact preflight");
        let before = storage_bytes(directory.path());
        std::fs::write(
            directory.path().join("platform-workspace-id"),
            "workspace-b",
        )
        .unwrap();

        let error = migrate_deployment_schema_prepared(&deployment, None, prepared)
            .await
            .expect_err("M1 changed proof rejects before migrations");
        assert!(
            error.starts_with("local_installation_changed:"),
            "M1/E1: {error}"
        );
        assert_eq!(storage_bytes(directory.path()), before, "M1/E2");
        assert_eq!(
            std::fs::read_to_string(directory.path().join("platform-workspace-id")).unwrap(),
            "workspace-b",
            "M1/E3"
        );
    }

    async fn runtime_startup_error(deployment: &config::ResolvedDeployment) -> String {
        match prepare_runtime_process(
            deployment,
            None,
            config::Role::AllInOne,
            PublicationModelSupply::PublishedProviders,
            ManagedServiceAdapters::default(),
        )
        .await
        {
            Ok(_) => panic!("invalid initialized Session storage must block runtime startup"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    async fn runtime_and_migration_fail_before_mutating_invalid_initialized_session_storage() {
        /* Cause/effect graph: C1 deployment identity/resume fence absent/present;
         * C2 marker absent/matching; C3 Session DB missing/zero/corrupt/valid.
         * Effects: E1 only marker+valid Session enters the ordinary opener;
         * E2 missing/partial identity, any zero/corrupt DB, or a missing expected
         * marker blocks before stores, migrations, or identity writes. Decision
         * table: SS1=!marker+missing=>explicit initialization required (service
         * migration tests); SS2=C1+matching+valid=>canonical opener/migration;
         * SS3=C1+matching+missing=>missing+E2;
         * SS4=zero=>empty+E2; SS5=corrupt=>invalid+E2; SS6=C1+!marker=>
         * platform_workspace_missing+E2. */
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("platform-workspace-id");
        std::fs::write(&marker, b"workspace_local_regression").unwrap();
        let sessions = directory.path().join("sessions.db");
        let mut deployment = crate::config::local_test_deployment(directory.path().to_path_buf());
        deployment.expected_platform_workspace_id = Some("workspace_local_regression".into());

        let runtime_error = runtime_startup_error(&deployment).await;
        assert!(
            runtime_error.starts_with("session_storage_missing:"),
            "SS3 stable runtime diagnostic: {runtime_error}"
        );
        assert!(!sessions.exists(), "SS3 runtime creates no Session DB");
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            b"workspace_local_regression",
            "SS3 runtime preserves marker"
        );

        let migration_error = migrate_deployment_schema(&deployment, None)
            .await
            .expect_err("SS3 migration must reject missing initialized Session storage");
        assert!(
            migration_error.starts_with("session_storage_missing:"),
            "SS3 stable migration diagnostic: {migration_error}"
        );
        assert!(!sessions.exists(), "SS3 migration creates no Session DB");
        assert_eq!(
            std::fs::read(&marker).unwrap(),
            b"workspace_local_regression",
            "SS3 migration preserves marker"
        );

        std::fs::write(&sessions, []).unwrap();
        let runtime_error = runtime_startup_error(&deployment).await;
        assert!(
            runtime_error.starts_with("session_storage_empty:"),
            "SS4 stable runtime diagnostic: {runtime_error}"
        );
        let migration_error = migrate_deployment_schema(&deployment, None)
            .await
            .expect_err("SS4 migration must reject zero-byte initialized Session storage");
        assert!(
            migration_error.starts_with("session_storage_empty:"),
            "SS4 stable migration diagnostic: {migration_error}"
        );
        assert_eq!(std::fs::metadata(&sessions).unwrap().len(), 0, "SS4/E3");

        let truncated = b"SQLite format 3\0truncated";
        std::fs::write(&sessions, truncated).unwrap();
        let runtime_error = runtime_startup_error(&deployment).await;
        assert!(
            runtime_error.starts_with("session_storage_invalid:"),
            "SS5 stable runtime diagnostic: {runtime_error}"
        );
        let migration_error = migrate_deployment_schema(&deployment, None)
            .await
            .expect_err("SS5 migration must reject corrupt initialized Session storage");
        assert!(
            migration_error.starts_with("session_storage_invalid:"),
            "SS5 stable migration diagnostic: {migration_error}"
        );
        assert_eq!(std::fs::read(&sessions).unwrap(), truncated, "SS5/E3");
        assert_eq!(
            std::fs::read(marker).unwrap(),
            b"workspace_local_regression",
            "SS3-SS5 preserve the initialization marker"
        );

        let uninitialized = tempfile::tempdir().unwrap();
        let sessions = uninitialized.path().join("sessions.db");
        std::fs::write(&sessions, truncated).unwrap();
        let deployment = crate::config::local_test_deployment(uninitialized.path().to_path_buf());
        let runtime_error = runtime_startup_error(&deployment).await;
        assert!(
            runtime_error.starts_with("session_storage_invalid:"),
            "SS5 corrupt existing storage blocks without a marker: {runtime_error}"
        );
        assert!(
            !uninitialized.path().join("platform-workspace-id").exists(),
            "SS5 failed startup does not publish an identity marker"
        );
        assert_eq!(
            std::fs::read(sessions).unwrap(),
            truncated,
            "SS5 failed startup preserves corrupt evidence"
        );

        let empty = tempfile::tempdir().unwrap();
        let mut deployment = crate::config::local_test_deployment(empty.path().to_path_buf());
        deployment.expected_platform_workspace_id = Some("workspace_local_regression".into());
        let runtime_error = runtime_startup_error(&deployment).await;
        assert!(
            runtime_error.starts_with("platform_workspace_missing:"),
            "SS6 stable runtime diagnostic: {runtime_error}"
        );
        assert_eq!(
            std::fs::read_dir(empty.path()).unwrap().count(),
            0,
            "SS6/E2"
        );
    }
}
