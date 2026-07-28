//! Product bootstrap configuration for the `awaken` command.
//!
//! It resolves an explicit `--config` path or the standard
//! `~/.awaken/config.toml`, command-line presentation overrides, and defaults
//! into one [`ResolvedDeployment`]. Process environment is not a deployment,
//! business, model, or credential configuration source. The
//! server, runtime host, worker, diagnostics, and storage commands consume that
//! typed value; they do not independently rediscover the deployment.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use awaken_runtime_host::{
    AcpWorkerProfile, DeploymentConfig, DispatchBackend, SandboxTier, StoreKind, Wake,
};
use serde::Deserialize;

pub const DEFAULT_BIND: &str = "127.0.0.1:8080";
const DEFAULT_WAKE_CHANNEL: &str = "awaken_dispatch_wake";

/// The product process role. Execution-plane `hand` remains the separate
/// `awaken-sandbox hand` binary and is deliberately not a control-plane role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Serve,
    Worker,
}

impl Role {
    fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "serve" | "server" | "coordinator" | "all-in-one" => Ok(Self::Serve),
            "worker" => Ok(Self::Worker),
            other => Err(format!("invalid role={other:?}: expected serve or worker")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Worker => "worker",
        }
    }
}

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

#[derive(Debug, Clone)]
pub struct WorkerBootstrap {
    pub admin_listen: Option<String>,
    pub drain_grace_secs: u64,
    pub build_digest: Option<String>,
    pub zone: Option<String>,
    pub capabilities: Vec<String>,
    pub max_concurrent: Option<u32>,
    pub credential_probe_interval_secs: u64,
    pub credential_observation_ttl_secs: u64,
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
    Inline(String),
    File(PathBuf),
    LocalFile(PathBuf),
}

impl std::fmt::Debug for SealKeySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline(_) => formatter.write_str("Inline([REDACTED])"),
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
            Self::LocalFile(path) => formatter.debug_tuple("LocalFile").field(path).finish(),
        }
    }
}

impl SealKeySource {
    pub fn description(&self) -> String {
        match self {
            Self::Inline(_) => "config.toml (redacted)".to_owned(),
            Self::File(path) | Self::LocalFile(path) => path.display().to_string(),
        }
    }

