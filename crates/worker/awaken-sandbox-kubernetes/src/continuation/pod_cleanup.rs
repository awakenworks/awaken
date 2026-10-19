// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: The original Pod cleanup interlock retains its exact UID,
// @formal reason: finalizer and current-authority checks; no new cleanup owner.
//! Durable Pod interlock for retained-claim cleanup.

use super::{
    PendingClaimDeletion, absent_pod_claim_deletion, await_claim_deletion, initiate_claim_deletion,
};
use crate::finalizer::{ExactFinalizerAdmission, update_exact_pod_finalizer};
use crate::{K8sRuntime, backend, participants};
use awaken_provisioning_contract as pc;
use awaken_sandbox_container::{KubernetesProjectedParticipants, RuntimeError};
use k8s_openapi::api::core::v1::Pod;
use kube::Api;
use kube::api::PostParams;

pub(crate) const CLAIM_UID_ANNOTATION: &str = "awaken.dev/continuation-claim-uid";
pub(crate) const CONTINUATION_CLEANUP_FINALIZER: &str = "awaken.dev/continuation-cleanup";
const CONTINUATION_CLEANUP_GATE_ANNOTATION: &str = "awaken.dev/continuation-cleanup-gate";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PodCleanupAuthorizationAdmission {
    Acquire,
    Resume,
    AdvanceSuccessor,
}

/// One persisted physical cleanup interlock with two non-overlapping authority
/// sources. Aggregate disposal owns published Session environments; exact
/// restoration evidence owns only its unpublished pre-CAS restore target.
/// Untagged encoding preserves the existing aggregate annotation wire shape.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum PodCleanupAuthorization {
    OriginalCreate(OriginalCreateGuardAuthorization),
    Aggregate(pc::SandboxDisposalAuthorization),
    Restoration(pc::SandboxRestorationEvidence),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OriginalCreateGuardAuthorization {
    original_create_guard: pc::SandboxDisposalAuthorization,
}

impl PodCleanupAuthorization {
    fn aggregate(&self) -> Option<&pc::SandboxDisposalAuthorization> {
        match self {
            Self::Aggregate(value) => Some(value),
            Self::OriginalCreate(value) => Some(&value.original_create_guard),
            Self::Restoration(_) => None,
        }
    }

    fn retainer(&self) -> &'static str {
        match self {
            Self::OriginalCreate(_) => crate::create_ledger::ORIGINAL_GUARD_FINALIZER,
            _ => CONTINUATION_CLEANUP_FINALIZER,
        }
    }

    fn for_original_guard(&self) -> Result<Self, RuntimeError> {
        let original_create_guard = self.aggregate().ok_or_else(|| {
            backend("original Create guard succession requires aggregate disposal authority")
        })?;
        Ok(Self::OriginalCreate(OriginalCreateGuardAuthorization {
            original_create_guard: original_create_guard.clone(),
        }))
    }
}

pub(crate) fn pod_cleanup_authorization_admission(
    observed: Option<&PodCleanupAuthorization>,
    requested: &PodCleanupAuthorization,
) -> Result<PodCleanupAuthorizationAdmission, RuntimeError> {
    use PodCleanupAuthorizationAdmission::{Acquire, AdvanceSuccessor, Resume};

    if let (Some(observed), Some(requested)) = (
        observed.and_then(PodCleanupAuthorization::aggregate),
        requested.aggregate(),
    ) {
        if observed.prepared_effect_fence() != requested.prepared_effect_fence()
            || observed.preparation_fingerprint() != requested.preparation_fingerprint()
        {
            return Err(backend(
                "Kubernetes continuation cleanup gate belongs to a different preparation",
            ));
        }
        if observed.effect_fence() == requested.effect_fence() {
            return Ok(Resume);
        }
        if observed
            .effect_fence()
            .authorizes_successor(requested.effect_fence())
        {
            return Ok(AdvanceSuccessor);
        }
        return Err(backend(
            "Kubernetes continuation cleanup gate is owned by a newer or conflicting successor",
        ));
    }
    match observed {
        None => Ok(Acquire),
        Some(PodCleanupAuthorization::Restoration(observed)) => match requested {
            PodCleanupAuthorization::Restoration(requested) if observed == requested => Ok(Resume),
            PodCleanupAuthorization::Restoration(_) => Err(backend(
                "Kubernetes continuation cleanup gate belongs to a different exact restoration",
            )),
            _ => Err(backend(
                "Kubernetes continuation cleanup gate belongs to restoration authority",
            )),
        },
        Some(_) => Err(backend(
            "Kubernetes continuation cleanup gate belongs to aggregate authority",
        )),
    }
}

