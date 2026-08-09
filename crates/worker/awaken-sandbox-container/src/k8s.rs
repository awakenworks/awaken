//! Real Kubernetes backend (ADR-0041 Slice 5, `k8s` feature).
//!
//! Implements [`ContainerRuntime`] over **kube** — the kube-apiserver via the SDK,
//! never `kubectl`. Faithful to awaken-next's `K3sHandWorker` + this crate's
//! [`K8sRuntime`]: a **Session-owned Pod** (PID 1 retains its namespaces while
//! attempts run through attached exec, `restartPolicy: Never`), **native GC** (an
//! `ownerReference` reaps orphans),
//! memory stores realized as **memoryd sidecars + emptyDir**, managed input Files
//! exposed through a **read-only shared volume + isolated projector sidecar**, the
//! untrusted agent
//! **hardened** (no SA token, dropped caps) + labeled for a NetworkPolicy, and the
//! stdio channel reached either by a direct **network dial** to the Service or, when
//! a rendezvous is set, by **reverse dial** (the egress-fenced Pod dials the host
//! out) — all via [`crate::net`]. Compile-verified here; running requires a cluster.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport, SplitChannel};
use awaken_provisioning_contract as pc;
use k8s_openapi::api::core::v1::{
    Capabilities, ConfigMap, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar,
    LocalObjectReference, Pod, PodSecurityContext, PodSpec, ResourceRequirements, Secret,
    SecretVolumeSource, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference, Status};
use kube::api::{AttachParams, DeleteParams, ListParams};
use kube::{Api, Client};
use std::collections::BTreeMap;
use tokio::io::AsyncReadExt;

use crate::net::TcpAgentTransport;
use crate::{
    BindPlan, ContainerPlan, ContainerRuntime, ContainerState, RuntimeAgentProcess, RuntimeError,
};

mod live_inputs;
mod names;
mod realization;
use names::{cfg_owner_label, configmap_name, credential_secret_name, k8s_runtime_id, pod_name};
use realization::{
    PodReadiness, await_pod_deleted, create_or_verify, pod_readiness, stamp_realization,
};

pub use crate::k8s_package_image::K8sPackageImageProvisioner;

static EXEC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

pub(crate) fn api_conflict(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 409)
}

pub(crate) fn api_not_found(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(response) if response.code == 404)
}

struct K8sExecState {
    completion: Option<tokio::task::JoinHandle<Option<Status>>>,
    status: Option<pc::ExitStatus>,
}

struct K8sExecProcess {
    id: String,
    pod: String,
    pid_file: String,
    pods: Api<Pod>,
    state: tokio::sync::Mutex<K8sExecState>,
}

fn k8s_exit_status(status: Option<Status>) -> pc::ExitStatus {
    let success = status.as_ref().and_then(|status| status.status.as_deref()) == Some("Success");
    let code = status
        .and_then(|status| status.details)
        .and_then(|details| details.causes)
        .and_then(|causes| {
            causes.into_iter().find_map(|cause| {
                (cause.reason.as_deref() == Some("ExitCode"))
                    .then_some(cause.message)
                    .flatten()
            })
        })
        .and_then(|message| message.parse::<i32>().ok())
        .or(Some(if success { 0 } else { 1 }));
    pc::ExitStatus {
        code,
        signaled: false,
    }
}

fn k8s_exec_argv(
    id: &str,
    command: pc::MaterializedCommand,
) -> Result<(String, Vec<String>), RuntimeError> {
    if command.argv.is_empty() {
        return Err(backend("exec command argv is empty"));
    }
    let pid_file = format!("/tmp/{id}.pid");
    let mut argv = vec!["env".to_string()];
    for var in command.env {
        if var.value.is_secret() {
            return Err(backend(
                "Kubernetes exec cannot deliver a process secret without argv exposure",
            ));
        }
        argv.push(format!("{}={}", var.name, var.value.expose()));
    }
    argv.extend([
        "sh".into(),
        "-c".into(),
        "pid_file=$1; cwd=$2; shift 2; printf '%s' \"$$\" > \"$pid_file\"; \
         if [ -n \"$cwd\" ]; then cd -- \"$cwd\" || exit 126; fi; exec \"$@\""
            .into(),
        "awaken-exec".into(),
        pid_file.clone(),
        command.cwd,
    ]);
    argv.extend(command.argv);
    Ok((pid_file, argv))
}

