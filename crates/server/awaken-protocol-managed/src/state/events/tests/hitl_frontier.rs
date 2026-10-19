//! The root lifecycle owns primary Thread Running across Session intervals.

use super::*;
use awaken_managed_session_events::{
    AggregateStop, FactProvenance, ManagedEventType, plan_aggregate,
};

#[test]
fn aggregate_fallback_and_interval_close_require_one_prefix_plan() {
    // Causes: A1 root Running3 then audited Awaiting7 in a legacy Session
    // with no interval; A2 an alternative committed history has an interval
    // admitted before 7 and closed by that boundary. Effects: E1 A1 owns one
    // aggregate opening3 and one Usage/Idle pair keyed by lifecycle-prefix7;
    // E2 A2 retains the same opening while the interval owns the one close
    // pair; E3 neither history duplicates its close. Rules R1=A1=>E1;
    // R2=A2=>E2+E3. These frozen EventIdentity roles/provenances are inputs hashed
    // into public IDs. This fixture does not claim an interval may be added
    // retroactively after the legacy fallback close has been published.
    // The old interval-only planner cannot satisfy R1: it returns no fallback
    // facts, which exposes the duplicate PM aggregate selection owner.
    let session_id = "aggregate-one-owner";
    let run = RunId("root-run".into());
    let accepted = [
        lifecycle(
            3,
            session_id,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            7,
            session_id,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ];
    let pending = std::collections::HashMap::from([(
        accepted[1].cursor,
        Pending {
            tool_use_id: "call-7".into(),
            name: "write".into(),
            input: serde_json::json!({}),
            client_executed: false,
        },
    )]);
    let persisted = crate::state::tests::sample_persisted(session_id);
    let plan = plan_aggregate(&persisted, &accepted, &pending, &[]).unwrap();
    assert_eq!(plan.fallback_openings.len(), 1, "R1/E1 one opening");
    assert_eq!(
        plan.fallback_openings[0].role,
        ManagedEventType::AggregateStatusRunning
    );
    assert_eq!(
        plan.fallback_openings[0].provenance,
        FactProvenance::Lifecycle {
            cursor: accepted[0].cursor.0
        },
        "R1/E1 frozen opening identity"
    );
    assert_eq!(plan.fallback_closes.len(), 1, "R1/E1 one close");
    let fallback = &plan.fallback_closes[0];
    assert_eq!(fallback.usage.role, ManagedEventType::AggregateUsage);
    assert_eq!(fallback.idle.role, ManagedEventType::AggregateStatusIdle);
    assert_eq!(
        fallback.usage.provenance,
        FactProvenance::LifecyclePrefix {
            cursor: accepted[1].cursor.0
        },
        "R1/E1 frozen close identity"
    );
    assert_eq!(
        fallback.stop,
        AggregateStop::Calls {
            thread_calls: vec![(session_id.into(), "call-7".into())]
        },
        "R1/E1 answerable stop stays neutral until Managed rendering"
    );
    assert!(plan.interval_closes.is_empty(), "R1 no interval pair");

    let mut with_interval = persisted;
    with_interval.closed_runtime_intervals.push(
        awaken_session_state_contract::SessionRuntimeInterval {
            interval_id: "interval-7".into(),
            activity_epoch: 1,
            started_at_unix_ms: 0,
            ended_at_unix_ms: 7,
            opened_revision: awaken_session_types::SessionRevision(1),
            closed_revision: awaken_session_types::SessionRevision(2),
            admitted_after_commit_cursor: Some(2),
            observations: vec![
                awaken_session_state_contract::SessionRuntimeIntervalObservation {
                    activity_epoch: 1,
                    thread_id: ThreadId(session_id.into()),
                    run_id: run.clone(),
                    lifecycle_cursor: accepted[1].cursor,
                    source_commit_cursor: 7,
                },
            ],
            usage: Default::default(),
            max_list_cost_minor: None,
        },
    );
    let interval_plan = plan_aggregate(&with_interval, &accepted, &pending, &[]).unwrap();
    assert_eq!(
        interval_plan.fallback_openings, plan.fallback_openings,
        "R2/E2"
    );
    assert!(
        interval_plan.fallback_closes.is_empty(),
        "R2/E2-E3 one owner"
    );
    assert_eq!(interval_plan.interval_closes.len(), 1, "R2/E2 one close");
    let close = &interval_plan.interval_closes[0];
    assert!(matches!(&close.usage.key.provenance,
        FactProvenance::RuntimeInterval { interval_id } if interval_id == "interval-7"));
    assert_eq!(close.idle.key.provenance, close.usage.key.provenance);
}

#[test]
fn aggregate_fallback_holds_root_and_child_answerable_calls_neutrally() {
    // Causes: M1 root Running1 then child Running2; M2 root Awaiting3 while
    // child still runs; M3 child Awaiting4, both with committed call targets.
    // Effects: E1 one opening at 1 and no close at 3; E2 one close at 4 with
    // both exact (Thread, call) pairs in Thread-key order, before Managed
    // resolves and sorts public IDs;
    // E3 no duplicate aggregate period. Rules R1=M1+M2=>E1;
    // R2=M1+M2+M3=>E1+E2+E3. The adapter's existing tool index must reject a
    // missing public ID before CAS, rather than this planner guessing one.
    let session_id = "aggregate-calls";
    let root_run = RunId("root-run".into());
    let child_run = RunId("child-run".into());
    let events = [
        lifecycle(
            1,
            session_id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            "child",
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            3,
            session_id,
            &root_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
        lifecycle(
            4,
            "child",
            &child_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ];
    let pending = std::collections::HashMap::from([
        (
            events[2].cursor,
            Pending {
                tool_use_id: "root-call".into(),
                name: "write".into(),
                input: serde_json::json!({}),
                client_executed: false,
            },
        ),
        (
            events[3].cursor,
            Pending {
                tool_use_id: "child-call".into(),
                name: "read".into(),
                input: serde_json::json!({}),
                client_executed: false,
            },
        ),
    ]);
    let link = awaken_session_execution_contract::CoordinatedThreadLink {
        session_id: session_id.into(),
        thread_id: ThreadId("child".into()),
        target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
            agent_id: "worker".into(),
        },
        created_by_operation_id: "spawn-child".into(),
        latest_run_id: Some(child_run),
    };
    let persisted = crate::state::tests::sample_persisted(session_id);
    let prefix = plan_aggregate(
        &persisted,
        &events[..3],
        &pending,
        std::slice::from_ref(&link),
    )
    .unwrap();
    assert_eq!(prefix.fallback_openings.len(), 1, "R1/E1");
    assert!(prefix.fallback_closes.is_empty(), "R1/E1 child still runs");
    let full = plan_aggregate(&persisted, &events, &pending, &[link]).unwrap();
    assert_eq!(full.fallback_openings, prefix.fallback_openings, "R2/E3");
    assert_eq!(full.fallback_closes.len(), 1, "R2/E2 one close");
    assert_eq!(
        full.fallback_closes[0].stop,
        AggregateStop::Calls {
            thread_calls: vec![
                (session_id.into(), "root-call".into()),
                ("child".into(), "child-call".into()),
            ],
        },
        "R2/E2 canonical Thread-key order before public ID sort"
    );
    assert_eq!(
        full.fallback_closes[0].usage.provenance,
        FactProvenance::LifecyclePrefix {
            cursor: events[3].cursor.0
        },
        "R2/E2 frozen terminal identity"
    );
}

#[test]
fn hitl_interval_close_cannot_insert_running_before_an_issued_tool_use() {
    // Causes: H1 the root Run opened at commit 3; H2 another child closes a
    // Session interval at 4 without closing that root Run; H3 the root ToolUse
    // and answerable Awaiting commit at 7, and its interval closes later;
    // H4 a reader already issued the ToolUse at cursor 7/phase 50. Effects:
    // E1 root lifecycle emits exactly its real Running3; E2 interval planning
    // emits Usage/Idle but no second primary Running7 at phase 20; E3 H4's old
    // cursor stays a prefix on warm/cold rebuild. Decision row R1=H1-H4=>
    // E1+E2+E3. Boundary-only H1 absent is owned by the root lifecycle test's
    // synthetic Running row, not by this interval planner.
    let session_id = "hitl-root";
    let run = RunId("outcome-worker".into());
    let child_run = RunId("child-earlier".into());
    let mut awaiting = lifecycle(
        7,
        session_id,
        &run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    awaiting.await_reason =
        Some(awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission);
    let accepted = [
        lifecycle(
            3,
            session_id,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            4,
            "child-earlier",
            &child_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
        awaiting.clone(),
    ];
    let observation = |close, thread_id: &str, run_id: &RunId| {
        awaken_session_state_contract::SessionRuntimeIntervalObservation {
            activity_epoch: close,
            thread_id: ThreadId(thread_id.into()),
            run_id: run_id.clone(),
            lifecycle_cursor: lifecycle_cursor(close),
            source_commit_cursor: close,
        }
    };
    let interval = |id: &str, close, thread_id: &str, run_id: &RunId| {
        awaken_session_state_contract::SessionRuntimeInterval {
            interval_id: id.into(),
            activity_epoch: close,
            started_at_unix_ms: 0,
            ended_at_unix_ms: close,
            opened_revision: awaken_session_types::SessionRevision(close),
            closed_revision: awaken_session_types::SessionRevision(close + 1),
            admitted_after_commit_cursor: None,
            observations: vec![observation(close, thread_id, run_id)],
            usage: Default::default(),
            max_list_cost_minor: None,
        }
    };
    let mut persisted = crate::state::tests::sample_persisted(session_id);
    persisted.closed_runtime_intervals = vec![
        interval("child-close", 4, "child-earlier", &child_run),
        interval("hitl-close", 7, session_id, &run),
    ];
    let pending = std::collections::HashMap::from([(
        awaiting.cursor,
        Pending {
            tool_use_id: "outcome-write-1".into(),
            name: "write".into(),
            input: serde_json::json!({"path":"deliverable"}),
            client_executed: false,
        },
    )]);
    let opening =
        awaken_managed_session_events::root_lifecycle_opening_facts(session_id, &accepted);
    assert_eq!(
        opening.iter().map(|fact| fact.key.role).collect::<Vec<_>>(),
        [ManagedEventType::RootLifecycleStatusRunning],
        "R1/E1 real Running3 owns the Thread"
    );
    let plan = plan_aggregate(&persisted, &accepted, &pending, &[]).unwrap();
    assert!(
        !plan.interval_closes.is_empty(),
        "R1/E2 Session interval still closes with its Usage/Idle pair"
    );
    assert!(
        !plan
            .fallback_openings
            .iter()
            .any(|key| key.role == ManagedEventType::RootTerminalStatusRunning)
            && plan.interval_closes.iter().all(|close| {
                close.usage.key.role != ManagedEventType::RootTerminalStatusRunning
                    && close.idle.key.role != ManagedEventType::RootTerminalStatusRunning
            }),
        "R1/E2-E3 no phase-20 Running7 may be inserted before ToolUse7"
    );
}

#[tokio::test]
async fn late_hitl_activity_cannot_rewrite_issued_usage_or_old_cursors() {
    // Causes: H5 root Running3 and an answerable ToolUse/Awaiting7 are already
    // public; H6 a later Session activity opens with admitted_after=7, then
    // settles from that same earlier boundary; H7 an independent replica
    // rebuilds after the empty interval close. Effects: E4 H5's exact IDs,
    // payload, order and every cursor survive H6; E5 the interval records no
    // pre-admission observation or second Usage/Idle pair; E6 H7 returns the
    // same full history and every old-cursor suffix. Decision rows R2=H5+H6
    // =>E4+E5; R3=R2+H7=>E6. The committed ticket audit, not a live cache,
    // classifies the old ToolUse while Session CAS owns activity settlement.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    assert_late_hitl_frontier(repository.clone(), repository, false).await;
}

#[tokio::test]
async fn legacy_hitl_interval_then_new_activity_preserves_the_public_frontier() {
    // Causes: L1 an old closed interval has no admitted-after cursor but its
    // answerable Awaiting7 and ToolUse are committed before the first public
    // read; L2 a new-version Session activity is admitted after cursor 7 and
    // settles while observing that old boundary; L3 a cold reader rebuilds.
    // Effects: E1 the legacy close and tool have one stable public history;
    // E2 L2 cannot absorb the old boundary or replace that history; E3 L3
    // returns every old ID/payload and after-cursor suffix unchanged. Decision
    // rows L1=>E1, L1+L2=>E1+E2, L1+L2+L3=>E1+E2+E3. This is the reachable
    // mixed-version case: old lineage exists before its first public read.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(ephemeral_session_repo().await);
    assert_late_hitl_frontier(repository.clone(), repository, true).await;
}

#[tokio::test]
async fn postgres_two_connection_late_hitl_frontier_preserves_old_cursors() {
    // Causes: P1 the same accepted ToolUse/Awaiting prefix is visible through
    // one PostgreSQL connection; P2 a later Session activity is admitted and
    // settled through that connection; P3 a second independently opened
    // repository/Managed cache rebuilds from the committed database. Effects:
    // E1 the later interval cannot claim the earlier boundary; E2 the first
    // cache keeps every issued ID/payload/position; E3 the second returns the
    // same complete page and every old-cursor suffix. Rule P1+P2+P3=>E1-E3.
    // An absent URL is no PG claim; pg_tests.sh supplies a required live URL.
    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        return;
    };
    let first: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .expect("configured first PostgreSQL Session repository"),
    );
    let second: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::PostgresManagedSessionRepository::connect(&url)
            .await
            .expect("configured second PostgreSQL Session repository"),
    );
    assert_late_hitl_frontier(first, second, false).await;
}

async fn assert_late_hitl_frontier(
    first: Arc<dyn awaken_session_repository_contract::SessionStore>,
    second: Arc<dyn awaken_session_repository_contract::SessionStore>,
    legacy_closed_interval: bool,
) {
    let runtime = LifecycleRuntime::default();
    let warm = {
        let session_fixture_runtime = runtime.clone();
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            first,
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    };
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let id = session.id.as_str();
    let run = RunId("outcome-worker".into());
    let call = "outcome-write-1";
    let input = serde_json::json!({"path":"deliverable"});
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        id,
        &run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    runtime.push_committed_message(
        7,
        Message::new(
            MessageId::assistant(&run, 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(call, "write", input.clone())],
        ),
    );
    let target = awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
        reason: awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission,
        call_id: call.into(),
        tool: awaken_agent_contract::agent::awaiting::PendingTool {
            tool_id: "write".into(),
            arguments: input.clone(),
        },
        authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
    };
    let draft: awaken_agent_contract::audit::draft::Draft =
        awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
            state: RunState::Awaiting,
            await_reason: Some(awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission),
            await_target: Some(target),
        }
        .into();
    runtime
        .audit_events
        .lock()
        .unwrap()
        .push(awaken_agent_contract::audit::record::Record {
            sequence: lifecycle_cursor(7).0,
            run_id: run.clone(),
            kind: draft.kind,
            payload: draft.payload,
        });
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: call.into(),
        name: "write".into(),
        input,
        client_executed: false,
    });
    let mut awaiting = lifecycle(
        7,
        id,
        &run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    awaiting.await_reason =
        Some(awaken_agent_contract::agent::awaiting::AwaitReason::ToolPermission);
    runtime.lifecycle.lock().unwrap().push(awaiting.clone());
    if legacy_closed_interval {
        let mut persisted = warm
            .roles
            .roots
            .session(&SessionId::from(id))
            .await
            .unwrap();
        persisted.closed_runtime_intervals.push(
            awaken_session_state_contract::SessionRuntimeInterval {
                interval_id: format!("legacy-hitl-{id}"),
                activity_epoch: 1,
                started_at_unix_ms: 0,
                ended_at_unix_ms: 7,
                opened_revision: awaken_session_types::SessionRevision(1),
                closed_revision: awaken_session_types::SessionRevision(2),
                admitted_after_commit_cursor: None,
                observations: vec![
                    awaken_session_state_contract::SessionRuntimeIntervalObservation {
                        activity_epoch: 1,
                        thread_id: ThreadId(id.into()),
                        run_id: run.clone(),
                        lifecycle_cursor: awaiting.cursor,
                        source_commit_cursor: 7,
                    },
                ],
                usage: Default::default(),
                max_list_cost_minor: None,
            },
        );
        warm.commit_session_snapshot(
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
            persisted,
            "legacy-hitl-interval-before-publication",
            Vec::new(),
        )
        .await
        .unwrap();
    }
    warm.refresh_committed_events(id).await.unwrap();
    let prior = warm.list_events(id, None, None, false).unwrap().data;
    assert!(
        prior
            .iter()
            .any(|event| event.type_str() == "agent.tool_use")
    );
    assert!(
        prior
            .iter()
            .any(|event| event.type_str() == "session.usage")
    );
    let before = serde_json::to_value(&prior).unwrap();

    let admitted = warm
        .application
        .begin_activity(&SessionId::from(id))
        .await
        .unwrap();
    assert_eq!(
        admitted
            .running_interval
            .as_ref()
            .unwrap()
            .admitted_after_commit_cursor,
        Some(7),
        "R2 late Session activity cannot own prior Runtime commit"
    );
    let settled = warm
        .application
        .settle_activity_observed(
            &SessionId::from(id),
            admitted.activity_epoch,
            Some(
                awaken_session_state_contract::SessionRuntimeIntervalObservation {
                    activity_epoch: admitted.activity_epoch,
                    thread_id: ThreadId(id.into()),
                    run_id: run,
                    lifecycle_cursor: awaiting.cursor,
                    source_commit_cursor: 7,
                },
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        settled
            .closed_runtime_intervals
            .iter()
            .flat_map(|interval| interval.observations.iter())
            .count(),
        usize::from(legacy_closed_interval),
        "R2/E5 the late activity cannot record the earlier boundary"
    );
    warm.refresh_committed_events(id).await.unwrap();
    assert_eq!(
        serde_json::to_value(warm.list_events(id, None, None, false).unwrap().data).unwrap(),
        before,
        "R2/E4-E5 complete previously issued prefix"
    );

    let cold = {
        let session_fixture_runtime = runtime;
        let session_fixture_clock = awaken_time::system_clock();
        crate::state::test_support::managed_state_from_parts(
            std::sync::Arc::new(session_fixture_runtime),
            None,
            second,
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    };
    cold.refresh_committed_events(id).await.unwrap();
    assert_eq!(
        serde_json::to_value(cold.list_events(id, None, None, false).unwrap().data).unwrap(),
        before,
        "R3/E6 full cold replay"
    );
    for cursor in prior.iter().map(|event| event.id.as_str()) {
        assert_eq!(
            serde_json::to_value(cold.list_events(id, Some(cursor), None, false).unwrap()).unwrap(),
            serde_json::to_value(warm.list_events(id, Some(cursor), None, false).unwrap()).unwrap(),
            "R3/E6 old cursor {cursor}"
        );
    }
}
