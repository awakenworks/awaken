//! Exact Kubernetes projection and incarnation fence for Sandbox control.

use super::*;
use awaken_sandbox_control::{
    REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH, REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH,
    SANDBOX_CONTROL_DIRECTORY_PATH,
};
use k8s_openapi::api::core::v1::{ContainerPort, EmptyDirVolumeSource, ExecAction, Probe};

pub const DEFAULT_REPOSITORY_GIT_CONTROL_PORT: u16 = 7778;
const SANDBOX_CONTROL_CONTAINER: &str = "sandbox-control";
const SANDBOX_CONTROL_VOLUME: &str = "sandbox-control";

/// Concrete no-secret bridge installed in one hosted Kubernetes Pod.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct K8sSandboxControlForwarder {
    image: String,
    executable: String,
    port: u16,
}

impl K8sSandboxControlForwarder {
    pub fn new(
        image: impl Into<String>,
        executable: impl Into<String>,
        port: u16,
    ) -> Result<Self, RuntimeError> {
        let image = image.into();
        let executable = executable.into();
        let image_is_pinned = image
            .rsplit_once("@sha256:")
            .is_some_and(|(repository, digest)| {
                !repository.is_empty()
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if image.trim() != image
            || !image_is_pinned
            || image
                .chars()
                .any(|character| character.is_whitespace() || character == '\0')
            || !std::path::Path::new(&executable).is_absolute()
            || executable
                .chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
            || port == 0
        {
            return Err(RuntimeError::Backend(
                "Kubernetes Sandbox control forwarder requires a digest-pinned operator image, absolute executable, and non-zero port".into(),
            ));
        }
        Ok(Self {
            image,
            executable,
            port,
        })
    }

    pub(super) const fn port(&self) -> u16 {
        self.port
    }
}

pub(super) fn projection(
    forwarder: &K8sSandboxControlForwarder,
) -> (Volume, VolumeMount, Container) {
    let volume = Volume {
        name: SANDBOX_CONTROL_VOLUME.into(),
        empty_dir: Some(EmptyDirVolumeSource::default()),
        ..Default::default()
    };
    let agent_mount = VolumeMount {
        name: SANDBOX_CONTROL_VOLUME.into(),
        mount_path: SANDBOX_CONTROL_DIRECTORY_PATH.into(),
        read_only: Some(true),
        ..Default::default()
    };
    let forwarder_container = Container {
        name: SANDBOX_CONTROL_CONTAINER.into(),
        image: Some(forwarder.image.clone()),
        image_pull_policy: Some("IfNotPresent".into()),
        command: Some(vec![
            forwarder.executable.clone(),
            "control-forwarder".into(),
            "--unix".into(),
            REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH.into(),
            "--listen".into(),
            format!("127.0.0.1:{}", forwarder.port),
            "--ready".into(),
            REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH.into(),
        ]),
        ports: Some(vec![ContainerPort {
            container_port: i32::from(forwarder.port),
            name: Some(SANDBOX_CONTROL_CONTAINER.into()),
            protocol: Some("TCP".into()),
            ..Default::default()
        }]),
        volume_mounts: Some(vec![VolumeMount {
            name: SANDBOX_CONTROL_VOLUME.into(),
            mount_path: SANDBOX_CONTROL_DIRECTORY_PATH.into(),
            read_only: Some(false),
            ..Default::default()
        }]),
        // The trusted forwarder writes this generation marker only after both
        // Unix and loopback listeners bind. The same digest-pinned binary checks
        // PID+starttime through exec; no probe connects to or consumes the
        // one-exchange business port.
        readiness_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec![
                    forwarder.executable.clone(),
                    "control-forwarder-ready".into(),
                    "--marker".into(),
                    REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH.into(),
                ]),
            }),
            failure_threshold: Some(3),
            initial_delay_seconds: Some(0),
            period_seconds: Some(1),
            success_threshold: Some(1),
            timeout_seconds: Some(1),
            ..Default::default()
        }),
        security_context: Some(hardened_security_context()),
        termination_message_path: Some("/dev/termination-log".into()),
        termination_message_policy: Some("File".into()),
        ..Default::default()
    };
    (volume, agent_mount, forwarder_container)
}

