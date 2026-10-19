use super::*;
use crate::K8sRuntime;
use crate::guarded_claim::{
    GuardedClaimPhysical, guarded_claim_physical_state, remove_guarded_claim,
};
use awaken_container_contract::K8sContinuationVolume;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxEffectFence;
use awaken_sandbox_container::KubernetesGuardedClaimHandle;

#[tokio::test]
async fn guarded_claim_disposal_requires_live_policy_exact_uid_and_current_authority() {
    // Physical terminal table. Causes: C1 the sealed ticket's inert Pod has
    // an exact UID/spec and active VAP denial; C2 original PVC UID, effect and
    // complete storage projection match; C3 current terminal authorization;
    // C4 PVC DELETE is accepted but its response is lost; C5 policy/UID/spec
    // drifts. Effects: E1 C1+C2+C3 deletes only the original PVC and leaves
    // the guard; E2 !C1|!C2|!C3 has zero DELETEs; E3 C4 returns uncertainty,
    // then exact absence replays without a second DELETE; E4 C5 blocks that
    // replay. Rules P1=C1+C2+C3->E1, P2=any missing->E2,
    // P3=P1+C4->E3, P4=post-delete C5->E4. The live API test owns defaulting.
    let predecessor =
        SandboxEffectFence::new("old-effect", "old-worker", "old-boot", 1, u64::MAX).unwrap();
    let terminal =
        SandboxEffectFence::new("terminal-effect", "old-worker", "old-boot", 2, u64::MAX).unwrap();
    let state = Arc::new(Mutex::new(Fixture {
        policy: true,
        delete_denied: true,
        ..Default::default()
    }));
    let api = client(state.clone());
    let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
    runtime.namespace = "sandbox-ns".into();
    runtime.clients.control = api.clone();
    runtime.continuation_volume = Some(K8sContinuationVolume {
        storage_class_name: None,
        size: "1Gi".into(),
    });
    let guard_name = crate::names::pod_name(&runtime.realization_runtime_id("scope").unwrap());
    let mut guard = ticket();
    guard.request = CreateTicketRequest::new(
        "group",
        "pods",
        "sandbox-ns",
        guard_name.as_str(),
        "a".repeat(64),
    )
    .unwrap();
    assert!(matches!(
        isolate_pod(&api, &guard).await.unwrap(),
        Some(TicketResult::Isolated { uid }) if uid == "sentinel-uid"
    ));
    guard.status = TicketStatus::Finished(TicketResult::Isolated {
        uid: "sentinel-uid".into(),
    });
    guard.isolation_pending = false;
    guard.origin = awaken_sandbox_container::TicketOrigin::TerminalGuard;
    let guarded =
        KubernetesGuardedClaimHandle::new("scope", &predecessor, guard, "claim-uid").unwrap();
    let mut claim = runtime
        .desired_claim_for_effect("scope", &predecessor)
        .unwrap();
    claim.metadata.namespace = Some("sandbox-ns".into());
    claim.metadata.uid = Some("claim-uid".into());
    claim.metadata.resource_version = Some("1".into());
    state.lock().unwrap().claim = Some(claim.clone());
    assert!(
        verify_reservation(&runtime.clients.control, guarded.guard(), "sentinel-uid")
            .await
            .unwrap(),
        "P1 guard policy and immutable projection"
    );
    assert!(
        crate::continuation::original_claim_effect_matches(
            &predecessor,
            &crate::pod_effect_fence(claim.metadata.annotations.as_ref())
                .unwrap()
                .unwrap(),
        ),
        "P1 predecessor effect"
    );
    assert!(
        crate::projection_verification::verify_guarded_claim_projection(
            &runtime
                .desired_claim_for_effect("scope", &predecessor)
                .unwrap(),
            &claim,
        )
        .is_ok(),
        "P1 full PVC projection"
    );
    assert_eq!(
        guarded_claim_physical_state(&runtime, guard_name.as_str(), &guarded)
            .await
            .unwrap(),
        GuardedClaimPhysical::Live,
        "P1"
    );

    state.lock().unwrap().policy_tampered = true;
    assert_eq!(
        guarded_claim_physical_state(&runtime, guard_name.as_str(), &guarded)
            .await
            .unwrap(),
        GuardedClaimPhysical::Incompatible,
        "P2 policy drift"
    );
    state.lock().unwrap().policy_tampered = false;
    state.lock().unwrap().claim.as_mut().unwrap().metadata.uid = Some("foreign".into());
    assert_eq!(
        guarded_claim_physical_state(&runtime, guard_name.as_str(), &guarded)
            .await
            .unwrap(),
        GuardedClaimPhysical::Incompatible,
        "P2 foreign PVC UID"
    );
    state.lock().unwrap().claim = Some(claim.clone());
    state
        .lock()
        .unwrap()
        .claim
        .as_mut()
        .unwrap()
        .spec
        .as_mut()
        .unwrap()
        .resources
        .as_mut()
        .unwrap()
        .requests
        .as_mut()
        .unwrap()
        .insert(
            "storage".into(),
            k8s_openapi::apimachinery::pkg::api::resource::Quantity("2Gi".into()),
        );
    assert_eq!(
        guarded_claim_physical_state(&runtime, guard_name.as_str(), &guarded)
            .await
            .unwrap(),
        GuardedClaimPhysical::Incompatible,
        "P2 copied digest cannot hide changed storage"
    );
    state.lock().unwrap().claim = Some(claim.clone());
    let authorization = awaken_sandbox_container::test_support::disposal_authorization(&terminal);
    assert!(
        remove_guarded_claim(
            &runtime,
            guard_name.as_str(),
            &guarded,
            &authorization,
            &pc::test_support::EffectAuthority::denied(),
        )
        .await
        .is_err(),
        "P2 revoked authority"
    );
    assert_eq!(state.lock().unwrap().claim_deletes, 0, "P2 zero DELETEs");

    state.lock().unwrap().claim_delete_response_lost = true;
    let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
    assert!(
        remove_guarded_claim(
            &runtime,
            guard_name.as_str(),
            &guarded,
            &authorization,
            &authority,
        )
        .await
        .is_err(),
        "P3 lost DELETE response cannot issue a receipt"
    );
    assert_eq!(
        state.lock().unwrap().claim_deletes,
        1,
        "P3 one accepted DELETE"
    );
    assert!(
        state.lock().unwrap().pod.is_some(),
        "P1 inert guard retained"
    );
    state.lock().unwrap().policy_tampered = true;
    assert_eq!(
        guarded_claim_physical_state(&runtime, guard_name.as_str(), &guarded)
            .await
            .unwrap(),
        GuardedClaimPhysical::Incompatible,
        "P4 policy drift blocks absent-claim replay"
    );
    state.lock().unwrap().policy_tampered = false;
    remove_guarded_claim(
        &runtime,
        guard_name.as_str(),
        &guarded,
        &authorization,
        &authority,
    )
    .await
    .unwrap();
    assert_eq!(
        state.lock().unwrap().claim_deletes,
        1,
        "P3 no second DELETE"
    );
    assert_eq!(
        guarded_claim_physical_state(&runtime, guard_name.as_str(), &guarded)
            .await
            .unwrap(),
        GuardedClaimPhysical::Absent,
        "P3 exact replay"
    );
}
