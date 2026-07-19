//! Real Kubernetes backend (ADR-0041 Slice 5, `k8s` feature).
//!
//! Implements [`ContainerRuntime`] over **kube** — the kube-apiserver via the SDK,
//! never `kubectl`. Faithful to awaken-next's `K3sHandWorker` + this crate's
//! [`crate::pod_plan`]: **process-as-container** (the Pod's container command is the
//! agent, `restartPolicy: Never`), **native GC** (an `ownerReference` reaps orphans),
//! memory stores realized as **memoryd sidecars + emptyDir**, the untrusted agent
//! **hardened** (no SA token, dropped caps) + labeled for a NetworkPolicy, and the
//! stdio channel reached either by a direct **network dial** to the Service or, when
//! a rendezvous is set, by **reverse dial** (the egress-fenced Pod dials the host
//! out) — all via [`crate::net`]. Compile-verified here; running requires a cluster.

use std::net::SocketAddr;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport};
use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMap, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar, Pod,
    PodSpec, ResourceRequirements, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{DeleteParams, ListParams, PostParams};
use kube::{Api, Client};
use std::collections::BTreeMap;

use crate::net::TcpAgentTransport;
use crate::{BindPlan, ContainerPlan, ContainerRuntime, ContainerState, RuntimeError};

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

/// The single ConfigMap data key each inline-content mount is stored under; the Pod
/// projects it back to the mount's exact `mount_path` via a `subPath`.
const CONFIGMAP_KEY: &str = "content";

/// The Pod's inline single-file binds — those carrying self-contained bytes (`Inline` /
/// `Other{content}`, or a resolved File/Resource) rather than a host ref, whether UTF-8
/// (`content` → ConfigMap `data`) or binary (`content_bytes` → `binaryData`). Each becomes a
/// ConfigMap volume; the stable order names the i-th ConfigMap in both `create` and `build_pod`.
fn content_binds(plan: &ContainerPlan) -> Vec<&BindPlan> {
    plan.binds
        .iter()
        .filter(|b| b.content.is_some() || b.content_bytes.is_some())
        .collect()
}

/// Deterministic ConfigMap name for the i-th inline-content mount of Pod `awaken-{id}`.
fn configmap_name(id: &str, i: usize) -> String {
    format!("awaken-{id}-cfg-{i}")
}

/// Label the Pod's ConfigMaps carry so `remove` can reap them by selector — value is the
/// Pod name (`awaken-{id}`), which is also the `container_id` handed back to `remove`.
fn cfg_owner_label(id: &str) -> String {
    format!("awaken-{id}")
}

/// Build a ConfigMap holding one inline-content mount's bytes under [`CONFIGMAP_KEY`].
/// Immutable (the content is fixed at create) and owned by the same GC anchor as the Pod
/// so it is reaped natively when set; labeled for best-effort `remove` in the ownerless
/// case. ConfigMaps cap at ~1MiB — inline config (codex `config.toml`, resource bytes)
/// is well under, and larger byte payloads belong on the blob-store path, not here.
fn build_configmap(
    id: &str,
    i: usize,
    content: Option<&str>,
    content_bytes: Option<&[u8]>,
    owner: &Option<OwnerReference>,
) -> ConfigMap {
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "awaken-sandbox".to_string());
    labels.insert("awaken-cfg-owner".to_string(), cfg_owner_label(id));
    // UTF-8 content rides `data`; binary content rides `binaryData` (base64 on the wire) —
    // the volume subPath projects the same `content` key as a file either way.
    let (data, binary_data) = match (content, content_bytes) {
        (Some(text), _) => (
            Some(BTreeMap::from([(
                CONFIGMAP_KEY.to_string(),
                text.to_string(),
            )])),
            None,
        ),
        (None, Some(bytes)) => (
            None,
            Some(BTreeMap::from([(
                CONFIGMAP_KEY.to_string(),
                k8s_openapi::ByteString(bytes.to_vec()),
            )])),
        ),
        (None, None) => (Some(BTreeMap::new()), None),
    };
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(configmap_name(id, i)),
            labels: Some(labels),
            owner_references: owner.clone().map(|o| vec![o]),
            ..Default::default()
        },
        data,
        binary_data,
        immutable: Some(true),
    }
}

/// The Pod container's `resources.limits` from a spec's caps, or `None` when unset —
/// so the tier actually enforces the `resource_limits` it advertises. k8s expresses
/// caps natively (CPU millicores, byte quantities), distinct from Docker's cgroup
/// fields, so the mapping is per-backend. `pids` has no standard pod limit key.
fn pod_resources(limits: &pc::ResourceLimits) -> Option<ResourceRequirements> {
    if !limits.is_set() {
        return None;
    }
    let mut m = BTreeMap::new();
    if let Some(cpu) = limits.cpu_millis {
        m.insert("cpu".to_string(), Quantity(format!("{cpu}m")));
    }
    if let Some(mem) = limits.memory_bytes {
        m.insert("memory".to_string(), Quantity(mem.to_string()));
    }
    if let Some(disk) = limits.disk_bytes {
        m.insert("ephemeral-storage".to_string(), Quantity(disk.to_string()));
    }
    if m.is_empty() {
        return None; // only `pids` was set — nothing k8s expresses as a pod limit
    }
    Some(ResourceRequirements {
        limits: Some(m),
        ..Default::default()
    })
}

