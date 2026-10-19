use super::*;

#[test]
fn budget_reach_uses_the_first_closed_interval_whose_cumulative_usage_crossed_it() {
    // Cause/effect graph: C1 interval one closes below a transition cursor; C2
    // interval two closes at/above it; C3 the matching interval is still open
    // or absent; C4 tool approval owns C2 and an equal-or-later interval closes
    // on committed BudgetReached; C5 the crossing closes while its exact
    // Awaiting target remains answerable. E1 the accounting crossing is interval two;
    // E2 C3 yields None; E3 C4 moves the one public budget pair to the later
    // pause, while absence of C4 retains E1; E4 C5 withholds the budget pair
    // until a later explicit BudgetReached pause. Decision rules B1=C1+C2=>E1;
    // B2=C1+C3=>E2; B3=B1+C4=>E3; B4=B1+!C4+!C5=>E1;
    // B5=B1+C5+!C4=>E4; B6=B1+C5+C4=>E3. Interval index and `last()`
    // are deliberately not causal evidence because reach generations and
    // Running intervals are not one-to-one.
    let usage = |tokens| awaken_session_budget_contract::ManagedBudgetUsageCursor {
        by_model: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_budget_contract::ManagedModelUsageCursor {
                input_tokens: tokens,
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let interval =
        |id: &str, tokens, close| awaken_session_state_contract::SessionRuntimeInterval {
            interval_id: id.into(),
            activity_epoch: close,
            started_at_unix_ms: 0,
            ended_at_unix_ms: close,
            opened_revision: awaken_session_types::SessionRevision(close),
            closed_revision: awaken_session_types::SessionRevision(close + 1),
            admitted_after_commit_cursor: None,
            observations: vec![
                awaken_session_state_contract::SessionRuntimeIntervalObservation {
                    activity_epoch: close,
                    thread_id: ThreadId("budget-root".into()),
                    run_id: RunId(format!("budget-run-{close}")),
                    lifecycle_cursor: lifecycle_cursor(close),
                    source_commit_cursor: close,
                },
            ],
            usage: usage(tokens),
            max_list_cost_minor: Some(1),
        };
    let transition = awaken_session_budget_contract::BudgetReachTransition {
        generation: 1,
        max_list_cost_minor: 1,
        consumed_numerator: awaken_session_budget_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_budget_contract::SessionBudgetState::COST_DENOMINATOR,
        usage_cursor: usage(5),
        price_snapshot_id: "budget-snapshot".into(),
    };
    let mut persisted = crate::state::tests::sample_persisted("budget-root");
    persisted.closed_runtime_intervals = vec![
        interval("below", 3, 10),
        interval("crossing", 5, 20),
        interval("later", 8, 30),
    ];
    assert_eq!(
        budget_reach_close_cursor(&persisted, &transition),
        Some(20),
        "B1/E1"
    );
    assert_eq!(
        budget_reach_projection_close_cursor(&persisted, &transition, &[], &Default::default()),
        Some(20),
        "B4/E1 crossing fallback without a committed budget pause"
    );
    let mut permission_pause = lifecycle(
        20,
        "budget-root",
        &RunId("budget-run-20".into()),
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    permission_pause.await_reason =
        Some(awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission);
    let historical = std::collections::HashMap::from([(
        permission_pause.cursor,
        Pending {
            tool_use_id: "approval".into(),
            name: "read".into(),
            input: serde_json::json!({"path":"source"}),
            client_executed: false,
        },
    )]);
    assert_eq!(
        budget_reach_projection_close_cursor(
            &persisted,
            &transition,
            &[permission_pause.clone()],
            &historical,
        ),
        None,
        "B5/E4 answerable crossing withholds budget pair"
    );
    let mut budget_pause = lifecycle(
        30,
        "budget-root",
        &RunId("budget-run-30".into()),
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    budget_pause.await_reason =
        Some(awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached);
    assert_eq!(
        budget_reach_projection_close_cursor(
            &persisted,
            &transition,
            &[permission_pause.clone(), budget_pause],
            &historical,
        ),
        Some(30),
        "B6/E3 the public pause, not the earlier approval interval, owns the pair"
    );
    // B7: the domain publishes exactly the exposed reaches, each counted at
    // its crossing cursor and priced by the transition's own numerator; B8:
    // a reach the prefix withholds publishes nothing.
    persisted.budget = awaken_session_budget_contract::SessionBudgetState::Active {
        max_list_cost_minor: 1,
        consumed_numerator: transition.consumed_numerator,
        usage_cursor: usage(8),
        snapshot: awaken_session_budget_contract::ManagedListPriceSnapshot {
            snapshot_id: "budget-snapshot".into(),
            version: 1,
            effective_at_unix_ms: 0,
            arithmetic_version: 1,
            model_rates: Default::default(),
            runtime_rates: Default::default(),
            fingerprint: "budget-snapshot".into(),
        },
        reach_transitions: vec![transition.clone()],
    };
    let reaches =
        awaken_managed_session_events::BudgetReach::exposed(&persisted, &[], &Default::default())
            .unwrap();
    assert_eq!(
        reaches,
        vec![awaken_managed_session_events::BudgetReach {
            generation: 1,
            usage: usage(5).to_session_usage().unwrap(),
            list_cost_minor: Some(1),
            max_list_cost_minor: 1,
        }],
        "B7 exposed reach"
    );
    let [reach_usage, reach_idle] = reaches[0].facts("budget-root");
    assert_eq!(
        (reach_usage.key.role, reach_idle.key.role),
        (
            awaken_managed_session_events::ManagedEventType::BudgetReachUsage,
            awaken_managed_session_events::ManagedEventType::BudgetReachStatusIdle,
        ),
        "B7 one usage/idle pair"
    );
    assert_eq!(
        reach_idle.kind,
        awaken_managed_session_events::SessionEventKind::SessionIdle {
            stop: awaken_managed_session_events::StopFact::BudgetReached,
            calls_owner: "budget-root".into(),
        },
        "B7 budget idle"
    );
    assert_eq!(
        awaken_managed_session_events::BudgetReach::exposed(
            &persisted,
            &[permission_pause.clone()],
            &historical,
        )
        .unwrap(),
        Vec::new(),
        "B8 withheld reach"
    );
    persisted.closed_runtime_intervals.truncate(1);
    assert_eq!(
        budget_reach_close_cursor(&persisted, &transition),
        None,
        "B2/E2"
    );
}

#[tokio::test]
async fn budget_reach_waits_for_its_interval_close_and_remains_in_every_old_cursor_suffix() {
    // Cause/effect graph: C1 the budget transition is durable while only a
    // below-threshold interval has closed; C2 callers have already received its
    // complete prefix/cursors; C3 a later interval closes with cumulative usage
    // at the transition cursor; C4 an independent ManagedState cold-rebuilds.
    // Effects: E1 C1 publishes no budget event; E2 C3 appends exactly one
    // transition-owned Usage/BudgetIdle pair at that interval's exact close,
    // without an overlapping interval-owned terminal pair or any C2 change;
    // E3 warm/cold full payloads and every C2 cursor suffix are equal. Decision table:
    // B1=C1=>E1; B2=C1+C2+C3=>E2; B3=B2+C4=>E3. An open or absent matching
    // interval is not replaced by transition index or latest-cursor fallback.
    let usage = |tokens| awaken_session_budget_contract::ManagedBudgetUsageCursor {
        by_model: std::collections::BTreeMap::from([(
            "test-model".into(),
            awaken_session_budget_contract::ManagedModelUsageCursor {
                input_tokens: tokens,
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let interval = |id: &str, tokens, open, close, thread_id: &str| {
        awaken_session_state_contract::SessionRuntimeInterval {
            interval_id: id.into(),
            activity_epoch: close,
            started_at_unix_ms: 0,
            ended_at_unix_ms: close,
            opened_revision: awaken_session_types::SessionRevision(open),
            closed_revision: awaken_session_types::SessionRevision(close),
            admitted_after_commit_cursor: None,
            observations: vec![
                awaken_session_state_contract::SessionRuntimeIntervalObservation {
                    activity_epoch: close,
                    thread_id: ThreadId(thread_id.into()),
                    run_id: RunId(format!("budget-prefix-run-{close}")),
                    lifecycle_cursor: lifecycle_cursor(close),
                    source_commit_cursor: close,
                },
            ],
            usage: usage(tokens),
            max_list_cost_minor: Some(1),
        }
    };
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo().await);
    let warm = {
        let session_fixture_runtime = runtime.clone();
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    }
    .with_managed_list_price_provider(Arc::new(ThreadPriceProvider));
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder",
                "environment_id":"env_local",
                "budget": {
                    "type":"limit",
                    "max_list_cost":{"amount":"1","currency":"USD"}
                }
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let first_run = RunId("budget-prefix-run-10".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            9,
            &session.id,
            &first_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            10,
            &session.id,
            &first_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    let mut persisted = warm
        .roles
        .roots
        .session(&SessionId::from(&session.id))
        .await
        .unwrap();
    persisted.closed_runtime_intervals = vec![interval("below", 3, 9, 10, &session.id)];
    let awaken_session_budget_contract::SessionBudgetState::Active {
        max_list_cost_minor,
        consumed_numerator,
        usage_cursor,
        ..
    } = &mut persisted.budget
    else {
        panic!("budget fixture is active")
    };
    *usage_cursor = usage(5);
    *consumed_numerator = u128::from(*max_list_cost_minor)
        * awaken_session_budget_contract::SessionBudgetState::MICROS_PER_MINOR_USD
        * awaken_session_budget_contract::SessionBudgetState::COST_DENOMINATOR;
    persisted
        .budget
        .record_reach_transition()
        .unwrap()
        .expect("B1 durable reach");
    warm.commit_session_snapshot(
        DEFAULT_SCOPE,
        persisted,
        "test-budget-prefix-below",
        Vec::new(),
    )
    .await
    .unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    let issued = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert!(
        issued.iter().all(|event| !matches!(
            &event.kind,
            OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::BudgetReached,
                ..
            }
        )),
        "B1/E1"
    );

    let second_run = RunId("budget-prefix-run-20".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            19,
            &session.id,
            &second_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            20,
            &session.id,
            &second_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    let mut persisted = warm
        .roles
        .roots
        .session(&SessionId::from(&session.id))
        .await
        .unwrap();
    persisted
        .closed_runtime_intervals
        .push(interval("crossing", 5, 19, 20, &session.id));
    warm.commit_session_snapshot(
        DEFAULT_SCOPE,
        persisted,
        "test-budget-prefix-crossing",
        Vec::new(),
    )
    .await
    .unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    let final_warm = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(&final_warm[..issued.len()], issued.as_slice(), "B2/E2");
    assert_eq!(
        final_warm
            .iter()
            .filter(|event| matches!(
                &event.kind,
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::BudgetReached,
                    ..
                }
            ))
            .count(),
        1,
        "B2/E2 one aggregate budget pause authority"
    );
    // B2/E2 wire pin: the transition's own counted usage, rounded list cost
    // and cap, and its budget idle, each published exactly once. Event ids
    // are omitted because the Session id is minted per run.
    let wire = final_warm
        .iter()
        .map(|event| {
            let mut value = serde_json::to_value(event).unwrap();
            value.as_object_mut().unwrap().remove("id");
            value
        })
        .collect::<Vec<_>>();
    for expected in [
        serde_json::json!({
            "type": "session.usage",
            "usage": {
                "active_seconds": 0, "input_tokens": 5, "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation": {"ephemeral_5m_input_tokens": 0},
                "list_cost": {"amount": "1", "currency": "USD"},
                "server_tool_use": {"web_fetch_requests": 0, "web_search_requests": 0}
            },
            "budget": {"type": "limit", "max_list_cost": {"amount": "1", "currency": "USD"}},
            "processed_at": null
        }),
        serde_json::json!({
            "type": "session.status_idle",
            "stop_reason": {"type": "budget_reached"},
            "stop_details": null,
            "processed_at": null
        }),
    ] {
        assert_eq!(
            wire.iter().filter(|value| **value == expected).count(),
            1,
            "B2/E2 byte-identical budget-reach wire {expected}"
        );
    }

    let cold = {
        let session_fixture_runtime = runtime;
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            repository,
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    }
    .with_managed_list_price_provider(Arc::new(ThreadPriceProvider));
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        cold.list_events(&session.id, None, None, false).unwrap(),
        warm.list_events(&session.id, None, None, false).unwrap(),
        "B3/E3 full payload"
    );
    for cursor in issued.iter().map(|event| event.id.as_str()) {
        assert_eq!(
            cold.list_events(&session.id, Some(cursor), Some(2), false)
                .unwrap(),
            warm.list_events(&session.id, Some(cursor), Some(2), false)
                .unwrap(),
            "B3/E3 page after {cursor}"
        );
        let warm_suffix = warm
            .list_events(&session.id, Some(cursor), None, false)
            .unwrap();
        let cold_suffix = cold
            .list_events(&session.id, Some(cursor), None, false)
            .unwrap();
        assert_eq!(cold_suffix, warm_suffix, "B3/E3 suffix after {cursor}");
        assert!(
            warm_suffix.data.iter().any(|event| matches!(
                &event.kind,
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::BudgetReached,
                    ..
                }
            )),
            "B3/E2 BudgetReached remains in the suffix after {cursor}"
        );
    }
}
