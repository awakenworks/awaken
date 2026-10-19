//! One provider-owned classification for a missing durable container handle.

use super::*;
use awaken_provisioning_contract::SandboxObservationPurpose;
use awaken_provisioning_contract::SandboxProvider;

#[tokio::test]
async fn handle_free_terminal_cleanup_accepts_only_runtime_proven_total_absence() {
    /* HU1 cause/effect table. C1 no durable handle; C2 the frozen plan has no
     * bind, Memory, package, or retained-filesystem participant; C3 the runtime
     * proves the canonical realization and participant set absent. C1+C2+C3
     * => E1 cleanup closes as absent. Without C3 => E2 fail closed. Rich plans
     * remain covered by CN1 E4 and cannot enter this proof seam. */
    for (proven, has_expected_fence, succeeds) in [
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let runtime = Arc::new(FakeRuntime::default());
        runtime.st.lock().unwrap().unmaterialized_absence_proven = proven;
        let provider = provider_without_broker(runtime.clone());
        let mut requested = spec("handle-free-terminal");
        requested.mounts.clear();
        requested.env.clear();
        requested.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
        let create = pc::SandboxEffectFence::new(
            "create-handle-free",
            "worker-owner",
            "worker-incarnation",
            1,
            u64::MAX,
        )
        .unwrap();
        let terminal = pc::SandboxEffectFence::new(
            "terminal-handle-free",
            "worker-owner",
            "worker-incarnation",
            2,
            u64::MAX,
        )
        .unwrap();
        let outcome = SandboxProvider::prepare_terminal_for_effect(
            &provider,
            &requested,
            None,
            has_expected_fence.then_some(&create),
            &terminal,
            &pc::test_support::EffectAuthority::new(&terminal),
        )
        .await;
        assert_eq!(outcome.is_ok(), succeeds);
        if let Ok(environment) = outcome {
            assert!(environment.is_none(), "E1 proven absence has no owner");
        }
        assert_eq!(
            runtime.st.lock().unwrap().unmaterialized_absence_checks,
            1,
            "the runtime owns the sole physical absence observation"
        );
    }
}

