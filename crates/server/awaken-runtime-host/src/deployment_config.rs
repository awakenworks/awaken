//! The deployment configuration surface, built once by a typed process startup.
//!
//! Historically the deployment axes — durable ingress, the commit/dispatch store
//! backends, the cross-node wake, the worker role — were read via scattered
//! `std::env::var` calls deep inside the runtime library. That is a hidden global
//! dependency: the library reaches into process env, which cannot be unit-tested
//! without mutating it and gives no single place to read a deployment's shape.
//!
//! [`DeploymentConfig`] is that single typed surface. The process startup builds
//! one from a typed configuration file (or explicitly for an embedding), and the
//! library reads it rather than process-global deployment configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::path::PathBuf;

/// The ACP adapters one Worker can actually launch.
///
/// This value is shared by Worker capability advertisement and Host launch
/// routing. It therefore prevents an advertised `acp:<cli>` capability from
/// drifting from the launch routes installed on that same Worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpWorkerProfile {
    cli_ids: BTreeSet<String>,
    launch_argv: BTreeMap<String, Vec<String>>,
}

impl AcpWorkerProfile {
    /// Select every adapter from the canonical executable catalog.
    ///
    /// Production images built with the catalog's `all` contract use this
    /// projection instead of making each embedding maintain a second id list.
    pub fn all_known() -> Result<Self, String> {
        Self::new(
            awaken_run_executor_acp::known_acp_clis()
                .iter()
                .map(|cli| cli.id.to_string()),
        )
    }

    pub fn new(cli_ids: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut normalized = BTreeSet::new();
        for cli_id in cli_ids {
            if cli_id.is_empty() || cli_id.trim() != cli_id {
                return Err(
                    "ACP CLI ids must be non-empty and contain no surrounding whitespace"
                        .to_string(),
                );
            }
            if awaken_run_executor_acp::acp_cli(&cli_id).is_none() {
                return Err(format!("unknown ACP CLI `{cli_id}`"));
            }
            if !normalized.insert(cli_id.clone()) {
                return Err(format!("duplicate ACP CLI `{cli_id}`"));
            }
        }
        if normalized.is_empty() {
            return Err("an ACP Worker profile requires at least one CLI".to_string());
        }
        Ok(Self {
            cli_ids: normalized,
            launch_argv: BTreeMap::new(),
        })
    }

    pub fn cli_ids(&self) -> impl Iterator<Item = &str> {
        self.cli_ids.iter().map(String::as_str)
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

    /// Atomically install the startup acquisition plan produced by the ACP
    /// application service. Process starters share this projection instead of
    /// maintaining separate per-CLI loops.
    pub fn apply_launch_argv(
        &mut self,
        launch_argv: &BTreeMap<String, Vec<String>>,
    ) -> Result<(), String> {
        let mut projected = self.clone();
        for (cli_id, argv) in launch_argv {
            projected.set_launch_argv(cli_id, argv.clone())?;
        }
        self.launch_argv = projected.launch_argv;
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
/// configured differently through typed deployment config, so one fleet mixes
/// backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxTier {
    /// No OS isolation — the ACP CLI runs as a plain child of the runtime process
    /// (`sandbox_tier = "local"`). The environment-agnostic executor drives it
    /// over the same agent-channel source as any sandboxed tier; only the host's
    /// choice of source differs (ADR-0057: the executor never learns the tier). For
    /// a trusted CLI or single-tenant dev where isolation is provided elsewhere.
    Local,
    /// Bubblewrap namespace isolation on the worker host (`sandbox_tier = "namespace"`,
    /// the default) — no user image, the agent runs under `bwrap`.
    #[default]
    Namespace,
    /// A Docker container from the configured image (`sandbox_tier = "docker"`).
    Docker,
    /// A rootless Podman container (`sandbox_tier = "podman"`).
    Podman,
    /// A Kubernetes Pod (`sandbox_tier = "k8s"`), for a multi-node cloud fleet.
    K8s,
}

/// Runtime used only to build and publish immutable package images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageImageBuilder {
    Docker,
    Podman,
    /// Rootless BuildKit Job running in the target Kubernetes namespace.
    Kubernetes,
}

/// Lifetime of the Session container's Hand role. One enum drives both the Pod
/// process and the Runtime Host binding, preventing parallel Hand owners.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContainerHandResidency {
    /// Launch a rebuildable `hand --stdio` process for the current Worker.
    #[default]
    AttachedExec,
    /// Run the Hand as the Session container process and reopen its private
    /// provider channel after Worker replacement.
    Resident,
}

