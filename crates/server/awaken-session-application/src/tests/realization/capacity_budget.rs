// @formal evidence session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
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
            app.realizations
                .begin_session_realization(
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
            repo.get(&id)
                .await
                .unwrap()
                .realization_progress()
                .attempts(),
            1,
            "D1 assignment counts one attempt"
        );
        app.realizations
            .fail_session_realization(failure(lease, true, true))
            .await
            .expect("D1 capacity failure");
        let after = repo.get(&id).await.unwrap();
        assert_eq!(after.execution, SessionExecutionState::Preparing, "D1/E1");
        assert_eq!(after.realization_progress().attempts(), 0, "D1/E1 refunded");
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
            app.realizations
                .fail_session_realization(failure(lease.clone(), false, true))
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "D3/E3"
    );
    assert_eq!(repo.get(&id).await.unwrap(), before_malformed, "D3/E3");

    app.realizations
        .fail_session_realization(failure(lease, true, false))
        .await
        .expect("D2 first retryable failure");
    let after_first = repo.get(&id).await.unwrap();
    assert_eq!(
        after_first.execution,
        SessionExecutionState::Preparing,
        "D2"
    );
    assert_eq!(
        after_first.realization_progress().attempts(),
        1,
        "D2 consumed"
    );
    let lease = begin(7).await;
    app.realizations
        .fail_session_realization(failure(lease, true, false))
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
            app.realizations.begin_session_realization(
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
            app.realizations
                .fail_session_realization(report)
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
                    after.realization_progress().attempts(),
                    round as u32,
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

#[tokio::test]
async fn retryable_initial_realization_is_fenced_and_budgeted_durably() {
    // Cause/effect graph: C1 failure retryable/permanent; C2 persisted attempts
    // below/at budget; C3 asserted lease exact/stale. Effects: E1 retryable below
    // budget keeps Preparing and expires the lease; E2 the next assignment
    // advances epoch and attempts; E3 retryable at budget or permanent enters
    // activation_failed; E4 stale delivery mutates nothing. Decision rules
    // covered: R1 retryable+attempt1<2+exact => E1; R2 next exact claim => E2;
    // R3 retryable+attempt2=2+exact => E3; stale ownership is covered by the
    // realization authority table. C4 executable refresh fails before claim;
    // E5 no lease/revision mutation, and successful retry enters R1. Constraint
    // K1 refresh owns no realization state. D1 C4=>E5; D2 !C4=>R1.
    // FMECA: an unbounded retry can strand create
    // forever (S8/O5/D6), while immediate terminal failure loses recoverability
    // (S8/O4/D4); persisted attempts plus lease expiry bound both modes.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        sqlite_session_repository("session repository").await;
    create(repo.as_ref(), persisted("retry-budget", false, "preparing")).await;
    let refresh = Arc::new(ToggleProjectionRefresh::default());
    refresh.fail.store(true, Ordering::SeqCst);
    let mut app = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            realization_retry_budget: 2,
            ..Default::default()
        },
    );
    app.set_executable_projection_refresh(paired_projection_refresh(refresh.clone()))
        .unwrap();
    let target =
        |incarnation: &str| awaken_session_realization_contract::SessionRealizationTarget {
            owner: "worker".into(),
            runtime_incarnation: incarnation.into(),
            lease_expires_at_unix_ms: u64::MAX,
            reassign_existing_lease: false,
        };

    let before_refresh_failure = repo.get(&SessionId::from("retry-budget")).await.unwrap();
    assert!(
        matches!(
            app.realizations
                .begin_session_realization(
                    awaken_session_realization_contract::BeginSessionRealization {
                        expected_resources: None,
                        session_id: "retry-budget".into(),
                        target: target("worker/refresh-failure"),
                    }
                )
                .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "D1/E5"
    );
    assert_eq!(
        repo.get(&SessionId::from("retry-budget")).await.unwrap(),
        before_refresh_failure,
        "D1/E5 no claim mutation"
    );
    refresh.fail.store(false, Ordering::SeqCst);

    let first = app
        .realizations
        .begin_session_realization(
            awaken_session_realization_contract::BeginSessionRealization {
                expected_resources: None,
                session_id: "retry-budget".into(),
                target: target("worker/1"),
            },
        )
        .await
        .expect("R1 begin");
    app.realizations
        .fail_session_realization(
            awaken_session_realization_contract::FailSessionRealization {
                session_id: "retry-budget".into(),
                lease: first.authority.lease,
                prepared_resource_revision: None,
                retryable: true,
                not_ready: false,
                source_run_id: None,
                reason: "worker unavailable".into(),
            },
        )
        .await
        .expect("R1 retryable failure");
    let after_first = repo.get(&SessionId::from("retry-budget")).await.unwrap();
    assert_eq!(
        after_first.execution,
        SessionExecutionState::Preparing,
        "R1/E1"
    );
    assert_eq!(after_first.realization_progress().attempts(), 1, "R1/E1");
    assert_eq!(
        after_first.realization.as_ref().unwrap().expires_at_unix_ms,
        0,
        "R1/E1"
    );

    let second = app
        .realizations
        .begin_session_realization(
            awaken_session_realization_contract::BeginSessionRealization {
                expected_resources: None,
                session_id: "retry-budget".into(),
                target: target("worker/2"),
            },
        )
        .await
        .expect("R2 reclaim");
    assert_eq!(second.authority.lease.epoch, 2, "R2/E2");
    assert_eq!(
        repo.get(&SessionId::from("retry-budget"))
            .await
            .unwrap()
            .realization_progress()
            .attempts(),
        2,
        "R2/E2"
    );
    app.realizations
        .fail_session_realization(
            awaken_session_realization_contract::FailSessionRealization {
                session_id: "retry-budget".into(),
                lease: second.authority.lease,
                prepared_resource_revision: None,
                retryable: true,
                not_ready: false,
                source_run_id: None,
                reason: "worker unavailable again".into(),
            },
        )
        .await
        .expect("R3 exhausted");
    assert_eq!(
        repo.get(&SessionId::from("retry-budget"))
            .await
            .unwrap()
            .execution,
        SessionExecutionState::ActivationFailed,
        "R3/E3"
    );
}

