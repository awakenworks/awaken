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

/// A memory-store mount realized as a **memoryd sidecar** sharing an `emptyDir` with
/// the agent container (the k8s/container form of ADR-0038 MemoryStore). A memory
/// store is a keyed store, not a host byte path — binding `store_id` as a path (the
/// prior behavior) was a meaningless no-op; the sidecar FUSE-serves it into a
/// pod-scoped volume the agent reads, and harvests writes back on teardown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMount {
    pub store_id: String,
    /// Sandbox-absolute path the agent sees the store at (the shared volume mount).
    pub mount_path: String,
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
    /// Memory-store mounts, realized as memoryd sidecars + shared volumes (NOT binds).
    pub memory_mounts: Vec<MemoryMount>,
}

/// Neutral cgroup caps derived from [`pc::ResourceLimits`] — what a container
/// runtime (Docker `HostConfig`, k8s `resources.limits`) must apply. Extracted as a
/// pure value so the swap-escape pin and disk mapping are unit-testable without a
/// live daemon (the daemon-backed adapters just translate the fields).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupCaps {
    /// Hard memory cap, bytes.
    pub memory_bytes: Option<i64>,
    /// Swap ceiling, **pinned equal to `memory_bytes`** so a memory-capped container
    /// cannot escape the cap by swapping (awaken-next parity; in Docker's model
    /// `memory_swap == memory` disables swap beyond the memory limit).
    pub memory_swap_bytes: Option<i64>,
    /// CPU quota expressed in nano-CPUs (`cpu_millis * 1e6`).
    pub nano_cpus: Option<i64>,
    /// Max process/thread count.
    pub pids: Option<i64>,
    /// Writable-layer size limit for `--storage-opt size=` (as the runtime expects it).
    pub disk_size: Option<String>,
}

impl CgroupCaps {
    /// Map neutral limits onto container-runtime cgroup fields, pinning swap to the
    /// memory cap (the swap-escape close).
    #[must_use]
    pub fn from_limits(limits: &pc::ResourceLimits) -> Self {
        let memory = limits.memory_bytes.map(|m| m as i64);
        Self {
            memory_bytes: memory,
            memory_swap_bytes: memory,
            nano_cpus: limits.cpu_millis.map(|c| i64::from(c) * 1_000_000),
            pids: limits.pids.map(i64::from),
            disk_size: limits.disk_bytes.map(|d| d.to_string()),
        }
    }
}

#[cfg(test)]
mod cgroup_caps_tests {
    use super::*;

    #[test]
    fn pins_swap_to_the_memory_cap_and_maps_every_field() {
        let limits = pc::ResourceLimits {
            cpu_millis: Some(1500),
            memory_bytes: Some(512 * 1024 * 1024),
            pids: Some(256),
            disk_bytes: Some(2 * 1024 * 1024 * 1024),
        };
        let caps = CgroupCaps::from_limits(&limits);
        assert_eq!(caps.memory_bytes, Some(512 * 1024 * 1024));
        // The swap-escape close: swap ceiling == memory cap.
        assert_eq!(caps.memory_swap_bytes, caps.memory_bytes);
        assert_eq!(caps.nano_cpus, Some(1_500_000_000));
        assert_eq!(caps.pids, Some(256));
        assert_eq!(caps.disk_size.as_deref(), Some("2147483648"));
    }

    #[test]
    fn unset_limits_map_to_no_caps() {
        let caps = CgroupCaps::from_limits(&pc::ResourceLimits::default());
        assert_eq!(caps, CgroupCaps::default());
        assert!(caps.memory_swap_bytes.is_none());
    }

    #[test]
    fn unrestricted_egress_needs_no_proxy() {
        let r = egress_plan(&pc::NetworkPolicy::Unrestricted, None).unwrap();
        assert_eq!(r.network, NetworkMode::Open);
        assert!(r.proxy_env.is_empty());
    }

    #[test]
    fn no_egress_needs_no_proxy() {
        let r = egress_plan(&pc::NetworkPolicy::None, None).unwrap();
        assert_eq!(r.network, NetworkMode::None);
        assert!(r.proxy_env.is_empty());
    }

    #[test]
    fn allowlist_routes_through_the_brokered_proxy() {
        let proxy = EgressProxy {
            url: "http://gw.internal:8888".into(),
        };
        let policy = pc::NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        let r = egress_plan(&policy, Some(&proxy)).unwrap();
        assert_eq!(
            r.network,
            NetworkMode::Allowlist(vec!["api.anthropic.com".into()])
        );
        assert!(
            r.proxy_env
                .contains(&("HTTPS_PROXY".into(), "http://gw.internal:8888".into()))
        );
        assert!(
            r.proxy_env
                .contains(&("HTTP_PROXY".into(), "http://gw.internal:8888".into()))
        );
        assert!(r.proxy_env.iter().any(|(k, _)| k == "NO_PROXY"));
    }