#[tokio::test]
async fn exact_created_pod_reenters_the_existing_handle_bound_terminal_path() {
    // Causes: C1 no Session handle, C2 an independent provider join retained
    // one exact live Pod UID, C3 the frozen spec has no mount/control owner,
    // C4 current terminal authority is present. Effects: E1 C1-C4 reconstruct
    // one typed V2 handle, ordinary terminal observation and one physical
    // removal after preparation; E2 C2
    // with a rich spec refuses before observation; E3 absent C4 refuses before
    // joining provider evidence. Rules P1=C1+C2+C3+C4->E1,
    // P2=C1+C2+!C3+C4->E2, P3=C1+C2+C3+!C4->E3. P4=C1+C2
    // with an original PVC UID and a still-Provisioning Pod -> terminal
    // adoption through the same disposal owner; an ordinary durable handle
    // with Provisioning has no such join and remains NotReady.
    let runtime = Arc::new(FakeRuntime::default());
    {
        let mut state = runtime.st.lock().unwrap();
        state.native_memory = true; // K8s reconstructs without host-bind staging.
        state.recovered_created_pod = Some(("exact-pod".into(), "pod-uid".into(), None));
    }
    let provider = provider_without_broker(runtime.clone());
    let mut empty = spec("created-pod-terminal");
    empty.mounts.clear();
    empty.env.clear();
    empty.filesystem_continuity = pc::FilesystemContinuity::Ephemeral;
    let create = pc::SandboxEffectFence::new(
        "create-created-pod",
        "worker-owner",
        "worker-incarnation",
        1,
        u64::MAX,
    )
    .unwrap();
    let terminal = pc::SandboxEffectFence::new(
        "terminal-created-pod",
        "worker-owner",
        "worker-incarnation",
        2,
        u64::MAX,
    )
    .unwrap();
    let authority = pc::test_support::EffectAuthority::new(&terminal);
    let environment = SandboxProvider::prepare_terminal_for_effect(
        &provider,
        &empty,
        None,
        Some(&create),
        &terminal,
        &authority,
    )
    .await
    .unwrap()
    .expect("P1 exact Pod uses existing terminal owner");
    assert_eq!(
        environment
            .handle()
            .container_physical_incarnation()
            .unwrap(),
        "pod-uid",
        "P1 only the ticket's immutable Pod incarnation is adopted"
    );
    assert_eq!(
        runtime.st.lock().unwrap().observation_purposes,
        vec![SandboxObservationPurpose::TerminalCleanup],
        "P1 ordinary terminal observation is reused"
    );
    let prepared = environment
        .prepare_disposal_for_effect(&terminal, &authority)
        .await
        .expect("P1 original cleanup preparation");
    let removal = disposal_authorization(&prepared);
    environment
        .dispose_for_effect(
            &removal,
            &pc::test_support::EffectAuthority::new(removal.effect_fence()),
        )
        .await
        .expect("P1 original physical disposal");
    assert_eq!(
        runtime.st.lock().unwrap().removals,
        vec![("exact-pod".into(), 0)],
        "P1 the existing removal edge targets one exact locator"
    );
    let observations_after_valid_cleanup = runtime.st.lock().unwrap().observations;

    let retained_runtime = Arc::new(FakeRuntime::default());
    {
        let mut state = retained_runtime.st.lock().unwrap();
        state.native_memory = true;
        state.recovered_created_pod = Some((
            "exact-pod".into(),
            "pod-uid".into(),
            Some("claim-uid".into()),
        ));
    }
    retained_runtime.set_observation(pc::SandboxObservation::Provisioning { deadline: None });
    let retained_provider = provider_without_broker(retained_runtime.clone());
    let mut retained = empty.clone();
    retained.filesystem_continuity = pc::FilesystemContinuity::Retained;
    let retained_environment = SandboxProvider::prepare_terminal_for_effect(
        &retained_provider,
        &retained,
        None,
        Some(&create),
        &terminal,
        &authority,
    )
    .await
    .unwrap()
    .expect("P4 exact created Pod/PVC may still be Provisioning");
    let retained_handle = retained_environment.handle().clone();
    assert!(matches!(
        retained_handle.container_payload().unwrap().runtime_handle.as_ref(),
        Some(crate::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid,
            claim_uid: Some(claim_uid),
        }) if pod_uid == "pod-uid" && claim_uid == "claim-uid"
    ));
    let prepared = retained_environment
        .prepare_disposal_for_effect(&terminal, &authority)
        .await
        .expect("P4 original cleanup preparation");
    let removal = disposal_authorization(&prepared);
    retained_environment
        .dispose_for_effect(
            &removal,
            &pc::test_support::EffectAuthority::new(removal.effect_fence()),
        )
        .await
        .expect("P4 original physical disposal");
    assert_eq!(retained_runtime.st.lock().unwrap().removals.len(), 1);
    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &retained_provider,
            &retained,
            Some(&retained_handle),
            Some(&create),
            &terminal,
            &authority,
        )
        .await
        .is_err(),
        "P4 ordinary Provisioning cannot borrow the missing-handle join"
    );

    let mut rich = empty.clone();
    rich.mounts = spec("created-pod-terminal").mounts;
    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &provider,
            &rich,
            None,
            Some(&create),
            &terminal,
            &authority,
        )
        .await
        .is_err(),
        "P2 unretained participants stay Unknown"
    );
    assert_eq!(
        runtime.st.lock().unwrap().observations,
        observations_after_valid_cleanup,
        "P2 zero additional observation"
    );
    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &provider, &empty, None, None, &terminal, &authority,
        )
        .await
        .is_err(),
        "P3 no previous effect means no physical join"
    );
    assert_eq!(
        runtime.st.lock().unwrap().observations,
        observations_after_valid_cleanup,
        "P3 zero additional observation"
    );
    assert_eq!(
        runtime.st.lock().unwrap().removals.len(),
        1,
        "P2/P3 no delete"
    );
}
#[tokio::test]
async fn guarded_claim_uses_the_existing_two_phase_terminal_owner() {
    // Causes: C1 the old Worker join retained one inert protected Pod and one
    // exact original PVC, but no Session handle; C2 the frozen spec has no
    // mount/control participant; C3 current terminal authority is present at
    // prepare and later disposal. Effects: E1 C1+C2+C3 returns one terminal
    // cleanup-only environment without adopting a runnable Pod; E2 preparation
    // makes zero physical DELETEs; E3 revoked disposal makes zero DELETEs; E4
    // authorized disposal re-observes and removes exactly once. Rules G1 all
    // causes=>E1+E2+E4, G2 !C2=>Unknown, G3 !C3=>E3. The real K8s test owns
    // guard policy/UID and PVC API effects; this case owns provider ordering.
    use crate::{CreateTicket, CreateTicketRequest, TicketOrigin, TicketResult, TicketStatus};
    let scope = "guarded-claim-terminal";
    let create = pc::SandboxEffectFence::new(
        "create-guarded-claim",
        "worker-owner",
        "worker-incarnation",
        1,
        u64::MAX,
    )
    .unwrap();
    let terminal = pc::SandboxEffectFence::new(
        "terminal-guarded-claim",
        "worker-owner",
        "worker-incarnation",
        2,
        u64::MAX,
    )
    .unwrap();
    let ticket = CreateTicket {
        request: CreateTicketRequest::new(
            "old-group",
            "pods",
            "default",
            "awaken-guarded-claim-terminal",
            "a".repeat(64),
        )
        .unwrap(),
        status: TicketStatus::Finished(TicketResult::Isolated {
            uid: "guard-uid".into(),
        }),
        origin: TicketOrigin::TerminalGuard,
        isolation_id: "0123456789abcdef0123456789abcdef".into(),
        isolation_pending: false,
    };
    let guarded =
        crate::KubernetesGuardedClaimHandle::new(scope, &create, ticket, "claim-uid").unwrap();
    let runtime = Arc::new(FakeRuntime::default());
    runtime.st.lock().unwrap().native_memory = true;
    runtime.st.lock().unwrap().recovered_guarded_claim =
        Some(("awaken-guarded-claim-terminal".into(), guarded));
    runtime.set_observation(pc::SandboxObservation::Terminal {
        physical_incarnation: "guard-uid".into(),
    });
    let provider = provider_without_broker(runtime.clone());
    let mut frozen = spec(scope);
    frozen.mounts.clear();
    frozen.env.clear();
    frozen.filesystem_continuity = pc::FilesystemContinuity::Retained;
    let authority = pc::test_support::EffectAuthority::new(&terminal);
    let environment = SandboxProvider::prepare_terminal_for_effect(
        &provider,
        &frozen,
        None,
        Some(&create),
        &terminal,
        &authority,
    )
    .await
    .unwrap()
    .expect("G1 exact guarded claim remains a physical cleanup target");
    assert_eq!(
        runtime.st.lock().unwrap().observations,
        0,
        "G1 no runnable Pod adoption"
    );
    assert_eq!(
        runtime.st.lock().unwrap().removals.len(),
        0,
        "G2 zero premature DELETE"
    );
    assert_eq!(
        environment
            .handle()
            .container_physical_incarnation()
            .unwrap(),
        "guard-uid"
    );
    let prepared = environment
        .prepare_disposal_for_effect(&terminal, &authority)
        .await
        .unwrap();
    assert_eq!(
        runtime.st.lock().unwrap().removals.len(),
        0,
        "G2 preparation only"
    );
    let removal = disposal_authorization(&prepared);
    assert!(
        environment
            .dispose_for_effect(&removal, &pc::test_support::EffectAuthority::denied())
            .await
            .is_err(),
        "G3 current authority required"
    );
    assert_eq!(runtime.st.lock().unwrap().removals.len(), 0, "G3 no DELETE");
    environment
        .dispose_for_effect(
            &removal,
            &pc::test_support::EffectAuthority::new(removal.effect_fence()),
        )
        .await
        .unwrap();
    assert_eq!(
        runtime.st.lock().unwrap().removals,
        vec![("awaken-guarded-claim-terminal".into(), 0)],
        "G1/E4 one existing disposal edge"
    );

    let mut rich = frozen;
    rich.mounts = spec(scope).mounts;
    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &provider,
            &rich,
            None,
            Some(&create),
            &terminal,
            &authority,
        )
        .await
        .is_err(),
        "G2 unretained participants stay Unknown"
    );
    assert_eq!(
        runtime.st.lock().unwrap().removals.len(),
        1,
        "G2 no second DELETE"
    );
}