    /// Resolve the existing operator key or create the Local-mode key exactly
    /// once with owner-only permissions. The returned bytes are never logged.
    pub fn load_or_create(&self) -> Result<[u8; 32], String> {
        let value = match self {
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
    pub run_local_pool: bool,
    pub worker_server: Option<String>,
    pub worker: WorkerBootstrap,
    pub identity_mode: awaken_control::ManagementIdentityMode,
    pub cloud_models: CloudModelMode,
    pub org_id: String,
    pub iam_workspaces: Vec<String>,
    pub cloud_iam: CloudIamConfig,
    pub mcp_bearer_token: Option<String>,
    pub admin_listen: Option<String>,
    pub runtime: DeploymentConfig,
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
            .unwrap_or(Role::Serve);
        let worker_server = overrides.worker_server.or(file.worker_server.clone());
        if role == Role::Worker && worker_server.is_none() {
            return Err(
                "worker_server_required: pass --server or configure worker_server".to_owned(),
            );
        }
        let worker = WorkerBootstrap {
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
        if worker.credential_probe_interval_secs == 0
            || worker.credential_observation_ttl_secs <= worker.credential_probe_interval_secs
        {
            return Err(
                "worker credential observation TTL must be greater than the non-zero probe interval"
                    .to_owned(),
            );
        }
        let run_local_pool = file.run_local_pool.unwrap_or(true);
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
        if !run_local_pool && dispatch_backend != DispatchBackend::Postgres {
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
        let runtime = DeploymentConfig {
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
            dispatch_owner: file
                .dispatch_owner
                .clone()
                .unwrap_or_else(|| format!("host-{}", std::process::id())),
            upstream: worker_server.clone(),
            sandbox_tier,
            sandbox_tier_explicit: file.sandbox_tier.is_some(),
            sandbox_dir: file.sandbox_dir.clone(),
            acp_session_blob_root: file.acp_session_blob_root.clone(),
            acp,
            container_image: file.container_image.clone(),
            disable_local_pool: !run_local_pool,
        };
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
        let seal_key = match (&file.control_seal_key, &file.control_seal_key_file, mode) {
            (Some(_), Some(_), _) => {
                return Err(
                    "configure exactly one of control_seal_key or control_seal_key_file".to_owned(),
                );
            }
            (Some(value), None, _) => SealKeySource::Inline(value.clone()),
            (None, Some(path), _) => SealKeySource::File(path.clone()),
            (None, None, OperatingMode::Local) => {
                SealKeySource::LocalFile(data_dir.join("control-seal.key"))
            }
            (None, None, OperatingMode::Server) => {
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
            .unwrap_or(awaken_control::ManagementIdentityMode::NoLogin);
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
        Ok(Self {
            role,
            mode,
            bind,
            data_dir,
            config_file_exists: config_path.exists(),
            config_path,
            no_browser: overrides.no_browser.or(file.no_browser).unwrap_or(false),
            run_local_pool,
            worker_server,
            worker,
            identity_mode,
            cloud_models,
            org_id: file
                .org_id
                .unwrap_or_else(|| awaken_control::DEFAULT_ORG_ID.to_owned()),
            iam_workspaces: file.iam_workspaces.unwrap_or_default(),
            cloud_iam,
            mcp_bearer_token: file.mcp_bearer_token,
            admin_listen: file.admin_listen,
            runtime,
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
    ) -> Result<(), String> {
        let explicit = self.runtime.acp.clone();
        let selected: Vec<_> = observations
            .iter()
            .filter(|observation| {
                explicit
                    .as_ref()
                    .is_none_or(|profile| profile.cli_ids().any(|id| id == observation.cli_id))
            })
            .cloned()
            .collect();
        let default_cli = explicit
            .as_ref()
            .and_then(|profile| profile.default_cli().map(str::to_string));
        let detected = selected
            .iter()
            .filter(|observation| observation.detected())
            .map(|observation| observation.cli_id.clone())
            .collect::<Vec<_>>();
        self.runtime.acp = if detected.is_empty() {
            None
        } else {
            Some(AcpWorkerProfile::new(detected, default_cli)?)
        };
        self.local_acp_observations = observations;
        Ok(())
    }

    pub fn report(&self, json: bool) -> String {
        let resource_backend = if self.resources.is_shared() {
            "postgres"
        } else {
            "embedded"
        };
        let runtime_backend = match self.runtime.dispatch_backend {
            DispatchBackend::Sqlite => "sqlite",
            DispatchBackend::Postgres => "postgres",
        };
        let databases = BTreeMap::from([
            ("catalog", render_store_backend(&self.control.catalog)),
            ("credential", render_store_backend(&self.control.credential)),
            ("config", render_store_backend(&self.control.config)),
            ("admin", render_store_backend(&self.control.admin)),
            ("sessions", render_store_backend(&self.control.sessions)),
        ]);
        if json {
            return serde_json::to_string_pretty(&serde_json::json!({
                "role": self.role.as_str(),
                "mode": self.mode.as_str(),
                "bind": self.bind,
                "data_dir": self.data_dir,
                "config_file": self.config_path,
                "config_file_exists": self.config_file_exists,
                "no_browser": self.no_browser,
                "run_local_pool": self.run_local_pool,
                "identity_mode": identity_mode_name(self.identity_mode),
                "cloud_models": self.cloud_models.as_str(),
                "runtime_dispatch_backend": runtime_backend,
                "resource_backend": resource_backend,
                "control_databases": databases,
                "database_migrations": "automatic at startup",
                "seal_key": self.seal_key.description(),
                "origins": self.origins,
                "deprecations": self.deprecations,
            }))
            .expect("configuration report is serializable");
        }
        let mut report = format!(
            "Awaken configuration\n\n  role                 {role}\n  mode                 {mode}\n  bind                 {bind}\n  data directory       {data}\n  config file          {config} ({exists})\n  local worker pool    {pool}\n  identity mode        {identity}\n  cloud models         {cloud_models}\n  runtime dispatch     {runtime}\n  resource plane       {resources}\n  control seal key     {key}\n\nSources: command line --config or standard config.toml, then defaults.\n",
            role = self.role.as_str(),
            mode = self.mode.as_str(),
            bind = self.bind,
            data = self.data_dir.display(),
            config = self.config_path.display(),
            exists = if self.config_file_exists {
                "present"
            } else {
                "not created"
            },
            pool = self.run_local_pool,
            identity = identity_mode_name(self.identity_mode),
            cloud_models = self.cloud_models.as_str(),
            runtime = runtime_backend,
            resources = resource_backend,
            key = self.seal_key.description(),
        );
        report.push_str("\nControl databases (migrations run automatically at startup)\n");
        for (name, backend) in databases {
            report.push_str(&format!("  {name:<20} {backend}\n"));
        }
        report
    }
}

const fn identity_mode_name(mode: awaken_control::ManagementIdentityMode) -> &'static str {
    match mode {
        awaken_control::ManagementIdentityMode::NoLogin => "no-login",
        awaken_control::ManagementIdentityMode::AwakenCloud => "awaken-cloud",
        awaken_control::ManagementIdentityMode::SelfManaged => "self-managed",
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

fn render_store_backend(backend: &awaken_control::StoreBackend) -> String {
    match backend {
        awaken_control::StoreBackend::Sqlite(path) => {
            format!("sqlite ({})", path.display())
        }
        awaken_control::StoreBackend::Postgres(_) => "postgres (URL redacted)".to_owned(),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    bind: Option<String>,
    mode: Option<String>,
    role: Option<String>,
    worker_server: Option<String>,
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
    runtime_database_url: Option<String>,
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
    mcp_bearer_token: Option<String>,
    cloud_iam_service_token_file: Option<PathBuf>,
    admin_listen: Option<String>,
    control_seal_key: Option<String>,
    control_seal_key_file: Option<PathBuf>,
    sandbox_tier: Option<String>,
    sandbox_dir: Option<PathBuf>,
    acp_session_blob_root: Option<PathBuf>,
    acp_clis: Option<Vec<String>>,
    acp_default_cli: Option<String>,
    container_image: Option<String>,
    dispatch_wake: Option<String>,
    dispatch_wake_channel: Option<String>,
    nats_url: Option<String>,
    dispatch_owner: Option<String>,
}

fn read_management_database_url(path: &Path) -> Result<String, String> {
    let value = fs::read_to_string(path)
        .map_err(|error| format!("read management database URL {}: {error}", path.display()))?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "management database URL file {} is empty",
            path.display()
        ));
    }
    if !is_postgres_url(value) {
        return Err("management_database_url_file must contain a postgres:// URL".to_owned());
    }
    Ok(value.to_owned())
}

fn override_port(bind: &str, port: u16) -> Result<String, String> {
    let mut address = bind
        .parse::<std::net::SocketAddr>()
        .map_err(|_| format!("invalid bind address {bind:?}; expected IP:PORT"))?;
    address.set_port(port);
    Ok(address.to_string())
}

fn is_postgres_url(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.starts_with("postgres://") || lower.starts_with("postgresql://")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
}

fn read_or_create_local_key(path: &Path) -> Result<String, String> {
    if path.exists() {
        return fs::read_to_string(path)
            .map_err(|error| format!("read local seal key {}: {error}", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let encoded = awaken_credential_vault::generate_seal_key_hex();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(encoded.as_bytes())
                .and_then(|()| file.write_all(b"\n"))
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("write local seal key {}: {error}", path.display()))?;
            Ok(encoded)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut value = String::new();
            fs::File::open(path)
                .and_then(|mut file| file.read_to_string(&mut value))
                .map_err(|error| format!("read local seal key {}: {error}", path.display()))?;
            Ok(value)
        }
        Err(error) => Err(format!("create local seal key {}: {error}", path.display())),
    }
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
        }
    }

    #[test]
    fn local_discovery_is_the_only_source_of_worker_routes_and_defaults() {
        // Cause graph:
        // catalog observations -> detected subset -> AcpWorkerProfile; an
        // explicit configured subset intersects that detected set. The profile
        // constructor alone decides whether one row becomes the default.
        //
        // Decision table:
        // A1 unconfigured + none detected -> no ACP Worker
        // A2 unconfigured + one detected  -> that CLI + automatic default
        // A3 unconfigured + many detected -> all CLIs + no random default
        // A4 explicit subset              -> only detected configured CLIs
        let missing = acp_observation("claude", awaken_acp_application::AcpDetectionState::Missing);
        let codex = acp_observation("codex", awaken_acp_application::AcpDetectionState::Detected);
        let claude = acp_observation(
            "claude",
            awaken_acp_application::AcpDetectionState::Detected,
        );

        let mut none = resolve(FileConfig::default(), ConfigOverrides::default());
        none.apply_local_acp_observations(vec![missing.clone()])
            .unwrap();
        assert!(none.runtime.acp.is_none(), "A1");

        let mut one = resolve(FileConfig::default(), ConfigOverrides::default());
        one.apply_local_acp_observations(vec![codex.clone(), missing])
            .unwrap();
        let one = one.runtime.acp.unwrap();
        assert_eq!(one.cli_ids().collect::<Vec<_>>(), ["codex"], "A2");
        assert_eq!(one.default_cli(), Some("codex"), "A2");

        let mut many = resolve(FileConfig::default(), ConfigOverrides::default());
        many.apply_local_acp_observations(vec![codex.clone(), claude.clone()])
            .unwrap();
        let many = many.runtime.acp.unwrap();
        assert_eq!(
            many.cli_ids().collect::<Vec<_>>(),
            ["claude", "codex"],
            "A3"
        );
        assert_eq!(many.default_cli(), None, "A3");

        let mut explicit = resolve(
            FileConfig {
                acp_clis: Some(vec!["claude".into()]),
                ..Default::default()
            },
            ConfigOverrides::default(),
        );
        explicit
            .apply_local_acp_observations(vec![codex, claude])
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
        assert_eq!(config.role, Role::Serve);
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
    fn cloud_login_and_cloud_model_supply_are_independent_and_fail_closed() {
        // Cause graph: C1 selects Cloud identity; C2 enables Cloud models.
        // E1 identity alone keeps model supply local; E2 C1+C2 enables brokered
        // supply; E3 C2 without C1 is rejected before any network wiring.
        //
        // | Rule | C1 Cloud identity | C2 Cloud models | Result |
        // |---|---:|---:|---|
        // | F1 | 0 | 0 | no-login + local/BYOK only |
        // | F2 | 1 | 0 | Cloud login + local/BYOK only |
        // | F3 | 1 | 1 | Cloud login + brokered supply |
        // | F4 | 0 | 1 | startup configuration error |
        let local = resolve(FileConfig::default(), ConfigOverrides::default());
        assert_eq!(
            local.identity_mode,
            awaken_control::ManagementIdentityMode::NoLogin
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
            assert!(!report.contains("user:secret"), "{report}");
            assert!(!report.contains("postgres://"), "{report}");
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