pub(crate) fn pod_cleanup_authorization(
    pod: &Pod,
) -> Result<Option<PodCleanupAuthorization>, RuntimeError> {
    let Some(encoded) = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(CONTINUATION_CLEANUP_GATE_ANNOTATION))
    else {
        return Ok(None);
    };
    serde_json::from_str::<PodCleanupAuthorization>(encoded)
        .map(Some)
        .map_err(|error| {
            backend(format!(
                "invalid Kubernetes continuation cleanup gate: {error}"
            ))
        })
}

/// Decode the one cleanup provenance annotation only when the same Pod also
/// carries the one cleanup finalizer. This is the sole consistency owner used
/// by observation, acquisition, replay, and release.
pub(crate) fn observed_pod_cleanup_authorization(
    pod: &Pod,
) -> Result<Option<PodCleanupAuthorization>, RuntimeError> {
    let authorization = pod_cleanup_authorization(pod)?;
    let finalizers = pod.metadata.finalizers.as_deref().unwrap_or_default();
    let continuation_held = pod_cleanup_gate_held(pod);
    let consistent = match authorization.as_ref() {
        Some(PodCleanupAuthorization::OriginalCreate(_)) => {
            !continuation_held
                && pod.metadata.deletion_timestamp.is_some()
                && finalizers
                    .iter()
                    .any(|value| value == crate::create_ledger::ORIGINAL_GUARD_FINALIZER)
        }
        Some(_) => continuation_held,
        None => !continuation_held,
    };
    if !consistent {
        return Err(backend(
            "Kubernetes continuation cleanup finalizer and provenance annotation disagree",
        ));
    }
    Ok(authorization)
}