#[async_trait]
impl pc::ProcessHandle for K8sExecProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        let mut state = self.state.lock().await;
        if let Some(status) = &state.status {
            return Ok(status.clone());
        }
        let completion = state
            .completion
            .take()
            .ok_or_else(|| pc::SandboxError::new("k8s exec completion is unavailable"))?;
        let status = completion
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let status = k8s_exit_status(status);
        state.status = Some(status.clone());
        Ok(status)
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let mut state = self.state.lock().await;
        if let Some(status) = &state.status {
            return Ok(Some(status.clone()));
        }
        let Some(completion) = state.completion.as_ref() else {
            return Err(pc::SandboxError::new("k8s exec completion is unavailable"));
        };
        if !completion.is_finished() {
            return Ok(None);
        }
        let completion = state.completion.take().expect("checked above");
        let status = completion
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let status = k8s_exit_status(status);
        state.status = Some(status.clone());
        Ok(Some(status))
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        let name = match signal {
            pc::Signal::Term => "TERM",
            pc::Signal::Kill => "KILL",
            pc::Signal::Int => "INT",
        };
        let script = format!(
            "pid=$(cat -- '{}') && kill -{} \"$pid\"",
            self.pid_file.replace('\'', "'\\''"),
            name
        );
        let mut attached = self
            .pods
            .exec(
                &self.pod,
                vec!["sh", "-c", &script],
                &AttachParams::default()
                    .container("agent")
                    .stdin(false)
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| pc::SandboxError::new("k8s signal exec has no stdout"))?;
        let status = attached
            .take_status()
            .ok_or_else(|| pc::SandboxError::new("k8s signal exec has no status"))?;
        let mut ignored = Vec::new();
        let (read, status) = tokio::join!(stdout.read_to_end(&mut ignored), status);
        read.map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if k8s_exit_status(status).code == Some(0) {
            Ok(())
        } else {
            Err(pc::SandboxError::new("k8s exec signal failed"))
        }
    }
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

fn credential_binds(plan: &ContainerPlan) -> Vec<&BindPlan> {
    plan.binds
        .iter()
        .filter(|bind| bind.secret_content.is_some())
        .collect()
}

fn credential_key(bind: &BindPlan) -> &str {
    bind.credential_file_path
        .as_deref()
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or(CONFIGMAP_KEY)
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

fn build_credential_secret(
    id: &str,
    i: usize,
    key: &str,
    bytes: &[u8],
    owner: &Option<OwnerReference>,
) -> Secret {
    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), "awaken-sandbox".to_string());
    labels.insert("awaken-cfg-owner".to_string(), cfg_owner_label(id));
    Secret {
        metadata: ObjectMeta {
            name: Some(credential_secret_name(id, i)),
            labels: Some(labels),
            owner_references: owner.clone().map(|o| vec![o]),
            ..Default::default()
        },
        immutable: Some(true),
        data: Some(BTreeMap::from([(
            key.to_string(),
            k8s_openapi::ByteString(bytes.to_vec()),
        )])),
        ..Default::default()
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
    image_pull_secrets: Vec<String>,
}

/// Default memoryd sidecar image (overridable via [`K8sRuntime::with_memoryd_image`]).
/// The image is the execution-plane `awaken-sandbox` binary in its `memoryd` role
/// (ENTRYPOINT `awaken-sandbox memoryd`, built with `--features memoryd`), packaged by
/// `deploy/images/sandbox/Dockerfile.memoryd`. The sidecar sets no `command`, so the
/// role reads the `AWAKEN_MEMORY_*` env this plan injects below.
const DEFAULT_MEMORYD_IMAGE: &str = "ghcr.io/awaken/memoryd:latest";

