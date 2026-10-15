//! New first-admission cases beside the existing binding authority owner.
use super::*;

pub(super) fn fresh_subject(id: &str) -> PersistedSession {
    let old = persisted(id, false, "idle");
    let mut fresh = PersistedSession::frozen_with_budget(
        id,
        old.frozen_baseline().unwrap().clone(),
        old.resources,
        old.mcp,
        old.title,
        old.metadata,
        old.tools,
        old.budget,
    );
    fresh.execution = old.execution;
    fresh.created_at_unix_ms = Some(100);
    fresh.updated_at_unix_ms = Some(100);
    fresh
}

pub(super) async fn claim_subject(
    app: &SessionApplication,
    repo: &dyn awaken_session_repository_contract::SessionStore,
    id: &str,
    lease: &crate::session_resource::SessionRealizationLease,
) {
    for epoch in 1..=lease.epoch {
        let mut root = awaken_session_state_contract::SessionAggregate::try_from(
            repo.get(&SessionId::from(id)).await.unwrap(),
        )
        .unwrap();
        root.assignment()
            .claim(
                crate::session_resource::SessionRealizationLease {
                    epoch,
                    ..lease.clone()
                },
                false,
            )
            .unwrap();
        let committed = app
            .commit_session_snapshot_with_outcome(
                "workspace",
                root,
                "test-first-materialization-placement",
                Vec::new(),
            )
            .await
            .unwrap();
        // The following tests require a real, Store-confirmed claim generation,
        // not an ignored commit result or locally authored authority fixture.
        assert!(committed.transitioned(), "fixture claim won the actual CAS");
        assert_eq!(
            committed.session().realization.as_ref().unwrap().epoch,
            epoch
        );
    }
}

fn original_lease() -> crate::session_resource::SessionRealizationLease {
    crate::session_resource::SessionRealizationLease {
        owner: "runtime-a".into(),
        runtime_incarnation: "runtime-a/boot-1".into(),
        epoch: 3,
        expires_at_unix_ms: u64::MAX,
    }
}

fn intent(
    id: &str,
    lease: &crate::session_resource::SessionRealizationLease,
) -> awaken_session_state_contract::SessionEnvironmentEffectIntent {
    awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
        id,
        awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        Some(lease.clone()),
    )
    .for_environment("env-7")
}