impl ContainerHandResidency {
    /// Recovery evidence shared by Worker registration and the realized
    /// SessionEnvironment executor. Keeping this exhaustive mapping here makes
    /// a new residency mode a compile error until both paths agree.
    #[must_use]
    pub const fn recovery_capability(
        self,
    ) -> awaken_runtime_contract::tool::ToolRecoveryCapability {
        match self {
            Self::AttachedExec => {
                awaken_runtime_contract::tool::ToolRecoveryCapability::NonRecoverable
            }
            Self::Resident => awaken_runtime_contract::tool::ToolRecoveryCapability::DurableRequest,
        }
    }
}

impl std::str::FromStr for ContainerHandResidency {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "attached_exec" | "attached" => Ok(Self::AttachedExec),
            "resident" => Ok(Self::Resident),
            other => Err(format!(
                "invalid container_hand_residency={other:?}: expected attached_exec or resident"
            )),
        }
    }
}

impl SandboxTier {
    /// Whether this tier runs the agent inside a container image (vs. the local or
    /// namespace tiers on the worker host) — the process startup builds a container
    /// ACP source.
    #[must_use]
    pub fn is_container(self) -> bool {
        matches!(self, Self::Docker | Self::Podman | Self::K8s)
    }
}

impl std::str::FromStr for SandboxTier {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "local" | "none" => Ok(Self::Local),
            "namespace" => Ok(Self::Namespace),
            "docker" => Ok(Self::Docker),
            "podman" => Ok(Self::Podman),
            "k8s" | "kubernetes" => Ok(Self::K8s),
            other => Err(format!("invalid sandbox_tier={other:?}")),
        }
    }
}

