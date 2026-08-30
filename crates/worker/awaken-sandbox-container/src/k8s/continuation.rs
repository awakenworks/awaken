//! Kubernetes realization of one retained active-filesystem volume.

use awaken_provisioning_contract::{self as pc, ContainerContinuationHandle};
use k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::{DeleteParams, PostParams, Preconditions};

use super::error::api_not_found;
use super::names::{continuation_claim_for_pod, continuation_claim_name};
use super::pod_projection::CONTINUATION_VOLUME;
use super::{
    ContainerEffectFence, ContainerPlan, ContainerRealizationIntent, RuntimeError, backend,
    hardened_security_context,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CreateClaimAdmission {
    Create,
    Reuse,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CreateClaimObservation<'a> {
    pub(super) effect_fence: Option<&'a ContainerEffectFence>,
    pub(super) uid_present: bool,
    pub(super) terminating: bool,
    pub(super) realization_matches: bool,
}

pub(super) fn create_claim_admission(
    expected: &ContainerEffectFence,
    observed: Option<CreateClaimObservation<'_>>,
) -> Result<CreateClaimAdmission, RuntimeError> {
    let Some(observed) = observed else {
        return Ok(CreateClaimAdmission::Create);
    };
    if !observed.uid_present {
        return Err(backend("Kubernetes continuation PVC has no UID"));
    }
    if observed.terminating {
        return Err(backend("Kubernetes continuation PVC is terminating"));
    }
    if !observed.realization_matches {
        return Err(backend(
            "existing Kubernetes continuation PVC belongs to a different realization",
        ));
    }
    let Some(observed_fence) = observed.effect_fence else {
        return Err(backend(
            "existing Kubernetes continuation PVC has no durable effect fence",
        ));
    };
    if !observed_fence.same_effect_identity(expected)
        || observed_fence.expires_at_unix_ms > expected.expires_at_unix_ms
    {
        return Err(backend(
            "existing Kubernetes continuation PVC is owned by a newer or conflicting effect",
        ));
    }
    Ok(CreateClaimAdmission::Reuse)
}

/// One fail-closed decision shared by the initial Rebuild continuity read and
/// the immediate pre-Pod-CREATE reread. A deleting claim is not live physical
/// continuity even while its immutable UID still matches the persisted handle.
pub(super) fn rebuild_claim_admission(
    observed: &PersistentVolumeClaim,
    expected_uid: &str,
) -> Result<(), RuntimeError> {
    if observed.metadata.deletion_timestamp.is_some() {
        return Err(backend(
            "Kubernetes rebuild source continuation PVC is terminating",
        ));
    }
    if claim_uid(observed)? != expected_uid {
        return Err(backend(
            "Kubernetes rebuild source continuation PVC changed incarnation",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct K8sRebuildContinuation {
    pub(super) claim_name: String,
    pub(super) claim_uid: String,
}

pub(super) fn rebuild_continuation_expectation(
    intent: &ContainerRealizationIntent,
    expected_claim_name: Option<String>,
) -> Result<Option<K8sRebuildContinuation>, RuntimeError> {
    let ContainerRealizationIntent::Rebuild {
        source_incarnation,
        source_runtime_handle,
    } = intent
    else {
        return Ok(None);
    };
    let Some(ContainerContinuationHandle::KubernetesContinuationV2 {
        pod_uid,
        claim_uid: Some(claim_uid),
    }) = source_runtime_handle.as_ref()
    else {
        return Err(backend(
            "Kubernetes rebuild requires current Pod and continuation PVC incarnation evidence",
        ));
    };
    if pod_uid != source_incarnation {
        return Err(backend(
            "Kubernetes rebuild source Pod differs from its authorized incarnation",
        ));
    }
    let Some(claim_name) = expected_claim_name else {
        return Err(backend(
            "Kubernetes rebuild continuation policy differs from its source handle",
        ));
    };
    Ok(Some(K8sRebuildContinuation {
        claim_name,
        claim_uid: claim_uid.clone(),
    }))
}

#[cfg(test)]
mod rebuild_continuation_tests {
    use super::*;

    #[test]
    fn rebuild_continuation_expectation_decision_table_is_total() {
        /* Continuation cause/effect table. Causes: C1 Create/Rebuild; C2
         * runtime handle current K8s V2/legacy/missing; C3 source Pod UID
         * exact/foreign; C4 current plan requires the same claim/no claim.
         * Effects: E1 Create carries no rebuild authority; E2 exact C2-C4
         * yields the one claim name+UID to revalidate at the final Pod-create
         * edge; E3 every incomplete/foreign row fails closed. Rules: K1
         * Create=>E1; K2 Rebuild+V2+exact(C3)+claim(C4)=>E2; K3 all other
         * Rebuild rows=>E3. */
        assert_eq!(
            rebuild_continuation_expectation(
                &ContainerRealizationIntent::Create,
                Some("claim-a".into()),
            )
            .unwrap(),
            None,
            "K1"
        );
        let exact = ContainerRealizationIntent::Rebuild {
            source_incarnation: "pod-a".into(),
            source_runtime_handle: Some(ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "pod-a".into(),
                claim_uid: Some("claim-uid-a".into()),
            }),
        };
        assert_eq!(
            rebuild_continuation_expectation(&exact, Some("claim-a".into())).unwrap(),
            Some(K8sRebuildContinuation {
                claim_name: "claim-a".into(),
                claim_uid: "claim-uid-a".into(),
            }),
            "K2"
        );
        for rejected in [
            ContainerRealizationIntent::Rebuild {
                source_incarnation: "pod-foreign".into(),
                source_runtime_handle: exact.source_runtime_handle().cloned(),
            },
            ContainerRealizationIntent::Rebuild {
                source_incarnation: "pod-a".into(),
                source_runtime_handle: Some(
                    ContainerContinuationHandle::KubernetesContinuationV2 {
                        pod_uid: "pod-a".into(),
                        claim_uid: None,
                    },
                ),
            },
            ContainerRealizationIntent::Rebuild {
                source_incarnation: "pod-a".into(),
                source_runtime_handle: None,
            },
        ] {
            assert!(
                rebuild_continuation_expectation(&rejected, Some("claim-a".into())).is_err(),
                "K3 incomplete or foreign source evidence"
            );
        }
        assert!(
            rebuild_continuation_expectation(&exact, None).is_err(),
            "K3 current plan removed the retained continuation"
        );
    }

    #[test]
    fn rebuild_claim_admission_requires_exact_live_incarnation() {
        /* Rebuild PVC cause/effect table at both participant-I/O boundaries.
         * Causes: C1 the observed claim UID is exact/foreign/missing; C2 the
         * claim is live/terminating. Effects: E1 only exact(C1)+live(C2)
         * authorizes the existing-realization decision and the final Pod
         * CREATE edge; E2 every other row fails closed with zero Pod effect.
         * Rules: KR1 exact+live=>E1; KR2 foreign|missing|terminating=>E2.
         * The same pure admission owner is invoked by the initial continuity
         * read and by the immediate pre-CREATE reread, so KR2 covers deletion
         * beginning in either race window without adding another authority. */
        let claim = |uid: Option<&str>, terminating: bool| PersistentVolumeClaim {
            metadata: ObjectMeta {
                uid: uid.map(str::to_owned),
                deletion_timestamp: terminating.then(|| {
                    serde_json::from_str("\"2026-08-30T00:00:00Z\"")
                        .expect("valid Kubernetes timestamp")
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(
            rebuild_claim_admission(&claim(Some("claim-a"), false), "claim-a").is_ok(),
            "KR1"
        );
        for rejected in [
            claim(None, false),
            claim(Some("claim-b"), false),
            claim(Some("claim-a"), true),
            claim(Some("claim-b"), true),
        ] {
            assert!(
                rebuild_claim_admission(&rejected, "claim-a").is_err(),
                "KR2"
            );
        }
    }
}

const DELETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DELETE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
pub(super) const CLAIM_UID_ANNOTATION: &str = "awaken.dev/continuation-claim-uid";
pub(super) const CONTINUATION_CLEANUP_FINALIZER: &str = "awaken.dev/continuation-cleanup";
const CONTINUATION_CLEANUP_GATE_ANNOTATION: &str = "awaken.dev/continuation-cleanup-gate";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PodCleanupAuthorizationAdmission {
    Acquire,
    Resume,
    AdvanceSuccessor,
}

fn pod_cleanup_authorization_admission(
    observed: Option<&pc::SandboxDisposalAuthorization>,
    requested: &pc::SandboxDisposalAuthorization,
) -> Result<PodCleanupAuthorizationAdmission, RuntimeError> {
    use PodCleanupAuthorizationAdmission::{Acquire, AdvanceSuccessor, Resume};

    match observed {
        None => Ok(Acquire),
        Some(observed) => {
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
            Err(backend(
                "Kubernetes continuation cleanup gate is owned by a newer or conflicting successor",
            ))
        }
    }
}

fn pod_cleanup_authorization(
    pod: &Pod,
) -> Result<Option<pc::SandboxDisposalAuthorization>, RuntimeError> {
    let Some(encoded) = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(CONTINUATION_CLEANUP_GATE_ANNOTATION))
    else {
        return Ok(None);
    };
    serde_json::from_str::<pc::SandboxDisposalAuthorization>(encoded)
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
pub(super) fn observed_pod_cleanup_authorization(
    pod: &Pod,
) -> Result<Option<pc::SandboxDisposalAuthorization>, RuntimeError> {
    let held = pod_cleanup_gate_held(pod);
    let authorization = pod_cleanup_authorization(pod)?;
    if held != authorization.is_some() {
        return Err(backend(
            "Kubernetes continuation cleanup finalizer and provenance annotation disagree",
        ));
    }
    Ok(authorization)
}

fn stamp_pod_cleanup_authorization(
    pod: &mut Pod,
    authorization: &pc::SandboxDisposalAuthorization,
) -> Result<(), RuntimeError> {
    let encoded = serde_json::to_string(authorization).map_err(backend)?;
    pod.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(CONTINUATION_CLEANUP_GATE_ANNOTATION.to_owned(), encoded);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PodCleanupGateAdmission {
    Acquire,
    Resume,
    MissingUid,
    StaleUid,
    MissingResourceVersion,
    DeletingBeforeGate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PodCleanupGateReleaseAdmission {
    Remove,
    AlreadyReleasedAfterDelete,
    LostBeforeDelete,
}

#[must_use]
const fn pod_cleanup_gate_release_admission(
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
const fn pod_cleanup_gate_admission(
    uid_present: bool,
    uid_matches: bool,
    resource_version_present: bool,
    terminating: bool,
    cleanup_finalizer_present: bool,
) -> PodCleanupGateAdmission {
    if !uid_present {
        PodCleanupGateAdmission::MissingUid
    } else if !uid_matches {
        PodCleanupGateAdmission::StaleUid
    } else if !resource_version_present {
        PodCleanupGateAdmission::MissingResourceVersion
    } else if terminating && !cleanup_finalizer_present {
        PodCleanupGateAdmission::DeletingBeforeGate
    } else if cleanup_finalizer_present {
        PodCleanupGateAdmission::Resume
    } else {
        PodCleanupGateAdmission::Acquire
    }
}

#[derive(Clone, Debug)]
pub(super) struct PendingPodCleanup {
    pub(super) pod: Pod,
    name: String,
    uid: String,
    authorization: pc::SandboxDisposalAuthorization,
}

pub(super) fn pod_cleanup_gate_held(pod: &Pod) -> bool {
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
pub(super) async fn acquire_pod_cleanup_gate(
    pods: &Api<Pod>,
    observed: &Pod,
    expected_uid: &str,
    authorization: &pc::SandboxDisposalAuthorization,
) -> Result<PendingPodCleanup, RuntimeError> {
    let name = observed
        .metadata
        .name
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no name"))?;
    let uid = observed.metadata.uid.clone();
    let resource_version = observed.metadata.resource_version.clone();
    let has_finalizer = pod_cleanup_gate_held(observed);
    let observed_authorization = observed_pod_cleanup_authorization(observed)?;
    match pod_cleanup_gate_admission(
        uid.is_some(),
        uid.as_deref() == Some(expected_uid),
        resource_version.is_some(),
        observed.metadata.deletion_timestamp.is_some(),
        has_finalizer,
    ) {
        PodCleanupGateAdmission::Acquire | PodCleanupGateAdmission::Resume => {}
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
    match pod_cleanup_authorization_admission(observed_authorization.as_ref(), authorization)? {
        PodCleanupAuthorizationAdmission::Resume => {
            return Ok(PendingPodCleanup {
                pod: observed.clone(),
                name,
                uid,
                authorization: authorization.clone(),
            });
        }
        PodCleanupAuthorizationAdmission::Acquire
        | PodCleanupAuthorizationAdmission::AdvanceSuccessor => {}
    }
    let mut replacement = observed.clone();
    if !has_finalizer {
        let mut finalizers = observed.metadata.finalizers.clone().unwrap_or_default();
        finalizers.push(CONTINUATION_CLEANUP_FINALIZER.to_owned());
        replacement.metadata.finalizers = Some(finalizers);
    }
    stamp_pod_cleanup_authorization(&mut replacement, authorization)?;
    let patched = pods
        .replace(&name, &PostParams::default(), &replacement)
        .await
        .map_err(backend)?;
    let patched_authorization = observed_pod_cleanup_authorization(&patched)?;
    if patched.metadata.uid.as_deref() != Some(uid.as_str())
        || !pod_cleanup_gate_held(&patched)
        || pod_cleanup_authorization_admission(patched_authorization.as_ref(), authorization)?
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
        authorization: authorization.clone(),
    })
}

/// Release only this cleanup owner's finalizer from the exact source Pod. A
/// resourceVersion conflict is retried from a fresh read, while any replacement
/// UID fails closed. No caller reconstructs or removes finalizers independently.
pub(super) async fn release_pod_cleanup_gate(
    pods: &Api<Pod>,
    pending: &PendingPodCleanup,
) -> Result<(), RuntimeError> {
    loop {
        let observed = match pods.get(&pending.name).await {
            Ok(pod) => pod,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        };
        if observed.metadata.uid.as_deref() != Some(pending.uid.as_str()) {
            return Err(backend(
                "Kubernetes Sandbox Pod incarnation changed before cleanup gate release",
            ));
        }
        let observed_authorization = observed_pod_cleanup_authorization(&observed)?;
        match pod_cleanup_gate_release_admission(
            observed.metadata.deletion_timestamp.is_some(),
            pod_cleanup_gate_held(&observed),
        ) {
            PodCleanupGateReleaseAdmission::Remove => {
                if pod_cleanup_authorization_admission(
                    observed_authorization.as_ref(),
                    &pending.authorization,
                )? != PodCleanupAuthorizationAdmission::Resume
                {
                    return Err(backend(
                        "Kubernetes continuation cleanup successor changed before gate release",
                    ));
                }
            }
            PodCleanupGateReleaseAdmission::AlreadyReleasedAfterDelete => return Ok(()),
            PodCleanupGateReleaseAdmission::LostBeforeDelete => {
                return Err(backend(
                    "Kubernetes Sandbox Pod lost its continuation cleanup finalizer before deletion",
                ));
            }
        }
        if observed.metadata.resource_version.is_none() {
            return Err(backend("Kubernetes Sandbox Pod has no resourceVersion"));
        }
        let finalizers = observed
            .metadata
            .finalizers
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|finalizer| finalizer != CONTINUATION_CLEANUP_FINALIZER)
            .collect::<Vec<_>>();
        let mut replacement = observed;
        replacement.metadata.finalizers = Some(finalizers);
        if let Some(annotations) = replacement.metadata.annotations.as_mut() {
            annotations.remove(CONTINUATION_CLEANUP_GATE_ANNOTATION);
        }
        match pods
            .replace(&pending.name, &PostParams::default(), &replacement)
            .await
        {
            Ok(patched)
                if patched.metadata.uid.as_deref() == Some(pending.uid.as_str())
                    && !pod_cleanup_gate_held(&patched)
                    && pod_cleanup_authorization(&patched)?.is_none() =>
            {
                return Ok(());
            }
            Ok(_) => {
                return Err(backend(
                    "Kubernetes Sandbox Pod cleanup gate release changed incarnation",
                ));
            }
            Err(error) if super::error::api_conflict(&error) => continue,
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
        }
    }
}

/// Closed admission decision for the irreversible PVC deletion API call. The
/// Kubernetes adapter supplies exact UID/resourceVersion observations; only a
/// complete match may be projected into `DeleteParams::preconditions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClaimDeletionAdmission {
    DeleteExact,
    MissingUid,
    StaleUid,
    MissingResourceVersion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AbsentPodClaimAdmission {
    AlreadyAbsent,
    AwaitTerminating,
    UnsafeLiveClaim,
    MissingUid,
    StaleUid,
}

#[must_use]
const fn absent_pod_claim_admission(
    present: bool,
    uid_present: bool,
    uid_matches: bool,
    terminating: bool,
) -> AbsentPodClaimAdmission {
    if !present {
        AbsentPodClaimAdmission::AlreadyAbsent
    } else if !uid_present {
        AbsentPodClaimAdmission::MissingUid
    } else if !uid_matches {
        AbsentPodClaimAdmission::StaleUid
    } else if terminating {
        AbsentPodClaimAdmission::AwaitTerminating
    } else {
        AbsentPodClaimAdmission::UnsafeLiveClaim
    }
}

#[must_use]
const fn claim_deletion_admission(
    uid_present: bool,
    uid_matches: bool,
    resource_version_present: bool,
) -> ClaimDeletionAdmission {
    if !uid_present {
        ClaimDeletionAdmission::MissingUid
    } else if !uid_matches {
        ClaimDeletionAdmission::StaleUid
    } else if !resource_version_present {
        ClaimDeletionAdmission::MissingResourceVersion
    } else {
        ClaimDeletionAdmission::DeleteExact
    }
}

/// One closed decision shared by PVC allocation and final Pod projection.
/// Keeping these paths on the same selector prevents an ephemeral plan from
/// either allocating a claim or retaining a stale claim reference.
pub(super) fn claim_required(plan: &ContainerPlan, configured: bool) -> bool {
    claim_required_for(plan.filesystem_continuity, configured)
}

fn claim_required_for(
    continuity: awaken_provisioning_contract::FilesystemContinuity,
    configured: bool,
) -> bool {
    configured && continuity == awaken_provisioning_contract::FilesystemContinuity::Retained
}

pub(super) fn claim_name(id: &str, plan: &ContainerPlan, configured: bool) -> Option<String> {
    claim_required(plan, configured).then(|| continuation_claim_name(id))
}

#[cfg(kani)]
#[kani::proof]
fn continuation_claim_selection_is_total_exact_and_non_widening() {
    let configured: bool = kani::any();
    let retained: bool = kani::any();
    let continuity = if retained {
        awaken_provisioning_contract::FilesystemContinuity::Retained
    } else {
        awaken_provisioning_contract::FilesystemContinuity::Ephemeral
    };

    let selected = claim_required_for(continuity, configured);
    assert_eq!(selected, configured && retained);
    assert!(!selected || configured);
    assert!(!selected || retained);
}

#[cfg(kani)]
#[kani::proof]
fn continuation_claim_deletion_requires_exact_uid_and_resource_version() {
    let uid_present = kani::any::<bool>();
    let uid_matches = kani::any::<bool>();
    let resource_version_present = kani::any::<bool>();
    let decision = claim_deletion_admission(uid_present, uid_matches, resource_version_present);
    assert_eq!(
        matches!(decision, ClaimDeletionAdmission::DeleteExact),
        uid_present && uid_matches && resource_version_present
    );
}

#[test]
fn pod_cleanup_gate_serializes_rebuild_and_claim_deletion() {
    /* Pod/PVC cleanup-gate table KPG1. Causes: C1 Pod UID/RV evidence is
     * complete and exact/stale; C2 Pod is live/terminating; C3 the sole
     * Awaken cleanup finalizer is absent/present. Effects: E1 exact live A
     * acquires the finalizer by UID+RV CAS before PVC I/O; E2 a prior
     * successful acquisition resumes across crashes, including while A is
     * terminating; E3 a Rebuild DELETE that made A terminating before the
     * gate rejects with zero PVC effect; E4 incomplete/foreign evidence
     * rejects. Rules: KPG1a exact+live+no-finalizer=>E1; KPG1b
     * exact+finalizer=>E2; KPG1c exact+terminating+no-finalizer=>E3; KPG1d
     * !exact=>E4. The API patch and release live rows are covered by KPV4;
     * this closed decision owns every pre-PVC interleaving. */
    use PodCleanupGateAdmission::{
        Acquire, DeletingBeforeGate, MissingResourceVersion, MissingUid, Resume, StaleUid,
    };

    assert_eq!(
        pod_cleanup_gate_admission(true, true, true, false, false),
        Acquire,
        "KPG1a"
    );
    for terminating in [false, true] {
        assert_eq!(
            pod_cleanup_gate_admission(true, true, true, terminating, true),
            Resume,
            "KPG1b"
        );
    }
    assert_eq!(
        pod_cleanup_gate_admission(true, true, true, true, false),
        DeletingBeforeGate,
        "KPG1c"
    );
    assert_eq!(
        pod_cleanup_gate_admission(false, false, true, false, false),
        MissingUid,
        "KPG1d"
    );
    assert_eq!(
        pod_cleanup_gate_admission(true, false, true, false, false),
        StaleUid,
        "KPG1d"
    );
    assert_eq!(
        pod_cleanup_gate_admission(true, true, false, false, false),
        MissingResourceVersion,
        "KPG1d"
    );

    /* Release response-loss rows KPG2. Once exact A's DELETE is visible,
     * missing our finalizer means the prior CAS removal committed and the
     * retry may continue awaiting A/P. Before DELETE, the same absence is a
     * lost serialization gate and must fail closed. */
    use PodCleanupGateReleaseAdmission::{AlreadyReleasedAfterDelete, LostBeforeDelete, Remove};
    assert_eq!(
        pod_cleanup_gate_release_admission(true, true),
        Remove,
        "KPG2a"
    );
    assert_eq!(
        pod_cleanup_gate_release_admission(true, false),
        AlreadyReleasedAfterDelete,
        "KPG2b"
    );
    assert_eq!(
        pod_cleanup_gate_release_admission(false, false),
        LostBeforeDelete,
        "KPG2c"
    );
}

#[test]
fn absent_pod_never_authorizes_a_new_claim_delete() {
    /* Absent-Pod/PVC table KAP1. Causes: C1 persisted V2 claim UID P;
     * C2 source Pod A is absent; C3 P is absent/live/terminating/foreign.
     * Effects: E1 absent P is idempotent cleanup completion; E2 exact
     * terminating P may only be awaited; E3 exact live P fails closed with
     * zero DELETE because Rebuild could concurrently bind it; E4 malformed
     * or foreign P fails closed. Rules: KAP1a absent=>E1; KAP1b
     * exact+terminating=>E2; KAP1c exact+live=>E3; KAP1d !exact=>E4. */
    use AbsentPodClaimAdmission::{
        AlreadyAbsent, AwaitTerminating, MissingUid, StaleUid, UnsafeLiveClaim,
    };

    assert_eq!(
        absent_pod_claim_admission(false, false, false, false),
        AlreadyAbsent,
        "KAP1a"
    );
    assert_eq!(
        absent_pod_claim_admission(true, true, true, true),
        AwaitTerminating,
        "KAP1b"
    );
    assert_eq!(
        absent_pod_claim_admission(true, true, true, false),
        UnsafeLiveClaim,
        "KAP1c"
    );
    assert_eq!(
        absent_pod_claim_admission(true, false, false, false),
        MissingUid,
        "KAP1d"
    );
    assert_eq!(
        absent_pod_claim_admission(true, true, false, false),
        StaleUid,
        "KAP1d"
    );
}

pub(super) fn claim_uid(claim: &PersistentVolumeClaim) -> Result<String, RuntimeError> {
    claim
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes continuation PVC has no UID"))
}

pub(super) fn bind_claim_uid(pod: &mut k8s_openapi::api::core::v1::Pod, uid: &str) {
    pod.metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(CLAIM_UID_ANNOTATION.into(), uid.into());
}

pub(super) fn bound_claim_uid(pod: &k8s_openapi::api::core::v1::Pod) -> Option<&str> {
    pod.metadata
        .annotations
        .as_ref()?
        .get(CLAIM_UID_ANNOTATION)
        .map(String::as_str)
}

/// Select only the canonical continuation volume. Other PVC-backed Resource or
/// Cache mounts are independent authorities and must never be mistaken for the
/// mutable-filesystem continuation claim.
pub(super) fn bound_claim_name(
    pod: &k8s_openapi::api::core::v1::Pod,
) -> Result<Option<&str>, RuntimeError> {
    let claims = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .into_iter()
        .flatten()
        .filter(|volume| volume.name == CONTINUATION_VOLUME)
        .collect::<Vec<_>>();
    match claims.as_slice() {
        [] => Ok(None),
        [volume] => volume
            .persistent_volume_claim
            .as_ref()
            .map(|claim| claim.claim_name.as_str())
            .filter(|name| !name.is_empty())
            .map(Some)
            .ok_or_else(|| backend("Kubernetes continuation volume has no exact PVC claim")),
        _ => Err(backend(
            "Kubernetes Sandbox Pod has multiple canonical continuation volumes",
        )),
    }
}

pub(super) fn handle_extra(
    pod: &k8s_openapi::api::core::v1::Pod,
) -> Result<Option<ContainerContinuationHandle>, RuntimeError> {
    let pod_uid = pod
        .metadata
        .uid
        .clone()
        .ok_or_else(|| backend("Kubernetes Sandbox Pod has no UID"))?;
    let binds_continuation_claim = bound_claim_name(pod)?.is_some();
    match (binds_continuation_claim, bound_claim_uid(pod)) {
        (false, None) => Ok(Some(
            ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid,
                claim_uid: None,
            },
        )),
        (true, Some(uid)) => Ok(Some(
            ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid,
                claim_uid: Some(uid.to_owned()),
            },
        )),
        (true, None) => Err(backend(
            "Kubernetes Sandbox Pod has no continuation PVC incarnation evidence",
        )),
        (false, Some(_)) => Err(backend(
            "Kubernetes Sandbox Pod has continuation PVC incarnation evidence without a bound claim",
        )),
    }
}

pub(super) fn build_claim(
    id: &str,
    config: &crate::K8sContinuationVolume,
) -> Result<PersistentVolumeClaim, RuntimeError> {
    let size = config.size.trim();
    if size.is_empty() {
        return Err(backend("k8s continuation volume size cannot be empty"));
    }
    let storage_class_name = config
        .storage_class_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok(PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(continuation_claim_name(id)),
            labels: Some(std::collections::BTreeMap::from([
                ("app".into(), "awaken-sandbox".into()),
                ("awaken-continuation".into(), "active".into()),
            ])),
            // Worker and Pod failure must not cascade into active Session data.
            owner_references: None,
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".into()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(std::collections::BTreeMap::from([(
                    "storage".into(),
                    Quantity(size.to_owned()),
                )])),
                ..Default::default()
            }),
            storage_class_name,
            volume_mode: Some("Filesystem".into()),
            ..Default::default()
        }),
        ..Default::default()
    })
}

