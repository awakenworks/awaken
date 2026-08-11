//! Real Kubernetes backend (ADR-0041 Slice 5, `k8s` feature).
//!
//! Implements [`ContainerRuntime`] over **kube** — the kube-apiserver via the SDK,
//! never `kubectl`. Faithful to awaken-next's `K3sHandWorker` + this crate's
//! [`K8sRuntime`]: a **Session-owned Pod** (PID 1 retains its namespaces while
//! attempts run through attached exec, `restartPolicy: Never`), **native GC** (an
//! `ownerReference` reaps orphans),
//! memory stores realized as authority-seeded **emptyDir** volumes, managed input Files
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
    ConfigMap, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar,
    LocalObjectReference, Pod, PodSecurityContext, PodSpec, Secret, SecretVolumeSource, Volume,
    VolumeMount,
};
#[cfg(test)]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Api;
#[cfg(test)]
use kube::Client;
use kube::api::{AttachParams, DeleteParams, ListParams, PostParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::net::TcpAgentTransport;
use crate::{
    BindPlan, ContainerPlan, ContainerRuntime, ContainerState, RuntimeAgentProcess, RuntimeError,
};

mod client;
mod error;
mod live_inputs;
mod memory;
mod names;
mod pod_projection;
mod pod_security;
mod process;
mod realization;
use client::K8sClients;
pub(crate) use client::install_rustls_crypto_provider;
use error::api_not_found;
pub(crate) use error::{api_conflict, backend};
use names::{configmap_name, credential_secret_name, k8s_runtime_id, pod_name};
use pod_projection::{
    CONFIGMAP_KEY, append_writable_and_cache_volumes, build_configmap, build_credential_secret,
    content_binds, credential_binds, credential_key,
};
use pod_security::{
    admit_network, egress_label, hardened_security_context, pod_resources, unenforceable_k8s_limit,
};
use process::{K8sExecProcess, K8sExecState, k8s_exec_argv, k8s_live_file_result};
#[cfg(test)]
use process::{k8s_exit_status, signal_effect_is_complete};
use realization::{
    PodReadiness, await_pod_deleted, create_or_verify, create_or_verify_with_status, pod_readiness,
    reap_terminal_pod, stamp_pod_realization, stamp_realization,
};

pub use crate::k8s_package_image::K8sPackageImageProvisioner;

static EXEC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A Kubernetes-backed [`ContainerRuntime`]. `agent_addr` is the Service endpoint the
/// runtime dials for the [`AgentChannel`]; `owner` (optional) is the GC owner.
pub struct K8sRuntime {
    clients: K8sClients,
    namespace: String,
    agent_addr: SocketAddr,
    owner: Option<OwnerReference>,
    /// Process-incarnation fence used by the cross-restart orphan reaper.
    owner_id: String,
    /// When set, the host binds this address as a **reverse-dial rendezvous**: the
    /// Pod dials *out* to it (no inbound, no Service, fully egress-fenced) and the
    /// address is injected into the agent as `AWAKEN_ACP_RENDEZVOUS`. When `None`,
    /// the host direct-dials `agent_addr` (a published Service) instead.
    rendezvous: Option<SocketAddr>,
    image_pull_secrets: Vec<String>,
    /// Exact external `awaken-egress=open|restricted` enforcement evidence;
    /// labels alone never imply a boundary.
    restricted_egress_policy: bool,
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
        let clients = K8sClients::infer().await?;
        Ok(Self {
            clients,
            namespace: namespace.into(),
            agent_addr,
            owner: None,
            owner_id: crate::runtime_owner_id(),
            rendezvous: None,
            image_pull_secrets: Vec::new(),
            restricted_egress_policy: false,
        })
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

    /// Attest that the cluster denies all Session ingress and restricted egress,
    /// while only `awaken-egress=open` may egress.
    #[must_use]
    pub fn with_restricted_egress_policy(mut self, installed: bool) -> Self {
        self.restricted_egress_policy = installed;
        self
    }

    /// A runtime backed by a **lazy** client (no cluster dial), for unit-testing the
    /// builder + Pod-assembly paths; the live `create`/`wait`/… methods still need a
    /// real apiserver (exercised by the gated `k8s_it` integration test).
    #[cfg(test)]
    fn for_test(agent_addr: SocketAddr) -> Self {
        install_rustls_crypto_provider();
        Self {
            clients: K8sClients::for_test(),
            namespace: "default".into(),
            agent_addr,
            owner: None,
            owner_id: crate::runtime_owner_id(),
            rendezvous: None,
            image_pull_secrets: Vec::new(),
            restricted_egress_policy: false,
        }
    }

    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.clients.control.clone(), &self.namespace)
    }

    fn streaming_pods(&self) -> Api<Pod> {
        Api::namespaced(self.clients.streaming.clone(), &self.namespace)
    }

    fn configmaps(&self) -> Api<ConfigMap> {
        Api::namespaced(self.clients.control.clone(), &self.namespace)
    }

    fn secrets(&self) -> Api<Secret> {
        Api::namespaced(self.clients.control.clone(), &self.namespace)
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
        let mut pod = build_pod(
            id,
            plan,
            &self.owner,
            rendezvous.as_deref(),
            &self.image_pull_secrets,
        );
        let labels = pod.metadata.labels.get_or_insert_with(Default::default);
        labels.insert(crate::REAPER_LABEL.to_string(), "1".to_string());
        labels.insert(crate::REAPER_OWNER_LABEL.to_string(), self.owner_id.clone());
        pod
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
    rendezvous: Option<&str>,
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

        // Each Memory store uses one authoritative host-side mounter. Its bounded
        // snapshot is streamed by a runtime-only projector into a pod-scoped
        // emptyDir before create returns; disposal reads that exact tree back and
        // lets the retained mounter perform the canonical CAS harvest. The Pod
        // receives neither a second Memory database nor Resource-authority credentials.
        let mut volumes: Vec<Volume> = Vec::new();
        let mut agent_mounts: Vec<VolumeMount> = Vec::new();
        let mut sidecars: Vec<Container> = Vec::new();
        let mut init_containers: Vec<Container> = Vec::new();
        let mut memory_projector_mounts = Vec::new();
        for (i, mm) in plan.memory_mounts.iter().enumerate() {
            let vol = format!("mem-{i}");
            volumes.push(Volume {
                name: vol.clone(),
                empty_dir: Some(EmptyDirVolumeSource::default()),
                ..Default::default()
            });
            agent_mounts.push(VolumeMount {
                name: vol.clone(),
                mount_path: mm.mount_path.clone(),
                read_only: Some(mm.access == pc::MountAccess::ReadOnly),
                ..Default::default()
            });
            memory_projector_mounts.push(VolumeMount {
                name: vol,
                mount_path: format!("/memory/{i}"),
                ..Default::default()
            });
        }
        if !memory_projector_mounts.is_empty() {
            sidecars.push(Container {
                name: memory::PROJECTOR.into(),
                image: Some(plan.image.clone()),
                command: Some(crate::environment_keepalive_command()),
                volume_mounts: Some(memory_projector_mounts),
                security_context: Some(hardened_security_context()),
                ..Default::default()
            });
        }

        append_writable_and_cache_volumes(plan, &mut volumes, &mut agent_mounts);
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
            if bind.credential_file_path.is_none() {
                volumes.push(Volume {
                    name: seed_vol.clone(),
                    secret: Some(SecretVolumeSource {
                        secret_name: Some(credential_secret_name(id, i)),
                        default_mode: Some(0o440),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                agent_mounts.push(VolumeMount {
                    name: seed_vol,
                    mount_path: bind.mount_path.clone(),
                    sub_path: Some(credential_key(bind).to_string()),
                    read_only: Some(true),
                    ..Default::default()
                });
                continue;
            }
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
            resources: pod_resources(&plan.requests, &plan.limits),
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

/// Render the exact Kubernetes Pod shape used by [`K8sRuntime`] from an already
/// normalized container plan, without contacting an apiserver. This is the
/// canonical deployment-proof seam for callers that need to validate generated
/// Pod contracts; live creation still adds runtime ownership labels in
/// [`K8sRuntime::pod`] before submitting the same Pod.
#[must_use]
pub fn pod_for_plan(id: &str, plan: &ContainerPlan) -> Pod {
    build_pod(id, plan, &None, None, &[])
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

#[async_trait]
impl ContainerRuntime for K8sRuntime {
    fn has_native_memory_mounts(&self) -> bool {
        true
    }

    fn uses_persistent_volume_claims(&self) -> bool {
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
        admit_network(&plan.network, self.restricted_egress_policy)?;
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
        let pods = self.pods();
        reap_terminal_pod(&pods, &pod_name(&runtime_id)).await?;
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
        stamp_pod_realization(&mut pod)?;
        let outcome = create_or_verify_with_status(&pods, &pod).await?;
        let was_created = outcome.created;
        let mut created = outcome.object;
        let name = created
            .metadata
            .name
            .clone()
            .ok_or_else(|| backend("created pod has no name"))?;
        if created
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(crate::REAPER_OWNER_LABEL))
            != Some(&self.owner_id)
        {
            // The immutable digest already proved this is the same frozen Session
            // realization. Transfer only the reaper lease with the observed
            // resourceVersion as the optimistic-concurrency fence; a concurrent
            // claimant gets 409 and must not steal a live Pod silently.
            created
                .metadata
                .labels
                .get_or_insert_with(Default::default)
                .insert(crate::REAPER_OWNER_LABEL.to_string(), self.owner_id.clone());
            pods.replace(&name, &PostParams::default(), &created)
                .await
                .map_err(backend)?;
        }
        realization::await_pod_ready(&pods, &name).await?;
        // Memory is mutable Session state. Seed a newly-created volume exactly
        // once; an adopted Pod already contains the live writes that the new
        // Worker must preserve and eventually harvest through the same mounter.
        if was_created {
            memory::project_snapshots(self, &name, plan).await?;
        }
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
        let execution = k8s_exec_argv(&id, command)?;
        let pods = self.streaming_pods();
        let mut attached = pods
            .exec(
                container_id,
                execution.argv,
                &AttachParams::default()
                    .container("agent")
                    .stdin(!execution.secret_stdin.is_empty())
                    // kube requires at least one attached stdio stream. Keep stdout
                    // attached and drain it in the completion task so a noisy command
                    // cannot block before the remote status frame is delivered.
                    .stdout(true)
                    .stderr(false),
            )
            .await
            .map_err(backend)?;
        if !execution.secret_stdin.is_empty() {
            let mut stdin = attached
                .stdin()
                .ok_or_else(|| backend("k8s secret prelude has no stdin"))?;
            for secret in execution.secret_stdin {
                stdin
                    .write_all(secret.expose().as_bytes())
                    .await
                    .map_err(backend)?;
            }
            stdin.shutdown().await.map_err(backend)?;
        }
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
            pid_file: execution.pid_file,
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
        let execution = k8s_exec_argv(&id, command)?;
        let pods = self.streaming_pods();
        let mut attached = pods
            .exec(
                container_id,
                execution.argv,
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
        let mut stdin = stdin;
        for secret in execution.secret_stdin {
            stdin
                .write_all(secret.expose().as_bytes())
                .await
                .map_err(backend)?;
        }
        stdin.flush().await.map_err(backend)?;
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
                pid_file: execution.pid_file,
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
            .streaming_pods()
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
        let status = attached
            .take_status()
            .ok_or_else(|| backend("k8s credential harvest has no completion status"))?;
        let mut bytes = Vec::new();
        let (read, status) = tokio::join!(stdout.read_to_end(&mut bytes), status);
        read.map_err(backend)?;
        k8s_live_file_result(status, bytes)
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

    async fn list_managed(&self) -> Result<Vec<crate::ManagedContainer>, RuntimeError> {
        let pods = self
            .pods()
            // `app=awaken-sandbox` predates the reaper labels. Selecting the stable
            // legacy label lets the first upgraded process collect Pods leaked by
            // older ownerless runtimes as well as all newly fenced Pods.
            .list(&ListParams::default().labels("app=awaken-sandbox"))
            .await
            .map_err(backend)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Ok(pods
            .into_iter()
            .filter_map(|pod| {
                let id = pod.metadata.name?;
                let owned_by_current_runtime = pod
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(crate::REAPER_OWNER_LABEL))
                    == Some(&self.owner_id);
                let running = !matches!(
                    pod.status
                        .as_ref()
                        .and_then(|status| status.phase.as_deref()),
                    Some("Succeeded" | "Failed")
                );
                let age_secs = pod
                    .metadata
                    .creation_timestamp
                    .map(|created| now.saturating_sub(created.0.timestamp().max(0) as u64))
                    .unwrap_or(0);
                Some(crate::ManagedContainer {
                    id,
                    owned_by_current_runtime,
                    running,
                    age_secs,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::ForwardProxy;
    use awaken_provisioning_contract::ProcessHandle;
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn kube_client_accepts_an_http_proxy_from_deployment_environment() {
        // A host-level HTTP(S)_PROXY is consumed by kube::Config discovery. K8s
        // workers must keep accepting that standard deployment posture instead of
        // panicking while the Session runtime is being composed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut config = kube::Config::new("https://127.0.0.1:1/".parse().unwrap());
        config.proxy_url = Some("http://127.0.0.1:18082/".parse().unwrap());
        Client::try_from(config).expect("the k8s client is compiled with HTTP proxy support");
    }

    #[tokio::test]
    async fn empty_scope_fails_before_the_first_k8s_write() {
        /* Boundary rule extending the table above: an empty opaque scope has no
         * runtime identity and fails before the lazy test client can reach its
         * deliberately unavailable API server. Long valid scopes are covered by
         * names::tests and map to a bounded content identity. */
        let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        let plan = plan_with_memory(Vec::new());
        let error = runtime.create("", &plan).await.unwrap_err();
        assert!(error.to_string().contains("sandbox scope"), "N5: {error}");
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
        .unwrap()
        .with_buildkit_image("registry.local:5000/system/buildkit:v0.30.0-rootless")
        .unwrap()
        .with_forward_proxy(ForwardProxy {
            url: "http://proxy.internal:8080".into(),
        })
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
        assert!(
            (30 * 60..60 * 60).contains(&spec.active_deadline_seconds.unwrap()),
            "R3 cold package builds stay bounded below the Coordinator run ceiling"
        );
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
            buildkit.image.as_deref(),
            Some("registry.local:5000/system/buildkit:v0.30.0-rootless"),
            "R3 the operator-selected mirror must be the only BuildKit pull reference"
        );
        assert_eq!(
            buildkit
                .security_context
                .as_ref()
                .and_then(|value| value.run_as_user),
            Some(1000),
            "R3"
        );
        assert_eq!(
            buildkit
                .security_context
                .as_ref()
                .and_then(|value| value.allow_privilege_escalation),
            Some(true),
            "R3 rootless newuidmap/newgidmap helpers require setuid execution"
        );
        assert!(
            buildkit.args.as_ref().unwrap()[0].contains("/dev/termination-log"),
            "R4"
        );
        let environment = buildkit.env.as_ref().unwrap();
        for name in [
            "FORWARD_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
        ] {
            assert!(
                environment.iter().any(|variable| {
                    variable.name == name
                        && variable.value.as_deref() == Some("http://proxy.internal:8080")
                }),
                "R6 BuildKit and package-manager egress must inherit {name}"
            );
        }
        assert!(
            environment.iter().any(|variable| {
                variable.name == "NO_PROXY"
                    && variable
                        .value
                        .as_deref()
                        .is_some_and(|value| value.contains("registry.local:5000"))
            }),
            "R6 the package Registry must bypass the external proxy"
        );
        assert!(
            buildkit.args.as_ref().unwrap()[0].contains("build-arg:HTTP_PROXY"),
            "R6 predefined proxy args must reach package-manager RUN steps"
        );
        assert!(
            buildkit.args.as_ref().unwrap()[0].contains(
                "mkdir -p /tmp/workspace\ncp /input/Dockerfile /tmp/workspace/Dockerfile"
            ),
            "R6 rootless BuildKit must use a writable workspace"
        );
        let environment = buildkit.env.as_ref().unwrap();
        for name in [
            "FORWARD_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
        ] {
            assert!(
                environment.iter().any(|variable| {
                    variable.name == name
                        && variable.value.as_deref() == Some("http://proxy.internal:8080")
                }),
                "R8 BuildKit and package-manager egress must inherit {name}"
            );
        }
        assert!(
            environment.iter().any(|variable| {
                variable.name == "NO_PROXY"
                    && variable
                        .value
                        .as_deref()
                        .is_some_and(|value| value.contains("registry.local:5000"))
            }),
            "R8 the package Registry must bypass the external proxy"
        );
        assert!(
            buildkit.args.as_ref().unwrap()[0].contains("build-arg:HTTP_PROXY"),
            "R8 predefined proxy args must reach package-manager RUN steps"
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
        assert!(
            K8sPackageImageProvisioner::new(
                Client::try_from(kube::Config::new("http://127.0.0.1:1/".parse().unwrap()))
                    .unwrap(),
                "awaken-system",
                "registry.local/environments",
                Vec::new(),
                true,
            )
            .unwrap()
            .with_buildkit_image("registry.local/bad image")
            .is_err(),
            "R7 an invalid mirrored builder reference must fail before a Kubernetes write"
        );
        assert!(
            K8sPackageImageProvisioner::new(
                Client::try_from(kube::Config::new("http://127.0.0.1:1/".parse().unwrap()))
                    .unwrap(),
                "awaken-system",
                "registry.local/environments",
                Vec::new(),
                true,
            )
            .unwrap()
            .with_forward_proxy(ForwardProxy {
                url: "file:///tmp/not-a-proxy".into(),
            })
            .is_err(),
            "R9 an invalid package-build proxy must fail before a Kubernetes write"
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

    #[test]
    fn signal_completion_uses_one_authoritative_success_rule() {
        // Cause/effect graph: C1 signal command exits zero/non-zero; C2 target
        // process is unfinished/finished. Effect E1 accepts the signal effect
        // when C1 is zero OR C2 is finished; E2 rejects it only when neither is
        // true. Decision table: R1 zero+unfinished=>E1, R2 zero+finished=>E1,
        // R3 non-zero+finished=>E1, R4 non-zero+unfinished=>E2. FMECA: parallel
        // success branches can drift and turn a harmless exit race into a false
        // failure (S4/O3/D4); this predicate is the sole owner of the rule.
        let success = pc::ExitStatus {
            code: Some(0),
            signaled: false,
        };
        let failure = pc::ExitStatus {
            code: Some(1),
            signaled: false,
        };
        let finished = pc::ExitStatus {
            code: Some(143),
            signaled: true,
        };

        assert!(signal_effect_is_complete(&success, None), "R1/E1");
        assert!(
            signal_effect_is_complete(&success, Some(&finished)),
            "R2/E1"
        );
        assert!(
            signal_effect_is_complete(&failure, Some(&finished)),
            "R3/E1"
        );
        assert!(!signal_effect_is_complete(&failure, None), "R4/E2");
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

    #[test]
    fn live_file_harvest_requires_a_successful_remote_exit_status() {
        // Credential-harvest FMECA decision table. Causes: C1 remote `cat`
        // exits 0; C2 it exits nonzero (missing/permission denied); C3 the API
        // stream closes without a Status frame; C4 returned bytes are empty.
        // Effects: E1 exact bytes are eligible for broker write-back; E2 fail
        // closed so stale/empty material cannot replace authority. Rules: H1
        // C1=>E1; H2 C1+C4=>E1 (empty is data, validation belongs upstream);
        // H3 C2|C3=>E2.
        assert_eq!(
            k8s_live_file_result(Some(success_status(0)), b"rotated".to_vec()).unwrap(),
            Some(b"rotated".to_vec()),
            "H1"
        );
        assert_eq!(
            k8s_live_file_result(Some(success_status(0)), Vec::new()).unwrap(),
            Some(Vec::new()),
            "H2"
        );
        assert!(
            k8s_live_file_result(Some(success_status(1)), Vec::new()).is_err(),
            "H3 nonzero"
        );
        assert!(
            k8s_live_file_result(None, Vec::new()).is_err(),
            "H3 missing status"
        );
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
        let secret = k8s_exec_argv("secret", secret).unwrap();
        assert_eq!(secret.secret_stdin.len(), 1);
        assert!(
            secret
                .argv
                .iter()
                .all(|value| !value.contains("container-process-secret")),
            "the secret prelude must never enter Kubernetes exec argv"
        );
        assert!(secret.argv.iter().any(|value| value == "TOKEN"));

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
        let inline = k8s_exec_argv("inline", inline).unwrap();
        assert_eq!(inline.pid_file, "/tmp/inline.pid");
        assert!(inline.argv.iter().any(|value| value == "MODE=test"));
        assert!(inline.argv.iter().any(|value| value == "/workspace"));
        assert!(inline.secret_stdin.is_empty());

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

    #[test]
    fn secret_stdin_prelude_becomes_process_env_and_preserves_agent_protocol_input() {
        use std::io::Write as _;

        let mut command = pc::MaterializedCommand::new([
            "sh",
            "-c",
            "printf '%s|' \"$TOKEN\"; IFS= read -r line; printf '%s' \"$line\"",
        ]);
        command.env.push(pc::MaterializedEnvVar {
            name: "TOKEN".into(),
            value: pc::MaterializedEnvValue::Secret(awaken_runtime_contract::RedactedString::new(
                "test-secret",
            )),
        });
        let execution = k8s_exec_argv("secret-prelude-test", command).unwrap();
        assert!(
            execution
                .argv
                .iter()
                .all(|part| !part.contains("test-secret"))
        );

        let mut child = std::process::Command::new(&execution.argv[0])
            .args(&execution.argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.as_mut().unwrap();
        for secret in execution.secret_stdin {
            stdin.write_all(secret.expose().as_bytes()).unwrap();
        }
        stdin.write_all(b"protocol-message\n").unwrap();
        let output = child.wait_with_output().unwrap();
        let _ = std::fs::remove_file(execution.pid_file);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"test-secret|protocol-message");
    }

    #[tokio::test]
    async fn builder_methods_set_every_field_and_pod_delegates_to_build_pod() {
        let rt = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
            .with_owner(OwnerReference::default())
            .with_rendezvous("127.0.0.1:7000".parse().unwrap())
            .with_image_pull_secrets(["registry-pull".into()]);
        assert!(rt.owner.is_some());
        assert_eq!(rt.rendezvous, Some("127.0.0.1:7000".parse().unwrap()));
        let labels = rt
            .pod("s1", &plan_with_memory(vec![]))
            .metadata
            .labels
            .unwrap();
        assert_eq!(
            labels.get(crate::REAPER_LABEL).map(String::as_str),
            Some("1")
        );
        assert_eq!(
            labels.get(crate::REAPER_OWNER_LABEL).map(String::as_str),
            Some(rt.owner_id.as_str())
        );
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
    fn pod_resources_maps_requests_and_limits_independently() {
        // Cause/effect graph: C1 requests set, C2 limits set, C3 pids set.
        // R1 C1+C2+C3 => E1 Kubernetes requests and limits carry cpu/memory/disk,
        // E2 pids is absent because Kubernetes has no Pod resource key. R2 neither
        // C1 nor an expressible C2 => no ResourceRequirements (next test).
        let r = pod_resources(
            &pc::ResourceRequests {
                cpu_millis: Some(750),
                memory_bytes: Some(536_870_912),
                disk_bytes: Some(1024),
            },
            &pc::ResourceLimits {
                cpu_millis: Some(1500),
                memory_bytes: Some(1_073_741_824),
                pids: Some(256),
                disk_bytes: Some(2048),
            },
        )
        .expect("requests and limits are set");
        let requests = r.requests.expect("requests map present");
        assert_eq!(requests.get("cpu").unwrap().0, "750m");
        assert_eq!(requests.get("memory").unwrap().0, "536870912");
        assert_eq!(requests.get("ephemeral-storage").unwrap().0, "1024");
        let limits = r.limits.expect("limits map present");
        assert_eq!(limits.get("cpu").unwrap().0, "1500m");
        assert_eq!(limits.get("memory").unwrap().0, "1073741824");
        assert_eq!(limits.get("ephemeral-storage").unwrap().0, "2048");
        // pids has no standard pod-level key.
        assert!(!limits.contains_key("pids"));
    }

    #[test]
    fn pod_resources_is_none_without_expressible_caps() {
        assert!(
            pod_resources(
                &pc::ResourceRequests::default(),
                &pc::ResourceLimits::default()
            )
            .is_none()
        );
        // pids-only → nothing k8s expresses as a pod limit.
        assert!(
            pod_resources(
                &pc::ResourceRequests::default(),
                &pc::ResourceLimits {
                    pids: Some(9),
                    ..Default::default()
                }
            )
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
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits {
                memory_bytes: Some(1 << 30),
                ..Default::default()
            },
            memory_mounts: mounts,
            rootfs: crate::RootfsPlan::HostUserland,
        }
    }

    fn empty_memory_tar() -> Vec<u8> {
        tar::Builder::new(Vec::new()).into_inner().unwrap()
    }

    #[test]
    fn build_pod_realizes_memory_mounts_from_one_authoritative_snapshot_path() {
        /* Memory projection cause/effect decision table — KM1:
         * C1 the canonical MemoryMounter produced a bounded snapshot; C2 access is
         * read-only or read-write; C3 the Pod has no Resource-authority network
         * credential. C1+C2+C3 => E1 one emptyDir per mount + one runtime-only
         * projector, E2 Agent access matches C2, E3 no memoryd/second database,
         * ConfigMap size ceiling, or network client exists in the Pod. Snapshot/harvest failures are covered
         * by the provider lifecycle table and fail before success is published.
         */
        let plan = plan_with_memory(vec![
            crate::MemoryMount {
                store_id: "s1".into(),
                mount_path: "/workspace/.mnt/a".into(),
                access: pc::MountAccess::ReadOnly,
                snapshot_tar: empty_memory_tar(),
            },
            crate::MemoryMount {
                store_id: "s2".into(),
                mount_path: "/workspace/.mnt/b".into(),
                access: pc::MountAccess::ReadWrite,
                snapshot_tar: empty_memory_tar(),
            },
        ]);
        let pod = build_pod("run-1", &plan, &None, None, &[]);
        let spec = pod.spec.unwrap();

        // Agent + Memory projector + isolated input projector; no second Memory implementation.
        assert_eq!(spec.containers.len(), 3);
        assert_eq!(spec.containers[0].name, "agent");
        assert_eq!(spec.containers[1].name, memory::PROJECTOR);
        assert!(spec.init_containers.is_none());
        // One emptyDir per store, the three writable-rootfs dirs, and the live
        // read-only input tree. Large snapshots are streamed, not ConfigMaps.
        let volumes = spec.volumes.as_ref().unwrap();
        assert_eq!(volumes.len(), 2 + 3 + 1);
        assert!(volumes.iter().all(|volume| volume.empty_dir.is_some()));
        // the agent mounts both memory volumes + writable dirs + live inputs.
        let agent = &spec.containers[0];
        assert_eq!(agent.volume_mounts.as_ref().unwrap().len(), 2 + 3 + 1);
        let mounts = agent.volume_mounts.as_ref().unwrap();
        assert_eq!(
            mounts
                .iter()
                .find(|m| m.mount_path.ends_with("/a"))
                .unwrap()
                .read_only,
            Some(true)
        );
        assert_eq!(
            mounts
                .iter()
                .find(|m| m.mount_path.ends_with("/b"))
                .unwrap()
                .read_only,
            Some(false)
        );
        assert!(agent.resources.is_some());
        assert!(
            spec.containers
                .iter()
                .all(|container| !container.name.starts_with("memoryd-"))
        );
        assert_eq!(
            spec.containers[1].volume_mounts.as_ref().unwrap().len(),
            2,
            "one projector owns only the writable sides of both Memory volumes"
        );
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
        let spec = build_pod("run-9", &plan, &None, None, &[]).spec.unwrap();

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
    fn cache_volume_projects_the_exact_existing_pvc() {
        // FMECA: F1 K8s ignores a CacheVolume bind (S7 O5 D3, RPN105);
        // F2 node hostPath is substituted for a portable claim (S9 O3 D4,
        // RPN108); F3 read-only intent is lost (S8 O2 D3, RPN48). The provider
        // resolves CacheVolume to a namespaced PVC reference before this pure
        // Pod projection; no second cache implementation exists here.
        // Cause graph: C1=PVC reference present; C2=read-only; C3=ordinary bind.
        // Effects: E1=PVC volume+mount; E2=read-only preserved; E3=no PVC.
        // | Rule | C1 | C2 | C3 | Effect |
        // | K1   | 1  | 1  | 0  | E1,E2  |
        // | K2   | 0  | -  | 1  | E3     |
        let mut plan = plan_with_memory(Vec::new());
        plan.binds.push(crate::BindPlan {
            source_ref: format!("{}build-cache-v7", crate::cache_volume::PVC_BIND_REF_PREFIX),
            mount_path: "/workspace/.cache/build".into(),
            read_only: true,
            content: None,
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        });
        let spec = build_pod("run-pvc", &plan, &None, None, &[]).spec.unwrap();
        let volume = spec
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|volume| volume.persistent_volume_claim.is_some())
            .expect("K1 PVC volume");
        assert_eq!(
            volume.persistent_volume_claim.as_ref().unwrap().claim_name,
            "build-cache-v7",
            "K1"
        );
        let mount = spec.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|mount| mount.name == volume.name)
            .expect("K1 PVC mount");
        assert_eq!(mount.mount_path, "/workspace/.cache/build", "K1");
        assert_eq!(mount.read_only, Some(true), "K1/K2");
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

        let pod = build_pod("oauth", &plan, &None, None, &[]);
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
    fn readonly_secret_is_projected_as_the_exact_file_not_a_directory() {
        let mut plan = plan_with_memory(Vec::new());
        plan.binds = vec![crate::BindPlan {
            source_ref: "credential://github".into(),
            mount_path: "/run/secrets/awaken-git-credential-0".into(),
            read_only: true,
            content: None,
            content_bytes: None,
            secret_content: Some(crate::SecretBytes::new(b"synthetic-token".to_vec())),
            secret_writeback: false,
            credential_file_path: None,
        }];

        let spec = build_pod("git", &plan, &None, None, &[]).spec.unwrap();
        assert!(spec.init_containers.is_none());
        let secret = spec
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find_map(|volume| volume.secret.as_ref())
            .expect("read-only credential Secret volume");
        assert_eq!(secret.default_mode, Some(0o440));
        let mount = spec.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .find(|mount| mount.mount_path == "/run/secrets/awaken-git-credential-0")
            .expect("exact credential file mount");
        assert_eq!(mount.sub_path.as_deref(), Some(CONFIGMAP_KEY));
        assert_eq!(mount.read_only, Some(true));
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
        let spec = build_pod("r", &plan, &None, None, &[]).spec.unwrap();
        let env = spec.containers[0].env.clone().unwrap();
        assert!(env.iter().any(|e| e.name == "AWAKEN_ACP_GATEWAY_URL"));
        assert!(
            env.iter()
                .all(|e| e.name != "ANTHROPIC_API_KEY" && e.name != "AWAKEN_ACP_LEASE_TOKEN")
        );
    }

    #[test]
    fn build_pod_without_memory_mounts_still_isolates_the_input_projector() {
        let pod = build_pod("r", &plan_with_memory(Vec::new()), &None, None, &[]);
        let spec = pod.spec.unwrap();
        // No memoryd sidecar; only the Agent and its runtime-owned input projector.
        // The Agent gets three writable-rootfs emptyDirs plus one read-only input tree.
        assert_eq!(spec.containers.len(), 2);
        assert_eq!(spec.containers[1].name, live_inputs::PROJECTOR);
        assert_eq!(spec.volumes.as_ref().unwrap().len(), 4);
        assert_eq!(spec.containers[0].volume_mounts.as_ref().unwrap().len(), 4);
    }

    #[test]
    fn build_pod_hardens_the_untrusted_agent() {
        let spec = build_pod("r", &plan_with_memory(Vec::new()), &None, None, &[])
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
        let no_rv = build_pod("r", &plan, &None, None, &[]);
        let env0 = no_rv.spec.unwrap().containers[0].env.clone().unwrap();
        assert!(env0.iter().all(|e| e.name != "AWAKEN_ACP_RENDEZVOUS"));
        // With one, the agent is told where to dial out.
        let with_rv = build_pod("r", &plan, &None, Some("10.0.0.5:9000"), &[]);
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
        let pod = build_pod("r", &plan, &None, None, &[]);
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(
            labels.get("awaken-egress").map(String::as_str),
            Some("restricted")
        );

        plan.network = crate::NetworkMode::Open;
        let open = build_pod("r", &plan, &None, None, &[]);
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
        let denied = build_pod("r", &plan, &None, None, &[]);
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
