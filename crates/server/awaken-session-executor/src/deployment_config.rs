// @formal entrypoint run_ingress.durable_dispatch_deployment_admission_is_exact_store_independent_and_evidence_monotonic
// @formal entrypoint sandbox.deployment_support_projection_is_total_exact_and_evidence_bound
// @formal entrypoint worker.sandbox_tool_recovery_is_a_hard_claim_axis
// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
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

use awaken_acp_contract::AcpAdapterPolicy;
use awaken_runtime_contract::resolved::ProviderServerTool;
use awaken_session_mcp::ContainerMcpRelayExposure;
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
    pub fn all_known(policy: &dyn AcpAdapterPolicy<ProviderServerTool>) -> Result<Self, String> {
        Self::new(policy.known_ids().into_iter().map(str::to_string), policy)
    }

    pub fn new(
        cli_ids: impl IntoIterator<Item = String>,
        policy: &dyn AcpAdapterPolicy<ProviderServerTool>,
    ) -> Result<Self, String> {
        let mut normalized = BTreeSet::new();
        for cli_id in cli_ids {
            if cli_id.is_empty() || cli_id.trim() != cli_id {
                return Err(
                    "ACP CLI ids must be non-empty and contain no surrounding whitespace"
                        .to_string(),
                );
            }
            if !policy.contains(&cli_id) {
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

// Lifetime of the Session container's Hand role; the Session environment
// contract owns the value so the environment and this Deployment agree.
use awaken_session_environment_contract::ContainerHandResidency;

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
    /// Allow exact `trusted_host` Environment policies on this installation.
    /// Workers advertise the matching backend only when this operator-owned
    /// switch is enabled; hosted installations therefore fail closed by default.
    pub trusted_host_enabled: bool,
    /// Opt into the installed Linux process boundary for TrustedHost execution.
    /// Startup must probe and install it; this is not a capability attestation.
    pub trusted_host_scoped_execution: bool,
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
    /// Externally reachable origin for the Runtime Host's exact-generation MCP
    /// relay. This is transport configuration only: it neither installs nor
    /// proves secret substitution or no-bypass provider capabilities.
    pub container_mcp_relay: Option<ContainerMcpRelayExposure>,
    /// Kubernetes namespace used by the K8s container adapter.
    pub k8s_namespace: String,
    /// Bound on waiting for the scheduler to place a Sandbox Pod, measured
    /// from Pod creation; past it the claim returns as no capacity (ADR-0092).
    pub k8s_unschedulable_wait_secs: u64,
    /// Existing namespace-local Secrets used by kubelet for private image pulls.
    pub k8s_image_pull_secrets: Vec<String>,
    /// Optional retained active-filesystem PVC policy. Association is derived
    /// from the existing Session Sandbox realization; this is provisioning
    /// policy, not a second lifecycle registry.
    pub k8s_continuation_volume: Option<awaken_container_contract::K8sContinuationVolume>,
    /// Optional native node-image prewarming, independent of container pooling.
    pub k8s_image_prewarm: Option<awaken_container_contract::K8sImagePrewarm>,
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
    /// Explicit operator opt-in for unrestricted Docker package builds to use
    /// the builder host network. Never changes Session networking or overrides
    /// None/Allowlist package policies. Defaults to the ordinary Docker network.
    pub package_build_host_network: bool,
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
            trusted_host_enabled: false,
            trusted_host_scoped_execution: false,
            allow_local_fallback: false,
            warm_pool_size: 0,
            warm_pool_total_size: 16,
            warm_pool_idle_ttl_secs: 300,
            container_forward_proxy: None,
            container_mcp_relay: None,
            k8s_namespace: "default".to_owned(),
            k8s_unschedulable_wait_secs: K8S_UNSCHEDULABLE_WAIT_DEFAULT_SECS,
            k8s_image_pull_secrets: Vec::new(),
            k8s_continuation_volume: None,
            k8s_image_prewarm: None,
            container_hand_bin: "/usr/local/bin/awaken-sandbox".to_owned(),
            container_hand_residency: ContainerHandResidency::AttachedExec,
            container_hand_idle_secs: 300,
            podman_bin: "podman".to_owned(),
            package_image_registry: None,
            package_registry_auth_file: None,
            package_registry_insecure: false,
            package_build_host_network: false,
            k8s_buildkit_image: "moby/buildkit:v0.30.0-rootless".to_owned(),
            package_image_builder: None,
            package_local_cache_ttl_secs: 7 * 24 * 60 * 60,
            inherit_agent_stderr: false,
        }
    }
}

