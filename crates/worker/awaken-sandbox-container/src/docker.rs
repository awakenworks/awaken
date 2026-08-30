//! Real Docker backend (ADR-0041 Slice 5, `docker` feature).
//!
//! Implements [`ContainerRuntime`] over **bollard** — the Docker Engine HTTP API
//! via the SDK, never the `docker` CLI. Faithful to awaken-next's `DockerHandWorker`:
//! the agent runs as the container's main command (process-as-container); its stdio
//! port is **published** and reached by a **network dial** (via [`crate::net`]), not
//! `docker exec`. Validated against a real daemon in `tests/docker_it.rs`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport, SplitChannel};
use awaken_provisioning_contract as pc;
use base64::Engine as _;
use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::container::{
    Config, CreateContainerOptions, DownloadFromContainerOptions, KillContainerOptions,
    ListContainersOptions, RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::{BuildImageOptions, CreateImageOptions, PruneImagesOptions, PushImageOptions};
use bollard::models::{
    ContainerInspectResponse, ContainerStateStatusEnum, HostConfig, PortBinding,
};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::net::TcpAgentTransport;
use crate::runtime::{
    ExistingRealization, ExistingRealizationDecision, ExistingRealizationPhase,
    ExistingRealizationRecovery, PhysicalIncarnation, RebuildContinuityEvidence,
    existing_realization_decision, legacy_unfenced_fingerprint, sandbox_observation,
};
use crate::{
    ContainerPlan, ContainerRealizationContext, ContainerRealizationIntent,
    ContainerRealizationNamespace, ContainerRuntime, ContainerState, MANAGED_SANDBOX_LABEL,
    PackageImageProvisioner, RUNTIME_OWNER_LABEL, RuntimeAgentProcess, RuntimeError,
    RuntimeRestoreTarget, SANDBOX_ADOPTION_LABEL, SANDBOX_ATTEMPT_LABEL,
    SANDBOX_EFFECT_EPOCH_LABEL, SANDBOX_EFFECT_EXPIRY_LABEL, SANDBOX_EFFECT_LABEL,
    SANDBOX_EFFECT_OWNER_LABEL, SANDBOX_EFFECT_RUNTIME_LABEL, SANDBOX_REALIZATION_LABEL,
    SANDBOX_SCOPE_LABEL, container_effect_fence_from_values, container_effect_label_values,
    restoration_metadata, restoration_plan_fingerprint, restore_container_name,
    runtime_container_name, sandbox_scope_identity,
};

static EXEC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn docker_realization(
    info: &ContainerInspectResponse,
) -> Result<ExistingRealization, RuntimeError> {
    let labels = info
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or_else(|| backend("Docker scope query returned a container without labels"))?;
    if labels.get(MANAGED_SANDBOX_LABEL).map(String::as_str) != Some("1") {
        return Err(backend(
            "Docker scope query returned a non-Awaken container",
        ));
    }
    let locator = info
        .id
        .clone()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| backend("Docker scope query returned a container without an id"))?;
    let phase = match info.state.as_ref().and_then(|state| state.status) {
        Some(ContainerStateStatusEnum::CREATED) => ExistingRealizationPhase::Creating,
        Some(ContainerStateStatusEnum::RUNNING) => ExistingRealizationPhase::Ready,
        Some(ContainerStateStatusEnum::EXITED) => ExistingRealizationPhase::Terminal,
        _ => ExistingRealizationPhase::Indeterminate,
    };
    Ok(ExistingRealization {
        locator: locator.clone(),
        incarnation: PhysicalIncarnation {
            identity: locator,
            version: None,
        },
        adoption_fingerprint: labels.get(SANDBOX_ADOPTION_LABEL).cloned(),
        fingerprint: labels.get(SANDBOX_REALIZATION_LABEL).cloned(),
        fence: container_effect_fence_from_values(
            labels.get(SANDBOX_EFFECT_LABEL).map(String::as_str),
            labels.get(SANDBOX_EFFECT_OWNER_LABEL).map(String::as_str),
            labels.get(SANDBOX_EFFECT_RUNTIME_LABEL).map(String::as_str),
            labels.get(SANDBOX_EFFECT_EPOCH_LABEL).map(String::as_str),
            labels.get(SANDBOX_EFFECT_EXPIRY_LABEL).map(String::as_str),
        )?,
        attempt_id: labels.get(SANDBOX_ATTEMPT_LABEL).cloned(),
        recovery: ExistingRealizationRecovery::CurrentAttemptOnly,
        phase,
    })
}

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

fn docker_backend(error: bollard::errors::Error) -> RuntimeError {
    match error {
        bollard::errors::Error::DockerStreamError { error } => RuntimeError::Backend(error),
        other => backend(other),
    }
}

fn docker_not_found(error: &bollard::errors::Error) -> bool {
    matches!(
        error,
        bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

#[derive(serde::Deserialize)]
struct RegistryAuthFile {
    #[serde(default)]
    auths: HashMap<String, RegistryAuthEntry>,
}

#[derive(serde::Deserialize)]
struct RegistryAuthEntry {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default, rename = "identitytoken")]
    identitytoken: Option<String>,
    #[serde(default, rename = "registrytoken")]
    registrytoken: Option<String>,
}

fn registry_key_matches(key: &str, registry: &str) -> bool {
    fn normalize(value: &str) -> &str {
        value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
            .unwrap_or(value)
            .trim_end_matches('/')
    }
    normalize(key) == normalize(registry)
}

fn exec_env(command: &pc::MaterializedCommand) -> Result<Vec<String>, RuntimeError> {
    Ok(command
        .env
        .iter()
        .map(|var| format!("{}={}", var.name, var.value.expose()))
        .collect())
}

struct DockerExecProcess {
    docker: Docker,
    container_id: String,
    exec_id: String,
    public_id: String,
    pid_file: Option<String>,
}

impl DockerExecProcess {
    fn new(docker: Docker, container_id: &str, exec_id: String, pid_file: String) -> Self {
        let public_id = format!("{exec_id}|{pid_file}");
        Self {
            docker,
            container_id: container_id.to_string(),
            exec_id,
            public_id,
            pid_file: Some(pid_file),
        }
    }

    fn recovered(docker: Docker, container_id: &str, public_id: &str) -> Self {
        let (exec_id, pid_file) = public_id
            .split_once('|')
            .map_or((public_id, None), |(exec_id, pid_file)| {
                (exec_id, Some(pid_file.to_string()))
            });
        Self {
            docker,
            container_id: container_id.to_string(),
            exec_id: exec_id.to_string(),
            public_id: public_id.to_string(),
            pid_file,
        }
    }