    #[test]
    fn allowlist_without_a_proxy_fails_closed() {
        let policy = pc::NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        assert_eq!(egress_plan(&policy, None), Err(EgressError::ProxyRequired));
    }

    #[test]
    fn rootfs_plan_maps_image_and_isolated_roots() {
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::Image {
                reference: "ghcr.io/x:1".into()
            })
            .unwrap(),
            RootfsPlan::Image("ghcr.io/x:1".into())
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::Sandbox).unwrap(),
            RootfsPlan::HostUserland
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::IsolatedRoot {
                base: pc::RootfsSource::Dir {
                    path_template: "/roots/{scope}".into()
                },
                writable_base: true,
            })
            .unwrap(),
            RootfsPlan::RootDir {
                path_template: "/roots/{scope}".into(),
                writable: true,
            }
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::IsolatedRoot {
                base: pc::RootfsSource::Tarball {
                    reference: "blob://root".into()
                },
                writable_base: false,
            })
            .unwrap(),
            RootfsPlan::RootTarball {
                reference: "blob://root".into(),
                writable: false,
            }
        );
    }

    #[test]
    fn rootfs_plan_rejects_non_container_tiers() {
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::Scope),
            Err(RootfsError::NotAContainerRootfs)
        );
        assert_eq!(
            rootfs_plan(&pc::EnvironmentKind::LocalDir {
                path_template: "/tmp/{scope}".into()
            }),
            Err(RootfsError::NotAContainerRootfs)
        );
    }
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

/// The concrete rootfs a container/rootless-podman runtime must realize from a
/// declared [`pc::EnvironmentKind`]. Pure, so the (fork-exec, daemonless) adapter
/// only has to translate it into `podman run` flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsPlan {
    /// Borrow the default image's userland (no custom root).
    HostUserland,
    /// An OCI image reference.
    Image(String),
    /// A private root bound from a host directory template.
    RootDir {
        path_template: String,
        /// A writable base forces single-active use of the environment.
        writable: bool,
    },
    /// A private root unpacked from a tarball reference.
    RootTarball { reference: String, writable: bool },
}

/// Why a declared environment has no container-tier rootfs realization.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RootfsError {
    /// `Scope`/`LocalDir` are non-container tiers — the container provider must not
    /// silently run them as a borrowed-userland container.
    #[error("environment kind has no container-tier rootfs realization")]
    NotAContainerRootfs,
}

/// Map a declared environment kind onto its container-tier rootfs, fail-closed. A
/// `Scope`/`LocalDir` kind is rejected (those belong to the workdir/namespace tiers),
/// so the container provider never realizes an environment it was not asked for.
pub fn rootfs_plan(kind: &pc::EnvironmentKind) -> Result<RootfsPlan, RootfsError> {
    match kind {
        // The namespace/bwrap tier borrows the host userland; on the container tier
        // that means the default image supplies it.
        pc::EnvironmentKind::Sandbox => Ok(RootfsPlan::HostUserland),
        pc::EnvironmentKind::Image { reference } => Ok(RootfsPlan::Image(reference.clone())),
        pc::EnvironmentKind::IsolatedRoot {
            base,
            writable_base,
        } => Ok(match base {
            pc::RootfsSource::Dir { path_template } => RootfsPlan::RootDir {
                path_template: path_template.clone(),
                writable: *writable_base,
            },
            pc::RootfsSource::Tarball { reference } => RootfsPlan::RootTarball {
                reference: reference.clone(),
                writable: *writable_base,
            },
        }),
        pc::EnvironmentKind::Scope | pc::EnvironmentKind::LocalDir { .. } => {
            Err(RootfsError::NotAContainerRootfs)
        }
    }
}

/// A brokered egress chokepoint the sandbox routes through. The host allowlist is
/// enforced **at the proxy**, out of the sandbox — the same secretless-gateway route
/// used for model/MCP egress. The endpoint is supplied by the host's gateway; it is
/// never chosen inside the sandbox tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressProxy {
    /// The forward-proxy URL the agent's HTTP client must use (e.g. `http://gw:8888`).
    pub url: String,
}

/// How egress is realized for a container: the effective network mode plus any proxy
/// env the agent process must export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRealization {
    pub network: NetworkMode,
    /// `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` pairs — empty for `Open`/`None`.
    pub proxy_env: Vec<(String, String)>,
}

/// Why an egress policy cannot be realized on this tier.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EgressError {
    /// An allowlist policy needs a brokered chokepoint; none was supplied.
    #[error("allowlist egress requires a brokered proxy endpoint, none supplied")]
    ProxyRequired,
}

