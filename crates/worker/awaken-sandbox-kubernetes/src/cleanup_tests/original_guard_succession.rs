// @formal surface obligation=resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
// @formal reason: Actual kube HTTP effects qualify the bounded retained-original handoff and replay;
// @formal reason: a mocked Root disposal authorization is not a Session completion or cluster receipt.
//! Original guard succession reuses the existing API fixture and disposal owner.
use super::*;

#[tokio::test]
async fn original_guard_handoff_refuses_unproved_source_or_restore_authority() {
    // C1 existing original guard; C2 terminal, unisolated source; C3 exact
    // Pod UID/RV; C4 aggregate rather than restore authorization. R1 any
    // missing/conflicting cause -> Err with unchanged Pod/PVC and zero writes.
    // These API rows give the kernel's original-retention input its physical
    // and authority meaning; no new finalizer repairs an unsupported row.
    for row in [
        "missing-guard",
        "running",
        "isolated",
        "foreign-uid",
        "missing-rv",
        "restore",
    ] {
        let (runtime, state, _, _, server) = fixture(0, false).await;
        retain_terminal_original(&state, true);
        {
            let mut snapshot = state.lock().unwrap();
            let pod = snapshot.pod.as_mut().unwrap();
            match row {
                "missing-guard" => pod.metadata.finalizers = None,
                "running" => {
                    pod.status = Some(serde_json::from_value(json!({"phase":"Running"})).unwrap())
                }
                "isolated" => {
                    pod.metadata.labels = Some(
                        [(
                            awaken_container_contract::ISOLATION_LABEL.into(),
                            "true".into(),
                        )]
                        .into_iter()
                        .collect(),
                    )
                }
                "foreign-uid" => pod.metadata.uid = Some("foreign-pod".into()),
                "missing-rv" => pod.metadata.resource_version = None,
                _ => {}
            }
        }
        let authorization = awaken_sandbox_container::test_support::disposal_authorization(
            &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        let authority = EffectAuthority::new(authorization.effect_fence());
        let admit = || {
            pc::with_current_sandbox_effect(authorization.effect_fence(), &authority, |_| Ok(()))
                .map_err(backend)
        };
        let requested = if row == "restore" {
            continuation::PodCleanupAuthorization::Restoration(
                pc::SandboxRestorationEvidence::from_exact_parts(
                    "restore",
                    "generation",
                    "checkpoint",
                    "digest",
                    "spec",
                    "exclusions",
                )
                .unwrap(),
            )
        } else {
            continuation::PodCleanupAuthorization::Aggregate(authorization.clone())
        };
        let before = {
            let snapshot = state.lock().unwrap();
            (json!(snapshot.pod), json!(snapshot.claim))
        };
        let pod = state.lock().unwrap().pod.clone().unwrap();
        assert!(
            continuation::acquire_pod_cleanup_gate_with_authorization(
                &runtime.pods(),
                &pod,
                "pod-uid",
                &requested,
                &admit
            )
            .await
            .is_err(),
            "{row}"
        );
        {
            let snapshot = state.lock().unwrap();
            assert!(snapshot.writes.is_empty(), "{row}");
            assert_eq!(json!(snapshot.pod), before.0);
            assert_eq!(json!(snapshot.claim), before.1);
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn original_guard_response_loss_replays_the_same_original_participants() {
    // Causes: C1 exact failed/deleting original + guard + PVC + full UID set;
    // C2 current separately prepared disposal authority; C3 response is lost
    // after accepted guard CAS / PVC DELETE / Pod DELETE / guard release.
    // R1=C1+C2+C3 -> no forged caller success; preserve the same handle and
    // preparation, then resume exact closure. E1 no new finalizer or Create;
    // E2 all original UIDs absent after replay. This crosses every irreversible
    // handoff edge through the real adapter, not a copied cleanup algorithm.
    for (call, nth) in [
        ("PUT pods", 1),
        ("DELETE persistentvolumeclaims", 1),
        ("DELETE pods", 1),
        ("PUT pods", 2),
    ] {
        let (runtime, state, reached, release, server) = fixture(0, false).await;
        retain_terminal_original(&state, true);
        let handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let authorization = awaken_sandbox_container::test_support::disposal_authorization(
            &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
        );
        let authority = Arc::new(EffectAuthority::new(authorization.effect_fence()));
        state.lock().unwrap().cut_after_write = Some((call.into(), nth));
        let runtime = Arc::new(runtime);
        let mut task = {
            let (runtime, handle, authorization, authority) = (
                runtime.clone(),
                handle.clone(),
                authorization.clone(),
                authority.clone(),
            );
            tokio::spawn(async move {
                runtime
                    .remove_exact_incarnation(
                        "awaken-cleanup",
                        Some(&handle),
                        &authorization,
                        authority.as_ref(),
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        task.abort();
        release.add_permits(1);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
        assert!(
            !state
                .lock()
                .unwrap()
                .writes
                .iter()
                .any(|write| write.starts_with("POST")),
            "E1 {call}/{nth}"
        );
        runtime
            .remove_exact_incarnation(
                "awaken-cleanup",
                Some(&handle),
                &authorization,
                authority.as_ref(),
            )
            .await
            .unwrap();
        {
            let snapshot = state.lock().unwrap();
            assert!(
                snapshot.pod.is_none() && snapshot.claim.is_none() && snapshot.projected.is_empty(),
                "E2 {call}/{nth}"
            );
        }
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

#[tokio::test]
async fn original_guard_revocation_after_cas_keeps_the_original_claim() {
    // C1 accepted annotation-only CAS; C2 authority revoked before its reply.
    // R1=C1+C2 -> retain the original guard/claim and no PVC DELETE or caller
    // success. R2 the same preparation regains current authority -> the one
    // UID disposal resumes; the original CAS is not replaced by another owner.
    let (runtime, state, reached, release, server) = fixture(0, false).await;
    retain_terminal_original(&state, true);
    let handle = continuation::handle_extra(state.lock().unwrap().pod.as_ref().unwrap())
        .unwrap()
        .unwrap();
    let authorization = awaken_sandbox_container::test_support::disposal_authorization(
        &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
    );
    let authority = Arc::new(EffectAuthority::new(authorization.effect_fence()));
    state.lock().unwrap().cut_after_write = Some(("PUT pods".into(), 1));
    let runtime = Arc::new(runtime);
    let task = {
        let (runtime, handle, authorization, authority) = (
            runtime.clone(),
            handle.clone(),
            authorization.clone(),
            authority.clone(),
        );
        tokio::spawn(async move {
            runtime
                .remove_exact_incarnation(
                    "awaken-cleanup",
                    Some(&handle),
                    &authorization,
                    authority.as_ref(),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .unwrap();
    *authority.current.lock().unwrap() = None;
    release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    {
        let snapshot = state.lock().unwrap();
        assert_eq!(snapshot.writes, vec!["PUT pods"]);
        assert!(
            snapshot
                .claim
                .as_ref()
                .unwrap()
                .metadata
                .deletion_timestamp
                .is_none()
        );
        assert!(
            continuation::observed_pod_cleanup_authorization(snapshot.pod.as_ref().unwrap())
                .unwrap()
                .is_some()
        );
    }
    *authority.current.lock().unwrap() =
        Some((authorization.effect_fence().clone(), Duration::from_secs(1)));
    runtime
        .remove_exact_incarnation(
            "awaken-cleanup",
            Some(&handle),
            &authorization,
            authority.as_ref(),
        )
        .await
        .unwrap();
    assert!(state.lock().unwrap().pod.is_none());
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn original_guard_succession_is_preparation_bound_and_never_regresses() {
    // C1 original guard acquired with preparation P; C2 retry is a new
    // preparation, a same-P successor, or the stale predecessor. R1 new P ->
    // Err/no writes; R2 same P/new epoch -> CAS the same guard, no PVC effects;
    // R3 predecessor -> Err/no regression. Separate current authority is
    // required at every edge; a present finalizer alone is no authorization.
    let (runtime, state, _, _, server) = fixture(0, false).await;
    retain_terminal_original(&state, true);
    let authorization = awaken_sandbox_container::test_support::disposal_authorization(
        &pc::SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX - 1).unwrap(),
    );
    let authority = EffectAuthority::new(authorization.effect_fence());
    let admit = || {
        pc::with_current_sandbox_effect(authorization.effect_fence(), &authority, |_| Ok(()))
            .map_err(backend)
    };
    let pod = state.lock().unwrap().pod.clone().unwrap();
    continuation::acquire_pod_cleanup_gate_with_authorization(
        &runtime.pods(),
        &pod,
        "pod-uid",
        &continuation::PodCleanupAuthorization::Aggregate(authorization.clone()),
        &admit,
    )
    .await
    .unwrap();
    let writes = state.lock().unwrap().writes.len();
    let foreign = awaken_sandbox_container::test_support::disposal_authorization(
        &pc::SandboxEffectFence::new("different-preparation", "owner", "boot", 1, u64::MAX - 1)
            .unwrap(),
    );
    let foreign_authority = EffectAuthority::new(foreign.effect_fence());
    let foreign_admit = || {
        pc::with_current_sandbox_effect(foreign.effect_fence(), &foreign_authority, |_| Ok(()))
            .map_err(backend)
    };
    let pod = state.lock().unwrap().pod.clone().unwrap();
    assert!(
        continuation::acquire_pod_cleanup_gate_with_authorization(
            &runtime.pods(),
            &pod,
            "pod-uid",
            &continuation::PodCleanupAuthorization::Aggregate(foreign.clone()),
            &foreign_admit
        )
        .await
        .is_err()
    );
    assert_eq!(state.lock().unwrap().writes.len(), writes);
    let next = pc::SandboxDisposalAuthorization::new(
        authorization.prepared_effect_fence().clone(),
        pc::SandboxEffectFence::new(
            authorization.effect_fence().operation_id(),
            "successor",
            "next-boot",
            2,
            u64::MAX - 1,
        )
        .unwrap(),
        authorization.preparation_fingerprint(),
    )
    .unwrap();
    let next_authority = EffectAuthority::new(next.effect_fence());
    let next_admit = || {
        pc::with_current_sandbox_effect(next.effect_fence(), &next_authority, |_| Ok(()))
            .map_err(backend)
    };
    continuation::acquire_pod_cleanup_gate_with_authorization(
        &runtime.pods(),
        &pod,
        "pod-uid",
        &continuation::PodCleanupAuthorization::Aggregate(next.clone()),
        &next_admit,
    )
    .await
    .unwrap();
    let pod = state.lock().unwrap().pod.clone().unwrap();
    let writes = state.lock().unwrap().writes.len();
    assert!(
        continuation::acquire_pod_cleanup_gate_with_authorization(
            &runtime.pods(),
            &pod,
            "pod-uid",
            &continuation::PodCleanupAuthorization::Aggregate(authorization.clone()),
            &admit
        )
        .await
        .is_err()
    );
    assert_eq!(state.lock().unwrap().writes.len(), writes);
    assert!(
        state
            .lock()
            .unwrap()
            .claim
            .as_ref()
            .unwrap()
            .metadata
            .deletion_timestamp
            .is_none()
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}
