//! [`SandboxChannelSource`] — the sandboxed [`AgentChannelSource`]: each turn
//! realizes a bubblewrap (namespace-tier) sandbox scoped to the run's thread and
//! launches the ACP CLI *inside* it, so an opaque agent is OS-confined regardless
//! of what it does. The isolated production counterpart of the trusted-CLI
//! [`awaken_run_executor_acp::SubprocessChannelSource`], behind the same trait —
//! the executor is unchanged either way (ADR-0043 D6/D9: the adapter lives in the
//! host plane, which sees both the executor port and the sandbox provider).
//!
//! Network egress follows the session's environment networking policy: a thread
//! registered deny-egress launches under `bwrap --unshare-net` (no route out, not
//! even to the host loopback), the same [`ThreadEgress`] registrations that drive
//! the native path's bash-tool jail.
//!
//! [`ContainerChannelSource`] is the sibling for a **user-supplied container image**:
//! it runs the ACP CLI as a container's main process on a worker-configured
//! [`AgentContainerProvider`](awaken_sandbox_container::AgentContainerProvider)
//! (podman / docker / k8s), behind the same [`AgentChannelSource`] trait. The
//! composition root wires whichever source a given worker is configured for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::{
    AcpCli, AcpLaunch, AgentChannelSource, AgentSession, LaunchResolver, OpenError, project_launch,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_sandbox_container::AgentContainerProvider;
use awaken_sandbox_local::NamespaceProvider;

/// How a sandboxed/containerized ACP source obtains a run's CLI launch: a **fixed**
/// argv (one CLI for every `acp:*` thread, from `AWAKEN_ACP_ARGV`), or a **per-run
/// projection** of the run's `acp:<cli>` backend_ref through its [`AcpCli`] row — so
/// the CLI the config plane selected *for that agent* runs inside the isolation, with
/// its model/env projected, rather than a single fixed command.
///
/// This is the public factory input to [`build_acp_channel_source`] (ADR-0057
/// `serve-selected-cli`): a composition root picks `Projected` to serve each run's
/// config-plane-selected CLI, or `Fixed` for a trusted/test single argv.
pub enum LaunchSource {
    /// One CLI for every `acp:*` thread (trusted/test; `AWAKEN_ACP_ARGV`).
    Fixed(AcpLaunch),
    /// The run's config-plane-selected CLI, projected per run through its [`AcpCli`]
    /// row + `resolver` (production; `AWAKEN_ACP_CLI`).
    Projected {
        cli: AcpCli,
        resolver: Arc<dyn LaunchResolver>,
    },
}

impl LaunchSource {
    /// The concrete launch for `activation` — the fixed argv, or the projection of the
    /// run's selected [`AcpCli`]. A projecting source serves exactly one CLI, so a run
    /// whose config-plane selection (`acp:<other>`) names a *different* CLI fails closed
    /// rather than silently running on the wrong runtime — the runtime-side of matching
    /// the declared ACP dialect to what this worker actually serves.
    fn resolve(&self, activation: &RunActivation) -> Result<AcpLaunch, OpenError> {
        match self {
            LaunchSource::Fixed(launch) => Ok(launch.clone()),
            LaunchSource::Projected { cli, resolver } => {
                use awaken_runtime_contract::resolved::Backend;
                let selected = &activation.snapshot.resolved_spec.model_binding.backend_ref;
                match Backend::from_ref(selected) {
                    // A bare `acp` (no CLI named) or the exact CLI this worker serves.
                    Backend::Acp { cli: id } if id.is_empty() || id == cli.id => {}
                    Backend::Acp { cli: id } => {
                        return Err(OpenError(format!(
                            "run selected `acp:{id}` but this worker serves `acp:{}`",
                            cli.id
                        )));
                    }
                    _ => return Err(OpenError(format!("run backend `{selected}` is not ACP"))),
                }
                project_launch(cli, resolver.as_ref(), activation)
            }
        }
    }

    /// The config-plane-selected [`AcpCli`] this source serves, if projecting. `None`
    /// for a fixed argv (which carries no catalog row, so no MCP projection).
    fn cli(&self) -> Option<&AcpCli> {
        match self {
            LaunchSource::Fixed(_) => None,
            LaunchSource::Projected { cli, .. } => Some(cli),
        }
    }
}

/// Shared per-thread deny-egress registrations: the host writes a thread's policy
/// at `prepare_session` (from its environment's networking policy), and both
/// consumers read it — `sandbox_spec` for the native bash-tool jail and
/// [`SandboxChannelSource`] for the ACP agent launch. A thread with no entry
/// shares the host network.
#[derive(Clone, Default)]
pub struct ThreadEgress(Arc<Mutex<HashMap<String, bool>>>);

impl ThreadEgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `thread`'s deny-egress policy (replaces any prior registration).
    pub fn set(&self, thread: &str, deny: bool) {
        self.0
            .lock()
            .expect("thread egress mutex poisoned")
            .insert(thread.to_string(), deny);
    }

    /// Whether `thread` is registered deny-egress.
    pub fn denies(&self, thread: &str) -> bool {
        self.0
            .lock()
            .expect("thread egress mutex poisoned")
            .get(thread)
            .copied()
            .unwrap_or(false)
    }
}

