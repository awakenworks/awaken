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
        let SessionWorkOwnership::Leased(lease) = app
            .work_assignments
            .acquire_session_work(&session, "worker:incarnation", now_ms, acquisition)
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
