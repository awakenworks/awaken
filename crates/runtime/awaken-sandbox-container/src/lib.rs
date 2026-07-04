//! Container / Kubernetes sandbox provider (ADR-0041 Slice 5), below the neutral
//! seam. It realizes the same [`pc::SandboxProvider`]/[`pc::Sandbox`] ports as the
//! local tier, over a dependency-inverted [`ContainerRuntime`] port so the provider
//! *logic* is exercised by a fake while the real bollard/kube clients slot in as
//! adapters behind the port (kept out of the neutral contract; never depended on by
//! the agents plane — G2).
//!
//! Three decisions from the ADR amendment are made concrete and unit-testable here
//! as **pure planners** (no daemon needed):
//! - **process-as-container**: [`pod_plan`] sets the Pod's container command to the
//!   agent argv rather than exec-ing into an idle Pod;
//! - **native GC**: the Pod carries an `owner_uid` (an `ownerReference`) so an
//!   orphaned sandbox is reaped by the platform, not a bespoke reaper;
//! - **out-of-band artifacts**: outputs live on a volume ([`ContainerPlan::outputs_volume`]),
//!   retrieved without streaming through the control plane.

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport, ChannelError};
use awaken_provisioning_contract as pc;
use std::sync::Arc;

/// Container-tier capabilities: OS-enforced isolation strong enough to host an
/// opaque agent, with the guarantees bwrap could not give (allowlist egress,
/// resource limits, a custom rootfs/image).
#[must_use]
pub fn container_capabilities() -> pc::SandboxCapabilities {
    pc::SandboxCapabilities {
        isolation: pc::IsolationClass::Container,
        tool_transparent: true,
        path_fidelity: true,
        enforced_readonly: true,
        network_isolation: true,
        secret_egress_substitution: false,
        resource_limits: true,
        custom_rootfs: true,
    }
}

// ── Pure planners ─────────────────────────────────────────────────────────────

/// How egress maps onto a container network mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkMode {
    /// Full egress (default bridge).
    Open,
    /// Deny-by-default with an allowlist (enforced by the runtime/CNI).
    Allowlist(Vec<String>),
    /// No network.
    None,
}

/// One realized bind inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindPlan {
    pub source_ref: String,
    pub mount_path: String,
    pub read_only: bool,
}

/// A neutral container plan rendered from a [`pc::SandboxSpec`] — the input a
/// bollard `create_container` (or the k8s planner) consumes. Pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerPlan {
    pub image: String,
    /// The agent process — the container's main command (process-as-container, NOT
    /// exec-into-idle). Fixed at create, like a Pod's container command.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub binds: Vec<BindPlan>,
    /// Out-of-band outputs volume mount path (artifacts leave via the volume).
    pub outputs_volume: String,
    pub network: NetworkMode,
    pub limits: pc::ResourceLimits,
}

fn image_of(spec: &pc::SandboxSpec, default_image: &str) -> String {
    spec.extra
        .as_ref()
        .and_then(|v| v.get("image"))
        .and_then(|v| v.as_str())
        .unwrap_or(default_image)
        .to_string()
}

fn network_of(policy: &pc::NetworkPolicy) -> NetworkMode {
    match policy {
        pc::NetworkPolicy::Unrestricted => NetworkMode::Open,
        pc::NetworkPolicy::Allowlist { hosts } => NetworkMode::Allowlist(hosts.clone()),
        pc::NetworkPolicy::None => NetworkMode::None,
    }
}

fn binds_of(spec: &pc::SandboxSpec) -> Vec<BindPlan> {
    spec.mounts
        .iter()
        .map(|m| BindPlan {
            source_ref: mount_ref(&m.source),
            mount_path: m.mount_path.clone(),
            read_only: m.access == pc::MountAccess::ReadOnly,
        })
        .collect()
}

fn mount_ref(source: &pc::MountSource) -> String {
    match source {
        pc::MountSource::File { file_id, .. } => file_id.clone(),
        pc::MountSource::Resource { resource_id, .. } => resource_id.clone(),
        pc::MountSource::MemoryStore { store_id } => store_id.clone(),
        pc::MountSource::Secret { reference, .. } => reference.clone(),
        pc::MountSource::Other(_) => String::new(),
    }
}

