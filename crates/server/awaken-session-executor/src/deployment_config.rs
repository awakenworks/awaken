//! The deployment configuration surface, built once by a typed composition root.
//!
//! Historically the deployment axes — durable ingress, the commit/dispatch store
//! backends, the cross-node wake, the worker role — were read via scattered
//! `std::env::var` calls deep inside the runtime library. That is a hidden global
//! dependency: the library reaches into process env, which cannot be unit-tested
//! without mutating it and gives no single place to read a deployment's shape.
//!
//! [`DeploymentConfig`] is that single typed surface. The composition root builds
//! one from a typed configuration file (or explicitly for an embedding), and the
//! library reads it rather than process-global deployment configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The ACP adapters one Worker can actually launch.
///
/// This value is shared by Worker capability advertisement and Host launch
/// routing. It therefore prevents an advertised `acp:<cli>` capability from
/// drifting from the launch routes installed on that same Worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpWorkerProfile {
    cli_ids: BTreeSet<String>,
    default_cli: Option<String>,
    launch_argv: BTreeMap<String, Vec<String>>,
}

impl AcpWorkerProfile {
    pub fn new(
        cli_ids: impl IntoIterator<Item = String>,
        default_cli: Option<String>,
    ) -> Result<Self, String> {
        let mut normalized = BTreeSet::new();
        for cli_id in cli_ids {
            let cli_id = cli_id.trim();
            if cli_id.is_empty() {
                continue;
            }
            if awaken_run_executor_acp::acp_cli(cli_id).is_none() {
                return Err(format!("unknown ACP CLI `{cli_id}`"));
            }
            if !normalized.insert(cli_id.to_string()) {
                return Err(format!("duplicate ACP CLI `{cli_id}`"));
            }
        }
        if normalized.is_empty() {
            return Err("an ACP Worker profile requires at least one CLI".to_string());
        }
        let default_cli = default_cli
            .map(|cli_id| cli_id.trim().to_string())
            .filter(|cli_id| !cli_id.is_empty())
            .or_else(|| {
                (normalized.len() == 1).then(|| normalized.first().expect("one ACP CLI").clone())
            });
        if let Some(default_cli) = &default_cli
            && !normalized.contains(default_cli)
        {
            return Err(format!(
                "default ACP CLI `{default_cli}` is not present in the Worker profile"
            ));
        }
        Ok(Self {
            cli_ids: normalized,
            default_cli,
            launch_argv: BTreeMap::new(),
        })
    }

    /// Build the launch profile from the canonical host-discovery projection.
    /// LoginRequired agents remain advertised so placement can explain the exact
    /// credential state; missing or broken agents are never advertised.
    pub fn from_discovery(
        observations: &[awaken_run_executor_acp::AcpHostObservation],
        default_cli: Option<String>,
    ) -> Result<Option<Self>, String> {
        let detected: Vec<String> = observations
            .iter()
            .filter(|observation| observation.detected())
            .map(|observation| observation.cli_id.clone())
            .collect();
        if detected.is_empty() {
            return Ok(None);
        }
        Self::new(detected, default_cli).map(Some)
    }

    pub fn cli_ids(&self) -> impl Iterator<Item = &str> {
        self.cli_ids.iter().map(String::as_str)
    }

    #[must_use]
    pub fn default_cli(&self) -> Option<&str> {
        self.default_cli.as_deref()
    }

    /// Pin one startup-resolved launch argv to this Worker profile. This is
    /// acquisition evidence, not another adapter definition; model/MCP/env
    /// projection continues to come only from the canonical AcpCli row.
    pub fn set_launch_argv(&mut self, cli_id: &str, argv: Vec<String>) -> Result<(), String> {
        if !self.cli_ids.contains(cli_id) {
            return Err(format!("ACP launch argv names unadvertised CLI `{cli_id}`"));
        }
        if argv.is_empty() || argv[0].trim().is_empty() {
            return Err(format!("ACP launch argv for `{cli_id}` must not be empty"));
        }
        self.launch_argv.insert(cli_id.to_string(), argv);
        Ok(())
    }

    #[must_use]
    pub fn launch_argv(&self, cli_id: &str) -> Option<&[String]> {
        self.launch_argv.get(cli_id).map(Vec::as_slice)
    }
}