/// Realize an egress policy for the container tier, fail-closed. An `Allowlist` is
/// realized as a route through the brokered `proxy` (direct egress denied; all
/// traffic flows through the chokepoint that enforces the host allowlist). Without a
/// proxy an allowlist **cannot** be enforced, so it is rejected rather than silently
/// opened — the sandbox never runs believing egress is controlled when it is not.
pub fn egress_plan(
    policy: &pc::NetworkPolicy,
    proxy: Option<&EgressProxy>,
) -> Result<EgressRealization, EgressError> {
    match policy {
        pc::NetworkPolicy::Unrestricted => Ok(EgressRealization {
            network: NetworkMode::Open,
            proxy_env: Vec::new(),
        }),
        pc::NetworkPolicy::None => Ok(EgressRealization {
            network: NetworkMode::None,
            proxy_env: Vec::new(),
        }),
        pc::NetworkPolicy::Allowlist { hosts } => {
            let proxy = proxy.ok_or(EgressError::ProxyRequired)?;
            Ok(EgressRealization {
                network: NetworkMode::Allowlist(hosts.clone()),
                proxy_env: vec![
                    ("HTTPS_PROXY".to_string(), proxy.url.clone()),
                    ("HTTP_PROXY".to_string(), proxy.url.clone()),
                    // Keep loopback (the agent's own sidecars) direct.
                    ("NO_PROXY".to_string(), "localhost,127.0.0.1".to_string()),
                ],
            })
        }
    }
}

fn binds_of(spec: &pc::SandboxSpec) -> Vec<BindPlan> {
    spec.mounts
        .iter()
        // MemoryStore is not a host byte path — it is realized as a sidecar, not a bind.
        .filter(|m| !matches!(m.source, pc::MountSource::MemoryStore { .. }))
        .map(|m| BindPlan {
            source_ref: mount_ref(&m.source),
            mount_path: m.mount_path.clone(),
            read_only: m.access == pc::MountAccess::ReadOnly,
        })
        .collect()
}

/// The memory-store mounts a spec requests, pulled out of the byte-bind set so the
/// container tier realizes each as a memoryd sidecar + shared volume.
fn memory_mounts_of(spec: &pc::SandboxSpec) -> Vec<MemoryMount> {
    spec.mounts
        .iter()
        .filter_map(|m| match &m.source {
            pc::MountSource::MemoryStore { store_id } => Some(MemoryMount {
                store_id: store_id.clone(),
                mount_path: m.mount_path.clone(),
            }),
            _ => None,
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
        memory_mounts: memory_mounts_of(spec),
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
    /// The brokered egress chokepoint an `Allowlist` policy routes through. Without
    /// one, an allowlist spec fails closed at `create` (never silently opened).
    egress_proxy: Option<EgressProxy>,
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    pub fn new(runtime: Arc<R>, default_image: impl Into<String>) -> Self {
        Self {
            runtime,
            default_image: default_image.into(),
            egress_proxy: None,
        }
    }

    /// Route `Allowlist` egress through the brokered `proxy` (the secretless-gateway
    /// egress route). Without this, an allowlist spec is rejected at `create`.
    #[must_use]
    pub fn with_egress_proxy(mut self, proxy: EgressProxy) -> Self {
        self.egress_proxy = Some(proxy);
        self
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
        let mut plan = container_plan(spec, &self.default_image, &command);
        // Realize egress: an Allowlist policy is routed through the brokered proxy
        // (its env is injected here); without a proxy an allowlist fails closed.
        let egress = egress_plan(&spec.network, self.egress_proxy.as_ref())
            .map_err(|e| err(RuntimeError::Backend(e.to_string())))?;
        plan.env.extend(egress.proxy_env);
        let container_id = self.runtime.create(&spec.scope, &plan).await.map_err(err)?;
        // Report each mount's realization: a byte mount is a Bind, a memory store is
        // a sidecar-FUSE. Built from spec.mounts directly (binds no longer align 1:1
        // now that memory stores are pulled out into sidecars).
        let realized = spec
            .mounts
            .iter()
            .map(|m| pc::RealizedMount {
                mount_id: m.mount_id.clone(),
                mount_path: m.mount_path.clone(),
                access: m.access,
                realization: match m.source {
                    pc::MountSource::MemoryStore { .. } => pc::Realization::Fuse,
                    _ => pc::Realization::Bind,
                },
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

/// Remote-tier transport over `awaken-connection` (TCP dial + reverse dial). Gated
/// behind the `connection` feature so the default build pulls no network stack.
#[cfg(feature = "connection")]
pub mod net;

/// Real Docker backend (bollard). Gated behind the `docker` feature; compile-verified
/// here, running requires a Docker daemon.
#[cfg(feature = "docker")]
pub mod docker;

/// Real Kubernetes backend (kube). Gated behind the `k8s` feature; compile-verified
/// here, running requires a cluster.
#[cfg(feature = "k8s")]
pub mod k8s;

#[cfg(test)]
mod tests;
