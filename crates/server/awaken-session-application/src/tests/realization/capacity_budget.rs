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
    let failure =
        |lease, retryable, not_ready| awaken_session_realization_contract::FailSessionRealization {
            session_id: "capacity-budget".into(),
            lease,
            prepared_resource_revision: None,
            retryable,
            not_ready,
            source_run_id: None,
            reason: "exceeded quota: compute".into(),
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

#[tokio::test]
async fn conflict_and_unclassified_internal_failures_retry_until_the_durable_budget_absorbs() {
    // The driver path's bounded retry on the durable aggregate. Causes: C1 the
    // failed effect's Run fault (state conflict / unclassified internal /
    // classified internal), reported exactly as the realization driver reports
    // it (`FailSessionRealization::for_effect`); C2 the persisted attempts
    // against the realization retry budget (2). Effects: E1 keep Preparing
    // with one attempt consumed; E2 ActivationFailed once the budget is spent,
    // after which the next claim's begin is refused as Terminal (the claim
    // absorbs the Run); E3 a classified internal fault absorbs at once.
    // | Rule | C1 | C2 | effect |
    // | B1 | conflict / unclassified internal | first failure | E1 |
    // | B2 | conflict / unclassified internal | budget spent | E2 |
    // | B3 | classified internal | first failure | E3 |
    use awaken_session_types::RunError;
    /// Row name, the failed effect's Run error, failures until it absorbs.
    type Row = (&'static str, fn() -> RunError, usize);
    let rows: [Row; 3] = [
        ("conflict", || RunError::conflict("raced"), 2),
        ("unclassified", || RunError::internal("unknown"), 2),
        (
            "classified",
            || RunError::classified("session_realization_invariant_violated", "broken"),
            1,
        ),
    ];
    for (name, failure, failures_to_absorb) in rows {
        let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
            sqlite_session_repository("session repository").await;
        let session = format!("budget-{name}");
        create(repo.as_ref(), persisted(&session, false, "preparing")).await;
        let app = application_with_configuration(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration {
                realization_retry_budget: 2,
                ..Default::default()
            },
        );
        let id = SessionId::from(session.as_str());
        let begin = |round: usize| {
            app.begin_session_realization(
                awaken_session_realization_contract::BeginSessionRealization {
                    expected_resources: None,
                    session_id: session.as_str().into(),
                    target: awaken_session_realization_contract::SessionRealizationTarget {
                        owner: "worker".into(),
                        runtime_incarnation: format!("worker/{round}"),
                        lease_expires_at_unix_ms: u64::MAX,
                        reassign_existing_lease: false,
                    },
                },
            )
        };
        for round in 1..=failures_to_absorb {
            let lease = begin(round).await.expect("begin").authority.lease;
            let report = awaken_session_realization_contract::FailSessionRealization::for_effect(
                session.as_str().into(),
                lease,
                None,
                None,
                &failure(),
            );
            app.fail_session_realization(report)
                .await
                .expect("report the failure");
            let after = repo.get(&id).await.unwrap();
            if round < failures_to_absorb {
                assert_eq!(
                    after.execution,
                    SessionExecutionState::Preparing,
                    "{name} B1"
                );
                assert_eq!(
                    after.realization_progress.attempts, round as u32,
                    "{name} B1"
                );
            } else {
                assert_eq!(
                    after.execution,
                    SessionExecutionState::ActivationFailed,
                    "{name} B2/B3"
                );
            }
        }
        assert!(
            matches!(
                begin(failures_to_absorb + 1).await,
                Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::Terminal
                )
            ),
            "{name}: the next claim's begin is terminal, so the claim absorbs the Run"
        );
    }
}
