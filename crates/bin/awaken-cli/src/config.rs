//! Product bootstrap configuration for the `awaken` command.
//!
//! This module is the only production boundary that reads `AWAKEN_*` process
//! environment variables. It resolves command-line overrides, environment,
//! `~/.awaken/config.toml`, and defaults into one [`ResolvedDeployment`]. The
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

pub const CONFIG_FILE_ENV: &str = "AWAKEN_CONFIG_FILE";
pub const DATA_DIR_ENV: &str = "AWAKEN_DATA_DIR";
pub const BIND_ENV: &str = "AWAKEN_BIND";
pub const MODE_ENV: &str = "AWAKEN_MODE";
pub const NO_BROWSER_ENV: &str = "AWAKEN_NO_BROWSER";
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
            other => Err(format!(
                "invalid AWAKEN_ROLE={other:?}: expected serve or worker"
            )),
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
            other => Err(format!(
                "invalid {MODE_ENV}={other:?}: expected local or server"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

/// CLI values have the highest precedence. `None` leaves resolution to the
/// environment, config file, and defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
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
    fn resolve_from_map(
        data_dir: &Path,
        env: &BTreeMap<String, String>,
        configured_url: Option<String>,
    ) -> Result<Self, String> {
        let selected = env_get(env, "AWAKEN_RESOURCE_DATABASE_URL")
            .or(configured_url)
            .or_else(|| env_get(env, "AWAKEN_RESOURCE_LIFECYCLE_DB"))
            .or_else(|| env_get(env, "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL"))
            .or_else(|| {
                let legacy_shared = env_get(env, "AWAKEN_STORE").as_deref() == Some("postgres")
                    || env_get(env, "AWAKEN_DISPATCH_BACKEND").as_deref() == Some("postgres");
                legacy_shared
                    .then(|| env_get(env, "AWAKEN_DATABASE_URL"))
                    .flatten()
            });
        match selected {
            Some(value) if is_postgres_url(&value) => Ok(Self::Postgres(value)),
            Some(_) => Err(
                "AWAKEN_RESOURCE_DATABASE_URL must be a postgres:// URL; local resources use the resolved data directory"
                    .to_owned(),
            ),
            None => Ok(Self::Embedded(data_dir.to_path_buf())),
        }
    }

    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Postgres(_))
    }

    /// Compatibility adapter for existing embeddings. Product startup uses the
    /// already-resolved [`ResolvedDeployment`] and never calls this function.
    pub fn from_env(data_dir: &Path) -> Result<Self, String> {
        let env = std::env::vars()
            .filter(|(key, value)| key.starts_with("AWAKEN_") && !value.trim().is_empty())
            .collect::<BTreeMap<_, _>>();
        Self::resolve_from_map(data_dir, &env, None)
    }

    pub fn validate_runtime_shape(
        &self,
        shared_runtime: bool,
        shared_resource_catalog: bool,
    ) -> Result<(), &'static str> {
        if shared_runtime && !self.is_shared() {
            Err("a shared Postgres runtime requires AWAKEN_RESOURCE_DATABASE_URL")
        } else if shared_runtime && !shared_resource_catalog {
            Err("a shared Postgres runtime requires AWAKEN_ADMIN_DB to use Postgres")
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
            Self::Inline(_) => "environment (redacted)".to_owned(),
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
            .finish()
    }
}