/// The agent command for a process-as-container tier, read from `spec.extra.command`
/// (a JSON array of strings). The container/pod runs this as its main process; an
/// empty command is a caller error the provider rejects fail-closed.
#[must_use]
pub fn command_of(spec: &pc::SandboxSpec) -> Vec<String> {
    spec.extra
        .as_ref()
        .and_then(|v| v.get("command"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn inline_env(spec: &pc::SandboxSpec) -> Vec<(String, String)> {
    spec.env
        .iter()
        .filter_map(|v| match &v.value {
            pc::EnvValue::Inline { value } => Some((v.name.clone(), value.clone())),
            // Secret refs are resolved by the broker at the runtime edge, not planned here.
            pc::EnvValue::Secret { .. } => None,
        })
        .collect()
}

/// Render a [`ContainerPlan`] from a spec + the agent command (Docker path). Pure.
#[must_use]
pub fn container_plan(
    spec: &pc::SandboxSpec,
    default_image: &str,
    command: &[String],
) -> ContainerPlan {
    ContainerPlan {
        image: image_of(spec, default_image),
        command: command.to_vec(),
        env: inline_env(spec),
        binds: binds_of(spec),
        outputs_volume: spec.outputs_path.clone(),
        network: network_of(&spec.network),
        limits: spec.limits.clone(),
    }
}

/// A neutral Kubernetes Pod plan (the k8s path). Encodes **process-as-container**
/// (the container command *is* the agent) and **native GC** (an `owner_uid`
/// ownerReference), with the outputs volume for out-of-band artifacts. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodPlan {
    pub name: String,
    pub image: String,
    /// The agent process — the Pod's container command (NOT exec-into-idle).
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub binds: Vec<BindPlan>,
    pub outputs_volume: String,
    pub network: NetworkMode,
    pub limits: pc::ResourceLimits,
    /// ownerReference UID; the platform garbage-collects the Pod when the owner
    /// (e.g. a lease object) is deleted — no custom reaper.
    pub owner_uid: String,
    /// Never restart in place: a finished agent Pod is reaped, not looped.
    pub restart_never: bool,
}

/// Render a [`PodPlan`] from a spec + the agent command + a GC owner. Pure.
#[must_use]
pub fn pod_plan(
    spec: &pc::SandboxSpec,
    command: &pc::Command,
    default_image: &str,
    owner_uid: &str,
) -> PodPlan {
    PodPlan {
        name: format!("awaken-{}", spec.scope),
        image: image_of(spec, default_image),
        command: command.argv.clone(),
        env: inline_env(spec),
        binds: binds_of(spec),
        outputs_volume: spec.outputs_path.clone(),
        network: network_of(&spec.network),
        limits: spec.limits.clone(),
        owner_uid: owner_uid.to_string(),
        restart_never: true,
    }
}

// ── Runtime port (dependency inversion) ─────────────────────────────────────────

/// A container/pod runtime failure.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("container {0:?} not found")]
    NotFound(String),
    #[error("container runtime failed: {0}")]
    Backend(String),
}

/// Whether a container is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    Gone,
}