    async fn status(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let state = self
            .docker
            .inspect_exec(&self.exec_id)
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if state.running.unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(pc::ExitStatus {
            code: state.exit_code.map(|code| code as i32),
            signaled: false,
        }))
    }
}

#[async_trait]
impl pc::ProcessHandle for DockerExecProcess {
    fn id(&self) -> &str {
        &self.public_id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        loop {
            if let Some(status) = self.status().await? {
                return Ok(status);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        self.status().await
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        if self.status().await?.is_some() {
            return Ok(());
        }
        let result = async {
            let pid_file = self.pid_file.as_ref().ok_or_else(|| {
                pc::SandboxError::new("legacy docker exec handle has no in-container pid reference")
            })?;
            let name = match signal {
                pc::Signal::Term => "TERM",
                pc::Signal::Kill => "KILL",
                pc::Signal::Int => "INT",
            };
            let request = self
                .docker
                .create_exec(
                    &self.container_id,
                    CreateExecOptions {
                        cmd: Some(vec![
                            "sh".to_string(),
                            "-c".to_string(),
                            "pid=$(cat -- \"$1\") && kill -\"$2\" \"$pid\"".to_string(),
                            "awaken-signal".to_string(),
                            pid_file.clone(),
                            name.to_string(),
                        ]),
                        attach_stdout: Some(true),
                        attach_stderr: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| pc::SandboxError::new(error.to_string()))?;
            let mut output = match self
                .docker
                .start_exec(
                    &request.id,
                    Some(StartExecOptions {
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|error| pc::SandboxError::new(error.to_string()))?
            {
                StartExecResults::Attached { output, .. } => output,
                StartExecResults::Detached => {
                    return Err(pc::SandboxError::new(
                        "docker returned detached result for signal exec",
                    ));
                }
            };
            while let Some(frame) = output.next().await {
                frame.map_err(|error| pc::SandboxError::new(error.to_string()))?;
            }
            let state = self
                .docker
                .inspect_exec(&request.id)
                .await
                .map_err(|error| pc::SandboxError::new(error.to_string()))?;
            if state.exit_code == Some(0) {
                Ok(())
            } else {
                Err(pc::SandboxError::new(format!(
                    "docker exec signal failed with {:?}",
                    state.exit_code
                )))
            }
        }
        .await;
        match result {
            Err(_) if self.status().await?.is_some() => Ok(()),
            result => result,
        }
    }
}

fn wrapped_exec_argv(command: Vec<String>) -> (String, Vec<String>) {
    let sequence = EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let pid_file = format!("/tmp/awaken-exec-{sequence}.pid");
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        "pid_file=$1; shift; printf '%s' \"$$\" > \"$pid_file\"; exec \"$@\"".to_string(),
        "awaken-exec".to_string(),
        pid_file.clone(),
    ];
    argv.extend(command);
    (pid_file, argv)
}

/// Map neutral resource limits onto a bollard `HostConfig`'s cgroup fields (limits
/// only — the caller merges binds/ports). Pure, so the swap-pin and disk mapping are
/// unit-testable without a daemon.
fn cgroup_host_config(limits: &pc::ResourceLimits) -> HostConfig {
    let caps = crate::CgroupCaps::from_limits(limits);
    HostConfig {
        memory: caps.memory_bytes,
        // Pin swap to the memory cap so a memory-limited agent cannot escape it by
        // swapping (the swap-escape close).
        memory_swap: caps.memory_swap_bytes,
        nano_cpus: caps.nano_cpus,
        pids_limit: caps.pids,
        storage_opt: caps.disk_size.map(|size| {
            let mut o = HashMap::new();
            o.insert("size".to_string(), size);
            o
        }),
        ..Default::default()
    }
}

/// The tmpfs map that keeps the writable app paths usable under a read-only rootfs
/// (`{dir: "rw,noexec,nosuid,size=64m"}`). Pure/testable.
fn tmpfs_for(plan: &ContainerPlan) -> HashMap<String, String> {
    crate::writable_dirs(plan)
        .into_iter()
        // OCI images commonly run as a non-root UID. A tmpfs mount is created as
        // root:root regardless of the image's baked directory ownership, so make
        // these single-tenant scratch mounts writable without assuming a UID.
        .map(|d| (d, "rw,noexec,nosuid,mode=1777,size=64m".to_string()))
        .collect()
}

#[cfg(test)]
// Keep these pure host-config tests beside the two assembly helpers they specify;
// the feature-gated Docker adapter follows below.
#[allow(clippy::items_after_test_module)]
mod cgroup_host_config_tests {
    use super::*;

    #[test]
    fn registry_auth_selects_only_the_configured_registry_without_exposing_it() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            r#"{"auths":{"https://registry.internal/":{"auth":"dXNlcjpwYXNz"}}}"#,
        )
        .unwrap();
        let runtime =
            DockerRuntime::with_client(Docker::connect_with_local_defaults().unwrap(), 8080)
                .with_package_registry("registry.internal")
                .with_package_registry_auth_file(file.path())
                .unwrap();
        let credentials = runtime.package_registry_credentials.unwrap();
        assert_eq!(credentials.auth.as_deref(), Some("dXNlcjpwYXNz"));
        assert_eq!(credentials.username.as_deref(), Some("user"));
        assert_eq!(credentials.password.as_deref(), Some("pass"));
        assert_eq!(
            credentials.serveraddress.as_deref(),
            Some("https://registry.internal/")
        );
    }

    fn plan() -> ContainerPlan {
        ContainerPlan {
            image: "img:1".into(),
            command: vec!["a".into()],
            env: Vec::new(),
            control_services: Default::default(),
            packages: Default::default(),
            binds: Vec::new(),
            outputs_volume: "/mnt/session/outputs".into(),
            network: crate::NetworkMode::Open,
            egress_identity: crate::EgressRealizationIdentity {
                network: crate::NetworkMode::Open,
                proxy_endpoint: None,
                capability_ttl_secs: None,
                issuer_revision: None,
                ephemeral_capability: false,
            },
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            memory_mounts: Vec::new(),
            rootfs: crate::RootfsPlan::HostUserland,
        }
    }

    #[test]
    fn tmpfs_keeps_outputs_and_tmp_writable_under_ro_rootfs() {
        let t = tmpfs_for(&plan());
        assert!(t.contains_key("/mnt/session/outputs"));
        assert!(t.contains_key("/tmp"));
        assert!(t["/tmp"].contains("noexec"));
    }

    #[test]
    fn memory_limit_pins_swap_and_maps_disk() {
        let hc = cgroup_host_config(&pc::ResourceLimits {
            cpu_millis: Some(2000),
            memory_bytes: Some(256 * 1024 * 1024),
            pids: Some(64),
            disk_bytes: Some(1024),
        });
        assert_eq!(hc.memory, Some(256 * 1024 * 1024));
        assert_eq!(
            hc.memory_swap, hc.memory,
            "swap is pinned to the memory cap"
        );
        assert_eq!(hc.nano_cpus, Some(2_000_000_000));
        assert_eq!(hc.pids_limit, Some(64));
        assert_eq!(
            hc.storage_opt
                .as_ref()
                .and_then(|o| o.get("size"))
                .map(String::as_str),
            Some("1024")
        );
    }

    #[test]
    fn no_limits_leaves_the_cgroup_fields_empty() {
        let hc = cgroup_host_config(&pc::ResourceLimits::default());
        assert!(hc.memory.is_none());
        assert!(hc.memory_swap.is_none());
        assert!(hc.storage_opt.is_none());
    }

    #[test]
    fn signal_name_maps_every_signal() {
        assert_eq!(signal_name(pc::Signal::Term), "SIGTERM");
        assert_eq!(signal_name(pc::Signal::Kill), "SIGKILL");
        assert_eq!(signal_name(pc::Signal::Int), "SIGINT");
    }

    fn plan_with_binds() -> ContainerPlan {
        ContainerPlan {
            binds: vec![
                crate::BindPlan {
                    source_ref: "/host/ro".into(),
                    mount_path: "/in".into(),
                    read_only: true,
                    content: None,
                    content_bytes: None,
                    secret_content: None,
                    secret_writeback: false,
                    credential_file_path: None,
                },
                crate::BindPlan {
                    source_ref: "/host/rw".into(),
                    mount_path: "/work".into(),
                    read_only: false,
                    content: None,
                    content_bytes: None,
                    secret_content: None,
                    secret_writeback: false,
                    credential_file_path: None,
                },
            ],
            ..plan()
        }
    }

    // The bollard client builds lazily (no dial until a request), so the pure
    // host-config assembly is unit-testable without a live daemon.
    #[test]
    fn host_config_hardens_rootfs_publishes_the_agent_port_and_maps_binds_ro_flag() {
        let rt = DockerRuntime::connect_local(8080).expect("client builds without a daemon");
        let hc = rt.host_config(&plan_with_binds());
        assert_eq!(hc.readonly_rootfs, Some(true));
        assert!(hc.tmpfs.as_ref().unwrap().contains_key("/tmp"));
        assert!(hc.port_bindings.as_ref().unwrap().contains_key("8080/tcp"));
        let binds = hc.binds.unwrap();
        assert!(binds.contains(&"/host/ro:/in:ro".to_string()));
        assert!(binds.contains(&"/host/rw:/work".to_string()));
    }

    #[test]
    fn host_config_applies_the_planned_network_mode_and_gates_port_publishing() {
        let rt = DockerRuntime::connect_local(8080).expect("client builds without a daemon");

        // Open: default bridge (no explicit network_mode) with the agent port published.
        let open = rt.host_config(&plan()); // plan() defaults to NetworkMode::Open
        assert_eq!(open.network_mode, None);
        assert!(
            open.port_bindings
                .as_ref()
                .unwrap()
                .contains_key("8080/tcp")
        );

        // None: an empty network is applied and port publishing is dropped (Docker
        // forbids publishing under `--network none`). Without this the deny-egress
        // policy is planned but never enforced — a fail-open egress leak.
        let denied = rt.host_config(&ContainerPlan {
            network: crate::NetworkMode::None,
            egress_identity: crate::EgressRealizationIdentity {
                network: crate::NetworkMode::None,
                proxy_endpoint: None,
                capability_ttl_secs: None,
                issuer_revision: None,
                ephemeral_capability: false,
            },
            ..plan()
        });
        assert_eq!(denied.network_mode.as_deref(), Some("none"));
        assert!(denied.port_bindings.is_none());
    }

    #[test]
    fn with_client_wraps_a_handle_and_connect_local_builds_one() {
        let docker = Docker::connect_with_local_defaults().unwrap();
        let rt = DockerRuntime::with_client(docker, 9000)
            .with_package_build_timeout(std::time::Duration::from_secs(17));
        assert_eq!(rt.agent_port, 9000);
        assert_eq!(rt.port_key(), "9000/tcp");
        assert_eq!(rt.package_build_timeout, std::time::Duration::from_secs(17));
    }

    #[tokio::test]
    async fn artifacts_are_out_of_band_and_a_missing_lease_target_fails_closed() {
        let rt = DockerRuntime::connect_local(8080).unwrap();
        assert!(rt.artifacts("cid").await.unwrap().is_empty());
        assert!(rt.touch_lease("cid").await.is_err());
    }
}

fn signal_name(signal: pc::Signal) -> &'static str {
    match signal {
        pc::Signal::Term => "SIGTERM",
        pc::Signal::Kill => "SIGKILL",
        pc::Signal::Int => "SIGINT",
    }
}

/// A Docker-backed [`ContainerRuntime`]. `agent_port` is the container-internal TCP
/// port the agent listens on; it is published to an ephemeral host port that
/// [`ContainerRuntime::open_channel`] discovers (via inspect) and dials.
pub struct DockerRuntime {
    docker: Docker,
    agent_port: u16,
    realization_namespace: ContainerRealizationNamespace,
    owner_id: String,
    /// Serialize the cache-probe/build sequence so concurrent sessions with the
    /// same Environment cannot race two identical immutable image builds.
    package_builds: tokio::sync::Mutex<()>,
    package_registry: Option<String>,
    package_registry_credentials: Option<DockerCredentials>,
    package_cache_ttl: Option<std::time::Duration>,
    package_build_timeout: std::time::Duration,
}

impl DockerRuntime {
    /// Connect using the local defaults (unix socket / named pipe / env).
    pub fn connect_local(agent_port: u16) -> Result<Self, RuntimeError> {
        let realization_namespace =
            ContainerRealizationNamespace::from_stable_parts(["legacy-docker-runtime"])
                .map_err(backend)?;
        Self::connect_local_for_realization(realization_namespace, agent_port)
    }

