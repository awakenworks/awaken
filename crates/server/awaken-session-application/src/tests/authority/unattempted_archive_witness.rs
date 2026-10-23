use super::unattempted_profiled_release::unattempted;
use super::*;

fn fact(id: &str) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: awaken_session_event_contract::session_terminated_fact_id(id),
        object_id: id.into(),
        workspace_id: Some("workspace".into()),
        event_type: awaken_session_event_contract::SESSION_TERMINATED.into(),
        timestamp: 1,
        runtime_interval: None,
    }
}

#[test]
fn only_the_exact_committed_unattempted_archive_receipt_projects_no_create() {
    run_composed_async_test(|| async {
        // Witness graph: C1 archived snapshot; C2 exact Root/Workspace/fact
        // identity; C3 unattempted-profiled admission hash; C4 receipt revision
        // belongs to the snapshot. W1 all -> true from durable receipt alone;
        // W2 absent receipt/live root/wrong owner/stale snapshot/other admission
        // -> false. W3 receipt authority unavailable -> error, never absence.
        // No token count, Run terminal or sampled handle is an input.
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let app = application_with_runtime(
            Arc::new(RecordingCleanupRuntime::default()),
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let id = "unattempted-witness";
        create(repo.as_ref(), unattempted(id)).await;
        let before = repo.get(&SessionId::from(id)).await.unwrap();
        assert!(
            !crate::archive_intent::unattempted_profiled_archive_witness(
                repo.as_ref(),
                "workspace",
                &before
            )
            .await
            .unwrap(),
            "W2 before admission"
        );
        app.terminate_session_with_admission(
            &SessionId::from(id),
            "2026-10-10T00:00:00Z",
            fact(id),
            awaken_session_state_contract::SessionArchiveAdmission::ProfiledRelease,
        )
        .await
        .unwrap();
        let root = repo.get(&SessionId::from(id)).await.unwrap();
        assert!(
            crate::archive_intent::unattempted_profiled_archive_witness(
                repo.as_ref(),
                "workspace",
                &root
            )
            .await
            .unwrap(),
            "W1 actual atomic receipt"
        );
        let faulting = FaultingSessionRepository::new(repo.clone());
        faulting
            .fail_idempotency_receipt_once
            .store(true, Ordering::SeqCst);
        assert!(
            matches!(
                crate::archive_intent::unattempted_profiled_archive_witness(
                    &faulting,
                    "workspace",
                    &root
                )
                .await,
                Err(awaken_session_state_contract::SessionRepositoryError::AuthorityUnavailable(_))
            ),
            "W3 unavailable receipt is not negative evidence"
        );
        assert!(
            !crate::archive_intent::unattempted_profiled_archive_witness(
                repo.as_ref(),
                "foreign",
                &root
            )
            .await
            .unwrap(),
            "W2 Workspace binding"
        );
        let mut stale = root.clone();
        stale.revision.0 = 0;
        assert!(
            !crate::archive_intent::unattempted_profiled_archive_witness(
                repo.as_ref(),
                "workspace",
                &stale
            )
            .await
            .unwrap(),
            "W2 future receipt excluded"
        );
        let mut foreign = root.clone();
        foreign.session_id = "another-root".into();
        assert!(
            !crate::archive_intent::unattempted_profiled_archive_witness(
                repo.as_ref(),
                "workspace",
                &foreign
            )
            .await
            .unwrap(),
            "W2 another Root/absence"
        );

        let ordinary = "ordinary-witness";
        let mut idle = unattempted(ordinary);
        idle.execution = SessionExecutionState::Idle;
        create(repo.as_ref(), idle).await;
        app.terminate_session(ordinary, "2026-10-10T00:00:00Z", fact(ordinary))
            .await
            .unwrap();
        let ordinary_root = repo.get(&SessionId::from(ordinary)).await.unwrap();
        assert!(
            !crate::archive_intent::unattempted_profiled_archive_witness(
                repo.as_ref(),
                "workspace",
                &ordinary_root
            )
            .await
            .unwrap(),
            "W2 same canonical key cannot borrow IdleOnly admission"
        );
    });
}