fn normalized_forwarder_security_projection(container: &Container) -> Container {
    let mut projected = container.clone();
    // LimitRange/defaulting may add resource requests/limits without changing
    // executable or trust topology. All other fields remain exact, including
    // image digest, argv, loopback port, mounts, security context, and exec probe.
    projected.resources = None;
    if projected.termination_message_path.as_deref() == Some("/dev/termination-log") {
        projected.termination_message_path = None;
    }
    if projected.termination_message_policy.as_deref() == Some("File") {
        projected.termination_message_policy = None;
    }
    projected
}

/// Render the canonical demanded-control Pod shape using one validated,
/// digest-pinned operator forwarder artifact.
#[must_use]
pub fn pod_for_plan_with_control_forwarder(
    id: &str,
    plan: &ContainerPlan,
    forwarder: &K8sSandboxControlForwarder,
) -> Pod {
    build_pod_with_continuation(id, plan, &None, None, &[], None, Some(forwarder))
}

pub(super) fn pod_incarnation(
    pod: &Pod,
    expected_name: &str,
    expected_owner: Option<&str>,
    forwarder: &K8sSandboxControlForwarder,
) -> Result<pc::SandboxControlIncarnation, RuntimeError> {
    if pod.metadata.name.as_deref() != Some(expected_name) {
        return Err(backend(
            "Kubernetes Sandbox control Pod name does not match its durable locator",
        ));
    }
    let labels = pod
        .metadata
        .labels
        .as_ref()
        .ok_or_else(|| backend("Kubernetes Sandbox control Pod has no ownership labels"))?;
    if labels.get(crate::MANAGED_SANDBOX_LABEL).map(String::as_str) != Some("1") {
        return Err(backend(
            "Kubernetes Sandbox control Pod is not a managed Sandbox realization",
        ));
    }
    if let Some(expected_owner) = expected_owner
        && labels.get(crate::RUNTIME_OWNER_LABEL).map(String::as_str) != Some(expected_owner)
    {
        return Err(backend(
            "Kubernetes Sandbox control Pod runtime-owner lease is not current",
        ));
    }
    let spec = pod
        .spec
        .as_ref()
        .ok_or_else(|| backend("Kubernetes Sandbox control Pod has no specification"))?;
    if spec.automount_service_account_token != Some(false)
        || super::has_forbidden_sandbox_namespace_shape(spec)
    {
        return Err(backend(
            "Kubernetes Sandbox control Pod has a forbidden token or namespace shape",
        ));
    }
    let (expected_volume, expected_agent_mount, expected_forwarder) = projection(forwarder);
    let control_volumes = spec
        .volumes
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|volume| volume.name == expected_volume.name)
        .collect::<Vec<_>>();
    if control_volumes.len() != 1 || control_volumes[0] != &expected_volume {
        return Err(backend(
            "Kubernetes Sandbox control Pod has no exact private emptyDir",
        ));
    }

    let agents = spec
        .containers
        .iter()
        .filter(|container| container.name == "agent")
        .collect::<Vec<_>>();
    let agent = agents
        .first()
        .filter(|_| agents.len() == 1)
        .ok_or_else(|| backend("Kubernetes Sandbox control Pod has no exact agent container"))?;
    let agent_control_mounts = agent
        .volume_mounts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|mount| mount.name == expected_agent_mount.name)
        .collect::<Vec<_>>();
    if agent_control_mounts.len() != 1 || agent_control_mounts[0] != &expected_agent_mount {
        return Err(backend(
            "Kubernetes Sandbox control Pod does not expose one exact read-only control mount",
        ));
    }

    let forwarders = spec
        .containers
        .iter()
        .filter(|container| container.name == SANDBOX_CONTROL_CONTAINER)
        .collect::<Vec<_>>();
    if forwarders.len() != 1
        || normalized_forwarder_security_projection(forwarders[0])
            != normalized_forwarder_security_projection(&expected_forwarder)
    {
        return Err(backend(
            "Kubernetes Sandbox control Pod forwarder realization is not exact",
        ));
    }

    let uid = pod
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox control Pod has no UID"))?;
    pc::SandboxControlIncarnation::kubernetes_pod(uid).map_err(|error| backend(error.to_string()))
}

