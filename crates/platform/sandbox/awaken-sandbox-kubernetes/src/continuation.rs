// @formal entrypoint resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal entrypoint sandbox_continuation.distinct_mutable_roots_share_one_retained_claim_without_aliasing
// @formal entrypoint sandbox_continuation.ephemeral_never_allocates_or_references_a_claim
//! Kubernetes realization of one retained active-filesystem volume.

use super::{
    SANDBOX_ADOPTION_ANNOTATION, SANDBOX_ATTEMPT_ANNOTATION, SANDBOX_REALIZATION_ANNOTATION,
    participants, realization,
};
use awaken_container_contract::RuntimeError;
use awaken_sandbox_container::ContainerContinuationHandle;
use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::{DeleteParams, Preconditions};

use super::pod_projection::CONTINUATION_VOLUME;
use super::{
    ContainerPlan, ContainerRealizationContext, ContainerRealizationIntent, K8sRuntime,
    SandboxEffectFence, backend, stamp_effect_fence, stamp_realization,
};
use awaken_worker_fleet_kubernetes::api_not_found;
use awaken_worker_fleet_kubernetes::managed_names::{
    continuation_claim_for_pod, continuation_claim_name,
};

pub(super) const ORPHANED_LIVE_CLAIM_ERROR: &str =
    "live Kubernetes continuation PVC cannot be disposed after its source Pod is absent";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CreateClaimAdmission {
    Create,
    Reuse,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CreateClaimObservation<'a> {
    pub(super) effect_fence: Option<&'a SandboxEffectFence>,
    pub(super) uid_present: bool,
    pub(super) terminating: bool,
}

pub(super) fn original_claim_effect_matches(
    expected: &SandboxEffectFence,
    observed: &SandboxEffectFence,
) -> bool {
    observed.same_effect_identity(expected)
        && observed.expires_at_unix_ms() <= expected.expires_at_unix_ms()
}