const K8S_UNSCHEDULABLE_WAIT_DEFAULT_SECS: u64 =
    awaken_container_contract::DEFAULT_K8S_UNSCHEDULABLE_WAIT.as_secs();
const K8S_UNSCHEDULABLE_WAIT_MAX_SECS: u64 = 24 * 60 * 60;

impl SandboxSettings {
    /// Apply an authored `k8s_unschedulable_wait_secs`: Kubernetes only, at
    /// most one day; `0` fails on the first unschedulable observation.
    pub fn apply_k8s_unschedulable_wait_secs(
        &mut self,
        tier: SandboxTier,
        secs: Option<u64>,
    ) -> Result<(), String> {
        let Some(secs) = secs else { return Ok(()) };
        if tier != SandboxTier::K8s {
            return Err("k8s_unschedulable_wait_secs requires sandbox_tier=k8s".into());
        }
        if secs > K8S_UNSCHEDULABLE_WAIT_MAX_SECS {
            return Err(format!(
                "k8s_unschedulable_wait_secs must be at most {K8S_UNSCHEDULABLE_WAIT_MAX_SECS}"
            ));
        }
        self.k8s_unschedulable_wait_secs = secs;
        Ok(())
    }
}

/// File-adapter input for package-image and private-registry sandbox policy.
/// CLI and standalone Worker keep separate authority schemas, then lower this
/// shared subset exactly once into [`SandboxSettings`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxPackageSettingsInput {
    pub k8s_image_pull_secrets: Vec<String>,
    pub package_image_registry: Option<String>,
    pub package_registry_auth_file: Option<PathBuf>,
    pub package_registry_insecure: bool,
    pub package_build_host_network: bool,
    pub k8s_buildkit_image: Option<String>,
    pub package_image_builder: Option<String>,
    pub package_local_cache_ttl_secs: Option<u64>,
    pub inherit_agent_stderr: bool,
}