impl ResolvedDeployment {
    pub fn load(overrides: ConfigOverrides) -> Result<Self, String> {
        let env = std::env::vars()
            .filter(|(key, value)| key.starts_with("AWAKEN_") && !value.trim().is_empty())
            .collect::<BTreeMap<_, _>>();
        let home = home_dir();
        let config_path = env_get(&env, CONFIG_FILE_ENV)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".awaken/config.toml")))
            .ok_or_else(|| {
                format!("cannot determine the config path; set {CONFIG_FILE_ENV} or {DATA_DIR_ENV}")
            })?;
        let explicit_path = env.contains_key(CONFIG_FILE_ENV);
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
        Self::resolve(overrides, env, home, config_path, file)
    }

    fn resolve(
        overrides: ConfigOverrides,
        env: BTreeMap<String, String>,
        home: Option<PathBuf>,
        config_path: PathBuf,
        file: FileConfig,
    ) -> Result<Self, String> {
        let mut deprecations = Vec::new();
        let mut origins = BTreeMap::new();

        let (env_data_dir, env_data_origin) = env_with_aliases(
            &env,
            DATA_DIR_ENV,
            &[
                "AWAKEN_DEPLOYMENT_DATA_DIR",
                "AWAKEN_STORAGE_DIR",
                "AWAKEN_MGMT_DIR",
            ],
            &mut deprecations,
        );
        let data_dir = if let Some(path) = overrides.data_dir {
            origins.insert("data_dir".to_owned(), "command line".to_owned());
            path
        } else if let Some(path) = env_data_dir {
            origins.insert("data_dir".to_owned(), env_data_origin);
            PathBuf::from(path)
        } else if let Some(path) = file.data_dir {
            origins.insert("data_dir".to_owned(), "config.toml".to_owned());
            path
        } else {
            origins.insert("data_dir".to_owned(), "default".to_owned());
            home.as_ref()
                .map(|home| home.join(".awaken"))
                .ok_or_else(|| format!("cannot determine home directory; set {DATA_DIR_ENV}"))?
        };

        let env_bind = env_get(&env, BIND_ENV).or_else(|| {
            env_get(&env, "AWAKEN_HTTP_ADDR").inspect(|_| {
                deprecations.push(format!("AWAKEN_HTTP_ADDR is deprecated; use {BIND_ENV}"));
            })
        });
        let mut bind = env_bind
            .or(file.bind)
            .unwrap_or_else(|| DEFAULT_BIND.to_owned());
        if let Some(port) = overrides.port {
            bind = override_port(&bind, port)?;
            origins.insert("bind".to_owned(), "command line".to_owned());
        } else {
            origins.insert(
                "bind".to_owned(),
                if env.contains_key(BIND_ENV) || env.contains_key("AWAKEN_HTTP_ADDR") {
                    "environment"
                } else if bind != DEFAULT_BIND {
                    "config.toml"
                } else {
                    "default"
                }
                .to_owned(),
            );
        }
        bind.parse::<std::net::SocketAddr>()
            .map_err(|_| format!("invalid bind address {bind:?}; expected IP:PORT"))?;

        let mode = match env_get(&env, MODE_ENV).or(file.mode) {
            Some(value) => OperatingMode::parse(&value)?,
            None => OperatingMode::Local,
        };
        let role = match overrides.role {
            Some(role) => role,
            None => match env_get(&env, "AWAKEN_ROLE").or(file.role) {
                Some(value) => Role::parse(&value)?,
                None => Role::Serve,
            },
        };

        let (legacy_server, legacy_server_origin) = env_with_aliases(
            &env,
            "AWAKEN_WORKER_SERVE_URL",
            &["AWAKEN_UPSTREAM_URL"],
            &mut deprecations,
        );
        let worker_server = overrides
            .worker_server
            .or(legacy_server)
            .or(file.worker_server);
        if role == Role::Worker && worker_server.is_none() {
            return Err(
                "a worker needs its server URL: pass --server or set AWAKEN_WORKER_SERVE_URL"
                    .to_owned(),
            );
        }
        if !legacy_server_origin.is_empty() {
            origins.insert("worker_server".to_owned(), legacy_server_origin);
        }
        let worker = WorkerBootstrap {
            admin_listen: env_get(&env, "AWAKEN_WORKER_ADMIN_LISTEN")
                .or(file.worker_admin_listen)
                .or_else(|| Some("0.0.0.0:9090".to_owned())),
            drain_grace_secs: env_get(&env, "AWAKEN_WORKER_DRAIN_GRACE_SECS")
                .map(|value| parse_u64("AWAKEN_WORKER_DRAIN_GRACE_SECS", &value))
                .transpose()?
                .or(file.worker_drain_grace_secs)
                .unwrap_or(20),
            build_digest: env_get(&env, "AWAKEN_WORKER_BUILD_DIGEST").or(file.worker_build_digest),
            zone: env_get(&env, "AWAKEN_WORKER_ZONE").or(file.worker_zone),
            capabilities: env_get(&env, "AWAKEN_WORKER_CAPABILITIES")
                .map(|value| split_csv(&value))
                .or(file.worker_capabilities)
                .unwrap_or_default(),
            max_concurrent: env_get(&env, "AWAKEN_WORKER_MAX_CONCURRENT")
                .map(|value| parse_u32("AWAKEN_WORKER_MAX_CONCURRENT", &value))
                .transpose()?
                .or(file.worker_max_concurrent),
        };

        let run_local_pool = env_get(&env, "AWAKEN_SERVER_RUN_LOCAL_POOL")
            .map(|value| parse_bool("AWAKEN_SERVER_RUN_LOCAL_POOL", &value))
            .transpose()?
            .or(file.run_local_pool)
            .unwrap_or_else(|| {
                if env_get(&env, "AWAKEN_DISABLE_LOCAL_POOL").as_deref() == Some("1") {
                    deprecations.push(
                        "AWAKEN_DISABLE_LOCAL_POOL is deprecated; use AWAKEN_SERVER_RUN_LOCAL_POOL"
                            .to_owned(),
                    );
                    false
                } else {
                    true
                }
            });

        let env_no_browser = env_get(&env, NO_BROWSER_ENV)
            .map(|value| parse_bool(NO_BROWSER_ENV, &value))
            .transpose()?;
        let no_browser = overrides
            .no_browser
            .or(env_no_browser)
            .or(file.no_browser)
            .unwrap_or(false);

        let dispatch_url = env_get(&env, "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL")
            .or_else(|| file.runtime_database_url.clone())
            .or_else(|| {
                (env_get(&env, "AWAKEN_DISPATCH_BACKEND").as_deref() == Some("postgres"))
                    .then(|| {
                        deprecations.push(
                            "AWAKEN_DISPATCH_BACKEND/AWAKEN_DATABASE_URL are deprecated; use AWAKEN_RUNTIME_DISPATCH_DATABASE_URL"
                                .to_owned(),
                        );
                        env_get(&env, "AWAKEN_DATABASE_URL")
                    })
                    .flatten()
            });
        if dispatch_url
            .as_ref()
            .is_some_and(|url| !is_postgres_url(url))
        {
            return Err("AWAKEN_RUNTIME_DISPATCH_DATABASE_URL must be postgres://".to_owned());
        }
        let legacy_store = env_get(&env, "AWAKEN_STORE");
        let store = match legacy_store.as_deref() {
            Some("postgres") => StoreKind::Postgres,
            Some("fs") => StoreKind::Fs,
            Some("sqlite") | None => StoreKind::Sqlite,
            Some(other) => return Err(format!("invalid AWAKEN_STORE={other:?}")),
        };
        let database_url = if store == StoreKind::Postgres {
            dispatch_url
                .clone()
                .or_else(|| env_get(&env, "AWAKEN_DATABASE_URL"))
        } else {
            dispatch_url.clone()
        };
        let dispatch_backend = if dispatch_url.is_some() {
            DispatchBackend::Postgres
        } else {
            DispatchBackend::Sqlite
        };
        if !run_local_pool && dispatch_backend != DispatchBackend::Postgres {
            return Err(
                "AWAKEN_SERVER_RUN_LOCAL_POOL=false requires AWAKEN_RUNTIME_DISPATCH_DATABASE_URL"
                    .to_owned(),
            );
        }

        let sandbox_tier = match env_get(&env, "AWAKEN_SANDBOX_TIER").as_deref() {
            Some("local" | "none") => SandboxTier::Local,
            Some("docker") => SandboxTier::Docker,
            Some("podman") => SandboxTier::Podman,
            Some("k8s" | "kubernetes") => SandboxTier::K8s,
            Some("namespace") | None => SandboxTier::Namespace,
            Some(other) => return Err(format!("invalid AWAKEN_SANDBOX_TIER={other:?}")),
        };
        let acp_ids = env_get(&env, "AWAKEN_ACP_CLIS")
            .or_else(|| env_get(&env, "AWAKEN_ACP_CLI"))
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let acp = (!acp_ids.is_empty())
            .then(|| AcpWorkerProfile::new(acp_ids, env_get(&env, "AWAKEN_ACP_DEFAULT_CLI")))
            .transpose()?;
        let wake = match env_get(&env, "AWAKEN_DISPATCH_WAKE").as_deref() {
            Some("pg-notify") => Wake::PgNotify,
            Some("nats") => Wake::Nats,
            Some("none") | None => Wake::None,
            Some(other) => return Err(format!("invalid AWAKEN_DISPATCH_WAKE={other:?}")),
        };
        let runtime = DeploymentConfig {
            durable: true,
            storage_dir: Some(data_dir.clone()),
            store,
            dispatch_backend,
            wake,
            wake_channel: env_get(&env, "AWAKEN_DISPATCH_WAKE_CHANNEL")
                .unwrap_or_else(|| DEFAULT_WAKE_CHANNEL.to_owned()),
            nats_url: env_get(&env, "AWAKEN_NATS_URL"),
            database_url,
            dispatch_owner: env_get(&env, "AWAKEN_DISPATCH_OWNER")
                .unwrap_or_else(|| format!("host-{}", std::process::id())),
            upstream: worker_server.clone(),
            sandbox_tier,
            sandbox_tier_explicit: env.contains_key("AWAKEN_SANDBOX_TIER"),
            sandbox_dir: env_get(&env, "AWAKEN_SANDBOX_DIR").map(PathBuf::from),
            acp_session_blob_root: env_get(&env, "AWAKEN_ACP_SESSION_BLOBS").map(PathBuf::from),
            acp,
            container_image: env_get(&env, "AWAKEN_CONTAINER_IMAGE"),
            disable_local_pool: !run_local_pool,
        };

        let control = awaken_control::ControlStoreConfig::resolve(&data_dir, |key| {
            env_get(&env, key).or_else(|| match key {
                "AWAKEN_CATALOG_DB" => file.catalog_db.clone(),
                "AWAKEN_CREDENTIAL_DB" => file.credential_db.clone(),
                "AWAKEN_CONFIG_DB" => file.config_db.clone(),
                "AWAKEN_ADMIN_DB" => file.admin_db.clone(),
                "AWAKEN_SESSIONS_DB" => file.sessions_db.clone(),
                _ => None,
            })
        });
        let resources = ResourcePlaneStoreBackend::resolve_from_map(
            &data_dir,
            &env,
            file.resource_database_url.clone(),
        )?;
        if !env.contains_key("AWAKEN_RESOURCE_DATABASE_URL")
            && file.resource_database_url.is_none()
            && resources.is_shared()
        {
            deprecations.push(
                "resource Postgres inferred from the runtime database; set AWAKEN_RESOURCE_DATABASE_URL explicitly"
                    .to_owned(),
            );
        }
        let shared_resource_catalog =
            matches!(&control.admin, awaken_control::StoreBackend::Postgres(_));
        if dispatch_backend == DispatchBackend::Postgres
            && (!resources.is_shared() || !shared_resource_catalog)
        {
            return Err(
                "a shared runtime requires AWAKEN_RESOURCE_DATABASE_URL and AWAKEN_ADMIN_DB to use Postgres"
                    .to_owned(),
            );
        }

        let inline_key = env_get(&env, "AWAKEN_CONTROL_SEAL_KEY")
            .or_else(|| env_get(&env, "AWAKEN_MGMT_SEAL_KEY"));
        let key_file = env_get(&env, "AWAKEN_CONTROL_SEAL_KEY_FILE")
            .or_else(|| env_get(&env, "AWAKEN_MGMT_SEAL_KEY_FILE"));
        if env.contains_key("AWAKEN_MGMT_SEAL_KEY") {
            deprecations
                .push("AWAKEN_MGMT_SEAL_KEY is deprecated; use AWAKEN_CONTROL_SEAL_KEY".to_owned());
        }
        if env.contains_key("AWAKEN_MGMT_SEAL_KEY_FILE") {
            deprecations.push(
                "AWAKEN_MGMT_SEAL_KEY_FILE is deprecated; use AWAKEN_CONTROL_SEAL_KEY_FILE"
                    .to_owned(),
            );
        }
        let seal_key = match (inline_key, key_file, mode) {
            (Some(_), Some(_), _) => {
                return Err(
                    "set exactly one of AWAKEN_CONTROL_SEAL_KEY or AWAKEN_CONTROL_SEAL_KEY_FILE"
                        .to_owned(),
                );
            }
            (Some(value), None, _) => SealKeySource::Inline(value),
            (None, Some(path), _) => SealKeySource::File(PathBuf::from(path)),
            (None, None, OperatingMode::Local) => {
                SealKeySource::LocalFile(data_dir.join("control-seal.key"))
            }
            (None, None, OperatingMode::Server) => {
                return Err(
                    "server mode requires AWAKEN_CONTROL_SEAL_KEY or AWAKEN_CONTROL_SEAL_KEY_FILE"
                        .to_owned(),
                );
            }
        };

        let identity_value = env_get(&env, "AWAKEN_IDENTITY_MODE")
            .or_else(|| env_get(&env, "AWAKEN_MGMT_IAM"))
            .or(file.identity_mode);
        let identity_mode = match identity_value {
            Some(value) => awaken_control::ManagementIdentityMode::parse(&value).ok_or_else(|| {
                format!(
                    "invalid identity mode {value:?}: expected no-login, awaken-cloud, or self-managed"
                )
            })?,
            None => awaken_control::ManagementIdentityMode::NoLogin,
        };
        let org_id = env_get(&env, "AWAKEN_ORG_ID")
            .or(file.org_id)
            .unwrap_or_else(|| awaken_control::DEFAULT_ORG_ID.to_owned());
        let iam_workspaces = env_get(&env, "AWAKEN_IAM_WORKSPACES")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .or(file.iam_workspaces)
            .unwrap_or_default();
        let cloud_iam = CloudIamConfig {
            base_url: env_get(&env, "AWAKEN_CLOUD_IAM_URL")
                .or(file.cloud_iam_url)
                .unwrap_or_else(|| "https://accounts.awakenworks.com".to_owned()),
            audience: env_get(&env, "AWAKEN_CLOUD_IAM_AUDIENCE")
                .or(file.cloud_iam_audience)
                .unwrap_or_else(|| "awaken-runtime".to_owned()),
            issuer: env_get(&env, "AWAKEN_CLOUD_IAM_ISSUER")
                .or(file.cloud_iam_issuer)
                .unwrap_or_else(|| "https://accounts.awakenworks.com".to_owned()),
            access_token: env_get(&env, "AWAKEN_CLOUD_ACCESS_TOKEN"),
            service_token: env_get(&env, "AWAKEN_CLOUD_IAM_SERVICE_TOKEN"),
        };

        Ok(Self {
            role,
            mode,
            bind,
            data_dir,
            config_file_exists: config_path.exists(),
            config_path,
            no_browser,
            run_local_pool,
            worker_server,
            worker,
            identity_mode,
            org_id,
            iam_workspaces,
            cloud_iam,
            mcp_bearer_token: env_get(&env, "AWAKEN_MCP_BEARER_TOKEN"),
            admin_listen: env_get(&env, "AWAKEN_SERVER_ADMIN_LISTEN").or(file.admin_listen),
            runtime,
            control,
            resources,
            seal_key,
            deprecations,
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
            let environment = ENV_REFERENCE.iter().copied().collect::<BTreeMap<_, _>>();
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
                "environment": environment,
            }))
            .expect("configuration report is serializable");
        }
        let mut report = format!(
            "Awaken configuration\n\n  role                 {role}\n  mode                 {mode}\n  bind                 {bind}\n  data directory       {data}\n  config file          {config} ({exists})\n  local worker pool    {pool}\n  runtime dispatch     {runtime}\n  resource plane       {resources}\n  control seal key     {key}\n\nPrecedence: command line > environment > config.toml > defaults.\n",
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
        report.push_str("\nRecognized environment variables (secret values are never printed)\n");
        for (name, description) in ENV_REFERENCE {
            report.push_str(&format!("  {name:<42} {description}\n"));
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
    admin_listen: Option<String>,
}

fn env_get(env: &BTreeMap<String, String>, key: &str) -> Option<String> {
    env.get(key)
        .cloned()
        .filter(|value| !value.trim().is_empty())
}

fn env_with_aliases(
    env: &BTreeMap<String, String>,
    canonical: &str,
    aliases: &[&str],
    warnings: &mut Vec<String>,
) -> (Option<String>, String) {
    if let Some(value) = env_get(env, canonical) {
        return (Some(value), format!("environment:{canonical}"));
    }
    for alias in aliases {
        if let Some(value) = env_get(env, alias) {
            warnings.push(format!("{alias} is deprecated; use {canonical}"));
            return (Some(value), format!("environment:{alias}"));
        }
    }
    (None, String::new())
}

fn parse_bool(name: &str, value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("invalid {name}={value:?}: expected true or false")),
    }
}

