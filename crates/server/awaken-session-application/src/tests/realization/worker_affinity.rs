use super::*;

#[tokio::test]
async fn terminal_cleanup_capacity_counts_distinct_owners_from_one_root_scan() {
    // Causes: C1 two requested terminal roots share owner A; C2 one requested
    // root has owner B; C3 one never-materialized terminal root is unbound;
    // C4 one live root has owner C; C5 the terminal owner leases have expired;
    // C6 B commits its cleanup receipt.
    // R1 C1-C4 -> three required Workers (A, B, first placement), never four
    // Sessions or a live-only C; R2 +C5+C6 -> two. The existing Session store and
    // root classifications own every row, with no Cloud-local index.
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    for (id, owner) in [
        ("terminal-a1", Some("worker-a")),
        ("terminal-a2", Some("worker-a")),
        ("terminal-b", Some("worker-b")),
        ("terminal-unbound", None),
    ] {
        let mut session = persisted(id, false, "terminated");
        session.archive("2026-10-05T00:00:00Z", 1).unwrap();
        if let Some(owner) = owner {
            session.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
                owner: owner.into(),
                runtime_incarnation: format!("{owner}:pod-uid"),
                epoch: 1,
                expires_at_unix_ms: 1,
            });
        }
        create(repo.as_ref(), session).await;
    }
    let mut live = persisted("live-c", true, "idle");
    live.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-c".into(),
        runtime_incarnation: "worker-c:pod-uid".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    });
    create(repo.as_ref(), live).await;
    assert_eq!(
        awaken_session_repository_contract::terminal_cleanup_worker_owners(repo.as_ref())
            .await
            .unwrap(),
        3,
        "R1",
    );
    application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .release_terminal_resources("workspace", "terminal-b")
    .await
    .expect("R2 cleanup")
    .expect("R2 retained Session");
    assert_eq!(
        awaken_session_repository_contract::terminal_cleanup_worker_owners(repo.as_ref())
            .await
            .unwrap(),
        2,
        "R2",
    );
}

#[tokio::test]
async fn terminal_cleanup_worker_demand_follows_the_verified_session_root() {
    // Causes: C1 execution is live/terminal; C2 cleanup is unrequested,
    // requested, or completed. Effects: R1 live/unrequested -> no terminal Worker demand;
    // R2 terminal/requested/exact -> demand even after its Run settled;
    // R3 completed with an actual root receipt -> no demand. The Session root
    // remains the sole lifecycle owner; forged requested binding is covered by
    // the contract test beside the predicate.
    let live = persisted("demand-live", true, "idle");
    assert!(!live.needs_terminal_cleanup_worker().unwrap(), "R1");
    let repo =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let mut terminal = persisted("demand-terminal", false, "terminated");
    terminal.archive("2026-10-05T00:00:00Z", 1).unwrap();
    assert!(terminal.needs_terminal_cleanup_worker().unwrap(), "R2");
    create(repo.as_ref(), terminal).await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    application
        .release_terminal_resources("workspace", "demand-terminal")
        .await
        .expect("R3 cleanup")
        .expect("R3 retained Session");
    let completed = repo.get(&SessionId::from("demand-terminal")).await.unwrap();
    assert!(
        completed.terminal_cleanup.is_completed(),
        "R3 durable completion"
    );
    assert!(!completed.needs_terminal_cleanup_worker().unwrap(), "R3");
}

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