    /// Connect with the stable deployment namespace used by durable Sessions.
    pub fn connect_local_for_realization(
        realization_namespace: ContainerRealizationNamespace,
        agent_port: u16,
    ) -> Result<Self, RuntimeError> {
        let docker = Docker::connect_with_local_defaults().map_err(backend)?;
        Ok(Self::with_client_for_realization(
            docker,
            realization_namespace,
            agent_port,
        ))
    }

    /// Wrap an already-built client.
    #[must_use]
    pub fn with_client(docker: Docker, agent_port: u16) -> Self {
        let realization_namespace =
            ContainerRealizationNamespace::from_stable_parts(["legacy-docker-runtime"])
                .expect("constant legacy Docker namespace is valid");
        Self::with_client_for_realization(docker, realization_namespace, agent_port)
    }

    /// Wrap a client with the durable Session realization namespace.
    #[must_use]
    pub fn with_client_for_realization(
        docker: Docker,
        realization_namespace: ContainerRealizationNamespace,
        agent_port: u16,
    ) -> Self {
        Self {
            docker,
            agent_port,
            realization_namespace,
            owner_id: crate::runtime_owner_id(),
            package_builds: tokio::sync::Mutex::new(()),
            package_registry: None,
            package_registry_credentials: None,
            package_cache_ttl: None,
            package_build_timeout: crate::packages::PACKAGE_BUILD_TIMEOUT,
        }
    }