/// The seam the provider drives — implemented by a bollard adapter (Docker) or a
/// kube adapter (K8s), and by an in-memory fake in tests. Names no neutral-contract
/// type beyond the value objects it must move.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Create + start the container/pod running `plan.command` as its main process.
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError>;
    /// Open a duplex channel to the running agent — bollard container attach for
    /// Docker, a network dial (TCP / reverse-dial via `awaken-connection`) for a
    /// firewalled Pod. This replaces exec-into-idle: the agent *is* the container.
    async fn open_channel(&self, container_id: &str)
    -> Result<Box<dyn AgentChannel>, RuntimeError>;
    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError>;
    /// Wait for the container's main process (the agent) to exit.
    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError>;
    /// Poll the main process; `None` while it is still running.
    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError>;
    /// Signal (reap) the container's main process.
    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError>;
    async fn artifacts(&self, container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError>;
    async fn read_artifact(
        &self,
        container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError>;
    async fn touch_lease(&self, container_id: &str) -> Result<(), RuntimeError>;
    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError>;
}

fn err(e: RuntimeError) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

// ── Provider + Sandbox over the port ────────────────────────────────────────────

/// Realizes [`pc::Sandbox`]es on a [`ContainerRuntime`].
pub struct ContainerProvider<R: ContainerRuntime> {
    runtime: Arc<R>,
    default_image: String,
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    pub fn new(runtime: Arc<R>, default_image: impl Into<String>) -> Self {
        Self {
            runtime,
            default_image: default_image.into(),
        }
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::SandboxProvider for ContainerProvider<R> {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        container_capabilities()
    }

    async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        // Fail closed against our capabilities before touching the runtime.
        pc::prepare_environment(spec, &container_capabilities())
            .map_err(|e| err(RuntimeError::Backend(e.to_string())))?;

        // Process-as-container: the agent command is provisioned at create, not
        // exec'd into an idle container later.
        let command = command_of(spec);
        if command.is_empty() {
            return Err(err(RuntimeError::Backend(
                "container tier requires spec.extra.command (process-as-container)".into(),
            )));
        }
        let plan = container_plan(spec, &self.default_image, &command);
        let container_id = self.runtime.create(&spec.scope, &plan).await.map_err(err)?;
        let realized = plan
            .binds
            .iter()
            .zip(&spec.mounts)
            .map(|(b, m)| pc::RealizedMount {
                mount_id: m.mount_id.clone(),
                mount_path: b.mount_path.clone(),
                access: m.access,
                realization: pc::Realization::Bind,
                content_hash: None,
            })
            .collect();
        Ok(Box::new(ContainerSandbox {
            runtime: self.runtime.clone(),
            id: spec.scope.clone(),
            container_id,
            outputs_path: spec.outputs_path.clone(),
            realized,
        }))
    }

    async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        let container_id = handle
            .extra
            .as_ref()
            .and_then(|v| v.get("container_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| err(RuntimeError::Backend("handle missing container_id".into())))?
            .to_string();
        let outputs_path = handle
            .extra
            .as_ref()
            .and_then(|v| v.get("outputs_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("/mnt/session/outputs")
            .to_string();
        // Prove the container is still there before handing back a live sandbox.
        self.runtime.inspect(&container_id).await.map_err(err)?;
        Ok(Box::new(ContainerSandbox {
            runtime: self.runtime.clone(),
            id: handle.sandbox_id.clone(),
            container_id,
            outputs_path,
            realized: Vec::new(),
        }))
    }
}

struct ContainerSandbox<R: ContainerRuntime> {
    runtime: Arc<R>,
    id: String,
    container_id: String,
    outputs_path: String,
    realized: Vec<pc::RealizedMount>,
}

/// A handle over the container's main process (the agent). On this tier the process
/// lifecycle *is* the container lifecycle — wait/poll/signal act on the container.
struct ContainerProcess<R: ContainerRuntime> {
    runtime: Arc<R>,
    container_id: String,
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::ProcessHandle for ContainerProcess<R> {
    fn id(&self) -> &str {
        &self.container_id
    }
    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        self.runtime.wait(&self.container_id).await.map_err(err)
    }
    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        self.runtime.poll(&self.container_id).await.map_err(err)
    }
    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        self.runtime
            .signal(&self.container_id, signal)
            .await
            .map_err(err)
    }
}

/// The tool-transparent capability: the container sandbox hands the ACP bridge a
/// duplex channel to its process-as-container agent (the same seam local uses).
#[async_trait]
impl<R: ContainerRuntime + 'static> AgentTransport for ContainerSandbox<R> {
    async fn open_channel(&self) -> Result<Box<dyn AgentChannel>, ChannelError> {
        self.runtime
            .open_channel(&self.container_id)
            .await
            .map_err(|e| ChannelError::Setup(e.to_string()))
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::Sandbox for ContainerSandbox<R> {
    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        let mut h = pc::SandboxHandle::new("container", &self.id);
        h.extra = Some(serde_json::json!({
            "container_id": self.container_id,
            "outputs_path": self.outputs_path,
        }));
        h
    }

    async fn spawn(
        &self,
        _command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        // Process-as-container: the agent was launched as the container's main
        // process at `create`. `spawn` returns a handle to it (the command is
        // provisioned, not re-launched); drive its stdio via `AgentTransport`.
        Ok(Box::new(ContainerProcess {
            runtime: self.runtime.clone(),
            container_id: self.container_id.clone(),
        }))
    }

    async fn attach(
        &self,
        _req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        // Docker/K8s cannot hot-mount into a running container; fail closed (mount
        // at create, or use a resourced sidecar over a shared volume).
        Err(err(RuntimeError::Backend(
            "late attach unsupported on the container tier; mount at create".into(),
        )))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        self.runtime
            .artifacts(&self.container_id)
            .await
            .map_err(err)
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.runtime
            .read_artifact(&self.container_id, id)
            .await
            .map_err(err)
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &self.realized
    }

    async fn process(
        &self,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        // One process per container tier: reconnect to the main agent process.
        Ok(Box::new(ContainerProcess {
            runtime: self.runtime.clone(),
            container_id: self.container_id.clone(),
        }))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        match self
            .runtime
            .inspect(&self.container_id)
            .await
            .map_err(err)?
        {
            ContainerState::Running => Ok(pc::SandboxStatus::Ready),
            ContainerState::Gone => Ok(pc::SandboxStatus::Terminated),
        }
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        self.runtime
            .touch_lease(&self.container_id)
            .await
            .map_err(err)
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        self.runtime.remove(&self.container_id).await.map_err(err)
    }
}

#[cfg(test)]
mod tests;
