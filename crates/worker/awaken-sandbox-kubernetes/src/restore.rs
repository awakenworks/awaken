// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: The Kubernetes exact-restore adapter verifies the immutable Pod, PVC, restoration, and
// @formal reason: realization identities before any API effect and delegates claim deletion to the
// @formal reason: existing continuation owner; this is a product-boundary classification rather than a
// @formal reason: second lifecycle authority or direct formal-proof claim.
//! Read-first exact restore observation and terminal orphan disposal.

use super::names::continuation_claim_name;
use super::{
    K8sRuntime, backend, continuation, k8s_runtime_id, participants, pod_name, realization,
};
use awaken_container_contract::RuntimeError;
use awaken_sandbox_container::{ContainerPlan, ContainerRuntime as _};
use k8s_openapi::api::core::v1::Pod;
use kube::ResourceExt as _;

use super::projection_verification::{verify_claim_projection, verify_pod_projection};
use awaken_provisioning_contract as pc;

fn verify_plan(plan: &ContainerPlan, expected: &str) -> Result<(), RuntimeError> {
    if awaken_sandbox_container::engine::restoration_plan_fingerprint(plan) == expected {
        Ok(())
    } else {
        Err(backend("Kubernetes restore plan fingerprint mismatch"))
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
    let Some(pod) = crate::get_optional(&pods, &name).await? else {
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
            annotations.get(crate::package_realization::RESOLVED_IMAGE_ANNOTATION)
        })
        .filter(|image| !image.is_empty())
        .ok_or_else(|| backend("Kubernetes restore Pod has no resolved image evidence"))?;
    let mut desired_plan = plan.clone();
    desired_plan.image.clone_from(resolved_image);
    let mut desired = runtime.pod(&runtime_id, &desired_plan);
    crate::package_realization::stamp_sandbox_release_annotations(&mut desired, id, resolved_image);
    if let Some(claim) = claim.as_ref() {
        continuation::bind_claim_uid(&mut desired, &continuation::claim_uid(claim)?);
    }
    realization::stamp_restoration(&mut desired, evidence);
    realization::stamp_restoration_plan(&mut desired, plan_fingerprint);
    if pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|values| values.contains_key(participants::DECLARATION))
    {
        participants::declare(&mut desired, &runtime_id, &desired_plan)?;
    }
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
) -> Result<Option<awaken_sandbox_container::RuntimeRestoreTarget>, RuntimeError> {
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
    let mut pod = observation.pod;
    if pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|values| values.contains_key(participants::DECLARATION))
    {
        participants::realize(
            runtime,
            pod,
            &k8s_runtime_id(id)?,
            plan,
            None,
            Some((evidence, plan_fingerprint)),
        )
        .await?;
        pod = runtime.pods().get(&name).await.map_err(backend)?;
        realization::verify_restoration(&pod, evidence)?;
        if realization::restoration_plan_fingerprint(&pod) != Some(plan_fingerprint) {
            return Err(backend(
                "restored projection handoff changed its original plan",
            ));
        }
    }
    participants::metadata::transfer_runtime_owner(&runtime.pods(), pod, &uid, &runtime.owner_id)
        .await?;
    let wait = runtime.unschedulable_wait;
    realization::await_pod_ready(&runtime.pods(), &name, wait, runtime.clock.as_ref()).await?;
    Ok(Some(awaken_sandbox_container::RuntimeRestoreTarget {
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
    Err(backend(
        "restored Kubernetes target lost its original participant carrier; terminal absence remains unknown",
    ))
}

#[cfg(test)]
mod tests;
