// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The one typed deployment resolver validates role, storage and lease
// @formal reason: authority before process adapters are assembled; moved unchanged from config.rs.
//! Resolve one authored deployment into its typed process topology.

use super::*;

impl ResolvedDeployment {
    pub(super) fn resolve_file(
        overrides: ConfigOverrides,
        home: Option<PathBuf>,
        config_path: PathBuf,
        file: FileConfig,
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, String> {
        let mut origins = BTreeMap::new();
        let (data_dir, data_dir_origin) = resolve_data_dir(
            overrides.data_dir.clone(),
            file.data_dir.clone(),
            home.as_deref(),
        )?;
        origins.insert("data_dir".to_owned(), data_dir_origin.to_owned());
        let expected_platform_workspace_id =
            resolve_expected_platform_workspace_id(file.expected_platform_workspace_id.as_deref())?;
        origins.insert(
            "expected_platform_workspace_id".to_owned(),
            if expected_platform_workspace_id.is_some() {
                "config.toml"
            } else {
                "not configured"
            }
            .to_owned(),
        );

        let mut bind = file.bind.clone().unwrap_or_else(|| DEFAULT_BIND.to_owned());
        if let Some(port) = overrides.port {
            bind = override_port(&bind, port)?;
            origins.insert("bind".to_owned(), "command line".to_owned());
        } else {
            origins.insert(
                "bind".to_owned(),
                if file.bind.is_some() {
                    "config.toml"
                } else {
                    "default"
                }
                .to_owned(),
            );
        }
        bind.parse::<std::net::SocketAddr>()
            .map_err(|_| format!("invalid bind address {bind:?}; expected IP:PORT"))?;
        let mode = file
            .mode
            .as_deref()
            .map(OperatingMode::parse)
            .transpose()?
            .unwrap_or_default();
        let role = overrides
            .role
            .or(file.role.as_deref().map(Role::parse).transpose()?)
            .unwrap_or(Role::AllInOne);
        service_boundary::enforce_worker_database_isolation(
            role,
            &[
                ("runtime_database_url", file.runtime_database_url.is_some()),
                (
                    "runtime_database_url_file",
                    file.runtime_database_url_file.is_some(),
                ),
                (
                    "resource_database_url",
                    file.resource_database_url.is_some(),
                ),
                (
                    "management_database_url_file",
                    file.management_database_url_file.is_some(),
                ),
                ("catalog_db", file.catalog_db.is_some()),
                ("credential_db", file.credential_db.is_some()),
                ("config_db", file.config_db.is_some()),
                ("admin_db", file.admin_db.is_some()),
                ("data_subject_db", file.data_subject_db.is_some()),
                ("environment_db", file.environment_db.is_some()),
                ("sessions_db", file.sessions_db.is_some()),
                ("captured_content_db", file.captured_content_db.is_some()),
                ("control_seal_key", file.control_seal_key.is_some()),
                (
                    "control_seal_key_file",
                    file.control_seal_key_file.is_some(),
                ),
            ],
        )?;
        service_boundary::enforce_control_execution_database_isolation(
            role,
            &[
                ("runtime_database_url", file.runtime_database_url.is_some()),
                (
                    "runtime_database_url_file",
                    file.runtime_database_url_file.is_some(),
                ),
                (
                    "resource_database_url",
                    file.resource_database_url.is_some(),
                ),
                ("sessions_db", file.sessions_db.is_some()),
                ("captured_content_db", file.captured_content_db.is_some()),
            ],
        )?;
        service_boundary::enforce_coordinator_control_database_isolation(
            role,
            &[
                (
                    "management_database_url_file",
                    file.management_database_url_file.is_some(),
                ),
                ("catalog_db", file.catalog_db.is_some()),
                ("credential_db", file.credential_db.is_some()),
                ("config_db", file.config_db.is_some()),
                ("admin_db", file.admin_db.is_some()),
                ("data_subject_db", file.data_subject_db.is_some()),
                ("environment_db", file.environment_db.is_some()),
                ("control_seal_key", file.control_seal_key.is_some()),
                (
                    "control_seal_key_file",
                    file.control_seal_key_file.is_some(),
                ),
            ],
        )?;
        let executable_agent_registration = ExecutableAgentRegistrationConfig::resolve(
            role,
            file.coordinator_internal_url.clone(),
            file.executable_agent_registration_token_file.clone(),
        )?;
        let worker_server = overrides.worker_server.or(file.worker_server.clone());
        if role == Role::Worker && worker_server.is_none() {
            return Err(
                "worker_server_required: pass --server or configure worker_server".to_owned(),
            );
        }
        worker_bootstrap::validate_file_ownership(role, mode, &file)?;
        let worker = WorkerBootstrap::resolve(
            awaken_worker::WorkerBootstrapInput {
                worker_id: file.worker_id.clone(),
                request_credential_file: file.worker_request_credential_file.clone(),
                kubernetes_identity_directory: file.worker_kubernetes_identity_directory.clone(),
                credential_material_root: file.worker_credential_material_root.clone(),
                credential_envelope_keys_file: file.credential_envelope_keys_file.clone(),
                credential_trust_domain: file.worker_credential_trust_domain.clone(),
                admin_listen: file.worker_admin_listen.clone(),
                drain_grace_secs: file.worker_drain_grace_secs,
                build_digest: file.worker_build_digest.clone(),
                zone: file.worker_zone.clone(),
                capabilities: file.worker_capabilities.clone(),
                max_concurrent: file.worker_max_concurrent,
                credential_probe_interval_secs: file.worker_credential_probe_interval_secs,
                credential_observation_ttl_secs: file.worker_credential_observation_ttl_secs,
            },
            &data_dir,
        )?;
        if role != Role::AllInOne && file.run_local_pool.is_some() {
            return Err(
                "run_local_pool is an all-in-one-only setting; Control and Coordinator never own a local claim pool, and Worker always runs its registered claim pool".to_owned(),
            );
        }
        let run_local_pool = match role {
            Role::AllInOne => file.run_local_pool.unwrap_or(true),
            Role::Worker => true,
            Role::Control | Role::Coordinator => false,
        };
        let dispatch_url = resolve_runtime_database_url(
            file.runtime_database_url.as_ref(),
            file.runtime_database_url_file.as_deref(),
        )?;
        let dispatch_backend =
            resolve_dispatch_backend(dispatch_url.as_ref(), role, run_local_pool)?;
        let control_service = ControlServiceConfig::resolve(
            role,
            file.control_internal_url.clone(),
            file.control_service_token_file.clone(),
        )?;
        let runtime_settings = runtime_settings::resolve(&file, &data_dir)?;
        let configured_acp_clis = file.acp_clis.as_ref().map(|_| {
            runtime_settings
                .acp
                .as_ref()
                .map(|profile| profile.cli_ids().map(str::to_owned).collect())
                .unwrap_or_default()
        });
        let postgres_max_connections = file
            .postgres_max_connections
            .map(|value| {
                std::num::NonZeroU32::new(value)
                    .ok_or_else(|| "postgres_max_connections must be greater than zero".to_owned())
            })
            .transpose()?
            .unwrap_or_else(awaken_session_executor::default_postgres_max_connections);
        for (field, value) in [
            ("otlp_protocol", file.otlp_protocol.as_deref()),
            ("otlp_traces_protocol", file.otlp_traces_protocol.as_deref()),
        ] {
            if let Some(value) = value
                && !matches!(value, "grpc" | "http/protobuf" | "http/json")
            {
                return Err(format!("invalid {field}={value:?}"));
            }
        }
        if file
            .otlp_headers
            .as_ref()
            .is_some_and(|headers| headers.keys().any(|key| key.trim().is_empty()))
        {
            return Err("otlp_headers keys must be non-empty".to_owned());
        }
        let observability = awaken_observability::ObservabilityConfig {
            filter: file.log_filter.clone().unwrap_or_else(|| "info".to_owned()),
            log_format: match file.log_format.as_deref() {
                Some("json") => awaken_observability::LogFormat::Json,
                Some("text") | None => awaken_observability::LogFormat::Text,
                Some(other) => return Err(format!("invalid log_format={other:?}")),
            },
            trace_file: file.trace_file.clone(),
            otel: awaken_observability::OtelConfig {
                endpoint: file.otlp_endpoint.clone(),
                traces_endpoint: file.otlp_traces_endpoint.clone(),
                protocol: file
                    .otlp_protocol
                    .as_deref()
                    .unwrap_or("http/protobuf")
                    .parse()
                    .unwrap_or_default(),
                traces_protocol: file
                    .otlp_traces_protocol
                    .as_deref()
                    .map(str::parse)
                    .transpose()
                    .unwrap_or_default(),
                headers: file
                    .otlp_headers
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                timeout: std::time::Duration::from_millis(file.otlp_timeout_ms.unwrap_or(10_000)),
                service_name: file.otel_service_name.clone(),
                service_version: file.otel_service_version.clone(),
                metric_export_interval: std::time::Duration::from_millis(
                    file.otel_metric_export_interval_ms.unwrap_or(60_000),
                ),
            },
        };
        if observability.filter.trim().is_empty()
            || observability.otel.timeout.is_zero()
            || observability.otel.metric_export_interval.is_zero()
        {
            return Err(
                "log_filter and OTel timeout/export interval must be non-empty/non-zero".to_owned(),
            );
        }
        let dispatch_owner = match file.dispatch_owner.as_deref() {
            Some(owner) if owner.trim().is_empty() => {
                return Err("dispatch_owner must be non-empty".to_owned());
            }
            Some(owner) => owner.trim().to_owned(),
            None => match role {
                // A co-located Worker must retain its logical identity across a
                // process restart so the new Runtime incarnation can fence the
                // old Session-realization lease immediately.
                Role::AllInOne => "embedded-worker".to_owned(),
                // Registered Worker identity already has one authoritative,
                // validated product value; do not mint a parallel owner name.
                Role::Worker => worker.worker_id.clone(),
                Role::Control | Role::Coordinator => {
                    format!("host-{}", std::process::id())
                }
            },
        };
        let mut runtime = DeploymentConfig {
            durable: true,
            storage_dir: Some(data_dir.clone()),
            store: if dispatch_url.is_some() {
                StoreKind::Postgres
            } else {
                StoreKind::Sqlite
            },
            dispatch_backend,
            wake: runtime_settings.wake,
            wake_channel: file
                .dispatch_wake_channel
                .clone()
                .unwrap_or_else(|| DEFAULT_WAKE_CHANNEL.to_owned()),
            nats_url: file.nats_url.clone(),
            database_url: dispatch_url.clone(),
            postgres_max_connections,
            dispatch_owner,
            dispatch_poll_interval: file
                .dispatch_poll_interval_ms
                .map(std::time::Duration::from_millis)
                .unwrap_or_else(|| awaken_dispatch::DispatchServiceConfig::default().poll_interval),
            sample_dispatch_queue_depth: true,
            sandbox_tier: runtime_settings.sandbox_tier,
            sandbox_dir: file.sandbox_dir.clone(),
            sandbox: runtime_settings.sandbox,
            content_capture: runtime_settings.content_capture,
            acp_session_blob_root: file.acp_session_blob_root.clone(),
            acp: runtime_settings.acp,
            container_image: file.container_image.clone(),
            disable_local_pool: !run_local_pool,
        };
        runtime.validate_local_execution()?;
        if role == Role::Worker {
            // The registered Worker receives durable claims through its injected
            // HTTP dispatch store. Everything it owns locally is ephemeral
            // execution state; retaining the product data directory here would
            // make `ExecutorComponents` silently open Session/File/Memory SQLite stores.
            runtime.storage_dir = None;
            runtime.database_url = None;
            runtime.dispatch_backend = DispatchBackend::Sqlite;
            runtime.store = StoreKind::Sqlite;
            runtime.nats_url = None;
        }
        let shared_management_database = file
            .management_database_url_file
            .as_deref()
            .map(|path| read_database_url_file(path, "management_database_url_file"))
            .transpose()?;
        if shared_management_database.is_some()
            && [
                &file.resource_database_url,
                &file.catalog_db,
                &file.credential_db,
                &file.config_db,
                &file.admin_db,
                &file.data_subject_db,
                &file.environment_db,
                &file.sessions_db,
                &file.captured_content_db,
            ]
            .into_iter()
            .any(Option::is_some)
        {
            return Err(
                "management_database_url_file cannot be combined with per-store database URLs"
                    .to_owned(),
            );
        }
        if shared_management_database.is_some() {
            origins.insert(
                "management_database".to_owned(),
                "management_database_url_file".to_owned(),
            );
        }
        let store_url = |specific: &Option<String>| {
            select_store_url(specific.as_ref(), None, shared_management_database.as_ref())
        };
        let execution_store_url = |specific: &Option<String>| {
            select_store_url(
                specific.as_ref(),
                (role == Role::Coordinator)
                    .then_some(dispatch_url.as_ref())
                    .flatten(),
                shared_management_database.as_ref(),
            )
        };
        let control = awaken_control::ControlStoreConfig::from_values(
            &data_dir,
            store_url(&file.catalog_db),
            store_url(&file.credential_db),
            store_url(&file.config_db),
            store_url(&file.admin_db),
            store_url(&file.data_subject_db),
            store_url(&file.environment_db),
        );
        let coordinator = CoordinatorStoreConfig {
            sessions: awaken_control::StoreBackend::resolve(
                execution_store_url(&file.sessions_db),
                data_dir.join("sessions.db"),
            ),
            captured_content: awaken_control::StoreBackend::resolve(
                execution_store_url(&file.captured_content_db),
                data_dir.join("captured_content.db"),
            ),
        };
        let workspace_data_lease_guard =
            workspace_data::lease_guard(file.workspace_data.as_ref(), clock.clone());
        let resources = workspace_data::resolve(
            execution_store_url(&file.resource_database_url),
            file.workspace_data.as_ref(),
            data_dir.clone(),
            clock.as_ref(),
        )?;
        resources
            .validate_dispatch_compatibility(dispatch_backend == DispatchBackend::Postgres)
            .map_err(str::to_owned)?;
        let seal_key = SealKeySource::from_config(
            role,
            &file.control_seal_key,
            &file.control_seal_key_file,
            mode,
            &data_dir,
        )?;
        let interactive_product = mode == OperatingMode::Local && role == Role::AllInOne;
        let identity_mode = overrides
            .identity_mode
            .or(file
                .identity_mode
                .as_deref()
                .map(|value| {
                    awaken_control::ManagementIdentityMode::parse(value).ok_or_else(|| {
                        format!("invalid identity_mode {value:?}: expected no-login, awaken-cloud, or self-managed")
                    })
                })
                .transpose()?)
            .unwrap_or(if interactive_product {
                awaken_control::ManagementIdentityMode::AwakenCloud
            } else {
                awaken_control::ManagementIdentityMode::SelfManaged
            });
        let cloud_models = overrides
            .cloud_models
            .or(file
                .cloud_models
                .as_deref()
                .map(CloudModelMode::parse)
                .transpose()?)
            .unwrap_or(
                if interactive_product
                    && identity_mode == awaken_control::ManagementIdentityMode::AwakenCloud
                {
                    CloudModelMode::Enabled
                } else {
                    CloudModelMode::Disabled
                },
            );
        if cloud_models.is_enabled()
            && identity_mode != awaken_control::ManagementIdentityMode::AwakenCloud
        {
            return Err(
                "cloud_models_require_awaken_cloud_identity: cloud_models=enabled requires identity_mode=awaken-cloud"
                    .to_owned(),
            );
        }
        let cloud_iam = CloudIamConfig::resolve(&file, mode, identity_mode)?;
        let suite_hub_url = file
            .suite_hub_url
            .as_deref()
            .map(|value| validate_suite_hub_url(value, mode))
            .transpose()?;
        let (ai_sdk_browser_cors, ai_sdk_browser_origins_source) =
            browser_access::resolve(file.ai_sdk_browser_origins.clone(), role)?;
        origins.insert(
            "ai_sdk_browser_origins".to_owned(),
            ai_sdk_browser_origins_source.to_owned(),
        );
        let internal_bind = service_boundary::resolve_internal_bind(
            role,
            file.internal_bind.clone(),
            &bind,
            run_local_pool,
        )?;
        origins.insert(
            "internal_bind".to_owned(),
            if file.internal_bind.is_some() {
                "config.toml"
            } else {
                "not applicable"
            }
            .to_owned(),
        );
        Ok(Self {
            role,
            mode,
            bind,
            internal_bind,
            data_dir,
            expected_platform_workspace_id,
            config_file_exists: config_path.exists(),
            config_path,
            no_browser: overrides.no_browser.or(file.no_browser).unwrap_or(false),
            suite_hub_url,
            ai_sdk_browser_cors,
            run_local_pool,
            worker_server,
            worker,
            worker_trust_credentials_file: file.worker_trust_credentials_file,
            worker_kubernetes: file.worker_kubernetes,
            identity_mode,
            cloud_models,
            org_id: file
                .org_id
                .unwrap_or_else(|| awaken_control::DEFAULT_ORG_ID.to_owned()),
            iam_workspaces: file.iam_workspaces.unwrap_or_default(),
            cloud_iam,
            executable_agent_registration,
            control_service,
            mcp_bearer_token: file.mcp_bearer_token,
            admin_listen: file.admin_listen,
            runtime,
            observability,
            local_acp_observations: Vec::new(),
            configured_acp_clis,
            control,
            coordinator,
            resources,
            workspace_data_lease_guard,
            seal_key,
            deprecations: Vec::new(),
            origins,
        })
    }
}