    /// Publish content-addressed package images to a shared OCI registry.
    #[must_use]
    pub fn with_package_registry(mut self, registry: impl Into<String>) -> Self {
        self.package_registry = Some(registry.into().trim_end_matches('/').to_string());
        self
    }

    /// Bound unused Awaken-derived images in the local engine cache. Docker's
    /// prune operation never removes an image referenced by a container.
    #[must_use]
    pub fn with_package_cache_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.package_cache_ttl = Some(ttl);
        self
    }

    /// Bound one registry lookup/build/push transaction. A timeout fails the
    /// Environment activation closed; it never selects the unmodified base image.
    #[must_use]
    pub fn with_package_build_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.package_build_timeout = timeout;
        self
    }

    fn container_config(
        &self,
        plan: &ContainerPlan,
        labels: HashMap<String, String>,
    ) -> Config<String> {
        let env = plan.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut exposed_ports = HashMap::new();
        exposed_ports.insert(self.port_key(), HashMap::new());
        Config {
            image: Some(plan.image.clone()),
            entrypoint: Some(Vec::new()),
            cmd: Some(plan.command.clone()),
            env: Some(env),
            exposed_ports: Some(exposed_ports),
            host_config: Some(self.host_config(plan)),
            labels: Some(labels),
            ..Default::default()
        }
    }

    fn exact_restoration_id(
        info: &ContainerInspectResponse,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<String, RuntimeError> {
        let observed = Self::restoration_evidence_from_info(info)?;
        if observed.as_ref() != Some(evidence) {
            return Err(backend(
                "Docker restore target belongs to a different exact effect",
            ));
        }
        if Self::restoration_plan_fingerprint_from_info(info).as_deref() != Some(plan_fingerprint) {
            return Err(backend(
                "Docker restore target belongs to a different immutable plan",
            ));
        }
        info.id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| backend("Docker restore target has no container id"))
    }

    fn restoration_evidence_from_info(
        info: &ContainerInspectResponse,
    ) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
        let labels = info
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref());
        crate::restoration_evidence_from_metadata(
            |key| labels.and_then(|labels| labels.get(key).cloned()),
            "Docker container",
        )
    }

    fn restoration_plan_fingerprint_from_info(info: &ContainerInspectResponse) -> Option<String> {
        info.config
            .as_ref()?
            .labels
            .as_ref()?
            .get(crate::RESTORE_PLAN_LABEL)
            .cloned()
    }

    async fn resolve_package_build(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
    ) -> Result<(String, String, String, String), RuntimeError> {
        let base = self
            .docker
            .inspect_image(base_image)
            .await
            .map_err(backend)?;
        let base_identity = base
            .id
            .ok_or_else(|| backend("docker returned an empty base-image identity"))?;
        if base_identity.is_empty() {
            return Err(backend("docker returned an empty base-image identity"));
        }
        let base_user = base
            .config
            .and_then(|config| config.user)
            .unwrap_or_default();
        let (dockerfile, fingerprint) =
            crate::packages::package_image_recipe(&base_identity, &base_user, packages)?;
        let repository = self.package_registry.as_ref().map_or_else(
            || "awaken-packages".to_string(),
            |registry| format!("{registry}/awaken-packages"),
        );
        let image = format!("{repository}:{fingerprint}");
        Ok((dockerfile, fingerprint, repository, image))
    }

    async fn prune_package_cache(&self) {
        let Some(ttl) = self.package_cache_ttl else {
            return;
        };
        let filters = [
            ("dangling".to_owned(), vec!["false".to_owned()]),
            (
                "label".to_owned(),
                vec!["org.awaken.package-recipe".to_owned()],
            ),
            ("until".to_owned(), vec![format!("{}s", ttl.as_secs())]),
        ]
        .into_iter()
        .collect();
        let _ = self
            .docker
            .prune_images(Some(PruneImagesOptions { filters }))
            .await;
    }

    /// Load the selected registry entry from a Docker/containers authentication
    /// file. Only the Worker-side Docker API receives these credentials.
    pub fn with_package_registry_auth_file(
        mut self,
        path: impl AsRef<Path>,
    ) -> Result<Self, RuntimeError> {
        let registry = self
            .package_registry
            .as_deref()
            .ok_or_else(|| backend("registry authentication requires a package registry"))?;
        let bytes = std::fs::read(path.as_ref()).map_err(backend)?;
        let config: RegistryAuthFile = serde_json::from_slice(&bytes).map_err(backend)?;
        let (serveraddress, entry) = config
            .auths
            .into_iter()
            .find(|(server, _)| registry_key_matches(server, registry))
            .ok_or_else(|| {
                backend(format!(
                    "registry authentication file has no entry for `{registry}`"
                ))
            })?;
        let (decoded_username, decoded_password) = entry
            .auth
            .as_deref()
            .and_then(|auth| base64::engine::general_purpose::STANDARD.decode(auth).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|value| {
                value
                    .split_once(':')
                    .map(|(username, password)| (username.to_owned(), password.to_owned()))
            })
            .unzip();
        self.package_registry_credentials = Some(DockerCredentials {
            username: entry.username.or(decoded_username),
            password: entry.password.or(decoded_password),
            auth: entry.auth,
            identitytoken: entry.identitytoken,
            registrytoken: entry.registrytoken,
            serveraddress: Some(serveraddress),
            ..Default::default()
        });
        Ok(self)
    }

    /// Probe the daemon (for tests / health checks): `Ok` iff it responds.
    pub async fn ping(&self) -> Result<(), RuntimeError> {
        self.docker.version().await.map(|_| ()).map_err(backend)
    }

    async fn package_image_reference(
        &self,
        image: &str,
        repository: &str,
    ) -> Result<String, RuntimeError> {
        if self.package_registry.is_none() {
            return Ok(image.to_string());
        }
        self.docker
            .inspect_image(image)
            .await
            .map_err(backend)?
            .repo_digests
            .unwrap_or_default()
            .into_iter()
            .find(|digest| digest.starts_with(&format!("{repository}@sha256:")))
            .ok_or_else(|| backend("package image has no immutable repository digest"))
    }

    fn port_key(&self) -> String {
        format!("{}/tcp", self.agent_port)
    }

    async fn existing_realizations(
        &self,
        scope: &str,
    ) -> Result<Vec<ExistingRealization>, RuntimeError> {
        let scope_identity = sandbox_scope_identity(self.realization_namespace.as_str(), scope)?;
        let filters = [(
            "label".to_string(),
            vec![
                format!("{MANAGED_SANDBOX_LABEL}=1"),
                format!("{SANDBOX_SCOPE_LABEL}={scope_identity}"),
            ],
        )]
        .into_iter()
        .collect();
        let listed = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
            .map_err(backend)?;
        let mut observed = Vec::with_capacity(listed.len());
        for summary in listed {
            let locator = summary
                .id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| backend("Docker scope query returned a container without an id"))?;
            let inspected = self
                .docker
                .inspect_container(&locator, None)
                .await
                .map_err(backend)?;
            observed.push(docker_realization(&inspected)?);
        }
        Ok(observed)
    }

    async fn create_decision(
        &self,
        context: &ContainerRealizationContext<'_>,
        realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<ExistingRealizationDecision, RuntimeError> {
        existing_realization_decision(
            context,
            realization_fingerprint,
            RebuildContinuityEvidence::Unavailable,
            &self.existing_realizations(context.scope).await?,
        )
    }

    async fn live_inputs_root(&self, container_id: &str) -> Result<PathBuf, RuntimeError> {
        self.docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?
            .mounts
            .unwrap_or_default()
            .into_iter()
            .find(|mount| mount.destination.as_deref() == Some(crate::LIVE_INPUTS_ROOT))
            .and_then(|mount| mount.source)
            .map(PathBuf::from)
            .ok_or_else(|| backend("Docker container has no managed live-input root bind"))
    }

    fn host_config(&self, plan: &ContainerPlan) -> HostConfig {
        let binds: Vec<String> = plan
            .binds
            .iter()
            .map(|b| {
                let ro = if b.read_only { ":ro" } else { "" };
                format!("{}:{}{ro}", b.source_ref, b.mount_path)
            })
            .collect();
        // Apply the planned egress policy at the container level. Only `Open`
        // receives a bridge; unresolved Allowlist intent is treated as total denial.
        let deny_net = matches!(plan.network, crate::NetworkMode::None);
        // Publish the agent port to an ephemeral 127.0.0.1 host port — but not under
        // `--network none`, where Docker forbids port publishing (and there is no
        // reachable agent channel anyway).
        let port_bindings = (!deny_net).then(|| {
            let mut m = HashMap::new();
            m.insert(
                self.port_key(),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: Some(String::new()),
                }]),
            );
            m
        });
        HostConfig {
            binds: (!binds.is_empty()).then_some(binds),
            port_bindings,
            network_mode: deny_net.then(|| "none".to_string()),
            // Harden the untrusted agent: read-only rootfs, writable app paths as tmpfs.
            readonly_rootfs: Some(true),
            tmpfs: Some(tmpfs_for(plan)),
            ..cgroup_host_config(&plan.limits)
        }
    }

    /// Discover the ephemeral host address the agent port was published to.
    async fn agent_addr(&self, container_id: &str) -> Result<SocketAddr, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        let host_port = info
            .network_settings
            .and_then(|n| n.ports)
            .and_then(|ports| ports.get(&self.port_key()).cloned().flatten())
            .and_then(|bindings| bindings.into_iter().next())
            .and_then(|b| b.host_port)
            .ok_or_else(|| backend("agent port is not published yet"))?;
        format!("127.0.0.1:{host_port}")
            .parse()
            .map_err(|e| backend(format!("bad published addr: {e}")))
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    fn realization_configuration(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, RuntimeError> {
        Ok(std::collections::BTreeMap::from([
            ("backend".into(), "docker".into()),
            ("agent_port".into(), self.agent_port.to_string()),
        ]))
    }

    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        self.ping().await
    }

    fn enforces_network_none(&self) -> bool {
        true
    }

    fn supports_package_provisioning(&self) -> bool {
        true
    }

    fn uses_host_live_input_bind(&self) -> bool {
        true
    }

    async fn project_live_input(
        &self,
        container_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeError> {
        let root = self.live_inputs_root(container_id).await?;
        crate::live_inputs::project_host_input(&root, path, bytes)
    }

    async fn remove_live_input(&self, container_id: &str, path: &str) -> Result<(), RuntimeError> {
        let root = self.live_inputs_root(container_id).await?;
        crate::live_inputs::remove_host_input(&root, path)
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        let operation = async {
            if packages.is_empty() {
                return Ok(base_image.to_string());
            }
            let _build_guard = self.package_builds.lock().await;
            self.prune_package_cache().await;
            let (dockerfile, fingerprint, repository, image) =
                self.resolve_package_build(base_image, packages).await?;
            if self.package_registry.is_none() && self.docker.inspect_image(&image).await.is_ok() {
                return self.package_image_reference(&image, &repository).await;
            }
            if self.package_registry.is_some() {
                let mut pull = self.docker.create_image(
                    Some(CreateImageOptions {
                        from_image: repository.clone(),
                        tag: fingerprint.clone(),
                        ..Default::default()
                    }),
                    None,
                    self.package_registry_credentials.clone(),
                );
                let mut pulled = true;
                while let Some(result) = pull.next().await {
                    if result.is_err() {
                        pulled = false;
                        break;
                    }
                }
                if pulled && self.docker.inspect_image(&image).await.is_ok() {
                    return self.package_image_reference(&image, &repository).await;
                }
                // A local cache hit is not evidence that another worker can pull the
                // image. Repair an empty/expired registry from the deterministic local
                // tag before returning a shared digest.
                if self.docker.inspect_image(&image).await.is_ok() {
                    let mut push = self.docker.push_image(
                        &repository,
                        Some(PushImageOptions {
                            tag: fingerprint.clone(),
                        }),
                        self.package_registry_credentials.clone(),
                    );
                    while let Some(result) = push.next().await {
                        result.map_err(docker_backend)?;
                    }
                    return self.package_image_reference(&image, &repository).await;
                }
            }

            let mut context = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_size(dockerfile.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            context
                .append_data(&mut header, "Containerfile", dockerfile.as_bytes())
                .map_err(backend)?;
            let context = context.into_inner().map_err(backend)?;
            let options = BuildImageOptions::<String> {
                dockerfile: "Containerfile".into(),
                t: image.clone(),
                rm: true,
                forcerm: true,
                networkmode: match network {
                    pc::NetworkPolicy::Unrestricted => "default".into(),
                    pc::NetworkPolicy::None => "none".into(),
                    pc::NetworkPolicy::Allowlist { .. } => {
                        return Err(backend(
                            "package image build has no no-bypass allowlist network",
                        ));
                    }
                },
                ..Default::default()
            };
            let mut build = self.docker.build_image(options, None, Some(context.into()));
            while let Some(result) = build.next().await {
                result.map_err(docker_backend)?;
            }
            self.docker.inspect_image(&image).await.map_err(backend)?;
            if self.package_registry.is_some() {
                let mut push = self.docker.push_image(
                    &repository,
                    Some(PushImageOptions {
                        tag: fingerprint.clone(),
                    }),
                    self.package_registry_credentials.clone(),
                );
                while let Some(result) = push.next().await {
                    result.map_err(docker_backend)?;
                }
                return self.package_image_reference(&image, &repository).await;
            }
            Ok(image)
        };
        tokio::time::timeout(self.package_build_timeout, operation)
            .await
            .map_err(|_| backend("package image preparation exceeded its deadline"))?
    }

    async fn preflight_create_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        _plan: &ContainerPlan,
        realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<(), RuntimeError> {
        self.create_decision(context, realization_fingerprint)
            .await
            .map(drop)
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let fingerprint = legacy_unfenced_fingerprint(id);
        let attempt = crate::ContainerCreateAttempt::fresh();
        let intent = ContainerRealizationIntent::Create;
        let context = ContainerRealizationContext::new(id, &fingerprint, None, &intent, &attempt);
        self.create_for_effect(&context, plan, &fingerprint).await
    }

    async fn create_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) -> Result<String, RuntimeError> {
        // Stable namespace+scope and immutable fingerprint are recovery
        // evidence. The process incarnation remains a diagnostic/liveness label
        // and never participates in identity or deletion authorization.
        let mut labels = HashMap::new();
        labels.insert(MANAGED_SANDBOX_LABEL.to_string(), "1".to_string());
        labels.insert(RUNTIME_OWNER_LABEL.to_string(), self.owner_id.clone());
        labels.insert(
            SANDBOX_SCOPE_LABEL.to_string(),
            sandbox_scope_identity(self.realization_namespace.as_str(), context.scope)?,
        );
        labels.insert(
            SANDBOX_ADOPTION_LABEL.to_string(),
            context.adoption_fingerprint.to_string(),
        );
        labels.insert(
            SANDBOX_REALIZATION_LABEL.to_string(),
            realization_fingerprint.to_string(),
        );
        labels.insert(
            SANDBOX_ATTEMPT_LABEL.to_string(),
            context.attempt.as_str().to_owned(),
        );
        labels.extend(
            container_effect_label_values(context.effect_fence)
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value)),
        );
        let config = self.container_config(plan, labels);
        let name = runtime_container_name(self.realization_namespace.as_str(), context.scope)?;
        let mut decision = self
            .create_decision(context, Some(realization_fingerprint))
            .await?;
        for _ in 0..4 {
            match decision {
                ExistingRealizationDecision::Create => {
                    let created = match self
                        .docker
                        .create_container(
                            Some(CreateContainerOptions {
                                name: name.clone(),
                                platform: None,
                            }),
                            config.clone(),
                        )
                        .await
                    {
                        Ok(created) => created,
                        Err(error) => {
                            let after = self
                                .create_decision(context, Some(realization_fingerprint))
                                .await
                                .map_err(RuntimeError::after_mutation)?;
                            if after == ExistingRealizationDecision::Create {
                                return Err(backend(error).after_mutation());
                            }
                            decision = after;
                            continue;
                        }
                    };
                    if let Err(error) = self
                        .docker
                        .start_container(&created.id, None::<StartContainerOptions<String>>)
                        .await
                    {
                        let after = self
                            .create_decision(context, Some(realization_fingerprint))
                            .await
                            .map_err(RuntimeError::after_mutation)?;
                        if let ExistingRealizationDecision::ReuseReady(observed) = &after {
                            return Ok(observed.incarnation.identity.clone());
                        }
                        if after == ExistingRealizationDecision::Create {
                            return Err(backend(error).after_mutation());
                        }
                        decision = after;
                        continue;
                    }
                    return Ok(created.id);
                }
                ExistingRealizationDecision::ConvergeCreating(observed) => {
                    if let Err(error) = self
                        .docker
                        .start_container(
                            &observed.incarnation.identity,
                            None::<StartContainerOptions<String>>,
                        )
                        .await
                    {
                        let after = self
                            .create_decision(context, Some(realization_fingerprint))
                            .await
                            .map_err(RuntimeError::after_mutation)?;
                        if let ExistingRealizationDecision::ReuseReady(observed) = &after {
                            return Ok(observed.incarnation.identity.clone());
                        }
                        if after == ExistingRealizationDecision::Create {
                            return Err(backend(error).after_mutation());
                        }
                        decision = after;
                        continue;
                    }
                    return Ok(observed.incarnation.identity);
                }
                ExistingRealizationDecision::ReuseReady(observed) => {
                    return Ok(observed.incarnation.identity);
                }
                ExistingRealizationDecision::ReplaceExact(observed) => {
                    let removal = self
                        .docker
                        .remove_container(
                            &observed.incarnation.identity,
                            Some(RemoveContainerOptions {
                                force: observed.phase != ExistingRealizationPhase::Terminal,
                                ..Default::default()
                            }),
                        )
                        .await;
                    let after = self
                        .create_decision(context, Some(realization_fingerprint))
                        .await
                        .map_err(RuntimeError::after_mutation)?;
                    if after == ExistingRealizationDecision::Create {
                        decision = after;
                    } else if removal.is_err() {
                        return Err(backend(removal.unwrap_err()).after_mutation());
                    } else {
                        decision = after;
                    }
                }
                ExistingRealizationDecision::ValidateExisting(_) => {
                    return Err(backend(
                        "Docker create reached a fingerprint-deferred decision",
                    ));
                }
            }
        }
        Err(backend("Docker exact realization did not converge").after_mutation())
    }

    async fn recover_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<Option<RuntimeRestoreTarget>, RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(backend("Docker restore plan fingerprint mismatch"));
        }
        let name = restore_container_name(id);
        let info = match self.docker.inspect_container(&name, None).await {
            Ok(info) => info,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(None),
            Err(error) => return Err(backend(error)),
        };
        let container_id = Self::exact_restoration_id(&info, plan_fingerprint, evidence)?;
        match info.state.as_ref().and_then(|state| state.status) {
            Some(ContainerStateStatusEnum::RUNNING) => {}
            Some(ContainerStateStatusEnum::CREATED) => self
                .docker
                .start_container(&container_id, None::<StartContainerOptions<String>>)
                .await
                .map_err(backend)?,
            _ => {
                return Err(backend(
                    "Docker exact restore target is not recoverably running",
                ));
            }
        }
        Ok(Some(RuntimeRestoreTarget {
            container_id,
            disposition: pc::SandboxRestoreTargetDisposition::Recovered,
        }))
    }

    async fn restore_or_adopt(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<RuntimeRestoreTarget, RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(backend("Docker restore plan fingerprint mismatch"));
        }
        let name = restore_container_name(id);
        let mut labels = HashMap::new();
        labels.insert(MANAGED_SANDBOX_LABEL.to_string(), "1".to_string());
        labels.insert(RUNTIME_OWNER_LABEL.to_string(), self.owner_id.clone());
        labels.insert(
            crate::RESTORE_PLAN_LABEL.to_string(),
            plan_fingerprint.to_string(),
        );
        labels.extend(
            restoration_metadata(evidence)
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        let config = self.container_config(plan, labels);
        match self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: name.clone(),
                    platform: None,
                }),
                config,
            )
            .await
        {
            Ok(created) => {
                let _ = self
                    .docker
                    .start_container(&created.id, None::<StartContainerOptions<String>>)
                    .await;
                Ok(RuntimeRestoreTarget {
                    container_id: created.id,
                    disposition: pc::SandboxRestoreTargetDisposition::Created,
                })
            }
            Err(create_error) => self
                .recover_restore_target(id, plan, plan_fingerprint, evidence)
                .await?
                .ok_or_else(|| backend(create_error)),
        }
    }

    async fn restoration_evidence(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        if info.state.as_ref().and_then(|state| state.status)
            != Some(ContainerStateStatusEnum::RUNNING)
        {
            return Err(backend("Docker restored target is not running"));
        }
        Self::restoration_evidence_from_info(&info)
    }

    async fn restoration_plan_fingerprint(
        &self,
        container_id: &str,
    ) -> Result<Option<String>, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        Ok(Self::restoration_plan_fingerprint_from_info(&info))
    }

    async fn dispose_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<(), RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(backend("Docker restore cleanup plan fingerprint mismatch"));
        }
        let name = restore_container_name(id);
        let info = match self.docker.inspect_container(&name, None).await {
            Ok(info) => info,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        let container_id = Self::exact_restoration_id(&info, plan_fingerprint, evidence)?;
        let live_inputs = info
            .mounts
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|mount| mount.destination.as_deref() == Some(crate::LIVE_INPUTS_ROOT))
            .and_then(|mount| mount.source.as_deref())
            .ok_or_else(|| backend("Docker restored target has no managed live-input root bind"))?;
        let staging_root = std::path::Path::new(live_inputs)
            .parent()
            .ok_or_else(|| backend("Docker restored live-input root has no staging parent"))?;
        crate::remove_host_staging_path(staging_root)?;
        self.docker
            .remove_container(
                &container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn handle_extra(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::ContainerContinuationHandle>, RuntimeError> {
        if self.restoration_evidence(container_id).await?.is_none() {
            return Ok(None);
        }
        let live_inputs = self.live_inputs_root(container_id).await?;
        let staging_root = live_inputs
            .parent()
            .ok_or_else(|| backend("Docker live-input root has no staging parent"))?;
        Ok(Some(pc::ContainerContinuationHandle::HostBindRestoration(
            pc::HostBindRestorationHandle::for_restore(staging_root.to_string_lossy().into_owned())
                .map_err(|error| backend(error.to_string()))?,
        )))
    }

    async fn observe(
        &self,
        expectation: crate::ContainerObservationExpectation<'_>,
    ) -> Result<pc::SandboxObservation, RuntimeError> {
        if expectation.runtime_handle.is_some() {
            return Err(backend(
                "Docker observation received foreign runtime continuation evidence",
            ));
        }
        let expected_incarnation = expectation
            .realization_fingerprint
            .map(|_| expectation.container_id);
        let inspected = match self
            .docker
            .inspect_container(expectation.container_id, None)
            .await
        {
            Ok(inspected) => inspected,
            Err(error) if docker_not_found(&error) => {
                return sandbox_observation(
                    expected_incarnation,
                    expectation.adoption_fingerprint,
                    expectation.realization_fingerprint,
                    expectation.effect_fence,
                    &[],
                );
            }
            Err(error) => return Err(backend(error)),
        };
        sandbox_observation(
            expected_incarnation,
            expectation.adoption_fingerprint,
            expectation.realization_fingerprint,
            expectation.effect_fence,
            &[docker_realization(&inspected)?],
        )
    }

    async fn spawn(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        if command.argv.is_empty() {
            return Err(backend("exec command argv is empty"));
        }
        let env = exec_env(&command)?;
        let working_dir = (!command.cwd.is_empty()).then_some(command.cwd.clone());
        let (pid_file, argv) = wrapped_exec_argv(command.argv);
        let request = self
            .docker
            .create_exec(
                container_id,
                CreateExecOptions {
                    cmd: Some(argv),
                    env: Some(env),
                    working_dir,
                    ..Default::default()
                },
            )
            .await
            .map_err(backend)?;
        match self
            .docker
            .start_exec(
                &request.id,
                Some(StartExecOptions {
                    detach: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(backend)?
        {
            StartExecResults::Detached => Ok(Box::new(DockerExecProcess::new(
                self.docker.clone(),
                container_id,
                request.id,
                pid_file,
            ))),
            StartExecResults::Attached { .. } => {
                Err(backend("docker returned attached result for detached exec"))
            }
        }
    }

    async fn spawn_agent(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        if command.argv.is_empty() {
            return Err(backend("agent exec command argv is empty"));
        }
        let env = exec_env(&command)?;
        let working_dir = (!command.cwd.is_empty()).then_some(command.cwd.clone());
        let (pid_file, argv) = wrapped_exec_argv(command.argv);
        let request = self
            .docker
            .create_exec(
                container_id,
                CreateExecOptions {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(false),
                    cmd: Some(argv),
                    env: Some(env),
                    working_dir,
                    ..Default::default()
                },
            )
            .await
            .map_err(backend)?;
        let (mut output, input) = match self
            .docker
            .start_exec(&request.id, None::<StartExecOptions>)
            .await
            .map_err(backend)?
        {
            StartExecResults::Attached { output, input } => (output, input),
            StartExecResults::Detached => {
                return Err(backend("docker returned detached result for agent exec"));
            }
        };
        let (mut output_writer, output_reader) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            while let Some(frame) = output.next().await {
                match frame {
                    Ok(frame) if output_writer.write_all(frame.as_ref()).await.is_ok() => {}
                    _ => break,
                }
            }
        });
        Ok(RuntimeAgentProcess {
            process: Box::new(DockerExecProcess::new(
                self.docker.clone(),
                container_id,
                request.id,
                pid_file,
            )),
            channel: Box::new(SplitChannel::new(output_reader, input)),
        })
    }

    async fn process(
        &self,
        container_id: &str,
        process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        let process = DockerExecProcess::recovered(self.docker.clone(), container_id, process_id);
        let state = self
            .docker
            .inspect_exec(&process.exec_id)
            .await
            .map_err(backend)?;
        if state.container_id.as_deref() != Some(container_id) {
            return Err(backend(format!(
                "exec {process_id} does not belong to container {container_id}"
            )));
        }
        Ok(Box::new(process))
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // The agent is the container's main process; reach its stdio over the published
        // port (awaken-next dials, it does not `docker exec`). The agent needs a moment
        // to bind its port after the container starts, so retry the port lookup + dial
        // with a short backoff: the FIRST turn on a COLD container must not race the
        // bind (a warm/reused container connects on the first attempt). Bounded (~6s)
        // so a genuinely dead agent still fails closed.
        // Docker may publish the host port before the container process has bound its
        // listener. During that window a TCP connect succeeds through docker-proxy but
        // the first read is reset, which a dial-only retry cannot observe. Give a cold
        // process one bounded settle interval before accepting a channel as ready.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut last: Option<RuntimeError> = None;
        for _ in 0..40 {
            match self.agent_addr(container_id).await {
                Ok(addr) => match TcpAgentTransport::new(addr).open_channel().await {
                    Ok(channel) => return Ok(channel),
                    Err(e) => last = Some(backend(e)),
                },
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        Err(last.unwrap_or_else(|| backend("agent channel never became reachable")))
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        let running = info.state.as_ref().and_then(|s| s.running).unwrap_or(false);
        Ok(if running {
            ContainerState::Running
        } else {
            ContainerState::Gone
        })
    }

    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        let mut stream = self
            .docker
            .wait_container(container_id, None::<WaitContainerOptions<String>>);
        match stream.next().await {
            Some(Ok(resp)) => Ok(pc::ExitStatus {
                code: Some(resp.status_code as i32),
                signaled: false,
            }),
            Some(Err(e)) => Err(backend(e)),
            None => Err(RuntimeError::NotFound(container_id.into())),
        }
    }

    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        let info = self
            .docker
            .inspect_container(container_id, None)
            .await
            .map_err(backend)?;
        let state = info.state.unwrap_or_default();
        if state.running.unwrap_or(false) {
            return Ok(None);
        }
        Ok(Some(pc::ExitStatus {
            code: state.exit_code.map(|c| c as i32),
            signaled: false,
        }))
    }

    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        self.docker
            .kill_container(
                container_id,
                Some(KillContainerOptions {
                    signal: signal_name(signal),
                }),
            )
            .await
            .map_err(backend)
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        // Out-of-band: artifacts are listed from the outputs volume/object store by
        // the deployment, not streamed through the Engine API. Wired per deployment.
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        // A fallback path via the Engine API: copy the file out as a tar stream.
        let mut stream = self.docker.download_from_container(
            container_id,
            Some(DownloadFromContainerOptions { path: artifact_id }),
        );
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk.map_err(backend)?);
        }
        Ok(buf)
    }

    async fn touch_lease(&self, container_id: &str) -> Result<(), RuntimeError> {
        // Docker labels are immutable. Renewal therefore proves the target remains
        // live; the durable Session/realization lease carries ownership authority.
        if self.inspect(container_id).await? != ContainerState::Running {
            return Err(RuntimeError::NotFound(container_id.into()));
        }
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.docker
            .remove_container(
                container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(backend)
    }

    async fn remove_exact_incarnation(
        &self,
        container_id: &str,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
        _authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), RuntimeError> {
        if runtime_handle.is_some() {
            return Err(backend(
                "Docker exact removal received foreign continuation evidence",
            ));
        }
        self.remove(container_id).await
    }
}

