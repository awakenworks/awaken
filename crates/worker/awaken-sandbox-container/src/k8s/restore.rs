// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The Kubernetes exact-restore adapter verifies the immutable Pod, PVC, restoration, and
// @formal reason: realization identities before any API effect and delegates claim deletion to the
// @formal reason: existing continuation owner; this is a product-boundary classification rather than a
// @formal reason: second lifecycle authority or direct formal-proof claim.
//! Read-first exact restore observation and terminal orphan disposal.

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
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

/// Decode only the historical directory initializer into the expected immutable
/// projection. New creation never emits it. Full spec and original digest still
/// have to match: an arbitrary init, command, image or mount is not normalized away.
fn expected_historical_directory_projection(
    expected: &Pod,
    observed: &Pod,
) -> Result<Pod, RuntimeError> {
    let mut expected = expected.clone();
    let has_legacy = observed
        .spec
        .as_ref()
        .and_then(|spec| spec.init_containers.as_ref())
        .is_some_and(|items| items.iter().any(|item| item.name == "continuation-init"));
    if !has_legacy {
        return Ok(expected);
    }
    let spec = expected
        .spec
        .as_mut()
        .ok_or_else(|| backend("desired Pod has no specification"))?;
    if spec
        .init_containers
        .as_ref()
        .is_some_and(|items| items.iter().any(|item| item.name == "continuation-init"))
    {
        return Ok(expected);
    }
    let agent = spec
        .containers
        .iter()
        .find(|item| item.name == "agent")
        .ok_or_else(|| backend("desired Pod has no agent"))?;
    let directories = agent
        .volume_mounts
        .iter()
        .flatten()
        .filter(|mount| mount.name == pod_projection::CONTINUATION_VOLUME)
        .filter_map(|mount| mount.sub_path.as_ref())
        .map(|path| format!("/state/{path}"))
        .collect::<Vec<_>>();
    if directories.is_empty() {
        return Ok(expected);
    }
    let initializer = Container {
        name: "continuation-init".into(),
        image: agent.image.clone(),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("mkdir -p {}", directories.join(" ")),
        ]),
        volume_mounts: Some(vec![VolumeMount {
            name: pod_projection::CONTINUATION_VOLUME.into(),
            mount_path: "/state".into(),
            ..Default::default()
        }]),
        security_context: Some(hardened_security_context()),
        ..Default::default()
    };
    spec.init_containers
        .get_or_insert_with(Vec::new)
        .insert(0, initializer);
    realization::stamp_pod_realization(&mut expected)?;
    Ok(expected)
}

pub(super) fn verify_pod_projection(
    expected: &Pod,
    observed: &Pod,
    disposal: Option<&pc::SandboxDisposalAuthorization>,
) -> Result<(), RuntimeError> {
    let expected = expected_historical_directory_projection(expected, observed)?;
    let mut expected_spec = expected
        .spec
        .clone()
        .ok_or_else(|| backend("desired Kubernetes restore Pod has no specification"))?;
    let mut observed_spec = observed
        .spec
        .clone()
        .ok_or_else(|| backend("observed Kubernetes restore Pod has no specification"))?;
    api_defaults::normalize_pod_spec(&mut expected_spec, false);
    api_defaults::normalize_pod_spec(&mut observed_spec, true);
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
        || expected_spec != observed_spec
        || has_forbidden_sandbox_namespace_shape(&observed_spec)
    {
        return Err(backend(
            "Kubernetes restore Pod differs from the complete immutable security projection",
        ));
    }
    realization::verify_realization(&expected, observed, disposal)
}

pub(super) fn verify_claim_projection(
    expected: &PersistentVolumeClaim,
    observed: &PersistentVolumeClaim,
    disposal: Option<&pc::SandboxDisposalAuthorization>,
) -> Result<(), RuntimeError> {
    verify_claim_projection_shape(expected, observed)?;
    realization::verify_realization(expected, observed, disposal)
}