pub(super) fn append_init_container(
    plan: &ContainerPlan,
    subpaths: &[String],
    init_containers: &mut Vec<Container>,
) {
    if subpaths.is_empty() {
        return;
    }
    let directories = subpaths
        .iter()
        .map(|path| format!("/state/{path}"))
        .collect::<Vec<_>>()
        .join(" ");
    init_containers.push(Container {
        name: "continuation-init".into(),
        image: Some(plan.image.clone()),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("mkdir -p {directories}"),
        ]),
        volume_mounts: Some(vec![VolumeMount {
            name: CONTINUATION_VOLUME.to_owned(),
            mount_path: "/state".into(),
            ..Default::default()
        }]),
        security_context: Some(hardened_security_context()),
        ..Default::default()
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingClaimDeletion {
    name: String,
    uid: String,
}

/// Establish the irreversible, UID/resourceVersion-fenced PVC deletion intent
/// while the exact source Pod still occupies its stable name. Kubernetes PVC
/// protection keeps the bytes until that Pod is deleted, while Create/Rebuild
/// admission observes `deletionTimestamp` and cannot reuse the claim.
pub(super) async fn initiate_claim_deletion(
    claims: &Api<PersistentVolumeClaim>,
    pod_name: &str,
    expected_uid: &str,
) -> Result<Option<PendingClaimDeletion>, RuntimeError> {
    let name = continuation_claim_for_pod(pod_name)?;
    let observed = match claims.get(&name).await {
        Ok(claim) => claim,
        Err(error) if api_not_found(&error) => return Ok(None),
        Err(error) => return Err(backend(error)),
    };
    let uid = observed.metadata.uid.clone();
    let resource_version = observed.metadata.resource_version.clone();
    match claim_deletion_admission(
        uid.is_some(),
        uid.as_deref() == Some(expected_uid),
        resource_version.is_some(),
    ) {
        ClaimDeletionAdmission::DeleteExact => {}
        ClaimDeletionAdmission::MissingUid => {
            return Err(backend("Kubernetes continuation PVC has no UID"));
        }
        ClaimDeletionAdmission::StaleUid => {
            return Err(backend(format!(
                "continuation PVC `{name}` incarnation changed before disposal"
            )));
        }
        ClaimDeletionAdmission::MissingResourceVersion => {
            return Err(backend(
                "Kubernetes continuation PVC has no resourceVersion",
            ));
        }
    }
    let uid = uid.expect("deletion admission proved the claim UID is present");
    let resource_version =
        resource_version.expect("deletion admission proved the claim resourceVersion is present");
    if observed.metadata.deletion_timestamp.is_some() {
        return Ok(Some(PendingClaimDeletion { name, uid }));
    }
    let params = DeleteParams::default().preconditions(Preconditions {
        uid: Some(uid.clone()),
        resource_version: Some(resource_version),
    });
    match claims.delete(&name, &params).await {
        Ok(_) => {}
        Err(error) if api_not_found(&error) => return Ok(None),
        Err(error) => return Err(backend(error)),
    }
    Ok(Some(PendingClaimDeletion { name, uid }))
}

/// Classify an exact retained claim after its persisted Pod incarnation is
/// absent. A live claim cannot be deleted safely because a concurrent Rebuild
/// can bind it between any read and PVC DELETE. A prior gated cleanup is
/// recoverable only through the claim's existing deletionTimestamp; total
/// absence is the idempotent response-loss row.
pub(super) async fn absent_pod_claim_deletion(
    claims: &Api<PersistentVolumeClaim>,
    pod_name: &str,
    expected_uid: &str,
) -> Result<Option<PendingClaimDeletion>, RuntimeError> {
    let name = continuation_claim_for_pod(pod_name)?;
    let observed = match claims.get(&name).await {
        Ok(claim) => Some(claim),
        Err(error) if api_not_found(&error) => None,
        Err(error) => return Err(backend(error)),
    };
    let uid = observed
        .as_ref()
        .and_then(|claim| claim.metadata.uid.clone());
    match absent_pod_claim_admission(
        observed.is_some(),
        uid.is_some(),
        uid.as_deref() == Some(expected_uid),
        observed
            .as_ref()
            .is_some_and(|claim| claim.metadata.deletion_timestamp.is_some()),
    ) {
        AbsentPodClaimAdmission::AlreadyAbsent => Ok(None),
        AbsentPodClaimAdmission::AwaitTerminating => Ok(Some(PendingClaimDeletion {
            name,
            uid: uid.expect("absent-Pod claim admission proved UID is present"),
        })),
        AbsentPodClaimAdmission::UnsafeLiveClaim => Err(backend(
            "live Kubernetes continuation PVC cannot be disposed after its source Pod is absent",
        )),
        AbsentPodClaimAdmission::MissingUid => {
            Err(backend("Kubernetes continuation PVC has no UID"))
        }
        AbsentPodClaimAdmission::StaleUid => Err(backend(format!(
            "continuation PVC `{name}` incarnation changed before disposal"
        ))),
    }
}

/// Wait only for the exact claim incarnation whose delete was accepted. If the
/// stable name already denotes a different UID, the authorized incarnation is
/// gone and this cleanup must neither wait on nor mutate its replacement.
pub(super) async fn await_claim_deletion(
    claims: &Api<PersistentVolumeClaim>,
    pending: &PendingClaimDeletion,
) -> Result<(), RuntimeError> {
    let deadline = tokio::time::Instant::now() + DELETE_TIMEOUT;
    loop {
        match claims.get(&pending.name).await {
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
            Ok(observed) if claim_uid(&observed)? != pending.uid => return Ok(()),
            Ok(_) if tokio::time::Instant::now() >= deadline => {
                return Err(backend(format!(
                    "continuation PVC `{}` incarnation `{}` was not deleted within {}s",
                    pending.name,
                    pending.uid,
                    DELETE_TIMEOUT.as_secs()
                )));
            }
            Ok(_) => tokio::time::sleep(DELETE_POLL).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_provisioning_contract as pc;

    use super::*;
    use crate::{ContainerPlan, NetworkMode, RootfsPlan};

    #[test]
    fn cleanup_gate_preserves_preparation_across_successor_failover() {
        /* Cleanup provenance/failover table KPG3. Causes: C1 the exact Pod has
         * neither/both/inconsistently one cleanup finalizer and its canonical
         * gate annotation; C2 the annotation carries the exact/foreign original
         * preparation A and fingerprint; C3 its latest successor is B/C; C4 the
         * requested aggregate-authorized successor is the exact replay B, a
         * same-lease renewal, higher-epoch C, late stale B after C, or foreign.
         * Effects: E1 only neither evidence acquires A+fingerprint+B by the same
         * Pod UID/RV CAS; E2 exact B response-loss replay resumes idempotently;
         * E3 an authorized renewal/C advances only latest_successor while A and
         * the fingerprint remain byte-exact; E4 stale/foreign/incomplete rows
         * reject before PVC or Pod mutation. The participant API interleaving is
         * covered by KPV7; this pure table owns annotation admission/codec.
         *
         * | Rule | gate evidence | requested successor | Effect |
         * |---|---|---|---|
         * | KPG3a | none | B | E1 acquire |
         * | KPG3b | A/fp/B | B | E2 resume |
         * | KPG3c | A/fp/B | renewed B or C | E3 advance |
         * | KPG3d | A/fp/C | late B | E4 reject |
         * | KPG3e | foreign A/fp/latest | C | E4 reject |
         * | KPG3f | finalizer xor annotation | any | E4 reject |
         */
        use PodCleanupAuthorizationAdmission::{Acquire, AdvanceSuccessor, Resume};

        let prepared =
            pc::SandboxEffectFence::new("prepare-a", "owner-a", "runtime-a", 4, 40_000).unwrap();
        let fingerprint = "preparation-fingerprint-a";
        let preparation =
            pc::SandboxDisposalPreparation::new(prepared.clone(), fingerprint).unwrap();
        let authorization_id = preparation.operation_id().unwrap();
        let successor_b = pc::SandboxEffectFence::new(
            authorization_id.clone(),
            "owner-a",
            "runtime-a",
            4,
            50_000,
        )
        .unwrap();
        let successor_b_renewed = pc::SandboxEffectFence::new(
            authorization_id.clone(),
            "owner-a",
            "runtime-a",
            4,
            55_000,
        )
        .unwrap();
        let successor_c =
            pc::SandboxEffectFence::new(authorization_id, "owner-c", "runtime-c", 5, 60_000)
                .unwrap();
        let authorization_b = pc::SandboxDisposalAuthorization::new(
            prepared.clone(),
            successor_b.clone(),
            fingerprint,
        )
        .unwrap();
        let authorization_b_renewed = pc::SandboxDisposalAuthorization::new(
            prepared.clone(),
            successor_b_renewed,
            fingerprint,
        )
        .unwrap();
        let authorization_c =
            pc::SandboxDisposalAuthorization::new(prepared.clone(), successor_c, fingerprint)
                .unwrap();

        assert_eq!(
            pod_cleanup_authorization_admission(None, &authorization_b).unwrap(),
            Acquire,
            "KPG3a/E1"
        );

        let mut pod = Pod::default();
        pod.metadata.finalizers = Some(vec![CONTINUATION_CLEANUP_FINALIZER.to_owned()]);
        stamp_pod_cleanup_authorization(&mut pod, &authorization_b).unwrap();
        let replayed = observed_pod_cleanup_authorization(&pod).unwrap().unwrap();
        assert_eq!(replayed, authorization_b, "KPG3b codec/E2");
        assert_eq!(
            pod_cleanup_authorization_admission(Some(&replayed), &authorization_b).unwrap(),
            Resume,
            "KPG3b/E2"
        );
        assert_eq!(
            pod_cleanup_authorization_admission(Some(&replayed), &authorization_b_renewed,)
                .unwrap(),
            AdvanceSuccessor,
            "KPG3c same-lease renewal/E3"
        );
        assert_eq!(
            pod_cleanup_authorization_admission(Some(&replayed), &authorization_c).unwrap(),
            AdvanceSuccessor,
            "KPG3c A-to-B response loss then A-to-C/E3"
        );
        assert_eq!(
            authorization_c.prepared_effect_fence(),
            &prepared,
            "KPG3c immutable A"
        );
        assert_eq!(
            authorization_c.preparation_fingerprint(),
            fingerprint,
            "KPG3c immutable fingerprint"
        );
        assert!(
            pod_cleanup_authorization_admission(Some(&authorization_c), &authorization_b).is_err(),
            "KPG3d late B after C/E4"
        );

        let foreign_prepared = pc::SandboxEffectFence::new(
            "prepare-foreign",
            "owner-foreign",
            "runtime-foreign",
            4,
            40_000,
        )
        .unwrap();
        let foreign_preparation =
            pc::SandboxDisposalPreparation::new(foreign_prepared, "foreign-fingerprint").unwrap();
        let foreign_id = foreign_preparation.operation_id().unwrap();
        let foreign = foreign_preparation
            .authorize(
                pc::SandboxEffectFence::new(
                    foreign_id,
                    "owner-foreign",
                    "runtime-foreign",
                    4,
                    50_000,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(
            pod_cleanup_authorization_admission(Some(&foreign), &authorization_c).is_err(),
            "KPG3e/E4"
        );
        let finalizer_only = Pod {
            metadata: ObjectMeta {
                finalizers: Some(vec![CONTINUATION_CLEANUP_FINALIZER.to_owned()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut annotation_only = Pod::default();
        stamp_pod_cleanup_authorization(&mut annotation_only, &authorization_b).unwrap();
        for inconsistent in [&finalizer_only, &annotation_only] {
            assert!(
                observed_pod_cleanup_authorization(inconsistent).is_err(),
                "KPG3f/E4"
            );
        }
    }

    #[test]
    fn create_claim_admission_is_exact_and_total() {
        /* Claim-only create recovery cause/effect table. Causes: C1 the
         * canonical retained PVC is absent/present; C2 an observed PVC has an
         * immutable UID or is malformed; C3 it is live/terminating; C4 its
         * realization digest equals/differs from the desired claim; C5 its
         * persisted aggregate effect is exact, missing, foreign generation, or
         * newer than the caller's asserted expiry. Effects: E1 an absent claim
         * may be created; E2 only an exact same-effect claim may be reused after
         * response loss; E3 every incomplete, foreign, stale-caller, terminating,
         * or different-realization row rejects without a participant mutation.
         * Rules: CC1 !C1=>E1; CC2 C1+UID+live+exact(C4)+exact-or-renewed(C5)=>E2;
         * CC3 every other C1 row=>E3. Rebuild is excluded: it remains authorized
         * only by the source V2 handle's claim UID in
         * `rebuild_continuation_expectation`. */
        let expected =
            pc::SandboxEffectFence::new("create-1", "owner-1", "runtime-1", 7, 200).unwrap();
        let original =
            pc::SandboxEffectFence::new("create-1", "owner-1", "runtime-1", 7, 100).unwrap();
        assert_eq!(
            create_claim_admission(&expected, None).unwrap(),
            CreateClaimAdmission::Create,
            "CC1"
        );
        let exact = CreateClaimObservation {
            effect_fence: Some(&original),
            uid_present: true,
            terminating: false,
            realization_matches: true,
        };
        assert_eq!(
            create_claim_admission(&expected, Some(exact)).unwrap(),
            CreateClaimAdmission::Reuse,
            "CC2 renewal-compatible replay"
        );

        let foreign =
            pc::SandboxEffectFence::new("create-foreign", "owner-2", "runtime-2", 8, 200).unwrap();
        let newer_expiry =
            pc::SandboxEffectFence::new("create-1", "owner-1", "runtime-1", 7, 300).unwrap();
        for rejected in [
            CreateClaimObservation {
                uid_present: false,
                ..exact
            },
            CreateClaimObservation {
                terminating: true,
                ..exact
            },
            CreateClaimObservation {
                realization_matches: false,
                ..exact
            },
            CreateClaimObservation {
                effect_fence: None,
                ..exact
            },
            CreateClaimObservation {
                effect_fence: Some(&foreign),
                ..exact
            },
            CreateClaimObservation {
                effect_fence: Some(&newer_expiry),
                ..exact
            },
        ] {
            assert!(
                create_claim_admission(&expected, Some(rejected)).is_err(),
                "CC3"
            );
        }
    }

    fn plan() -> ContainerPlan {
        ContainerPlan {
            image: "agent:1".into(),
            command: vec!["sleep".into(), "30".into()],
            env: Vec::new(),
            control_services: Default::default(),
            packages: Default::default(),
            binds: Vec::new(),
            outputs_volume: "/mnt/session/outputs".into(),
            network: NetworkMode::Open,
            egress_identity: Default::default(),
            requests: pc::ResourceRequests::default(),
            limits: pc::ResourceLimits::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            memory_mounts: Vec::new(),
            rootfs: RootfsPlan::HostUserland,
        }
    }

    #[test]
    fn continuation_claim_decision_table() {
        /* Filesystem-continuity cause/effect table.
         * Causes: C1 the deployment has a continuation-volume policy (covered
         * by the existing creation tests); C2 the neutral spec requests a
         * retained or ephemeral filesystem. Effects: E1 allocate/reuse the
         * canonical PVC; E2 realize the same Pod without a PVC.
         * Rules: FC1 C1+retained=>E1; FC2 C1+ephemeral=>E2. FC2 is the
         * prompt-free capability-probe path and must not consume Session SSD.
         */
        let mut retained = plan();
        assert!(claim_required(&retained, true), "FC1");
        retained.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
        assert!(!claim_required(&retained, true), "FC2");
    }

    #[test]
    fn continuation_claim_deletion_fails_closed_on_every_missing_fence() {
        assert_eq!(
            claim_deletion_admission(false, false, false),
            ClaimDeletionAdmission::MissingUid,
        );
        assert_eq!(
            claim_deletion_admission(true, false, true),
            ClaimDeletionAdmission::StaleUid,
        );
        assert_eq!(
            claim_deletion_admission(true, true, false),
            ClaimDeletionAdmission::MissingResourceVersion,
        );
        assert_eq!(
            claim_deletion_admission(true, true, true),
            ClaimDeletionAdmission::DeleteExact,
        );
    }

    #[tokio::test]
    async fn handle_evidence_follows_the_realized_continuation_binding() {
        /* Handle-evidence cause/effect graph.
         * Causes: C1 the deployment has continuation storage configured; C2
         * the realized Pod binds the canonical continuation PVC; C3 the Pod
         * carries the exact PVC-incarnation UID annotation; C4 the API supplied
         * an immutable Pod UID. Effects: E1 persist the Pod-only UID fence for a
         * PVC-free Pod; E2 persist both fences for a retained Pod; E3 fail closed
         * on missing/orphaned evidence or an ambiguous canonical selector.
         * Decision rules: H1/H2 !C2+!C3+C4=>E1; H3 C2+C3+C4=>E2;
         * H4 C2+!C3+C4=>E3; H5 !C2+C3+C4=>E3; H6 !C4=>E3;
         * H7 duplicate canonical volumes or an empty claim name=>E3.
         * C2 is the realized-object authority, so this does not duplicate the
         * typed claim selector used by allocation and Pod projection.
         * Constraint/invariant: every current handle fences Pod disposal by UID;
         * claim evidence exists iff that exact Pod binds the canonical PVC.
         */
        let plain = super::super::K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        let mut plain_pod = plain.pod("configured-off", &plan());
        assert!(handle_extra(&plain_pod).is_err(), "H6");
        plain_pod.metadata.uid = Some("pod-plain-1".into());
        assert_eq!(
            handle_extra(&plain_pod).unwrap(),
            Some(ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "pod-plain-1".into(),
                claim_uid: None,
            }),
            "H1"
        );

        let configured = super::super::K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
            .with_continuation_volume(crate::K8sContinuationVolume {
                storage_class_name: Some("retained-rwo".into()),
                size: "8Gi".into(),
            });
        let mut ephemeral_plan = plan();
        ephemeral_plan.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
        let mut ephemeral = configured.pod("probe", &ephemeral_plan);
        ephemeral.metadata.uid = Some("pod-ephemeral-1".into());
        assert_eq!(
            handle_extra(&ephemeral).unwrap(),
            Some(ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "pod-ephemeral-1".into(),
                claim_uid: None,
            }),
            "H2"
        );

        let mut retained = configured.pod("session", &plan());
        retained.metadata.uid = Some("pod-retained-1".into());
        assert!(handle_extra(&retained).is_err(), "H4");
        bind_claim_uid(&mut retained, "claim-incarnation-1");
        assert_eq!(
            handle_extra(&retained).unwrap(),
            Some(ContainerContinuationHandle::KubernetesContinuationV2 {
                pod_uid: "pod-retained-1".into(),
                claim_uid: Some("claim-incarnation-1".into()),
            }),
            "H3"
        );

        let canonical_volume = retained
            .spec
            .as_ref()
            .and_then(|spec| spec.volumes.as_ref())
            .and_then(|volumes| {
                volumes
                    .iter()
                    .find(|volume| volume.name == CONTINUATION_VOLUME)
            })
            .cloned()
            .expect("retained Pod has one canonical continuation volume");
        let mut duplicate = retained.clone();
        duplicate
            .spec
            .as_mut()
            .and_then(|spec| spec.volumes.as_mut())
            .expect("retained Pod has volumes")
            .push(canonical_volume);
        assert!(handle_extra(&duplicate).is_err(), "H7 duplicate selector");
        let claim = retained
            .spec
            .as_mut()
            .and_then(|spec| spec.volumes.as_mut())
            .and_then(|volumes| {
                volumes
                    .iter_mut()
                    .find(|volume| volume.name == CONTINUATION_VOLUME)
            })
            .and_then(|volume| volume.persistent_volume_claim.as_mut())
            .expect("retained Pod has one claim projection");
        claim.claim_name.clear();
        assert!(handle_extra(&retained).is_err(), "H7 empty claim name");

        let mut orphaned = ephemeral;
        bind_claim_uid(&mut orphaned, "orphaned-incarnation");
        assert!(handle_extra(&orphaned).is_err(), "H5");
    }

    #[tokio::test]
    async fn pod_projection_matches_the_continuation_allocation_decision() {
        /* Filesystem-continuity cause/effect table at the final Pod projection.
         * Causes: C1 a continuation policy is configured; C2 the exact neutral
         * plan is retained/ephemeral. Effects: E1 the Pod references the one
         * canonical PVC; E2 the Pod has no PVC reference. Rules:
         * KP1 C1+retained=>E1; KP2 C1+ephemeral=>E2. Testing only claim creation
         * is insufficient: a stale Pod reference to an intentionally omitted
         * claim leaves disposable ACP probes Pending and exhausts storage quota.
         */
        let rt = super::super::K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
            .with_continuation_volume(crate::K8sContinuationVolume {
                storage_class_name: Some("retained-rwo".into()),
                size: "8Gi".into(),
            });
        let mut retained = plan();
        let retained_pod = rt.pod("retained", &retained);
        assert!(
            retained_pod
                .spec
                .unwrap()
                .volumes
                .unwrap()
                .iter()
                .any(|volume| {
                    volume.name == CONTINUATION_VOLUME && volume.persistent_volume_claim.is_some()
                }),
            "KP1"
        );

        retained.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
        let ephemeral_pod = rt.pod("ephemeral", &retained);
        assert!(
            ephemeral_pod
                .spec
                .unwrap()
                .volumes
                .unwrap()
                .iter()
                .all(|volume| volume.persistent_volume_claim.is_none()),
            "KP2"
        );
    }

    #[test]
    fn pvc_is_one_non_owned_binding_for_every_mutable_root() {
        /* Active-volume cause/effect graph and decision table.
         * Causes: C1 continuation policy absent/present; C2 canonical writable
         * roots are workspace/output/tmp; C3 Pod or Worker ownership disappears;
         * C4 storage policy is blank/valid. Effects: E1 legacy emptyDirs; E2 one
         * deterministic PVC with no ownerReference; E3 distinct pre-created
         * subpaths mounted at every C2 root; E4 fail before an API write.
         * Rules: V1 !C1=>E1; V2 C1+C2+C3+valid(C4)=>E2+E3;
         * V3 C1+blank(C4)=>E4. FMECA: a Pod-owned claim makes Pod recovery lose
         * live Session data (S5/O2/D3=30), while reusing one subpath for several
         * roots aliases unrelated data (S4/O2/D3=24). This module is the single
         * owner of both claim and mount identity.
         */
        let config = crate::K8sContinuationVolume {
            storage_class_name: Some("fast-rwo".into()),
            size: "8Gi".into(),
        };
        let claim = build_claim("session-1", &config).unwrap();
        assert_eq!(claim.metadata.name.as_deref(), Some("awc-session-1"), "V2");
        assert!(claim.metadata.owner_references.is_none(), "V2/C3");
        let claim_spec = claim.spec.as_ref().unwrap();
        assert_eq!(claim_spec.storage_class_name.as_deref(), Some("fast-rwo"));
        assert_eq!(
            claim_spec.access_modes.as_deref(),
            Some(&[String::from("ReadWriteOnce")][..])
        );

        let pod = super::super::build_pod_with_continuation(
            "session-1",
            &plan(),
            &None,
            None,
            &[],
            Some("awc-session-1"),
            None,
            None,
        );
        let spec = pod.spec.unwrap();
        let continuation = spec
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .filter(|volume| volume.name == CONTINUATION_VOLUME)
            .collect::<Vec<_>>();
        assert_eq!(continuation.len(), 1, "V2");
        assert_eq!(
            continuation[0]
                .persistent_volume_claim
                .as_ref()
                .map(|source| source.claim_name.as_str()),
            Some("awc-session-1"),
            "V2"
        );
        let mounts = spec.containers[0]
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .filter(|mount| mount.name == CONTINUATION_VOLUME)
            .collect::<Vec<_>>();
        assert_eq!(
            mounts
                .iter()
                .map(|mount| (mount.mount_path.as_str(), mount.sub_path.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("/workspace", Some("root-0")),
                ("/mnt/session/outputs", Some("root-1")),
                ("/tmp", Some("root-2")),
            ],
            "V2/E3"
        );
        let init = spec
            .init_containers
            .as_ref()
            .and_then(|containers| containers.iter().find(|c| c.name == "continuation-init"))
            .expect("V2 initializes subpaths before kubelet mounts the agent");
        assert!(
            init.command.as_ref().unwrap()[2].contains("/state/root-0 /state/root-1 /state/root-2"),
            "V2/E3"
        );
        assert!(
            build_claim(
                "session-1",
                &crate::K8sContinuationVolume {
                    storage_class_name: None,
                    size: "  ".into(),
                }
            )
            .is_err(),
            "V3/E4"
        );

        let mut fenced = super::super::build_pod_with_continuation(
            "session-1",
            &plan(),
            &None,
            None,
            &[],
            Some("awc-session-1"),
            None,
            None,
        );
        bind_claim_uid(&mut fenced, "claim-incarnation-1");
        assert_eq!(
            bound_claim_uid(&fenced),
            Some("claim-incarnation-1"),
            "V2/C3"
        );
    }
}
