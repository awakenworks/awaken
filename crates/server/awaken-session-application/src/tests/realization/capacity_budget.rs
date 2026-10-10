use super::*;

#[tokio::test]
async fn capacity_failures_keep_preparing_without_consuming_the_durable_budget() {
    // ADR-0092 decision 3 on the durable aggregate. Causes: C1 the reported
    // failure is a capacity refusal / an ordinary retryable failure / a
    // malformed capacity report on a non-retryable failure; C2 the persisted
    // attempts reach the budget. Effects: E1 capacity keeps Preparing, expires
    // the lease and refunds the attempt its assignment counted, any number of
    // times; E2 ordinary retryable failures still consume and exhaust the same
    // budget afterwards; E3 a malformed report is rejected with no mutation.
    // | Rule | C1 | C2 | effect |
    // | D1 | capacity, repeated past the budget | n/a | E1 |
    // | D2 | retryable, after D1 | below then at | E2 |
    // | D3 | capacity on a non-retryable failure | any | E3 |
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        sqlite_session_repository("session repository").await;
    create(
        repo.as_ref(),
        persisted("capacity-budget", false, "preparing"),
    )
    .await;
    let app = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            realization_retry_budget: 2,
            ..Default::default()
        },
    );
    let id = SessionId::from("capacity-budget");
    let begin = |round: usize| {
        let app = &app;
        async move {
            app.begin_session_realization(
                awaken_session_realization_contract::BeginSessionRealization {
                    expected_resources: None,
                    session_id: "capacity-budget".into(),
                    target: awaken_session_realization_contract::SessionRealizationTarget {
                        owner: "worker".into(),
                        runtime_incarnation: format!("worker/{round}"),
                        lease_expires_at_unix_ms: u64::MAX,
                        reassign_existing_lease: false,
                    },
                },
            )
            .await
            .expect("begin")
            .authority
            .lease
        }
    };
    let failure = |lease, retryable, capacity_unavailable| {
        awaken_session_realization_contract::FailSessionRealization {
            session_id: "capacity-budget".into(),
            lease,
            prepared_resource_revision: None,
            retryable,
            capacity_unavailable,
            source_run_id: None,
            reason: "exceeded quota: compute".into(),
        }
    };

    for round in 1..=5 {
        let lease = begin(round).await;
        assert_eq!(
            repo.get(&id).await.unwrap().realization_progress.attempts,
            1,
            "D1 assignment counts one attempt"
        );
        app.fail_session_realization(failure(lease, true, true))
            .await
            .expect("D1 capacity failure");
        let after = repo.get(&id).await.unwrap();
        assert_eq!(after.execution, SessionExecutionState::Preparing, "D1/E1");
        assert_eq!(after.realization_progress.attempts, 0, "D1/E1 refunded");
        assert_eq!(
            after.realization.as_ref().unwrap().expires_at_unix_ms,
            0,
            "D1/E1 lease expired"
        );
    }

    let lease = begin(6).await;
    let before_malformed = repo.get(&id).await.unwrap();
    assert!(
        matches!(
            app.fail_session_realization(failure(lease.clone(), false, true))
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "D3/E3"
    );
    assert_eq!(repo.get(&id).await.unwrap(), before_malformed, "D3/E3");

    app.fail_session_realization(failure(lease, true, false))
        .await
        .expect("D2 first retryable failure");
    let after_first = repo.get(&id).await.unwrap();
    assert_eq!(
        after_first.execution,
        SessionExecutionState::Preparing,
        "D2"
    );
    assert_eq!(after_first.realization_progress.attempts, 1, "D2 consumed");
    let lease = begin(7).await;
    app.fail_session_realization(failure(lease, true, false))
        .await
        .expect("D2 exhausted");
    assert_eq!(
        repo.get(&id).await.unwrap().execution,
        SessionExecutionState::ActivationFailed,
        "D2/E2"
    );
}