/// Typed operator policy for sandbox realization.
///
/// This is part of the one [`DeploymentConfig`] aggregate. Runtime adapters
/// consume it explicitly and never rediscover these choices from process
/// environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSettings {
    /// Permit an unavailable Namespace provider to degrade to unsandboxed Workdir.
    pub allow_local_fallback: bool,
    /// Startup and steady-state target of ready, never-used containers per exact
    /// mount-less Session shape. Zero disables warm capacity.
    pub warm_pool_size: usize,
    /// Global cap across the default Session shape and all exact Environment
    /// shapes. The Worker owns one plan for this shared budget.
    pub warm_pool_total_size: usize,
    /// Opportunistic expiry for unused shapes; desired-state removal is immediate.
    pub warm_pool_idle_ttl_secs: u64,
    /// Optional HTTP(S) proxy used by the container provider.
    pub container_forward_proxy: Option<String>,
    /// Capability-authenticated proxy used only when the container runtime has
    /// independently attested a no-bypass allowlist network boundary.
    pub container_allowlist_proxy: Option<awaken_sandbox_container::AllowlistProxy>,
    /// Kubernetes namespace used by the K8s container adapter.
    pub k8s_namespace: String,
    /// Existing namespace-local Secrets used by kubelet for private image pulls.
    pub k8s_image_pull_secrets: Vec<String>,
    /// Optional retained active-filesystem PVC policy. Association is derived
    /// from the existing Session Sandbox realization; this is provisioning
    /// policy, not a second lifecycle registry.
    pub k8s_continuation_volume: Option<awaken_sandbox_container::K8sContinuationVolume>,
    /// Executable path for the Awaken Hand inside a container image.
    pub container_hand_bin: String,
    /// Whether the container Hand is Worker-attached or Environment-resident.
    pub container_hand_residency: ContainerHandResidency,
    /// Inactivity horizon after which the Worker-local Session owner releases
    /// its rebuildable Hand process/channel. Zero disables idle hibernation.
    pub container_hand_idle_secs: u64,
    /// Podman executable used by the rootless container adapter.
    pub podman_bin: String,
    /// Shared OCI repository prefix for package images. When absent, Docker and
    /// Podman use their local engine cache; Kubernetes package provisioning is
    /// unavailable because Pods cannot consume a node-local image reliably.
    pub package_image_registry: Option<String>,
    /// Docker/Podman-compatible registry authentication file read only by the
    /// Worker-side image builder. Its secret material is never projected into a
    /// Session or Agent process.
    pub package_registry_auth_file: Option<PathBuf>,
    /// Permit plain-HTTP/TLS-insecure access from the Kubernetes BuildKit Job.
    /// Intended for explicitly trusted development registries such as k3d.
    pub package_registry_insecure: bool,
    /// Rootless BuildKit image used by Kubernetes package-build Jobs. Operators
    /// may point this at an admitted private mirror so Environment startup never
    /// depends on live Docker Hub availability.
    pub k8s_buildkit_image: String,
    /// Optional builder independent from the Session execution backend.
    pub package_image_builder: Option<PackageImageBuilder>,
    /// Age after which unused, Awaken-labeled derived images may be pruned from
    /// the builder's local engine cache. Registry retention remains an operator
    /// policy because the registry is shared infrastructure.
    pub package_local_cache_ttl_secs: u64,
    /// Whether locally launched ACP agents inherit the Worker process stderr.
    pub inherit_agent_stderr: bool,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            allow_local_fallback: false,
            warm_pool_size: 0,
            warm_pool_total_size: 16,
            warm_pool_idle_ttl_secs: 300,
            container_forward_proxy: None,
            container_allowlist_proxy: None,
            k8s_namespace: "default".to_owned(),
            k8s_image_pull_secrets: Vec::new(),
            k8s_continuation_volume: None,
            container_hand_bin: "/usr/local/bin/awaken-sandbox".to_owned(),
            container_hand_residency: ContainerHandResidency::AttachedExec,
            container_hand_idle_secs: 300,
            podman_bin: "podman".to_owned(),
            package_image_registry: None,
            package_registry_auth_file: None,
            package_registry_insecure: false,
            k8s_buildkit_image: "moby/buildkit:v0.30.0-rootless".to_owned(),
            package_image_builder: None,
            package_local_cache_ttl_secs: 7 * 24 * 60 * 60,
            inherit_agent_stderr: false,
        }
    }
}

/// Redactor selected for captured model/tool content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContentRedaction {
    /// Preserve content bytes when Full capture is explicitly enabled.
    #[default]
    None,
    /// Apply the built-in conservative PII pattern redactor before persistence.
    Regex,
}

/// Typed deployment ceiling for optional content capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentCaptureSettings {
    pub level: awaken_runtime_contract::ContentCapture,
    pub redaction: ContentRedaction,
}

impl Default for ContentCaptureSettings {
    fn default() -> Self {
        Self {
            level: awaken_runtime_contract::ContentCapture::Structured,
            redaction: ContentRedaction::None,
        }
    }
}

/// The deployment axes a single binary configures from — parsed once, injected into
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
    /// Exact maximum size of each runtime Postgres pool. Resolved once by the
    /// process startup and injected into commit/dispatch stores.
    pub postgres_max_connections: NonZeroU32,
    /// The dispatch lease owner (`AWAKEN_DISPATCH_OWNER`), distinct per process/node.
    pub dispatch_owner: String,
    /// The sandbox tier this worker realizes ACP agents on.
    pub sandbox_tier: SandboxTier,
    /// The ACP sandbox and per-Session configuration root.
    /// `None` selects a process-scoped temporary root.
    pub sandbox_dir: Option<PathBuf>,
    /// Sandbox/container operator policy, resolved once by the process startup.
    pub sandbox: SandboxSettings,
    /// Deployment content-capture ceiling and redaction behavior.
    pub content_capture: ContentCaptureSettings,
    /// The durable ACP Session blob root.
    pub acp_session_blob_root: Option<PathBuf>,
    /// The exact ACP adapters this Worker advertises and serves.
    pub acp: Option<AcpWorkerProfile>,
    /// The container image an ACP agent runs in on a container tier
    /// (`container_image`); `None` on the namespace tier / when unset.
    pub container_image: Option<String>,
    /// A coordinator-only server (`DeploymentConfig::disable_local_pool=1`): own the store + HTTP
    /// but run no local pool, so remote workers are the sole drainers.
    pub disable_local_pool: bool,
}