pub(crate) fn stamp_pod_cleanup_authorization(
    pod: &mut Pod,
    authorization: &PodCleanupAuthorization,
) -> Result<(), RuntimeError> {
    let encoded = serde_json::to_string(authorization).map_err(backend)?;
    pod.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(CONTINUATION_CLEANUP_GATE_ANNOTATION.to_owned(), encoded);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PodCleanupGateAdmission {
    Acquire,
    AdoptOriginal,
    Resume,
    MissingUid,
    StaleUid,
    MissingResourceVersion,
    DeletingBeforeGate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PodCleanupGateReleaseAdmission {
    Remove,
    AlreadyReleasedAfterDelete,
    LostBeforeDelete,
}

#[must_use]
pub(crate) const fn pod_cleanup_gate_release_admission(
    terminating: bool,
    cleanup_finalizer_present: bool,
) -> PodCleanupGateReleaseAdmission {
    if cleanup_finalizer_present {
        PodCleanupGateReleaseAdmission::Remove
    } else if terminating {
        PodCleanupGateReleaseAdmission::AlreadyReleasedAfterDelete
    } else {
        PodCleanupGateReleaseAdmission::LostBeforeDelete
    }
}

#[must_use]
pub(crate) const fn pod_cleanup_gate_admission(
    uid_present: bool,
    uid_matches: bool,
    resource_version_present: bool,
    terminating: bool,
    cleanup_finalizer_present: bool,
    original_retention: bool,
) -> PodCleanupGateAdmission {
    if !uid_present {
        PodCleanupGateAdmission::MissingUid
    } else if !uid_matches {
        PodCleanupGateAdmission::StaleUid
    } else if !resource_version_present {
        PodCleanupGateAdmission::MissingResourceVersion
    } else if cleanup_finalizer_present {
        PodCleanupGateAdmission::Resume
    } else if terminating {
        if original_retention {
            PodCleanupGateAdmission::AdoptOriginal
        } else {
            PodCleanupGateAdmission::DeletingBeforeGate
        }
    } else {
        PodCleanupGateAdmission::Acquire
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PendingPodCleanup {
    pub(crate) pod: Pod,
    name: String,
    uid: String,
    authorization: PodCleanupAuthorization,
}

impl PendingPodCleanup {
    pub(crate) fn releases_original_guard(&self) -> bool {
        matches!(
            self.authorization,
            PodCleanupAuthorization::OriginalCreate(_)
        )
    }
}

pub(crate) fn terminal_original_guard_retained(pod: &Pod) -> bool {
    pod.metadata.finalizers.as_ref().is_some_and(|values| {
        values
            .iter()
            .any(|value| value == crate::create_ledger::ORIGINAL_GUARD_FINALIZER)
    }) && pod
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(crate::create_ledger::ISOLATION_LABEL))
        .is_none()
        && matches!(
            pod.status
                .as_ref()
                .and_then(|status| status.phase.as_deref()),
            Some("Failed" | "Succeeded")
        )
}

pub(crate) fn pod_cleanup_gate_held(pod: &Pod) -> bool {
    pod.metadata.finalizers.as_ref().is_some_and(|finalizers| {
        finalizers
            .iter()
            .any(|finalizer| finalizer == CONTINUATION_CLEANUP_FINALIZER)
    })
}

/// Acquire the sole durable interlock between exact Pod replacement and
/// retained-claim disposal. The resourceVersion patch is the linearization
/// point: if Rebuild's DELETE wins first, the Pod becomes terminating and this
/// operation has no PVC side effect; if this patch wins, the stable Pod name
/// cannot be released until [`release_pod_cleanup_gate`] removes the finalizer.
/// Begin the existing retained Pod/PVC disposal in its fixed order. The
/// retained Pod gate stays installed on a lost PVC response; re-entry observes
/// that same deletion intent. This is the original protocol, not another state.
pub(crate) async fn begin_bound_disposal(
    runtime: &K8sRuntime,
    container_id: &str,
    pod: &Pod,
    pod_uid: Option<&str>,
    claim_uid: Option<&str>,
    authorization: Option<&PodCleanupAuthorization>,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(Option<PendingPodCleanup>, Option<PendingClaimDeletion>), RuntimeError> {
    let Some(claim_uid) = claim_uid else {
        return Ok((None, None));
    };
    let authorization = authorization.ok_or_else(|| {
        backend(
            "Kubernetes retained continuation disposal requires aggregate authorization authority",
        )
    })?;
    let uid = pod_uid.ok_or_else(|| backend("retained bound Pod has no persisted UID"))?;
    let pods = runtime.pods();
    let claims = runtime.persistent_volume_claims();
    let _gate =
        acquire_pod_cleanup_gate_with_authorization(&pods, pod, uid, authorization, admit).await?;
    // Keep the gate on every API ambiguity; only the same successor can resume.
    let claim = initiate_claim_deletion(&claims, container_id, claim_uid, admit).await?;
    let current = pods.get(container_id).await;
    admit()?;
    let current = current.map_err(backend)?;
    let gate =
        acquire_pod_cleanup_gate_with_authorization(&pods, &current, uid, authorization, admit)
            .await?;
    Ok((Some(gate), claim))
}

pub(crate) async fn acquire_pod_cleanup_gate_with_authorization(
    pods: &Api<Pod>,
    observed: &Pod,
    expected_uid: &str,
    authorization: &PodCleanupAuthorization,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<PendingPodCleanup, RuntimeError> {
    admit()?;
    let name = observed
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let uid = observed.metadata.uid.clone();
    let observed_authorization = observed_pod_cleanup_authorization(observed)?;
    let has_gate = observed_authorization.is_some();
    let admission = pod_cleanup_gate_admission(
        uid.is_some(),
        uid.as_deref() == Some(expected_uid),
        observed.metadata.resource_version.is_some(),
        observed.metadata.deletion_timestamp.is_some(),
        has_gate,
        authorization.aggregate().is_some() && terminal_original_guard_retained(observed),
    );
    match admission {
        PodCleanupGateAdmission::Acquire
        | PodCleanupGateAdmission::Resume
        | PodCleanupGateAdmission::AdoptOriginal => {}
        PodCleanupGateAdmission::MissingUid => {
            return Err(backend("Kubernetes Sandbox Pod has no UID"));
        }
        PodCleanupGateAdmission::StaleUid => {
            return Err(backend(
                "Kubernetes Sandbox Pod incarnation changed before cleanup gate",
            ));
        }
        PodCleanupGateAdmission::MissingResourceVersion => {
            return Err(backend("Kubernetes Sandbox Pod has no resourceVersion"));
        }
        PodCleanupGateAdmission::DeletingBeforeGate => {
            return Err(backend(
                "Kubernetes Sandbox Pod deletion won before continuation cleanup gate",
            ));
        }
    }
    let uid = uid.expect("cleanup admission proved Pod UID is present");
    let original = admission == PodCleanupGateAdmission::AdoptOriginal
        || observed_authorization
            .as_ref()
            .is_some_and(|value| matches!(value, PodCleanupAuthorization::OriginalCreate(_)));
    let selected = if original {
        authorization.for_original_guard()?
    } else {
        authorization.clone()
    };
    match pod_cleanup_authorization_admission(observed_authorization.as_ref(), &selected)? {
        PodCleanupAuthorizationAdmission::Resume => {
            return Ok(PendingPodCleanup {
                pod: observed.clone(),
                name,
                uid,
                authorization: selected,
            });
        }
        PodCleanupAuthorizationAdmission::Acquire
        | PodCleanupAuthorizationAdmission::AdvanceSuccessor => {}
    }
    let mut replacement = observed.clone();
    if !has_gate && !original {
        replacement
            .metadata
            .finalizers
            .get_or_insert_with(Vec::new)
            .push(CONTINUATION_CLEANUP_FINALIZER.to_owned());
    }
    // A deleting original keeps its existing finalizer. Only the exact
    // preparation/successor annotation changes in this UID/resourceVersion CAS.
    stamp_pod_cleanup_authorization(&mut replacement, &selected)?;
    let patched = pods
        .replace(&name, &PostParams::default(), &replacement)
        .await
        .map_err(backend)?;
    admit()?;
    let patched_authorization = observed_pod_cleanup_authorization(&patched)?
        .ok_or_else(|| backend("Kubernetes Sandbox Pod cleanup gate lost its provenance"))?;
    if patched.metadata.uid.as_deref() != Some(uid.as_str())
        || patched_authorization.retainer() != selected.retainer()
        || pod_cleanup_authorization_admission(Some(&patched_authorization), &selected)?
            != PodCleanupAuthorizationAdmission::Resume
    {
        return Err(backend(
            "Kubernetes Sandbox Pod cleanup gate changed incarnation",
        ));
    }
    Ok(PendingPodCleanup {
        pod: patched,
        name,
        uid,
        authorization: selected,
    })
}

/// Release only this cleanup owner's finalizer from the exact source Pod. A
/// resourceVersion conflict is retried from a fresh read, while any replacement
/// UID fails closed. No caller reconstructs or removes finalizers independently.
pub(crate) async fn release_pod_cleanup_gate(
    pods: &Api<Pod>,
    pending: &PendingPodCleanup,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    update_exact_pod_finalizer(
        pods,
        &pending.name,
        pending.authorization.retainer(),
        false,
        admit,
        &|observed| {
        if observed.metadata.uid.as_deref() != Some(pending.uid.as_str()) {
            return Err(backend(
                "Kubernetes Sandbox Pod incarnation changed before cleanup gate release",
            ));
        }
        let observed_authorization = observed_pod_cleanup_authorization(observed)?;
        match pod_cleanup_gate_release_admission(
            observed.metadata.deletion_timestamp.is_some(),
            observed_authorization.is_some(),
        ) {
            PodCleanupGateReleaseAdmission::Remove => {
                if observed_authorization.as_ref().is_none_or(|value| value.retainer()!=pending.authorization.retainer())
                    || pod_cleanup_authorization_admission(
                    observed_authorization.as_ref(),
                    &pending.authorization,
                )? != PodCleanupAuthorizationAdmission::Resume
                {
                    return Err(backend(
                        "Kubernetes continuation cleanup successor changed before gate release",
                    ));
                }
                Ok(ExactFinalizerAdmission::Remove)
            }
            PodCleanupGateReleaseAdmission::AlreadyReleasedAfterDelete => {
                Ok(ExactFinalizerAdmission::Complete)
            }
            PodCleanupGateReleaseAdmission::LostBeforeDelete => {
                Err(backend(
                    "Kubernetes Sandbox Pod lost its continuation cleanup finalizer before deletion",
                ))
            }
            }
        },
        &|replacement| {
            if let Some(annotations) = replacement.metadata.annotations.as_mut() {
                annotations.remove(CONTINUATION_CLEANUP_GATE_ANNOTATION);
            }
        },
    )
    .await
}

/// Release the selected physical retainer once. A claimed original guard is
/// not also released through the independent original-result cleanup edge.
pub(crate) async fn release_bound_guards(
    pods: &Api<Pod>,
    container_id: &str,
    pod_uid: &str,
    pending: Option<&PendingPodCleanup>,
    original: bool,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    if let Some(pending) = pending {
        release_pod_cleanup_gate(pods, pending, admit).await?;
    }
    if original && !pending.is_some_and(PendingPodCleanup::releases_original_guard) {
        crate::create_ledger::release_original_guard(pods, container_id, pod_uid, admit).await?;
    }
    Ok(())
}

/// Finish the existing response-loss row after its cleanup interlock was
/// released. The exact Pod/PVC and original projected identities remain owned.
pub(crate) async fn finish_released_bound_disposal(
    runtime: &K8sRuntime,
    container_id: &str,
    pod_uid: &str,
    claim_uid: Option<&str>,
    projected: Option<&awaken_sandbox_container::KubernetesProjectedParticipants>,
    original: bool,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    release_bound_guards(
        &runtime.pods(),
        container_id,
        pod_uid,
        None,
        original,
        admit,
    )
    .await?;
    crate::lifecycle::await_exact_pod_deleted(runtime, container_id, pod_uid, admit).await?;
    finish_after_absent_pod(runtime, container_id, claim_uid, projected, admit).await
}

/// After the exact Pod is gone: delete its retained claim under the same
/// incarnation fence, then finish the original projected objects.
pub(crate) async fn finish_after_absent_pod(
    runtime: &K8sRuntime,
    container_id: &str,
    claim_uid: Option<&str>,
    projected: Option<&KubernetesProjectedParticipants>,
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    if let Some(uid) = claim_uid {
        let claims = runtime.persistent_volume_claims();
        let pending = absent_pod_claim_deletion(&claims, container_id, uid).await?;
        admit()?;
        if let Some(pending) = pending.as_ref() {
            await_claim_deletion(&claims, pending, admit).await?;
        }
    }
    participants::finish_cleanup(runtime, projected, container_id, admit).await
}