/// The fixed interior path the namespace sandbox binds the workspace to and chdirs
/// into (see `awaken-sandbox-local`), so a cwd-keyed CLI's session slug is stable
/// across relaunches and machines regardless of the host workspace path.
const SANDBOX_WORKSPACE: &str = "/workspace";

/// The fixed interior config-home a `ConfigFileToml` CLI (codex) reads its MCP config
/// from: the projected `config.toml` is mounted here (`MountSource::Inline`) and the
/// CLI's config-home env (e.g. `CODEX_HOME`) points at it.
const SANDBOX_CONFIG_HOME: &str = "/acp-config";

/// A shared clone of the host's per-thread staged-resource registry (ADR-0038), so a
/// sandboxed source carries the SAME file/resource mounts the native `sandbox_spec`
/// does into the bwrap sandbox it launches the CLI in. Opaque like [`ThreadEgress`] —
/// the internal `StagedResources` never crosses the public boundary.
#[derive(Clone, Default)]
pub struct ThreadResources(Arc<Mutex<HashMap<String, crate::provisioning::StagedResources>>>);

impl ThreadResources {
    /// Wrap the host's registry handle (the same `Arc` `sandbox_spec` reads).
    pub(crate) fn new(
        inner: Arc<Mutex<HashMap<String, crate::provisioning::StagedResources>>>,
    ) -> Self {
        Self(inner)
    }

    /// The staged file/resource mounts for `thread`, mapped to the neutral
    /// [`pc::MountRequirement`] the bwrap provider realizes (reusing the native path's
    /// `mount_to_requirement`). Empty when none are staged.
    fn mounts_for(&self, thread: &str) -> Vec<pc::MountRequirement> {
        self.0
            .lock()
            .ok()
            .and_then(|g| {
                g.get(thread).map(|s| {
                    s.mounts
                        .iter()
                        .map(crate::provisioning::mount_to_requirement)
                        .collect()
                })
            })
            .unwrap_or_default()
    }
}

/// Opens each run's [`AgentSession`] inside a fresh namespace-tier sandbox: build
/// the spec from the activation's thread (scope + egress policy), realize it, and
/// `spawn_agent` the launch's argv under bwrap with piped stdio.
pub struct SandboxChannelSource {
    provider: NamespaceProvider,
    launch: LaunchSource,
    egress: ThreadEgress,
    codec: awaken_run_executor_acp::Codec,
    /// The host's staged-resource registry (files/resources → sandbox mounts). `None`
    /// carries no resources (the trusted/test path); the production host wires it.
    resources: Option<ThreadResources>,
}

impl SandboxChannelSource {
    /// A source realizing its sandboxes under `base` (one root per thread scope).
    /// The provider is constructed here so a composition root names only this
    /// crate, not the sandbox tier. Defaults to the newline stand-in wire (the
    /// in-tree fixture agent); a real CLI sets [`Self::with_codec`] to `Codec::Acp`.
    pub fn new(base: impl Into<std::path::PathBuf>, launch: AcpLaunch) -> Self {
        Self {
            provider: NamespaceProvider::new(base),
            launch: LaunchSource::Fixed(launch),
            egress: ThreadEgress::default(),
            codec: awaken_run_executor_acp::Codec::Newline,
            resources: None,
        }
    }

    /// A source that launches the run's **config-plane-selected** CLI: each run's
    /// `acp:<cli>` backend_ref projects through `cli`'s [`AcpCli`] row + `resolver`
    /// (model/env) into the argv run under bwrap. Sets `Codec::Acp` — a real CLI.
    pub fn projecting(
        base: impl Into<std::path::PathBuf>,
        cli: AcpCli,
        resolver: Arc<dyn LaunchResolver>,
    ) -> Self {
        Self {
            provider: NamespaceProvider::new(base),
            launch: LaunchSource::Projected { cli, resolver },
            egress: ThreadEgress::default(),
            codec: awaken_run_executor_acp::Codec::Acp,
            resources: None,
        }
    }

    /// Build from a [`LaunchSource`] chosen by the composition root: `Projected` →
    /// the projecting path (real-ACP codec), `Fixed` → the trusted/test argv (newline
    /// codec). The single seam [`build_acp_channel_source`] threads through.
    pub fn from_source(base: impl Into<std::path::PathBuf>, source: LaunchSource) -> Self {
        match source {
            LaunchSource::Fixed(launch) => Self::new(base, launch),
            LaunchSource::Projected { cli, resolver } => Self::projecting(base, cli, resolver),
        }
    }

    /// Follow per-thread egress registrations (the host's [`ThreadEgress`] handle).
    /// Without it every launch shares the host network.
    #[must_use]
    pub fn with_thread_egress(mut self, egress: ThreadEgress) -> Self {
        self.egress = egress;
        self
    }