/// The neutral limit k8s cannot enforce at the Pod-spec level, if the spec asks for it.
/// k8s expresses CPU / memory / ephemeral-storage as Pod `resources.limits`, but a
/// per-Pod `pids` cap is a node/kubelet setting (`--pod-max-pids`), NOT a Pod-spec
/// field — so `pod_resources` cannot carry it. The container tier advertises
/// `resource_limits = true` for every backend; on k8s a `pids`-limited spec must
/// therefore FAIL CLOSED at create rather than be silently dropped (a fail-open on a
/// fork-bomb guard), the same discipline as the neutral admission gate.
fn unenforceable_k8s_limit(limits: &pc::ResourceLimits) -> Option<&'static str> {
    limits.pids.map(|_| "pids")
}

/// A Kubernetes-backed [`ContainerRuntime`]. `agent_addr` is the Service endpoint the
/// runtime dials for the [`AgentChannel`]; `owner` (optional) is the GC owner.
pub struct K8sRuntime {
    client: Client,
    namespace: String,
    agent_addr: SocketAddr,
    owner: Option<OwnerReference>,
    /// The image of the memoryd sidecar that FUSE-serves a memory store into the
    /// shared volume the agent reads (ADR-0038 MemoryStore, in-pod realization).
    memoryd_image: String,
    /// When set, the host binds this address as a **reverse-dial rendezvous**: the
    /// Pod dials *out* to it (no inbound, no Service, fully egress-fenced) and the
    /// address is injected into the agent as `AWAKEN_ACP_RENDEZVOUS`. When `None`,
    /// the host direct-dials `agent_addr` (a published Service) instead.
    rendezvous: Option<SocketAddr>,
    /// Whether the memoryd sidecar FUSE-mounts the store (needs `SYS_ADMIN` +
    /// `/dev/fuse` on the node). `false` (the default) runs the sidecar in **copy
    /// mode** — unprivileged; it materializes the store into the shared volume and
    /// harvests writes back on teardown (ADR-0053 D6) — so a cluster without FUSE
    /// still works, just without live write-through.
    memoryd_fuse: bool,
}

/// Default memoryd sidecar image (overridable via [`K8sRuntime::with_memoryd_image`]).
/// The image is the execution-plane `awaken-sandbox` binary in its `memoryd` role
/// (ENTRYPOINT `awaken-sandbox memoryd`, built with `--features memoryd`), packaged by
/// `deploy/images/sandbox/Dockerfile.memoryd`. The sidecar sets no `command`, so the
/// role reads the `AWAKEN_MEMORY_*` env this plan injects below.
const DEFAULT_MEMORYD_IMAGE: &str = "ghcr.io/awaken/memoryd:latest";

impl K8sRuntime {
    /// Connect via in-cluster ServiceAccount or the ambient kubeconfig.
    pub async fn connect(
        namespace: impl Into<String>,
        agent_addr: SocketAddr,
    ) -> Result<Self, RuntimeError> {
        // kube's rustls client needs a process-level CryptoProvider; install ring
        // once (idempotent — a prior install by the host is fine).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::try_default().await.map_err(backend)?;
        Ok(Self {
            client,
            namespace: namespace.into(),
            agent_addr,
            owner: None,
            memoryd_image: DEFAULT_MEMORYD_IMAGE.to_string(),
            rendezvous: None,
            memoryd_fuse: false,
        })
    }

    /// Run the memoryd sidecar in FUSE mode (grants it `SYS_ADMIN`). Off by default:
    /// the portable, unprivileged **copy** fallback is used unless the cluster is
    /// known to support FUSE.
    #[must_use]
    pub fn with_memoryd_fuse(mut self, fuse: bool) -> Self {
        self.memoryd_fuse = fuse;
        self
    }

    /// Set the GC owner (e.g. a Lease/ConfigMap) whose deletion reaps orphan Pods.
    #[must_use]
    pub fn with_owner(mut self, owner: OwnerReference) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Use a reverse-dial rendezvous at `addr` instead of direct-dialing the Service:
    /// the Pod dials out to it (egress-only, no inbound) and the host accepts.
    #[must_use]
    pub fn with_rendezvous(mut self, addr: SocketAddr) -> Self {
        self.rendezvous = Some(addr);
        self
    }

    /// Override the memoryd sidecar image.
    #[must_use]
    pub fn with_memoryd_image(mut self, image: impl Into<String>) -> Self {
        self.memoryd_image = image.into();
        self
    }

