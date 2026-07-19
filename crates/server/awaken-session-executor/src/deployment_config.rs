//! The deployment configuration surface, parsed once from the environment.
//!
//! Historically the deployment axes — durable ingress, the commit/dispatch store
//! backends, the cross-node wake, the worker role — were read via scattered
//! `std::env::var` calls deep inside the runtime library. That is a hidden global
//! dependency: the library reaches into process env, which cannot be unit-tested
//! without mutating it and gives no single place to read a deployment's shape.
//!
//! [`DeploymentConfig`] is that single typed surface. The composition root builds
//! one — from [`DeploymentConfig::from_env`] (backward-compatible with the historic
//! `AWAKEN_*` variables) or explicitly in a test — and the library reads *it*, not
//! the environment. This is step ① of the config-driven-deployment cleanup: pull
//! env-reading out of the library; migrate call sites onto the injected config.

use std::path::PathBuf;

/// The commit-store backend for a thread's committed truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    /// Per-thread SQLite databases under the storage dir (the default durable store).
    Sqlite,
    /// Per-thread filesystem append-log directories (`AWAKEN_STORE=fs`).
    Fs,
    /// One shared Postgres coordinator keyed by thread (`AWAKEN_STORE=postgres`).
    Postgres,
}

/// The dispatch-queue backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchBackend {
    /// One local SQLite queue file (`dispatch.db`) under the storage dir, or an
    /// in-memory queue when there is no dir. The default.
    Sqlite,
    /// One shared Postgres queue across the fleet (`AWAKEN_DISPATCH_BACKEND=postgres`).
    Postgres,
}

/// The cross-node wake for the served dispatch pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wake {
    /// In-process `LocalWakeSignal` + poll (no cross-node wake). The default.
    None,
    /// `pg_notify` on the Postgres store's own database.
    PgNotify,
    /// A NATS broker (requires the `nats` feature).
    Nats,
}

/// The sandbox tier a worker realizes an ACP agent on (ADR-0041/0056). The default is
/// the namespace (bubblewrap) tier; a container tier runs the agent inside a
/// user-supplied image via the matching `ContainerRuntime`. Different workers can be
/// configured differently (`AWAKEN_SANDBOX_TIER`), so one fleet mixes backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxTier {
    /// No OS isolation — the ACP CLI runs as a plain child of the runtime process
    /// (`AWAKEN_SANDBOX_TIER=local`). The environment-agnostic executor drives it
    /// over the same [`AgentChannelSource`] as any sandboxed tier; only the host's
    /// choice of source differs (ADR-0057: the executor never learns the tier). For
    /// a trusted CLI or single-tenant dev where isolation is provided elsewhere.
    Local,
    /// Bubblewrap namespace isolation on the worker host (`AWAKEN_SANDBOX_TIER=namespace`,
    /// the default) — no user image, the agent runs under `bwrap`.
    #[default]
    Namespace,
    /// A Docker container from the configured image (`AWAKEN_SANDBOX_TIER=docker`).
    Docker,
    /// A rootless Podman container (`AWAKEN_SANDBOX_TIER=podman`).
    Podman,
    /// A Kubernetes Pod (`AWAKEN_SANDBOX_TIER=k8s`), for a multi-node cloud fleet.
    K8s,
}

impl SandboxTier {
    /// Parse the `AWAKEN_SANDBOX_TIER` value; unknown/absent → the namespace default.
    fn from_env_str(value: Option<&str>) -> Self {
        match value {
            Some("local") | Some("none") => Self::Local,
            Some("docker") => Self::Docker,
            Some("podman") => Self::Podman,
            Some("k8s") | Some("kubernetes") => Self::K8s,
            _ => Self::Namespace,
        }
    }

    /// Whether this tier runs the agent inside a container image (vs. the local or
    /// namespace tiers on the worker host) — the composition root builds a container
    /// ACP source.
    #[must_use]
    pub fn is_container(self) -> bool {
        matches!(self, Self::Docker | Self::Podman | Self::K8s)
    }
}