/// Select the process-wide provider before any kube client is built. Workspace
/// feature unification can compile both rustls providers, so relying on rustls'
/// implicit selection is not deterministic.
pub(crate) fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl K8sRuntime {
    /// Connect via in-cluster ServiceAccount or the ambient kubeconfig.
    pub async fn connect(
        namespace: impl Into<String>,
        agent_addr: SocketAddr,
    ) -> Result<Self, RuntimeError> {
        // kube's rustls client needs a process-level CryptoProvider; install ring
        // once (idempotent — a prior install by the host is fine).
        install_rustls_crypto_provider();
        let client = Client::try_default().await.map_err(backend)?;
        Ok(Self {
            client,
            namespace: namespace.into(),
            agent_addr,
            owner: None,
            memoryd_image: DEFAULT_MEMORYD_IMAGE.to_string(),
            rendezvous: None,
            memoryd_fuse: false,
            image_pull_secrets: Vec::new(),
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

    /// Attach existing namespace-local imagePullSecrets to every Session Pod.
    /// Registry credentials remain Kubernetes-owned and are never injected into
    /// the Agent container.
    #[must_use]
    pub fn with_image_pull_secrets(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.image_pull_secrets = names
            .into_iter()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .collect();
        self
    }

    /// A runtime backed by a **lazy** client (no cluster dial), for unit-testing the
    /// builder + Pod-assembly paths; the live `create`/`wait`/… methods still need a
    /// real apiserver (exercised by the gated `k8s_it` integration test).
    #[cfg(test)]
    fn for_test(agent_addr: SocketAddr) -> Self {
        install_rustls_crypto_provider();
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
            image_pull_secrets: Vec::new(),
        }
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn configmaps(&self) -> Api<ConfigMap> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    fn secrets(&self) -> Api<Secret> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    async fn cleanup_projected_content(&self, container_id: &str) {
        let selector = format!("awaken-cfg-owner={container_id}");
        let params = ListParams::default().labels(&selector);
        let _ = self
            .configmaps()
            .delete_collection(&DeleteParams::default(), &params)
            .await;
        let _ = self
            .secrets()
            .delete_collection(&DeleteParams::default(), &params)
            .await;
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
            &self.image_pull_secrets,
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
    image_pull_secrets: &[String],
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
        let mut init_containers: Vec<Container> = Vec::new();
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
                args: Some(vec![
                    "--store-id".into(),
                    mm.store_id.clone(),
                    "--mount-path".into(),
                    mm.mount_path.clone(),
                    "--mode".into(),
                    if memoryd_fuse { "fuse" } else { "copy" }.into(),
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

        live_inputs::append_projection(plan, &mut volumes, &mut agent_mounts, &mut sidecars);

        // Inline content has no host path a Pod can bind, so every item is backed by
        // a ConfigMap created alongside the Pod, except Managed Files whose initial
        // and later generations share the runtime-owned live projector. Other paths
        // keep the exact read-only subPath projection used by bwrap parity.
        for (i, bind) in content_binds(plan).iter().enumerate() {
            if crate::live_inputs::manages(bind) {
                continue;
            }
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

        // A Kubernetes Secret volume is immutable/read-only. Seed each native OAuth
        // credential into a pod-scoped writable emptyDir in an init container, then
        // mount that config directory at the CLI's native home. The worker harvests it via the
        // exec subresource before terminating the agent and writes it back to the broker.
        for (i, bind) in credential_binds(plan).iter().enumerate() {
            let seed_vol = format!("credential-seed-{i}");
            let writable_vol = format!("credential-rw-{i}");
            volumes.push(Volume {
                name: seed_vol.clone(),
                secret: Some(SecretVolumeSource {
                    secret_name: Some(credential_secret_name(id, i)),
                    default_mode: Some(0o400),
                    ..Default::default()
                }),
                ..Default::default()
            });
            volumes.push(Volume {
                name: writable_vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            init_containers.push(Container {
                name: format!("credential-init-{i}"),
                image: Some(plan.image.clone()),
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "cp -a /seed/. /writable/ && chmod 600 /writable/*".into(),
                ]),
                volume_mounts: Some(vec![
                    VolumeMount {
                        name: seed_vol,
                        mount_path: "/seed".into(),
                        read_only: Some(true),
                        ..Default::default()
                    },
                    VolumeMount {
                        name: writable_vol.clone(),
                        mount_path: "/writable".into(),
                        ..Default::default()
                    },
                ]),
                security_context: Some(hardened_security_context()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: writable_vol,
                mount_path: bind.mount_path.clone(),
                read_only: Some(false),
                ..Default::default()
            });
        }

        let mut containers = vec![Container {
            name: "agent".into(),
            image: Some(plan.image.clone()),
            // Session environment: PID 1 keeps the namespaces alive; attempt agents
            // are attached exec processes created by `spawn_agent`.
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
                name: Some(pod_name(id)),
                labels: Some(labels),
                // native GC: the platform reaps this Pod when the owner is deleted.
                owner_references: owner.clone().map(|o| vec![o]),
                ..Default::default()
            },
            spec: Some(PodSpec {
                init_containers: (!init_containers.is_empty()).then_some(init_containers),
                containers,
                volumes: (!volumes.is_empty()).then_some(volumes),
                security_context: Some(PodSecurityContext {
                    fs_group: Some(10001),
                    ..Default::default()
                }),
                // a finished agent Pod is reaped, not looped.
                restart_policy: Some("Never".into()),
                // The untrusted agent must NOT reach the kube API (no SA token): its
                // only control-plane channel is the ACP data channel, nothing else.
                automount_service_account_token: Some(false),
                image_pull_secrets: (!image_pull_secrets.is_empty()).then(|| {
                    image_pull_secrets
                        .iter()
                        .map(|name| LocalObjectReference { name: name.clone() })
                        .collect()
                }),
                ..Default::default()
            }),
            status: None,
        }
    }
}

/// The egress-posture label value an external platform policy may select on. The
/// label itself is metadata, not enforcement, so this adapter does not advertise
/// network isolation until composition can verify that policy separately.
fn egress_label(network: &crate::NetworkMode) -> &'static str {
    match network {
        crate::NetworkMode::Open => "open",
        crate::NetworkMode::None => "restricted",
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
    fn has_native_memory_mounts(&self) -> bool {
        true
    }

    fn supports_secret_writeback(&self) -> bool {
        true
    }

    fn supports_live_input_projection(&self) -> bool {
        true
    }

    async fn project_live_input(
        &self,
        container_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeError> {
        live_inputs::project(self, container_id, path, bytes).await
    }

    async fn remove_live_input(&self, container_id: &str, path: &str) -> Result<(), RuntimeError> {
        live_inputs::remove(self, container_id, path).await
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        if !matches!(plan.network, crate::NetworkMode::Open) {
            return Err(RuntimeError::Backend(
                "k8s adapter cannot prove an installed network-isolation policy".into(),
            ));
        }
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
        let runtime_id = k8s_runtime_id(id)?;
        // Realize ordinary inline-content mounts as ConfigMaps *before* the Pod: the
        // Pod's volumes reference them by name. Managed Files deliberately bypass
        // this immutable path and use the one stable live projector after readiness.
        let cms = self.configmaps();
        for (i, bind) in content_binds(plan).iter().enumerate() {
            if crate::live_inputs::manages(bind) {
                continue;
            }
            let mut cm = build_configmap(
                &runtime_id,
                i,
                bind.content.as_deref(),
                bind.content_bytes.as_deref(),
                &self.owner,
            );
            stamp_realization(&mut cm)?;
            create_or_verify(&cms, &cm).await?;
        }
        let secrets = self.secrets();
        for (i, bind) in credential_binds(plan).iter().enumerate() {
            let mut secret = build_credential_secret(
                &runtime_id,
                i,
                credential_key(bind),
                bind.secret_content
                    .as_ref()
                    .expect("credential bind has secret bytes")
                    .expose(),
                &self.owner,
            );
            stamp_realization(&mut secret)?;
            create_or_verify(&secrets, &secret).await?;
        }
        let mut pod = self.pod(&runtime_id, plan);
        stamp_realization(&mut pod)?;
        let pods = self.pods();
        let created = create_or_verify(&pods, &pod).await?;
        let name = created
            .metadata
            .name
            .ok_or_else(|| backend("created pod has no name"))?;
        realization::await_pod_ready(&pods, &name).await?;
        // This is also the idempotent recovery path: an identical Pod realization
        // is adopted first, then the current Session manifest replaces its managed
        // files without changing the Pod or its realization digest.
        live_inputs::project_manifest(self, &name, plan).await?;
        Ok(name)
    }

    async fn spawn(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        if command.stdio == pc::Stdio::Piped {
            return Err(backend(
                "piped container exec requires the agent-channel capability",
            ));
        }
        let id = format!(
            "k8s-exec-{}-{}",
            std::process::id(),
            EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let (pid_file, argv) = k8s_exec_argv(&id, command)?;
        let pods = self.pods();
        let mut attached = pods
            .exec(
                container_id,
                argv,
                &AttachParams::default()
                    .container("agent")
                    .stdin(false)
                    // kube requires at least one attached stdio stream. Keep stdout
                    // attached and drain it in the completion task so a noisy command
                    // cannot block before the remote status frame is delivered.
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(backend)?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| backend("k8s exec has no stdout"))?;
        let status = attached
            .take_status()
            .ok_or_else(|| backend("k8s exec has no completion status"))?;
        let completion = tokio::spawn(async move {
            let mut ignored = Vec::new();
            let (_, status) = tokio::join!(stdout.read_to_end(&mut ignored), status);
            status
        });
        Ok(Box::new(K8sExecProcess {
            id,
            pod: container_id.to_string(),
            pid_file,
            pods,
            state: tokio::sync::Mutex::new(K8sExecState {
                completion: Some(completion),
                status: None,
            }),
        }))
    }

    async fn spawn_agent(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        let id = format!(
            "k8s-agent-exec-{}-{}",
            std::process::id(),
            EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let (pid_file, argv) = k8s_exec_argv(&id, command)?;
        let pods = self.pods();
        let mut attached = pods
            .exec(
                container_id,
                argv,
                &AttachParams::default()
                    .container("agent")
                    .stdin(true)
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(backend)?;
        let stdin = attached
            .stdin()
            .ok_or_else(|| backend("k8s agent exec has no stdin"))?;
        let stdout = attached
            .stdout()
            .ok_or_else(|| backend("k8s agent exec has no stdout"))?;
        let completion = attached
            .take_status()
            .ok_or_else(|| backend("k8s agent exec has no completion status"))?;
        Ok(RuntimeAgentProcess {
            process: Box::new(K8sExecProcess {
                id,
                pod: container_id.to_string(),
                pid_file,
                pods,
                state: tokio::sync::Mutex::new(K8sExecState {
                    completion: Some(tokio::spawn(completion)),
                    status: None,
                }),
            }),
            channel: Box::new(SplitChannel::new(stdout, stdin)),
        })
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

    async fn read_live_file(
        &self,
        container_id: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, RuntimeError> {
        let mut attached = self
            .pods()
            .exec(
                container_id,
                vec!["cat", "--", path],
                &AttachParams::default().container("agent").stderr(false),
            )
            .await
            .map_err(backend)?;
        let mut stdout = attached
            .stdout()
            .ok_or_else(|| backend("k8s credential harvest has no stdout"))?;
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map_err(backend)?;
        drop(stdout);
        attached.join().await.map_err(backend)?;
        Ok(Some(bytes))
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        let pod = match self.pods().get(container_id).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => {
                return Ok(ContainerState::Gone);
            }
            Err(error) => return Err(backend(error)),
        };
        Ok(match pod_readiness(&pod) {
            PodReadiness::Ready => ContainerState::Running,
            PodReadiness::Waiting(_) => ContainerState::Provisioning,
            PodReadiness::Failed(_) => ContainerState::Gone,
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
            ContainerState::Provisioning | ContainerState::Running => Ok(None),
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
            .map_err(backend)?;
        self.cleanup_projected_content(container_id).await;
        Ok(())
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
        self.cleanup_projected_content(container_id).await;
        let pods = self.pods();
        match pods.delete(container_id, &DeleteParams::default()).await {
            Ok(_) => await_pod_deleted(&pods, container_id).await,
            Err(error) if api_not_found(&error) => Ok(()),
            Err(error) => Err(backend(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_provisioning_contract::ProcessHandle;
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn overlong_or_empty_scope_fails_before_the_first_k8s_write() {
        /* Boundary rules extending the table above: N5 an empty scope or an
         * injective encoding whose Pod/owner-label exceeds 63 bytes produces an
         * explicit adapter error before the lazy test client can reach its
         * deliberately unavailable API server. */
        let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        let plan = plan_with_memory(Vec::new());
        for scope in [String::new(), "x".repeat(57)] {
            let error = runtime.create(&scope, &plan).await.unwrap_err();
            assert!(error.to_string().contains("sandbox scope"), "N5: {error}");
        }
    }

    #[tokio::test]
    async fn package_builder_job_is_rootless_bounded_and_registry_backed() {
        // Cause/effect decision table: R1 exact base/packages + shared Registry
        // produce one deterministic ConfigMap/Job destination; R2 insecure local
        // Registry emits an explicit BuildKit host policy; R3 the Job is rootless,
        // tokenless, no-retry, and ends before Coordinator's lease; R4 output is
        // recorded through the termination digest contract; R5 an unsafe Registry
        // prefix is rejected before it can enter generated BuildKit configuration;
        // R6 ring+aws-lc feature unification => select the canonical ring provider
        // before constructing the lazy kube client (no provider ambiguity panic).
        install_rustls_crypto_provider();
        let config = kube::Config::new("http://127.0.0.1:1/".parse().unwrap());
        let client = Client::try_from(config).unwrap();
        let builder = K8sPackageImageProvisioner::new(
            client.clone(),
            "awaken-system",
            "registry.local:5000/environments",
            vec!["registry-auth".into()],
            true,
        )
        .unwrap();
        let packages = pc::PackageRequirements {
            managers: [("npm".into(), vec!["@playwright/mcp@latest".into()])]
                .into_iter()
                .collect(),
            resolution_id: Some("env-browser:3".into()),
        };
        let (config, job, destination) = builder
            .build_objects("registry.local/base@sha256:exact", &packages)
            .unwrap();
        assert!(
            destination.starts_with("registry.local:5000/environments/awaken-packages:"),
            "R1"
        );
        let data = config.data.unwrap();
        assert!(data["Dockerfile"].contains("@playwright/mcp@latest"), "R1");
        assert!(data["buildkitd.toml"].contains("http = true"), "R2");
        let spec = job.spec.unwrap();
        assert_eq!(spec.backoff_limit, Some(0), "R3");
        assert!(spec.active_deadline_seconds.unwrap() < 15 * 60, "R3");
        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.automount_service_account_token, Some(false), "R3");
        assert_eq!(
            pod.image_pull_secrets.as_ref().unwrap()[0].name,
            "registry-auth",
            "R3"
        );
        assert!(
            pod.volumes
                .as_ref()
                .unwrap()
                .iter()
                .any(|volume| volume.name == "registry-auth"),
            "R3 private Registry auth"
        );
        let buildkit = &pod.containers[0];
        assert_eq!(
            buildkit
                .security_context
                .as_ref()
                .and_then(|value| value.run_as_user),
            Some(1000),
            "R3"
        );
        assert!(
            buildkit.args.as_ref().unwrap()[0].contains("/dev/termination-log"),
            "R4"
        );
        assert!(
            K8sPackageImageProvisioner::new(
                client,
                "awaken-system",
                "registry.local/environments\n[registry.\"attacker\"]",
                Vec::new(),
                true,
            )
            .is_err(),
            "R5"
        );
    }

    struct FixedBroker;

    #[async_trait]
    impl pc::SecretBroker for FixedBroker {
        async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Ok(b"k8s-secret".to_vec())
        }

        async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            self.materialize(reference).await
        }

        async fn write_back(
            &self,
            _reference: &str,
            _bytes: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            Err(pc::SandboxError::new("not supported"))
        }
    }

    async fn materialized(command: pc::Command) -> pc::MaterializedCommand {
        pc::materialize_process_command(&[], command, None)
            .await
            .unwrap()
    }

    fn exec_process(completion: Option<tokio::task::JoinHandle<Option<Status>>>) -> K8sExecProcess {
        let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        K8sExecProcess {
            id: "k8s-exec-test".into(),
            pod: "pod-test".into(),
            pid_file: "/tmp/pid".into(),
            pods: rt.pods(),
            state: tokio::sync::Mutex::new(K8sExecState {
                completion,
                status: None,
            }),
        }
    }

    fn success_status(code: i32) -> Status {
        Status {
            status: Some(if code == 0 { "Success" } else { "Failure" }.into()),
            details: Some(
                k8s_openapi::apimachinery::pkg::apis::meta::v1::StatusDetails {
                    causes: Some(vec![
                        k8s_openapi::apimachinery::pkg::apis::meta::v1::StatusCause {
                            reason: Some("ExitCode".into()),
                            message: Some(code.to_string()),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                },
            ),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn exec_wait_and_poll_cache_the_remote_completion_status() {
        let process = exec_process(Some(tokio::spawn(async { Some(success_status(7)) })));
        assert_eq!(process.id(), "k8s-exec-test");
        assert_eq!(process.wait().await.unwrap().code, Some(7));
        assert_eq!(process.wait().await.unwrap().code, Some(7));
        assert_eq!(process.poll().await.unwrap().unwrap().code, Some(7));
    }

    #[tokio::test]
    async fn exec_poll_distinguishes_running_missing_and_finished_status() {
        let process = exec_process(Some(tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Some(success_status(0))
        })));
        assert_eq!(process.poll().await.unwrap(), None);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(process.poll().await.unwrap().unwrap().code, Some(0));

        let missing = exec_process(None);
        assert!(missing.poll().await.is_err());
        assert!(missing.wait().await.is_err());
        assert_eq!(k8s_exit_status(None).code, Some(1));
    }

    #[tokio::test]
    async fn exec_completion_join_failures_and_signal_transport_fail_closed() {
        let aborted_wait = tokio::spawn(std::future::pending::<Option<Status>>());
        aborted_wait.abort();
        let process = exec_process(Some(aborted_wait));
        assert!(process.wait().await.is_err());

        let aborted_poll = tokio::spawn(std::future::pending::<Option<Status>>());
        aborted_poll.abort();
        tokio::task::yield_now().await;
        let process = exec_process(Some(aborted_poll));
        assert!(process.poll().await.is_err());

        let process = exec_process(None);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            process.signal(pc::Signal::Kill),
        )
        .await
        .expect("unreachable test API must fail promptly");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn exec_admission_and_argv_materialization_cover_all_command_boundaries() {
        assert!(
            k8s_exec_argv("empty", pc::MaterializedCommand::new(Vec::<String>::new())).is_err()
        );

        let mut secret = pc::Command::new(["echo", "value"]);
        secret.env.push(pc::EnvVar {
            name: "TOKEN".into(),
            value: pc::EnvValue::Secret {
                reference: "credential://test".into(),
            },
            visibility: pc::EnvVisibility::Process,
        });
        let broker: Arc<dyn pc::SecretBroker> = Arc::new(FixedBroker);
        let secret = pc::materialize_process_command(&[], secret, Some(&broker))
            .await
            .unwrap();
        assert!(k8s_exec_argv("secret", secret).is_err());

        let mut inline = pc::Command::new(["echo", "value"]);
        inline.cwd = "/workspace".into();
        inline.env.push(pc::EnvVar {
            name: "MODE".into(),
            value: pc::EnvValue::Inline {
                value: "test".into(),
            },
            visibility: pc::EnvVisibility::Process,
        });
        let inline = materialized(inline).await;
        let (pid_file, argv) = k8s_exec_argv("inline", inline).unwrap();
        assert_eq!(pid_file, "/tmp/inline.pid");
        assert!(argv.iter().any(|value| value == "MODE=test"));
        assert!(argv.iter().any(|value| value == "/workspace"));

        let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        let mut piped = pc::MaterializedCommand::new(["echo", "value"]);
        piped.stdio = pc::Stdio::Piped;
        assert!(rt.spawn("pod", piped).await.is_err());
        assert!(
            rt.spawn_agent("pod", pc::MaterializedCommand::new(Vec::<String>::new()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn builder_methods_set_every_field_and_pod_delegates_to_build_pod() {
        let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
            .with_memoryd_fuse(true)
            .with_owner(OwnerReference::default())
            .with_rendezvous("127.0.0.1:7000".parse().unwrap())
            .with_memoryd_image("custom/memoryd:1")
            .with_image_pull_secrets(["registry-pull".into()]);
        assert!(rt.memoryd_fuse);
        assert!(rt.owner.is_some());
        assert_eq!(rt.rendezvous, Some("127.0.0.1:7000".parse().unwrap()));
        assert_eq!(rt.memoryd_image, "custom/memoryd:1");
        assert_eq!(
            rt.pod("s1", &plan_with_memory(vec![]))
                .spec
                .unwrap()
                .image_pull_secrets
                .unwrap()[0]
                .name,
            "registry-pull"
        );
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
        assert!(rt.read_artifact("pod", "artifact").await.is_err());
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
            packages: Default::default(),
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
        let pod = build_pod("run-1", &plan, &None, "memoryd:9", None, false, &[]);
        let spec = pod.spec.unwrap();

        // agent + one memoryd sidecar per memory store + the isolated input projector.
        assert_eq!(spec.containers.len(), 4);
        assert_eq!(spec.containers[0].name, "agent");
        assert_eq!(
            spec.containers
                .iter()
                .filter(|c| c.name.starts_with("memoryd-"))
                .count(),
            2
        );
        // one pod-scoped emptyDir per store, the three writable-rootfs dirs
        // (workspace, outputs, and /tmp), and the live read-only input tree.
        let volumes = spec.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 2 + 3 + 1);
        assert!(volumes.iter().all(|v| v.empty_dir.is_some()));
        // the agent mounts both memory volumes + writable dirs + live inputs.
        let agent = &spec.containers[0];
        assert_eq!(agent.volume_mounts.as_ref().unwrap().len(), 2 + 3 + 1);
        assert!(agent.resources.is_some());
        // The sidecar names store/mount/mode through explicit argv; no environment
        // configuration path exists (the privilege lives only on this container).
        let sc = spec
            .containers
            .iter()
            .find(|c| c.name == "memoryd-0")
            .unwrap();
        assert_eq!(sc.image.as_deref(), Some("memoryd:9"));
        assert_eq!(
            sc.args
                .as_ref()
                .unwrap()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "--store-id",
                "s1",
                "--mount-path",
                "/workspace/.mnt/a",
                "--mode",
                "copy",
            ]
        );
        assert!(sc.env.is_none());
    }

    #[test]
    fn build_pod_projects_inline_content_as_configmap_subpath_volumes() {
        /* Cause/effect projection decision table — KP3:
         * C1 an ordinary inline bind is outside the managed input root; C2 a managed
         * inline bind precedes it; C3 a ref-only bind has no carried bytes.
         * C1+C2+C3 => E1 only the ordinary bind becomes a ConfigMap/subPath, E2 its
         * stable content-bind index remains cfg-1 in both build/create, and E3 neither
         * the managed bind nor ref-only bind creates a parallel ConfigMap path.
         */
        let mut plan = plan_with_memory(Vec::new());
        plan.binds = vec![
            crate::BindPlan {
                source_ref: String::new(),
                mount_path: "/mnt/session/uploads/current/index.html".into(),
                read_only: true,
                content: Some("<h1>managed</h1>".into()),
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            },
            crate::BindPlan {
                source_ref: String::new(),
                mount_path: "/acp-config/config.toml".into(),
                read_only: true,
                content: Some("[mcp_servers.gh]\nx\n".into()),
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            },
            // A ref-backed bind (no content) must NOT become a ConfigMap volume.
            crate::BindPlan {
                source_ref: "blob-123".into(),
                mount_path: "/data/in".into(),
                read_only: true,
                content: None,
                content_bytes: None,
                secret_content: None,
                secret_writeback: false,
                credential_file_path: None,
            },
        ];
        let spec = build_pod("run-9", &plan, &None, "m", None, false, &[])
            .spec
            .unwrap();

        // One ConfigMap volume (only the ordinary content bind), named cfg-1, plus the
        // 2 writable-rootfs emptyDirs. The ref-backed bind adds nothing on this tier.
        let volumes = spec.volumes.as_ref().unwrap();
        let cfg = volumes
            .iter()
            .find(|v| v.config_map.is_some())
            .expect("a configmap volume");
        assert_eq!(cfg.name, "cfg-1");
        assert_eq!(cfg.config_map.as_ref().unwrap().name, "awaken-run-9-cfg-1");
        assert_eq!(volumes.iter().filter(|v| v.config_map.is_some()).count(), 1);

        // The agent mounts it as a single file at the exact path (subPath = the CM key).
        let agent = &spec.containers[0];
        let m = agent
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|m| m.name == "cfg-1")
            .expect("the configmap mount");
        assert_eq!(m.mount_path, "/acp-config/config.toml");
        assert_eq!(m.sub_path.as_deref(), Some("content"));
        assert_eq!(m.read_only, Some(true));
    }

    #[test]
    fn native_credential_is_seeded_from_a_secret_into_a_writable_file() {
        let credential = br#"{"tokens":{"refresh_token":"never-log-me"}}"#.to_vec(); // awaken-allow: secret -- synthetic test fixture
        let mut plan = plan_with_memory(Vec::new());
        plan.binds = vec![crate::BindPlan {
            source_ref: "credential://acp/native/codex".into(),
            mount_path: "/acp-config".into(),
            read_only: false,
            content: None,
            content_bytes: None,
            secret_content: Some(crate::SecretBytes::new(credential.clone())),
            secret_writeback: true,
            credential_file_path: Some("/acp-config/auth.json".into()),
        }];

        let pod = build_pod("oauth", &plan, &None, "memoryd", None, false, &[]);
        let spec = pod.spec.unwrap();
        let init = spec
            .init_containers
            .as_ref()
            .and_then(|containers| containers.first())
            .expect("credential init container");
        assert_eq!(init.name, "credential-init-0");
        assert!(
            init.command
                .as_ref()
                .is_some_and(|command| command.iter().any(|part| part.contains("chmod 600")))
        );
        let agent = spec
            .containers
            .iter()
            .find(|container| container.name == "agent")
            .unwrap();
        let auth = agent
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|mount| mount.mount_path == "/acp-config")
            .unwrap();
        assert_eq!(auth.read_only, Some(false));
        assert_eq!(auth.sub_path, None);
        assert!(
            spec.volumes
                .as_ref()
                .unwrap()
                .iter()
                .any(|v| v.secret.is_some())
        );

        let secret = build_credential_secret("oauth", 0, "auth.json", &credential, &None);
        assert_eq!(
            secret
                .data
                .as_ref()
                .and_then(|data| data.get("auth.json"))
                .map(|bytes| bytes.0.as_slice()),
            Some(credential.as_slice())
        );
        assert!(!format!("{plan:?}").contains("never-log-me"));
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
    fn build_pod_carries_only_non_secret_base_environment() {
        // ContainerPlan is the environment-container creation layer and carries
        // public base env only. Process credentials are materialized for exec later;
        // the Kubernetes adapter currently rejects that operation because the exec
        // API would otherwise expose the value in argv.
        let mut plan = plan_with_memory(Vec::new());
        plan.env = vec![
            ("AWAKEN_ACP_GATEWAY_URL".into(), "http://gw.internal".into()),
            ("HTTPS_PROXY".into(), "http://gw.internal:8888".into()),
        ];
        let spec = build_pod("r", &plan, &None, "m", None, false, &[])
            .spec
            .unwrap();
        let env = spec.containers[0].env.clone().unwrap();
        assert!(env.iter().any(|e| e.name == "AWAKEN_ACP_GATEWAY_URL"));
        assert!(
            env.iter()
                .all(|e| e.name != "ANTHROPIC_API_KEY" && e.name != "AWAKEN_ACP_LEASE_TOKEN")
        );
    }

    #[test]
    fn build_pod_without_memory_mounts_still_isolates_the_input_projector() {
        let pod = build_pod(
            "r",
            &plan_with_memory(Vec::new()),
            &None,
            "m",
            None,
            false,
            &[],
        );
        let spec = pod.spec.unwrap();
        // No memoryd sidecar; only the Agent and its runtime-owned input projector.
        // The Agent gets three writable-rootfs emptyDirs plus one read-only input tree.
        assert_eq!(spec.containers.len(), 2);
        assert_eq!(spec.containers[1].name, live_inputs::PROJECTOR);
        assert_eq!(spec.volumes.as_ref().unwrap().len(), 4);
        assert_eq!(spec.containers[0].volume_mounts.as_ref().unwrap().len(), 4);
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
        let spec = build_pod("r", &plan, &None, "m", None, false, &[])
            .spec
            .unwrap();
        let sc = memoryd_sidecar(&spec);
        assert!(sc.args.as_ref().unwrap().iter().any(|arg| arg == "copy"));
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
        let spec = build_pod("r", &plan, &None, "m", None, true, &[])
            .spec
            .unwrap();
        let sc = memoryd_sidecar(&spec);
        assert!(sc.args.as_ref().unwrap().iter().any(|arg| arg == "fuse"));
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
        let spec = build_pod(
            "r",
            &plan_with_memory(Vec::new()),
            &None,
            "m",
            None,
            false,
            &[],
        )
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
        let no_rv = build_pod("r", &plan, &None, "m", None, false, &[]);
        let env0 = no_rv.spec.unwrap().containers[0].env.clone().unwrap();
        assert!(env0.iter().all(|e| e.name != "AWAKEN_ACP_RENDEZVOUS"));
        // With one, the agent is told where to dial out.
        let with_rv = build_pod("r", &plan, &None, "m", Some("10.0.0.5:9000"), false, &[]);
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
        // Restricted intent is labeled for platform observation, while provider
        // capability remains false until an installed policy is verified.
        let mut plan = plan_with_memory(Vec::new());
        plan.network = crate::NetworkMode::None;
        let pod = build_pod("r", &plan, &None, "m", None, false, &[]);
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(
            labels.get("awaken-egress").map(String::as_str),
            Some("restricted")
        );

        plan.network = crate::NetworkMode::Open;
        let open = build_pod("r", &plan, &None, "m", None, false, &[]);
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
        let denied = build_pod("r", &plan, &None, "m", None, false, &[]);
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