    /// A runtime backed by a **lazy** client (no cluster dial), for unit-testing the
    /// builder + Pod-assembly paths; the live `create`/`wait`/… methods still need a
    /// real apiserver (exercised by the gated `k8s_it` integration test).
    #[cfg(test)]
    fn for_test(agent_addr: SocketAddr) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = kube::Config::new("http://127.0.0.1:1/".parse().unwrap());
        let client = Client::try_from(config).expect("lazy kube client builds without a cluster");
        Self {
            client,
            namespace: "default".into(),
            agent_addr,
            owner: None,
            memoryd_image: DEFAULT_MEMORYD_IMAGE.to_string(),
            rendezvous: None,
            memoryd_fuse: false,
        }
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn configmaps(&self) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Probe the apiserver (for tests / health checks): `Ok` iff it responds.
    pub async fn ping(&self) -> Result<(), RuntimeError> {
        self.pods()
            .list(&ListParams::default().limit(1))
            .await
            .map(|_| ())
            .map_err(backend)
    }

    fn pod(&self, id: &str, plan: &ContainerPlan) -> Pod {
        let rendezvous = self.rendezvous.map(|a| a.to_string());
        build_pod(
            id,
            plan,
            &self.owner,
            &self.memoryd_image,
            rendezvous.as_deref(),
            self.memoryd_fuse,
        )
    }
}

/// Build the Pod object from a plan (pure — no client/cluster), so the multi-container
/// sidecar/volume shape and resource limits are unit-testable without a cluster.
/// `rendezvous`, when set, is injected as `AWAKEN_ACP_RENDEZVOUS` so the in-pod agent
/// dials the host out (reverse-dial) instead of listening for an inbound connection.
fn build_pod(
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
    memoryd_image: &str,
    rendezvous: Option<&str>,
    memoryd_fuse: bool,
) -> Pod {
    {
        let mut agent_env: Vec<EnvVar> = plan
            .env
            .iter()
            .map(|(k, v)| EnvVar {
                name: k.clone(),
                value: Some(v.clone()),
                value_from: None,
            })
            .collect();
        if let Some(addr) = rendezvous {
            agent_env.push(EnvVar {
                name: "AWAKEN_ACP_RENDEZVOUS".into(),
                value: Some(addr.to_string()),
                value_from: None,
            });
        }

        // Each memory store → a pod-scoped emptyDir + a memoryd sidecar that serves
        // the store into it; the agent container mounts the same volume and reads it
        // as files. The privileged FUSE lives in the minimal sidecar, never the agent.
        let mut volumes: Vec<Volume> = Vec::new();
        let mut agent_mounts: Vec<VolumeMount> = Vec::new();
        let mut sidecars: Vec<Container> = Vec::new();
        for (i, mm) in plan.memory_mounts.iter().enumerate() {
            let vol = format!("mem-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            let mount = VolumeMount {
                name: vol.clone(),
                mount_path: mm.mount_path.clone(),
                ..Default::default()
            };
            agent_mounts.push(mount.clone());
            sidecars.push(Container {
                name: format!("memoryd-{i}"),
                image: Some(memoryd_image.to_string()),
                env: Some(vec![
                    EnvVar {
                        name: "AWAKEN_MEMORY_STORE_ID".into(),
                        value: Some(mm.store_id.clone()),
                        value_from: None,
                    },
                    EnvVar {
                        name: "AWAKEN_MOUNT_PATH".into(),
                        value: Some(mm.mount_path.clone()),
                        value_from: None,
                    },
                    // The sidecar reads this to FUSE-mount or fall back to copy.
                    EnvVar {
                        name: "AWAKEN_MEMORY_MODE".into(),
                        value: Some(if memoryd_fuse { "fuse" } else { "copy" }.into()),
                        value_from: None,
                    },
                ]),
                volume_mounts: Some(vec![mount]),
                // FUSE needs SYS_ADMIN; copy mode stays unprivileged (portable, so a
                // cluster without /dev/fuse still serves the store, via copy+harvest).
                security_context: memoryd_fuse.then(fuse_sidecar_security_context),
                ..Default::default()
            });
        }

        // Under the read-only rootfs, the writable app paths (outputs + scratch /tmp)
        // are backed by pod-scoped emptyDir volumes so the agent can still write.
        for (i, dir) in crate::writable_dirs(plan).into_iter().enumerate() {
            let vol = format!("rw-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: vol,
                mount_path: dir,
                ..Default::default()
            });
        }

        // Inline content (codex `config.toml`, ADR-0038 resource bytes) has no host path a
        // Pod can bind — each is realized as a ConfigMap volume (the ConfigMaps are created
        // alongside the Pod in `create`) and projected read-only as a single file at its
        // exact `mount_path` via `subPath`, so the interior layout matches the bwrap tier.
        for (i, bind) in content_binds(plan).iter().enumerate() {
            let vol = format!("cfg-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                config_map: Some(ConfigMapVolumeSource {
                    name: configmap_name(id, i),
                    ..Default::default()
                }),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: vol,
                mount_path: bind.mount_path.clone(),
                sub_path: Some(CONFIGMAP_KEY.to_string()),
                read_only: Some(bind.read_only),
                ..Default::default()
            });
        }

        let mut containers = vec![Container {
            name: "agent".into(),
            image: Some(plan.image.clone()),
            // process-as-container: the agent argv is the container command.
            command: Some(plan.command.clone()),
            env: Some(agent_env),
            // Enforce the advertised resource caps as the container's limits.
            resources: pod_resources(&plan.limits),
            volume_mounts: (!agent_mounts.is_empty()).then_some(agent_mounts),
            // Harden the untrusted agent: no privilege escalation, all caps dropped.
            security_context: Some(hardened_security_context()),
            ..Default::default()
        }];
        containers.extend(sidecars);

        // Label the Pod with its egress posture so a platform-managed NetworkPolicy
        // selects and enforces it (deny-all-except-proxy/DNS for `restricted`). The
        // per-cluster NetworkPolicy holds the real proxy/DNS addresses; the adapter
        // only declares the posture — it does not invent IPs it doesn't have.
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("app".to_string(), "awaken-sandbox".to_string());
        labels.insert(
            "awaken-egress".to_string(),
            egress_label(&plan.network).to_string(),
        );

        Pod {
            metadata: ObjectMeta {
                name: Some(format!("awaken-{id}")),
                labels: Some(labels),
                // native GC: the platform reaps this Pod when the owner is deleted.
                owner_references: owner.clone().map(|o| vec![o]),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers,
                volumes: (!volumes.is_empty()).then_some(volumes),
                // a finished agent Pod is reaped, not looped.
                restart_policy: Some("Never".into()),
                // The untrusted agent must NOT reach the kube API (no SA token): its
                // only control-plane channel is the ACP data channel, nothing else.
                automount_service_account_token: Some(false),
                ..Default::default()
            }),
            status: None,
        }
    }
}