    /// The wire the sandboxed agent speaks (a real `claude --acp` → `Codec::Acp`).
    #[must_use]
    pub fn with_codec(mut self, codec: awaken_run_executor_acp::Codec) -> Self {
        self.codec = codec;
        self
    }

    /// Carry the host's staged resource mounts (ADR-0038 files/resources) into the
    /// bwrap sandbox — the SAME registry the native `sandbox_spec` reads, so a resource
    /// bound to a session reaches the isolated ACP CLI, not only the in-process Workdir.
    #[must_use]
    pub fn with_thread_resources(mut self, resources: ThreadResources) -> Self {
        self.resources = Some(resources);
        self
    }

    /// The staged file/resource mounts for `thread`. Empty when no registry is wired.
    fn resource_mounts(&self, thread: &str) -> Vec<pc::MountRequirement> {
        self.resources
            .as_ref()
            .map(|r| r.mounts_for(thread))
            .unwrap_or_default()
    }

    /// The provisioning request for one run: sandbox scoped to the thread (so a
    /// multi-turn session reuses one workspace), network from its registration, and the
    /// session's staged file/resource mounts (ADR-0038) bound into the bwrap interior.
    fn spec(&self, thread: &str) -> pc::SandboxSpec {
        let network = if self.egress.denies(thread) {
            pc::NetworkPolicy::None
        } else {
            pc::NetworkPolicy::Unrestricted
        };
        pc::SandboxSpec {
            scope: thread.to_string(),
            isolation: pc::IsolationClass::Namespace,
            mounts: self.resource_mounts(thread),
            env: Vec::new(),
            network,
            outputs_path: "/mnt/session/outputs".to_string(),
            limits: pc::ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    /// A resolved `launch` projected into the neutral process vocabulary: argv + env as
    /// per-process inline vars (the CLI sees the real values; a secret-splitting broker
    /// is a container-tier capability).
    fn command(launch: &AcpLaunch) -> pc::Command {
        pc::Command {
            argv: launch.argv.clone(),
            cwd: String::new(),
            env: launch
                .env
                .iter()
                .map(|(name, value)| pc::EnvVar {
                    name: name.clone(),
                    value: pc::EnvValue::Inline {
                        value: value.clone(),
                    },
                    visibility: pc::EnvVisibility::Process,
                })
                .collect(),
            stdio: pc::Stdio::Piped,
        }
    }
}

#[async_trait]
impl AgentChannelSource for SandboxChannelSource {
    async fn open(&self, activation: &RunActivation) -> Result<AgentSession, OpenError> {
        let thread = activation.thread_id.0.as_str();
        // The launch is fixed, or projected from this run's config-plane-selected CLI.
        let mut launch = self.launch.resolve(activation)?;
        // Project the run's declared MCP servers once. `session/new` servers ride in-band
        // over the ACP wire the executor drives (claude/gemini/opencode); a config-file
        // CLI (codex) gets its config.toml. Fail-closed on an inline-secret credential
        // (only broker references are sandbox-safe).
        let injection = match self.launch.cli() {
            Some(cli) => awaken_run_executor_acp::mcp_injection(
                cli,
                &activation.snapshot.resolved_spec.plugin_config,
                true,
            )?,
            None => awaken_run_executor_acp::McpInjection::default(),
        };
        // The session's staged resource mounts, plus — for a config-file CLI — the
        // projected config as an inline, read-only, never-harvested mount at the fixed
        // interior config home, with the CLI's config-home env pointed there.
        let mut spec = self.spec(thread);
        if let Some(cli) = self.launch.cli()
            && let Some((mount, (env_key, env_val))) =
                acp_config_mount(cli, injection.config_file.clone())
        {
            spec.mounts.push(mount);
            launch.env.retain(|(k, _)| *k != env_key);
            launch.env.push((env_key, env_val));
        }
        let sandbox = self
            .provider
            .create_sandbox(&spec)
            .await
            .map_err(|e| OpenError(format!("sandbox create: {e}")))?;
        let (process, channel) = sandbox
            .spawn_agent(Self::command(&launch))
            .await
            .map_err(|e| OpenError(format!("sandboxed agent launch: {e}")))?;
        Ok(AgentSession {
            channel,
            process: Arc::from(process),
            codec: self.codec,
            // The namespace sandbox binds the (host-varying) workspace to the fixed
            // interior path `/workspace` and chdirs there, so a cwd-keyed CLI keys its
            // session under the same slug every relaunch/machine — the stable interior
            // identity cross-directory/cross-machine recovery needs.
            workspace_cwd: Some(SANDBOX_WORKSPACE.to_string()),
            mcp_session_servers: injection.session_servers,
        })
    }
}

/// The interior config mount + config-home env override for a config-file CLI's
/// projected MCP config (codex `config.toml`): an inline, read-only, per-run,
/// never-harvested mount at the fixed interior config home, plus `(config_home_env,
/// path)` to point the CLI there. `None` for a session-server CLI (no config file).
/// Pure, so it is testable without a live sandbox.
fn acp_config_mount(
    cli: &AcpCli,
    config_file: Option<(String, String)>,
) -> Option<(pc::MountRequirement, (String, String))> {
    let (rel_path, contents) = config_file?;
    Some((
        pc::MountRequirement {
            mount_id: "acp-config".to_string(),
            source: pc::MountSource::Inline { contents },
            mount_path: format!("{SANDBOX_CONFIG_HOME}/{rel_path}"),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        },
        (
            cli.config_home_env.to_string(),
            SANDBOX_CONFIG_HOME.to_string(),
        ),
    ))
}

/// Runs each turn's agent inside a **user-supplied container image** (ADR-0056 custom
/// rootfs): it realizes a container from the worker-configured [`AgentContainerProvider`]
/// (podman / docker / k8s) with the ACP CLI as the container's main process
/// (process-as-container), then opens the ACP channel to it. The container counterpart
/// of [`SandboxChannelSource`]; the composition root wires whichever a given worker is
/// configured for, so one host binary drives any backend.
pub struct ContainerChannelSource {
    provider: Arc<dyn AgentContainerProvider>,
    launch: LaunchSource,
    egress: ThreadEgress,
    codec: awaken_run_executor_acp::Codec,
}

impl ContainerChannelSource {
    /// A source whose containers are realized by `provider` (the worker's configured
    /// runtime backend + default image). Defaults to the newline stand-in wire; a real
    /// CLI sets [`Self::with_codec`] to `Codec::Acp`.
    pub fn new(provider: Arc<dyn AgentContainerProvider>, launch: AcpLaunch) -> Self {
        Self {
            provider,
            launch: LaunchSource::Fixed(launch),
            egress: ThreadEgress::default(),
            codec: awaken_run_executor_acp::Codec::Newline,
        }
    }

    /// A source that runs the run's **config-plane-selected** CLI inside the container:
    /// each run's `acp:<cli>` backend_ref projects through `cli`'s [`AcpCli`] row +
    /// `resolver` into the container's main command. Sets `Codec::Acp` — a real CLI.
    pub fn projecting(
        provider: Arc<dyn AgentContainerProvider>,
        cli: AcpCli,
        resolver: Arc<dyn LaunchResolver>,
    ) -> Self {
        Self {
            provider,
            launch: LaunchSource::Projected { cli, resolver },
            egress: ThreadEgress::default(),
            codec: awaken_run_executor_acp::Codec::Acp,
        }
    }

    /// Build from a [`LaunchSource`] chosen by the composition root: `Projected` →
    /// the projecting path (real-ACP codec), `Fixed` → the trusted/test argv.
    pub fn from_source(provider: Arc<dyn AgentContainerProvider>, source: LaunchSource) -> Self {
        match source {
            LaunchSource::Fixed(launch) => Self::new(provider, launch),
            LaunchSource::Projected { cli, resolver } => Self::projecting(provider, cli, resolver),
        }
    }

    /// Follow per-thread egress registrations (the host's [`ThreadEgress`] handle): a
    /// deny-egress thread's container runs with a restricted network policy.
    #[must_use]
    pub fn with_thread_egress(mut self, egress: ThreadEgress) -> Self {
        self.egress = egress;
        self
    }

    /// The wire the containerized agent speaks (a real `claude --acp` → `Codec::Acp`).
    #[must_use]
    pub fn with_codec(mut self, codec: awaken_run_executor_acp::Codec) -> Self {
        self.codec = codec;
        self
    }

    /// The provisioning request for one run: a Container-tier spec scoped to the thread,
    /// carrying the ACP CLI argv as the container's main command (process-as-container)
    /// and the launch env as inline process vars; network from the thread's egress
    /// registration. The image is the provider's worker-configured default.
    fn spec(&self, thread: &str, launch: &AcpLaunch) -> pc::SandboxSpec {
        let network = if self.egress.denies(thread) {
            pc::NetworkPolicy::None
        } else {
            pc::NetworkPolicy::Unrestricted
        };
        let env = launch
            .env
            .iter()
            .map(|(name, value)| pc::EnvVar {
                name: name.clone(),
                value: pc::EnvValue::Inline {
                    value: value.clone(),
                },
                visibility: pc::EnvVisibility::Process,
            })
            .collect();
        pc::SandboxSpec {
            scope: thread.to_string(),
            isolation: pc::IsolationClass::Container,
            mounts: Vec::new(),
            env,
            network,
            outputs_path: "/mnt/session/outputs".to_string(),
            limits: pc::ResourceLimits::default(),
            lease_ttl_secs: None,
            // Process-as-container: the agent argv IS the container's main command.
            extra: Some(serde_json::json!({ "command": launch.argv })),
        }
    }
}

#[async_trait]
impl AgentChannelSource for ContainerChannelSource {
    async fn open(&self, activation: &RunActivation) -> Result<AgentSession, OpenError> {
        let thread = activation.thread_id.0.as_str();
        // The container command is fixed, or projected from the run's selected CLI.
        let launch = self.launch.resolve(activation)?;
        let session = self
            .provider
            .open_agent(&self.spec(thread, &launch))
            .await
            .map_err(|e| OpenError(format!("containerized agent launch: {e}")))?;
        Ok(AgentSession {
            channel: session.channel,
            process: Arc::from(session.process),
            codec: self.codec,
            // The container image defines its own interior working directory; the
            // fixed image makes the cwd-keyed session slug stable across relaunches.
            workspace_cwd: None,
            // A containerized AcpSession's session/new MCP projection is a follow-up
            // (needs the CLI row + config-home write into the container interior).
            mcp_session_servers: Vec::new(),
        })
    }
}

/// The TCP port a containerized agent publishes its ACP wire on — the image's
/// entrypoint binds it, the runtime dials it. A fixed convention for now.
#[cfg(any(feature = "container-docker", feature = "container-podman"))]
const CONTAINER_AGENT_PORT: u16 = 8080;

/// Build the ACP [`AgentChannelSource`] a worker serves, from its configured
/// [`SandboxTier`](crate::deployment_config::SandboxTier): the namespace (bwrap) tier
/// by default, or a container tier running the agent in `image` via the matching
/// runtime (podman / docker / k8s). This is the composition seam behind
/// `AWAKEN_SANDBOX_TIER` — different workers pick different backends. A container tier
/// whose backend feature is not compiled in, or with no image configured, fails closed.
pub async fn build_acp_channel_source(
    tier: crate::deployment_config::SandboxTier,
    image: Option<&str>,
    source: LaunchSource,
    egress: ThreadEgress,
    resources: ThreadResources,
    namespace_base: std::path::PathBuf,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    use crate::deployment_config::SandboxTier;
    match tier {
        // No OS isolation: the ACP CLI runs as a plain child of the runtime, driven by
        // the same environment-agnostic executor over an unsandboxed source (ADR-0057).
        SandboxTier::Local => Ok(local_source(source)),
        SandboxTier::Namespace => Ok(Arc::new(
            SandboxChannelSource::from_source(namespace_base, source)
                .with_thread_egress(egress)
                .with_thread_resources(resources),
        )),
        SandboxTier::Docker => build_docker_source(image, source, egress),
        SandboxTier::Podman => build_podman_source(image, source, egress),
        SandboxTier::K8s => build_k8s_source(image, source, egress).await,
    }
}

/// The unsandboxed local source for [`SandboxTier::Local`]: the run's projected CLI
/// (`ProjectingChannelSource`) or a fixed argv (`SubprocessChannelSource`, real-ACP
/// codec). The executor is identical either way — only the source differs.
fn local_source(source: LaunchSource) -> Arc<dyn AgentChannelSource> {
    match source {
        LaunchSource::Fixed(launch) => Arc::new(
            awaken_run_executor_acp::SubprocessChannelSource::new(launch)
                .with_codec(awaken_run_executor_acp::Codec::Acp),
        ),
        LaunchSource::Projected { cli, resolver } => Arc::new(
            awaken_run_executor_acp::ProjectingChannelSource::new(cli, resolver),
        ),
    }
}

/// The image a container tier requires, or a fail-closed error naming the config var.
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn container_image(image: Option<&str>) -> Result<String, String> {
    image
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "a container sandbox tier requires AWAKEN_CONTAINER_IMAGE".to_string())
}

/// Wrap a worker-configured [`AgentContainerProvider`] into a [`ContainerChannelSource`].
#[cfg(any(
    feature = "container-docker",
    feature = "container-podman",
    feature = "container-k8s"
))]
fn container_source(
    provider: Arc<dyn AgentContainerProvider>,
    source: LaunchSource,
    egress: ThreadEgress,
) -> Arc<dyn AgentChannelSource> {
    Arc::new(ContainerChannelSource::from_source(provider, source).with_thread_egress(egress))
}