/// The commit-store backend for a thread's committed truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    /// Per-thread SQLite databases under the storage dir (the default durable store).
    Sqlite,
    /// Per-thread filesystem append-log directories (`DeploymentConfig::store=Fs`).
    Fs,
    /// One shared Postgres coordinator keyed by thread (`DeploymentConfig::store=Postgres`).
    Postgres,
}

/// The dispatch-queue backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchBackend {
    /// One local SQLite queue file (`dispatch.db`) under the storage dir, or an
    /// in-memory queue when there is no dir. The default.
    Sqlite,
    /// One shared Postgres queue across the fleet (`DeploymentConfig::dispatch_backend=Postgres`).
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
    #[cfg(test)]
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
    /// Durable ingress (`typed durable ingress`): spawn the standing dispatch pool
    /// and route runs through the durable queue. Direct otherwise.
    pub durable: bool,
    /// The durable storage root (`DeploymentConfig::storage_dir`); `None` = in-memory/ephemeral.
    pub storage_dir: Option<PathBuf>,
    /// The commit-store backend (`DeploymentConfig::store`).
    pub store: StoreKind,
    /// The dispatch-queue backend (`DeploymentConfig::dispatch_backend`).
    pub dispatch_backend: DispatchBackend,
    /// The cross-node wake (`AWAKEN_DISPATCH_WAKE`).
    pub wake: Wake,
    /// The wake channel/subject (`AWAKEN_DISPATCH_WAKE_CHANNEL`).
    pub wake_channel: String,
    /// The NATS broker url for `Wake::Nats` (`AWAKEN_NATS_URL`).
    pub nats_url: Option<String>,
    /// The shared database url for Postgres backends (`DeploymentConfig::database_url`).
    pub database_url: Option<String>,
    /// The dispatch lease owner (`AWAKEN_DISPATCH_OWNER`), distinct per process/node.
    pub dispatch_owner: String,
    /// When set, this process is a database-less **worker** of the cell server at
    /// this url (`AWAKEN_UPSTREAM_URL`): commits and dispatch go to the server.
    pub upstream: Option<String>,
    /// The sandbox tier this worker realizes ACP agents on (`AWAKEN_SANDBOX_TIER`).
    pub sandbox_tier: SandboxTier,
    /// Whether the sandbox tier was explicitly selected. An unavailable default
    /// namespace sandbox may degrade to local; an explicit request fails closed.
    pub sandbox_tier_explicit: bool,
    /// The ACP sandbox and per-Session configuration root (`AWAKEN_SANDBOX_DIR`).
    /// `None` selects a process-scoped temporary root.
    pub sandbox_dir: Option<PathBuf>,
    /// The durable ACP Session blob root (`AWAKEN_ACP_SESSION_BLOBS`).
    pub acp_session_blob_root: Option<PathBuf>,
    /// The exact ACP adapters this Worker advertises and serves.
    pub acp: Option<AcpWorkerProfile>,
    /// The container image an ACP agent runs in on a container tier
    /// (`AWAKEN_CONTAINER_IMAGE`); `None` on the namespace tier / when unset.
    pub container_image: Option<String>,
    /// A coordinator-only server (`DeploymentConfig::disable_local_pool=1`): own the store + HTTP
    /// but run no local pool, so remote workers are the sole drainers.
    pub disable_local_pool: bool,
}

/// The default wake channel/subject, shared by the `pg_notify` channel and the NATS
/// subject.
pub const DEFAULT_WAKE_CHANNEL: &str = "awaken_dispatch_wake";

