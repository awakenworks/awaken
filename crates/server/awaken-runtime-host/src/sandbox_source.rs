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
use awaken_run_executor_acp::{AcpLaunch, AgentChannelSource, AgentSession, OpenError};
use awaken_runtime_contract::activation::RunActivation;
use awaken_sandbox_container::AgentContainerProvider;
use awaken_sandbox_local::NamespaceProvider;

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

/// Opens each run's [`AgentSession`] inside a fresh namespace-tier sandbox: build
/// the spec from the activation's thread (scope + egress policy), realize it, and
/// `spawn_agent` the launch's argv under bwrap with piped stdio.
pub struct SandboxChannelSource {
    provider: NamespaceProvider,
    launch: AcpLaunch,
    egress: ThreadEgress,
    codec: awaken_run_executor_acp::Codec,
}

impl SandboxChannelSource {
    /// A source realizing its sandboxes under `base` (one root per thread scope).
    /// The provider is constructed here so a composition root names only this
    /// crate, not the sandbox tier. Defaults to the newline stand-in wire (the
    /// in-tree fixture agent); a real CLI sets [`Self::with_codec`] to `Codec::Acp`.
    pub fn new(base: impl Into<std::path::PathBuf>, launch: AcpLaunch) -> Self {
        Self {
            provider: NamespaceProvider::new(base),
            launch,
            egress: ThreadEgress::default(),
            codec: awaken_run_executor_acp::Codec::Newline,
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

    /// The provisioning request for one run: sandbox scoped to the thread (so a
    /// multi-turn session reuses one workspace), network from its registration.
    fn spec(&self, thread: &str) -> pc::SandboxSpec {
        let network = if self.egress.denies(thread) {
            pc::NetworkPolicy::None
        } else {
            pc::NetworkPolicy::Unrestricted
        };
        pc::SandboxSpec {
            scope: thread.to_string(),
            isolation: pc::IsolationClass::Namespace,
            mounts: Vec::new(),
            env: Vec::new(),
            network,
            outputs_path: "/mnt/session/outputs".to_string(),
            limits: pc::ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    /// The launch projected into the neutral process vocabulary: argv + env as
    /// per-process inline vars (the CLI sees the real values; a secret-splitting
    /// broker is a container-tier capability).
    fn command(&self) -> pc::Command {
        pc::Command {
            argv: self.launch.argv.clone(),
            cwd: String::new(),
            env: self
                .launch
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
        let sandbox = self
            .provider
            .create_sandbox(&self.spec(thread))
            .await
            .map_err(|e| OpenError(format!("sandbox create: {e}")))?;
        let (process, channel) = sandbox
            .spawn_agent(self.command())
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
            // The sandboxed source launches a fixed `AcpLaunch`, not a per-run `AcpCli`
            // row, so it does not yet project `session/new` MCP servers (that lives in
            // `ProjectingChannelSource::open`). A sandboxed AcpSession CLI's α-secretless
            // MCP delivery is a follow-up: it needs the CLI row + config-home write into
            // the sandbox interior. Empty here means "none projected", not "none declared".
            mcp_session_servers: Vec::new(),
        })
    }
}

/// Runs each turn's agent inside a **user-supplied container image** (ADR-0056 custom
/// rootfs): it realizes a container from the worker-configured [`AgentContainerProvider`]
/// (podman / docker / k8s) with the ACP CLI as the container's main process
/// (process-as-container), then opens the ACP channel to it. The container counterpart
/// of [`SandboxChannelSource`]; the composition root wires whichever a given worker is
/// configured for, so one host binary drives any backend.
pub struct ContainerChannelSource {
    provider: Arc<dyn AgentContainerProvider>,
    launch: AcpLaunch,
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
            launch,
            egress: ThreadEgress::default(),
            codec: awaken_run_executor_acp::Codec::Newline,
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
    fn spec(&self, thread: &str) -> pc::SandboxSpec {
        let network = if self.egress.denies(thread) {
            pc::NetworkPolicy::None
        } else {
            pc::NetworkPolicy::Unrestricted
        };
        let env = self
            .launch
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
            extra: Some(serde_json::json!({ "command": self.launch.argv })),
        }
    }
}

#[async_trait]
impl AgentChannelSource for ContainerChannelSource {
    async fn open(&self, activation: &RunActivation) -> Result<AgentSession, OpenError> {
        let thread = activation.thread_id.0.as_str();
        let session = self
            .provider
            .open_agent(&self.spec(thread))
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
    launch: AcpLaunch,
    egress: ThreadEgress,
    namespace_base: std::path::PathBuf,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    use crate::deployment_config::SandboxTier;
    match tier {
        SandboxTier::Namespace => Ok(Arc::new(
            SandboxChannelSource::new(namespace_base, launch).with_thread_egress(egress),
        )),
        SandboxTier::Docker => build_docker_source(image, launch, egress),
        SandboxTier::Podman => build_podman_source(image, launch, egress),
        SandboxTier::K8s => build_k8s_source(image, launch, egress).await,
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
    launch: AcpLaunch,
    egress: ThreadEgress,
) -> Arc<dyn AgentChannelSource> {
    Arc::new(ContainerChannelSource::new(provider, launch).with_thread_egress(egress))
}

#[cfg(feature = "container-docker")]
fn build_docker_source(
    image: Option<&str>,
    launch: AcpLaunch,
    egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    let runtime =
        awaken_sandbox_container::docker::DockerRuntime::connect_local(CONTAINER_AGENT_PORT)
            .map_err(|e| format!("docker runtime: {e}"))?;
    let provider = awaken_sandbox_container::ContainerProvider::new(
        std::sync::Arc::new(runtime),
        container_image(image)?,
    );
    Ok(container_source(Arc::new(provider), launch, egress))
}

#[cfg(not(feature = "container-docker"))]
fn build_docker_source(
    _image: Option<&str>,
    _launch: AcpLaunch,
    _egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    Err("AWAKEN_SANDBOX_TIER=docker needs the `container-docker` feature".into())
}

#[cfg(feature = "container-podman")]
fn build_podman_source(
    image: Option<&str>,
    launch: AcpLaunch,
    egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    let runtime = awaken_sandbox_container::podman::PodmanRuntime::new(CONTAINER_AGENT_PORT);
    let provider = awaken_sandbox_container::ContainerProvider::new(
        std::sync::Arc::new(runtime),
        container_image(image)?,
    );
    Ok(container_source(Arc::new(provider), launch, egress))
}

#[cfg(not(feature = "container-podman"))]
fn build_podman_source(
    _image: Option<&str>,
    _launch: AcpLaunch,
    _egress: ThreadEgress,
) -> Result<Arc<dyn AgentChannelSource>, String> {
    Err("AWAKEN_SANDBOX_TIER=podman needs the `container-podman` feature".into())
}

#[cfg(feature = "container-k8s")]
async fn build_k8s_source(
    image: Option<&str>,
    launch: AcpLaunch,
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
    Ok(container_source(Arc::new(provider), launch, egress))
}

#[cfg(not(feature = "container-k8s"))]
async fn build_k8s_source(
    _image: Option<&str>,
    _launch: AcpLaunch,
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
        let src = SandboxChannelSource::new(
            base(),
            AcpLaunch::custom(
                vec!["prog".into(), "--flag".into()],
                vec![("K".into(), "V".into())],
            ),
        );
        let cmd = src.command();
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

    fn container_source(argv: Vec<String>, env: Vec<(String, String)>) -> ContainerChannelSource {
        ContainerChannelSource::new(
            Arc::new(UnusedContainerProvider),
            AcpLaunch::custom(argv, env),
        )
    }

    #[test]
    fn container_spec_is_process_as_container_on_the_container_tier() {
        let src = container_source(vec!["claude".into(), "--acp".into()], vec![]);
        let spec = src.spec("t");
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
        let src = container_source(vec!["claude".into()], vec![("K".into(), "V".into())])
            .with_thread_egress(egress);
        let spec = src.spec("iso");
        assert!(matches!(spec.network, pc::NetworkPolicy::None));
        assert_eq!(spec.env.len(), 1);
        assert_eq!(spec.env[0].name, "K");
        assert!(matches!(spec.env[0].value, pc::EnvValue::Inline { .. }));
    }

    #[tokio::test]
    async fn the_factory_builds_the_namespace_source_by_default() {
        use crate::deployment_config::SandboxTier;
        // The namespace tier always builds (no image, no container feature needed).
        let src = build_acp_channel_source(
            SandboxTier::Namespace,
            None,
            AcpLaunch::custom(vec!["claude".into()], vec![]),
            ThreadEgress::new(),
            base(),
        )
        .await;
        assert!(src.is_ok(), "namespace tier must always build");
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
                AcpLaunch::custom(vec!["claude".into()], vec![]),
                ThreadEgress::new(),
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
