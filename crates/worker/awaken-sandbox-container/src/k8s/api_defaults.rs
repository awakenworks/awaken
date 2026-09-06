//! Shared API-default representations for Kubernetes immutable projections.
//! Policy owners still decide which fields are governed; normalization never
//! removes explicit resource requirements, executable inputs or trust boundaries.

use k8s_openapi::api::core::v1::{Container, PodSpec, Toleration, Volume};

fn normalize_empty<T>(items: &mut Option<Vec<T>>) {
    if items.as_ref().is_some_and(Vec::is_empty) {
        *items = None;
    }
}

pub(crate) fn normalize_volume(volume: &mut Volume) {
    if let Some(claim) = volume.persistent_volume_claim.as_mut()
        && claim.read_only == Some(false)
    {
        claim.read_only = None;
    }
    if let Some(config) = volume.config_map.as_mut()
        && config.default_mode == Some(0o644)
    {
        config.default_mode = None;
    }
    if let Some(secret) = volume.secret.as_mut()
        && secret.default_mode == Some(0o644)
    {
        secret.default_mode = None;
    }
}

pub(crate) fn normalize_container(container: &mut Container) {
    // Image defaulting is shared by restore, package and control projections.
    // Digest or non-latest tag => IfNotPresent; untagged/latest => Always.
    let image = container.image.as_deref().unwrap_or_default();
    let tail = image.rsplit('/').next().unwrap_or_default();
    let default_pull = if image.contains('@') || (tail.contains(':') && !tail.ends_with(":latest")) {
        "IfNotPresent"
    } else { "Always" };
    if container.image_pull_policy.as_deref() == Some(default_pull) {
        container.image_pull_policy = None;
    }

    // Zero is the API's omitted initial-delay representation. Apply the same
    // rule to every probe role; positive delays and probe commands stay exact.
    for probe in [
        &mut container.readiness_probe,
        &mut container.liveness_probe,
        &mut container.startup_probe,
    ]
    .into_iter()
    .flatten()
    {
        if probe.initial_delay_seconds == Some(0) {
            probe.initial_delay_seconds = None;
        }
    }
    normalize_empty(&mut container.env);
    normalize_empty(&mut container.env_from);
    normalize_empty(&mut container.command);
    normalize_empty(&mut container.args);
    normalize_empty(&mut container.ports);
    normalize_empty(&mut container.volume_mounts);
    normalize_empty(&mut container.volume_devices);
    if let Some(mounts) = container.volume_mounts.as_mut() {
        for mount in mounts {
            if mount.read_only == Some(false) {
                mount.read_only = None;
            }
        }
    }
    if container
        .resources
        .as_ref()
        .is_some_and(|resources| resources == &Default::default())
    {
        container.resources = None;
    }
    if container.termination_message_path.as_deref() == Some("/dev/termination-log") {
        container.termination_message_path = None;
    }
    if container.termination_message_policy.as_deref() == Some("File") {
        container.termination_message_policy = None;
    }
}

pub(crate) fn normalize_pod_spec(spec: &mut PodSpec, observed_pod: bool) {
    normalize_empty(&mut spec.init_containers);
    normalize_empty(&mut spec.ephemeral_containers);
    normalize_empty(&mut spec.volumes);
    for container in &mut spec.containers {
        normalize_container(container);
    }
    if let Some(containers) = spec.init_containers.as_mut() {
        for container in containers {
            normalize_container(container);
        }
    }
    if let Some(volumes) = spec.volumes.as_mut() {
        for volume in volumes {
            normalize_volume(volume);
        }
    }
    if spec.dns_policy.as_deref() == Some("ClusterFirst") {
        spec.dns_policy = None;
    }
    if spec.enable_service_links == Some(true) {
        spec.enable_service_links = None;
    }
    if spec.scheduler_name.as_deref() == Some("default-scheduler") {
        spec.scheduler_name = None;
    }
    if spec.service_account.as_deref() == Some("default") {
        spec.service_account = None;
    }
    if spec.service_account_name.as_deref() == Some("default") {
        spec.service_account_name = None;
    }
    if spec.termination_grace_period_seconds == Some(30) {
        spec.termination_grace_period_seconds = None;
    }
    if spec.preemption_policy.as_deref() == Some("PreemptLowerPriority") {
        spec.preemption_policy = None;
    }
    if spec.image_pull_secrets.as_ref().is_some_and(Vec::is_empty) {
        spec.image_pull_secrets = None;
    }
    if spec
        .security_context
        .as_ref()
        .is_some_and(|context| context == &Default::default())
    {
        spec.security_context = None;
    }
    if observed_pod {
        spec.node_name = None;
        if spec.priority == Some(0) {
            spec.priority = None;
        }
        if spec.host_users == Some(true) {
            spec.host_users = None;
        }
        if spec.set_hostname_as_fqdn == Some(false) {
            spec.set_hostname_as_fqdn = None;
        }
        if spec.share_process_namespace == Some(false) {
            spec.share_process_namespace = None;
        }
        if spec.host_network == Some(false) {
            spec.host_network = None;
        }
        if spec.host_pid == Some(false) {
            spec.host_pid = None;
        }
        if spec.host_ipc == Some(false) {
            spec.host_ipc = None;
        }
        if let Some(tolerations) = spec.tolerations.as_mut() {
            tolerations.retain(|toleration| !default_no_execute_toleration(toleration));
            if tolerations.is_empty() {
                spec.tolerations = None;
            }
        }
    }
}

fn default_no_execute_toleration(toleration: &Toleration) -> bool {
    matches!(
        toleration.key.as_deref(),
        Some("node.kubernetes.io/not-ready" | "node.kubernetes.io/unreachable")
    ) && toleration.operator.as_deref() == Some("Exists")
        && toleration.effect.as_deref() == Some("NoExecute")
        && toleration.toleration_seconds == Some(300)
        && toleration.value.is_none()
}