/// The deployment axes a single binary composes from — parsed once, injected into
/// the runtime rather than re-read from the environment at each call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentConfig {
    /// Durable ingress (`AWAKEN_INGRESS=durable`): spawn the standing dispatch pool
    /// and route runs through the durable queue. Direct otherwise.
    pub durable: bool,
    /// The durable storage root (`AWAKEN_STORAGE_DIR`); `None` = in-memory/ephemeral.
    pub storage_dir: Option<PathBuf>,
    /// The commit-store backend (`AWAKEN_STORE`).
    pub store: StoreKind,
    /// The dispatch-queue backend (`AWAKEN_DISPATCH_BACKEND`).
    pub dispatch_backend: DispatchBackend,
    /// The cross-node wake (`AWAKEN_DISPATCH_WAKE`).
    pub wake: Wake,
    /// The wake channel/subject (`AWAKEN_DISPATCH_WAKE_CHANNEL`).
    pub wake_channel: String,
    /// The NATS broker url for `Wake::Nats` (`AWAKEN_NATS_URL`).
    pub nats_url: Option<String>,
    /// The shared database url for Postgres backends (`AWAKEN_DATABASE_URL`).
    pub database_url: Option<String>,
    /// The dispatch lease owner (`AWAKEN_DISPATCH_OWNER`), distinct per process/node.
    pub dispatch_owner: String,
    /// When set, this process is a database-less **worker** of the cell server at
    /// this url (`AWAKEN_UPSTREAM_URL`): commits and dispatch go to the server.
    pub upstream: Option<String>,
    /// The sandbox tier this worker realizes ACP agents on (`AWAKEN_SANDBOX_TIER`).
    pub sandbox_tier: SandboxTier,
    /// The container image an ACP agent runs in on a container tier
    /// (`AWAKEN_CONTAINER_IMAGE`); `None` on the namespace tier / when unset.
    pub container_image: Option<String>,
    /// A coordinator-only server (`AWAKEN_DISABLE_LOCAL_POOL=1`): own the store + HTTP
    /// but run no local pool, so remote workers are the sole drainers.
    pub disable_local_pool: bool,
}

/// The default wake channel/subject, shared by the `pg_notify` channel and the NATS
/// subject.
pub const DEFAULT_WAKE_CHANNEL: &str = "awaken_dispatch_wake";

impl DeploymentConfig {
    /// Parse the deployment axes from the historic `AWAKEN_*` environment variables.
    /// This is the backward-compatible bridge: every existing deployment keeps
    /// working unchanged; the library now reads the parsed config instead of env.
    pub fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let store = match env("AWAKEN_STORE").as_deref() {
            Some("postgres") => StoreKind::Postgres,
            Some("fs") => StoreKind::Fs,
            _ => StoreKind::Sqlite,
        };
        let dispatch_backend = match env("AWAKEN_DISPATCH_BACKEND").as_deref() {
            Some("postgres") => DispatchBackend::Postgres,
            _ => DispatchBackend::Sqlite,
        };
        let wake = match env("AWAKEN_DISPATCH_WAKE").as_deref() {
            Some("pg-notify") => Wake::PgNotify,
            Some("nats") => Wake::Nats,
            _ => Wake::None,
        };
        let sandbox_tier = SandboxTier::from_env_str(env("AWAKEN_SANDBOX_TIER").as_deref());
        let disable_local_pool = local_pool_disabled(
            env("AWAKEN_SERVER_RUN_LOCAL_POOL").as_deref(),
            env("AWAKEN_DISABLE_LOCAL_POOL").as_deref(),
        );
        Self {
            durable: env("AWAKEN_INGRESS").as_deref() == Some("durable"),
            storage_dir: env("AWAKEN_STORAGE_DIR").map(PathBuf::from),
            store,
            dispatch_backend,
            wake,
            wake_channel: env("AWAKEN_DISPATCH_WAKE_CHANNEL")
                .unwrap_or_else(|| DEFAULT_WAKE_CHANNEL.to_string()),
            nats_url: env("AWAKEN_NATS_URL"),
            database_url: env("AWAKEN_DATABASE_URL"),
            dispatch_owner: env("AWAKEN_DISPATCH_OWNER").unwrap_or_else(default_owner),
            upstream: env("AWAKEN_UPSTREAM_URL"),
            sandbox_tier,
            container_image: env("AWAKEN_CONTAINER_IMAGE"),
            disable_local_pool,
        }
    }

    /// Whether a durable ingress is backed by a persistent queue (Postgres, an
    /// on-disk SQLite dir, or an injected backend). A durable ingress on a volatile
    /// in-memory queue silently drops queued/crashed/scheduled runs on restart, so
    /// the composition root refuses to serve one — the no-data-loss invariant.
    /// `injected` is passed in because an assembled shard fan-out lives outside this
    /// config (it owns its own durability contract).
    pub fn durable_needs_persistence_error(&self, injected: bool) -> Option<&'static str> {
        let postgres_backend = self.dispatch_backend == DispatchBackend::Postgres;
        let has_storage_dir = self.storage_dir.is_some();
        if self.durable && !postgres_backend && !has_storage_dir && !injected {
            return Some(
                "AWAKEN_INGRESS=durable needs a persistent dispatch queue, but none is \
                 configured: the default SQLite backend has no AWAKEN_STORAGE_DIR, so the \
                 queue would be in-memory and a restart would silently drop every queued, \
                 crashed, dead-lettered, and scheduled run. Set AWAKEN_STORAGE_DIR for the \
                 durable on-disk queue (<dir>/dispatch.db), or AWAKEN_DISPATCH_BACKEND=postgres \
                 with AWAKEN_DATABASE_URL for the shared queue. Refusing to serve a 'durable' \
                 ingress on a volatile queue.",
            );
        }
        None
    }
}

