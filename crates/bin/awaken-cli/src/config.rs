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
}

#[derive(Debug, Clone)]
pub struct WorkerBootstrap {
    pub admin_listen: Option<String>,
    pub drain_grace_secs: u64,
    pub build_digest: Option<String>,
    pub zone: Option<String>,
    pub capabilities: Vec<String>,
    pub max_concurrent: Option<u32>,
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
    pub org_id: String,
    pub iam_workspaces: Vec<String>,
    pub cloud_iam: CloudIamConfig,
    pub mcp_bearer_token: Option<String>,
    pub admin_listen: Option<String>,
    pub runtime: DeploymentConfig,
    pub control: awaken_control::ControlStoreConfig,
    pub resources: ResourcePlaneStoreBackend,
    pub seal_key: SealKeySource,
    pub deprecations: Vec<String>,
    pub origins: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct CloudIamConfig {
    pub base_url: String,
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
        };
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
        let control = awaken_control::ControlStoreConfig::from_values(
            &data_dir,
            file.catalog_db.clone(),
            file.credential_db.clone(),
            file.config_db.clone(),
            file.admin_db.clone(),
            file.sessions_db.clone(),
        );
        let resources = match file.resource_database_url.clone() {
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
        let identity_mode = file
            .identity_mode
            .as_deref()
            .map(|value| {
                awaken_control::ManagementIdentityMode::parse(value).ok_or_else(|| {
                    format!("invalid identity_mode {value:?}: expected no-login, awaken-cloud, or self-managed")
                })
            })
            .transpose()?
            .unwrap_or(awaken_control::ManagementIdentityMode::NoLogin);
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
            org_id: file
                .org_id
                .unwrap_or_else(|| awaken_control::DEFAULT_ORG_ID.to_owned()),
            iam_workspaces: file.iam_workspaces.unwrap_or_default(),
            cloud_iam,
            mcp_bearer_token: file.mcp_bearer_token,
            admin_listen: file.admin_listen,
            runtime,
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
            "Awaken configuration\n\n  role                 {role}\n  mode                 {mode}\n  bind                 {bind}\n  data directory       {data}\n  config file          {config} ({exists})\n  local worker pool    {pool}\n  runtime dispatch     {runtime}\n  resource plane       {resources}\n  control seal key     {key}\n\nSources: command line --config or standard config.toml, then defaults.\n",
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
    run_local_pool: Option<bool>,
    no_browser: Option<bool>,
    runtime_database_url: Option<String>,
    resource_database_url: Option<String>,
    catalog_db: Option<String>,
    credential_db: Option<String>,
    config_db: Option<String>,
    admin_db: Option<String>,
    sessions_db: Option<String>,
    identity_mode: Option<String>,
    org_id: Option<String>,
    iam_workspaces: Option<Vec<String>>,
    cloud_iam_url: Option<String>,
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