/// The egress-posture label value a platform NetworkPolicy selects on: `restricted`
/// for any non-`Open` policy (routed through the proxy chokepoint), else `open`.
fn egress_label(network: &crate::NetworkMode) -> &'static str {
    match network {
        crate::NetworkMode::Open => "open",
        crate::NetworkMode::Allowlist(_) | crate::NetworkMode::None => "restricted",
    }
}

/// Host side of the reverse-dial: bind the rendezvous and accept the Pod's outbound
/// connection, returning it as the agent channel. Extracted so it is testable with a
/// stand-in dialer (no cluster).
async fn accept_reverse(addr: SocketAddr) -> Result<Box<dyn AgentChannel>, RuntimeError> {
    use awaken_connection::ListenSide;
    let listen = crate::net::ReverseListen::bind(addr)
        .await
        .map_err(backend)?;
    let chan = listen.accept().await.map_err(backend)?;
    Ok(Box::new(chan))
}

/// The memoryd sidecar's `securityContext` in FUSE mode: it needs `SYS_ADMIN` to
/// mount `/dev/fuse`. Only granted when FUSE is enabled — the copy fallback needs no
/// privilege, so a locked-down (no-FUSE) cluster runs the sidecar unprivileged.
fn fuse_sidecar_security_context() -> SecurityContext {
    SecurityContext {
        capabilities: Some(Capabilities {
            add: Some(vec!["SYS_ADMIN".to_string()]),
            drop: None,
        }),
        ..Default::default()
    }
}

/// The hardened `securityContext` for the untrusted agent container: no privilege
/// escalation, every Linux capability dropped.
fn hardened_security_context() -> SecurityContext {
    SecurityContext {
        allow_privilege_escalation: Some(false),
        read_only_root_filesystem: Some(true),
        capabilities: Some(Capabilities {
            drop: Some(vec!["ALL".to_string()]),
            add: None,
        }),
        ..Default::default()
    }
}

