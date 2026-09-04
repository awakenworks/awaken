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

pub(super) fn verify_pod_projection(expected: &Pod, observed: &Pod) -> Result<(), RuntimeError> {
    let mut expected_spec = expected
        .spec
        .clone()
        .ok_or_else(|| backend("desired Kubernetes restore Pod has no specification"))?;
    let mut observed_spec = observed
        .spec
        .clone()
        .ok_or_else(|| backend("observed Kubernetes restore Pod has no specification"))?;
    realization::normalize_pod_spec(&mut expected_spec, false);
    realization::normalize_pod_spec(&mut observed_spec, true);
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
    {
        return Err(backend(
            "Kubernetes restore Pod differs from the complete immutable security projection",
        ));
    }
    realization::verify_realization(expected, observed)
}

pub(super) fn verify_claim_projection(
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
        || !claim_storage_class_matches(
            expected_spec.storage_class_name.as_deref(),
            observed_spec.storage_class_name.as_deref(),
        )
        || expected_spec.volume_mode != observed_spec.volume_mode
    {
        return Err(backend(
            "Kubernetes continuation PVC differs from the exact immutable projection",
        ));
    }
    realization::verify_realization(expected, observed)
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

#[cfg(test)]
mod claim_projection_tests {
    use super::claim_storage_class_matches;

    #[test]
    fn storage_class_defaulting_decision_table_is_exact() {
        // Cause/effect graph: C1 the desired PVC names a class or delegates to
        // apiserver defaulting; C2 the observed class is absent, non-empty, or
        // empty. Effects: E1 an explicit class must match byte-for-byte; E2 a
        // delegated class accepts either no default or one concrete admitted
        // default; E3 an empty admitted name fails closed. Rules are all six
        // meaningful rows below; the live restore test owns the API mutation.
        assert!(
            claim_storage_class_matches(Some("fast"), Some("fast")),
            "R1/E1"
        );
        assert!(
            !claim_storage_class_matches(Some("fast"), Some("slow")),
            "R2/E1"
        );
        assert!(!claim_storage_class_matches(Some("fast"), None), "R3/E1");
        assert!(
            claim_storage_class_matches(None, Some("local-path")),
            "R4/E2"
        );
        assert!(claim_storage_class_matches(None, None), "R5/E2");
        assert!(!claim_storage_class_matches(None, Some("  ")), "R6/E3");
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
        let (pod_uid, claim_uid) = super::runtime_contract::deletion_incarnation(handle.as_ref())?;
        let authorization = continuation::PodCleanupAuthorization::Restoration(evidence.clone());
        return runtime
            .remove_bound(&name, pod_uid, claim_uid, Some(&authorization))
            .await;
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
        let claims = runtime.persistent_volume_claims();
        if let Some(pending) = continuation::initiate_claim_deletion(&claims, &name, &uid).await? {
            continuation::await_claim_deletion(&claims, &pending).await?;
        }
    }
    Ok(())
}