pub(super) fn create_claim_admission(
    expected: &SandboxEffectFence,
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
    let Some(observed_fence) = observed.effect_fence else {
        return Err(backend(
            "existing Kubernetes continuation PVC has no durable effect fence",
        ));
    };
    if !original_claim_effect_matches(expected, observed_fence) {
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
    if source_runtime_handle
        .as_ref()
        .is_some_and(ContainerContinuationHandle::terminal_only)
    {
        return Err(backend("joined-empty evidence cannot authorize rebuild"));
    }
    let Some((pod_uid, Some(claim_uid))) = source_runtime_handle
        .as_ref()
        .and_then(ContainerContinuationHandle::kubernetes_incarnation)
    else {
        return Err(backend(
            "Kubernetes rebuild requires current Pod and continuation PVC incarnation evidence",
        ));
    };
    if pod_uid != source_incarnation.as_str() {
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
        claim_uid: claim_uid.to_owned(),
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
mod pod_cleanup;
pub(super) use pod_cleanup::*;

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
pub(super) enum AbsentPodClaimAdmission {
    AlreadyAbsent,
    AwaitTerminating,
    UnsafeLiveClaim,
    MissingUid,
    StaleUid,
}

#[must_use]
pub(super) const fn absent_pod_claim_admission(
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
/// @obligation sandbox_continuation.ephemeral_never_allocates_or_references_a_claim
/// @kani profile=solver-kissat-v1
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
/// @obligation resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
/// @kani profile=solver-kissat-v1
/// @proof-input-debt free_symbol absent_pod_claim_admission present
/// @proof-input-debt free_symbol absent_pod_claim_admission terminating
/// @proof-input-debt free_symbol absent_pod_claim_admission uid_matches
/// @proof-input-debt free_symbol absent_pod_claim_admission uid_present
#[kani::proof]
fn absent_pod_claim_admission_is_total_and_never_starts_deletion() {
    // KAP-proof: all 16 raw observations, including malformed combinations,
    // refine the existing KAP1 decision table. Only an exact already-terminating
    // incarnation can be awaited; a live claim always rejects. API effects
    // remain checked by KPV5/KPV6, not asserted by this pure-kernel proof.
    let present = kani::any::<bool>();
    let uid_present = kani::any::<bool>();
    let uid_matches = kani::any::<bool>();
    let terminating = kani::any::<bool>();
    let decision = absent_pod_claim_admission(present, uid_present, uid_matches, terminating);
    use AbsentPodClaimAdmission::*;
    assert_eq!(matches!(decision, AlreadyAbsent), !present);
    assert_eq!(matches!(decision, MissingUid), present && !uid_present);
    assert_eq!(
        matches!(decision, StaleUid),
        present && uid_present && !uid_matches
    );
    assert_eq!(
        matches!(decision, AwaitTerminating),
        present && uid_present && uid_matches && terminating
    );
    assert_eq!(
        matches!(decision, UnsafeLiveClaim),
        present && uid_present && uid_matches && !terminating
    );
}

#[cfg(kani)]
/// @obligation resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
/// @kani profile=solver-kissat-v1
/// @proof-input-debt free_symbol claim_deletion_admission resource_version_present
/// @proof-input-debt free_symbol claim_deletion_admission uid_matches
/// @proof-input-debt free_symbol claim_deletion_admission uid_present
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
        pod_cleanup_gate_admission(true, true, true, false, false, false),
        Acquire,
        "KPG1a"
    );
    for terminating in [false, true] {
        assert_eq!(
            pod_cleanup_gate_admission(true, true, true, terminating, true, false),
            Resume,
            "KPG1b"
        );
    }
    assert_eq!(
        pod_cleanup_gate_admission(true, true, true, true, false, false),
        DeletingBeforeGate,
        "KPG1c"
    );
    assert_eq!(
        pod_cleanup_gate_admission(false, false, true, false, false, false),
        MissingUid,
        "KPG1d"
    );
    assert_eq!(
        pod_cleanup_gate_admission(true, false, true, false, false, false),
        StaleUid,
        "KPG1d"
    );
    assert_eq!(
        pod_cleanup_gate_admission(true, true, false, false, false, false),
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

pub(super) fn validated_bound_pod_claim_uid(
    observed_pod: Option<&Pod>,
    persisted_pod_uid: Option<&str>,
    persisted_claim_uid: Option<&str>,
) -> Result<Option<String>, RuntimeError> {
    let pod_claim_uid = observed_pod.and_then(bound_claim_uid).map(str::to_owned);
    match (observed_pod, persisted_pod_uid) {
        (None, _) => {}
        (Some(_), None) => {
            return Err(backend(
                "Kubernetes Sandbox deletion requires a persisted Pod UID",
            ));
        }
        (Some(pod), Some(expected)) if pod.metadata.uid.as_deref() != Some(expected) => {
            return Err(backend(
                "Kubernetes Sandbox Pod incarnation changed before deletion",
            ));
        }
        (Some(_), Some(_)) => {}
    }
    if persisted_pod_uid.is_some()
        && observed_pod.is_some()
        && persisted_claim_uid != pod_claim_uid.as_deref()
    {
        return Err(backend(
            "Sandbox Pod continuation PVC differs from its persisted incarnation evidence",
        ));
    }
    Ok(pod_claim_uid)
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
    let claim_uid = match (binds_continuation_claim, bound_claim_uid(pod)) {
        (false, None) => None,
        (true, Some(uid)) => Some(uid.to_owned()),
        (true, None) => {
            return Err(backend(
                "Kubernetes Sandbox Pod has no continuation PVC incarnation evidence",
            ));
        }
        (false, Some(_)) => {
            return Err(backend(
                "Kubernetes Sandbox Pod has continuation PVC incarnation evidence without a bound claim",
            ));
        }
    };
    if let Some(participants) = participants::retained(pod)? {
        let normal = pod.metadata.annotations.as_ref().is_some_and(|values| {
            values
                .get(SANDBOX_ATTEMPT_ANNOTATION)
                .is_some_and(|value| !value.trim().is_empty())
                && values.contains_key(SANDBOX_REALIZATION_ANNOTATION)
                && values.contains_key(SANDBOX_ADOPTION_ANNOTATION)
        });
        let restored = realization::restoration_evidence(pod)?.is_some()
            && realization::restoration_plan_fingerprint(pod).is_some();
        if !normal && !restored {
            return Err(backend(
                "original projected-object handoff lacks its creation or restore identity",
            ));
        }
        return Ok(Some(
            ContainerContinuationHandle::KubernetesContinuationV3 {
                pod_uid,
                claim_uid,
                participants: Box::new(participants),
            },
        ));
    }
    Ok(Some(
        ContainerContinuationHandle::KubernetesContinuationV2 { pod_uid, claim_uid },
    ))
}

impl K8sRuntime {
    pub(super) fn desired_create_claim(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
    ) -> Result<Option<PersistentVolumeClaim>, RuntimeError> {
        if !matches!(context.intent, ContainerRealizationIntent::Create)
            || !claim_required(plan, self.continuation_volume.is_some())
        {
            return Ok(None);
        }
        let effect_fence = context.effect_fence.ok_or_else(|| {
            backend("Kubernetes retained continuation creation requires a durable effect fence")
        })?;
        self.desired_create_claim_for_effect(context.scope, plan, effect_fence)
    }

    pub(super) fn desired_create_claim_for_effect(
        &self,
        scope: &str,
        plan: &ContainerPlan,
        effect_fence: &SandboxEffectFence,
    ) -> Result<Option<PersistentVolumeClaim>, RuntimeError> {
        if !claim_required(plan, self.continuation_volume.is_some()) {
            return Ok(None);
        }
        self.desired_claim_for_effect(scope, effect_fence).map(Some)
    }

    /// One immutable PVC projection for first Create, 409 replay, joined
    /// Pod/PVC reconstruction, and guarded claim-only terminal disposal.
    pub(super) fn desired_claim_for_effect(
        &self,
        scope: &str,
        effect_fence: &SandboxEffectFence,
    ) -> Result<PersistentVolumeClaim, RuntimeError> {
        let runtime_id = self.realization_runtime_id(scope)?;
        let config = self
            .continuation_volume
            .as_ref()
            .ok_or_else(|| backend("Kubernetes retained claim has no frozen storage policy"))?;
        let mut claim = build_claim(&runtime_id, config)?;
        // The ordinary digest owns the immutable PVC shape. The aggregate fence
        // is stamped afterwards so a lease-expiry renewal does not create a
        // second filesystem identity; admission below still compares the exact
        // stable effect identity and monotonic expiry.
        stamp_realization(&mut claim)?;
        stamp_effect_fence(&mut claim, effect_fence);
        Ok(claim)
    }
}

pub(super) fn build_claim(
    id: &str,
    config: &awaken_container_contract::K8sContinuationVolume,
) -> Result<PersistentVolumeClaim, RuntimeError> {
    config
        .validate()
        .map_err(|error| RuntimeError::Backend(error.to_string()))?;
    let size = config.size.trim();
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
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<Option<PendingClaimDeletion>, RuntimeError> {
    admit()?;
    let name = continuation_claim_for_pod(pod_name)?;
    let observed = claims.get(&name).await;
    admit()?;
    let observed = match observed {
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
    let deleted = claims.delete(&name, &params).await;
    admit()?;
    match deleted {
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
        AbsentPodClaimAdmission::UnsafeLiveClaim => Err(backend(ORPHANED_LIVE_CLAIM_ERROR)),
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
    admit: &(dyn Fn() -> Result<(), RuntimeError> + Sync),
) -> Result<(), RuntimeError> {
    super::lifecycle::await_exact_deletion(
        claims,
        &pending.name,
        &pending.uid,
        DELETE_TIMEOUT,
        "continuation PVC",
        admit,
    )
    .await
}

#[cfg(test)]
mod tests {
    use awaken_provisioning_contract as pc;

    use super::*;
    use awaken_sandbox_container::{ContainerPlan, NetworkMode, RootfsPlan};

    #[test]
    fn cleanup_gate_preserves_preparation_across_successor_failover() {
        /* Cleanup provenance/failover table KPG3. Causes: C1 the exact Pod has
         * neither/both/inconsistently one cleanup finalizer and its canonical
         * gate annotation; C2 the annotation carries the exact/foreign original
         * preparation A and fingerprint; C3 its latest successor is B/C; C4 the
         * requested aggregate-authorized successor is the exact replay B, a
         * same-lease renewal, higher-epoch C, late stale B after C, or foreign.
         * C5 the authority kind is aggregate disposal or exact unpublished
         * restoration. Effects: E1 only neither evidence acquires the authority by the same
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
         * | KPG3g | exact restore evidence | exact replay | E2 resume |
         * | KPG3h | either authority | other kind/evidence | E4 reject |
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
        let gate_b = PodCleanupAuthorization::Aggregate(authorization_b.clone());
        let gate_b_renewed = PodCleanupAuthorization::Aggregate(authorization_b_renewed.clone());
        let gate_c = PodCleanupAuthorization::Aggregate(authorization_c.clone());

        assert_eq!(
            pod_cleanup_authorization_admission(None, &gate_b).unwrap(),
            Acquire,
            "KPG3a/E1"
        );

        let mut pod = Pod::default();
        pod.metadata.finalizers = Some(vec![CONTINUATION_CLEANUP_FINALIZER.to_owned()]);
        stamp_pod_cleanup_authorization(&mut pod, &gate_b).unwrap();
        let replayed = observed_pod_cleanup_authorization(&pod).unwrap().unwrap();
        assert_eq!(replayed, gate_b, "KPG3b codec/E2");
        assert_eq!(
            pod_cleanup_authorization_admission(Some(&replayed), &gate_b).unwrap(),
            Resume,
            "KPG3b/E2"
        );
        assert_eq!(
            pod_cleanup_authorization_admission(Some(&replayed), &gate_b_renewed).unwrap(),
            AdvanceSuccessor,
            "KPG3c same-lease renewal/E3"
        );
        assert_eq!(
            pod_cleanup_authorization_admission(Some(&replayed), &gate_c).unwrap(),
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
            pod_cleanup_authorization_admission(Some(&gate_c), &gate_b).is_err(),
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
        let foreign_gate = PodCleanupAuthorization::Aggregate(foreign);
        assert!(
            pod_cleanup_authorization_admission(Some(&foreign_gate), &gate_c).is_err(),
            "KPG3e/E4"
        );
        let restoration = pc::SandboxRestorationEvidence::from_exact_parts(
            "restore-effect",
            "restore-generation",
            "checkpoint",
            "checkpoint-digest",
            "spec-fingerprint",
            "exclusions-fingerprint",
        )
        .unwrap();
        let restore_gate = PodCleanupAuthorization::Restoration(restoration.clone());
        assert_eq!(
            pod_cleanup_authorization_admission(None, &restore_gate).unwrap(),
            Acquire,
            "KPG3g acquire/E1"
        );
        let mut restore_pod = Pod::default();
        restore_pod.metadata.finalizers = Some(vec![CONTINUATION_CLEANUP_FINALIZER.to_owned()]);
        stamp_pod_cleanup_authorization(&mut restore_pod, &restore_gate).unwrap();
        let restore_replay = observed_pod_cleanup_authorization(&restore_pod)
            .unwrap()
            .unwrap();
        assert_eq!(restore_replay, restore_gate, "KPG3g codec/E2");
        assert_eq!(
            pod_cleanup_authorization_admission(Some(&restore_replay), &restore_gate).unwrap(),
            Resume,
            "KPG3g replay/E2"
        );
        assert!(
            pod_cleanup_authorization_admission(Some(&restore_replay), &gate_b).is_err()
                && pod_cleanup_authorization_admission(Some(&gate_b), &restore_gate).is_err(),
            "KPG3h cross-authority/E4"
        );
        let different_restore = PodCleanupAuthorization::Restoration(
            pc::SandboxRestorationEvidence::from_exact_parts(
                "other-effect",
                "restore-generation",
                "checkpoint",
                "checkpoint-digest",
                "spec-fingerprint",
                "exclusions-fingerprint",
            )
            .unwrap(),
        );
        assert!(
            pod_cleanup_authorization_admission(Some(&restore_replay), &different_restore).is_err(),
            "KPG3h foreign restoration/E4"
        );
        let finalizer_only = Pod {
            metadata: ObjectMeta {
                finalizers: Some(vec![CONTINUATION_CLEANUP_FINALIZER.to_owned()]),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut annotation_only = Pod::default();
        stamp_pod_cleanup_authorization(&mut annotation_only, &gate_b).unwrap();
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
         * persisted aggregate effect is exact, missing, foreign generation,
         * or newer than the caller's asserted expiry. Effects: E1 an absent
         * claim may be created; E2 only an exact same-effect claim may be
         * reused after response loss; E3 every incomplete, foreign,
         * stale-caller or terminating row rejects without mutation. Rules:
         * CC1 !C1=>E1; CC2 C1+UID+live+exact-or-renewed(C4)=>E2;
         * CC3 every other C1 row=>E3. The caller separately runs the sole
         * typed full PVC projection verifier before accepting E2; the runtime
         * test owns the changed-spec decision row. Rebuild is excluded: it
         * needs the source V2 handle's claim UID. */
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
            .with_continuation_volume(awaken_container_contract::K8sContinuationVolume {
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
            .with_continuation_volume(awaken_container_contract::K8sContinuationVolume {
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
         * kubelet-created subpaths mounted at every C2 root, without a helper
         * init container; E4 fail before an API write.
         * Rules: V1 !C1=>E1; V2 C1+C2+C3+valid(C4)=>E2+E3;
         * V3 C1+blank(C4)=>E4. FMECA: a Pod-owned claim makes Pod recovery lose
         * live Session data (S5/O2/D3=30), while reusing one subpath for several
         * roots aliases unrelated data (S4/O2/D3=24). This module is the single
         * owner of both claim and mount identity.
         */
        let config = awaken_container_contract::K8sContinuationVolume {
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
        assert!(spec.init_containers.is_none(), "V2/E3 kubelet owns mkdir");
        assert_eq!(
            spec.security_context.unwrap().fs_group,
            Some(10001),
            "V2/E3"
        );
        assert!(
            build_claim(
                "session-1",
                &awaken_container_contract::K8sContinuationVolume {
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

#[cfg(any(test, kani))]
fn check_original_guard_admission_domain(bits: u8) {
    let uid = bits & 1 != 0;
    let exact = bits & 2 != 0;
    let rv = bits & 4 != 0;
    let deleting = bits & 8 != 0;
    let owned = bits & 16 != 0;
    let original = bits & 32 != 0;
    let result = pod_cleanup_gate_admission(uid, exact, rv, deleting, owned, original);
    let valid = uid && exact && rv;
    use PodCleanupGateAdmission::{Acquire, AdoptOriginal, DeletingBeforeGate, Resume};
    assert!(!matches!(result, Acquire | AdoptOriginal | Resume) || valid);
    assert_eq!(
        matches!(result, AdoptOriginal),
        valid && deleting && !owned && original
    );
    assert_eq!(matches!(result, Acquire), valid && !deleting && !owned);
    assert_eq!(matches!(result, Resume), valid && owned);
    if valid && deleting && !owned && !original {
        assert_eq!(result, DeletingBeforeGate);
    }
}

#[test]
fn original_guard_gate_admission_exhausts_the_exact_authority_domain() {
    // Cause/effect rules: exact UID + RV are mandatory for every admitting
    // outcome. An already-owned gate resumes; an unprotected live source may
    // acquire; a deleting source may only adopt its independently eligible
    // original guard. Exhaust all 64 raw combinations, including contradictory
    // UID facts. No sampled absence or raw finalizer alone authorizes deletion.
    for bits in 0u8..64 {
        check_original_guard_admission_domain(bits);
    }
}

#[cfg(kani)]
/// @obligation resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
/// @kani profile=solver-kissat-v1
#[kani::proof]
#[kani::unwind(65)]
fn original_guard_gate_admission_preserves_exact_uid_and_retention() {
    // The complete finite production-kernel domain, not unconstrained physical
    // observations. HTTP tests qualify eligibility mapping and API CAS effects;
    // this harness does not prove Kubernetes linearizability or source bytes.
    for bits in 0u8..64 {
        check_original_guard_admission_domain(bits);
    }
}

#[test]
fn original_guard_retention_is_qualified_by_observation_purpose() {
    use awaken_provisioning_contract::SandboxObservationPurpose as Purpose;
    // Cause/effect decision table: C1 original guard; C2 no isolation label;
    // C3 Pod phase; C4 observation purpose. R1 C1+C2+Running grants only an
    // unprepared Worker-retirement source read. R2 C1+C2+terminal preserves
    // eligibility for every purpose. R3 Pending/Unknown/missing phase and R4
    // !C1 or !C2 deny every purpose, including a false-valued isolation label.
    // This is a prerequisite, never execution, disposal or full retirement
    // authority. The API table separately qualifies identities and no writes.
    let purposes = [
        Purpose::Recovery,
        Purpose::TerminalCleanup,
        Purpose::WorkerRetirement {
            source_prepared: false,
        },
        Purpose::WorkerRetirement {
            source_prepared: true,
        },
    ];
    for guard in [false, true] {
        for (phase, expected) in [
            (Some("Running"), [false, false, true, false]),
            (Some("Failed"), [true; 4]),
            (Some("Succeeded"), [true; 4]),
            (Some("Pending"), [false; 4]),
            (Some("Unknown"), [false; 4]),
            (None, [false; 4]),
        ] {
            for label in [None, Some("true"), Some("false")] {
                let mut pod = Pod::default();
                pod.metadata.finalizers =
                    guard.then(|| vec![awaken_container_contract::ORIGINAL_GUARD_FINALIZER.into()]);
                pod.status = phase.map(|phase| k8s_openapi::api::core::v1::PodStatus {
                    phase: Some(phase.into()),
                    ..Default::default()
                });
                if let Some(value) = label {
                    pod.metadata.labels = Some(
                        [(
                            awaken_container_contract::ISOLATION_LABEL.into(),
                            value.into(),
                        )]
                        .into_iter()
                        .collect(),
                    );
                }
                for (purpose, phase_allowed) in purposes.into_iter().zip(expected) {
                    assert_eq!(
                        original_guard_retained(&pod, purpose),
                        guard && label.is_none() && phase_allowed,
                        "{phase:?} {purpose:?} guard={guard} isolation={label:?}"
                    );
                }
            }
        }
    }
}
