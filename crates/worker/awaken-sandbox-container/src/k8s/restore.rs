//! Read-first exact restore observation and terminal orphan disposal.

use k8s_openapi::api::core::v1::{Container, PersistentVolumeClaim, Pod};
use kube::ResourceExt as _;

use super::names::continuation_claim_name;
use super::*;

fn verify_plan(plan: &ContainerPlan, expected: &str) -> Result<(), RuntimeError> {
    if crate::restoration_plan_fingerprint(plan) == expected {
        Ok(())
    } else {
        Err(backend("Kubernetes restore plan fingerprint mismatch"))
    }
}

fn verify_container_projection(expected: &Container, observed: &Container) -> bool {
    expected.name == observed.name
        && expected.image == observed.image
        && expected.command == observed.command
        && expected.args == observed.args
        && expected.env == observed.env
        && expected.ports == observed.ports
        && expected.volume_mounts == observed.volume_mounts
        && expected.security_context == observed.security_context
        && expected.readiness_probe == observed.readiness_probe
        && expected.liveness_probe == observed.liveness_probe
        && expected.startup_probe == observed.startup_probe
        && expected.resources == observed.resources
}

fn verify_pod_projection(expected: &Pod, observed: &Pod) -> Result<(), RuntimeError> {
    let expected_spec = expected
        .spec
        .as_ref()
        .ok_or_else(|| backend("desired Kubernetes restore Pod has no specification"))?;
    let observed_spec = observed
        .spec
        .as_ref()
        .ok_or_else(|| backend("observed Kubernetes restore Pod has no specification"))?;
    let exact_containers = expected_spec.containers.len() == observed_spec.containers.len()
        && expected_spec.containers.iter().all(|expected| {
            observed_spec
                .containers
                .iter()
                .find(|observed| observed.name == expected.name)
                .is_some_and(|observed| verify_container_projection(expected, observed))
        });
    let expected_init = expected_spec.init_containers.as_deref().unwrap_or_default();
    let observed_init = observed_spec.init_containers.as_deref().unwrap_or_default();
    let exact_init = expected_init.len() == observed_init.len()
        && expected_init.iter().all(|expected| {
            observed_init
                .iter()
                .find(|observed| observed.name == expected.name)
                .is_some_and(|observed| verify_container_projection(expected, observed))
        });
    let expected_labels = expected.metadata.labels.as_ref();
    let observed_labels = observed.metadata.labels.as_ref();
    let exact_labels = expected_labels.is_some_and(|expected| {
        expected.iter().all(|(key, value)| {
            key == crate::RUNTIME_OWNER_LABEL
                || observed_labels.and_then(|labels| labels.get(key)) == Some(value)
        })
    });
    if expected.metadata.name != observed.metadata.name
        || !exact_labels
        || !exact_containers
        || !exact_init
        || expected_spec.volumes != observed_spec.volumes
        || expected_spec.security_context != observed_spec.security_context
        || expected_spec.restart_policy != observed_spec.restart_policy
        || expected_spec.automount_service_account_token
            != observed_spec.automount_service_account_token
        || expected_spec.image_pull_secrets != observed_spec.image_pull_secrets
        || observed_spec.host_network == Some(true)
        || observed_spec.host_pid == Some(true)
        || observed_spec.host_ipc == Some(true)
        || observed_spec.share_process_namespace == Some(true)
    {
        return Err(backend(
            "Kubernetes restore Pod differs from the complete immutable security projection",
        ));
    }
    realization::verify_realization(expected, observed)
}

fn verify_claim_projection(
    expected: &PersistentVolumeClaim,
    observed: &PersistentVolumeClaim,
) -> Result<(), RuntimeError> {
    let expected_spec = expected
        .spec
        .as_ref()
        .ok_or_else(|| backend("desired Kubernetes continuation PVC has no specification"))?;
    let observed_spec = observed
        .spec
        .as_ref()
        .ok_or_else(|| backend("observed Kubernetes continuation PVC has no specification"))?;
    if expected.metadata.name != observed.metadata.name
        || expected_spec.access_modes != observed_spec.access_modes
        || expected_spec.resources != observed_spec.resources
        || expected_spec.storage_class_name != observed_spec.storage_class_name
        || expected_spec.volume_mode != observed_spec.volume_mode
    {
        return Err(backend(
            "Kubernetes continuation PVC differs from the exact immutable projection",
        ));
    }
    realization::verify_realization(expected, observed)
}

struct Observation {
    pod: Pod,
}

