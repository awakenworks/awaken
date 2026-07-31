//! Product bootstrap configuration for the `awaken` command.
//!
//! It resolves an explicit `--config` path or the standard
//! `~/.awaken/config.toml`, command-line presentation overrides, and defaults
//! into one [`ResolvedDeployment`]. Process environment is not a deployment,
//! business, model, or credential configuration source. The
//! server, runtime host, worker, diagnostics, and storage commands consume that
//! typed value; they do not independently rediscover the deployment.

use std::collections::BTreeMap;
use std::fs;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use awaken_runtime_host::{
    AcpWorkerProfile, ContentCaptureSettings, ContentRedaction, DeploymentConfig, DispatchBackend,
    SandboxSettings, SandboxTier, StoreKind, Wake,
};
use serde::Deserialize;

mod file_support;
mod report;
mod role;
mod service_boundary;
mod worker_bootstrap;

use file_support::{
    home_dir, is_postgres_url, override_port, read_management_database_url,
    read_or_create_local_key, validate_suite_hub_url,
};
pub use role::Role;
pub use service_boundary::{DeploymentSessionLaunchConfig, ExecutableAgentRegistrationConfig};
pub use worker_bootstrap::WorkerBootstrap;

pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
const DEFAULT_WAKE_CHANNEL: &str = "awaken_dispatch_wake";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OperatingMode {
    #[default]
    Local,
    Server,
}

impl OperatingMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            other => Err(format!("invalid mode={other:?}: expected local or server")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

/// CLI presentation values have the highest precedence. `None` leaves
/// resolution to the typed config file and defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub config_path: Option<PathBuf>,
    pub role: Option<Role>,
    pub data_dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub no_browser: Option<bool>,
    pub worker_server: Option<String>,
    pub identity_mode: Option<awaken_control::ManagementIdentityMode>,
    pub cloud_models: Option<CloudModelMode>,
}

/// Whether this process may project and execute Awaken Cloud subscription models.
/// Cloud identity remains independent so users may sign in while staying BYOK-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CloudModelMode {
    #[default]
    Disabled,
    Enabled,
}

impl CloudModelMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" | "false" => Ok(Self::Disabled),
            "enabled" | "on" | "true" => Ok(Self::Enabled),
            other => Err(format!(
                "invalid cloud_models={other:?}: expected disabled or enabled"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
        }
    }

    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// One backend family for the complete resource plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourcePlaneStoreBackend {
    Embedded(PathBuf),
    Postgres(String),
}

impl ResourcePlaneStoreBackend {
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Postgres(_))
    }

    pub fn validate_runtime_shape(
        &self,
        shared_runtime: bool,
        shared_resource_catalog: bool,
    ) -> Result<(), &'static str> {
        if shared_runtime && !self.is_shared() {
            Err("a shared Postgres runtime requires resource_database_url")
        } else if shared_runtime && !shared_resource_catalog {
            Err("a shared Postgres runtime requires admin_db to use Postgres")
        } else {
            Ok(())
        }
    }
}

#[derive(Clone)]
pub enum SealKeySource {
    NotOwnedByRole,
    Inline(String),
    File(PathBuf),
    LocalFile(PathBuf),
}

impl std::fmt::Debug for SealKeySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOwnedByRole => formatter.write_str("NotOwnedByRole"),
            Self::Inline(_) => formatter.write_str("Inline([REDACTED])"),
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
            Self::LocalFile(path) => formatter.debug_tuple("LocalFile").field(path).finish(),
        }
    }
}

impl SealKeySource {
    pub fn description(&self) -> String {
        match self {
            Self::NotOwnedByRole => "not owned by this role".to_owned(),
            Self::Inline(_) => "config.toml (redacted)".to_owned(),
            Self::File(path) | Self::LocalFile(path) => path.display().to_string(),
        }
    }

    /// Resolve the existing operator key or create the Local-mode key exactly
    /// once with owner-only permissions. The returned bytes are never logged.
    pub fn load_or_create(&self) -> Result<[u8; 32], String> {
        let value = match self {
            Self::NotOwnedByRole => {
                return Err("this process role does not own the Control seal key".to_owned());
            }
            Self::Inline(value) => value.clone(),
            Self::File(path) => fs::read_to_string(path)
                .map_err(|error| format!("read seal key {}: {error}", path.display()))?,
            Self::LocalFile(path) => read_or_create_local_key(path)?,
        };
        awaken_credential_vault::parse_seal_key(value.trim())
            .map_err(|reason| format!("invalid control-plane seal key: {reason}"))
    }
}

/// Fully resolved bootstrap truth shared by the command, server assembly, and
/// runtime host. Database URLs and key material are intentionally absent from
/// its rendered reports.
#[derive(Debug, Clone)]
pub struct ResolvedDeployment {
    pub role: Role,
    pub mode: OperatingMode,
    pub bind: String,
    pub data_dir: PathBuf,
    pub config_path: PathBuf,
    pub config_file_exists: bool,
    pub no_browser: bool,
    /// Optional deployment-owned suite hub shown by the browser console.
    pub suite_hub_url: Option<String>,
    pub run_local_pool: bool,
    pub worker_server: Option<String>,
    pub worker: WorkerBootstrap,
    /// Coordinator-owned enrollment file for authenticated Worker requests.
    /// It contains no database or Control sealing material.
    pub worker_trust_credentials_file: Option<PathBuf>,
    pub identity_mode: awaken_control::ManagementIdentityMode,
    pub cloud_models: CloudModelMode,
    pub org_id: String,
    pub iam_workspaces: Vec<String>,
    pub cloud_iam: CloudIamConfig,
    pub executable_agent_registration: ExecutableAgentRegistrationConfig,
    pub deployment_session_launch: DeploymentSessionLaunchConfig,
    pub mcp_bearer_token: Option<String>,
    pub admin_listen: Option<String>,
    pub runtime: DeploymentConfig,
    /// Deployment-owned topology for logical Agent `hand` declarations.
    pub hand_connections: BTreeMap<String, awaken_connection_plan::ConnectionPlan>,
    /// Process-global logging, tracing and metrics policy.
    pub observability: awaken_observability::ObservabilityConfig,
    /// Secret-free local ACP observations captured once during product startup.
    /// Empty means discovery was not run (for example in Server mode).
    pub local_acp_observations: Vec<awaken_acp_application::AcpHostObservation>,
    pub control: awaken_control::ControlStoreConfig,
    pub resources: ResourcePlaneStoreBackend,
    pub seal_key: SealKeySource,
    pub deprecations: Vec<String>,
    pub origins: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct CloudIamConfig {
    pub base_url: String,
    pub inference_base_url: String,
    pub audience: String,
    pub issuer: String,
    pub access_token: Option<String>,
    pub service_token: Option<String>,
    pub service_token_file: Option<PathBuf>,
}

impl std::fmt::Debug for CloudIamConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudIamConfig")
            .field("base_url", &self.base_url)
            .field("inference_base_url", &self.inference_base_url)
            .field("audience", &self.audience)
            .field("issuer", &self.issuer)
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "service_token",
                &self.service_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("service_token_file", &self.service_token_file)
            .finish()
    }
}

