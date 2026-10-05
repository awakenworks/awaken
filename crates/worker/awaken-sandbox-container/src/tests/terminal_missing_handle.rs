//! One provider-owned classification for a missing durable container handle.

use super::*;

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
        let outcome = ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
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
    // P2=C1+C2+!C3+C4->E2, P3=C1+C2+C3+!C4->E3.
    let runtime = Arc::new(FakeRuntime::default());
    {
        let mut state = runtime.st.lock().unwrap();
        state.native_memory = true; // K8s reconstructs without host-bind staging.
        state.recovered_created_pod = Some(("exact-pod".into(), "pod-uid".into()));
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
    let environment = ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
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
        vec![ContainerObservationPurpose::TerminalCleanup],
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

    let mut rich = empty.clone();
    rich.mounts = spec("created-pod-terminal").mounts;
    assert!(
        ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
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
        ContainerEnvironmentProvider::prepare_terminal_environment_for_effect(
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
