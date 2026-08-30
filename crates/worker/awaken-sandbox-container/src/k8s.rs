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
use awaken_sandbox_control::SandboxControlServiceKind;
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, EmptyDirVolumeSource, EnvVar,
    LocalObjectReference, PersistentVolumeClaim, Pod, PodSecurityContext, PodSpec, Secret,
    SecretVolumeSource, Volume, VolumeMount,
};
#[cfg(test)]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::Api;
#[cfg(test)]
use kube::Client;
use kube::api::{AttachParams, DeleteParams, ListParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::net::TcpAgentTransport;
use crate::{
    BindPlan, ContainerPlan, ContainerRuntime, ContainerState, RuntimeAgentProcess, RuntimeError,
    SandboxControlBindingRequest,
};

#[cfg(test)]
pub(crate) type PodSpecMutation = (&'static str, fn(&mut PodSpec));

mod channel;
mod client;
mod continuation;
mod creation;
mod error;
mod lifecycle;
mod live_inputs;
mod memory;
mod names;
mod network_policy;
mod pod_projection;
mod pod_security;
mod process;
mod realization;
mod restore;
mod sandbox_control;
use client::K8sClients;
pub(crate) use client::install_rustls_crypto_provider;
use error::api_not_found;
pub(crate) use error::backend;
use names::{configmap_name, credential_secret_name};
pub(crate) use names::{k8s_runtime_id, pod_name};
#[cfg(test)]
use pod_projection::build_pod;
pub use pod_projection::pod_for_plan;
use pod_projection::{
    CONFIGMAP_KEY, append_writable_and_cache_volumes, build_configmap, build_credential_secret,
    content_binds, credential_binds, credential_key,
};
use pod_security::{
    admit_network, egress_label, hardened_security_context, pod_resources, unenforceable_k8s_limit,
};
pub(crate) use pod_security::{has_forbidden_sandbox_namespace_shape, sandbox_network_labels};
use process::{K8sExecProcess, K8sExecState, k8s_exec_argv, k8s_live_file_result};
#[cfg(test)]
use process::{k8s_exit_status, signal_effect_is_complete};
use realization::{
    PodReadiness, create_or_verify, create_or_verify_with_status, pod_readiness, reap_terminal_pod,
    stamp_pod_realization,
};
pub(crate) use realization::{create_or_verify_with_status_exact, stamp_realization};
pub use sandbox_control::{
    DEFAULT_REPOSITORY_GIT_CONTROL_PORT, K8sSandboxControlForwarder,
    pod_for_plan_with_control_forwarder,
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
    /// Process-incarnation fence used by realization adoption.
    owner_id: String,
    /// When set, the host binds this address as a **reverse-dial rendezvous**: the
    /// Pod dials *out* to it (no inbound, no Service, fully egress-fenced) and the
    /// address is injected into the agent as `AWAKEN_ACP_RENDEZVOUS`. When `None`,
    /// the host direct-dials `agent_addr` (a published Service) instead.
    rendezvous: Option<SocketAddr>,
    image_pull_secrets: Vec<String>,
    /// Live apiserver evidence for the canonical sandbox NetworkPolicy graph.
    /// The same fact drives capability projection and restricted-Pod admission.
    network_policy_attestation: network_policy::Attestation,
    /// Private resident-process port reached only through the authenticated Pod
    /// port-forward subresource. `None` preserves the legacy channel topology.
    pod_channel_port: Option<u16>,
    sandbox_control_forwarder: Option<K8sSandboxControlForwarder>,
    continuation_volume: Option<crate::K8sContinuationVolume>,
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
        let namespace = namespace.into();
        let runtime = Self {
            clients,
            namespace,
            agent_addr,
            owner: None,
            owner_id: crate::runtime_owner_id(),
            rendezvous: None,
            image_pull_secrets: Vec::new(),
            network_policy_attestation: network_policy::Attestation::new(),
            pod_channel_port: None,
            sandbox_control_forwarder: None,
            continuation_volume: None,
        };
        runtime
            .network_policy_attestation
            .refresh(&runtime.clients.control, &runtime.namespace)
            .await?;
        Ok(runtime)
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

    #[must_use]
    pub fn with_pod_channel_port(mut self, port: u16) -> Self {
        self.pod_channel_port = Some(port);
        self
    }

    #[must_use]
    pub fn with_sandbox_control_forwarder(mut self, forwarder: K8sSandboxControlForwarder) -> Self {
        self.sandbox_control_forwarder = Some(forwarder);
        self
    }

    /// Persist every canonical writable root on one PVC which is deliberately
    /// not owned by the Pod or Worker. Terminal-Pod rebuild reuses it; explicit
    /// Environment disposal removes it.
    #[must_use]
    pub fn with_continuation_volume(mut self, config: crate::K8sContinuationVolume) -> Self {
        self.continuation_volume = Some(config);
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
            network_policy_attestation: network_policy::Attestation::new(),
            pod_channel_port: None,
            sandbox_control_forwarder: None,
            continuation_volume: None,
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

    fn persistent_volume_claims(&self) -> Api<PersistentVolumeClaim> {
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
}

fn build_pod_with_continuation(
    id: &str,
    plan: &ContainerPlan,
    owner: &Option<OwnerReference>,
    rendezvous: Option<&str>,
    image_pull_secrets: &[String],
    continuation_claim: Option<&str>,
    sandbox_control_forwarder: Option<&K8sSandboxControlForwarder>,
) -> Pod {
    assert!(
        plan.control_services.is_empty() || sandbox_control_forwarder.is_some(),
        "a demanded Sandbox control service requires an installed trusted forwarder"
    );
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

        let continuation_subpaths = append_writable_and_cache_volumes(
            plan,
            continuation_claim,
            &mut volumes,
            &mut agent_mounts,
        );
        continuation::append_init_container(plan, &continuation_subpaths, &mut init_containers);
        live_inputs::append_projection(plan, &mut volumes, &mut agent_mounts, &mut sidecars);
        if plan
            .control_services
            .contains(&SandboxControlServiceKind::RepositoryGitCredential)
            && let Some(forwarder) = sandbox_control_forwarder
        {
            let (volume, agent_mount, forwarder_container) = sandbox_control::projection(forwarder);
            volumes.push(volume);
            agent_mounts.push(agent_mount);
            sidecars.push(forwarder_container);
        }

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
        let labels = sandbox_network_labels(Some(egress_label(&plan.network)));

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
                // Disposable child/probe environments have no continuity to flush.
                // Make their canonical Pod deletion complete inside the bounded
                // provider-disposal contract; retained Sessions keep Kubernetes'
                // normal graceful-termination default.
                termination_grace_period_seconds: lifecycle::termination_grace_period(
                    plan.filesystem_continuity,
                ),
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

#[async_trait]
impl ContainerRuntime for K8sRuntime {
    fn enforces_network_none(&self) -> bool {
        self.network_policy_attestation.current()
    }

    fn enforces_network_allowlist(&self) -> bool {
        self.network_policy_attestation.allowlist_current()
    }

    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        self.network_policy_attestation
            .refresh(&self.clients.control, &self.namespace)
            .await
    }

    fn has_native_memory_mounts(&self) -> bool {
        true
    }

    fn sandbox_control_services(&self) -> std::collections::BTreeSet<SandboxControlServiceKind> {
        sandbox_control::services(self)
    }

    async fn sandbox_control_binding(
        &self,
        container_id: &str,
        request: SandboxControlBindingRequest<'_>,
    ) -> Result<Option<pc::SandboxControlIncarnation>, RuntimeError> {
        sandbox_control::bind(self, container_id, request).await
    }

    async fn open_sandbox_control_channel(
        &self,
        container_id: &str,
        binding: &pc::SandboxControlIncarnation,
        kind: SandboxControlServiceKind,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        sandbox_control::open_channel(self, container_id, binding, kind).await
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
        if matches!(plan.network, crate::NetworkMode::None) {
            self.probe_ready().await?;
        }
        admit_network(&plan.network, self.network_policy_attestation.current())?;
        creation::create(self, id, plan).await
    }

    async fn recover_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<Option<crate::RuntimeRestoreTarget>, RuntimeError> {
        if matches!(plan.network, crate::NetworkMode::None) {
            self.probe_ready().await?;
        }
        admit_network(&plan.network, self.network_policy_attestation.current())?;
        restore::recover(self, id, plan, plan_fingerprint, evidence).await
    }

    async fn restore_or_adopt(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<crate::RuntimeRestoreTarget, RuntimeError> {
        if matches!(plan.network, crate::NetworkMode::None) {
            self.probe_ready().await?;
        }
        admit_network(&plan.network, self.network_policy_attestation.current())?;
        creation::restore_or_adopt(self, id, plan, plan_fingerprint, evidence).await
    }

    async fn restoration_evidence(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
        restore::restoration_evidence(self, container_id).await
    }

    async fn restoration_plan_fingerprint(
        &self,
        container_id: &str,
    ) -> Result<Option<String>, RuntimeError> {
        restore::plan_fingerprint(self, container_id).await
    }

    async fn dispose_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<(), RuntimeError> {
        restore::dispose(self, id, plan, plan_fingerprint, evidence).await
    }

    async fn handle_extra(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::ContainerContinuationHandle>, RuntimeError> {
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        continuation::handle_extra(&pod)
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
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        if let Some(port) = self.pod_channel_port {
            return channel::open_pod_channel(&self.streaming_pods(), container_id, port).await;
        }
        match self.rendezvous {
            // Reverse-dial: the host listens, the egress-fenced Pod dials out to us.
            Some(addr) => channel::accept_reverse(addr).await,
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
        // The owner controller patches the native Lease renewTime. This adapter
        // must not duplicate that authority or infer disposal from Pod age.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        continuation::remove_bound(self, container_id, None).await
    }

    async fn remove_with_handle(
        &self,
        container_id: &str,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
    ) -> Result<(), RuntimeError> {
        let claim_uid = continuation::cleanup_claim_uid(runtime_handle)?;
        continuation::remove_bound(self, container_id, claim_uid).await
    }
}

#[cfg(test)]
mod tests;