impl DeploymentConfig {
    /// Project the configured Sandbox adapter into the exact secret-free support
    /// a Worker may advertise before provider construction. Runtime Host owns this
    /// mapping because it also owns tier realization; Worker manifest derivation
    /// must not duplicate provider semantics.
    #[must_use]
    pub fn sandbox_support(
        &self,
    ) -> (
        awaken_provisioning_contract::SandboxCapabilities,
        &'static str,
    ) {
        use awaken_provisioning_contract::{IsolationClass, SandboxCapabilities};

        match self.sandbox_tier {
            SandboxTier::Local => (
                SandboxCapabilities {
                    isolation: IsolationClass::Workdir,
                    tool_transparent: false,
                    path_fidelity: false,
                    enforced_readonly: false,
                    network_isolation: false,
                    enforced_network_allowlist: false,
                    secret_egress_substitution: false,
                    resource_limits: false,
                    custom_rootfs: false,
                    package_provisioning: false,
                },
                "local",
            ),
            SandboxTier::Docker | SandboxTier::Podman | SandboxTier::K8s => (
                SandboxCapabilities {
                    isolation: IsolationClass::Container,
                    tool_transparent: true,
                    path_fidelity: true,
                    enforced_readonly: true,
                    // Docker/Podman structurally apply `network none`. The current
                    // Kubernetes adapter only labels restricted pods and cannot claim
                    // enforcement until composition verifies an installed policy.
                    network_isolation: !matches!(self.sandbox_tier, SandboxTier::K8s),
                    enforced_network_allowlist: false,
                    secret_egress_substitution: false,
                    resource_limits: true,
                    custom_rootfs: true,
                    package_provisioning: matches!(self.sandbox_tier, SandboxTier::Podman),
                },
                match self.sandbox_tier {
                    SandboxTier::Docker => "docker",
                    SandboxTier::Podman => "podman",
                    SandboxTier::K8s => "k8s",
                    SandboxTier::Local | SandboxTier::Namespace => {
                        unreachable!("matched container sandbox tier")
                    }
                },
            ),
            SandboxTier::Namespace => (
                SandboxCapabilities {
                    isolation: IsolationClass::Namespace,
                    tool_transparent: true,
                    path_fidelity: true,
                    enforced_readonly: true,
                    network_isolation: true,
                    enforced_network_allowlist: false,
                    secret_egress_substitution: false,
                    resource_limits: false,
                    custom_rootfs: false,
                    package_provisioning: false,
                },
                "namespace",
            ),
        }
    }

    /// Environment-independent defaults for embedding composition roots.
    #[must_use]
    pub fn ephemeral() -> Self {
        Self {
            durable: false,
            storage_dir: None,
            store: StoreKind::Sqlite,
            dispatch_backend: DispatchBackend::Sqlite,
            wake: Wake::None,
            wake_channel: DEFAULT_WAKE_CHANNEL.to_string(),
            nats_url: None,
            database_url: None,
            dispatch_owner: "embedded-worker".to_string(),
            upstream: None,
            sandbox_tier: SandboxTier::Namespace,
            sandbox_tier_explicit: false,
            sandbox_dir: None,
            acp_session_blob_root: None,
            acp: None,
            container_image: None,
            disable_local_pool: false,
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
                "typed durable ingress needs a persistent dispatch queue, but none is \
                 configured: the default SQLite backend has no DeploymentConfig::storage_dir, so the \
                 queue would be in-memory and a restart would silently drop every queued, \
                 crashed, dead-lettered, and scheduled run. Set DeploymentConfig::storage_dir for the \
                 durable on-disk queue (<dir>/dispatch.db), or DeploymentConfig::dispatch_backend=Postgres \
                 with DeploymentConfig::database_url for the shared queue. Refusing to serve a 'durable' \
                 ingress on a volatile queue.",
            );
        }
        None
    }
}