#[async_trait]
impl PackageImageProvisioner for DockerRuntime {
    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        self.docker
            .inspect_image(reference)
            .await
            .map_err(backend)?
            .id
            .filter(|identity| !identity.is_empty())
            .ok_or_else(|| backend("docker returned an empty base-image identity"))
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        ContainerRuntime::prepare_package_image(self, base_image, packages, network).await
    }

    async fn package_image_available(
        &self,
        _base_image: &str,
        _packages: &pc::PackageRequirements,
        _network: &pc::NetworkPolicy,
        image: &str,
    ) -> Result<bool, RuntimeError> {
        if self.package_registry.is_none() {
            return Ok(self.docker.inspect_image(image).await.is_ok());
        }
        let mut pull = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: image.to_string(),
                ..Default::default()
            }),
            None,
            self.package_registry_credentials.clone(),
        );
        while let Some(result) = pull.next().await {
            if result.is_err() {
                return Ok(false);
            }
        }
        Ok(self.docker.inspect_image(image).await.is_ok())
    }
}

#[cfg(test)]
mod observation_error_tests {
    use super::*;

    #[test]
    fn only_an_exact_not_found_response_proves_absence() {
        // Cause/effect table: C1 Docker response is 404/other HTTP/backend
        // failure. R1 only 404 is absence evidence; R2 every other failure is
        // indeterminate and must not authorize a replacement.
        let not_found = bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "missing".into(),
        };
        let unavailable = bollard::errors::Error::DockerResponseServerError {
            status_code: 503,
            message: "unavailable".into(),
        };
        assert!(docker_not_found(&not_found), "R1");
        assert!(!docker_not_found(&unavailable), "R2");
        assert!(
            !docker_not_found(&bollard::errors::Error::RequestTimeoutError),
            "R2"
        );
    }
}