#[async_trait]
impl ContainerRuntime for K8sRuntime {
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        // Fail closed on a limit k8s cannot enforce at the Pod-spec level (pids), rather
        // than silently placing the spec and dropping the cap — the tier advertises
        // `resource_limits`, so honoring it means refusing what it cannot enforce.
        if let Some(limit) = unenforceable_k8s_limit(&plan.limits) {
            return Err(RuntimeError::Backend(format!(
                "k8s cannot enforce a per-Pod `{limit}` limit (it is a node/kubelet \
                 setting, not a Pod-spec field); refusing to place a `{limit}`-limited \
                 spec on the k8s tier rather than silently dropping the cap"
            )));
        }
        // Realize inline-content mounts as ConfigMaps *before* the Pod: the Pod's volumes
        // reference them by name, and the kubelet blocks the Pod as `ContainerCreating`
        // until they exist. Ordered + named identically to `build_pod`'s projection.
        let cms = self.configmaps();
        for (i, bind) in content_binds(plan).iter().enumerate() {
            let cm = build_configmap(
                id,
                i,
                bind.content.as_deref(),
                bind.content_bytes.as_deref(),
                &self.owner,
            );
            cms.create(&PostParams::default(), &cm)
                .await
                .map_err(backend)?;
        }
        let pod = self.pod(id, plan);
        let created = self
            .pods()
            .create(&PostParams::default(), &pod)
            .await
            .map_err(backend)?;
        created
            .metadata
            .name
            .ok_or_else(|| backend("created pod has no name"))
    }

    async fn open_channel(
        &self,
        _container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        match self.rendezvous {
            // Reverse-dial: the host listens, the egress-fenced Pod dials out to us.
            Some(addr) => accept_reverse(addr).await,
            // Direct-dial the agent's stdio over its published Service.
            None => TcpAgentTransport::new(self.agent_addr)
                .open_channel()
                .await
                .map_err(backend),
        }
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        let phase = pod.status.and_then(|s| s.phase).unwrap_or_default();
        Ok(if phase == "Running" || phase == "Pending" {
            ContainerState::Running
        } else {
            ContainerState::Gone
        })
    }

    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        // Compile path: read the terminated exit code. A running deployment watches
        // the Pod to completion instead of a single read.
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        let code = pod
            .status
            .and_then(|s| s.container_statuses)
            .and_then(|c| c.into_iter().next())
            .and_then(|cs| cs.state)
            .and_then(|st| st.terminated)
            .map(|t| t.exit_code);
        match code {
            Some(code) => Ok(pc::ExitStatus {
                code: Some(code),
                signaled: false,
            }),
            None => Err(backend("agent pod has not terminated")),
        }
    }

    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        match self.inspect(container_id).await? {
            ContainerState::Running => Ok(None),
            ContainerState::Gone => Ok(Some(pc::ExitStatus {
                code: None,
                signaled: true,
            })),
        }
    }

    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        // K8s has no per-process signal; deletion is the reap. SIGKILL → grace 0.
        let params = match signal {
            pc::Signal::Kill => DeleteParams::default().grace_period(0),
            _ => DeleteParams::default(),
        };
        self.pods()
            .delete(container_id, &params)
            .await
            .map(|_| ())
            .map_err(backend)
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        // Out-of-band: read from the outputs PVC, not through the API server.
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        _container_id: &str,
        _artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        Err(backend(
            "k8s artifacts are read out-of-band from the outputs PVC",
        ))
    }

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        // Native GC: the owner Lease's renewTime is patched by the owner controller;
        // ownerReference + TTL reaps orphans (no bespoke reaper here).
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        // Reap the Pod's inline-content ConfigMaps too. Owner GC covers the owned case;
        // this best-effort sweep (label = the Pod name) covers the ownerless dev/e2e case
        // so inline-content maps don't leak. It precedes the Pod delete and never fails it.
        let selector = format!("awaken-cfg-owner={container_id}");
        let _ = self
            .configmaps()
            .delete_collection(
                &DeleteParams::default(),
                &ListParams::default().labels(&selector),
            )
            .await;
        self.pods()
            .delete(container_id, &DeleteParams::default())
            .await
            .map(|_| ())
            .map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn builder_methods_set_every_field_and_pod_delegates_to_build_pod() {
        let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
            .with_memoryd_fuse(true)
            .with_owner(OwnerReference::default())
            .with_rendezvous("127.0.0.1:7000".parse().unwrap())
            .with_memoryd_image("custom/memoryd:1");
        assert!(rt.memoryd_fuse);
        assert!(rt.owner.is_some());
        assert_eq!(rt.rendezvous, Some("127.0.0.1:7000".parse().unwrap()));
        assert_eq!(rt.memoryd_image, "custom/memoryd:1");
        // pod() threads the builder state into build_pod and the Api handle builds.
        let pod = rt.pod("s1", &plan_with_memory(vec![]));
        assert!(pod.metadata.name.is_some());
        assert!(pod.spec.is_some());
        let _ = rt.pods();
    }

    #[tokio::test]
    async fn artifacts_are_out_of_band_and_touch_lease_is_a_noop() {
        let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        assert!(rt.artifacts("pod").await.unwrap().is_empty());
        assert!(rt.touch_lease("pod").await.is_ok());
    }

    #[test]
    fn pod_resources_maps_cpu_memory_and_disk() {
        let r = pod_resources(&pc::ResourceLimits {
            cpu_millis: Some(1500),
            memory_bytes: Some(1_073_741_824),
            pids: Some(256),
            disk_bytes: Some(2048),
        })
        .expect("limits are set");
        let limits = r.limits.expect("limits map present");
        assert_eq!(limits.get("cpu").unwrap().0, "1500m");
        assert_eq!(limits.get("memory").unwrap().0, "1073741824");
        assert_eq!(limits.get("ephemeral-storage").unwrap().0, "2048");
        // pids has no standard pod-level key.
        assert!(!limits.contains_key("pids"));
    }

    #[test]
    fn pod_resources_is_none_without_expressible_caps() {
        assert!(pod_resources(&pc::ResourceLimits::default()).is_none());
        // pids-only → nothing k8s expresses as a pod limit.
        assert!(
            pod_resources(&pc::ResourceLimits {
                pids: Some(9),
                ..Default::default()
            })
            .is_none()
        );
    }

    #[test]
    fn a_pids_limit_is_flagged_unenforceable_on_k8s_so_create_fails_closed() {
        // pids is the one limit k8s cannot express at the Pod spec — flagged so `create`
        // refuses it rather than silently dropping the cap (C6: no fail-open on a
        // fork-bomb guard). CPU/memory/disk are enforceable, so they are NOT flagged.
        assert_eq!(
            unenforceable_k8s_limit(&pc::ResourceLimits {
                pids: Some(64),
                ..Default::default()
            }),
            Some("pids")
        );
        assert_eq!(
            unenforceable_k8s_limit(&pc::ResourceLimits {
                cpu_millis: Some(1000),
                memory_bytes: Some(1 << 30),
                disk_bytes: Some(1 << 20),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            unenforceable_k8s_limit(&pc::ResourceLimits::default()),
            None
        );
    }

    fn plan_with_memory(mounts: Vec<crate::MemoryMount>) -> ContainerPlan {
        ContainerPlan {
            image: "agent:1".into(),
            command: vec!["claude".into(), "--acp".into()],
            env: vec![("TZ".into(), "UTC".into())],
            binds: Vec::new(),
            outputs_volume: "/mnt/session/outputs".into(),
            network: crate::NetworkMode::Open,
            limits: pc::ResourceLimits {
                memory_bytes: Some(1 << 30),
                ..Default::default()
            },
            memory_mounts: mounts,
            rootfs: crate::RootfsPlan::HostUserland,
        }
    }

    #[test]
    fn build_pod_realizes_memory_mounts_as_sidecars_and_shared_volumes() {
        let plan = plan_with_memory(vec![
            crate::MemoryMount {
                store_id: "s1".into(),
                mount_path: "/workspace/.mnt/a".into(),
            },
            crate::MemoryMount {
                store_id: "s2".into(),
                mount_path: "/workspace/.mnt/b".into(),
            },
        ]);
        let pod = build_pod("run-1", &plan, &None, "memoryd:9", None, false);
        let spec = pod.spec.unwrap();

        // agent + one memoryd sidecar per memory store.
        assert_eq!(spec.containers.len(), 3);
        assert_eq!(spec.containers[0].name, "agent");
        assert_eq!(
            spec.containers
                .iter()
                .filter(|c| c.name.starts_with("memoryd-"))
                .count(),
            2
        );
        // one pod-scoped emptyDir per store, PLUS the 2 writable-rootfs dirs (outputs+/tmp).
        let volumes = spec.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 2 + 2);
        assert!(volumes.iter().all(|v| v.empty_dir.is_some()));
        // the agent mounts both memory volumes + the writable dirs + carries limits.
        let agent = &spec.containers[0];
        assert_eq!(agent.volume_mounts.as_ref().unwrap().len(), 2 + 2);
        assert!(agent.resources.is_some());
        // the sidecar names the store + mount path + image (the privilege lives here).
        let sc = spec
            .containers
            .iter()
            .find(|c| c.name == "memoryd-0")
            .unwrap();
        assert_eq!(sc.image.as_deref(), Some("memoryd:9"));
        let env = sc.env.as_ref().unwrap();
        assert!(
            env.iter()
                .any(|e| e.name == "AWAKEN_MEMORY_STORE_ID" && e.value.as_deref() == Some("s1"))
        );
        assert!(
            env.iter().any(|e| e.name == "AWAKEN_MOUNT_PATH"
                && e.value.as_deref() == Some("/workspace/.mnt/a"))
        );
    }

    #[test]
    fn build_pod_projects_inline_content_as_configmap_subpath_volumes() {
        // Inline-content binds (codex config.toml, ADR-0038 resource bytes) have no host
        // path a Pod can bind — build_pod must project each as a ConfigMap volume mounted
        // read-only at its exact mount_path via subPath, named to match `create`'s CMs.
        let mut plan = plan_with_memory(Vec::new());
        plan.binds = vec![
            crate::BindPlan {
                source_ref: String::new(),
                mount_path: "/acp-config/config.toml".into(),
                read_only: true,
                content: Some("[mcp_servers.gh]\nx\n".into()),
                content_bytes: None,
            },
            // A ref-backed bind (no content) must NOT become a ConfigMap volume.
            crate::BindPlan {
                source_ref: "blob-123".into(),
                mount_path: "/data/in".into(),
                read_only: true,
                content: None,
                content_bytes: None,
            },
        ];
        let spec = build_pod("run-9", &plan, &None, "m", None, false)
            .spec
            .unwrap();

        // One ConfigMap volume (only the content bind), named awaken-{id}-cfg-0, plus the
        // 2 writable-rootfs emptyDirs. The ref-backed bind adds nothing on this tier.
        let volumes = spec.volumes.as_ref().unwrap();
        let cfg = volumes
            .iter()
            .find(|v| v.config_map.is_some())
            .expect("a configmap volume");
        assert_eq!(cfg.name, "cfg-0");
        assert_eq!(cfg.config_map.as_ref().unwrap().name, "awaken-run-9-cfg-0");
        assert_eq!(volumes.iter().filter(|v| v.config_map.is_some()).count(), 1);

        // The agent mounts it as a single file at the exact path (subPath = the CM key).
        let agent = &spec.containers[0];
        let m = agent
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "cfg-0")
            .expect("the configmap mount");
        assert_eq!(m.mount_path, "/acp-config/config.toml");
        assert_eq!(m.sub_path.as_deref(), Some("content"));
        assert_eq!(m.read_only, Some(true));
    }

    #[test]
    fn build_configmap_holds_the_bytes_under_the_key_and_is_immutable() {
        let cm = build_configmap("run-9", 0, Some("hello-inline"), None, &None);
        assert_eq!(cm.metadata.name.as_deref(), Some("awaken-run-9-cfg-0"));
        assert_eq!(
            cm.data.as_ref().unwrap().get("content").unwrap(),
            "hello-inline"
        );
        assert_eq!(cm.immutable, Some(true));
        // Labeled so `remove` reaps it by the Pod name (the container_id).
        assert_eq!(
            cm.metadata
                .labels
                .as_ref()
                .unwrap()
                .get("awaken-cfg-owner")
                .map(String::as_str),
            Some("awaken-run-9")
        );
    }

    #[test]
    fn build_configmap_uses_binary_data_for_non_utf8_bytes() {
        // A binary File (non-UTF-8) rides `binaryData`, not text `data` — else the k8s API
        // rejects the invalid UTF-8. The same `content` key is projected by the volume subPath.
        let cm = build_configmap("run-9", 1, None, Some(&[0xff, 0xfe, 0x00, 0x01]), &None);
        assert!(cm.data.is_none(), "binary content must not ride text data");
        assert_eq!(
            cm.binary_data.as_ref().unwrap().get("content").unwrap().0,
            vec![0xff, 0xfe, 0x00, 0x01]
        );
        assert_eq!(cm.immutable, Some(true));
    }

    #[test]
    fn build_pod_carries_the_brokered_lease_token_env_not_a_raw_key() {
        // D-R2: the host injects the gateway + short-lived lease token into plan.env
        // (via acp_provision → spec.env). The k8s pod must carry those to the agent so
        // it reaches the model through the egress proxy — and never a raw provider key.
        let mut plan = plan_with_memory(Vec::new());
        plan.env = vec![
            ("AWAKEN_ACP_GATEWAY_URL".into(), "http://gw.internal".into()),
            ("AWAKEN_ACP_LEASE_TOKEN".into(), "lease-abc".into()),
            ("HTTPS_PROXY".into(), "http://gw.internal:8888".into()),
        ];
        let spec = build_pod("r", &plan, &None, "m", None, false).spec.unwrap();
        let env = spec.containers[0].env.clone().unwrap();
        assert!(
            env.iter()
                .any(|e| e.name == "AWAKEN_ACP_LEASE_TOKEN"
                    && e.value.as_deref() == Some("lease-abc"))
        );
        assert!(env.iter().any(|e| e.name == "AWAKEN_ACP_GATEWAY_URL"));
        // The sandbox holds no raw provider key.
        assert!(env.iter().all(|e| e.name != "ANTHROPIC_API_KEY"));
    }

    #[test]
    fn build_pod_without_memory_mounts_is_a_single_container() {
        let pod = build_pod("r", &plan_with_memory(Vec::new()), &None, "m", None, false);
        let spec = pod.spec.unwrap();
        // No memoryd sidecar, but the agent still gets the 2 writable-rootfs emptyDirs.
        assert_eq!(spec.containers.len(), 1);
        assert_eq!(spec.volumes.as_ref().unwrap().len(), 2);
        assert_eq!(spec.containers[0].volume_mounts.as_ref().unwrap().len(), 2);
    }

    fn memoryd_sidecar(spec: &PodSpec) -> &Container {
        spec.containers
            .iter()
            .find(|c| c.name == "memoryd-0")
            .unwrap()
    }

    #[test]
    fn memoryd_sidecar_copy_mode_is_the_unprivileged_default() {
        // No-FUSE cluster: the sidecar runs copy mode with NO SYS_ADMIN, so a locked
        // -down node still serves the store (materialize + harvest).
        let plan = plan_with_memory(vec![crate::MemoryMount {
            store_id: "s1".into(),
            mount_path: "/workspace/.mnt/a".into(),
        }]);
        let spec = build_pod("r", &plan, &None, "m", None, false).spec.unwrap();
        let sc = memoryd_sidecar(&spec);
        let env = sc.env.as_ref().unwrap();
        assert!(
            env.iter()
                .any(|e| e.name == "AWAKEN_MEMORY_MODE" && e.value.as_deref() == Some("copy"))
        );
        assert!(
            sc.security_context.is_none(),
            "copy-mode sidecar must be unprivileged (no /dev/fuse needed)"
        );
    }

    #[test]
    fn memoryd_sidecar_fuse_mode_grants_sys_admin() {
        let plan = plan_with_memory(vec![crate::MemoryMount {
            store_id: "s1".into(),
            mount_path: "/workspace/.mnt/a".into(),
        }]);
        // FUSE opt-in: the sidecar is told fuse mode and granted SYS_ADMIN for /dev/fuse.
        let spec = build_pod("r", &plan, &None, "m", None, true).spec.unwrap();
        let sc = memoryd_sidecar(&spec);
        let env = sc.env.as_ref().unwrap();
        assert!(
            env.iter()
                .any(|e| e.name == "AWAKEN_MEMORY_MODE" && e.value.as_deref() == Some("fuse"))
        );
        let caps = sc
            .security_context
            .as_ref()
            .unwrap()
            .capabilities
            .as_ref()
            .unwrap();
        assert_eq!(caps.add.as_deref(), Some(&["SYS_ADMIN".to_string()][..]));
    }

    #[test]
    fn build_pod_hardens_the_untrusted_agent() {
        let spec = build_pod("r", &plan_with_memory(Vec::new()), &None, "m", None, false)
            .spec
            .unwrap();
        // No SA token → the agent cannot reach the kube API.
        assert_eq!(spec.automount_service_account_token, Some(false));
        let sc = spec.containers[0].security_context.as_ref().unwrap();
        assert_eq!(sc.allow_privilege_escalation, Some(false));
        assert_eq!(sc.read_only_root_filesystem, Some(true));
        assert_eq!(
            sc.capabilities.as_ref().unwrap().drop.as_deref(),
            Some(&["ALL".to_string()][..])
        );
        // The writable app paths (outputs + /tmp) are backed by emptyDir mounts so a
        // read-only rootfs doesn't break the agent's writes.
        let mounts = spec.containers[0].volume_mounts.as_ref().unwrap();
        let paths: Vec<&str> = mounts.iter().map(|m| m.mount_path.as_str()).collect();
        assert!(paths.contains(&"/mnt/session/outputs"));
        assert!(paths.contains(&"/tmp"));
    }

    #[test]
    fn build_pod_injects_the_reverse_dial_rendezvous() {
        let plan = plan_with_memory(Vec::new());
        // Without a rendezvous, no such env.
        let no_rv = build_pod("r", &plan, &None, "m", None, false);
        let env0 = no_rv.spec.unwrap().containers[0].env.clone().unwrap();
        assert!(env0.iter().all(|e| e.name != "AWAKEN_ACP_RENDEZVOUS"));
        // With one, the agent is told where to dial out.
        let with_rv = build_pod("r", &plan, &None, "m", Some("10.0.0.5:9000"), false);
        let env1 = with_rv.spec.unwrap().containers[0].env.clone().unwrap();
        assert!(
            env1.iter().any(|e| e.name == "AWAKEN_ACP_RENDEZVOUS"
                && e.value.as_deref() == Some("10.0.0.5:9000"))
        );
    }

    #[tokio::test]
    async fn accept_reverse_receives_the_pods_outbound_dial() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Bind an ephemeral rendezvous; a stand-in "pod" dials it and the host accepts.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // free the port for accept_reverse to bind

        let host = tokio::spawn(async move { accept_reverse(addr).await });
        // Give the host a moment to bind, then dial like an egress-fenced pod would.
        let mut pod = loop {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(s) => break s,
                Err(_) => tokio::task::yield_now().await,
            }
        };
        pod.write_all(b"ping\n").await.unwrap();
        pod.flush().await.unwrap();

        let mut chan = host.await.unwrap().unwrap();
        let mut buf = [0u8; 5];
        chan.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping\n");
    }

    #[test]
    fn build_pod_labels_the_egress_posture_for_a_networkpolicy() {
        // Restricted egress → the pod is labeled so a platform NetworkPolicy fences it.
        let mut plan = plan_with_memory(Vec::new());
        plan.network = crate::NetworkMode::Allowlist(vec!["api.anthropic.com".into()]);
        let pod = build_pod("r", &plan, &None, "m", None, false);
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(
            labels.get("awaken-egress").map(String::as_str),
            Some("restricted")
        );

        plan.network = crate::NetworkMode::Open;
        let open = build_pod("r", &plan, &None, "m", None, false);
        assert_eq!(
            open.metadata
                .labels
                .unwrap()
                .get("awaken-egress")
                .map(String::as_str),
            Some("open")
        );

        // No-network policy is also `restricted`, never `open` — a fail-open label
        // here would let a NetworkPolicy grant egress to a pod that asked for none.
        plan.network = crate::NetworkMode::None;
        let denied = build_pod("r", &plan, &None, "m", None, false);
        assert_eq!(
            denied
                .metadata
                .labels
                .unwrap()
                .get("awaken-egress")
                .map(String::as_str),
            Some("restricted")
        );
    }
}