impl ResolvedDeployment {
    pub fn load(overrides: ConfigOverrides) -> Result<Self, String> {
        let home = home_dir();
        let explicit_path = overrides.config_path.is_some();
        let config_path = overrides
            .config_path
            .clone()
            .or_else(|| home.as_ref().map(|home| home.join(".awaken/config.toml")))
            .ok_or_else(|| "config_path_unavailable: pass --config <PATH>".to_owned())?;
        let file = if config_path.exists() {
            let source = fs::read_to_string(&config_path)
                .map_err(|error| format!("read {}: {error}", config_path.display()))?;
            toml::from_str::<FileConfig>(&source)
                .map_err(|error| format!("parse {}: {error}", config_path.display()))?
        } else if explicit_path {
            return Err(format!(
                "configured file {} does not exist",
                config_path.display()
            ));
        } else {
            FileConfig::default()
        };
        Self::resolve_file(overrides, home, config_path, file)
    }

    fn resolve_file(
        overrides: ConfigOverrides,
        home: Option<PathBuf>,
        config_path: PathBuf,
        file: FileConfig,
    ) -> Result<Self, String> {
        let mut origins = BTreeMap::new();
        let data_dir = if let Some(path) = overrides.data_dir {
            origins.insert("data_dir".to_owned(), "command line".to_owned());
            path
        } else if let Some(path) = file.data_dir.clone() {
            origins.insert("data_dir".to_owned(), "config.toml".to_owned());
            path
        } else {
            origins.insert("data_dir".to_owned(), "default".to_owned());
            home.as_ref()
                .map(|home| home.join(".awaken"))
                .ok_or_else(|| "data_dir_unavailable: configure data_dir".to_owned())?
        };

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
                ("sessions_db", file.sessions_db.is_some()),
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
        let deployment_session_launch = DeploymentSessionLaunchConfig::resolve(
            role,
            file.coordinator_internal_url.clone(),
            file.deployment_session_launch_token_file.clone(),
        )?;
        let worker_server = overrides.worker_server.or(file.worker_server.clone());
        if role == Role::Worker && worker_server.is_none() {
            return Err(
                "worker_server_required: pass --server or configure worker_server".to_owned(),
            );
        }
        worker_bootstrap::validate_credential_file_ownership(
            role,
            file.worker_request_credential_file.is_some(),
            file.worker_trust_credentials_file.is_some(),
        )?;
        let worker = WorkerBootstrap {
            worker_id: file
                .worker_id
                .clone()
                .unwrap_or_else(|| "awaken-worker".to_owned()),
            request_credential_file: file.worker_request_credential_file.clone(),
            credential_material_root: file
                .worker_credential_material_root
                .clone()
                .unwrap_or_else(|| data_dir.join("worker-credentials")),
            credential_trust_domain: file.worker_credential_trust_domain.clone().unwrap_or_else(
                || awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN.to_owned(),
            ),
            admin_listen: file
                .worker_admin_listen
                .clone()
                .or_else(|| Some("0.0.0.0:9090".to_owned())),
            drain_grace_secs: file.worker_drain_grace_secs.unwrap_or(20),
            build_digest: file.worker_build_digest.clone(),
            zone: file.worker_zone.clone(),
            capabilities: file.worker_capabilities.clone().unwrap_or_default(),
            max_concurrent: file.worker_max_concurrent,
            credential_probe_interval_secs: file
                .worker_credential_probe_interval_secs
                .unwrap_or(10),
            credential_observation_ttl_secs: file
                .worker_credential_observation_ttl_secs
                .unwrap_or(30),
        };
        if worker.worker_id.trim().is_empty() {
            return Err("worker_id must not be empty".to_owned());
        }
        if worker.credential_material_root.as_os_str().is_empty()
            || worker.credential_trust_domain.trim().is_empty()
        {
            return Err(
                "worker_credential_material_root and worker_credential_trust_domain must not be empty"
                    .to_owned(),
            );
        }
        if worker.credential_probe_interval_secs == 0
            || worker.credential_observation_ttl_secs <= worker.credential_probe_interval_secs
        {
            return Err(
                "worker credential observation TTL must be greater than the non-zero probe interval"
                    .to_owned(),
            );
        }
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
        let dispatch_url = file.runtime_database_url.clone();
        if dispatch_url
            .as_ref()
            .is_some_and(|url| !is_postgres_url(url))
        {
            return Err("runtime_database_url must be postgres://".to_owned());
        }
        let dispatch_backend = if dispatch_url.is_some() {
            DispatchBackend::Postgres
        } else {
            DispatchBackend::Sqlite
        };
        if role == Role::Coordinator && dispatch_backend != DispatchBackend::Postgres {
            return Err("Coordinator requires runtime_database_url".to_owned());
        }
        if role == Role::AllInOne
            && !run_local_pool
            && dispatch_backend != DispatchBackend::Postgres
        {
            return Err("run_local_pool=false requires runtime_database_url".to_owned());
        }
        let sandbox_tier = match file.sandbox_tier.as_deref() {
            Some("local" | "none") => SandboxTier::Local,
            Some("docker") => SandboxTier::Docker,
            Some("podman") => SandboxTier::Podman,
            Some("k8s" | "kubernetes") => SandboxTier::K8s,
            Some("namespace") | None => SandboxTier::Namespace,
            Some(other) => return Err(format!("invalid sandbox_tier={other:?}")),
        };
        let acp_ids = file.acp_clis.clone().unwrap_or_default();
        let acp = (!acp_ids.is_empty())
            .then(|| AcpWorkerProfile::new(acp_ids, file.acp_default_cli.clone()))
            .transpose()?;
        let wake = match file.dispatch_wake.as_deref() {
            Some("pg-notify") => Wake::PgNotify,
            Some("nats") => Wake::Nats,
            Some("none") | None => Wake::None,
            Some(other) => return Err(format!("invalid dispatch_wake={other:?}")),
        };
        let sandbox_defaults = SandboxSettings::default();
        let sandbox = SandboxSettings {
            allow_local_fallback: file.sandbox_allow_local_fallback.unwrap_or(false),
            warm_pool_size: file.sandbox_warm_pool_size.unwrap_or(0),
            container_forward_proxy: file.container_forward_proxy.clone(),
            k8s_namespace: file
                .k8s_namespace
                .clone()
                .unwrap_or_else(|| "default".to_owned()),
            container_hand_bin: file
                .container_hand_bin
                .clone()
                .unwrap_or_else(|| sandbox_defaults.container_hand_bin.clone()),
            podman_bin: file
                .podman_bin
                .clone()
                .unwrap_or_else(|| sandbox_defaults.podman_bin.clone()),
            inherit_agent_stderr: file.sandbox_inherit_agent_stderr.unwrap_or(false),
            reaper_enabled: file.sandbox_reaper_enabled.unwrap_or(true),
            reaper_interval_secs: file
                .sandbox_reaper_interval_secs
                .unwrap_or(sandbox_defaults.reaper_interval_secs),
            reaper_max_age_secs: file
                .sandbox_reaper_max_age_secs
                .unwrap_or(sandbox_defaults.reaper_max_age_secs),
        };
        if sandbox.k8s_namespace.trim().is_empty()
            || sandbox.container_hand_bin.trim().is_empty()
            || sandbox.podman_bin.trim().is_empty()
        {
            return Err(
                "k8s_namespace, container_hand_bin and podman_bin must not be empty".to_owned(),
            );
        }
        if sandbox.reaper_enabled
            && (sandbox.reaper_interval_secs == 0 || sandbox.reaper_max_age_secs == 0)
        {
            return Err(
                "sandbox reaper interval and max age must be non-zero when enabled".to_owned(),
            );
        }
        let content_capture = ContentCaptureSettings {
            level: match file.content_capture.as_deref() {
                Some("off") => awaken_runtime_contract::ContentCapture::Off,
                Some("structured") | None => awaken_runtime_contract::ContentCapture::Structured,
                Some("full") => awaken_runtime_contract::ContentCapture::Full,
                Some(other) => return Err(format!("invalid content_capture={other:?}")),
            },
            redaction: match file.content_redaction.as_deref() {
                Some("none") | None => ContentRedaction::None,
                Some("regex") => ContentRedaction::Regex,
                Some(other) => return Err(format!("invalid content_redaction={other:?}")),
            },
        };
        let postgres_max_connections = file
            .postgres_max_connections
            .map(|value| {
                std::num::NonZeroU32::new(value)
                    .ok_or_else(|| "postgres_max_connections must be greater than zero".to_owned())
            })
            .transpose()?
            .unwrap_or_else(awaken_runtime_host::default_postgres_max_connections);
        let hand_connections = file.hand_connections.clone().unwrap_or_default();
        for (hand_id, plan) in &hand_connections {
            if hand_id.trim().is_empty() {
                return Err("hand_connections keys must be non-empty".to_owned());
            }
            if plan.dial != awaken_connection_plan::DialPolicy::Dial {
                return Err(format!(
                    "hand_connections.{hand_id} must use dial = \"dial\""
                ));
            }
            if !matches!(
                plan.transport,
                awaken_connection_plan::DialAddr::Unix(_)
                    | awaken_connection_plan::DialAddr::Tcp(_)
            ) {
                return Err(format!(
                    "hand_connections.{hand_id} must use a unix or tcp transport"
                ));
            }
        }
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
        let mut runtime = DeploymentConfig {
            durable: true,
            storage_dir: Some(data_dir.clone()),
            store: if dispatch_url.is_some() {
                StoreKind::Postgres
            } else {
                StoreKind::Sqlite
            },
            dispatch_backend,
            wake,
            wake_channel: file
                .dispatch_wake_channel
                .clone()
                .unwrap_or_else(|| DEFAULT_WAKE_CHANNEL.to_owned()),
            nats_url: file.nats_url.clone(),
            database_url: dispatch_url,
            postgres_max_connections,
            dispatch_owner: file
                .dispatch_owner
                .clone()
                .unwrap_or_else(|| format!("host-{}", std::process::id())),
            upstream: worker_server.clone(),
            sandbox_tier,
            sandbox_dir: file.sandbox_dir.clone(),
            sandbox,
            content_capture,
            acp_session_blob_root: file.acp_session_blob_root.clone(),
            acp,
            container_image: file.container_image.clone(),
            disable_local_pool: !run_local_pool,
        };
        if role == Role::Worker {
            // The registered Worker receives durable claims through its injected
            // HTTP dispatch store. Everything it owns locally is ephemeral
            // execution state; retaining the product data directory here would
            // make `SharedHost` silently open Session/File/Memory SQLite stores.
            runtime.storage_dir = None;
            runtime.database_url = None;
            runtime.dispatch_backend = DispatchBackend::Sqlite;
            runtime.store = StoreKind::Sqlite;
            runtime.nats_url = None;
        }
        let shared_management_database = file
            .management_database_url_file
            .as_deref()
            .map(read_management_database_url)
            .transpose()?;
        if shared_management_database.is_some()
            && [
                &file.resource_database_url,
                &file.catalog_db,
                &file.credential_db,
                &file.config_db,
                &file.admin_db,
                &file.sessions_db,
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
            specific
                .clone()
                .or_else(|| shared_management_database.clone())
        };
        let control = awaken_control::ControlStoreConfig::from_values(
            &data_dir,
            store_url(&file.catalog_db),
            store_url(&file.credential_db),
            store_url(&file.config_db),
            store_url(&file.admin_db),
            store_url(&file.sessions_db),
        );
        let resources = match store_url(&file.resource_database_url) {
            Some(url) if is_postgres_url(&url) => ResourcePlaneStoreBackend::Postgres(url),
            Some(_) => return Err("resource_database_url must be postgres://".to_owned()),
            None => ResourcePlaneStoreBackend::Embedded(data_dir.clone()),
        };
        let shared_resource_catalog =
            matches!(&control.admin, awaken_control::StoreBackend::Postgres(_));
        if dispatch_backend == DispatchBackend::Postgres
            && (!resources.is_shared() || !shared_resource_catalog)
        {
            return Err(
                "a shared runtime requires resource_database_url and admin_db to use Postgres"
                    .to_owned(),
            );
        }
        let seal_key = match (
            role,
            &file.control_seal_key,
            &file.control_seal_key_file,
            mode,
        ) {
            (Role::Worker, None, None, _) => SealKeySource::NotOwnedByRole,
            (_, Some(_), Some(_), _) => {
                return Err(
                    "configure exactly one of control_seal_key or control_seal_key_file".to_owned(),
                );
            }
            (_, Some(value), None, _) => SealKeySource::Inline(value.clone()),
            (_, None, Some(path), _) => SealKeySource::File(path.clone()),
            (_, None, None, OperatingMode::Local) => {
                SealKeySource::LocalFile(data_dir.join("control-seal.key"))
            }
            (_, None, None, OperatingMode::Server) => {
                return Err(
                    "server mode requires control_seal_key or control_seal_key_file".to_owned(),
                );
            }
        };
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
            .unwrap_or(awaken_control::ManagementIdentityMode::SelfManaged);
        let cloud_models = overrides
            .cloud_models
            .or(file
                .cloud_models
                .as_deref()
                .map(CloudModelMode::parse)
                .transpose()?)
            .unwrap_or_default();
        if cloud_models.is_enabled()
            && identity_mode != awaken_control::ManagementIdentityMode::AwakenCloud
        {
            return Err(
                "cloud_models_require_awaken_cloud_identity: cloud_models=enabled requires identity_mode=awaken-cloud"
                    .to_owned(),
            );
        }
        if file.cloud_iam_service_token.is_some() && file.cloud_iam_service_token_file.is_some() {
            return Err(
                "configure exactly one of cloud_iam_service_token or cloud_iam_service_token_file"
                    .to_owned(),
            );
        }
        if mode == OperatingMode::Server
            && identity_mode == awaken_control::ManagementIdentityMode::AwakenCloud
            && file.cloud_iam_service_token_file.is_none()
        {
            return Err(
                "server-mode Awaken Cloud identity requires cloud_iam_service_token_file"
                    .to_owned(),
            );
        }
        let cloud_iam = CloudIamConfig {
            base_url: file
                .cloud_iam_url
                .clone()
                .unwrap_or_else(|| "https://accounts.awakenworks.com".to_owned()),
            inference_base_url: file
                .cloud_api_url
                .clone()
                .unwrap_or_else(|| "https://api.awakenworks.com".to_owned()),
            audience: file
                .cloud_iam_audience
                .clone()
                .unwrap_or_else(|| "awaken-runtime".to_owned()),
            issuer: file
                .cloud_iam_issuer
                .clone()
                .unwrap_or_else(|| "https://accounts.awakenworks.com".to_owned()),
            access_token: file.cloud_access_token.clone(),
            service_token: file.cloud_iam_service_token.clone(),
            service_token_file: file.cloud_iam_service_token_file.clone(),
        };
        let suite_hub_url = file
            .suite_hub_url
            .as_deref()
            .map(|value| validate_suite_hub_url(value, mode))
            .transpose()?;
        Ok(Self {
            role,
            mode,
            bind,
            data_dir,
            config_file_exists: config_path.exists(),
            config_path,
            no_browser: overrides.no_browser.or(file.no_browser).unwrap_or(false),
            suite_hub_url,
            run_local_pool,
            worker_server,
            worker,
            worker_trust_credentials_file: file.worker_trust_credentials_file,
            identity_mode,
            cloud_models,
            org_id: file
                .org_id
                .unwrap_or_else(|| awaken_control::DEFAULT_ORG_ID.to_owned()),
            iam_workspaces: file.iam_workspaces.unwrap_or_default(),
            cloud_iam,
            executable_agent_registration,
            deployment_session_launch,
            mcp_bearer_token: file.mcp_bearer_token,
            admin_listen: file.admin_listen,
            runtime,
            hand_connections,
            observability,
            local_acp_observations: Vec::new(),
            control,
            resources,
            seal_key,
            deprecations: Vec::new(),
            origins,
        })
    }