#[cfg(feature = "container-docker")]
fn build_docker_source(
    image: Option<&str>,
    source: LaunchSource,
    egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    let runtime =
        awaken_sandbox_container::docker::DockerRuntime::connect_local(CONTAINER_AGENT_PORT)
            .map_err(|e| format!("docker runtime: {e}"))?;
    let provider = awaken_sandbox_container::ContainerProvider::new(
        std::sync::Arc::new(runtime),
        container_image(image)?,
    );
    Ok(container_source(Arc::new(provider), source, egress))
}

#[cfg(not(feature = "container-docker"))]
fn build_docker_source(
    _image: Option<&str>,
    _source: LaunchSource,
    _egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    Err("AWAKEN_SANDBOX_TIER=docker needs the `container-docker` feature".into())
}

#[cfg(feature = "container-podman")]
fn build_podman_source(
    image: Option<&str>,
    source: LaunchSource,
    egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    let runtime = awaken_sandbox_container::podman::PodmanRuntime::new(CONTAINER_AGENT_PORT);
    let provider = awaken_sandbox_container::ContainerProvider::new(
        std::sync::Arc::new(runtime),
        container_image(image)?,
    );
    Ok(container_source(Arc::new(provider), source, egress))
}

#[cfg(not(feature = "container-podman"))]
fn build_podman_source(
    _image: Option<&str>,
    _source: LaunchSource,
    _egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    Err("AWAKEN_SANDBOX_TIER=podman needs the `container-podman` feature".into())
}