impl SandboxPackageSettingsInput {
    pub fn apply(self, tier: SandboxTier, settings: &mut SandboxSettings) -> Result<(), String> {
        settings.k8s_image_pull_secrets = self
            .k8s_image_pull_secrets
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect();
        settings.package_image_registry = self
            .package_image_registry
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.trim_end_matches('/').to_owned());
        settings.package_registry_auth_file = self.package_registry_auth_file;
        settings.package_registry_insecure = self.package_registry_insecure;
        settings.package_build_host_network = self.package_build_host_network;
        if let Some(image) = self
            .k8s_buildkit_image
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            settings.k8s_buildkit_image = image.to_owned();
        }
        settings.package_image_builder = self
            .package_image_builder
            .as_deref()
            .map(|value| match value {
                "docker" => Ok(PackageImageBuilder::Docker),
                "podman" => Ok(PackageImageBuilder::Podman),
                "k8s" => Ok(PackageImageBuilder::Kubernetes),
                other => Err(format!(
                    "invalid package_image_builder={other:?}: expected docker, podman or k8s"
                )),
            })
            .transpose()?;
        settings.package_local_cache_ttl_secs = self
            .package_local_cache_ttl_secs
            .unwrap_or(settings.package_local_cache_ttl_secs);
        settings.inherit_agent_stderr = self.inherit_agent_stderr;

        if settings.package_image_builder.is_some() && settings.package_image_registry.is_none() {
            return Err("package_image_builder requires a non-empty package_image_registry".into());
        }
        if settings.package_registry_auth_file.is_some()
            && settings.package_image_registry.is_none()
        {
            return Err(
                "package_registry_auth_file requires a non-empty package_image_registry".into(),
            );
        }
        if settings.package_registry_insecure && settings.package_image_registry.is_none() {
            return Err(
                "package_registry_insecure requires a non-empty package_image_registry".into(),
            );
        }
        if tier == SandboxTier::K8s
            && (settings.package_image_builder.is_some()
                ^ settings.package_image_registry.is_some())
        {
            return Err(
                "Kubernetes package provisioning requires package_image_builder and package_image_registry together"
                    .into(),
            );
        }
        if settings.package_local_cache_ttl_secs == 0 {
            return Err("package image local cache TTL must be non-zero".into());
        }
        Ok(())
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
    /// The cross-node wake selected by this typed deployment.
    pub wake: Wake,
    /// The selected wake channel or subject.
    pub wake_channel: String,
    /// The NATS broker URL for `Wake::Nats`.
    pub nats_url: Option<String>,
    /// The shared database url for Postgres backends (`DeploymentConfig::database_url`).
    pub database_url: Option<String>,
    /// Exact maximum size of each runtime Postgres pool. Resolved once by the
    /// process startup and injected into commit/dispatch stores.
    pub postgres_max_connections: NonZeroU32,
    /// The dispatch lease owner (`AWAKEN_DISPATCH_OWNER`), distinct per process/node.
    pub dispatch_owner: String,
    /// Maximum fallback wait before a pool checks durable dispatch again after a
    /// lost wake. The cross-node wake is only a hint; this is the recovery bound.
    pub dispatch_poll_interval: std::time::Duration,
    /// Hosted composition may provide the sole queue-depth observation used by
    /// its scaler; standalone deployments retain the built-in sampler.
    pub sample_dispatch_queue_depth: bool,
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

