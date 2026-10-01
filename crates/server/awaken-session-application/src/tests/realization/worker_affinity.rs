use super::*;

#[tokio::test]
async fn worker_realization_owner_read_keeps_expired_unfinished_session_occupied() {
    // Owner-domain cause/effect table: C1 exact/different Worker, C2 expired
    // realization lease, C3 cleanup incomplete/completed. R1 exact+C2+
    // incomplete -> occupied; R2 different owner -> empty; R3 terminal
    // cleanup completed -> empty. Expiry cannot manufacture physical cleanup.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut retained = persisted("affinity-expired", true, "idle");
    retained.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-affinity".into(),
        runtime_incarnation: "worker-affinity:pod-uid".into(),
        epoch: 7,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), retained).await;
    let mut terminal = persisted("affinity-completed", false, "terminated");
    terminal.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-completed".into(),
        runtime_incarnation: "worker-completed:pod-uid".into(),
        epoch: 1,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), terminal).await;
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    assert!(
        application
            .has_unfinished_worker_realizations("worker-affinity")
            .await
            .unwrap(),
        "R1 expired lease remains physical affinity",
    );
    assert!(
        !application
            .has_unfinished_worker_realizations("other-worker")
            .await
            .unwrap(),
        "R2 another Worker owns no matching Session",
    );
    assert!(
        application
            .has_unfinished_worker_realizations("")
            .await
            .is_err(),
        "invalid owner cannot prove an empty domain",
    );
    application
        .release_terminal_resources("workspace", "affinity-completed")
        .await
        .expect("R3 terminal cleanup")
        .expect("R3 retained Session");
    assert!(
        !application
            .has_unfinished_worker_realizations("worker-completed")
            .await
            .unwrap(),
        "R3 completed cleanup releases replacement hold",
    );
}
