use super::*;

#[tokio::test]
async fn durable_session_truth_owns_one_work_projection_path() {
    // Cause/effect graph: C1 frozen Environment is self-hosted; C2 Session is
    // nonterminal; C3 the projection command is replayed. Effects: E1 only
    // C1+C2 dispatches;
    // E2 replay uses the same idempotent port and creates no second identity.
    //
    // | Rule | self-hosted | terminal | replay | effect |
    // | R1 | yes | no | no | project one |
    // | R2 | yes | no | yes | retain one |
    // | R3 | no | no | any | skip |
    // | R4 | yes | yes | any | skip |
    let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
        .await
        .expect("session repository");
    create(&repo, persisted("external", true, "idle")).await;
    create(&repo, persisted("local", false, "idle")).await;
    create(&repo, persisted("terminal", true, "terminated")).await;
    let environments = RecordingEnvironmentSource::default();

    let first = reconcile_work_dispatches(&repo, &environments, 0).await;
    assert_eq!(first.settled, 1, "R1/R3/R4");
    assert!(first.failures.is_empty());
    assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R1");

    let replay = reconcile_work_dispatches(&repo, &environments, 0).await;
    assert_eq!(replay.settled, 1, "R2");
    assert!(replay.failures.is_empty());
    assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R2");
}