async fn observe(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
    plan_fingerprint: &str,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<Option<Observation>, RuntimeError> {
    verify_plan(plan, plan_fingerprint)?;
    let runtime_id = k8s_runtime_id(id)?;
    let name = pod_name(&runtime_id);
    let pods = runtime.pods();
    let Some(pod) = pods.get_opt(&name).await.map_err(backend)? else {
        return Ok(None);
    };
    realization::verify_restoration(&pod, evidence)?;
    if realization::restoration_plan_fingerprint(&pod) != Some(plan_fingerprint) {
        return Err(backend(
            "Kubernetes restore Pod belongs to a different immutable plan",
        ));
    }

    let claim_required = continuation::claim_required(plan, runtime.continuation_volume.is_some());
    let claim_name = continuation::bound_claim_name(&pod)?;
    if claim_required != claim_name.is_some() {
        return Err(backend(
            "Kubernetes restore Pod continuation binding differs from the exact plan",
        ));
    }
    let claim = if let Some(claim_name) = claim_name {
        let expected_name = continuation_claim_name(&runtime_id);
        if claim_name != expected_name {
            return Err(backend(
                "Kubernetes restore Pod binds a non-canonical continuation PVC",
            ));
        }
        let claim = runtime
            .persistent_volume_claims()
            .get(claim_name)
            .await
            .map_err(backend)?;
        realization::verify_restoration(&claim, evidence)?;
        if realization::restoration_plan_fingerprint(&claim) != Some(plan_fingerprint) {
            return Err(backend(
                "Kubernetes continuation PVC belongs to a different immutable plan",
            ));
        }
        let claim_uid = continuation::claim_uid(&claim)?;
        if continuation::bound_claim_uid(&pod) != Some(claim_uid.as_str()) {
            return Err(backend(
                "Kubernetes restored Pod no longer binds the exact continuation PVC",
            ));
        }
        let mut desired = continuation::build_claim(
            &runtime_id,
            runtime
                .continuation_volume
                .as_ref()
                .expect("claim requirement proves continuation policy"),
        )?;
        realization::stamp_restoration(&mut desired, evidence);
        realization::stamp_restoration_plan(&mut desired, plan_fingerprint);
        realization::stamp_realization(&mut desired)?;
        verify_claim_projection(&desired, &claim)?;
        Some(claim)
    } else {
        None
    };

    let resolved_image = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| {
            annotations.get(crate::k8s_package_realization::RESOLVED_IMAGE_ANNOTATION)
        })
        .filter(|image| !image.is_empty())
        .ok_or_else(|| backend("Kubernetes restore Pod has no resolved image evidence"))?;
    let mut desired_plan = plan.clone();
    desired_plan.image.clone_from(resolved_image);
    let mut desired = runtime.pod(&runtime_id, &desired_plan);
    crate::k8s_package_realization::stamp_sandbox_release_annotations(
        &mut desired,
        id,
        resolved_image,
    );
    if let Some(claim) = claim.as_ref() {
        continuation::bind_claim_uid(&mut desired, &continuation::claim_uid(claim)?);
    }
    realization::stamp_restoration(&mut desired, evidence);
    realization::stamp_restoration_plan(&mut desired, plan_fingerprint);
    realization::stamp_pod_realization(&mut desired)?;
    verify_pod_projection(&desired, &pod)?;
    Ok(Some(Observation { pod }))
}

pub(super) async fn recover(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
    plan_fingerprint: &str,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<Option<crate::RuntimeRestoreTarget>, RuntimeError> {
    let Some(observation) = observe(runtime, id, plan, plan_fingerprint, evidence).await? else {
        return Ok(None);
    };
    let name = observation.pod.name_any();
    let uid = observation
        .pod
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes restore target has no Pod UID"))?;
    realization::transfer_runtime_owner(&runtime.pods(), observation.pod, &uid, &runtime.owner_id)
        .await?;
    realization::await_pod_ready(&runtime.pods(), &name).await?;
    Ok(Some(crate::RuntimeRestoreTarget {
        container_id: name,
        disposition: pc::SandboxRestoreTargetDisposition::Recovered,
    }))
}

pub(super) async fn restoration_evidence(
    runtime: &K8sRuntime,
    container_id: &str,
) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
    let pod = runtime.pods().get(container_id).await.map_err(backend)?;
    let evidence = realization::restoration_evidence(&pod)?;
    if let Some(claim_name) = continuation::bound_claim_name(&pod)? {
        let claim = runtime
            .persistent_volume_claims()
            .get(claim_name)
            .await
            .map_err(backend)?;
        if realization::restoration_evidence(&claim)? != evidence {
            return Err(backend(
                "Kubernetes Pod and continuation PVC restore evidence differ",
            ));
        }
        let claim_uid = continuation::claim_uid(&claim)?;
        if continuation::bound_claim_uid(&pod) != Some(claim_uid.as_str()) {
            return Err(backend(
                "Kubernetes restored Pod no longer binds the exact continuation PVC",
            ));
        }
    }
    Ok(evidence)
}

pub(super) async fn plan_fingerprint(
    runtime: &K8sRuntime,
    container_id: &str,
) -> Result<Option<String>, RuntimeError> {
    let pod = runtime.pods().get(container_id).await.map_err(backend)?;
    Ok(realization::restoration_plan_fingerprint(&pod).map(str::to_owned))
}

pub(super) async fn dispose(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
    plan_fingerprint: &str,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<(), RuntimeError> {
    let runtime_id = k8s_runtime_id(id)?;
    let name = pod_name(&runtime_id);
    if let Some(observation) = observe(runtime, id, plan, plan_fingerprint, evidence).await? {
        let handle = continuation::handle_extra(&observation.pod)?;
        return runtime.remove_with_handle(&name, handle.as_ref()).await;
    }
    if continuation::claim_required(plan, runtime.continuation_volume.is_some()) {
        let claim_name = continuation_claim_name(&runtime_id);
        let claim = match runtime.persistent_volume_claims().get(&claim_name).await {
            Ok(claim) => claim,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        realization::verify_restoration(&claim, evidence)?;
        if realization::restoration_plan_fingerprint(&claim) != Some(plan_fingerprint) {
            return Err(backend(
                "Kubernetes orphan continuation PVC belongs to a different immutable plan",
            ));
        }
        let mut desired = continuation::build_claim(
            &runtime_id,
            runtime
                .continuation_volume
                .as_ref()
                .expect("claim requirement proves continuation policy"),
        )?;
        realization::stamp_restoration(&mut desired, evidence);
        realization::stamp_restoration_plan(&mut desired, plan_fingerprint);
        realization::stamp_realization(&mut desired)?;
        verify_claim_projection(&desired, &claim)?;
        let uid = continuation::claim_uid(&claim)?;
        continuation::delete_claim(&runtime.persistent_volume_claims(), &name, &uid).await?;
    }
    Ok(())
}