#[tokio::test]
async fn legacy_binding_subject_rejects_new_admission_but_accepts_its_inflight_receipt() {
    // B15 preserves the original binding-fence subject/epoch/time/Unknown
    // history. New authorize rejects with no mutation; its genuine legacy
    // outcome still joins binding through the old receipt law, never origin.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let lease = original_lease();
    let mut old = persisted("binding-fence", false, "idle");
    old.realization = Some(lease.clone());
    old.created_at_unix_ms = Some(100);
    old.updated_at_unix_ms = Some(100);
    create(repo.as_ref(), old).await;
    let sink = crate::SessionPersistence::new(
        repo.clone(),
        Arc::new(awaken_lease_testkit::ManualClock::new(200)),
    )
    .environment_binding_sink();
    let original = intent("binding-fence", &lease);
    let before = repo.get(&SessionId::from("binding-fence")).await.unwrap();
    assert_eq!(
        sink.authorize(&original).await.unwrap_err().code,
        "session_environment_materialization_pending"
    );
    assert_eq!(repo.get(&before.session_id).await.unwrap(), before);
    sink.persist(
        awaken_session_state_contract::SessionEnvironmentReceipt::from_intent(
            &original,
            "legacy-binding",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    assert!(
        repo.get(&before.session_id)
            .await
            .unwrap()
            .environment_materialization()
            .is_none()
    );
}

/// L1 known-empty / L2 saved-pending, crossed with Adopt/Reservation and
/// admission/receipt: retryable Pending before CAS, exact snapshot/revision.
/// Both cuts use the real Repository sink; no fake authorization stands in for it.
async fn assert_late_noncreate_is_pending(
    sink: &dyn awaken_session_execution_contract::SessionEnvironmentBindingSink,
    repo: &dyn awaken_session_repository_contract::SessionRepository,
    before: &PersistedSession,
    original: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
) {
    use awaken_session_state_contract::{
        SessionEnvironmentEffectIntent, SessionEnvironmentEffectKind, SessionEnvironmentReceipt,
    };
    for kind in [
        SessionEnvironmentEffectKind::Adopt,
        SessionEnvironmentEffectKind::ResourceProjectionReservation {
            transition_fingerprint: "late".into(),
        },
    ] {
        let late = SessionEnvironmentEffectIntent::new(
            before.session_id.clone(),
            kind,
            original.realization().cloned(),
        )
        .for_environment(original.environment_fingerprint().unwrap())
        .from_binding("late-binding");
        assert_eq!(
            sink.authorize(&late).await.unwrap_err().code,
            "session_environment_materialization_pending"
        );
        assert_eq!(
            sink.persist(SessionEnvironmentReceipt::from_intent(&late, "late-binding").unwrap())
                .await
                .unwrap_err()
                .code,
            "session_environment_materialization_pending"
        );
        assert_eq!(
            repo.get(&before.session_id).await.unwrap(),
            *before,
            "L1/L2 zero root/fact/revision mutation"
        );
    }
}

#[tokio::test]
async fn first_admission_waits_for_real_cas_and_returns_the_durable_winner_after_response_loss() {
    // The sink has one closed Admit/Persist CAS executor: Admit's response-loss
    // recovery requires exact readback; the existing B1-B18 receipt table keeps
    // Persist errors and the terminal Fenced join, without a second writer.
    // B16 decision table: C1 pre-CAS outage -> no grant/root write; C2 real
    // CAS paused -> no post-authorize continuation; C3 same-effect writer with
    // another expiry wins and loses acknowledgement -> exactly one first;
    // C4 stale paused writer retries -> returns winner, never its own value.
    // All mutations use the real SQLite owner/CAS and existing fault cuts.
    // L1/L2 cross known-empty/pending with both non-Create kinds and both
    // admission/late-receipt planes; L3 then proves exact first receipt/replay.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let faulting = Arc::new(FaultingSessionRepository::new(repo.clone()));
    let persistence = crate::SessionPersistence::new(
        faulting.clone(),
        Arc::new(awaken_lease_testkit::ManualClock::new(200)),
    );
    let sink = persistence.environment_binding_sink();
    let app = SessionApplicationBuilder::fixture(
        Arc::new(RecordingCleanupRuntime::default()),
        Arc::new(NoopMcpRealizer),
        persistence,
        Arc::new(RecordingEnvironmentSource::default()),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .expect("valid fixture Session application");
    let id = "first-admission-race";
    create(repo.as_ref(), fresh_subject(id)).await;
    let lease = original_lease();
    claim_subject(&app, repo.as_ref(), id, &lease).await;
    let original = intent(id, &lease);
    let before = repo.get(&SessionId::from(id)).await.unwrap();
    assert_late_noncreate_is_pending(sink.as_ref(), repo.as_ref(), &before, &original).await;
    faulting.fail_once("admit-environment");
    assert!(sink.authorize(&original).await.is_err());
    assert_eq!(repo.get(&before.session_id).await.unwrap(), before, "C1");
    let pause = faulting.pause_next_mutation("admit-environment");
    let mut old_lease = lease;
    old_lease.expires_at_unix_ms -= 1;
    let attempted = intent(id, &old_lease);
    let continued = Arc::new(AtomicBool::new(false));
    let waiting = {
        let sink = sink.clone();
        let continued = continued.clone();
        tokio::spawn(async move {
            let result = sink.authorize(&attempted).await;
            if result.is_ok() {
                continued.store(true, Ordering::SeqCst);
            }
            result
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered.notified())
        .await
        .unwrap();
    assert!(
        !continued.load(Ordering::SeqCst),
        "C2 no permission before CAS"
    );
    assert_eq!(repo.get(&before.session_id).await.unwrap(), before);
    faulting.commit_then_conflict_once("admit-environment");
    assert_eq!(
        sink.authorize(&original).await.unwrap().intent(),
        &original,
        "C3"
    );
    pause.release.notify_one();
    assert_eq!(waiting.await.unwrap().unwrap().intent(), &original, "C4");
    let after = repo.get(&before.session_id).await.unwrap();
    assert_eq!(after.revision.0, before.revision.0 + 1);
    assert_eq!(
        after.environment_materialization().unwrap().pending(),
        Some(&original)
    );
    assert_late_noncreate_is_pending(sink.as_ref(), repo.as_ref(), &after, &original).await;
    // L3 the exact first response-loss receipt joins once, replay is inert;
    // every later origin read retains the original full value/expiry.
    let receipt = awaken_session_state_contract::SessionEnvironmentReceipt::from_intent(
        &original,
        "first-binding",
    )
    .unwrap();
    faulting.commit_then_conflict_once("bind-environment");
    let completed = sink.persist(receipt.clone()).await.unwrap();
    let origin = repo.get(&after.session_id).await.unwrap();
    assert_eq!(origin.revision.0, after.revision.0 + 1);
    assert_eq!(
        origin.environment_materialization().unwrap().origin(),
        Some(&original)
    );
    assert!(
        origin
            .environment_materialization()
            .unwrap()
            .pending()
            .is_none()
    );
    assert_eq!(sink.persist(receipt).await.unwrap(), completed);
    assert_eq!(
        repo.get(&origin.session_id).await.unwrap(),
        origin,
        "L3 receipt replay has no new CAS"
    );
}
