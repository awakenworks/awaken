//! Kubernetes realization of one retained active-filesystem volume.

use awaken_provisioning_contract::ContainerContinuationHandle;
use k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaim, PersistentVolumeClaimSpec, VolumeMount,
    VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::{DeleteParams, Preconditions};

use super::error::api_not_found;
use super::names::continuation_claim_name;
use super::pod_projection::CONTINUATION_VOLUME;
use super::{ContainerPlan, RuntimeError, backend, hardened_security_context};

const DELETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DELETE_POLL: std::time::Duration = std::time::Duration::from_millis(100);
pub(super) const CLAIM_UID_ANNOTATION: &str = "awaken.dev/continuation-claim-uid";

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

pub(super) fn handle_extra(
    pod: &k8s_openapi::api::core::v1::Pod,
) -> Result<Option<ContainerContinuationHandle>, RuntimeError> {
    let binds_continuation_claim = pod
        .spec
        .as_ref()
        .and_then(|spec| spec.volumes.as_ref())
        .is_some_and(|volumes| {
            volumes.iter().any(|volume| {
                volume.name == CONTINUATION_VOLUME && volume.persistent_volume_claim.is_some()
            })
        });
    match (binds_continuation_claim, bound_claim_uid(pod)) {
        (false, None) => Ok(None),
        (true, Some(uid)) => Ok(Some(ContainerContinuationHandle::KubernetesContinuation {
            claim_uid: uid.to_owned(),
        })),
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

pub(super) async fn delete_claim(
    claims: &Api<PersistentVolumeClaim>,
    pod_name: &str,
    expected_uid: &str,
) -> Result<(), RuntimeError> {
    let runtime_id = pod_name
        .strip_prefix("awaken-")
        .ok_or_else(|| backend("invalid managed Kubernetes Pod identity"))?;
    let name = continuation_claim_name(runtime_id);
    let observed = match claims.get(&name).await {
        Ok(claim) => claim,
        Err(error) if api_not_found(&error) => return Ok(()),
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
    let params = DeleteParams::default().preconditions(Preconditions {
        uid: Some(uid),
        resource_version: Some(resource_version),
    });
    match claims.delete(&name, &params).await {
        Ok(_) => {}
        Err(error) if api_not_found(&error) => return Ok(()),
        Err(error) => return Err(backend(error)),
    }
    let deadline = tokio::time::Instant::now() + DELETE_TIMEOUT;
    loop {
        match claims.get(&name).await {
            Err(error) if api_not_found(&error) => return Ok(()),
            Err(error) => return Err(backend(error)),
            Ok(_) if tokio::time::Instant::now() >= deadline => {
                return Err(backend(format!(
                    "continuation PVC `{name}` was not deleted within {}s",
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
         * carries the exact PVC-incarnation UID annotation. Effects: E1 return
         * no durable handle for a PVC-free Pod; E2 persist the UID fence for a
         * retained Pod; E3 fail closed on a missing or orphaned UID. Decision
         * rules: H1 !C1+!C2+!C3=>E1 (configuration disabled); H2
         * C1+!C2+!C3=>E1 (ephemeral capability probe); H3 C1+C2+C3=>E2
         * (retained Session); H4 C1+C2+!C3=>E3; H5 any(C1)+!C2+C3=>E3.
         * C2 is the realized-object authority, so this does not duplicate the
         * typed claim selector used by allocation and Pod projection.
         * Constraint/invariant: durable handle evidence exists only when the
         * realized Pod binds the canonical PVC and carries its exact UID fence;
         * configured capability alone can neither fabricate nor retain a handle.
         */
        let plain = super::super::K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        let retained = plain.pod("configured-off", &plan());
        assert_eq!(handle_extra(&retained).unwrap(), None, "H1");

        let configured = super::super::K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap())
            .with_continuation_volume(crate::K8sContinuationVolume {
                storage_class_name: Some("retained-rwo".into()),
                size: "8Gi".into(),
            });
        let mut ephemeral_plan = plan();
        ephemeral_plan.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
        let ephemeral = configured.pod("probe", &ephemeral_plan);
        assert_eq!(handle_extra(&ephemeral).unwrap(), None, "H2");

        let mut retained = configured.pod("session", &plan());
        assert!(handle_extra(&retained).is_err(), "H4");
        bind_claim_uid(&mut retained, "claim-incarnation-1");
        assert_eq!(
            handle_extra(&retained).unwrap(),
            Some(ContainerContinuationHandle::KubernetesContinuation {
                claim_uid: "claim-incarnation-1".into(),
            }),
            "H3"
        );

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
        );
        bind_claim_uid(&mut fenced, "claim-incarnation-1");
        assert_eq!(
            bound_claim_uid(&fenced),
            Some("claim-incarnation-1"),
            "V2/C3"
        );
    }
}