/// The default wake channel/subject, shared by the `pg_notify` channel and the NATS
/// subject.
pub const DEFAULT_WAKE_CHANNEL: &str = "awaken_dispatch_wake";

/// Pure admission kernel for the durable dispatch axis. Commit-store choice is
/// deliberately absent: only the dispatch backend, its local storage root, or
/// an explicitly injected durable adapter can preserve queued work.
#[must_use]
const fn durable_dispatch_is_admitted(
    durable: bool,
    dispatch_backend: DispatchBackend,
    has_storage_dir: bool,
    injected: bool,
) -> bool {
    if !durable {
        return true;
    }
    matches!(dispatch_backend, DispatchBackend::Postgres) || has_storage_dir || injected
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxBackend {
    Local,
    Namespace,
    Docker,
    Podman,
    K8s,
}

impl SandboxBackend {
    const fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Namespace => "namespace",
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::K8s => "k8s",
        }
    }
}

/// The deployment-authored evidence projection that may affect advertised
/// sandbox support. This kernel intentionally knows only evidence presence;
/// adapters remain responsible for validating the external policy, registry,
/// and builder named by that evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SandboxSupportProjection {
    backend: SandboxBackend,
    network_isolation: bool,
    package_provisioning: bool,
}

#[must_use]
const fn sandbox_support_projection(
    tier: SandboxTier,
    has_package_registry: bool,
    has_package_builder: bool,
) -> SandboxSupportProjection {
    match tier {
        SandboxTier::Local => SandboxSupportProjection {
            backend: SandboxBackend::Local,
            network_isolation: false,
            package_provisioning: false,
        },
        SandboxTier::Namespace => SandboxSupportProjection {
            backend: SandboxBackend::Namespace,
            network_isolation: true,
            package_provisioning: false,
        },
        SandboxTier::Docker => SandboxSupportProjection {
            backend: SandboxBackend::Docker,
            network_isolation: true,
            package_provisioning: true,
        },
        SandboxTier::Podman => SandboxSupportProjection {
            backend: SandboxBackend::Podman,
            network_isolation: true,
            package_provisioning: true,
        },
        SandboxTier::K8s => SandboxSupportProjection {
            backend: SandboxBackend::K8s,
            // Configuration cannot prove live cluster policy. The constructed
            // provider replaces this conservative projection with attested
            // runtime evidence before Worker registration.
            network_isolation: false,
            package_provisioning: has_package_registry && has_package_builder,
        },
    }
}

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

        let projection = sandbox_support_projection(
            self.sandbox_tier,
            self.sandbox.package_image_registry.is_some(),
            self.sandbox.package_image_builder.is_some(),
        );
        let capabilities = match self.sandbox_tier {
            SandboxTier::Local => awaken_sandbox_local::LocalProvider::capabilities(),
            SandboxTier::Docker | SandboxTier::Podman | SandboxTier::K8s => {
                SandboxCapabilities {
                    isolation: IsolationClass::Container,
                    tool_transparent: true,
                    path_fidelity: true,
                    enforced_readonly: true,
                    // Docker/Podman structurally apply `network none`. Kubernetes
                    // may claim the same capability only under the exact external
                    // label-policy evidence consumed by its adapter.
                    network_isolation: projection.network_isolation,
                    enforced_network_allowlist: false,
                    secret_egress_substitution: false,
                    resource_limits: true,
                    custom_rootfs: true,
                    // Docker and Podman can build a content-addressed derived
                    // image before the Session container starts. Kubernetes may
                    // advertise this only when an external builder and shared
                    // registry are both configured.
                    package_provisioning: projection.package_provisioning,
                    control_services: Default::default(),
                }
            }
            SandboxTier::Namespace => awaken_sandbox_local::NamespaceProvider::capabilities(),
        };
        (capabilities, projection.backend.name())
    }

    /// Environment-independent defaults for embedding process startups.
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
            postgres_max_connections: default_postgres_max_connections(),
            dispatch_owner: "embedded-worker".to_string(),
            sandbox_tier: SandboxTier::Namespace,
            sandbox_dir: None,
            sandbox: SandboxSettings::default(),
            content_capture: ContentCaptureSettings::default(),
            acp_session_blob_root: None,
            acp: None,
            container_image: None,
            disable_local_pool: false,
        }
    }

    /// Whether a durable ingress is backed by a persistent queue (Postgres, an
    /// on-disk SQLite dir, or an injected backend). A durable ingress on a volatile
    /// in-memory queue silently drops queued/crashed/scheduled runs on restart, so
    /// the process startup refuses to serve one — the no-data-loss invariant.
    /// `injected` is passed in because an assembled shard fan-out lives outside this
    /// config (it owns its own durability contract).
    pub fn durable_needs_persistence_error(&self, injected: bool) -> Option<&'static str> {
        if !durable_dispatch_is_admitted(
            self.durable,
            self.dispatch_backend,
            self.storage_dir.is_some(),
            injected,
        ) {
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

/// Runtime-aware default used only at the typed deployment authoring boundary.
/// Lower Postgres adapters receive the resolved number and own no sizing policy.
#[must_use]
pub fn default_postgres_max_connections() -> NonZeroU32 {
    let value = std::thread::available_parallelism()
        .map(|parallelism| parallelism.get() as u32)
        .unwrap_or(4)
        .saturating_add(8);
    NonZeroU32::new(value).expect("parallelism plus eight is non-zero")
}

#[cfg(kani)]
mod verification {
    use super::{
        ContainerHandResidency, DispatchBackend, SandboxBackend, SandboxTier,
        durable_dispatch_is_admitted, sandbox_support_projection,
    };
    use awaken_runtime_contract::tool::ToolRecoveryCapability;

    fn symbolic_dispatch_backend(value: bool) -> DispatchBackend {
        if value {
            DispatchBackend::Postgres
        } else {
            DispatchBackend::Sqlite
        }
    }

    fn symbolic_sandbox_tier(value: u8) -> SandboxTier {
        match value % 5 {
            0 => SandboxTier::Local,
            1 => SandboxTier::Namespace,
            2 => SandboxTier::Docker,
            3 => SandboxTier::Podman,
            _ => SandboxTier::K8s,
        }
    }

    #[kani::proof]
    fn container_hand_residency_recovery_mapping_is_total_exact_and_non_widening() {
        let residency = if kani::any::<bool>() {
            ContainerHandResidency::AttachedExec
        } else {
            ContainerHandResidency::Resident
        };
        let expected = match residency {
            ContainerHandResidency::AttachedExec => ToolRecoveryCapability::NonRecoverable,
            ContainerHandResidency::Resident => ToolRecoveryCapability::DurableRequest,
        };

        assert_eq!(residency.recovery_capability(), expected);
        if residency == ContainerHandResidency::AttachedExec {
            assert_ne!(
                residency.recovery_capability(),
                ToolRecoveryCapability::DurableRequest
            );
        }
    }

    #[kani::proof]
    fn durable_dispatch_admission_is_exact_and_commit_store_independent() {
        let durable: bool = kani::any();
        let postgres_dispatch: bool = kani::any();
        let has_storage_dir: bool = kani::any();
        let injected: bool = kani::any();
        let backend = symbolic_dispatch_backend(postgres_dispatch);

        let admitted = durable_dispatch_is_admitted(durable, backend, has_storage_dir, injected);
        assert_eq!(
            admitted,
            !durable || postgres_dispatch || has_storage_dir || injected
        );

        // No commit-store value is accepted by this kernel. When all actual
        // dispatch persistence evidence is absent, durable ingress is rejected.
        if durable && !postgres_dispatch && !has_storage_dir && !injected {
            assert!(!admitted);
        }
    }

    #[kani::proof]
    fn durable_dispatch_admission_is_monotonic_in_persistence_evidence() {
        let durable: bool = kani::any();
        let postgres_dispatch: bool = kani::any();
        let has_storage_dir: bool = kani::any();
        let injected: bool = kani::any();
        let admitted = durable_dispatch_is_admitted(
            durable,
            symbolic_dispatch_backend(postgres_dispatch),
            has_storage_dir,
            injected,
        );

        if admitted {
            assert!(durable_dispatch_is_admitted(
                durable,
                DispatchBackend::Postgres,
                has_storage_dir,
                injected,
            ));
            assert!(durable_dispatch_is_admitted(
                durable,
                symbolic_dispatch_backend(postgres_dispatch),
                true,
                injected,
            ));
            assert!(durable_dispatch_is_admitted(
                durable,
                symbolic_dispatch_backend(postgres_dispatch),
                has_storage_dir,
                true,
            ));
        }
    }

    #[kani::proof]
    fn sandbox_support_projection_is_total_exact_and_evidence_bound() {
        let tier = symbolic_sandbox_tier(kani::any());
        let package_registry: bool = kani::any();
        let package_builder: bool = kani::any();
        let projection = sandbox_support_projection(tier, package_registry, package_builder);

        match tier {
            SandboxTier::Local => {
                assert_eq!(projection.backend, SandboxBackend::Local);
                assert!(!projection.network_isolation);
                assert!(!projection.package_provisioning);
            }
            SandboxTier::Namespace => {
                assert_eq!(projection.backend, SandboxBackend::Namespace);
                assert!(projection.network_isolation);
                assert!(!projection.package_provisioning);
            }
            SandboxTier::Docker => {
                assert_eq!(projection.backend, SandboxBackend::Docker);
                assert!(projection.network_isolation);
                assert!(projection.package_provisioning);
            }
            SandboxTier::Podman => {
                assert_eq!(projection.backend, SandboxBackend::Podman);
                assert!(projection.network_isolation);
                assert!(projection.package_provisioning);
            }
            SandboxTier::K8s => {
                assert_eq!(projection.backend, SandboxBackend::K8s);
                assert!(!projection.network_isolation);
                assert_eq!(
                    projection.package_provisioning,
                    package_registry && package_builder
                );
            }
        }
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
    fn sandbox_tier_default_and_container_classification_are_total() {
        /* Sandbox-tier cause/effect decision table. Causes: C1 token names an
         * unsandboxed tier, C2 names namespace isolation, C3 names a container
         * provider, C4 is an accepted compatibility alias, C5 is unknown.
         * Effects: E1 the canonical typed tier; E2 container classification;
         * E3 fail-closed parse error. Rules: T1 local/none=>Local+!E2;
         * T2 namespace=>Namespace+!E2; T3 docker/podman/k8s=>E1+E2;
         * T4 kubernetes=>K8s+E2; T5 unknown=>E3. Omission is owned by each
         * startup schema and selects the domain default Namespace. */
        assert_eq!(SandboxTier::default(), SandboxTier::Namespace);
        assert!(!SandboxTier::Namespace.is_container());
        assert!(!SandboxTier::Local.is_container());
        for t in [SandboxTier::Docker, SandboxTier::Podman, SandboxTier::K8s] {
            assert!(t.is_container());
        }
        for (token, expected) in [
            ("local", SandboxTier::Local),
            ("none", SandboxTier::Local),
            ("namespace", SandboxTier::Namespace),
            ("docker", SandboxTier::Docker),
            ("podman", SandboxTier::Podman),
            ("k8s", SandboxTier::K8s),
            ("kubernetes", SandboxTier::K8s),
        ] {
            assert_eq!(token.parse(), Ok(expected), "canonical parse for {token}");
        }
        assert_eq!(
            "vm".parse::<SandboxTier>(),
            Err("invalid sandbox_tier=\"vm\"".into()),
            "T5/E3"
        );
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
    /// | k8s, no policy evidence | 0 | 0 | neither |
    #[test]
    fn sandbox_support_reports_adapter_evidence_not_isolation_class() {
        for (tier, deny_all, package_provisioning, backend) in [
            (SandboxTier::Local, false, false, "local"),
            (SandboxTier::Namespace, true, false, "namespace"),
            (SandboxTier::Docker, true, true, "docker"),
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

        let mut k8s_with_builder = base();
        k8s_with_builder.sandbox_tier = SandboxTier::K8s;
        k8s_with_builder.sandbox.package_image_registry = Some("registry.internal/agents".into());
        k8s_with_builder.sandbox.package_image_builder = Some(PackageImageBuilder::Docker);
        assert!(
            k8s_with_builder.sandbox_support().0.package_provisioning,
            "Kubernetes may advertise packages only with an independent builder and shared registry"
        );

        for (registry, builder, expected) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let projection = sandbox_support_projection(SandboxTier::K8s, registry, builder);
            assert_eq!(
                projection.package_provisioning, expected,
                "Kubernetes requires both registry and builder evidence"
            );
        }
    }

    #[test]
    fn acp_worker_profile_has_only_exact_routes() {
        let profile = AcpWorkerProfile::new(["claude".to_string(), "codex".to_string()]).unwrap();
        assert_eq!(
            profile.cli_ids().collect::<Vec<_>>(),
            vec!["claude", "codex"]
        );
        assert!(AcpWorkerProfile::new(["claude".to_string(), "claude".to_string()]).is_err());
        assert!(AcpWorkerProfile::new(["unknown-third-party-cli".to_string()]).is_err());
        assert!(AcpWorkerProfile::new(["".to_string(), "codex".to_string()]).is_err());
        assert!(AcpWorkerProfile::new([" codex".to_string()]).is_err());
        assert!(AcpWorkerProfile::new(["codex ".to_string()]).is_err());

        // Catalog-selection cause/effect rule: the production image's `all`
        // contract selects every and only canonical row; embeddings therefore
        // cannot drift by copying ids into their own configuration table.
        let all = AcpWorkerProfile::all_known().unwrap();
        assert_eq!(
            all.cli_ids().collect::<std::collections::BTreeSet<_>>(),
            awaken_run_executor_acp::known_acp_clis()
                .iter()
                .map(|cli| cli.id)
                .collect::<std::collections::BTreeSet<_>>()
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
        // A4 mixed valid/invalid batch  -> reject atomically
        let mut profile = AcpWorkerProfile::new(["codex".to_string()]).unwrap();
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
        let before = profile.clone();
        assert!(
            profile
                .apply_launch_argv(&BTreeMap::from([
                    ("codex".into(), vec!["/new/codex-acp".into()]),
                    ("claude".into(), vec!["/new/claude-agent-acp".into()]),
                ]))
                .is_err(),
            "A4"
        );
        assert_eq!(profile, before, "A4");
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
    fn durable_dispatch_admission_matches_the_complete_boolean_decision_table() {
        for durable in [false, true] {
            for postgres in [false, true] {
                for storage_dir in [false, true] {
                    for injected in [false, true] {
                        let backend = if postgres {
                            DispatchBackend::Postgres
                        } else {
                            DispatchBackend::Sqlite
                        };
                        assert_eq!(
                            durable_dispatch_is_admitted(durable, backend, storage_dir, injected,),
                            !durable || postgres || storage_dir || injected,
                            "durable={durable} postgres={postgres} storage_dir={storage_dir} injected={injected}"
                        );
                    }
                }
            }
        }
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
