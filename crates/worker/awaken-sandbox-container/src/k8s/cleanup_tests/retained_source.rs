// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: Exact Kube HTTP denial witnesses retain original Pod/PVC and projected-object
// @formal reason: evidence. They do not qualify source-read readiness or successful physical cleanup.
//! Historic retained-source observations must preserve their physical evidence.

use super::*;

#[tokio::test]
async fn historic_failed_source_denials_keep_pod_claim_and_projection_evidence() {
    // Causes: C1 retained original Pod/PVC UIDs; C2 the failed source has an old
    // V2 handle without an original projected-object handoff; C3 the failed Pod
    // is already deleting while its exact PVC is still live; C4 current effect
    // authority is present. C2 and C3 are separate historic input partitions.
    // Effects: E1 report the precise unresolved provider fact; E2 preserve Pod,
    // PVC and projected participants; E3 no mutation or guessed LIST membership.
    // R1=C1+C2+C4 -> unknown membership + E2/E3. R2=C1+C3+C4 -> indeterminate
    // deletion + E2/E3. These are denial witnesses, not source-read readiness or
    // successful cleanup. A future source-preparation proof must add its own
    // positive path without turning either missing fact into a disposal receipt.
    for (legacy, deleting, expected) in [
        (
            true,
            false,
            "terminal Kubernetes projected-object membership is unknown",
        ),
        (
            false,
            true,
            "Kubernetes Sandbox Pod deletion is indeterminate before continuation cleanup",
        ),
    ] {
        let (runtime, state, _, _, server) = fixture(0, false).await;
        let mut handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let before = {
            let mut state = state.lock().unwrap();
            let pod = state.pod.as_mut().unwrap();
            pod.status = Some(serde_json::from_value(json!({"phase":"Failed"})).unwrap());
            if deleting {
                pod.metadata.deletion_timestamp =
                    Some(serde_json::from_value(json!("2026-10-14T06:37:35Z")).unwrap());
            }
            if legacy {
                let annotations = pod.metadata.annotations.as_mut().unwrap();
                annotations.remove(participants::DECLARATION);
                annotations.remove("awaken.dev/projected-object-results");
                handle = crate::ContainerContinuationHandle::KubernetesContinuationV2 {
                    pod_uid: "pod-uid".into(),
                    claim_uid: Some("claim-uid".into()),
                };
            }
            (
                serde_json::to_value(&state.pod).unwrap(),
                serde_json::to_value(&state.claim).unwrap(),
                state.projected.clone(),
            )
        };
        let fingerprint = serde_json::from_value(json!("frozen-spec")).unwrap();
        let fence =
            pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let error = runtime
            .observe(ContainerObservationExpectation {
                purpose: ContainerObservationPurpose::TerminalCleanup,
                container_id: "awaken-cleanup",
                adoption_fingerprint: Some(&fingerprint),
                realization_fingerprint: Some(&fingerprint),
                runtime_handle: Some(&handle),
                effect_fence: Some(&fence),
                retained_attempt: None,
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        {
            let state = state.lock().unwrap();
            assert_eq!(serde_json::to_value(&state.pod).unwrap(), before.0);
            assert_eq!(serde_json::to_value(&state.claim).unwrap(), before.1);
            assert_eq!(state.projected, before.2);
            assert!(state.writes.is_empty());
            assert_eq!(state.projected_lists, 0);
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
