//! Complete transfer at the controlled Work authority boundary.
use super::*;

#[tokio::test]
async fn session_work_authority_bridges_the_complete_transfer_for_acquisition_and_release() {
    use awaken_session_application_contract::{
        SessionWorkAcquisition, SessionWorkGrant, SessionWorkOwnership, SessionWorkerLeaseAuthority,
    };

    let repo = authority_repository("work lease transfer bridge").await;
    create(repo.as_ref(), persisted("bridge", true, "idle")).await;
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application(repo, environments.clone());
    let session = SessionId::from("bridge");
    // Exercise both acquisition branches: repair stopped Work for a claimed
    // Run, then observe the existing grant during realization renewal.
    for (now_ms, acquisition) in [
        (11, SessionWorkAcquisition::ClaimedRun),
        (23, SessionWorkAcquisition::RealizationRenewal),
    ] {
        environments.work_clock.set(now_ms);
        let SessionWorkOwnership::Leased(lease) = app
            .work_assignments
            .acquire_session_work(&session, "worker:incarnation", acquisition)
            .await
            .unwrap()
        else {
            panic!("must observe the canonical Work grant");
        };
        let expected = awaken_session_types::SessionWorkLease::from_persisted(
            "work:bridge",
            "env-worker",
            session.clone(),
            "worker:incarnation",
            1,
            now_ms + 30_000,
        )
        .unwrap();
        assert_eq!(
            lease.to_transfer(),
            expected,
            "all coordinates survive acquisition"
        );
        assert_eq!(lease.epoch().get(), expected.epoch());
        let before = environments.released_leases.lock().unwrap().len();
        let foreign = SessionWorkGrant::from_transfer(
            awaken_session_types::SessionWorkLease::from_persisted(
                expected.work_id(),
                "foreign-environment",
                session.clone(),
                expected.owner(),
                expected.epoch(),
                expected.expires_at_unix_ms(),
            )
            .unwrap(),
        );
        assert!(
            !app.work_assignments
                .release_session_work(&foreign)
                .await
                .unwrap()
        );
        assert_eq!(
            environments.released_leases.lock().unwrap().len(),
            before,
            "foreign environment must not reach the lower lease port"
        );
        assert!(
            !app.work_assignments
                .release_session_work(&lease)
                .await
                .unwrap(),
            "preserve the lower port's refusal rather than manufacturing success"
        );
        let released = environments.released_leases.lock().unwrap();
        assert_eq!(released.len(), before + 1);
        assert_eq!(
            released.last(),
            Some(&expected),
            "release presents the exact original transfer"
        );
    }
}

#[tokio::test]
async fn an_idle_source_lease_renews_without_its_stopped_session_work() {
    // Causes: C1 the root: a self-hosted Worker-placed checkpoint-and-release
    // Session holding its source (generated Resident), the same root under the
    // Resident policy, or Hibernated; C2 the cause: a realization lease renewal
    // (no Environment effect) or a realization (effect) renewal; C3 the
    // Session's Work is stopped (a settled Run released it) or held.
    // Effects: E1 Work waived (NotRequired: the lease renews); E2 Unowned (the
    // Worker retires its local authority); E3 Leased (the held grant).
    // | Rule | C1                         | C2     | C3      | Effect |
    // | W1   | checkpoint, source held    | lease  | stopped | E1     |
    // | W2   | checkpoint, source held    | effect | stopped | E2     |
    // | W3   | Resident policy            | lease  | stopped | E2     |
    // | W4   | checkpoint, Hibernated     | lease  | stopped | E2     |
    // | W5   | checkpoint, source held    | lease  | held    | E3     |
    use awaken_session_application_contract::{
        SessionWorkAcquisition, SessionWorkOwnership, SessionWorkerLeaseAuthority,
    };
    let rooted = |id: &str, checkpoint: bool, hibernated: bool| {
        let mut session = persisted(id, true, "idle");
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut session.baseline
        else {
            unreachable!("fixture baseline is frozen")
        };
        if checkpoint {
            baseline.environment.idle_retention =
                super::continuation::checkpoint_release_retention();
        }
        let generation = awaken_session_contract::SandboxGeneration::new(
            &SessionId::from(id),
            0,
            u64::MAX,
            baseline.environment.config_fingerprint.0.clone(),
            "base",
        );
        session.environment = if hibernated {
            awaken_session_contract::SessionEnvironmentState::Hibernated {
                checkpoint: awaken_provisioning_contract::SandboxCheckpointRef {
                    id: "checkpoint".into(),
                    format: "awaken-fs-tar-v1".into(),
                    digest: "digest".into(),
                    size_bytes: 1,
                    created_at_unix_ms: 1,
                    expires_at_unix_ms: u64::MAX,
                    environment_fingerprint: generation.environment_fingerprint().to_owned(),
                    base_image_fingerprint: generation.base_image_fingerprint().to_owned(),
                    excluded_mounts: Vec::new(),
                    suspend_effect_id: "suspend".into(),
                },
                generation,
            }
        } else {
            awaken_session_contract::SessionEnvironmentState::Resident {
                binding: "source".into(),
                effect_id: Some("create".into()),
                generation: Some(generation),
                idle_since_unix_ms: Some(1),
            }
        };
        session
    };
    let repo = authority_repository("idle source lease renewal").await;
    for (id, checkpoint, hibernated) in [
        ("source", true, false),
        ("resident-policy", false, false),
        ("hibernated", true, true),
    ] {
        create(repo.as_ref(), rooted(id, checkpoint, hibernated)).await;
    }
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application(repo, environments.clone());
    let observe = |id: &'static str, cause| {
        let app = &app;
        async move {
            app.work_assignments
                .acquire_session_work(&SessionId::from(id), "worker:incarnation", cause)
                .await
                .unwrap()
        }
    };
    let lease_only = SessionWorkAcquisition::RealizationLeaseRenewal;
    assert_eq!(
        observe("source", lease_only).await,
        SessionWorkOwnership::NotRequired,
        "W1"
    );
    assert_eq!(
        observe("source", SessionWorkAcquisition::RealizationRenewal).await,
        SessionWorkOwnership::Unowned,
        "W2"
    );
    assert_eq!(
        observe("resident-policy", lease_only).await,
        SessionWorkOwnership::Unowned,
        "W3"
    );
    assert_eq!(
        observe("hibernated", lease_only).await,
        SessionWorkOwnership::Unowned,
        "W4"
    );
    environments
        .awakened
        .lock()
        .unwrap()
        .insert("source".to_owned());
    assert!(
        matches!(
            observe("source", lease_only).await,
            SessionWorkOwnership::Leased(_)
        ),
        "W5"
    );
}