fn ready_pod_incarnation(
    pod: &Pod,
    expected_name: &str,
    expected_owner: Option<&str>,
    forwarder: &K8sSandboxControlForwarder,
) -> Result<pc::SandboxControlIncarnation, RuntimeError> {
    match realization::pod_readiness(pod) {
        realization::PodReadiness::Ready => {
            pod_incarnation(pod, expected_name, expected_owner, forwarder)
        }
        realization::PodReadiness::Waiting(_) | realization::PodReadiness::Failed(_) => Err(
            backend("Kubernetes Sandbox control Pod is not currently ready"),
        ),
    }
}

impl K8sRuntime {
    pub(super) async fn observed_sandbox_control_incarnation(
        &self,
        container_id: &str,
        forwarder: &K8sSandboxControlForwarder,
    ) -> Result<pc::SandboxControlIncarnation, RuntimeError> {
        let pod = self.pods().get(container_id).await.map_err(backend)?;
        ready_pod_incarnation(&pod, container_id, Some(&self.owner_id), forwarder)
    }

    pub(super) async fn adopt_sandbox_control_incarnation(
        &self,
        container_id: &str,
        expected: &pc::SandboxControlIncarnation,
        forwarder: &K8sSandboxControlForwarder,
    ) -> Result<pc::SandboxControlIncarnation, RuntimeError> {
        let pods = self.pods();
        let pod = pods.get(container_id).await.map_err(backend)?;
        let observed = ready_pod_incarnation(&pod, container_id, None, forwarder)?;
        if &observed != expected {
            return Err(backend(
                "adopted Kubernetes Sandbox control Pod incarnation changed",
            ));
        }
        let expected_uid = expected
            .kubernetes_pod_uid()
            .ok_or_else(|| backend("Sandbox control incarnation is not a Kubernetes Pod"))?;
        let transferred =
            realization::transfer_runtime_owner(&pods, pod, expected_uid, &self.owner_id).await?;
        let transferred =
            ready_pod_incarnation(&transferred, container_id, Some(&self.owner_id), forwarder)?;
        if &transferred != expected {
            return Err(backend(
                "adopted Kubernetes Sandbox control Pod changed during owner transfer",
            ));
        }
        Ok(transferred)
    }
}

pub(super) fn services(
    runtime: &K8sRuntime,
) -> std::collections::BTreeSet<SandboxControlServiceKind> {
    runtime
        .sandbox_control_forwarder
        .as_ref()
        .map_or_else(std::collections::BTreeSet::new, |_| {
            std::collections::BTreeSet::from([SandboxControlServiceKind::RepositoryGitCredential])
        })
}

pub(super) async fn bind(
    runtime: &K8sRuntime,
    container_id: &str,
    request: SandboxControlBindingRequest<'_>,
) -> Result<Option<pc::SandboxControlIncarnation>, RuntimeError> {
    if request.required().is_empty() {
        return Ok(None);
    }
    let forwarder = runtime
        .sandbox_control_forwarder
        .as_ref()
        .ok_or_else(|| backend("Kubernetes Sandbox control forwarder is not installed"))?;
    if !request.required().is_subset(&services(runtime)) {
        return Err(backend(
            "Kubernetes runtime cannot bind every requested Sandbox control service",
        ));
    }
    match request {
        SandboxControlBindingRequest::New { .. } => Ok(Some(
            runtime
                .observed_sandbox_control_incarnation(container_id, forwarder)
                .await?,
        )),
        SandboxControlBindingRequest::Adopt {
            expected: Some(expected),
            ..
        } => Ok(Some(
            runtime
                .adopt_sandbox_control_incarnation(container_id, expected, forwarder)
                .await?,
        )),
        SandboxControlBindingRequest::Adopt { expected: None, .. } => Err(backend(
            "adopted Kubernetes Sandbox has no durable control Pod incarnation",
        )),
    }
}