    pub fn ensure_data_layout(&self) -> Result<(), String> {
        for path in [
            self.data_dir.clone(),
            self.data_dir.join("runtime"),
            self.data_dir.join("sandboxes"),
            self.data_dir.join("logs"),
        ] {
            fs::create_dir_all(&path)
                .map_err(|error| format!("create data directory {}: {error}", path.display()))?;
        }
        Ok(())
    }

    /// Apply the canonical host-discovery result to the one runtime profile.
    /// Explicit `acp_clis` constrain the discovered set; an unconfigured Local
    /// install advertises every detected catalog row. Missing/broken CLIs are
    /// retained only in the diagnostic read model and never in Worker routes.
    pub fn apply_local_acp_observations(
        &mut self,
        observations: Vec<awaken_acp_application::AcpHostObservation>,
        routable_cli_ids: Vec<String>,
    ) -> Result<(), String> {
        let explicit = self.runtime.acp.clone();
        let default_cli = explicit
            .as_ref()
            .and_then(|profile| profile.default_cli().map(str::to_string));
        self.runtime.acp = if routable_cli_ids.is_empty() {
            None
        } else {
            Some(AcpWorkerProfile::new(routable_cli_ids, default_cli)?)
        };
        self.local_acp_observations = observations;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn local_test_deployment(data_dir: PathBuf) -> ResolvedDeployment {
    ResolvedDeployment::resolve_file(
        ConfigOverrides {
            data_dir: Some(data_dir),
            ..Default::default()
        },
        Some(PathBuf::from("/home/test")),
        PathBuf::from("/home/test/.awaken/config.toml"),
        FileConfig::default(),
    )
    .expect("test local deployment")
}

#[cfg(test)]
pub(crate) fn worker_test_deployment(data_dir: PathBuf) -> ResolvedDeployment {
    ResolvedDeployment::resolve_file(
        ConfigOverrides {
            role: Some(Role::Worker),
            worker_server: Some("http://coordinator".to_owned()),
            data_dir: Some(data_dir),
            ..Default::default()
        },
        Some(PathBuf::from("/home/test")),
        PathBuf::from("/home/test/.awaken/config.toml"),
        FileConfig::default(),
    )
    .expect("test Worker deployment")
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    bind: Option<String>,
    mode: Option<String>,
    role: Option<String>,
    worker_server: Option<String>,
    worker_id: Option<String>,
    worker_request_credential_file: Option<PathBuf>,
    worker_trust_credentials_file: Option<PathBuf>,
    worker_credential_material_root: Option<PathBuf>,
    worker_credential_trust_domain: Option<String>,
    worker_admin_listen: Option<String>,
    worker_drain_grace_secs: Option<u64>,
    worker_build_digest: Option<String>,
    worker_zone: Option<String>,
    worker_capabilities: Option<Vec<String>>,
    worker_max_concurrent: Option<u32>,
    worker_credential_probe_interval_secs: Option<u64>,
    worker_credential_observation_ttl_secs: Option<u64>,
    run_local_pool: Option<bool>,
    no_browser: Option<bool>,
    suite_hub_url: Option<String>,
    runtime_database_url: Option<String>,
    postgres_max_connections: Option<u32>,
    hand_connections: Option<BTreeMap<String, awaken_connection_plan::ConnectionPlan>>,
    log_filter: Option<String>,
    log_format: Option<String>,
    trace_file: Option<PathBuf>,
    otlp_endpoint: Option<String>,
    otlp_traces_endpoint: Option<String>,
    otlp_protocol: Option<String>,
    otlp_traces_protocol: Option<String>,
    otlp_headers: Option<BTreeMap<String, String>>,
    otlp_timeout_ms: Option<u64>,
    otel_service_name: Option<String>,
    otel_service_version: Option<String>,
    otel_metric_export_interval_ms: Option<u64>,
    management_database_url_file: Option<PathBuf>,
    resource_database_url: Option<String>,
    catalog_db: Option<String>,
    credential_db: Option<String>,
    config_db: Option<String>,
    admin_db: Option<String>,
    sessions_db: Option<String>,
    identity_mode: Option<String>,
    cloud_models: Option<String>,
    org_id: Option<String>,
    iam_workspaces: Option<Vec<String>>,
    cloud_iam_url: Option<String>,
    cloud_api_url: Option<String>,
    cloud_iam_audience: Option<String>,
    cloud_iam_issuer: Option<String>,
    cloud_access_token: Option<String>,
    cloud_iam_service_token: Option<String>,
    coordinator_internal_url: Option<String>,
    executable_agent_registration_token_file: Option<PathBuf>,
    deployment_session_launch_token_file: Option<PathBuf>,
    mcp_bearer_token: Option<String>,
    cloud_iam_service_token_file: Option<PathBuf>,
    admin_listen: Option<String>,
    control_seal_key: Option<String>,
    control_seal_key_file: Option<PathBuf>,
    sandbox_tier: Option<String>,
    sandbox_dir: Option<PathBuf>,
    sandbox_allow_local_fallback: Option<bool>,
    sandbox_warm_pool_size: Option<usize>,
    container_forward_proxy: Option<String>,
    k8s_namespace: Option<String>,
    container_hand_bin: Option<String>,
    podman_bin: Option<String>,
    sandbox_inherit_agent_stderr: Option<bool>,
    sandbox_reaper_enabled: Option<bool>,
    sandbox_reaper_interval_secs: Option<u64>,
    sandbox_reaper_max_age_secs: Option<u64>,
    content_capture: Option<String>,
    content_redaction: Option<String>,
    acp_session_blob_root: Option<PathBuf>,
    acp_clis: Option<Vec<String>>,
    acp_default_cli: Option<String>,
    container_image: Option<String>,
    dispatch_wake: Option<String>,
    dispatch_wake_channel: Option<String>,
    nats_url: Option<String>,
    dispatch_owner: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(file: FileConfig, overrides: ConfigOverrides) -> ResolvedDeployment {
        ResolvedDeployment::resolve_file(
            overrides,
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file,
        )
        .unwrap()
    }

    fn acp_observation(
        id: &str,
        detection: awaken_acp_application::AcpDetectionState,
    ) -> awaken_acp_application::AcpHostObservation {
        awaken_acp_application::AcpHostObservation {
            cli_id: id.to_string(),
            display_name: id.to_string(),
            detection,
            version: (detection == awaken_acp_application::AcpDetectionState::Detected)
                .then(|| "1.0".to_string()),
            credential_state: (detection == awaken_acp_application::AcpDetectionState::Detected)
                .then_some(awaken_runtime_contract::CredentialObservationState::Available),
            reason_code: Some("fixture".to_string()),
            capability_state: (detection == awaken_acp_application::AcpDetectionState::Detected)
                .then_some(awaken_acp_application::AcpCapabilityState::Verified),
            capability_fingerprint: (detection
                == awaken_acp_application::AcpDetectionState::Detected)
                .then(|| "fixture".to_string()),
            capability_reason_code: None,
        }
    }

    #[test]
    fn declared_hand_connections_are_typed_and_fail_closed() {
        use awaken_connection_plan::ConnectionPlan;

        // Cause/effect graph:
        // typed deployment connection plan -> startup topology validation -> one
        // live executor table; unsupported topology or unresolved credentials
        // must stop startup before Runtime can observe an incomplete placement.
        //
        // Decision table:
        // | id | direction | transport | credential | result |
        // | absent | - | - | - | empty table |
        // | non-empty | dial | Unix/TCP | none | accepted exactly |
        // | empty | dial | Unix | none | reject |
        // | non-empty | listen | Unix | none | reject |
        // | non-empty | dial | in-process | none | reject |
        assert!(
            resolve(FileConfig::default(), ConfigOverrides::default())
                .hand_connections
                .is_empty()
        );

        let valid_plan = ConnectionPlan::tcp_dial("127.0.0.1:7000");
        let valid = resolve(
            FileConfig {
                hand_connections: Some(BTreeMap::from([(
                    "research-hand".to_owned(),
                    valid_plan.clone(),
                )])),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            valid.hand_connections.get("research-hand"),
            Some(&valid_plan)
        );

        for (id, plan) in [
            ("", ConnectionPlan::unix_dial("/tmp/hand.sock")),
            ("listen", ConnectionPlan::unix_listen("/tmp/hand.sock")),
            ("in-process", ConnectionPlan::in_process()),
        ] {
            let result = ResolvedDeployment::resolve_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                FileConfig {
                    hand_connections: Some(BTreeMap::from([(id.to_owned(), plan)])),
                    ..Default::default()
                },
            );
            assert!(result.is_err(), "invalid Hand rule {id:?} was accepted");
        }
    }

    #[test]
    fn observability_is_explicit_typed_deployment_policy() {
        // Cause/effect graph:
        // config.toml observability fields -> one ResolvedDeployment value ->
        // subscriber/tracer/meter initialization; no observability library reads
        // ambient process configuration.
        //
        // Decision table:
        // | log/protocol/timing input | result |
        // | absent | text + info + disabled OTLP + 10s/60s defaults |
        // | valid explicit JSON/HTTP/timings/headers | exact typed projection |
        // | invalid log/protocol, empty filter/header, zero timing | reject |
        let defaults = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(defaults.observability, Default::default());

        let explicit = resolve(
            FileConfig {
                log_filter: Some("awaken=debug".to_owned()),
                log_format: Some("json".to_owned()),
                trace_file: Some(PathBuf::from("/tmp/trace.jsonl")),
                otlp_endpoint: Some("http://collector:4318".to_owned()),
                otlp_protocol: Some("http/protobuf".to_owned()),
                otlp_headers: Some(BTreeMap::from([(
                    "authorization".to_owned(),
                    "Bearer token".to_owned(),
                )])),
                otlp_timeout_ms: Some(2500),
                otel_metric_export_interval_ms: Some(5000),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            explicit.observability.log_format,
            awaken_observability::LogFormat::Json
        );
        assert_eq!(
            explicit.observability.otel.metric_export_interval,
            std::time::Duration::from_secs(5)
        );

        for file in [
            FileConfig {
                log_format: Some("yaml".to_owned()),
                ..Default::default()
            },
            FileConfig {
                log_filter: Some(" ".to_owned()),
                ..Default::default()
            },
            FileConfig {
                otlp_protocol: Some("udp".to_owned()),
                ..Default::default()
            },
            FileConfig {
                otlp_timeout_ms: Some(0),
                ..Default::default()
            },
            FileConfig {
                otlp_headers: Some(BTreeMap::from([(" ".to_owned(), "x".to_owned())])),
                ..Default::default()
            },
        ] {
            assert!(
                ResolvedDeployment::resolve_file(
                    ConfigOverrides::default(),
                    Some(PathBuf::from("/home/dev")),
                    PathBuf::from("/home/dev/.awaken/config.toml"),
                    file,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn local_discovery_is_the_only_source_of_worker_routes_and_defaults() {
        // Cause graph:
        // The ACP application service supplies the already filtered route ids;
        // this composition method only projects them into AcpWorkerProfile. The
        // profile constructor alone decides whether one row becomes the default.
        //
        // Decision table:
        // A1 unconfigured + none detected -> no ACP Worker
        // A2 unconfigured + one detected  -> that CLI + automatic default
        // A3 unconfigured + many detected -> all CLIs + no random default
        // A4 explicit subset              -> only detected configured CLIs
        // A5 available + capability failure -> diagnostic only, no route
        let missing = acp_observation("claude", awaken_acp_application::AcpDetectionState::Missing);
        let codex = acp_observation("codex", awaken_acp_application::AcpDetectionState::Detected);
        let claude = acp_observation(
            "claude",
            awaken_acp_application::AcpDetectionState::Detected,
        );

        let mut none = resolve(FileConfig::default(), ConfigOverrides::default());
        none.apply_local_acp_observations(vec![missing.clone()], vec![])
            .unwrap();
        assert!(none.runtime.acp.is_none(), "A1");

        let mut one = resolve(FileConfig::default(), ConfigOverrides::default());
        one.apply_local_acp_observations(vec![codex.clone(), missing], vec!["codex".into()])
            .unwrap();
        let one = one.runtime.acp.unwrap();
        assert_eq!(one.cli_ids().collect::<Vec<_>>(), ["codex"], "A2");
        assert_eq!(one.default_cli(), Some("codex"), "A2");

        let mut many = resolve(FileConfig::default(), ConfigOverrides::default());
        many.apply_local_acp_observations(
            vec![codex.clone(), claude.clone()],
            vec!["claude".into(), "codex".into()],
        )
        .unwrap();
        let many = many.runtime.acp.unwrap();
        assert_eq!(
            many.cli_ids().collect::<Vec<_>>(),
            ["claude", "codex"],
            "A3"
        );
        assert_eq!(many.default_cli(), None, "A3");

        let mut incompatible = codex.clone();
        incompatible.capability_state =
            Some(awaken_acp_application::AcpCapabilityState::ProbeFailed);
        incompatible.capability_fingerprint = None;
        let mut failed = resolve(FileConfig::default(), ConfigOverrides::default());
        failed
            .apply_local_acp_observations(vec![incompatible], vec![])
            .unwrap();
        assert!(failed.runtime.acp.is_none(), "A5");

        let mut explicit = resolve(
            FileConfig {
                acp_clis: Some(vec!["claude".into()]),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        explicit
            .apply_local_acp_observations(vec![codex, claude], vec!["claude".into()])
            .unwrap();
        let explicit = explicit.runtime.acp.unwrap();
        assert_eq!(explicit.cli_ids().collect::<Vec<_>>(), ["claude"], "A4");
    }

    #[test]
    fn local_defaults_are_durable_and_single_rooted() {
        let config = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(config.data_dir, PathBuf::from("/home/dev/.awaken"));
        assert_eq!(
            config.runtime.storage_dir.as_deref(),
            Some(config.data_dir.as_path())
        );
        assert!(config.runtime.durable);
        assert!(matches!(config.seal_key, SealKeySource::LocalFile(_)));
        assert_eq!(config.role, Role::AllInOne);
    }

    #[test]
    fn postgres_pool_size_is_resolved_once_as_a_non_zero_deployment_value() {
        // Cause/effect graph:
        // C1 the author omits postgres_max_connections -> E1 the composition
        // root derives one host-aware non-zero default; C2 the author supplies
        // a positive value -> E2 that exact value is retained; C3 the author
        // supplies zero -> E3 configuration fails before any store is built.
        //
        // Decision table:
        // R1 !configured       => E1
        // R2 configured && > 0 => E2
        // R3 configured && = 0 => E3
        let defaulted = resolve(FileConfig::default(), ConfigOverrides::default());
        assert!(defaulted.runtime.postgres_max_connections.get() > 0, "R1");

        let explicit = resolve(
            FileConfig {
                postgres_max_connections: Some(23),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(explicit.runtime.postgres_max_connections.get(), 23, "R2");

        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                postgres_max_connections: Some(0),
                ..Default::default()
            },
        )
        .expect_err("R3");
        assert!(error.contains("postgres_max_connections"), "R3: {error}");
    }

    #[test]
    fn worker_identity_is_authored_only_at_the_typed_product_boundary() {
        // Cause/effect graph:
        // C1 worker_id omitted -> E1 stable product default; C2 non-empty
        // worker_id supplied -> E2 exact identity retained for WorkerUpstream;
        // C3 empty identity supplied -> E3 configuration fails before startup.
        //
        // Decision table: R1 omitted=>default, R2 non-empty=>exact,
        // R3 empty=>error.
        let defaulted = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(defaulted.worker.worker_id, "awaken-worker", "R1");

        let explicit = resolve(
            FileConfig {
                worker_id: Some("worker-shanghai-1".to_owned()),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(explicit.worker.worker_id, "worker-shanghai-1", "R2");

        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                worker_id: Some(" ".to_owned()),
                ..Default::default()
            },
        )
        .expect_err("R3");
        assert!(error.contains("worker_id"), "R3: {error}");
    }

    #[test]
    fn sandbox_operator_policy_is_projected_once_from_typed_config() {
        // Cause/effect graph: each explicit file value selects one field on the
        // authoritative DeploymentConfig; omission selects SandboxSettings defaults.
        // No runtime adapter may add environment precedence.
        //
        // | Rule | file values | Effect |
        // |---|---|---|
        // | S1 | all omitted | fail-closed fallback, no pool/proxy, default namespace/reaper |
        // | S2 | all explicit valid | exact typed values projected losslessly |
        let defaults = resolve(FileConfig::default(), ConfigOverrides::default())
            .runtime
            .sandbox;
        assert!(!defaults.allow_local_fallback, "S1");
        assert_eq!(defaults.warm_pool_size, 0, "S1");
        assert_eq!(defaults.container_forward_proxy, None, "S1");
        assert_eq!(defaults.k8s_namespace, "default", "S1");
        assert_eq!(
            defaults.container_hand_bin, "/usr/local/bin/awaken-sandbox",
            "S1"
        );
        assert_eq!(defaults.podman_bin, "podman", "S1");
        assert!(!defaults.inherit_agent_stderr, "S1");
        assert!(defaults.reaper_enabled, "S1");

        let selected = resolve(
            FileConfig {
                sandbox_allow_local_fallback: Some(true),
                sandbox_warm_pool_size: Some(3),
                container_forward_proxy: Some("http://proxy.internal:8080".into()),
                k8s_namespace: Some("agents".into()),
                container_hand_bin: Some("/opt/awaken/bin/hand".into()),
                podman_bin: Some("/opt/podman/bin/podman".into()),
                sandbox_inherit_agent_stderr: Some(true),
                sandbox_reaper_enabled: Some(true),
                sandbox_reaper_interval_secs: Some(17),
                sandbox_reaper_max_age_secs: Some(91),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        )
        .runtime
        .sandbox;
        assert!(selected.allow_local_fallback, "S2");
        assert_eq!(selected.warm_pool_size, 3, "S2");
        assert_eq!(
            selected.container_forward_proxy.as_deref(),
            Some("http://proxy.internal:8080"),
            "S2"
        );
        assert_eq!(selected.k8s_namespace, "agents", "S2");
        assert_eq!(selected.container_hand_bin, "/opt/awaken/bin/hand", "S2");
        assert_eq!(selected.podman_bin, "/opt/podman/bin/podman", "S2");
        assert!(selected.inherit_agent_stderr, "S2");
        assert_eq!(selected.reaper_interval_secs, 17, "S2");
        assert_eq!(selected.reaper_max_age_secs, 91, "S2");
    }

    #[test]
    fn sandbox_operator_policy_rejects_invalid_active_values() {
        // Cause/effect graph: an empty namespace is never usable; an enabled
        // reaper needs non-zero cadence and age. Disabling the reaper makes its
        // dormant numeric values irrelevant.
        //
        // | Rule | namespace | enabled | interval/max | Effect |
        // |---|---|---:|---|---|
        // | V1 | empty | either | any | reject |
        // | V2 | valid | true | either zero | reject |
        // | V3 | valid | false | zero/zero | accept |
        let resolve_error = |file: FileConfig| {
            ResolvedDeployment::resolve_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                file,
            )
            .unwrap_err()
        };
        assert!(
            resolve_error(FileConfig {
                k8s_namespace: Some(" ".into()),
                ..FileConfig::default()
            })
            .contains("k8s_namespace"),
            "V1"
        );
        assert!(
            resolve_error(FileConfig {
                container_hand_bin: Some(String::new()),
                ..FileConfig::default()
            })
            .contains("container_hand_bin"),
            "V1"
        );
        assert!(
            resolve_error(FileConfig {
                podman_bin: Some(String::new()),
                ..FileConfig::default()
            })
            .contains("podman_bin"),
            "V1"
        );
        assert!(
            resolve_error(FileConfig {
                sandbox_reaper_interval_secs: Some(0),
                ..FileConfig::default()
            })
            .contains("reaper"),
            "V2"
        );
        let dormant = resolve(
            FileConfig {
                sandbox_reaper_enabled: Some(false),
                sandbox_reaper_interval_secs: Some(0),
                sandbox_reaper_max_age_secs: Some(0),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert!(!dormant.runtime.sandbox.reaper_enabled, "V3");
    }

    #[test]
    fn content_capture_policy_has_one_typed_authoring_boundary() {
        // Cause/effect graph: omitted values select the privacy-preserving
        // Structured/None defaults; known strings project to typed values;
        // unknown strings fail before Host construction.
        //
        // | Rule | capture | redaction | Effect |
        // |---|---|---|---|
        // | P1 | omitted | omitted | Structured + None |
        // | P2 | Full | Regex | exact typed policy |
        // | P3 | unknown | either | reject |
        // | P4 | either | unknown | reject |
        let defaults = resolve(FileConfig::default(), ConfigOverrides::default())
            .runtime
            .content_capture;
        assert_eq!(
            defaults.level,
            awaken_runtime_contract::ContentCapture::Structured,
            "P1"
        );
        assert_eq!(defaults.redaction, ContentRedaction::None, "P1");

        let selected = resolve(
            FileConfig {
                content_capture: Some("full".into()),
                content_redaction: Some("regex".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        )
        .runtime
        .content_capture;
        assert_eq!(
            selected.level,
            awaken_runtime_contract::ContentCapture::Full,
            "P2"
        );
        assert_eq!(selected.redaction, ContentRedaction::Regex, "P2");

        let resolve_error = |file: FileConfig| {
            ResolvedDeployment::resolve_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                file,
            )
            .unwrap_err()
        };
        assert!(
            resolve_error(FileConfig {
                content_capture: Some("everything".into()),
                ..FileConfig::default()
            })
            .contains("content_capture"),
            "P3"
        );
        assert!(
            resolve_error(FileConfig {
                content_redaction: Some("external".into()),
                ..FileConfig::default()
            })
            .contains("content_redaction"),
            "P4"
        );
    }

    #[test]
    fn precedence_is_cli_then_file_then_default() {
        let config = resolve(
            FileConfig {
                data_dir: Some(PathBuf::from("/file")),
                bind: Some("127.0.0.1:7000".to_owned()),
                ..FileConfig::default()
            },
            ConfigOverrides {
                data_dir: Some(PathBuf::from("/cli")),
                port: Some(9100),
                ..ConfigOverrides::default()
            },
        );
        assert_eq!(config.data_dir, PathBuf::from("/cli"));
        assert_eq!(config.bind, "127.0.0.1:9100");
    }

    #[test]
    fn cloud_inference_url_is_independent_from_identity_url() {
        let config = resolve(
            FileConfig {
                cloud_api_url: Some("https://file.cloud.example".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            config.cloud_iam.inference_base_url,
            "https://file.cloud.example"
        );
        assert_eq!(
            resolve(FileConfig::default(), ConfigOverrides::default())
                .cloud_iam
                .inference_base_url,
            "https://api.awakenworks.com"
        );
    }

    #[test]
    fn suite_hub_is_optional_exact_and_transport_safe() {
        // Cause graph: deployment mode + exact absolute URL -> browser exit;
        // absent value -> standalone console; remote cleartext or URL-carried
        // credentials/query/fragment -> fail before the listener starts.
        //
        // | rule | mode | value | effect |
        // | S1 | any | absent | standalone projection |
        // | S2 | server | HTTPS path | exact hub retained |
        // | S3 | local | loopback HTTP | exact dev hub retained |
        // | S4 | server | HTTP | reject |
        // | S5 | any | credential/query/fragment | reject |
        let standalone = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(standalone.suite_hub_url, None, "S1");

        let hosted = resolve(
            FileConfig {
                mode: Some("server".into()),
                control_seal_key: Some(
                    "0000000000000000000000000000000000000000000000000000000000000000".into(),
                ),
                suite_hub_url: Some("https://cloud.example/products".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            hosted.suite_hub_url.as_deref(),
            Some("https://cloud.example/products"),
            "S2"
        );

        let local = resolve(
            FileConfig {
                suite_hub_url: Some("http://127.0.0.1:17878/products".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(
            local.suite_hub_url.as_deref(),
            Some("http://127.0.0.1:17878/products"),
            "S3"
        );

        let error_for = |value: &str| {
            ResolvedDeployment::resolve_file(
                ConfigOverrides::default(),
                Some(PathBuf::from("/home/dev")),
                PathBuf::from("/home/dev/.awaken/config.toml"),
                FileConfig {
                    mode: Some("server".into()),
                    control_seal_key: Some(
                        "0000000000000000000000000000000000000000000000000000000000000000".into(),
                    ),
                    suite_hub_url: Some(value.into()),
                    ..FileConfig::default()
                },
            )
            .unwrap_err()
        };
        assert!(
            error_for("http://cloud.example/products").contains("HTTPS"),
            "S4"
        );
        for unsafe_url in [
            "https://user@cloud.example/products",
            "https://cloud.example/products?tenant=forbidden",
            "https://cloud.example/products#fragment",
        ] {
            assert!(error_for(unsafe_url).contains("must not contain"), "S5");
        }
    }

    #[test]
    fn cloud_login_and_cloud_model_supply_are_independent_and_fail_closed() {
        // Cause graph: C1 selects Cloud identity; C2 enables Cloud models.
        // E1 identity alone keeps model supply local; E2 C1+C2 enables brokered
        // supply; E3 C2 without C1 is rejected before any network wiring.
        //
        // | Rule | C1 Cloud identity | C2 Cloud models | Result |
        // |---|---:|---:|---|
        // | F1 | 0 | 0 | self-managed local session + local/BYOK only |
        // | F2 | 1 | 0 | Cloud login + local/BYOK only |
        // | F3 | 1 | 1 | Cloud login + brokered supply |
        // | F4 | 0 | 1 | startup configuration error |
        let local = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(
            local.identity_mode,
            awaken_control::ManagementIdentityMode::SelfManaged
        );
        assert_eq!(local.cloud_models, CloudModelMode::Disabled);

        let login_only = resolve(
            FileConfig {
                identity_mode: Some("awaken-cloud".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(login_only.cloud_models, CloudModelMode::Disabled);

        let full = resolve(
            FileConfig {
                identity_mode: Some("awaken-cloud".into()),
                cloud_models: Some("enabled".into()),
                ..FileConfig::default()
            },
            ConfigOverrides::default(),
        );
        assert_eq!(full.cloud_models, CloudModelMode::Enabled);

        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                cloud_models: Some("enabled".into()),
                ..FileConfig::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("cloud_models_require_awaken_cloud_identity"));
    }

    #[test]
    fn command_line_cloud_modes_override_the_config_file() {
        let config = resolve(
            FileConfig {
                identity_mode: Some("no-login".into()),
                cloud_models: Some("disabled".into()),
                ..FileConfig::default()
            },
            ConfigOverrides {
                identity_mode: Some(awaken_control::ManagementIdentityMode::AwakenCloud),
                cloud_models: Some(CloudModelMode::Enabled),
                ..ConfigOverrides::default()
            },
        );
        assert_eq!(
            config.identity_mode,
            awaken_control::ManagementIdentityMode::AwakenCloud
        );
        assert_eq!(config.cloud_models, CloudModelMode::Enabled);
    }

    #[test]
    fn worker_requires_an_explicit_server() {
        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides {
                role: Some(Role::Worker),
                ..Default::default()
            },
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig::default(),
        )
        .unwrap_err();
        assert!(error.contains("--server"));
    }

    #[test]
    fn process_role_owns_one_local_pool_meaning() {
        // Cause/effect graph:
        // role + optional run_local_pool + shared Dispatch -> one process-role
        // interpretation. AllInOne may own a co-located pool, Control owns none,
        // Coordinator requires shared Dispatch and owns no pool, and Worker owns
        // its mandatory registered pool.
        //
        // Decision table:
        // | rule | role        | setting | shared Dispatch | result |
        // | P1   | AllInOne    | absent  | no              | co-located pool enabled |
        // | P2   | AllInOne    | false   | no              | reject |
        // | P3   | Worker      | absent  | no              | registered pool enabled |
        // | P4   | non-AllInOne| any     | any             | reject ambiguous setting |
        // | P5   | Control     | absent  | no              | no pool |
        // | P6   | Coordinator | absent  | no              | reject |
        // | P7   | Coordinator | absent  | yes             | no co-located pool |
        let all_in_one = resolve(FileConfig::default(), ConfigOverrides::default());
        assert!(all_in_one.run_local_pool, "P1");
        assert!(!all_in_one.runtime.disable_local_pool, "P1");

        let all_in_one_without_shared_dispatch = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                run_local_pool: Some(false),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            all_in_one_without_shared_dispatch.contains("runtime_database_url"),
            "P2"
        );

        let worker = resolve(
            FileConfig::default(),
            ConfigOverrides {
                role: Some(Role::Worker),
                worker_server: Some("http://coordinator".to_owned()),
                ..Default::default()
            },
        );
        assert!(worker.run_local_pool, "P3");
        assert!(!worker.runtime.disable_local_pool, "P3");

        for role in [Role::Control, Role::Coordinator, Role::Worker] {
            for setting in [false, true] {
                let error = ResolvedDeployment::resolve_file(
                    ConfigOverrides {
                        role: Some(role),
                        worker_server: (role == Role::Worker)
                            .then(|| "http://coordinator".to_owned()),
                        ..Default::default()
                    },
                    Some(PathBuf::from("/home/dev")),
                    PathBuf::from("/home/dev/.awaken/config.toml"),
                    FileConfig {
                        run_local_pool: Some(setting),
                        ..Default::default()
                    },
                )
                .unwrap_err();
                assert!(error.contains("all-in-one-only"), "P4: {role:?}/{setting}");
            }
        }

        let control = resolve(
            FileConfig::default(),
            ConfigOverrides {
                role: Some(Role::Control),
                ..Default::default()
            },
        );
        assert!(!control.run_local_pool, "P5");
        assert!(control.runtime.disable_local_pool, "P5");

        let coordinator_without_shared_dispatch = ResolvedDeployment::resolve_file(
            ConfigOverrides {
                role: Some(Role::Coordinator),
                ..Default::default()
            },
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig::default(),
        )
        .unwrap_err();
        assert!(
            coordinator_without_shared_dispatch.contains("runtime_database_url"),
            "P6"
        );

        let coordinator = resolve(
            FileConfig {
                runtime_database_url: Some("postgres://coordinator/db".to_owned()),
                resource_database_url: Some("postgres://resource/db".to_owned()),
                admin_db: Some("postgres://control/admin".to_owned()),
                ..Default::default()
            },
            ConfigOverrides {
                role: Some(Role::Coordinator),
                ..Default::default()
            },
        );
        assert!(!coordinator.run_local_pool, "P7");
        assert!(coordinator.runtime.disable_local_pool, "P7");
    }

    #[test]
    fn worker_credential_observation_window_fails_closed() {
        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides {
                role: Some(Role::Worker),
                worker_server: Some("http://control".to_owned()),
                ..Default::default()
            },
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                worker_credential_probe_interval_secs: Some(10),
                worker_credential_observation_ttl_secs: Some(10),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("TTL"));
    }

    #[test]
    fn reports_never_include_database_credentials() {
        // Cause/effect decision table: R1 Local mode -> report automatic
        // startup migration; R2 Server mode -> report explicit migration and
        // verify-only startup; R3 either mode with credential-bearing URLs ->
        // redact credentials and schemes. The report must not promise the Local
        // lifecycle for a distributed deployment.
        let config = resolve(
            FileConfig {
                runtime_database_url: Some("postgres://user:secret@db/awaken".to_owned()),
                resource_database_url: Some("postgres://user:secret@db/resources".to_owned()),
                admin_db: Some("postgres://user:secret@db/admin".to_owned()),
                ..FileConfig::default()
            },
            Default::default(),
        );
        for report in [config.report(false), config.report(true)] {
            assert!(report.contains("automatic at startup"), "R1: {report}");
            assert!(!report.contains("user:secret"), "{report}");
            assert!(!report.contains("postgres://"), "{report}");
        }
        let server = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                mode: Some("server".to_owned()),
                runtime_database_url: Some("postgres://db/awaken".to_owned()),
                resource_database_url: Some("postgres://db/awaken".to_owned()),
                admin_db: Some("postgres://db/awaken".to_owned()),
                control_seal_key: Some(
                    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".to_owned(),
                ),
                ..FileConfig::default()
            },
        )
        .unwrap();
        for report in [server.report(false), server.report(true)] {
            assert!(report.contains("awaken database migrate"), "R2: {report}");
            assert!(report.contains("startup verifies"), "R2: {report}");
            assert!(!report.contains("postgres://"), "R3: {report}");
        }
    }

    #[test]
    fn projected_management_database_url_is_one_shared_secret_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database-url");
        fs::write(&path, "postgres://user:secret@database/awaken\n").unwrap();
        let config = resolve(
            FileConfig {
                management_database_url_file: Some(path),
                ..FileConfig::default()
            },
            Default::default(),
        );
        for backend in [
            &config.control.catalog,
            &config.control.credential,
            &config.control.config,
            &config.control.admin,
            &config.control.sessions,
        ] {
            assert!(matches!(backend, awaken_control::StoreBackend::Postgres(_)));
        }
        assert!(config.resources.is_shared());
        assert_eq!(
            config
                .origins
                .get("management_database")
                .map(String::as_str),
            Some("management_database_url_file")
        );
        assert!(!config.report(false).contains("user:secret"));
    }

    #[test]
    fn projected_management_database_url_rejects_ambiguous_or_invalid_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database-url");
        fs::write(&path, "postgres://database/awaken").unwrap();
        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                management_database_url_file: Some(path.clone()),
                catalog_db: Some("postgres://other/catalog".to_owned()),
                ..FileConfig::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("cannot be combined"));

        fs::write(&path, "https://not-a-database.example").unwrap();
        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            FileConfig {
                management_database_url_file: Some(path),
                ..FileConfig::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("must contain a postgres:// URL"));
    }

    #[test]
    fn local_seal_key_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-seal.key");
        let source = SealKeySource::LocalFile(path.clone());
        assert_eq!(
            source.load_or_create().unwrap(),
            source.load_or_create().unwrap()
        );
        assert_eq!(fs::read_to_string(&path).unwrap().trim().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn hosted_cloud_identity_requires_one_projected_service_credential() {
        let mut file = FileConfig {
            mode: Some("server".to_owned()),
            identity_mode: Some("awaken-cloud".to_owned()),
            control_seal_key: Some(
                "0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            ),
            ..FileConfig::default()
        };
        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file.clone(),
        )
        .unwrap_err();
        assert!(error.contains("cloud_iam_service_token_file"));

        file.cloud_iam_service_token_file =
            Some(PathBuf::from("/var/run/awaken/management-iam/token"));
        let config = resolve(file.clone(), ConfigOverrides::default());
        assert_eq!(
            config.cloud_iam.service_token_file.as_deref(),
            Some(Path::new("/var/run/awaken/management-iam/token"))
        );
        assert!(config.cloud_iam.service_token.is_none());

        file.cloud_iam_service_token = Some("inline-is-forbidden".to_owned());
        let error = ResolvedDeployment::resolve_file(
            ConfigOverrides::default(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file,
        )
        .unwrap_err();
        assert!(error.contains("configure exactly one"));
    }
}