#[cfg(feature = "container-k8s")]
async fn build_k8s_source(
    image: Option<&str>,
    source: LaunchSource,
    egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    // The Pod's reachable agent address + namespace come from the worker's env; the
    // Service/NodePort exposure is a cluster-deployment concern outside this process.
    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
    let addr = std::env::var("AWAKEN_K8S_AGENT_ADDR")
        .map_err(|_| {
            "AWAKEN_SANDBOX_TIER=k8s needs AWAKEN_K8S_AGENT_ADDR (the Pod's reachable ACP address)"
                .to_string()
        })?
        .parse()
        .map_err(|e| format!("bad AWAKEN_K8S_AGENT_ADDR: {e}"))?;
    let runtime = awaken_sandbox_container::k8s::K8sRuntime::connect(namespace, addr)
        .await
        .map_err(|e| format!("k8s runtime: {e}"))?;
    let provider = awaken_sandbox_container::ContainerProvider::new(
        std::sync::Arc::new(runtime),
        container_image(image)?,
    );
    Ok(container_source(Arc::new(provider), source, egress))
}

#[cfg(not(feature = "container-k8s"))]
async fn build_k8s_source(
    _image: Option<&str>,
    _source: LaunchSource,
    _egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    Err("AWAKEN_SANDBOX_TIER=k8s needs the `container-k8s` feature".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("awaken-sbxsrc-ut-{}", std::process::id()))
    }

    #[test]
    fn spec_carries_the_thread_staged_resource_mounts_into_the_sandbox() {
        // ADR-0038 resources bound to a session must reach the isolated ACP sandbox,
        // not only the in-process Workdir — the same registry the native sandbox_spec
        // reads, mapped to bwrap-realizable mounts.
        use awaken_sandbox_local::{Mount, ResourceMount};
        let registry = ThreadResources::default();
        registry.0.lock().unwrap().insert(
            "t1".to_string(),
            crate::provisioning::StagedResources {
                mounts: vec![Mount::Resource(ResourceMount {
                    id: "id-data".into(),
                    content_hash: String::new(),
                    logical_path: "data.csv".into(),
                    content: "a,b\n".into(),
                })],
                ..Default::default()
            },
        );
        let src = SandboxChannelSource::new(base(), AcpLaunch::custom(vec!["a".into()], vec![]))
            .with_thread_resources(registry);
        let spec = src.spec("t1");
        assert_eq!(
            spec.mounts.len(),
            1,
            "the staged resource reaches the sandbox spec"
        );
        assert_eq!(spec.mounts[0].mount_path, ".mnt/data.csv");
        // A thread with nothing staged mounts nothing.
        assert!(src.spec("other").mounts.is_empty());
    }

    #[test]
    fn codex_config_file_becomes_an_inline_readonly_mount_and_points_the_config_home() {
        // A config-file CLI (codex): the projected config.toml is mounted inline
        // (ephemeral, read-only, never-harvested) at the interior config home, and the
        // CLI's config-home env (CODEX_HOME) points there so it reads the MCP config.
        let cli = awaken_run_executor_acp::acp_cli("codex").unwrap();
        let (mount, (env_key, env_val)) = acp_config_mount(
            cli,
            Some(("config.toml".into(), "[mcp_servers.gh]\n".into())),
        )
        .expect("codex gets a config mount");
        assert_eq!(mount.mount_path, "/acp-config/config.toml");
        assert!(matches!(mount.source, pc::MountSource::Inline { .. }));
        assert_eq!(mount.access, pc::MountAccess::ReadOnly);
        assert_eq!(mount.lifetime, pc::MountLifetime::PerRun);
        assert_eq!(env_key, "CODEX_HOME");
        assert_eq!(env_val, "/acp-config");

        // A session-server CLI (claude, no config file) → no config mount.
        let claude = awaken_run_executor_acp::acp_cli("claude").unwrap();
        assert!(acp_config_mount(claude, None).is_none());
    }

    #[test]
    fn spec_shares_the_host_network_without_a_registration() {
        let src = SandboxChannelSource::new(base(), AcpLaunch::custom(vec!["a".into()], vec![]));
        let spec = src.spec("t");
        assert!(matches!(spec.network, pc::NetworkPolicy::Unrestricted));
        assert_eq!(spec.isolation, pc::IsolationClass::Namespace);
        assert_eq!(spec.scope, "t");
        assert_eq!(spec.outputs_path, "/mnt/session/outputs");
    }

    #[test]
    fn spec_maps_a_deny_egress_thread_to_no_network() {
        let egress = ThreadEgress::new();
        egress.set("iso", true);
        let src = SandboxChannelSource::new(base(), AcpLaunch::custom(vec!["a".into()], vec![]))
            .with_thread_egress(egress);
        assert!(matches!(src.spec("iso").network, pc::NetworkPolicy::None));
        // A sibling thread with no registration still shares the host network.
        assert!(matches!(
            src.spec("open").network,
            pc::NetworkPolicy::Unrestricted
        ));
    }

    #[test]
    fn command_projects_launch_env_as_inline_process_vars_with_piped_stdio() {
        let launch = AcpLaunch::custom(
            vec!["prog".into(), "--flag".into()],
            vec![("K".into(), "V".into())],
        );
        let cmd = SandboxChannelSource::command(&launch);
        assert_eq!(cmd.argv, vec!["prog".to_string(), "--flag".to_string()]);
        assert!(matches!(cmd.stdio, pc::Stdio::Piped));
        assert_eq!(cmd.env.len(), 1);
        assert_eq!(cmd.env[0].name, "K");
        assert!(matches!(cmd.env[0].value, pc::EnvValue::Inline { .. }));
        assert!(matches!(cmd.env[0].visibility, pc::EnvVisibility::Process));
    }

    #[test]
    fn thread_egress_defaults_false_and_set_overwrites() {
        let e = ThreadEgress::new();
        assert!(
            !e.denies("x"),
            "an unregistered thread shares the host network"
        );
        e.set("x", true);
        assert!(e.denies("x"));
        e.set("x", false); // a later registration replaces the prior one
        assert!(!e.denies("x"));
    }

    /// A container provider stand-in — the spec-projection tests never open a
    /// container, so `open_agent` is unreachable here (that path is covered in the
    /// container crate's `open_agent` test against its scripted runtime).
    struct UnusedContainerProvider;
    #[async_trait]
    impl AgentContainerProvider for UnusedContainerProvider {
        async fn open_agent(
            &self,
            _spec: &pc::SandboxSpec,
        ) -> Result<awaken_sandbox_container::AgentContainerSession, pc::SandboxError> {
            unreachable!("spec-projection tests do not open a container")
        }
    }

    fn container_source(
        argv: Vec<String>,
        env: Vec<(String, String)>,
    ) -> (ContainerChannelSource, AcpLaunch) {
        let launch = AcpLaunch::custom(argv, env);
        let src = ContainerChannelSource::new(Arc::new(UnusedContainerProvider), launch.clone());
        (src, launch)
    }

    #[test]
    fn container_spec_is_process_as_container_on_the_container_tier() {
        let (src, launch) = container_source(vec!["claude".into(), "--acp".into()], vec![]);
        let spec = src.spec("t", &launch);
        assert_eq!(spec.isolation, pc::IsolationClass::Container);
        assert_eq!(spec.scope, "t");
        // The agent argv IS the container's main command (not exec-into-idle).
        assert_eq!(
            spec.extra
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>()),
            Some(vec!["claude", "--acp"])
        );
        // No egress registration → shares the host network.
        assert!(matches!(spec.network, pc::NetworkPolicy::Unrestricted));
    }

    #[test]
    fn container_spec_maps_deny_egress_to_no_network_and_env_to_inline_vars() {
        let egress = ThreadEgress::new();
        egress.set("iso", true);
        let (src, launch) = container_source(vec!["claude".into()], vec![("K".into(), "V".into())]);
        let src = src.with_thread_egress(egress);
        let spec = src.spec("iso", &launch);
        assert!(matches!(spec.network, pc::NetworkPolicy::None));
        assert_eq!(spec.env.len(), 1);
        assert_eq!(spec.env[0].name, "K");
        assert!(matches!(spec.env[0].value, pc::EnvValue::Inline { .. }));
    }

    // ---- per-run CLI projection (the config-plane-selected ACP runtime) ----

    /// A `LaunchResolver` stand-in — the projection reads the model through it.
    struct FakeResolver;
    impl LaunchResolver for FakeResolver {
        fn model(
            &self,
            _a: &RunActivation,
        ) -> Result<awaken_run_executor_acp::ResolvedModel, OpenError> {
            Ok(awaken_run_executor_acp::ResolvedModel {
                base_url: "http://x".into(),
                model: "m".into(),
                api_key: "k".into(),
            })
        }
    }

    /// A container process stand-in for the projection test (never polled for real).
    struct FakeProc;
    #[async_trait]
    impl pc::ProcessHandle for FakeProc {
        fn id(&self) -> &str {
            "p"
        }
        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(pc::ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }
        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(None)
        }
        async fn signal(&self, _s: pc::Signal) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    /// Records the container command the source asked to run.
    struct CapturingProvider(std::sync::Mutex<Option<Vec<String>>>);
    #[async_trait]
    impl AgentContainerProvider for CapturingProvider {
        async fn open_agent(
            &self,
            spec: &pc::SandboxSpec,
        ) -> Result<awaken_sandbox_container::AgentContainerSession, pc::SandboxError> {
            let cmd = spec
                .extra
                .as_ref()
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                });
            *self.0.lock().unwrap() = cmd;
            let (ours, _peer) = tokio::io::duplex(64);
            Ok(awaken_sandbox_container::AgentContainerSession {
                channel: Box::new(ours),
                process: Box::new(FakeProc),
                handle: pc::SandboxHandle::new("container", "t"),
            })
        }
    }

    fn acp_activation(backend_ref: &str) -> RunActivation {
        use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
        use awaken_runtime_contract::snapshot::{
            AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
        };
        RunActivation::new(
            awaken_agent_contract::agent::run::Id("r".into()),
            awaken_agent_contract::agent::thread::Id("t".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    // The config-plane selection: this agent runs on `acp:claude`.
                    model_binding: ModelBinding::new("prov", "m", backend_ref),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn a_projecting_container_source_runs_the_agents_selected_cli() {
        let captured = Arc::new(CapturingProvider(std::sync::Mutex::new(None)));
        let cli = awaken_run_executor_acp::acp_cli("claude").expect("claude is a known cli");
        let resolver = Arc::new(FakeResolver);
        let source = ContainerChannelSource::projecting(captured.clone(), *cli, resolver.clone());

        // A run whose agent selected `acp:claude` in the config plane.
        let act = acp_activation("acp:claude");
        source
            .open(&act)
            .await
            .expect("open the containerized agent");

        // The container ran claude's *projected* launch (npx …), not a fixed argv —
        // i.e. the config-plane CLI selection reached the container tier.
        let cmd = captured
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("open_agent was called");
        let expected = project_launch(cli, resolver.as_ref(), &act)
            .expect("project")
            .argv;
        assert_eq!(
            cmd, expected,
            "the container runs the agent's projected CLI launch"
        );
        assert_eq!(
            cmd.first().map(String::as_str),
            Some("npx"),
            "claude projects to an npx launch: {cmd:?}"
        );
    }

    #[tokio::test]
    async fn a_run_for_a_different_cli_than_the_worker_serves_fails_closed() {
        // This worker serves `acp:claude`; a run whose agent selected `acp:codex` must
        // not silently run on claude — the declared ACP dialect is matched at open time.
        let captured = Arc::new(CapturingProvider(std::sync::Mutex::new(None)));
        let cli = awaken_run_executor_acp::acp_cli("claude").expect("claude");
        let source =
            ContainerChannelSource::projecting(captured.clone(), *cli, Arc::new(FakeResolver));

        let err = source
            .open(&acp_activation("acp:codex"))
            .await
            .err()
            .expect("a CLI mismatch must fail closed");
        assert!(
            err.0.contains("acp:codex") && err.0.contains("acp:claude"),
            "{err:?}"
        );
        assert!(
            captured.0.lock().unwrap().is_none(),
            "no container was created for the mismatched run"
        );
    }

    #[tokio::test]
    async fn the_factory_builds_the_namespace_source_by_default() {
        use crate::deployment_config::SandboxTier;
        // The namespace tier always builds (no image, no container feature needed).
        let src = build_acp_channel_source(
            SandboxTier::Namespace,
            None,
            LaunchSource::Fixed(AcpLaunch::custom(vec!["claude".into()], vec![])),
            ThreadEgress::new(),
            ThreadResources::default(),
            base(),
        )
        .await;
        assert!(src.is_ok(), "namespace tier must always build");
    }

    #[tokio::test]
    async fn the_local_tier_builds_an_unsandboxed_source() {
        use crate::deployment_config::SandboxTier;
        // The `local` tier needs no bwrap and no container feature — the same executor
        // drives it over a plain subprocess source (ADR-0057: executor is tier-unaware).
        let src = build_acp_channel_source(
            SandboxTier::Local,
            None,
            LaunchSource::Fixed(AcpLaunch::custom(vec!["claude".into()], vec![])),
            ThreadEgress::new(),
            ThreadResources::default(),
            base(),
        )
        .await;
        assert!(src.is_ok(), "local tier must always build");
    }

    #[tokio::test]
    async fn a_container_tier_without_its_backend_feature_fails_closed() {
        use crate::deployment_config::SandboxTier;
        // With no container-* feature compiled in, a container tier is a clear error —
        // never a silent fallback that would ignore the worker's configured backend.
        for tier in [SandboxTier::Docker, SandboxTier::Podman, SandboxTier::K8s] {
            let out = build_acp_channel_source(
                tier,
                Some("ghcr.io/x/agent:1"),
                LaunchSource::Fixed(AcpLaunch::custom(vec!["claude".into()], vec![])),
                ThreadEgress::new(),
                ThreadResources::default(),
                base(),
            )
            .await;
            #[cfg(not(any(
                feature = "container-docker",
                feature = "container-podman",
                feature = "container-k8s"
            )))]
            assert!(
                out.is_err(),
                "{tier:?} without its feature must fail closed"
            );
            let _ = out;
        }
    }
}