/// One parser for the positive deployment axis and its legacy negated alias.
/// An explicit new value wins, matching the CLI validation layer.
#[cfg(test)]
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
#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DeploymentConfig {
        DeploymentConfig {
            dispatch_owner: "host-1".into(),
            ..DeploymentConfig::ephemeral()
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

    /// Sandbox-network evidence graph:
    ///
    /// C1 adapter structurally severs networking -> E1 `network_isolation`.
    /// C2 adapter structurally enforces host allowlists -> E2
    /// `enforced_network_allowlist`. Isolation class alone implies neither.
    ///
    /// | Tier | C1 | C2 | Advertised support |
    /// |---|---:|---:|---|
    /// | local | 0 | 0 | neither |
    /// | namespace | 1 | 0 | deny-all only |
    /// | docker/podman | 1 | 0 | deny-all only |
    /// | k8s (current adapter) | 0 | 0 | neither |
    #[test]
    fn sandbox_support_reports_adapter_evidence_not_isolation_class() {
        for (tier, deny_all, package_provisioning, backend) in [
            (SandboxTier::Local, false, false, "local"),
            (SandboxTier::Namespace, true, false, "namespace"),
            (SandboxTier::Docker, true, false, "docker"),
            (SandboxTier::Podman, true, true, "podman"),
            (SandboxTier::K8s, false, false, "k8s"),
        ] {
            let mut deployment = base();
            deployment.sandbox_tier = tier;
            let (support, actual_backend) = deployment.sandbox_support();
            assert_eq!(actual_backend, backend);
            assert_eq!(support.network_isolation, deny_all, "{tier:?}");
            assert_eq!(
                support.package_provisioning, package_provisioning,
                "{tier:?} package build evidence"
            );
            assert!(
                !support.enforced_network_allowlist,
                "{tier:?} must not claim a no-bypass allowlist"
            );
        }
    }

    #[test]
    fn acp_worker_profile_has_exact_routes_and_an_unambiguous_default() {
        let profile = AcpWorkerProfile::new(
            ["claude".to_string(), "codex".to_string()],
            Some("codex".to_string()),
        )
        .unwrap();
        assert_eq!(
            profile.cli_ids().collect::<Vec<_>>(),
            vec!["claude", "codex"]
        );
        assert_eq!(profile.default_cli(), Some("codex"));

        let one = AcpWorkerProfile::new(["claude".to_string()], None).unwrap();
        assert_eq!(one.default_cli(), Some("claude"));
        assert!(
            AcpWorkerProfile::new(
                ["claude".to_string(), "claude".to_string()],
                Some("claude".to_string())
            )
            .is_err()
        );
        assert!(
            AcpWorkerProfile::new(
                ["claude".to_string(), "codex".to_string()],
                Some("gemini".to_string())
            )
            .is_err()
        );
    }

    #[test]
    fn acp_worker_profile_accepts_only_resolved_argv_for_its_own_routes() {
        // Cause graph: advertised route + successful startup acquisition -> one
        // immutable launch override. Unadvertised or empty evidence is rejected
        // before the Worker can advertise a route it cannot launch.
        //
        // Decision table:
        // A1 advertised + absolute argv -> stored for that route
        // A2 unadvertised route         -> reject
        // A3 empty/blank executable     -> reject
        let mut profile =
            AcpWorkerProfile::new(["codex".to_string()], Some("codex".to_string())).unwrap();
        profile
            .set_launch_argv("codex", vec!["/opt/awaken/codex-acp".into()])
            .expect("A1");
        assert_eq!(
            profile.launch_argv("codex"),
            Some(&["/opt/awaken/codex-acp".to_string()][..]),
            "A1"
        );
        assert!(
            profile
                .set_launch_argv("claude", vec!["/opt/awaken/claude-agent-acp".into()])
                .is_err(),
            "A2"
        );
        assert!(
            profile
                .set_launch_argv("codex", vec!["   ".into()])
                .is_err(),
            "A3"
        );
    }

    #[test]
    fn discovery_projects_one_launch_profile_without_persisting_an_inventory() {
        use awaken_run_executor_acp::{AcpDetectionState, AcpHostObservation};

        // Cause graph: supported catalog rows + host detection -> exact launch
        // routes; login state remains separate Worker credential evidence.
        //
        // Decision table:
        // D1 none detected       -> no profile
        // D2 one detected        -> profile + inferred default
        // D3 several detected    -> profile + no random default
        // D4 requested undetected default -> reject
        let observation = |id: &str, detection| AcpHostObservation {
            cli_id: id.to_string(),
            display_name: id.to_string(),
            detection,
            version: Some("1".to_string()),
            credential_state: Some(
                awaken_runtime_contract::CredentialObservationState::LoginRequired,
            ),
            reason_code: Some("fixture".to_string()),
        };
        let missing = observation("codex", AcpDetectionState::Missing);
        assert_eq!(
            AcpWorkerProfile::from_discovery(std::slice::from_ref(&missing), None).unwrap(),
            None,
            "D1"
        );

        let codex = observation("codex", AcpDetectionState::Detected);
        let one = AcpWorkerProfile::from_discovery(std::slice::from_ref(&codex), None)
            .unwrap()
            .unwrap();
        assert_eq!(one.cli_ids().collect::<Vec<_>>(), ["codex"], "D2");
        assert_eq!(one.default_cli(), Some("codex"), "D2");

        let claude = observation("claude", AcpDetectionState::Detected);
        let several = AcpWorkerProfile::from_discovery(&[codex, claude], None)
            .unwrap()
            .unwrap();
        assert_eq!(several.default_cli(), None, "D3");
        assert!(
            AcpWorkerProfile::from_discovery(
                &[observation("claude", AcpDetectionState::Detected), missing],
                Some("codex".to_string())
            )
            .is_err(),
            "D4"
        );
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
        assert!(err.contains("DeploymentConfig::storage_dir"), "{err}");
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