/// The terminal guard already excludes any new Pod binding. A claim whose
/// exact UID has deletionTimestamp may be a lost response to this cleanup;
/// preserve the same immutable spec/digest check while allowing its deletion
/// to converge. The caller must separately verify guard policy, UID and the
/// predecessor effect fence before using this narrower terminal projection.
pub(super) fn verify_guarded_claim_projection(
    expected: &PersistentVolumeClaim,
    observed: &PersistentVolumeClaim,
) -> Result<(), RuntimeError> {
    verify_claim_projection_shape(expected, observed)?;
    if realization::realization_digest_matches(expected, observed)? {
        Ok(())
    } else {
        Err(backend(
            "guarded Kubernetes continuation PVC differs from its original realization",
        ))
    }
}

fn verify_claim_projection_shape(
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
    // Omitted StorageClass delegates selection to Kubernetes. Admission may
    // assign the cluster default before returning CREATE (or later), so it is
    // not an explicitly pinned class. An explicit empty class instead disables
    // defaulting and must not be substituted for this delegated policy.
    // volumeName is the controller's binding output. Preserve every other spec
    // field exactly, including selectors and clone/data sources; a copied
    // realization annotation cannot authorize an extra storage input.
    let mut projected = expected_spec.clone();
    if projected.storage_class_name.is_none()
        && claim_storage_class_matches(None, observed_spec.storage_class_name.as_deref())
    {
        projected.storage_class_name = observed_spec.storage_class_name.clone();
    }
    projected.volume_name = observed_spec.volume_name.clone();
    if expected.metadata.name != observed.metadata.name || projected != *observed_spec {
        return Err(backend(
            "Kubernetes continuation PVC differs from the exact immutable projection",
        ));
    }
    Ok(())
}

/// An omitted desired storage class delegates exactly one choice to the
/// apiserver's defaulting admission. A named class remains byte-exact; an empty
/// admitted class is never a valid default projection.
fn claim_storage_class_matches(expected: Option<&str>, observed: Option<&str>) -> bool {
    match expected {
        Some(expected) => observed == Some(expected),
        None => observed.is_none_or(|observed| !observed.trim().is_empty()),
    }
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
    disposal: Option<&pc::SandboxDisposalAuthorization>,
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
        verify_claim_projection(&desired, &claim, disposal)?;
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
    verify_pod_projection(&desired, &pod, disposal)?;
    Ok(Some(Observation { pod }))
}

pub(super) async fn recover(
    runtime: &K8sRuntime,
    id: &str,
    plan: &ContainerPlan,
    plan_fingerprint: &str,
    evidence: &pc::SandboxRestorationEvidence,
) -> Result<Option<crate::RuntimeRestoreTarget>, RuntimeError> {
    let Some(observation) = observe(runtime, id, plan, plan_fingerprint, evidence, None).await?
    else {
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
    authorization: &pc::SandboxDisposalAuthorization,
    authority: &dyn pc::CurrentExecutionAuthority,
) -> Result<(), RuntimeError> {
    authorization.validate().map_err(backend)?;
    let admit = || {
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))
            .map_err(backend)
    };
    admit()?;
    let runtime_id = k8s_runtime_id(id)?;
    let name = pod_name(&runtime_id);
    if let Some(observation) = observe(
        runtime,
        id,
        plan,
        plan_fingerprint,
        evidence,
        Some(authorization),
    )
    .await?
    {
        admit()?;
        let handle = continuation::handle_extra(&observation.pod)?;
        runtime
            .remove_exact_incarnation(&name, handle.as_ref(), authorization, authority)
            .await?;
        return admit();
    }
    admit()?;
    if continuation::claim_required(plan, runtime.continuation_volume.is_some()) {
        let claim_name = continuation_claim_name(&runtime_id);
        let claim = match runtime.persistent_volume_claims().get(&claim_name).await {
            Ok(claim) => claim,
            Err(error) if api_not_found(&error) => return admit(),
            Err(error) => return Err(backend(error)),
        };
        admit()?;
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
        verify_claim_projection(&desired, &claim, Some(authorization))?;
        let uid = continuation::claim_uid(&claim)?;
        let claims = runtime.persistent_volume_claims();
        // A restore tuple does not bypass the shared absent-Pod rule: without
        // the Pod cleanup gate a live claim can race exact target acquisition.
        // Only already-terminating/absent claims can finish this response-loss cut.
        if let Some(pending) = continuation::absent_pod_claim_deletion(&claims, &name, &uid).await?
        {
            admit()?;
            continuation::await_claim_deletion(&claims, &pending, &admit).await?;
        }
    }
    admit()
}

#[cfg(test)]
mod tests;