/// One parser for the positive deployment axis and its legacy negated alias.
/// An explicit new value wins, matching the CLI validation layer.
fn local_pool_disabled(run_local_pool: Option<&str>, legacy_disable: Option<&str>) -> bool {
    match run_local_pool {
        Some(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "false" | "0" | "no"
        ),
        None => legacy_disable == Some("1"),
    }
}

/// The default dispatch owner: `<hostname>-<pid>`, distinct per process and node so
/// the lease is owner-scoped (single-owner-per-run, ADR-0019/0024).
fn default_owner() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string());
    format!("{host}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DeploymentConfig {
        DeploymentConfig {
            durable: false,
            storage_dir: None,
            store: StoreKind::Sqlite,
            dispatch_backend: DispatchBackend::Sqlite,
            wake: Wake::None,
            wake_channel: DEFAULT_WAKE_CHANNEL.to_string(),
            nats_url: None,
            database_url: None,
            dispatch_owner: "host-1".into(),
            upstream: None,
            sandbox_tier: SandboxTier::Namespace,
            container_image: None,
            disable_local_pool: false,
        }
    }

    #[test]
    fn sandbox_tier_parses_the_worker_backend_and_defaults_to_namespace() {
        assert_eq!(
            SandboxTier::from_env_str(Some("docker")),
            SandboxTier::Docker
        );
        assert_eq!(
            SandboxTier::from_env_str(Some("podman")),
            SandboxTier::Podman
        );
        assert_eq!(SandboxTier::from_env_str(Some("k8s")), SandboxTier::K8s);
        assert_eq!(
            SandboxTier::from_env_str(Some("kubernetes")),
            SandboxTier::K8s
        );
        // Unknown / absent → the namespace (bwrap) default; the host tier stays local.
        assert_eq!(SandboxTier::from_env_str(Some("?")), SandboxTier::Namespace);
        assert_eq!(SandboxTier::from_env_str(None), SandboxTier::Namespace);
        assert_eq!(SandboxTier::default(), SandboxTier::Namespace);

        // Only the container tiers run the agent inside an image.
        assert!(!SandboxTier::Namespace.is_container());
        for t in [SandboxTier::Docker, SandboxTier::Podman, SandboxTier::K8s] {
            assert!(t.is_container());
        }
    }

    #[test]
    fn local_pool_axis_matches_the_cli_and_new_name_wins() {
        for value in ["false", "0", "no", "FALSE"] {
            assert!(local_pool_disabled(Some(value), None), "{value}");
        }
        for value in ["true", "1", "yes"] {
            assert!(!local_pool_disabled(Some(value), Some("1")), "{value}");
        }
        assert!(local_pool_disabled(None, Some("1")));
        assert!(!local_pool_disabled(None, None));
    }

    #[test]
    fn durable_on_a_volatile_sqlite_queue_is_refused() {
        // durable + default sqlite + no storage dir + not injected → the footgun.
        let cfg = DeploymentConfig {
            durable: true,
            ..base()
        };
        let err = cfg.durable_needs_persistence_error(false).unwrap();
        assert!(err.contains("AWAKEN_STORAGE_DIR"), "{err}");
        assert!(err.contains("durable"), "{err}");
    }

    #[test]
    fn durable_is_accepted_on_any_persistent_backing() {
        // A storage dir, Postgres, or an injected backend each satisfy the contract.
        let with_dir = DeploymentConfig {
            durable: true,
            storage_dir: Some("/data".into()),
            ..base()
        };
        assert!(with_dir.durable_needs_persistence_error(false).is_none());

        let with_pg = DeploymentConfig {
            durable: true,
            dispatch_backend: DispatchBackend::Postgres,
            ..base()
        };
        assert!(with_pg.durable_needs_persistence_error(false).is_none());

        let injected = DeploymentConfig {
            durable: true,
            ..base()
        };
        assert!(injected.durable_needs_persistence_error(true).is_none());
    }

    #[test]
    fn a_direct_ingress_never_requires_persistence() {
        assert!(base().durable_needs_persistence_error(false).is_none());
    }

    #[test]
    fn a_postgres_commit_store_does_not_satisfy_the_dispatch_queue() {
        // The commit store and the dispatch queue are SEPARATE backends: selecting
        // Postgres for committed truth (`store`) does not make the dispatch queue
        // persistent. With a durable ingress on the default SQLite *dispatch* backend
        // and no storage dir, the queue is still in-memory — so this must remain the
        // refused footgun, driven only by `dispatch_backend`/storage-dir, not `store`.
        let cfg = DeploymentConfig {
            durable: true,
            store: StoreKind::Postgres,
            dispatch_backend: DispatchBackend::Sqlite,
            storage_dir: None,
            ..base()
        };
        assert!(
            cfg.durable_needs_persistence_error(false).is_some(),
            "a postgres COMMIT store must not be mistaken for a persistent DISPATCH queue"
        );
    }
}