fn parse_u64(name: &str, value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {name}={value:?}: expected a non-negative integer"))
}

fn parse_u32(name: &str, value: &str) -> Result<u32, String> {
    value
        .parse()
        .map_err(|_| format!("invalid {name}={value:?}: expected a non-negative integer"))
        .and_then(|value| {
            (value > 0)
                .then_some(value)
                .ok_or_else(|| format!("invalid {name}=0: expected at least 1"))
        })
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
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

pub const ENV_REFERENCE: &[(&str, &str)] = &[
    (
        DATA_DIR_ENV,
        "Persistent data directory (default ~/.awaken)",
    ),
    (CONFIG_FILE_ENV, "Bootstrap config.toml path"),
    (BIND_ENV, "HTTP listen address (default 127.0.0.1:8080)"),
    (MODE_ENV, "Operating mode: local | server"),
    (NO_BROWSER_ENV, "Disable browser opening"),
    ("AWAKEN_WORKER_SERVE_URL", "Control-plane URL for a worker"),
    (
        "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL",
        "Postgres dispatch URL (secret; redacted)",
    ),
    (
        "AWAKEN_RESOURCE_DATABASE_URL",
        "Shared resource Postgres URL (secret; redacted)",
    ),
    (
        "AWAKEN_CONTROL_SEAL_KEY",
        "Control seal key (secret; redacted)",
    ),
    (
        "AWAKEN_CONTROL_SEAL_KEY_FILE",
        "Path containing the control seal key",
    ),
    (
        "AWAKEN_IDENTITY_MODE",
        "Identity mode: no-login | awaken-cloud | self-managed",
    ),
    ("AWAKEN_ORG_ID", "Local organization id"),
    (
        "AWAKEN_IAM_WORKSPACES",
        "Comma-separated self-managed workspace ids",
    ),
    (
        "AWAKEN_MCP_BEARER_TOKEN",
        "Bearer for optional MCP export (secret; redacted)",
    ),
    ("AWAKEN_ROLE", "Process role: serve | worker"),
    (
        "AWAKEN_SERVER_RUN_LOCAL_POOL",
        "Run the in-process worker pool",
    ),
    ("AWAKEN_WORKER_ADMIN_LISTEN", "Worker admin listen address"),
    (
        "AWAKEN_WORKER_DRAIN_GRACE_SECS",
        "Worker shutdown grace period",
    ),
    ("AWAKEN_WORKER_BUILD_DIGEST", "Worker build identity"),
    ("AWAKEN_WORKER_ZONE", "Worker scheduling zone"),
    ("AWAKEN_WORKER_CAPABILITIES", "Extra Worker capabilities"),
    ("AWAKEN_WORKER_MAX_CONCURRENT", "Worker concurrency limit"),
    ("AWAKEN_CATALOG_DB", "Catalog SQLite path or Postgres URL"),
    (
        "AWAKEN_CREDENTIAL_DB",
        "Credential SQLite path or Postgres URL (secret URL redacted)",
    ),
    ("AWAKEN_CONFIG_DB", "Config SQLite path or Postgres URL"),
    ("AWAKEN_ADMIN_DB", "Admin SQLite path or Postgres URL"),
    ("AWAKEN_SESSIONS_DB", "Session SQLite path or Postgres URL"),
    ("AWAKEN_SANDBOX_TIER", "Sandbox tier"),
    ("AWAKEN_SANDBOX_DIR", "Sandbox working directory"),
    ("AWAKEN_ACP_CLIS", "Installed ACP CLI identifiers"),
    ("AWAKEN_ACP_DEFAULT_CLI", "Default ACP CLI identifier"),
    (
        "AWAKEN_DISPATCH_WAKE",
        "Cross-node wake: none | pg-notify | nats",
    ),
    ("AWAKEN_DISPATCH_WAKE_CHANNEL", "Cross-node wake channel"),
    ("AWAKEN_NATS_URL", "NATS broker URL (secret URL redacted)"),
    ("AWAKEN_DISPATCH_OWNER", "Dispatch lease owner identity"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(
        pairs: &[(&str, &str)],
        file: FileConfig,
        overrides: ConfigOverrides,
    ) -> ResolvedDeployment {
        ResolvedDeployment::resolve(
            overrides,
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            Some(PathBuf::from("/home/dev")),
            PathBuf::from("/home/dev/.awaken/config.toml"),
            file,
        )
        .unwrap()
    }

    #[test]
    fn local_defaults_are_durable_and_single_rooted() {
        let config = resolve(&[], FileConfig::default(), ConfigOverrides::default());
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
    fn precedence_is_cli_then_env_then_file_then_default() {
        let config = resolve(
            &[(DATA_DIR_ENV, "/env"), (BIND_ENV, "127.0.0.1:9000")],
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
    fn legacy_roots_converge_on_the_one_data_dir() {
        for key in [
            "AWAKEN_DEPLOYMENT_DATA_DIR",
            "AWAKEN_STORAGE_DIR",
            "AWAKEN_MGMT_DIR",
        ] {
            let config = resolve(
                &[(key, "/legacy")],
                FileConfig::default(),
                Default::default(),
            );
            assert_eq!(config.data_dir, PathBuf::from("/legacy"));
            assert_eq!(
                config.runtime.storage_dir.as_deref(),
                Some(Path::new("/legacy"))
            );
            assert!(
                config
                    .deprecations
                    .iter()
                    .any(|warning| warning.contains(key))
            );
        }
    }

    #[test]
    fn worker_requires_an_explicit_server() {
        let error = ResolvedDeployment::resolve(
            ConfigOverrides {
                role: Some(Role::Worker),
                ..Default::default()
            },
            BTreeMap::new(),
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
            &[
                (
                    "AWAKEN_RUNTIME_DISPATCH_DATABASE_URL",
                    "postgres://user:secret@db/awaken",
                ),
                (
                    "AWAKEN_RESOURCE_DATABASE_URL",
                    "postgres://user:secret@db/resources",
                ),
                ("AWAKEN_ADMIN_DB", "postgres://user:secret@db/admin"),
            ],
            FileConfig::default(),
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
}