/// The base dir the Session sandbox roots and per-thread config homes live
/// under, or a per-process temp dir when unset.
pub fn session_sandbox_base(deployment: &DeploymentConfig) -> std::path::PathBuf {
    if let Some(sandbox) = &deployment.sandbox_dir {
        return sandbox.clone();
    }
    if let Some(storage) = &deployment.storage_dir {
        return storage.join("sandboxes");
    }
    // One process-temporary root per process start: a pid alone is reused by
    // later processes, and a reused root still holds the realization markers
    // they left, so a fixed Session id could meet another process's sandbox.
    static BASE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let nonce = || uuid::Uuid::new_v4().simple().to_string();
    let leaf = || format!("awaken-acp-sbx-{}-{}", std::process::id(), nonce());
    BASE.get_or_init(|| std::env::temp_dir().join(leaf()))
        .clone()
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
    /// Project the Run-execution axes into the typed topology a Host is
    /// constructed with (ADR-0091 D6). This is the composition-side reading of
    /// the file: the Host itself never sees `durable`, `disable_local_pool`,
    /// `dispatch_owner`, the poll bound or the depth flag.
    pub fn execution_topology(&self) -> Result<crate::ExecutionTopology, String> {
        let topology = crate::ExecutionTopology {
            ingress: if self.durable {
                crate::RunIngress::Durable
            } else {
                crate::RunIngress::Direct
            },
            local: if self.disable_local_pool {
                crate::LocalExecution::RemoteWorkersOnly
            } else {
                crate::LocalExecution::in_process_pool()
            },
            lease_owner: crate::LeaseOwnerId::new(self.dispatch_owner.clone())?,
            lost_wake_recovery_bound: self.dispatch_poll_interval,
            queue_depth: if self.sample_dispatch_queue_depth {
                crate::QueueDepthSource::BuiltInSampler
            } else {
                crate::QueueDepthSource::External
            },
        };
        topology.validate()?;
        Ok(topology)
    }

    /// The process a Host is constructed in, projected from this file by the
    /// composition root. Consumer: every `SessionExecutorBuilder` constructor.
    pub fn session_execution_process(
        &self,
        local_workspace: String,
        clock: std::sync::Arc<dyn awaken_time::WallClock>,
    ) -> Result<crate::SessionExecutionProcess, String> {
        Ok(crate::SessionExecutionProcess {
            local_workspace,
            execution: self.execution_topology()?,
            content_capture: self.content_capture,
            storage_dir: self.storage_dir.clone(),
            sandbox_root: session_sandbox_base(self),
            trusted_host: if self.trusted_host_enabled() {
                crate::TrustedHostPolicy::Admitted
            } else {
                crate::TrustedHostPolicy::Refused
            },
            warm_pool: crate::WarmPool::new(
                self.sandbox.warm_pool_size,
                self.sandbox.warm_pool_total_size,
            ),
            mcp_relay: self.sandbox.container_mcp_relay.clone(),
            acp_profile: self.acp.clone(),
            clock,
        })
    }

    /// One configuration admission rule, reused by Worker and direct Host
    /// construction. The capability projection remains independently qualified.
    pub fn validate_local_execution(&self) -> Result<(), String> {
        self.execution_topology()?;
        if self.sandbox.trusted_host_scoped_execution
            && (!cfg!(target_os = "linux") || !self.trusted_host_enabled())
        {
            return Err(
                "TrustedHost scoped execution requires Linux and an enabled TrustedHost backend"
                    .into(),
            );
        }
        Ok(())
    }

    /// Whether this installation may realize the exact host-identity boundary.
    /// Selecting the Local tier is itself an explicit operator choice; other
    /// tiers require the independent opt-in so a cloud Worker cannot inherit
    /// ambient host access from an unrelated fallback setting.
    #[must_use]
    pub fn trusted_host_enabled(&self) -> bool {
        self.sandbox_tier == SandboxTier::Local || self.sandbox.trusted_host_enabled
    }

    /// Exact additional backend projection from the same provider installed by
    /// Host. Never derive it from the primary Namespace/Container capability.
    #[must_use]
    pub fn trusted_host_support(
        &self,
    ) -> Option<awaken_provisioning_contract::SandboxCapabilities> {
        self.trusted_host_enabled()
            .then(awaken_session_environment_contract::workdir_capabilities)
    }

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
        self.sandbox_support_for_tier(self.sandbox_tier)
    }

    /// Project an already-resolved local tier through the same capability
    /// mapping as the configured tier. The Worker probes an explicitly allowed
    /// Namespace fallback before registration; changing the Deployment's
    /// configured tier would also grant TrustedHost access, which fallback
    /// alone does not authorize.
    #[must_use]
    pub fn sandbox_support_for_tier(
        &self,
        tier: SandboxTier,
    ) -> (
        awaken_provisioning_contract::SandboxCapabilities,
        &'static str,
    ) {
        use awaken_provisioning_contract::{IsolationClass, SandboxCapabilities};

        let projection = sandbox_support_projection(
            tier,
            self.sandbox.package_image_registry.is_some(),
            self.sandbox.package_image_builder.is_some(),
        );
        let capabilities = match tier {
            SandboxTier::Local => awaken_session_environment_contract::workdir_capabilities(),
            SandboxTier::Docker | SandboxTier::Podman | SandboxTier::K8s => {
                SandboxCapabilities {
                    process_tree_supervision: false,
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
            SandboxTier::Namespace => awaken_session_environment_contract::namespace_capabilities(),
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
            dispatch_poll_interval: awaken_dispatch_execution::DispatchServiceConfig::default()
                .poll_interval,
            sample_dispatch_queue_depth: true,
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
    use awaken_tool_contract::tool::ToolRecoveryCapability;

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

    /// @obligation worker.sandbox_tool_recovery_is_a_hard_claim_axis
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

    /// @obligation run_ingress.durable_dispatch_deployment_admission_is_exact_store_independent_and_evidence_monotonic
    /// @proof-input-debt free_symbol durable_dispatch_is_admitted has_storage_dir
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

    /// @obligation run_ingress.durable_dispatch_deployment_admission_is_exact_store_independent_and_evidence_monotonic
    /// @proof-input-debt free_symbol durable_dispatch_is_admitted has_storage_dir
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

    /// @obligation sandbox.deployment_support_projection_is_total_exact_and_evidence_bound
    /// @proof-input-debt free_symbol sandbox_support_projection has_package_builder
    /// @proof-input-debt free_symbol sandbox_support_projection has_package_registry
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
    #[test]
    fn k8s_unschedulable_wait_defaults_and_accepts_only_bounded_kubernetes_values() {
        // Decision table (ADR-0092 decision 2 setting). Causes: C1 authored
        // value absent / 0 / in range / the one-day maximum / above it; C2 the
        // tier is Kubernetes or not. Effects: E1 the 300 s default stays; E2 the
        // value is applied; E3 rejected with the setting named.
        // | Rule | C1 | C2 | effect |
        // | U1 | absent | any | E1 |
        // | U2 | 0 / 45 / 86400 | k8s | E2 (0 fails on the first refusal) |
        // | U3 | 86401 | k8s | E3 |
        // | U4 | any value | not k8s | E3 |
        use super::{SandboxSettings, SandboxTier};
        assert_eq!(
            SandboxSettings::default().k8s_unschedulable_wait_secs,
            300,
            "U1"
        );
        for tier in [SandboxTier::K8s, SandboxTier::Docker] {
            let mut settings = SandboxSettings::default();
            settings
                .apply_k8s_unschedulable_wait_secs(tier, None)
                .unwrap();
            assert_eq!(settings.k8s_unschedulable_wait_secs, 300, "U1 {tier:?}");
        }
        for secs in [0, 45, 86_400] {
            let mut settings = SandboxSettings::default();
            settings
                .apply_k8s_unschedulable_wait_secs(SandboxTier::K8s, Some(secs))
                .unwrap();
            assert_eq!(settings.k8s_unschedulable_wait_secs, secs, "U2");
        }
        let mut settings = SandboxSettings::default();
        let too_long = settings
            .apply_k8s_unschedulable_wait_secs(SandboxTier::K8s, Some(86_401))
            .unwrap_err();
        assert!(too_long.contains("k8s_unschedulable_wait_secs"), "U3");
        for tier in [
            SandboxTier::Local,
            SandboxTier::Namespace,
            SandboxTier::Docker,
            SandboxTier::Podman,
        ] {
            let error = settings
                .apply_k8s_unschedulable_wait_secs(tier, Some(60))
                .unwrap_err();
            assert!(error.contains("sandbox_tier=k8s"), "U4 {tier:?}");
        }
        assert_eq!(
            settings.k8s_unschedulable_wait_secs, 300,
            "rejection mutates nothing"
        );
    }

    use super::*;

    struct TestAcpAdapterPolicy;

    impl AcpAdapterPolicy<ProviderServerTool> for TestAcpAdapterPolicy {
        fn lookup_id(&self, id: &str) -> Option<&'static str> {
            match id {
                "claude" => Some("claude"),
                "codex" => Some("codex"),
                "codex-alias" => Some("codex"),
                _ => None,
            }
        }

        fn known_ids(&self) -> Vec<&'static str> {
            vec!["claude", "codex"]
        }

        fn admits_mcp_client_credential(
            &self,
            _id: &str,
            _delivery: Option<awaken_credential_contract::McpCredentialDelivery>,
            _http_transport: bool,
        ) -> bool {
            false
        }

        fn realizes_provider_server_tool(&self, _id: &str, _tool: &ProviderServerTool) -> bool {
            false
        }
    }

    static TEST_ACP_POLICY: TestAcpAdapterPolicy = TestAcpAdapterPolicy;

    fn base() -> DeploymentConfig {
        DeploymentConfig {
            dispatch_owner: "host-1".into(),
            ..DeploymentConfig::ephemeral()
        }
    }

    #[test]
    fn execution_axes_project_into_one_validated_topology() {
        // Causes: C1 durable; C2 disable_local_pool; C3 dispatch_owner blank;
        // C4 poll interval outside 1ms..=60s; C5 sample_dispatch_queue_depth.
        // Effects: E1 ingress Durable iff C1; E2 RemoteWorkersOnly iff C2, else
        // a CPU-sized in-process pool; E3 C3 or C4 refused before a Host
        // exists; E4 External depth observer iff !C5; E5 owner and bound kept
        // verbatim. The Host reads only this projection.
        let mut config = base();
        config.dispatch_poll_interval = std::time::Duration::from_millis(750);
        let direct = config.execution_topology().expect("valid");
        assert_eq!(direct.ingress, crate::RunIngress::Direct, "E1");
        assert_eq!(direct.local, crate::LocalExecution::in_process_pool(), "E2");
        assert_eq!(direct.lease_owner.as_str(), "host-1", "E5");
        assert_eq!(
            direct.lost_wake_recovery_bound,
            std::time::Duration::from_millis(750),
            "E5"
        );
        assert_eq!(
            direct.queue_depth,
            crate::QueueDepthSource::BuiltInSampler,
            "E4"
        );

        config.durable = true;
        config.disable_local_pool = true;
        config.sample_dispatch_queue_depth = false;
        let coordinator = config.execution_topology().expect("valid");
        assert_eq!(coordinator.ingress, crate::RunIngress::Durable, "C1/E1");
        assert_eq!(
            coordinator.local,
            crate::LocalExecution::RemoteWorkersOnly,
            "C2/E2"
        );
        assert_eq!(
            coordinator.queue_depth,
            crate::QueueDepthSource::External,
            "C5/E4"
        );

        for invalid in [0, 60_001] {
            config.dispatch_poll_interval = std::time::Duration::from_millis(invalid);
            assert!(
                config
                    .validate_local_execution()
                    .unwrap_err()
                    .contains("dispatch_poll_interval_ms"),
                "C4/E3"
            );
        }
        config.dispatch_poll_interval = std::time::Duration::from_millis(750);
        config.dispatch_owner = " ".into();
        assert!(
            config
                .execution_topology()
                .unwrap_err()
                .contains("dispatch_owner"),
            "C3/E3"
        );
    }

    #[test]
    fn package_sandbox_settings_have_one_cause_effect_table() {
        // Causes: P1 registry+builder are both present; P2 either one is absent
        // on K8s; P3 auth/insecure lacks a registry; P4 builder token is unknown;
        // P5 TTL is zero; P6 pull Secret/registry/build image have surrounding
        // whitespace. Effects: E1 P1 normalizes and admits; E2 P2-P5 reject;
        // E3 P6 preserves only non-empty normalized coordinates. CLI and the
        // standalone Worker call this same owner, so these rules cannot drift.
        let apply = |input: SandboxPackageSettingsInput, tier| {
            let mut settings = SandboxSettings::default();
            let result = input.apply(tier, &mut settings);
            (result, settings)
        };
        let (result, settings) = apply(
            SandboxPackageSettingsInput {
                k8s_image_pull_secrets: vec![" registry-auth ".into(), " ".into()],
                package_image_registry: Some(" registry.example/team/ ".into()),
                k8s_buildkit_image: Some(" mirror/buildkit:1 ".into()),
                package_image_builder: Some("k8s".into()),
                inherit_agent_stderr: true,
                ..Default::default()
            },
            SandboxTier::K8s,
        );
        assert!(result.is_ok(), "P1/P6: {result:?}");
        assert_eq!(settings.k8s_image_pull_secrets, ["registry-auth"]);
        assert_eq!(
            settings.package_image_registry.as_deref(),
            Some("registry.example/team")
        );
        assert_eq!(settings.k8s_buildkit_image, "mirror/buildkit:1");
        assert_eq!(
            settings.package_image_builder,
            Some(PackageImageBuilder::Kubernetes)
        );
        assert!(settings.inherit_agent_stderr);

        for input in [
            SandboxPackageSettingsInput {
                package_image_registry: Some("registry.example/team".into()),
                ..Default::default()
            },
            SandboxPackageSettingsInput {
                package_image_builder: Some("k8s".into()),
                ..Default::default()
            },
            SandboxPackageSettingsInput {
                package_registry_auth_file: Some("/secret/config.json".into()),
                ..Default::default()
            },
            SandboxPackageSettingsInput {
                package_registry_insecure: true,
                ..Default::default()
            },
            SandboxPackageSettingsInput {
                package_image_builder: Some("unknown".into()),
                package_image_registry: Some("registry.example/team".into()),
                ..Default::default()
            },
            SandboxPackageSettingsInput {
                package_local_cache_ttl_secs: Some(0),
                ..Default::default()
            },
        ] {
            assert!(apply(input, SandboxTier::K8s).0.is_err(), "P2-P5");
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
        // Causes: C1 the injected catalog recognizes an exact CLI id; C2 the
        // configured list is nonempty, normalized and duplicate-free. Effects:
        // E1 C1+C2 builds only those routes; E2 any failed cause rejects before
        // advertisement. R1 exact known set -> profile; R2 unknown/alias,
        // empty/blank or duplicate -> error. `all_known` uses the policy list.
        let profile = AcpWorkerProfile::new(
            ["claude".to_string(), "codex".to_string()],
            &TEST_ACP_POLICY,
        )
        .unwrap();
        assert_eq!(
            profile.cli_ids().collect::<Vec<_>>(),
            vec!["claude", "codex"]
        );
        assert!(
            AcpWorkerProfile::new(
                ["claude".to_string(), "claude".to_string()],
                &TEST_ACP_POLICY
            )
            .is_err()
        );
        assert!(
            AcpWorkerProfile::new(["unknown-third-party-cli".to_string()], &TEST_ACP_POLICY)
                .is_err()
        );
        assert!(AcpWorkerProfile::new(["codex-alias".to_string()], &TEST_ACP_POLICY).is_err());
        assert!(
            AcpWorkerProfile::new(["".to_string(), "codex".to_string()], &TEST_ACP_POLICY).is_err()
        );
        assert!(AcpWorkerProfile::new([" codex".to_string()], &TEST_ACP_POLICY).is_err());
        assert!(AcpWorkerProfile::new(["codex ".to_string()], &TEST_ACP_POLICY).is_err());

        // The production policy is tested against the sole catalog in
        // execution-composition; this test proves Host uses its injected list.
        let all = AcpWorkerProfile::all_known(&TEST_ACP_POLICY).unwrap();
        assert_eq!(
            all.cli_ids().collect::<std::collections::BTreeSet<_>>(),
            TEST_ACP_POLICY
                .known_ids()
                .into_iter()
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
        let mut profile = AcpWorkerProfile::new(["codex".to_string()], &TEST_ACP_POLICY).unwrap();
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
    fn local_execution_selection_has_one_platform_and_authorization_table() {
        // E1 disabled -> existing behavior on every platform/tier. E2 enabled
        // -> Linux AND (Local tier OR explicit TrustedHost opt-in). E3 selection
        // never manufactures full process_tree_supervision capability evidence.
        // Pure configuration has no timing state; launch/exit concurrency is
        // owned by LocalProcessLifecycle and SessionExecutionBinding.
        for enabled in [false, true] {
            for trusted in [false, true] {
                for tier in [
                    SandboxTier::Local,
                    SandboxTier::Namespace,
                    SandboxTier::Docker,
                ] {
                    let mut config = base();
                    config.sandbox_tier = tier;
                    config.sandbox.trusted_host_enabled = trusted;
                    config.sandbox.trusted_host_scoped_execution = enabled;
                    let allowed = !enabled
                        || (cfg!(target_os = "linux") && (tier == SandboxTier::Local || trusted));
                    assert_eq!(config.validate_local_execution().is_ok(), allowed);
                    assert!(!config.sandbox_support().0.process_tree_supervision);
                    assert!(
                        config
                            .trusted_host_support()
                            .is_none_or(|caps| !caps.process_tree_supervision)
                    );
                }
            }
        }
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