pub(super) async fn open_channel(
    runtime: &K8sRuntime,
    container_id: &str,
    binding: &pc::SandboxControlIncarnation,
    kind: SandboxControlServiceKind,
) -> Result<Box<dyn AgentChannel>, RuntimeError> {
    let forwarder = runtime
        .sandbox_control_forwarder
        .as_ref()
        .filter(|_| kind == SandboxControlServiceKind::RepositoryGitCredential)
        .ok_or_else(|| {
            RuntimeError::Backend("Kubernetes Sandbox control forwarder is not installed".into())
        })?;
    let before = runtime
        .observed_sandbox_control_incarnation(container_id, forwarder)
        .await?;
    if &before != binding {
        return Err(backend(
            "Kubernetes Sandbox control Pod changed before channel establishment",
        ));
    }
    // Pod readiness already proves the trusted current-generation marker. This
    // authenticated exact-Pod port-forward is the first real business channel;
    // it is never consumed by readiness probing.
    let channel =
        channel::open_pod_channel(&runtime.streaming_pods(), container_id, forwarder.port())
            .await?;
    let after = runtime
        .observed_sandbox_control_incarnation(container_id, forwarder)
        .await?;
    if &after != binding {
        return Err(backend(
            "Kubernetes Sandbox control Pod changed during channel establishment",
        ));
    }
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarder() -> K8sSandboxControlForwarder {
        K8sSandboxControlForwarder::new(
            format!("registry.test/awaken-sandbox@sha256:{}", "a".repeat(64)),
            "/usr/local/bin/awaken-sandbox",
            DEFAULT_REPOSITORY_GIT_CONTROL_PORT,
        )
        .unwrap()
    }

    fn demanded_plan() -> ContainerPlan {
        ContainerPlan {
            image: "agent:test".into(),
            command: vec!["sleep".into(), "30".into()],
            env: Vec::new(),
            control_services: std::collections::BTreeSet::from([
                SandboxControlServiceKind::RepositoryGitCredential,
            ]),
            packages: Default::default(),
            binds: Vec::new(),
            outputs_volume: "/mnt/session/outputs".into(),
            network: crate::NetworkMode::Open,
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            memory_mounts: Vec::new(),
            rootfs: crate::RootfsPlan::Image("agent:test".into()),
        }
    }

    fn realized_pod() -> Pod {
        let forwarder = forwarder();
        let mut pod =
            pod_for_plan_with_control_forwarder("sandbox-a", &demanded_plan(), &forwarder);
        pod.metadata.uid = Some("pod-uid-a".into());
        let labels = pod.metadata.labels.get_or_insert_with(Default::default);
        labels.insert(crate::MANAGED_SANDBOX_LABEL.into(), "1".into());
        labels.insert(crate::RUNTIME_OWNER_LABEL.into(), "owner-a".into());
        pod
    }

    #[test]
    fn forwarder_configuration_requires_exact_operator_artifact() {
        /* K1 cause/effect table: C1=digest-pinned image; C2=absolute
         * executable; C3=non-zero private port. E1=typed forwarder. Missing
         * any cause => E2 reject before Pod creation.
         */
        assert!(
            K8sSandboxControlForwarder::new(
                format!("registry.test/image@sha256:{}", "a".repeat(64)),
                "/bin/awaken-sandbox",
                7778,
            )
            .is_ok(),
            "K1/E1"
        );
        assert!(
            K8sSandboxControlForwarder::new(
                "registry.test/image:latest",
                "/bin/awaken-sandbox",
                7778
            )
            .is_err(),
            "K1/E2 pin"
        );
        assert!(
            K8sSandboxControlForwarder::new(
                format!("registry.test/image@sha256:{}", "A".repeat(64)),
                "/bin/awaken-sandbox",
                7778,
            )
            .is_err(),
            "K1/E2 canonical digest"
        );
        assert!(
            K8sSandboxControlForwarder::new(
                format!("registry.test/image@sha256:{}", "a".repeat(64)),
                "awaken-sandbox",
                7778,
            )
            .is_err(),
            "K1/E2 executable"
        );
        assert!(
            K8sSandboxControlForwarder::new(
                format!("registry.test/image@sha256:{}", "a".repeat(64)),
                "/bin/awaken-sandbox",
                0,
            )
            .is_err(),
            "K1/E2 port"
        );
    }

    #[test]
    fn projection_uses_marker_exec_readiness_and_incarnation_checks_security_shape() {
        /* K2/K3 cause/effect table:
         * C1=typed forwarder demand; C2=exact managed Pod name/UID/owner;
         * C3=exact emptyDir, read-only Agent mount, command, loopback port,
         * image, marker exec probe, token, and control security shape;
         * C4=LimitRange adds resources or the API adds known false/empty/message
         * defaults; C5=any executable/control field or host/shared/ephemeral
         * namespace shape changes. E1=one durable Pod incarnation;
         * E2=no TCP/HTTP business-port probe; E3=C4 remains admissible;
         * E4=C5 rejects adoption/channel authority. Rules:
         * K2 C1+C2+C3=>E1+E2; K3 C4=>E3; K4 !C2 or C5=>E4.
         */
        let forwarder = forwarder();
        let (_, _, sidecar) = projection(&forwarder);
        let probe = sidecar.readiness_probe.as_ref().expect("K2 marker probe");
        assert!(
            probe.tcp_socket.is_none() && probe.http_get.is_none(),
            "K2/E2"
        );
        assert!(
            probe
                .exec
                .as_ref()
                .and_then(|exec| exec.command.as_ref())
                .is_some_and(|command| command
                    == &vec![
                        "/usr/local/bin/awaken-sandbox".to_string(),
                        "control-forwarder-ready".to_string(),
                        "--marker".to_string(),
                        REPOSITORY_GIT_CREDENTIAL_READY_MARKER_PATH.to_string(),
                    ]),
            "K2 trusted exec marker",
        );
        assert!(
            sidecar
                .command
                .as_ref()
                .is_some_and(|argv| argv.iter().any(|arg| arg == "127.0.0.1:7778")),
            "K2 loopback only"
        );
        let pod = realized_pod();
        let expected_name = pod_name("sandbox-a");
        assert_eq!(
            pod.metadata.name.as_deref(),
            Some(expected_name.as_str()),
            "K2 exact durable locator"
        );
        let incarnation =
            pod_incarnation(&pod, &expected_name, Some("owner-a"), &forwarder).expect("K2/E1");
        assert_eq!(incarnation.kubernetes_pod_uid(), Some("pod-uid-a"));
        assert!(
            pod_incarnation(&pod, "awaken-other", Some("owner-a"), &forwarder).is_err(),
            "K4/E4 durable locator"
        );

        let mut defaulted = pod.clone();
        let defaulted_spec = defaulted.spec.as_mut().unwrap();
        defaulted_spec.host_network = Some(false);
        defaulted_spec.host_pid = Some(false);
        defaulted_spec.host_ipc = Some(false);
        defaulted_spec.share_process_namespace = Some(false);
        defaulted_spec.ephemeral_containers = Some(Vec::new());
        let sidecar = defaulted_spec
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap();
        sidecar.resources = Some(k8s_openapi::api::core::v1::ResourceRequirements {
            requests: Some(std::collections::BTreeMap::from([(
                "cpu".into(),
                k8s_openapi::apimachinery::pkg::api::resource::Quantity("10m".into()),
            )])),
            ..Default::default()
        });
        sidecar.termination_message_path = None;
        sidecar.termination_message_policy = None;
        assert!(
            pod_incarnation(&defaulted, &expected_name, Some("owner-a"), &forwarder).is_ok(),
            "K3/E3 LimitRange resources and known API message defaults"
        );

        let namespace_mutations: [crate::k8s::PodSpecMutation; 5] = [
            ("host network", |spec| spec.host_network = Some(true)),
            ("host PID", |spec| spec.host_pid = Some(true)),
            ("host IPC", |spec| spec.host_ipc = Some(true)),
            ("shared process namespace", |spec| {
                spec.share_process_namespace = Some(true)
            }),
            ("ephemeral container", |spec| {
                spec.ephemeral_containers = Some(vec![Default::default()])
            }),
        ];
        for (cause, mutate) in namespace_mutations {
            let mut changed = pod.clone();
            mutate(changed.spec.as_mut().unwrap());
            assert!(
                pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
                "K4/E4 {cause}"
            );
        }

        let mut changed = pod.clone();
        let sidecar = changed
            .spec
            .as_mut()
            .unwrap()
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap();
        sidecar.args = Some(vec!["unexpected".into()]);
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 appended argv"
        );

        let mut changed = pod.clone();
        let sidecar = changed
            .spec
            .as_mut()
            .unwrap()
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap();
        sidecar.image = Some("registry.test/other@sha256:".to_string() + &"b".repeat(64));
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 image digest"
        );

        let mut changed = pod.clone();
        let sidecar = changed
            .spec
            .as_mut()
            .unwrap()
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap();
        *sidecar
            .command
            .as_mut()
            .unwrap()
            .iter_mut()
            .find(|argument| argument.starts_with("127.0.0.1:"))
            .unwrap() = "0.0.0.0:7778".into();
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 loopback command"
        );

        let mut changed = pod.clone();
        changed
            .spec
            .as_mut()
            .unwrap()
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap()
            .volume_mounts
            .as_mut()
            .unwrap()[0]
            .read_only = Some(true);
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 forwarder mount"
        );

        let mut changed = pod.clone();
        changed
            .spec
            .as_mut()
            .unwrap()
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap()
            .readiness_probe
            .as_mut()
            .unwrap()
            .exec
            .as_mut()
            .unwrap()
            .command = Some(vec!["/bin/true".into()]);
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 readiness executable"
        );

        let mut changed = pod.clone();
        changed
            .spec
            .as_mut()
            .unwrap()
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap()
            .security_context
            .as_mut()
            .unwrap()
            .allow_privilege_escalation = Some(true);
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 security context"
        );

        let mut changed = pod.clone();
        changed
            .spec
            .as_mut()
            .unwrap()
            .containers
            .iter_mut()
            .find(|container| container.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap()
            .ports
            .as_mut()
            .unwrap()[0]
            .host_port = Some(i32::from(DEFAULT_REPOSITORY_GIT_CONTROL_PORT));
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 port exposure"
        );

        let mut changed = pod.clone();
        changed
            .spec
            .as_mut()
            .unwrap()
            .automount_service_account_token = Some(true);
        assert!(
            pod_incarnation(&changed, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 token mount"
        );

        let mut wrong_owner = pod;
        wrong_owner
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert(crate::RUNTIME_OWNER_LABEL.into(), "owner-b".into());
        assert!(
            pod_incarnation(&wrong_owner, &expected_name, Some("owner-a"), &forwarder).is_err(),
            "K4/E4 owner fence"
        );
    }

    #[test]
    fn empty_control_demand_preserves_the_ordinary_pod_and_agent_command() {
        /* K4 cause/effect table: C1=empty control-service demand. E1=no
         * control volume, mount, sidecar, port, or argv; E2=the existing Agent
         * command remains byte-for-byte unchanged. Rule: K4 C1=>E1+E2.
         */
        let mut plan = demanded_plan();
        plan.control_services.clear();
        let pod = pod_for_plan("ordinary", &plan);
        let spec = pod.spec.unwrap();
        assert!(
            spec.containers
                .iter()
                .all(|container| container.name != SANDBOX_CONTROL_CONTAINER),
            "K4/E1 sidecar"
        );
        assert!(
            spec.volumes
                .as_deref()
                .unwrap_or_default()
                .iter()
                .all(|volume| volume.name != SANDBOX_CONTROL_VOLUME),
            "K4/E1 volume"
        );
        let agent = spec
            .containers
            .iter()
            .find(|container| container.name == "agent")
            .unwrap();
        assert_eq!(agent.command.as_ref(), Some(&plan.command), "K4/E2");
        assert!(
            agent
                .volume_mounts
                .as_deref()
                .unwrap_or_default()
                .iter()
                .all(|mount| mount.name != SANDBOX_CONTROL_VOLUME),
            "K4/E1 mount"
        );
    }

    #[test]
    fn control_authority_requires_one_current_ready_pod_incarnation() {
        /* Control-authority readiness table:
         * C1=agent and ordinary projector are ready;
         * C2=the control sidecar has not yet passed its marker exec probe;
         * C3=the current-generation marker later passes; C4=Pod is terminal or
         * deleting; C5=same name now has another UID. E1=C1+C2 denies control
         * authority; E2=C1+C3 returns the exact incarnation; E3=C4 denies;
         * E4=C5 yields a different incarnation that the adoption/channel fence
         * rejects. Rules KR1 C1+C2=>E1; KR2 C1+C3=>E2;
         * KR3 C4=>E3; KR4 C1+C3+C5=>E4. The sole helper is used before owner
         * transfer and before/after port-forward, so no rule consumes a
         * business channel while Waiting or terminal.
         */
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateRunning, ContainerStatus, PodStatus,
        };

        let mut pod = realized_pod();
        let expected_name = pod_name("sandbox-a");
        assert_eq!(
            pod.metadata.name.as_deref(),
            Some(expected_name.as_str()),
            "KR0 exact durable locator"
        );
        let status = |name: &str, ready: bool| ContainerStatus {
            name: name.into(),
            ready,
            image: "image@test".into(),
            image_id: String::new(),
            restart_count: 0,
            started: Some(true),
            state: Some(ContainerState {
                running: Some(ContainerStateRunning::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let names = pod
            .spec
            .as_ref()
            .unwrap()
            .containers
            .iter()
            .map(|container| container.name.clone())
            .collect::<Vec<_>>();
        pod.status = Some(PodStatus {
            phase: Some("Running".into()),
            container_statuses: Some(
                names
                    .iter()
                    .map(|name| status(name, name != SANDBOX_CONTROL_CONTAINER))
                    .collect(),
            ),
            ..Default::default()
        });
        assert!(
            matches!(
                realization::pod_readiness(&pod),
                realization::PodReadiness::Waiting(_)
            ),
            "KR1/E1"
        );
        assert!(
            ready_pod_incarnation(&pod, &expected_name, Some("owner-a"), &forwarder()).is_err(),
            "KR1/E1 no authority"
        );
        pod.status
            .as_mut()
            .unwrap()
            .container_statuses
            .as_mut()
            .unwrap()
            .iter_mut()
            .find(|status| status.name == SANDBOX_CONTROL_CONTAINER)
            .unwrap()
            .ready = true;
        assert_eq!(
            realization::pod_readiness(&pod),
            realization::PodReadiness::Ready,
            "KR2/E2"
        );
        let expected =
            ready_pod_incarnation(&pod, &expected_name, Some("owner-a"), &forwarder()).unwrap();
        assert_eq!(expected.kubernetes_pod_uid(), Some("pod-uid-a"), "KR2/E2");

        let mut terminal = pod.clone();
        terminal.status.as_mut().unwrap().phase = Some("Failed".into());
        assert!(
            ready_pod_incarnation(&terminal, &expected_name, Some("owner-a"), &forwarder(),)
                .is_err(),
            "KR3/E3 terminal"
        );

        let mut deleting = pod.clone();
        deleting.metadata.deletion_timestamp =
            Some(serde_json::from_str("\"2026-08-30T00:00:00Z\"").unwrap());
        assert!(
            ready_pod_incarnation(&deleting, &expected_name, Some("owner-a"), &forwarder(),)
                .is_err(),
            "KR3/E3 deleting"
        );

        let mut replaced = pod;
        replaced.metadata.uid = Some("pod-uid-b".into());
        let replacement =
            ready_pod_incarnation(&replaced, &expected_name, Some("owner-a"), &forwarder())
                .unwrap();
        assert_ne!(replacement, expected, "KR4/E4 UID fence");
    }
}