#[tokio::test]
async fn failed_nonterminal_realization_expires_assignment_before_same_incarnation_recovery() {
    // Causes: C1 initial Preparing / already Idle; C2 retryable / permanent;
    // C3 attempt below / at budget; C4 exact / stale failure lease.
    // Decision table: F0 stale -> no root mutation; F1 initial+retry+below ->
    // Preparing, expired assignment; F2 other initial -> ActivationFailed;
    // F3 Idle+any C2/C3 -> Idle, expired assignment, retained active resources,
    // realization attempts unchanged (the Run's dispatch budget bounds retries).
    // F4 F1/F3+same-incarnation begin -> successor epoch, never reuse the closed
    // generation; old failure replay cannot mutate that successor. The existing
    // running-realization test owns interval/terminal settlement. The HTTP MCP
    // hot-swap test additionally proves the previous Active server still runs.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        sqlite_session_repository("session repository").await;
    let app = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            realization_retry_budget: 2,
            ..Default::default()
        },
    );
    for idle in [false, true] {
        for retryable in [false, true] {
            for attempts in [1, 2] {
                let id = format!("failure-{idle}-{retryable}-{attempts}");
                let lease = crate::session_resource::SessionRealizationLease {
                    owner: "worker".into(),
                    runtime_incarnation: "worker/1".into(),
                    epoch: 7,
                    expires_at_unix_ms: u64::MAX,
                };
                let mut root = persisted(&id, false, if idle { "idle" } else { "preparing" });
                root.realization = Some(lease.clone());
                // W3/F1-F3: retain the historical count at epoch 7, including
                // Idle; manufacturing new claims would change this cause.
                let mut progress = serde_json::to_value(&root).unwrap();
                progress["realization_progress"]["attempts"] = serde_json::json!(attempts);
                root = serde_json::from_value(progress).unwrap();
                create(repo.as_ref(), root).await;
                let before = repo.get(&SessionId::from(&id)).await.unwrap();
                let command = awaken_session_realization_contract::FailSessionRealization {
                    session_id: SessionId::from(&id),
                    lease: lease.clone(),
                    prepared_resource_revision: None,
                    retryable,
                    not_ready: false,
                    source_run_id: None,
                    reason: "external stage failed".into(),
                };
                let mut stale = command.clone();
                stale.lease.epoch -= 1;
                assert!(
                    app.realizations
                        .fail_session_realization(stale)
                        .await
                        .is_err(),
                    "F0"
                );
                assert_eq!(repo.get(&SessionId::from(&id)).await.unwrap(), before, "F0");
                app.realizations
                    .fail_session_realization(command.clone())
                    .await
                    .unwrap();
                let failed = repo.get(&SessionId::from(&id)).await.unwrap();
                let recoverable = idle || (retryable && attempts < 2);
                assert_eq!(
                    failed.execution,
                    if idle {
                        SessionExecutionState::Idle
                    } else if recoverable {
                        SessionExecutionState::Preparing
                    } else {
                        SessionExecutionState::ActivationFailed
                    },
                    "F1/F2/F3"
                );
                assert_eq!(
                    failed.resources, before.resources,
                    "retain active resource authority"
                );
                if idle {
                    // F3: an Idle Session's failure spends no realization
                    // budget; the Run's dispatch retry budget bounds it.
                    assert_eq!(
                        failed.realization_progress().attempts(),
                        attempts,
                        "F3 Idle keeps the realization budget"
                    );
                }
                assert_eq!(failed.realization.as_ref().unwrap().epoch, 7);
                if recoverable {
                    assert_eq!(
                        failed.realization.as_ref().unwrap().expires_at_unix_ms,
                        0,
                        "F1/F3 root cannot reissue the locally closed generation"
                    );
                    let directive = app
                        .realizations
                        .begin_session_realization(
                            awaken_session_realization_contract::BeginSessionRealization {
                                session_id: SessionId::from(&id),
                                expected_resources: None,
                                target:
                                    awaken_session_realization_contract::SessionRealizationTarget {
                                        owner: lease.owner.clone(),
                                        runtime_incarnation: lease.runtime_incarnation.clone(),
                                        lease_expires_at_unix_ms: u64::MAX,
                                        reassign_existing_lease: false,
                                    },
                            },
                        )
                        .await
                        .unwrap();
                    assert_eq!(directive.authority.lease.epoch, 8, "F4");
                    let successor = repo.get(&SessionId::from(&id)).await.unwrap();
                    assert!(
                        app.realizations
                            .fail_session_realization(command)
                            .await
                            .is_err(),
                        "F4 stale replay"
                    );
                    assert_eq!(
                        repo.get(&SessionId::from(id)).await.unwrap(),
                        successor,
                        "F4 successor retained"
                    );
                }
            }
        }
    }
}