#[tokio::test]
async fn work_dispatch_reconciliation_isolates_each_session_failure() {
    // Work-projection FMECA decision table. Causes: C1 a durable Session needs
    // external work; C2 its queue write succeeds; C3 a sibling queue write
    // fails; C4 an unrelated Session needs no dispatch. Effects: E1 every
    // eligible Session is attempted; E2 successes settle independently; E3
    // failures retain exact Session/Environment diagnostics for later retry;
    // E4 ineligible Sessions cause no side effect. Rules: W1 C1+C2=>E1+E2;
    // W2 C1+C3=>E1+E3 without aborting W1; W3 C4=>E4.
    let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
        .await
        .expect("session repository");
    create(&repo, persisted("external-failed", true, "idle")).await;
    create(&repo, persisted("external-settled", true, "idle")).await;
    create(&repo, persisted("local-skip", false, "idle")).await;
    let environments = RecordingEnvironmentSource::default();
    environments.fail_for("external-failed");

    let report = reconcile_work_dispatches(&repo, &environments, 0).await;
    assert_eq!(report.settled, 1, "W1/W2");
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["external-settled".to_string()],
        "W1/W3"
    );
    assert_eq!(report.failures.len(), 1, "W2");
    assert_eq!(report.failures[0].session_id, "external-failed", "W2");
    assert_eq!(report.failures[0].environment_id, "env-worker", "W2");
    assert!(
        report.failures[0].message.contains("injected failure"),
        "W2 preserves the retry diagnostic"
    );
}
#[tokio::test]
async fn complete_recovery_scan_reaches_page_two_and_purge_fails_closed() {
    // Full-scan cause/effect graph: C1 the canonical recovery index has 256
    // active, nonblocking rows on page one; C2 row 257 both needs WorkQueue
    // recovery and retains the target MemoryStore; C3 page two succeeds,
    // returns an error, reports quarantine, or returns a non-advancing cursor.
    // Effects: E1 successful recovery observes and dispatches row 257; E2 the
    // purge guard returns its exact Session blocker and therefore grants zero
    // physical-purge authority; E3 any incomplete or invalid full scan fails
    // closed before a partial recovery effect or purge decision. The repository
    // keyset remains the sole row authority; the application retains only
    // disposable identities and owns no durable cursor or recovery registry.
    //
    // | Rule | page one | page two | Effect |
    // |---|---|---|---|
    // | F1 | 256 active rows | row 257 succeeds | E1 + E2 |
    // | F2 | 256 active rows | repository error | E3 |
    // | F3 | 256 active rows | quarantine | E3 |
    // | F4 | 256 active rows | non-advancing cursor | E3 |
    use awaken_resource_contract::ResourcePurgeGuard as _;

    let repo = authority_repository("paged Session repository").await;
    for index in 0..256 {
        create(
            repo.as_ref(),
            persisted(&format!("page-a-active-{index:03}"), true, "idle"),
        )
        .await;
    }
    let memory_store_id = awaken_resource_contract::MemoryStoreId::from("page-two-memory");
    let mut blocker = persisted("page-z-blocker", true, "idle");
    blocker.resources = awaken_session_contract::SessionResourceState::from_active(
        crate::session_resource::ResolvedSessionResources::try_new(
            vec![crate::session_resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::from("page-two-memory"),
                source: crate::session_resource::ResolvedInputSource::MemoryStore {
                    memory_store_id: memory_store_id.clone(),
                    config: awaken_resource_contract::MemoryStoreConfigVersion {
                        memory_store_id: memory_store_id.clone(),
                        version: awaken_resource_contract::ConfigVersion::INITIAL,
                        retention_policy: Default::default(),
                    },
                },
                mount_path: "/memory/page-two".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .expect("valid page-two MemoryStore binding"),
    );
    create(repo.as_ref(), blocker).await;
    let repository: Arc<dyn crate::SessionStore> = repo.clone();

    let failing_recovery = Arc::new(FaultingSessionRepository::new(repository.clone()));
    failing_recovery.fail_recovery_scan_on_call(2);
    let incomplete_environments = RecordingEnvironmentSource::default();
    let incomplete =
        reconcile_work_dispatches(failing_recovery.as_ref(), &incomplete_environments, 0).await;
    assert_eq!(incomplete.settled, 0, "F2/E3");
    assert_eq!(
        incomplete_environments
            .dispatch_calls
            .load(Ordering::SeqCst),
        0,
        "F2/E3 page-one rows cannot produce partial effects"
    );
    assert_eq!(incomplete.failures.len(), 1, "F2/E3 repository failure");

    let environments = RecordingEnvironmentSource::default();
    let recovery = reconcile_work_dispatches(repo.as_ref(), &environments, 0).await;
    assert!(recovery.failures.is_empty(), "F1/E1: {recovery:?}");
    assert_eq!(recovery.settled, 257, "F1/E1 every page is recovered");
    assert!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .contains("page-z-blocker"),
        "F1/E1 row 257 remains visible to recovery"
    );

    let target = awaken_resource_contract::ResourceTarget::new(
        "workspace",
        awaken_resource_contract::ResourceKind::MemoryStore,
        memory_store_id.as_str(),
    );
    let guard = SessionResourcePurgeGuard::new(repository.clone(), Arc::new(UnusedFileCatalog));
    assert_eq!(
        guard.blockers(&target, None, 0).await.expect("F1 scan"),
        [awaken_resource_contract::ResourceReference {
            kind: awaken_resource_contract::ResourceReferenceKind::SessionBinding,
            reference_id: "page-z-blocker".into(),
        }],
        "F1/E2 row 257 blocks physical purge"
    );

    let failing = Arc::new(FaultingSessionRepository::new(repository.clone()));
    failing.fail_recovery_scan_on_call(2);
    let guard = SessionResourcePurgeGuard::new(failing, Arc::new(UnusedFileCatalog));
    assert!(
        guard.blockers(&target, None, 0).await.is_err(),
        "F2/E3 a later-page outage grants zero purge authority"
    );

    let quarantined = Arc::new(FaultingSessionRepository::new(repository.clone()));
    quarantined.quarantine_recovery_scan_on_call(2);
    let guard = SessionResourcePurgeGuard::new(quarantined, Arc::new(UnusedFileCatalog));
    assert!(
        guard.blockers(&target, None, 0).await.is_err(),
        "F3/E3 later-page quarantine grants zero purge authority"
    );

    let stalled = Arc::new(FaultingSessionRepository::new(repository));
    stalled.stall_recovery_cursor_on_call(2);
    let guard = SessionResourcePurgeGuard::new(stalled, Arc::new(UnusedFileCatalog));
    assert!(
        guard.blockers(&target, None, 0).await.is_err(),
        "F4/E3 a non-advancing cursor grants zero purge authority"
    );
}
