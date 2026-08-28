use super::committed_projection::{
    budget_reach_close_cursor, budget_reach_projection_close_cursor,
};
use super::*;
use crate::state::test_support::{RehydrateFake, ephemeral_session_repo};
use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{
    RunLifecycleCursor, RunLifecycleEvent, RunLifecyclePage, encode_run_lifecycle_cursor,
};
use awaken_runtime_contract::tool_batch::ToolBatch;
use awaken_session_contract::{Pending, RunError, SessionRuntime, ToolPermissionDecision};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

async fn persist_batch_projection_anchors(
    state: &ManagedState,
    session_id: &str,
    batch_id: &str,
    source_commit_cursors: &[u64],
) {
    let mut session = state
        .application
        .session(session_id)
        .await
        .expect("load Session root");
    let batch = session
        .event_batches
        .iter_mut()
        .find(|batch| batch.batch_id == batch_id)
        .expect("retained test batch");
    assert_eq!(batch.events.len(), source_commit_cursors.len());
    for (entry, source_commit_cursor) in batch
        .events
        .iter_mut()
        .zip(source_commit_cursors.iter().copied())
    {
        entry.projection_anchor = Some(awaken_session_contract::SessionEventProjectionAnchor {
            source_commit_cursor,
        });
        entry.processed = true;
    }
    let owner = state.application.owner(session_id).await.expect("owner");
    state
        .application
        .commit_session_snapshot(
            &owner,
            session,
            &format!("test-projection-anchor-{batch_id}"),
            Vec::new(),
        )
        .await
        .expect("commit projection anchors");
}

#[test]
fn committed_thread_state_decides_interruptibility_for_every_child_kind() {
    // Cause/effect graph: C1 the committed disposition is Active or Archived;
    // C2 the relationship is an ordinary Agent or one-shot Advisor; C3 the
    // latest committed Run is absent, active, reusable terminal, or failed.
    // Effects: E1 Archived is never targetable; E2 a pending/active child is
    // targetable before any Managed projection; E3 ordinary Completed and
    // Cancelled Threads remain reusable; E4 ordinary Failed and every terminal
    // Advisor are absorbing. The canonical lifecycle classifier remains the
    // sole ordinary-failure owner; this helper stores no parallel status.
    //
    // | Rule | Disposition | Kind | Latest state | Effect |
    // |---|---|---|---|---|
    // | IT1 | Archived | any | any | E1 false |
    // | IT2 | Active | any | absent/Running/Awaiting | E2 true |
    // | IT3 | Active | Agent | NaturalEnd/Cancelled | E3 true |
    // | IT4 | Active | Agent | Failed class | E4 false |
    // | IT5 | Active | Advisor | any Ended | E4 false |
    let agent = CoordinatedThreadTarget::Agent {
        agent_id: "researcher".into(),
    };
    let advisor = CoordinatedThreadTarget::Advisor {
        model: "advisor-model".into(),
    };
    let active = awaken_agent_contract::ThreadDisposition::Active;
    let archived = awaken_agent_contract::ThreadDisposition::Archived;
    for target in [&agent, &advisor] {
        assert!(
            ManagedState::coordinated_thread_is_interruptible(target, active, None),
            "IT2 absent: {target:?}"
        );
        for state in [RunState::Running, RunState::Awaiting] {
            assert!(
                ManagedState::coordinated_thread_is_interruptible(target, active, Some(&state),),
                "IT2 active: {target:?} {state:?}"
            );
            assert!(
                !ManagedState::coordinated_thread_is_interruptible(target, archived, Some(&state),),
                "IT1: {target:?} {state:?}"
            );
        }
    }
    for cause in [EndCause::NaturalEnd, EndCause::Cancelled] {
        let state = RunState::Ended(cause);
        assert!(
            ManagedState::coordinated_thread_is_interruptible(&agent, active, Some(&state),),
            "IT3: {state:?}"
        );
        assert!(
            !ManagedState::coordinated_thread_is_interruptible(&advisor, active, Some(&state),),
            "IT5: {state:?}"
        );
    }
    for cause in [
        EndCause::MaxSteps,
        EndCause::Stopped("policy".into()),
        EndCause::Error(awaken_agent_contract::agent::run::Failure::StateConflict),
        EndCause::Indeterminate,
    ] {
        let state = RunState::Ended(cause);
        assert!(
            !ManagedState::coordinated_thread_is_interruptible(&agent, active, Some(&state),),
            "IT4: {state:?}"
        );
        assert!(
            !ManagedState::coordinated_thread_is_interruptible(&advisor, active, Some(&state),),
            "IT5: {state:?}"
        );
    }
}

#[tokio::test]
async fn interrupt_freezes_a_committed_child_before_the_managed_projection_catches_up() {
    // Cause/effect graph: C1 the disposable Managed cache still contains only
    // the primary Thread; C2 committed Runtime truth already contains the child
    // relationship and Running Run; C3 a selector-free interrupt is admitted.
    // Effects: E1 C1 cannot hide C2; E2 the frozen command contains primary plus
    // that exact child once; E3 no projection refresh or second target ledger is
    // required. Decision table: IC1=C1+C2+C3=>E1+E2+E3. The real Advisor E2E
    // covers the same window while the provider request is in flight.
    let runtime = crate::test_support::CoordinatedRuntimeFake::default();
    runtime.defer_child_completion();
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .expect("IC1 Session");
    SessionRuntime::run(
        &runtime,
        "coder",
        &session.id,
        vec![ContentBlock::text("coordinate")],
    )
    .await
    .expect("IC1 committed child relationship");
    assert_eq!(
        state
            .list_threads(&session.id)
            .expect("IC1 stale list")
            .len(),
        1,
        "IC1/C1 only the primary projection is visible"
    );
    let (links, snapshots) = state
        .coordinated_projection_prefix(&session.id)
        .await
        .expect("IC1 committed coordinated prefix");
    let targets = ManagedState::interrupt_targets(&session.id, None, &links, &snapshots)
        .expect("IC1 frozen targets");
    assert_eq!(
        targets,
        vec![
            SessionThreadTarget::Primary,
            SessionThreadTarget::Child(ThreadId(
                crate::test_support::CoordinatedRuntimeFake::CHILD_THREAD_ID.into(),
            )),
        ],
        "IC1/E1-E3"
    );
}

#[test]
fn durable_inbound_projection_uses_only_session_root_provenance() {
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    /* Cause/effect graph. Causes: C1 a complete root-owned User/System
     * batch is retained; C2 each entry is queued/processed; C3 projection
     * is repeated after a cold rebuild. Effects: E1 every accepted DTO is
     * reconstructed in batch ordinal order; E2 `processed_at` follows only
     * the root marker; E3 ids are stable across C3. Decision table: R1
     * C1+queued=>pending-suffix+null; R2 mark exact entries=>E1+timestamp; R3 replay
     * unchanged root=>E3. Thread transcript roles/ids deliberately do not
     * reconstruct the original public DTO. */
    let session_id = "session-durable-input";
    let batch_id = "initial:create-key";
    let mut batch = awaken_session_contract::SessionEventBatch::compile(
        session_id,
        batch_id,
        vec![
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("first")],
            },
            SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("second")],
            },
            SessionEventInput::SystemMessage {
                content: vec![ContentBlock::text("context")],
            },
        ],
    )
    .expect("R1 accepted batch");
    let queued = durable_inbound_projections(session_id, std::slice::from_ref(&batch));
    assert!(
        queued.is_empty(),
        "R1 unanchored receipts are not falsely classified as anchored history"
    );
    let pending = unanchored_inbound_projections(session_id, std::slice::from_ref(&batch));
    assert_eq!(
        pending.len(),
        3,
        "R1 pending suffix retains every accepted DTO"
    );
    assert!(
        pending
            .iter()
            .all(|projection| projection.event.processed_at.is_none()),
        "R1 pending suffix preserves the unprocessed marker"
    );
    let operation_ids = batch
        .events
        .iter()
        .map(|entry| entry.event.operation_id().to_string())
        .collect::<Vec<_>>();
    for (ordinal, operation_id) in operation_ids.iter().enumerate() {
        batch
            .mark_processed(
                operation_id,
                awaken_session_contract::SessionEventProjectionAnchor {
                    source_commit_cursor: ordinal as u64 + 1,
                },
            )
            .expect("R2 exact marker");
    }
    let committed = durable_inbound_projections(session_id, &[batch.clone()]);
    assert_eq!(
        committed
            .iter()
            .map(|projection| projection.event.type_str())
            .collect::<Vec<_>>(),
        vec!["user.message", "user.message", "system.message"],
        "R4/E4"
    );
    assert!(
        committed
            .iter()
            .all(|projection| projection.event.processed_at.is_some()),
        "R1+R2/E2"
    );
    assert_eq!(
        committed[2].event.id,
        durable_inbound_event_id(session_id, &operation_ids[2]),
        "R2/E3"
    );
    assert_eq!(
        committed
            .iter()
            .map(|projection| projection.event.id.as_str())
            .collect::<Vec<_>>(),
        durable_inbound_projections(session_id, &[batch])
            .iter()
            .map(|projection| projection.event.id.as_str())
            .collect::<Vec<_>>(),
        "R3/E3"
    );
}

#[test]
fn legacy_processed_batch_without_anchor_remains_visible_after_serde_recovery() {
    // Cause/effect decision table: C1 a pre-anchor Session row has
    // processed=true and no projection_anchor; C2 it is serialized/recovered.
    // E1 the accepted inbound remains listable with its stable legacy-prefix
    // identity; E2 no Runtime-derived anchor is invented. Rule L1=C1+C2=>E1+E2.
    // New unprocessed rows are covered by the adjacent fail-closed test and do
    // not enter this isolated migration lineage.
    let session_id = "legacy-processed-input";
    let mut batch = awaken_session_contract::SessionEventBatch::compile(
        session_id,
        "legacy-batch",
        vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("legacy accepted")],
        }],
    )
    .unwrap();
    batch.events[0].processed = true;
    let encoded = serde_json::to_vec(&batch).unwrap();
    let recovered: awaken_session_contract::SessionEventBatch =
        serde_json::from_slice(&encoded).unwrap();
    assert!(recovered.events[0].projection_anchor.is_none(), "L1/E2");
    let projection = durable_inbound_projections(session_id, &[recovered]);
    assert_eq!(projection.len(), 1, "L1/E1");
    assert_eq!(projection[0].event.type_str(), "user.message", "L1/E1");
}

#[tokio::test]
async fn legacy_inbound_cursor_precedes_later_anchored_batches_warm_and_cold() {
    // Cause/effect graph: C1 a pre-anchor processed User entry is already
    // listable and its cursor issued; C2 a new batch later receives the exact
    // Runtime projection anchor under root CAS; C3 a second ManagedState starts
    // cold. Effects: E1 C1 remains the immutable legacy prefix; E2 C2 appends
    // after it; E3 full payload and the Page after every C1 cursor, including
    // next_page, are equal warm/cold. Rules I1=C1=>E1; I2=C1+C2=>E1+E2;
    // I3=I2+C3=>E3. Legacy source zero is isolated lineage, not a fallback for
    // any new unanchored command.
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let mut persisted = warm.application.session(&session.id).await.unwrap();
    let mut legacy = awaken_session_contract::SessionEventBatch::compile(
        &session.id,
        "legacy-inbound",
        vec![SessionEventInput::UserMessage {
            content: vec![ContentBlock::text("legacy")],
        }],
    )
    .unwrap();
    legacy.events[0].processed = true;
    persisted.event_batches.push(legacy);
    warm.commit_session_snapshot(
        DEFAULT_SCOPE,
        persisted,
        "test-legacy-inbound-prefix",
        Vec::new(),
    )
    .await
    .unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    let issued = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(issued.len(), 1, "I1/E1");

    let later = warm
        .application
        .append_session_event_batch(
            &session.id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("later")],
            }],
            None,
            None,
        )
        .await
        .unwrap();
    persist_batch_projection_anchors(&warm, &session.id, &later.batch_id, &[10]).await;
    warm.refresh_committed_events(&session.id).await.unwrap();
    let final_warm = warm.list_events(&session.id, None, None, false).unwrap();
    assert_eq!(&final_warm.data[..issued.len()], issued.as_slice(), "I2/E1");
    assert_eq!(final_warm.data.len(), 2, "I2/E2");

    let cold = ManagedState::new(runtime).with_session_repo(repository);
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        cold.list_events(&session.id, None, None, false).unwrap(),
        final_warm,
        "I3/E3 full payload"
    );
    for cursor in issued.iter().map(|event| event.id.as_str()) {
        assert_eq!(
            cold.list_events(&session.id, Some(cursor), Some(1), false)
                .unwrap(),
            warm.list_events(&session.id, Some(cursor), Some(1), false)
                .unwrap(),
            "I3/E3 page after {cursor}"
        );
        let warm_suffix = warm
            .list_events(&session.id, Some(cursor), None, false)
            .unwrap();
        let cold_suffix = cold
            .list_events(&session.id, Some(cursor), None, false)
            .unwrap();
        assert_eq!(cold_suffix, warm_suffix, "I3/E3 suffix after {cursor}");
        assert!(
            warm_suffix
                .data
                .iter()
                .any(|event| event.type_str() == "user.message"),
            "I3/E2 later anchored input remains in the suffix after {cursor}"
        );
    }
}

#[test]
fn budget_reach_uses_the_first_closed_interval_whose_cumulative_usage_crossed_it() {
    // Cause/effect graph: C1 interval one closes below a transition cursor; C2
    // interval two closes at/above it; C3 the matching interval is still open
    // or absent; C4 tool approval owns C2 and an equal-or-later interval closes
    // on committed BudgetReached. E1 the accounting crossing is interval two;
    // E2 C3 yields None; E3 C4 moves the one public budget pair to the later
    // pause, while absence of C4 retains E1. Decision rules B1=C1+C2=>E1;
    // B2=C1+C3=>E2; B3=B1+C4=>E3; B4=B1+!C4=>E1. Interval index and `last()`
    // are deliberately not causal evidence because reach generations and
    // Running intervals are not one-to-one.
    let usage = |tokens| awaken_session_contract::ManagedBudgetUsageCursor {
        by_model: std::collections::BTreeMap::from([(
            "model".into(),
            awaken_session_contract::ManagedModelUsageCursor {
                input_tokens: tokens,
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let interval = |id: &str, tokens, close| awaken_session_contract::SessionRuntimeInterval {
        interval_id: id.into(),
        activity_epoch: close,
        started_at_unix_ms: 0,
        ended_at_unix_ms: close,
        opened_revision: awaken_session_contract::SessionRevision(close),
        closed_revision: awaken_session_contract::SessionRevision(close + 1),
        observations: vec![awaken_session_contract::SessionRuntimeIntervalObservation {
            activity_epoch: close,
            thread_id: ThreadId("budget-root".into()),
            run_id: RunId(format!("budget-run-{close}")),
            lifecycle_cursor: lifecycle_cursor(close),
            source_commit_cursor: close,
        }],
        usage: usage(tokens),
        max_list_cost_minor: Some(1),
    };
    let transition = awaken_session_contract::BudgetReachTransition {
        generation: 1,
        max_list_cost_minor: 1,
        consumed_numerator: awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
            * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR,
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
        budget_reach_projection_close_cursor(&persisted, &transition, &[]),
        Some(20),
        "B4/E1 crossing fallback without a committed budget pause"
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
        budget_reach_projection_close_cursor(&persisted, &transition, &[budget_pause]),
        Some(30),
        "B3/E3 the public pause, not the earlier approval interval, owns the pair"
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
    let usage = |tokens| awaken_session_contract::ManagedBudgetUsageCursor {
        by_model: std::collections::BTreeMap::from([(
            "test-model".into(),
            awaken_session_contract::ManagedModelUsageCursor {
                input_tokens: tokens,
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let interval = |id: &str, tokens, open, close, thread_id: &str| {
        awaken_session_contract::SessionRuntimeInterval {
            interval_id: id.into(),
            activity_epoch: close,
            started_at_unix_ms: 0,
            ended_at_unix_ms: close,
            opened_revision: awaken_session_contract::SessionRevision(open),
            closed_revision: awaken_session_contract::SessionRevision(close),
            observations: vec![awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: close,
                thread_id: ThreadId(thread_id.into()),
                run_id: RunId(format!("budget-prefix-run-{close}")),
                lifecycle_cursor: lifecycle_cursor(close),
                source_commit_cursor: close,
            }],
            usage: usage(tokens),
            max_list_cost_minor: Some(1),
        }
    };
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone())
        .with_session_repo(repository.clone())
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
            None,
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
    let mut persisted = warm.application.session(&session.id).await.unwrap();
    persisted.closed_runtime_intervals = vec![interval("below", 3, 9, 10, &session.id)];
    let awaken_session_contract::SessionBudgetState::Active {
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
        * awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
        * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR;
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
                stop_reason: StopReason::BudgetReached
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
    let mut persisted = warm.application.session(&session.id).await.unwrap();
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
                    stop_reason: StopReason::BudgetReached
                }
            ))
            .count(),
        1,
        "B2/E2 one aggregate budget pause authority"
    );

    let cold = ManagedState::new(runtime)
        .with_session_repo(repository)
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
                    stop_reason: StopReason::BudgetReached
                }
            )),
            "B3/E2 BudgetReached remains in the suffix after {cursor}"
        );
    }
}

struct AdvisorProfile;

#[derive(Clone, Copy)]
enum FrozenTestToolFamily {
    Custom,
    AgentAlwaysAllow,
    AgentAlwaysAsk,
}

/// One config-plane fixture for tests that need to prove Managed tool-event
/// family selection. The Session/child Agent snapshots remain the only
/// classification input; Runtime capability fields are deliberately not a
/// parallel declaration source.
struct FrozenToolFamilyProfiles {
    profiles: std::collections::HashMap<
        String,
        awaken_executable_agent_contract::ExecutableAgentSessionProfile,
    >,
}

impl FrozenToolFamilyProfiles {
    fn native_roster(root_agent_id: &str, delegates: &[&str]) -> Self {
        let mut profiles = std::collections::HashMap::new();
        for agent_id in std::iter::once(root_agent_id).chain(delegates.iter().copied()) {
            let mut profile = awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                name: Some(agent_id.to_string()),
                source_revision: 1,
                model: Some("test-model".into()),
                execution_model_ref: Some("test-model".into()),
                backend_ref: "native".into(),
                ..Default::default()
            };
            if agent_id == root_agent_id {
                profile.delegates = delegates
                    .iter()
                    .map(
                        |delegate| awaken_executable_agent_contract::ExecutableAgentDelegate {
                            agent_id: (*delegate).to_string(),
                            source_revision: Some(1),
                        },
                    )
                    .collect();
            }
            profiles.insert(agent_id.to_string(), profile);
        }
        Self { profiles }
    }

    fn uniform(
        root_agent_id: &str,
        delegates: &[&str],
        family: FrozenTestToolFamily,
        tool_name: &str,
    ) -> Self {
        let mut fixture = Self::native_roster(root_agent_id, delegates);
        for profile in fixture.profiles.values_mut() {
            match family {
                FrozenTestToolFamily::Custom => {
                    profile.client_tools = vec![awaken_agent_contract::ClientToolDescriptor {
                        name: tool_name.to_string(),
                        description: format!("test client tool {tool_name}"),
                        input_schema: serde_json::json!({"type":"object"}),
                    }];
                }
                FrozenTestToolFamily::AgentAlwaysAllow | FrozenTestToolFamily::AgentAlwaysAsk => {
                    profile.toolsets = vec![awaken_agent_contract::ToolsetPolicy {
                        source: awaken_agent_contract::ToolsetSource::Agent,
                        default: awaken_agent_contract::ToolExecutionPolicy {
                            enabled: true,
                            permission: match family {
                                FrozenTestToolFamily::AgentAlwaysAsk => {
                                    awaken_agent_contract::ToolPermissionRequirement::AlwaysAsk
                                }
                                FrozenTestToolFamily::AgentAlwaysAllow => {
                                    awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow
                                }
                                FrozenTestToolFamily::Custom => unreachable!(),
                            },
                        },
                        overrides: Vec::new(),
                    }];
                }
            }
        }
        fixture
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for FrozenToolFamilyProfiles {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.profiles.get(agent_id).cloned()
    }

    fn session_profile_at_revision_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.profiles
            .get(agent_id)
            .filter(|profile| profile.source_revision == source_revision)
            .cloned()
    }

    fn executable_snapshot_at_revision_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        let profile = self
            .profiles
            .get(agent_id)
            .filter(|profile| profile.source_revision == source_revision)?;
        let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder(agent_id)
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                agent_id,
                profile.execution_model_ref.as_deref()?,
                &profile.backend_ref,
            ))
            .build();
        snapshot.metadata.source.revision = source_revision;
        Some(snapshot)
    }
}

struct ThreadPriceProvider;

#[async_trait]
impl awaken_session_contract::ManagedListPriceProvider for ThreadPriceProvider {
    async fn resolve_snapshot(
        &self,
        request: awaken_session_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_contract::ManagedListPriceSnapshot,
        awaken_session_contract::ManagedListPriceError,
    > {
        let rate = awaken_session_contract::ManagedTokenListRates {
            input_micros_per_million: 10_000_000_000,
            output_micros_per_million: 10_000_000_000,
            cache_read_micros_per_million: 10_000_000_000,
            cache_creation_micros_per_million: 10_000_000_000,
        };
        let mut model_rates = request
            .model_refs
            .into_iter()
            .map(|model| (model, rate))
            .collect::<std::collections::BTreeMap<_, _>>();
        model_rates.insert("served".into(), rate);
        Ok(awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "thread-price-test".into(),
            version: 1,
            effective_at_unix_ms: request.occurred_at_unix_ms,
            arithmetic_version: 1,
            model_rates,
            runtime_rates: Default::default(),
            fingerprint: "thread-price-fingerprint".into(),
        })
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for AdvisorProfile {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        (agent_id == "coder").then(|| {
            awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                name: Some("Coder".into()),
                source_revision: 1,
                model: Some("primary-model".into()),
                execution_model_ref: Some("primary-model".into()),
                backend_ref: "native".into(),
                advisor_model: Some("claude-opus-5".into()),
                ..Default::default()
            }
        })
    }
}

#[derive(Clone, Default)]
struct LifecycleRuntime {
    messages: Arc<Mutex<Vec<Message>>>,
    message_cursors: Arc<Mutex<Vec<u64>>>,
    audit_events: Arc<Mutex<Vec<awaken_agent_contract::audit::record::Record>>>,
    lifecycle: Arc<Mutex<Vec<RunLifecycleEvent>>>,
    state: Arc<Mutex<Vec<awaken_agent_contract::agent::state::Command>>>,
    state_cursors: Arc<Mutex<Vec<u64>>>,
    outcome_projections: Arc<
        Mutex<
            std::collections::HashMap<String, awaken_session_contract::CommittedOutcomeProjection>,
        >,
    >,
    /// Causal test hook: expose one complete Outcome commit when its terminal
    /// projection is observed, modeling a commit between the projection and
    /// root-snapshot reads without manufacturing a partial transaction.
    outcome_commit_on_read: Arc<Mutex<Vec<(awaken_agent_contract::agent::state::Command, u64)>>>,
    include_runs_in_snapshot: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<Pending>>>,
    snapshot_reads: Arc<AtomicUsize>,
    split_message_reads: Arc<AtomicUsize>,
    split_pending_reads: Arc<AtomicUsize>,
}

impl LifecycleRuntime {
    /// Install one atomic committed-message prefix with the exact Runtime
    /// commit coordinates consumed by the production projector. Keeping the
    /// two vectors together prevents a test from inventing a transcript that
    /// no production recovery backend can return.
    fn replace_committed_messages(&self, entries: Vec<(u64, Message)>) {
        let (cursors, messages): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
        *self.messages.lock().unwrap() = messages;
        *self.message_cursors.lock().unwrap() = cursors;
    }

    fn push_committed_message(&self, cursor: u64, message: Message) {
        let mut messages = self.messages.lock().unwrap();
        let mut cursors = self.message_cursors.lock().unwrap();
        assert_eq!(
            messages.len(),
            cursors.len(),
            "fixture transcript and commit coordinates must stay aligned"
        );
        messages.push(message);
        cursors.push(cursor);
    }
}

#[async_trait]
impl SessionRuntime for LifecycleRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        crate::test_support::complete_session_projection_init(thread, &projection, &mode)?;
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn committed_messages(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        self.split_message_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.messages.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.snapshot_reads.fetch_add(1, Ordering::SeqCst);
        let lifecycle = self.lifecycle.lock().unwrap().clone();
        let Some(run_id) = lifecycle
            .iter()
            .rev()
            .find(|event| event.thread_id.0 == thread_id)
            .map(|event| event.run_id.clone())
        else {
            return Ok(None);
        };
        let resume_tickets = self
            .pending
            .lock()
            .unwrap()
            .clone()
            .map(
                |pending| awaken_agent_contract::thread::read::recovery::RunResumeTicket {
                    run_id: run_id.clone(),
                    ticket: awaken_agent_contract::agent::awaiting::ResumeTicket::new(
                        format!("ticket:{}", pending.tool_use_id),
                        run_id.clone(),
                        ThreadId(thread_id.to_string()),
                        "test-snapshot",
                        "test-catalog",
                        awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
                            reason: if pending.client_executed {
                                awaken_agent_contract::agent::awaiting::ToolAwaitReason::ClientExecution
                            } else {
                                awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission
                            },
                            call_id: pending.tool_use_id,
                            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                                tool_id: pending.name,
                                arguments: pending.input,
                            },
                        },
                    ),
                },
            )
            .into_iter()
            .collect();
        let runs = if self.include_runs_in_snapshot.load(Ordering::SeqCst) {
            let mut runs = Vec::<awaken_agent_contract::agent::run::Record>::new();
            for event in lifecycle
                .iter()
                .filter(|event| event.thread_id.0 == thread_id)
            {
                if let Some(run) = runs.iter_mut().find(|run| run.id == event.run_id) {
                    run.state = event.state.clone();
                } else {
                    runs.push(awaken_agent_contract::agent::run::Record {
                        id: event.run_id.clone(),
                        thread_id: event.thread_id.clone(),
                        state: event.state.clone(),
                    });
                }
            }
            runs
        } else {
            Vec::new()
        };
        let messages = self.messages.lock().unwrap().clone();
        let message_commit_cursors = self.message_cursors.lock().unwrap().clone();
        let state = self.state.lock().unwrap().clone();
        let state_commit_cursors = self.state_cursors.lock().unwrap().clone();
        let store_cursor = lifecycle
            .iter()
            .map(|event| event.source_commit_cursor)
            .chain(message_commit_cursors.iter().copied())
            .chain(state_commit_cursors.iter().copied())
            .max()
            .unwrap_or_default();
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                thread_id: ThreadId(thread_id.to_string()),
                claimed_run_id: run_id.clone(),
                runs,
                latest_run_id: Some(run_id),
                messages,
                message_commit_cursors,
                state,
                state_commit_cursors,
                events: self.audit_events.lock().unwrap().clone(),
                resume_tickets,
                thread_version: lifecycle.len() as u64,
                store_cursor,
                next_commit_ordinal: 0,
            },
        ))
    }

    async fn committed_run_lifecycle(
        &self,
        _thread: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        let events = self
            .lifecycle
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        Ok(RunLifecyclePage {
            next_cursor: events.last().map_or(cursor, |event| event.cursor),
            events,
        })
    }

    async fn pending_tool(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        self.split_pending_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.pending.lock().unwrap().clone())
    }

    async fn committed_outcome_projection(
        &self,
        _thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_contract::CommittedOutcomeProjection>, RunError> {
        let projection = self
            .outcome_projections
            .lock()
            .unwrap()
            .get(outcome_id)
            .cloned();
        if projection.is_some() {
            let committed = std::mem::take(&mut *self.outcome_commit_on_read.lock().unwrap());
            let (commands, cursors): (Vec<_>, Vec<_>) = committed.into_iter().unzip();
            self.state.lock().unwrap().extend(commands);
            self.state_cursors.lock().unwrap().extend(cursors);
        }
        Ok(projection)
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        unreachable!()
    }

    fn model(&self) -> String {
        "test-model".into()
    }
}

fn lifecycle_cursor(source_commit_cursor: u64) -> RunLifecycleCursor {
    encode_run_lifecycle_cursor(source_commit_cursor, 0)
        .expect("test lifecycle commit cursor must be representable")
}

fn lifecycle(
    source_commit_cursor: u64,
    thread: &str,
    run_id: &RunId,
    kind: RunLifecycleEventKind,
    state: RunState,
) -> RunLifecycleEvent {
    RunLifecycleEvent {
        cursor: lifecycle_cursor(source_commit_cursor),
        source_commit_cursor,
        thread_id: ThreadId(thread.into()),
        run_id: run_id.clone(),
        kind,
        state,
        await_reason: None,
    }
}

#[test]
fn tool_reply_freezes_the_latest_awaiting_commit_inside_its_recovery_fence() {
    // Cause/effect graph: C1 lifecycle contains older/current/future Awaiting
    // commits for one Run; C2 another Thread or Run has an Awaiting commit; C3
    // a legacy event has no source cursor; C4 the fenced prefix has no matching
    // Awaiting. Effects: E1 select the latest same-Thread/same-Run commit no
    // later than the snapshot fence; E2 ignore C2/C3; E3 C4 remains retryable
    // and cannot mint an ordering anchor. Decision table: R1=C1+C2+C3=>E1+E2;
    // R2=C4=>E3. The lifecycle feed remains the sole commit-coordinate owner.
    let thread = ThreadId("thread-reply-order".into());
    let run = RunId("run-reply-order".into());
    let events = vec![
        lifecycle(
            10,
            &thread.0,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
        lifecycle(
            30,
            &thread.0,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
        lifecycle(
            40,
            &thread.0,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
        lifecycle(
            25,
            "other-thread",
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ];
    assert_eq!(
        ManagedState::answered_pending_commit_cursor(&thread, &run, 35, &events).unwrap(),
        30,
        "R1/E1-E2"
    );
    assert_eq!(
        ManagedState::answered_pending_commit_cursor(&thread, &run, 20, &events).unwrap(),
        10,
        "R1/E1 older fence"
    );
    assert!(
        ManagedState::answered_pending_commit_cursor(
            &thread,
            &RunId("missing-run".into()),
            35,
            &events,
        )
        .is_err(),
        "R2/E3"
    );
}

#[tokio::test]
async fn recovery_pending_projection_enforces_one_current_run_ticket() {
    // Causes: the fixtures below establish `recovery pending projection enforces one current run
    // ticket` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1=the latest Runtime Run has zero, one, or two
    // committed ResumeTickets; C2=the Awaiting audit retains the exact closed
    // target independently of the consumable ticket. Effects: E1=zero with no
    // target projects no pending but strict reply admission rejects the damaged
    // wait; E2=one projects that exact pending; E3=two fail closed for replies,
    // while list projection rebuilds the same pending from C2. This records the
    // separation between reply authority and historical/public projection.
    // Decision table:
    // | Rule | Current-Run tickets | Effect |
    // | P1 | 0, no audit target | lenient E1; strict reject |
    // | P2 | 1 | E2 |
    // | P3 | 2, exact audit target | strict E3; lenient exact rebuild |
    let runtime = LifecycleRuntime::default();
    runtime
        .include_runs_in_snapshot
        .store(true, Ordering::SeqCst);
    let thread_id = "sthr-single-ticket";
    let run_id = RunId("run-single-ticket".into());
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        thread_id,
        &run_id,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    ));

    let empty = runtime
        .session_thread_recovery_snapshot("sesn-single-ticket", thread_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        ManagedState::pending_from_recovery_snapshot(&empty)
            .unwrap()
            .is_none(),
        "P1/E1"
    );
    assert!(
        ManagedState::pending_ticket_from_recovery_snapshot(&empty).is_err(),
        "P1 strict admission rejects an Awaiting Run without reply authority"
    );

    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: "call-single-ticket".into(),
        name: "client_lookup".into(),
        input: serde_json::json!({}),
        client_executed: true,
    });
    let mut one = runtime
        .session_thread_recovery_snapshot("sesn-single-ticket", thread_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ManagedState::pending_from_recovery_snapshot(&one)
            .unwrap()
            .unwrap()
            .tool_use_id,
        "call-single-ticket",
        "P2/E2"
    );

    one.events
        .push(awaken_agent_contract::audit::record::Record {
            sequence: 1,
            run_id: run_id.clone(),
            kind: awaken_agent_contract::audit::kind::Kind::RunStateChanged,
            payload: serde_json::json!({
                "state": RunState::Awaiting,
                "await_target": one.resume_tickets[0].ticket.target().clone(),
            }),
        });
    one.resume_tickets.push(one.resume_tickets[0].clone());
    assert!(
        ManagedState::pending_ticket_from_recovery_snapshot(&one).is_err(),
        "P3/E3 strict reply admission"
    );
    assert_eq!(
        ManagedState::pending_from_recovery_snapshot(&one)
            .unwrap()
            .unwrap()
            .tool_use_id,
        "call-single-ticket",
        "P3/C2 list projection reconstructs without choosing a duplicate ticket"
    );
}

#[tokio::test]
async fn pure_interrupt_bypasses_only_damaged_reply_authority() {
    // Cause/effect graph: C1=the latest Run is durably Awaiting; C2=its active
    // ResumeTicket is isolated-missing; C3=RunStateChanged retains the exact
    // AwaitTarget; C4=batch is pure user.interrupt, an ordinary message/reply,
    // or a mixed interrupt batch.
    // Effects: E1=list projection rebuilds the pending payload from C3; E2=pure
    // interrupt freezes the canonical target without reading reply authority;
    // E3=ordinary input remains fail-closed and cannot start a competing Run.
    //
    // | Rule | Awaiting | Ticket | Audit target | Input | Effect |
    // | DI1 | yes | missing | exact | list | E1 |
    // | DI2 | yes | missing | exact | pure interrupt | E2 |
    // | DI3 | yes | missing | exact | user message/reply | E3 |
    // | DI4 | yes | missing | exact | interrupt + message | E3 |
    // Constraint: only the control command bypasses ticket correlation; mixed
    // batches and every reply still use strict ResumeTicket admission.
    let runtime = LifecycleRuntime::default();
    runtime
        .include_runs_in_snapshot
        .store(true, Ordering::SeqCst);
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .expect("DI fixture Session");
    let run_id = RunId("run-damaged-interrupt".into());
    let call_id = "call-damaged-interrupt";
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    ));
    runtime
        .audit_events
        .lock()
        .unwrap()
        .push(awaken_agent_contract::audit::record::Record {
            sequence: lifecycle_cursor(1).0,
            run_id: run_id.clone(),
            kind: awaken_agent_contract::audit::kind::Kind::RunStateChanged,
            payload: serde_json::json!({
                "state": RunState::Awaiting,
                "await_target": awaken_agent_contract::agent::awaiting::AwaitTarget::RemoteInput {
                    reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
                    call_id: call_id.to_string(),
                },
            }),
        });

    let snapshot = runtime
        .session_thread_recovery_snapshot(&session.id, &session.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ManagedState::pending_from_recovery_snapshot(&snapshot)
            .unwrap()
            .unwrap()
            .tool_use_id,
        call_id,
        "DI1/E1"
    );
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("DI1 list projection remains available");
    assert!(
        state
            .list_events(&session.id, None, None, false)
            .expect("DI1 public list does not inherit ticket corruption")
            .data
            .iter()
            .any(|event| {
                decode_managed_tool_event_id(&event.id)
                    .is_some_and(|identity| identity.call_id == call_id)
            }),
        "DI1/E1 public pending projection"
    );
    let interrupt = state
        .validate_event_batch(
            &session.id,
            &[InboundEvent::UserInterrupt {
                session_thread_id: None,
            }],
        )
        .await
        .expect("DI2 pure interrupt remains admissible");
    assert!(
        matches!(
            interrupt.inputs.as_slice(),
            [SessionEventInput::Interrupt(SessionEventInterrupt { targets, .. })]
                if targets == &[SessionThreadTarget::Primary]
        ),
        "DI2/E2"
    );
    assert!(
        state
            .validate_event_batch(
                &session.id,
                &[InboundEvent::UserMessage {
                    content: vec![ContentBlock::text("must not bypass the damaged wait")],
                }],
            )
            .await
            .is_err(),
        "DI3/E3"
    );
    assert!(
        state
            .validate_event_batch(
                &session.id,
                &[InboundEvent::UserCustomToolResult {
                    custom_tool_use_id: call_id.into(),
                    content: Some(vec![ContentBlock::text("must retain ticket correlation")]),
                    is_error: false,
                }],
            )
            .await
            .is_err(),
        "DI3/E3 reply"
    );
    assert!(
        state
            .validate_event_batch(
                &session.id,
                &[
                    InboundEvent::UserInterrupt {
                        session_thread_id: None,
                    },
                    InboundEvent::UserMessage {
                        content: vec![ContentBlock::text("mixed batches remain strict")],
                    },
                ],
            )
            .await
            .is_err(),
        "DI4/E3"
    );
}

#[tokio::test]
async fn pending_without_transcript_uses_awaiting_lifecycle_anchor() {
    // Synthetic pending projection cause/effect graph: C1 one Runtime
    // ResumeTicket exists; C2 its exact Run has a committed Awaiting lifecycle
    // fact; C3 no ToolUse transcript message exists. Effects: E1 publish one
    // qualified client-answerable tool event; E2 canonical ordering uses C2 and
    // does not reject the event as unanchored. Decision table: S1 C1+C2+C3 =>
    // E1+E2. The converse C1+!C2 remains covered by the global unanchored-event
    // fail-closed guard and must not acquire a guessed cursor.
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone()).with_config_source(Arc::new(
        FrozenToolFamilyProfiles::uniform(
            "coder",
            &[],
            FrozenTestToolFamily::Custom,
            "client_lookup",
        ),
    ));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let run_id = RunId("run-synthetic-pending".into());
    let call_id = "call-synthetic-pending";
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: call_id.into(),
        name: "agent_input".into(),
        input: serde_json::json!({"reason":"input_required"}),
        client_executed: true,
    });
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            10,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            20,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);
    runtime
        .audit_events
        .lock()
        .unwrap()
        .push(awaken_agent_contract::audit::record::Record {
            sequence: lifecycle_cursor(20).0,
            run_id: run_id.clone(),
            kind: awaken_agent_contract::audit::kind::Kind::RunStateChanged,
            payload: serde_json::json!({
                "state": RunState::Awaiting,
                "await_target": awaken_agent_contract::agent::awaiting::AwaitTarget::RemoteInput {
                    reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
                    call_id: call_id.to_string(),
                },
            }),
        });

    state.refresh_committed_events(&session.id).await.unwrap();
    let tools = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .filter(|event| event.type_str() == "agent.tool_use")
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1, "S1/E1 one synthetic pending tool");
    let identity = decode_managed_tool_event_id(&tools[0].id).expect("S1/E2 qualified id");
    assert_eq!(identity.source_id, run_id.0, "S1/E2 Awaiting Run owner");
    assert_eq!(identity.call_id, call_id, "S1/E1 exact Runtime call");

    // S2: consuming the active ticket cannot erase the historical coordinate
    // of the already-public tool event. The Awaiting audit fact remains its one
    // anchor through resume and terminal projection.
    *runtime.pending.lock().unwrap() = None;
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            30,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Resumed,
            RunState::Running,
        ),
        lifecycle(
            40,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    state.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.id == tools[0].id)
            .count(),
        1,
        "S2 historical Awaiting anchor retains one public tool event"
    );
}

#[tokio::test]
async fn root_projection_and_reply_admission_share_one_recovery_prefix() {
    // Causes: the fixtures below establish `root projection and reply admission share one recovery
    // prefix` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C0 the frozen root Agent declares `client_lookup`
    // as a custom client tool; C1 the root recovery snapshot contains
    // transcript, Awaiting lifecycle watermark, and one ResumeTicket; C2
    // the legacy message/pending query ports also exist; C3 projection or
    // reply-batch admission reads root truth. Effects: E1 C0+C1+C3 projects
    // and resolves the exact qualified custom-tool Event; E2 C2 is never
    // consulted, preventing a commit between split reads from constructing
    // a nonexistent prefix. Decision table: S1=C0+C1+projection=>E1+E2;
    // S2=C0+C1+reply admission=>E1+E2.
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone()).with_config_source(Arc::new(
        FrozenToolFamilyProfiles::uniform(
            "coder",
            &[],
            FrozenTestToolFamily::Custom,
            "client_lookup",
        ),
    ));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let run_id = RunId("run-root-one-prefix".into());
    let call_id = "call-root-one-prefix";
    runtime.push_committed_message(
        2,
        Message {
            id: MessageId("message-root-one-prefix".into()),
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                call_id,
                "client_lookup",
                serde_json::json!({"query":"one prefix"}),
            )],
        },
    );
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: call_id.into(),
        name: "client_lookup".into(),
        input: serde_json::json!({"query":"one prefix"}),
        client_executed: true,
    });
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);

    let split_before = (
        runtime.split_message_reads.load(Ordering::SeqCst),
        runtime.split_pending_reads.load(Ordering::SeqCst),
    );
    let snapshots_before = runtime.snapshot_reads.load(Ordering::SeqCst);
    state.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        (
            runtime.split_message_reads.load(Ordering::SeqCst),
            runtime.split_pending_reads.load(Ordering::SeqCst),
        ),
        split_before,
        "S1/E2"
    );
    assert!(
        runtime.snapshot_reads.load(Ordering::SeqCst) > snapshots_before,
        "S1/E1"
    );
    let public_event_id = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .find(|event| event.type_str() == "agent.custom_tool_use")
        .expect("S1/E1 projected custom tool Event")
        .id;

    let split_before = (
        runtime.split_message_reads.load(Ordering::SeqCst),
        runtime.split_pending_reads.load(Ordering::SeqCst),
    );
    let snapshots_before = runtime.snapshot_reads.load(Ordering::SeqCst);
    let validated = state
        .validate_event_batch(
            &session.id,
            &[InboundEvent::UserCustomToolResult {
                custom_tool_use_id: public_event_id,
                content: Some(vec![ContentBlock::text("resolved")]),
                is_error: false,
            }],
        )
        .await
        .unwrap();
    assert!(
        matches!(
            validated.inputs.first(),
            Some(SessionEventInput::ToolReply(reply))
                if reply.target == SessionThreadTarget::Primary
                    && reply.runtime_tool_use_id == call_id
        ),
        "S2/E1"
    );
    assert_eq!(
        (
            runtime.split_message_reads.load(Ordering::SeqCst),
            runtime.split_pending_reads.load(Ordering::SeqCst),
        ),
        split_before,
        "S2/E2"
    );
    assert!(
        runtime.snapshot_reads.load(Ordering::SeqCst) > snapshots_before,
        "S2/E1"
    );
}

#[test]
fn tool_reply_target_resolution_decision_table() {
    // Causes: the fixtures below establish `tool reply target resolution decision table` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Resolver-kernel test: candidates are constructed directly to prove
    // exact identity consumption independently of today’s singular Runtime
    // ticket producer. The production constraint is tested immediately
    // above; this test is not a same-Thread multi-pending integration claim.
    // Cause/effect graph: C1=EventId is qualified or legacy; C2=its embedded
    // owner is primary/child; C3=the committed pending call and reply kind
    // match; C4=a legacy call/type pair has one or multiple pending owners;
    // C5=the qualified source coordinate is current or stale; C6=one owner
    // has one or multiple distinct pending calls; C7=the committed public
    // tool-use family is Agent/Custom/MCP. Effects:
    // E1=return one canonical primary/child target plus Runtime call id and
    // derive primary activity ownership from that same target;
    // E2=reject before persistence; E3=distinct calls on one owner resolve
    // independently in either order; E4=a duplicate call is rejected and a
    // partial reply leaves the other call unresolved. The qualified Event id
    // is the sole public routing authority; the current projected id fences
    // call reuse. Legacy raw ids are accepted only for upgrade recovery when
    // committed state proves one unique owner.
    // Decision table:
    // | Rule | Id | Owner | C3 | Legacy owners | Source | Effect |
    // | R1 | qualified | child | T | - | current | E1 child |
    // | R2 | qualified | root | T | - | current | E1 root |
    // | R3 | legacy | root/child | T | one | - | E1 owner |
    // | R4 | legacy | any | T | many | - | E2 |
    // | R5 | either | any | F | any | any | E2 |
    // | R6 | qualified | any | T | - | stale | E2 |
    // | R7 | qualified | same owner, two calls | T | - | current | E3 |
    // | R8 | qualified | same owner, duplicate/partial | T | - | current | E4 |
    // | R9 | qualified | any | reply family differs from C7 | - | current | E2 |
    let session_id = "sesn-route";
    let primary_thread_id = public_thread_id(session_id, session_id);
    let child_id = "sthr-route-child";
    let child_two_id = "sthr-route-child-two";
    let make = |target: SessionThreadTarget,
                public_thread_id: &str,
                source_id: &str,
                call_id: &str,
                client_executed: bool| PendingToolReplyCandidate {
        key: PendingToolReplyKey {
            target,
            expected_thread_version: 7,
            expected_run_id: RunId(format!("run-{call_id}")),
            expected_correlation_id: format!("correlation-{call_id}"),
            runtime_call_id: call_id.into(),
            client_executed,
        },
        answered_pending_commit_cursor: 41,
        projected_event_id: Some(managed_tool_event_id(public_thread_id, source_id, call_id)),
        projected_family: Some(if client_executed {
            ProjectedToolUseFamily::Custom
        } else {
            ProjectedToolUseFamily::Tool
        }),
    };
    let mut candidates = vec![
        make(
            SessionThreadTarget::Primary,
            &primary_thread_id,
            "root-source",
            "root-call",
            true,
        ),
        make(
            SessionThreadTarget::Primary,
            &primary_thread_id,
            "root-source-two",
            "root-call-two",
            true,
        ),
        make(
            SessionThreadTarget::Child(ThreadId(child_id.into())),
            child_id,
            "child-source",
            "child-call",
            true,
        ),
        make(
            SessionThreadTarget::Child(ThreadId(child_id.into())),
            child_id,
            "child-source-two",
            "child-call-two",
            true,
        ),
        make(
            SessionThreadTarget::Child(ThreadId("sthr-confirm".into())),
            "sthr-confirm",
            "confirm-source",
            "confirm-call",
            false,
        ),
        make(
            SessionThreadTarget::Child(ThreadId("sthr-ambiguous-one".into())),
            "sthr-ambiguous-one",
            "ambiguous-source-one",
            "ambiguous-call",
            true,
        ),
        make(
            SessionThreadTarget::Child(ThreadId(child_two_id.into())),
            child_two_id,
            "ambiguous-source-two",
            "ambiguous-call",
            true,
        ),
        make(
            SessionThreadTarget::Child(ThreadId("sthr-legacy-child".into())),
            "sthr-legacy-child",
            "legacy-source",
            "legacy-child-call",
            true,
        ),
    ];
    let mut generic_tool = make(
        SessionThreadTarget::Primary,
        &primary_thread_id,
        "generic-source",
        "generic-tool-call",
        true,
    );
    generic_tool.projected_family = Some(ProjectedToolUseFamily::Tool);
    candidates.push(generic_tool);
    let public_id = |call_id: &str| {
        candidates
            .iter()
            .find(|candidate| candidate.key.runtime_call_id == call_id)
            .and_then(|candidate| candidate.projected_event_id.as_deref())
            .expect("candidate has a projected id")
    };
    let child_public_id = public_id("child-call");
    let root_public_id = public_id("root-call");

    let resolved_child = ManagedState::resolve_tool_reply(
        session_id,
        child_public_id,
        ToolReplyFamily::CustomResult,
        &candidates,
    )
    .expect("R1/E1 qualified id routes to child");
    assert_eq!(
        resolved_child.key.target,
        SessionThreadTarget::Child(ThreadId(child_id.into())),
        "R1/E1"
    );
    assert!(
        matches!(resolved_child.key.target, SessionThreadTarget::Child(_)),
        "R1/E1 child owns its required-action activity"
    );
    assert_eq!(resolved_child.key.runtime_call_id, "child-call", "R1/E1");
    let root = ManagedState::resolve_tool_reply(
        session_id,
        root_public_id,
        ToolReplyFamily::CustomResult,
        &candidates,
    )
    .expect("R2/E1 qualified root");
    assert_eq!(root.key.target, SessionThreadTarget::Primary, "R2/E1");
    assert_eq!(
        root.key.target,
        SessionThreadTarget::Primary,
        "R2/E1 root activity"
    );
    let legacy_root = ManagedState::resolve_tool_reply(
        session_id,
        "root-call",
        ToolReplyFamily::CustomResult,
        &candidates,
    )
    .expect("R3/E1 unique legacy root");
    assert_eq!(
        legacy_root.key.target,
        SessionThreadTarget::Primary,
        "R3/E1"
    );
    let legacy_child = ManagedState::resolve_tool_reply(
        session_id,
        "legacy-child-call",
        ToolReplyFamily::CustomResult,
        &candidates,
    )
    .expect("R3/E1 unique legacy child");
    assert_eq!(
        legacy_child.key.target,
        SessionThreadTarget::Child(ThreadId("sthr-legacy-child".into())),
        "R3/E1"
    );
    assert!(
        ManagedState::resolve_tool_reply(
            session_id,
            "ambiguous-call",
            ToolReplyFamily::CustomResult,
            &candidates,
        )
        .is_err(),
        "R4/E2 legacy id cannot choose among owners"
    );
    assert!(
        ManagedState::resolve_tool_reply(
            session_id,
            child_public_id,
            ToolReplyFamily::Confirmation,
            &candidates,
        )
        .is_err(),
        "R5/E2 wrong reply kind"
    );
    let stale_child_id = managed_tool_event_id(child_id, "stale-source", "child-call");
    assert!(
        ManagedState::resolve_tool_reply(
            session_id,
            &stale_child_id,
            ToolReplyFamily::CustomResult,
            &candidates,
        )
        .is_err(),
        "R6/E2 stale qualified source"
    );
    assert!(
        ManagedState::resolve_tool_reply(
            session_id,
            public_id("child-call"),
            ToolReplyFamily::ToolResult,
            &candidates,
        )
        .is_err(),
        "R9/E2 generic result cannot answer agent.custom_tool_use"
    );
    assert!(
        ManagedState::resolve_tool_reply(
            session_id,
            public_id("generic-tool-call"),
            ToolReplyFamily::CustomResult,
            &candidates,
        )
        .is_err(),
        "R9/E2 custom result cannot answer agent.tool_use"
    );
    let generic = ManagedState::resolve_tool_reply(
        session_id,
        public_id("generic-tool-call"),
        ToolReplyFamily::ToolResult,
        &candidates,
    )
    .expect("R9 matching agent.tool_use result");
    assert_eq!(generic.key.expected_run_id.0, "run-generic-tool-call");
    assert_eq!(
        generic.key.expected_correlation_id,
        "correlation-generic-tool-call"
    );

    for (owner, first_call, second_call) in [
        (SessionThreadTarget::Primary, "root-call", "root-call-two"),
        (
            SessionThreadTarget::Child(ThreadId(child_id.into())),
            "child-call",
            "child-call-two",
        ),
    ] {
        let owner_candidates = candidates
            .iter()
            .filter(|candidate| {
                candidate.key.target == owner
                    && matches!(
                        candidate.key.runtime_call_id.as_str(),
                        call if call == first_call || call == second_call
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        for order in [[first_call, second_call], [second_call, first_call]] {
            let mut unresolved = owner_candidates
                .iter()
                .map(|candidate| candidate.key.clone())
                .collect::<std::collections::HashSet<_>>();
            let first = ManagedState::resolve_tool_reply(
                session_id,
                public_id(order[0]),
                ToolReplyFamily::CustomResult,
                &owner_candidates,
            )
            .expect("R7/E3 first same-owner reply");
            ManagedState::consume_tool_reply_identity(&mut unresolved, &first)
                .expect("R7/E3 first identity");
            assert!(!unresolved.is_empty(), "R8/E4 partial remains unresolved");
            for followup in [
                InboundEvent::UserMessage {
                    content: vec![ContentBlock::text("continue")],
                },
                InboundEvent::SystemMessage {
                    content: vec![ContentBlock::text("context")],
                },
            ] {
                assert!(
                    ManagedState::unresolved_tool_replies_block_followup(&followup, &unresolved,),
                    "R8/E4 partial reply blocks user/system follow-up"
                );
            }
            assert!(
                ManagedState::consume_tool_reply_identity(&mut unresolved, &first).is_err(),
                "R8/E4 duplicate same-call reply"
            );
            let second = ManagedState::resolve_tool_reply(
                session_id,
                public_id(order[1]),
                ToolReplyFamily::CustomResult,
                &owner_candidates,
            )
            .expect("R7/E3 second same-owner reply");
            ManagedState::consume_tool_reply_identity(&mut unresolved, &second)
                .expect("R7/E3 second identity");
            assert!(unresolved.is_empty(), "R7/E3 both orders complete");
        }
    }
}

async fn pending_child_reply_fixture(
    label: &str,
    client_executed: bool,
    declared_custom: bool,
) -> (RehydrateFake, Arc<ManagedState>, String, String, String) {
    let runtime = RehydrateFake::default();
    let family = if declared_custom {
        FrozenTestToolFamily::Custom
    } else if client_executed {
        FrozenTestToolFamily::AgentAlwaysAllow
    } else {
        FrozenTestToolFamily::AgentAlwaysAsk
    };
    let tool_name = if declared_custom {
        "client_lookup"
    } else {
        "bash"
    };
    let state = Arc::new(
        ManagedState::new(runtime.clone()).with_config_source(Arc::new(
            FrozenToolFamilyProfiles::uniform("coder", &["researcher"], family, tool_name),
        )),
    );
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let child_id = format!("sthr-route-{label}");
    let child_run = RunId(format!("run-route-{label}"));
    let call_id = format!("call-route-{label}");
    runtime.install_agent_coordination_prefix(
        &session.id,
        RunId("root".into()),
        0,
        &call_id,
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.clone()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &RunId("root".into()),
                0,
                &call_id,
            ),
            latest_run_id: Some(child_run.clone()),
        },
    );
    runtime.committed_by_thread.lock().unwrap().insert(
        child_id.clone(),
        vec![Message::new(
            MessageId(format!("message-route-{label}")),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                &call_id,
                tool_name,
                serde_json::json!({"label":label}),
            )],
        )],
    );
    runtime.pending_by_thread.lock().unwrap().insert(
        child_id.clone(),
        Pending {
            tool_use_id: call_id.clone(),
            name: tool_name.into(),
            input: serde_json::json!({"label":label}),
            client_executed,
        },
    );
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &child_id,
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &child_id,
            &child_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);
    state.refresh_committed_events(&session.id).await.unwrap();
    let public_event_id = state
        .list_thread_events(&session.id, &child_id, None, None)
        .unwrap()
        .data
        .into_iter()
        .find(|event| {
            matches!(
                event.kind,
                OutboundKind::AgentToolUse { .. }
                    | OutboundKind::AgentCustomToolUse { .. }
                    | OutboundKind::AgentMcpToolUse { .. }
            )
        })
        .expect("child pending tool is projected")
        .id;
    (runtime, state, session.id, child_id, public_event_id)
}

#[tokio::test]
async fn retained_unprocessed_tool_reply_queues_a_followup_user_message() {
    // Cause/effect graph: C1 Runtime still exposes one exact permission ticket;
    // C2 the Session root already retains the exact matching ToolReply; C3 that
    // receipt is unprocessed because delivery/recovery has not completed; C4 a
    // standalone UserMessage arrives. Effects: E1 C2 is classified as resolving,
    // not unresolved; E2 C4 is durably admissible behind C2; E3 a pending ticket
    // with no retained reply still blocks follow-up; E4 no second pending ledger
    // is introduced (the root entry's `processed` bit remains authoritative).
    //
    // | Rule | Runtime ticket | retained exact reply | processed | follow-up |
    // |---|---|---|---|---|
    // | AR1 | yes | no  | n/a   | reject (E3) |
    // | AR2 | yes | yes | false | queue (E1+E2+E4) |
    // | AR3 | yes | yes | true  | Runtime replay/projection owns closure |
    let (_runtime, state, session_id, _child_id, public_event_id) =
        pending_child_reply_fixture("retained-followup", false, false).await;
    assert!(
        state
            .validate_event_batch(
                &session_id,
                &[InboundEvent::UserMessage {
                    content: vec![ContentBlock::text("must not bypass approval")],
                }],
            )
            .await
            .is_err(),
        "AR1/E3"
    );
    let confirmation = InboundEvent::UserToolConfirmation {
        tool_use_id: public_event_id,
        result: ConfirmResult::Allow,
        deny_message: None,
    };
    let accepted_reply = state
        .validate_event_batch(&session_id, &[confirmation])
        .await
        .expect("AR2 exact confirmation");
    state
        .application
        .append_session_event_batch(&session_id, accepted_reply.inputs, None, None)
        .await
        .expect("AR2 retain reply without driving it");

    let followup = state
        .validate_event_batch(
            &session_id,
            &[InboundEvent::UserMessage {
                content: vec![ContentBlock::text("continue after approval")],
            }],
        )
        .await
        .expect("AR2 retained reply admits a queued follow-up");
    assert!(
        matches!(
            followup.inputs.as_slice(),
            [SessionEventInput::UserMessage { .. }]
        ),
        "AR2/E2"
    );
}

#[tokio::test]
async fn qualified_event_id_routes_every_child_tool_reply_variant() {
    // Causes: the fixtures below establish `qualified event id routes every child tool reply
    // variant` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C0=the child Agent freezes an Agent toolset with
    // always-ask/always-allow policy or an exact custom client descriptor;
    // C1=a child has a committed permission/custom wait; C2=the pending
    // projection is permission, declared custom, or self-hosted
    // Agent-toolset; C3=the reply is the exact matching confirmation,
    // custom_tool_result, or tool_result family; C4=its qualified public
    // EventId embeds that child; C5=the reply is replayed after the
    // ticket is consumed; C6=the validation read may also observe unrelated
    // committed Runtime facts; C7=a primary-visible requires_action boundary
    // references the child Event id; C8=a child Agent-toolset Event is not
    // referenced. Effects: E1=all three variants deliver through the same
    // typed Thread-reply port with the Runtime call id and exact family;
    // E2=the canonical child id is echoed into parent/child projections;
    // E3=C5 is rejected before another matching receipt or delivery,
    // independently of C6; E4=C7 makes the exact child tool-use visible in
    // both primary list and live projection; E5=C8 remains child-private.
    // Primary activity ownership is covered directly
    // by the resolver decision table above; total event count is not a reply
    // atomicity oracle because the sole committed projector remains live.
    // Decision table:
    // | Rule | Frozen family | Pending kind | Reply event | Qualified id | Replay | Projection read | Effects |
    // | R1 | Agent/ask | permission | confirmation | T | F | any | E1,E2,E4 |
    // | R2 | custom | client | custom result | T | F | any | E1,E2,E4 |
    // | R3 | Agent/allow | client | generic result | T | F | any | E1,E2,E4 |
    // | R4 | matching frozen family | matching prior kind | matching reply | T | T | any | E3 |
    // | R5 | Agent/allow | client | none | T | F | any | E5 |
    for (label, client_executed, declared_custom) in [
        ("confirmation", false, false),
        ("custom", true, true),
        ("generic", true, false),
    ] {
        let (runtime, state, session_id, child_id, public_event_id) =
            pending_child_reply_fixture(label, client_executed, declared_custom).await;
        let primary_event = state
            .list_events(&session_id, None, None, false)
            .unwrap()
            .data
            .into_iter()
            .find(|event| event.id == public_event_id)
            .expect("R1-R3/E4 primary list closes requires_action reference");
        assert!(
            matches!(
                &primary_event.kind,
                OutboundKind::AgentToolUse {
                    session_thread_id: Some(target),
                    ..
                }
                    | OutboundKind::AgentCustomToolUse {
                        session_thread_id: Some(target),
                        ..
                    }
                    | OutboundKind::AgentMcpToolUse {
                        session_thread_id: Some(target),
                        ..
                    }
                    if target == &child_id
            ),
            "R1-R3/E4 primary routing hint {label}"
        );
        let committed_event = {
            let sessions = state.sessions.lock().unwrap();
            sessions[&session_id]
                .events
                .iter()
                .find(|event| event.id == public_event_id)
                .cloned()
                .expect("R1-R3 committed child event")
        };
        assert_eq!(
            state
                .project_committed_event_for_thread(&session_id, &session_id, committed_event,)
                .expect("R1-R3/E4 live projection closes the same reference")
                .id,
            public_event_id,
            "R1-R3/E4 list/live identity"
        );
        let inbound = match label {
            "confirmation" => InboundEvent::UserToolConfirmation {
                tool_use_id: public_event_id.clone(),
                result: ConfirmResult::Allow,
                deny_message: None,
            },
            "custom" => InboundEvent::UserCustomToolResult {
                custom_tool_use_id: public_event_id.clone(),
                content: Some(vec![ContentBlock::text("custom result")]),
                is_error: false,
            },
            "generic" => InboundEvent::UserToolResult {
                tool_use_id: public_event_id.clone(),
                content: Some(vec![ContentBlock::text("generic result")]),
                is_error: false,
            },
            _ => unreachable!(),
        };
        state
            .send_events(
                &session_id,
                SendEventsRequest {
                    events: vec![inbound.clone()],
                },
            )
            .await
            .unwrap();

        {
            let replies = runtime.thread_tool_replies.lock().unwrap();
            assert_eq!(replies.len(), 1, "R1-R3/E1 {label}");
            assert_eq!(
                replies[0].target,
                SessionThreadTarget::Child(ThreadId(child_id.clone())),
                "R1-R3/E1"
            );
            assert_eq!(
                replies[0].tool_use_id,
                format!("call-route-{label}"),
                "R1-R3/E1"
            );
            match (&replies[0].reply, label) {
                (
                    SessionThreadToolReply::Confirm(ToolPermissionDecision::Allow { note: None }),
                    "confirmation",
                )
                | (
                    SessionThreadToolReply::Custom {
                        is_error: false, ..
                    },
                    "custom",
                )
                | (
                    SessionThreadToolReply::Result {
                        is_error: false, ..
                    },
                    "generic",
                ) => {}
                other => panic!("R1-R3/E1 unexpected reply: {other:?}"),
            }
        }

        assert!(
            state
                .list_thread_events(&session_id, &child_id, None, None)
                .unwrap()
                .data
                .iter()
                .any(|event| match &event.kind {
                    OutboundKind::UserToolConfirmation {
                        session_thread_id: Some(target),
                        ..
                    }
                    | OutboundKind::UserCustomToolResult {
                        session_thread_id: Some(target),
                        ..
                    }
                    | OutboundKind::UserToolResult {
                        session_thread_id: Some(target),
                        ..
                    } => target == &child_id,
                    _ => false,
                }),
            "R1-R3/E2 {label}"
        );
        let receipt_type = match label {
            "confirmation" => "user.tool_confirmation",
            "custom" => "user.custom_tool_result",
            "generic" => "user.tool_result",
            _ => unreachable!(),
        };
        let receipt_count = state
            .list_events(&session_id, None, None, false)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == receipt_type)
            .count();
        assert!(
            state
                .send_events(
                    &session_id,
                    SendEventsRequest {
                        events: vec![inbound],
                    },
                )
                .await
                .is_err(),
            "R4/E3 {label}"
        );
        assert_eq!(
            state
                .list_events(&session_id, None, None, false)
                .unwrap()
                .data
                .iter()
                .filter(|event| event.type_str() == receipt_type)
                .count(),
            receipt_count,
            "R4/E3 no replay receipt {label}"
        );
        assert_eq!(
            runtime.thread_tool_replies.lock().unwrap().len(),
            1,
            "R4/E3 one delivery {label}"
        );
    }

    let unreferenced = Event {
        id: "mtool_v1_unreferenced".into(),
        kind: OutboundKind::AgentToolUse {
            name: "internal_child_tool".into(),
            input: serde_json::json!({}),
            evaluated_permission: Some(EvaluatedPermission::Allow),
            session_thread_id: None,
        },
        processed_at: Some(PROCESSED_AT.into()),
    };
    assert!(
        ManagedState::project_event_for_thread(
            "session",
            "session",
            unreferenced,
            Some("sthr-child"),
            false,
        )
        .is_none(),
        "R5/E5 unreferenced child Agent tools remain private"
    );
}

#[tokio::test]
async fn legacy_tool_reply_id_routes_one_pending_owner() {
    // Causes: the fixtures below establish `legacy tool reply ids require one pending owner` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C0=root and child freeze the same custom client
    // descriptor; C1=a pre-qualification reply carries only a Runtime call
    // id; C2=one child or both root+child currently expose the same
    // call/custom-result family.
    // Effects: E1=one owner is routed through the canonical child reply port;
    // E2=multiple owners are rejected before a receipt/delivery.
    // Decision table (C0 is true for every rule):
    // | Rule | Frozen family | Matching owners | Effect |
    // | L1 | custom | child only | E1 |
    // | L2 | custom | root + child | E2 |
    let (runtime, state, session_id, child_id, _) =
        pending_child_reply_fixture("legacy-unique", true, true).await;
    state
        .send_events(
            &session_id,
            SendEventsRequest {
                events: vec![InboundEvent::UserCustomToolResult {
                    custom_tool_use_id: "call-route-legacy-unique".into(),
                    content: Some(vec![ContentBlock::text("legacy result")]),
                    is_error: false,
                }],
            },
        )
        .await
        .expect("L1/E1 unique legacy child");
    assert_eq!(
        runtime.thread_tool_replies.lock().unwrap()[0].target,
        SessionThreadTarget::Child(ThreadId(child_id)),
        "L1/E1"
    );
}

#[tokio::test]
async fn legacy_tool_reply_id_rejects_ambiguous_pending_owners() {
    // L2 from the legacy-id ownership decision table above is intentionally a
    // separate async fixture. Keeping both full ManagedState fixtures alive in
    // one test future exceeded the default libtest thread stack after runtime
    // profiles grew, even though neither production path recursed.
    let (runtime, state, session_id, _child_id, _) =
        pending_child_reply_fixture("legacy-ambiguous", true, true).await;
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: "call-route-legacy-ambiguous".into(),
        name: "client_lookup".into(),
        input: serde_json::json!({"owner":"root"}),
        client_executed: true,
    });
    runtime
        .committed_by_thread
        .lock()
        .unwrap()
        .get_mut(&session_id)
        .expect("L2 canonical root transcript")
        .push(Message::new(
            MessageId("message-route-legacy-ambiguous-root".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "call-route-legacy-ambiguous",
                "client_lookup",
                serde_json::json!({"owner":"root"}),
            )],
        ));
    runtime
        .message_cursors_by_thread
        .lock()
        .unwrap()
        .get_mut(&session_id)
        .expect("L2 canonical root transcript coordinates")
        .push(4);
    let root_run = RunId("run-route-legacy-ambiguous-root".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            3,
            &session_id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            4,
            &session_id,
            &root_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);
    state.refresh_committed_events(&session_id).await.unwrap();
    let before = state
        .list_events(&session_id, None, None, false)
        .unwrap()
        .data
        .len();
    assert!(
        state
            .send_events(
                &session_id,
                SendEventsRequest {
                    events: vec![InboundEvent::UserCustomToolResult {
                        custom_tool_use_id: "call-route-legacy-ambiguous".into(),
                        content: Some(vec![ContentBlock::text("ambiguous")]),
                        is_error: false,
                    }],
                },
            )
            .await
            .is_err(),
        "L2/E2 legacy id cannot choose among owners"
    );
    assert_eq!(
        state
            .list_events(&session_id, None, None, false)
            .unwrap()
            .data
            .len(),
        before,
        "L2/E2 no receipt"
    );
    assert!(
        runtime.thread_tool_replies.lock().unwrap().is_empty(),
        "L2/E2 no delivery"
    );
}

#[tokio::test]
async fn aggregate_running_edge_is_decided_from_the_locked_event_tail() {
    // Causes: the fixtures below establish `aggregate running edge` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 durable Session state is Running/non-Running;
    // C2 the disposable Session DTO is already Running or not; C3 the latest
    // aggregate wire state is absent, Idle/Rescheduled, Running, or
    // Terminated; C4 arbitrary observational events trail that state.
    // Effects: E1 emit one Running edge for absent/idle/rescheduled whenever
    // C1 is Running, independently of C2; E2 suppress a duplicate after the
    // first refresh appends Running; E3 never reopen Terminated; E4
    // non-Running durable state emits nothing. The event tail, not the
    // current DTO, is the sole projection idempotency authority.
    //
    // | Rule | DTO | Durable | Latest aggregate | Effect |
    // |---|---|---|---|---|
    // | R1 | any | Running | absent | E1 |
    // | R2 | any | Running | Idle/Rescheduled | E1 |
    // | R3 | any | Running | Running | E2 |
    // | R4 | any | Running | Terminated | E3 |
    // | R5 | any | Idle | any | E4 |
    // | R6 | Running | Running | absent | E1; DTO is not event history |
    let state = ManagedState::new(LifecycleRuntime::default());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let mut sessions = state.sessions.lock().unwrap();
    let record = sessions.get_mut(&session.id).unwrap();
    let event = |kind| Event {
        id: "test-event".into(),
        kind,
        processed_at: Some(PROCESSED_AT.into()),
    };

    record.events.clear();
    assert!(
        ManagedState::should_append_aggregate_running(record, SessionStatus::Running),
        "R1/E1"
    );
    assert!(
        !ManagedState::should_append_aggregate_running(record, SessionStatus::Idle),
        "R5/E4"
    );
    record.session.status = SessionStatus::Running;
    assert!(
        ManagedState::should_append_aggregate_running(record, SessionStatus::Running),
        "R6/E1"
    );

    record.events.extend([
        event(OutboundKind::SessionStatusIdle {
            stop_reason: StopReason::EndTurn,
        }),
        event(OutboundKind::AgentThinking {}),
    ]);
    assert!(
        ManagedState::should_append_aggregate_running(record, SessionStatus::Running),
        "R2/E1"
    );
    record.events.clear();
    record.events.extend([
        event(OutboundKind::SessionStatusRescheduled {}),
        event(OutboundKind::AgentThinking {}),
    ]);
    assert!(
        ManagedState::should_append_aggregate_running(record, SessionStatus::Running),
        "R2/E1 rescheduled"
    );
    record
        .events
        .push(event(OutboundKind::SessionStatusRunning {}));
    record.events.push(event(OutboundKind::AgentThinking {}));
    assert!(
        !ManagedState::should_append_aggregate_running(record, SessionStatus::Running),
        "R2-R3/E2"
    );

    record
        .events
        .push(event(OutboundKind::SessionStatusTerminated {}));
    assert!(
        !ManagedState::should_append_aggregate_running(record, SessionStatus::Running),
        "R4/E3"
    );
}

#[tokio::test]
async fn cached_running_dto_cannot_precede_the_aggregate_running_event() {
    // Causes: the fixtures below establish `cached running dto` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the canonical Session activity CAS has committed
    // Running; C2 the disposable Session DTO is refreshed from C1 before any
    // lifecycle projection; C3 the latest committed root lifecycle fact is
    // Running; C4 a live subscriber is already attached; C5 the identical
    // committed prefix is refreshed again. Effects: E1 C1+C2+C3 append and
    // broadcast aggregate Running before primary Thread Running; E2 the
    // lifecycle cursor advances through C3; E3 C5 appends and broadcasts
    // nothing. The DTO is current-state projection only and cannot prove an
    // append-only history edge already exists.
    //
    // | Rule | Session | DTO | Root lifecycle | Refresh | Effects |
    // |---|---|---|---|---|---|
    // | O1 | Running | Running | latest Running | first | E1,E2 |
    // | O2 | Running | Running | same prefix | repeat | E3 |
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let (snapshot, mut live) = state.stream_subscribe(&session.id).unwrap();
    assert!(snapshot.is_empty(), "O1 starts without event history");

    let running = state
        .application
        .begin_activity(&session.id)
        .await
        .expect("O1/C1 commits aggregate Running");
    state
        .refresh_cached_projection(&running)
        .expect("O1/C2 refreshes the disposable DTO first");
    assert_eq!(
        state.get_session(&session.id).unwrap().status,
        SessionStatus::Running,
        "O1/C2"
    );
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &RunId("run-order-window".into()),
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));

    state.refresh_committed_events(&session.id).await.unwrap();
    let events = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        events.iter().map(Event::type_str).collect::<Vec<_>>(),
        ["session.status_running", "session.thread_status_running"],
        "O1/E1 committed order"
    );
    let first_live = live.try_recv().expect("O1/E1 aggregate live event");
    let second_live = live.try_recv().expect("O1/E1 primary Thread live event");
    assert_eq!(
        [first_live.type_str(), second_live.type_str()],
        ["session.status_running", "session.thread_status_running"],
        "O1/E1 broadcast preserves committed order"
    );
    assert_eq!(
        state.lifecycle_cursor(&session.id).unwrap(),
        lifecycle_cursor(1),
        "O1/E2"
    );

    state.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .len(),
        2,
        "O2/E3 no duplicate history"
    );
    assert!(
        matches!(
            live.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "O2/E3 no duplicate broadcast"
    );
}

#[tokio::test]
async fn root_terminal_defers_idle_until_the_session_activity_cas_settles() {
    // Causes: the fixtures below establish `root terminal defers idle until the session activity
    // cas settles` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the root Run terminal commit is visible; C2 the
    // Session activity epoch is still active when the first refresh reads
    // durable aggregate state; C3 that exact epoch settles before a later
    // refresh; C4 the root lifecycle cursor was already consumed by C2.
    // Effects: E1 C1+C2 projects the root output/terminal once but no false
    // aggregate idle; E2 it preserves the terminal stop reason in the sole
    // disposable projector continuation; E3 C3+C4 emits exactly one idle
    // without requiring a second lifecycle event; E4 further refreshes are
    // idempotent. This is the commit-order window exercised by coordinated
    // child reports: the report Run terminal can precede Session settlement.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effects |
    // | R1   | T  | T  | F  | F  | E1,E2   |
    // | R2   | T  | F  | T  | T  | E3      |
    // | R3   | T  | F  | T  | T  | E4      |
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let active = state
        .application
        .begin_activity(&session.id)
        .await
        .expect("R1/C2 opens the authoritative Session activity");
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &RunId("run-root-report".into()),
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));

    state.refresh_committed_events(&session.id).await.unwrap();
    {
        let record = state.sessions.lock().unwrap();
        let record = record.get(&session.id).unwrap();
        assert_eq!(
            record
                .events
                .iter()
                .filter(|event| event.type_str() == "session.status_idle")
                .count(),
            0,
            "R1/E1"
        );
        assert_eq!(
            record
                .deferred_session_stop_reason
                .as_ref()
                .map(|(_, reason)| reason),
            Some(&StopReason::EndTurn),
            "R1/E2"
        );
    }

    state
        .application
        .settle_activity(&session.id, active.activity_epoch)
        .await
        .expect("R2/C3 settles the exact activity epoch");
    state.refresh_committed_events(&session.id).await.unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();

    let record = state.sessions.lock().unwrap();
    let record = record.get(&session.id).unwrap();
    assert_eq!(
        record
            .events
            .iter()
            .filter(|event| event.type_str() == "session.status_idle")
            .count(),
        1,
        "R2-R3/E3-E4"
    );
    assert!(
        record.deferred_session_stop_reason.is_none(),
        "R2/E3 consumes the one projector continuation"
    );
}

#[tokio::test]
async fn newer_same_run_terminal_replaces_deferred_scheduled_awaiting_reason() {
    // Cause/effect graph: C1 a root Run commits ScheduledAction Awaiting with an
    // answerable tool while its Session activity remains Running; C2 the Worker
    // autonomously resumes that same Run and commits NaturalEnd before the
    // aggregate activity settles; C3 the exact activity epoch then settles.
    // Effects: E1 C1 defers RequiresAction and emits no aggregate idle; E2 C2
    // replaces that disposable reason with EndTurn but still emits no early idle;
    // E3 C3 emits exactly one aggregate idle with EndTurn and no stale
    // RequiresAction. K1 committed Run lifecycle and pending truth are the sole
    // authority; K2 pending action and BudgetReached remain higher priority.
    //
    // | Rule | Root terminal | Same Run | Activity | Pending | Effect |
    // |---|---|---|---|---|---|
    // | S1 | Awaiting | n/a | Running | exact tool | E1 |
    // | S2 | NaturalEnd | yes | Running | none | E2 |
    // | S3 | already projected | yes | Idle | none | E3 |
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let active = state
        .application
        .begin_activity(&session.id)
        .await
        .expect("S1 opens the authoritative Session activity");
    let run_id = RunId("run-scheduled-autonomous".into());
    let call_id = "scheduled-call";
    runtime.push_committed_message(
        1,
        Message::new(
            MessageId("scheduled-tool".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                call_id,
                "read",
                serde_json::json!({"path":"probe.txt"}),
            )],
        ),
    );
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: call_id.into(),
        name: "read".into(),
        input: serde_json::json!({"path":"probe.txt"}),
        client_executed: false,
    });
    let mut awaiting = lifecycle(
        1,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    );
    awaiting.await_reason =
        Some(awaken_agent_contract::agent::awaiting::AwaitReason::ScheduledAction);
    runtime.lifecycle.lock().unwrap().push(awaiting);

    state.refresh_committed_events(&session.id).await.unwrap();
    {
        let records = state.sessions.lock().unwrap();
        let record = records.get(&session.id).unwrap();
        let (owner, reason) = record
            .deferred_session_stop_reason
            .as_ref()
            .expect("S1/E1 deferred reason");
        assert_eq!(owner.as_ref(), Some(&run_id), "S1/E1 exact root owner");
        let StopReason::RequiresAction { event_ids } = reason else {
            panic!("S1/E1 expected RequiresAction, got {reason:?}");
        };
        assert_eq!(
            event_ids,
            &[record
                .projected_tool_index_for(None)
                .latest
                .get(call_id)
                .cloned()
                .expect("S1 answerable public tool id")],
            "S1/E1 exact deferred public tool id"
        );
        assert!(
            record
                .events
                .iter()
                .all(|event| event.type_str() != "session.status_idle"),
            "S1/E1 no early aggregate idle"
        );
    }

    *runtime.pending.lock().unwrap() = None;
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            2,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Resumed,
            RunState::Running,
        ),
        lifecycle(
            3,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    state.refresh_committed_events(&session.id).await.unwrap();
    {
        let records = state.sessions.lock().unwrap();
        let record = records.get(&session.id).unwrap();
        assert_eq!(
            record.deferred_session_stop_reason,
            Some((Some(run_id.clone()), StopReason::EndTurn)),
            "S2/E2 newer same-Run terminal replaces stale Awaiting"
        );
        assert!(
            record
                .events
                .iter()
                .all(|event| event.type_str() != "session.status_idle"),
            "S2/E2 activity still fences aggregate idle"
        );
    }

    state
        .application
        .settle_activity(&session.id, active.activity_epoch)
        .await
        .expect("S3 settles the exact activity epoch");
    state.refresh_committed_events(&session.id).await.unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();
    let records = state.sessions.lock().unwrap();
    let record = records.get(&session.id).unwrap();
    let idle_reasons = record
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            OutboundKind::SessionStatusIdle { stop_reason } => Some(stop_reason),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(idle_reasons, [&StopReason::EndTurn], "S3/E3");
    assert!(
        record.deferred_session_stop_reason.is_none(),
        "S3/E3 clears the one projector continuation"
    );
}

#[tokio::test]
async fn recovered_terminal_brackets_output_in_warm_and_cold_projections() {
    // Causes: the fixtures below establish `recovered terminal brackets output in warm and cold
    // projections` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the Session root retains the canonical User
    // Event and the committed Thread transcript carries its matching
    // operation identity plus assistant output; C2 Running is observed warm
    // before the same Run commits terminal; C3 the aggregate has already
    // returned to Idle before a cold peer's first protocol read; C4 the two
    // disposable projectors have deliberately skewed process-local event
    // counters; C5 either projector is refreshed again; C6 a page cursor from
    // the warm replica is resumed on the cold replica. Effects: E1 the
    // root-owned input precedes the reconstructed
    // Running edge; E2 Running precedes output; E3 terminal
    // Thread/usage/Idle close the interval; E4 replay adds nothing; E5 every
    // primary Thread lifecycle field uses the same stable public `sthr_` id
    // while Runtime recovery remains keyed by the Session id; E6 every full
    // `(type,id)` sequence and every continuation page is identical across
    // replicas despite C4. A transcript MessageId alone is deliberately not
    // User Event provenance.
    //
    // | Rule | Root prefix | Warm Running | Cold after terminal | Counter skew | Cursor crosses replica | Effects |
    // | W1 | T | T | F | T | F | E1+E2+E5 |
    // | W2 | T | T | F | T | F | E3+E4 |
    // | C1 | T | F | T | T | F | E1-E5 |
    // | X1 | T | T | T | T | T | E6 |
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let active = warm
        .application
        .begin_activity(&session.id)
        .await
        .expect("C2 opens the durable aggregate interval");
    let retained = warm
        .application
        .append_session_event_batch(
            &session.id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("start")],
            }],
            None,
            None,
        )
        .await
        .expect("C1 retains the canonical User Event in the Session root");
    let operation_id = retained.events[0].event.operation_id().to_string();
    persist_batch_projection_anchors(&warm, &session.id, &retained.batch_id, &[1]).await;
    *runtime.messages.lock().unwrap() = vec![Message::new(
        MessageId::session_event_input(&session.id, &operation_id),
        Role::User,
        vec![ContentBlock::text("start")],
    )];
    *runtime.message_cursors.lock().unwrap() = vec![1];
    let run_id = RunId("recovered-run".into());
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    for _ in 0..3 {
        warm.next_event_id();
    }
    warm.refresh_committed_events(&session.id).await.unwrap();

    runtime.messages.lock().unwrap().push(Message::text(
        MessageId("recovered-agent-output".into()),
        Role::Assistant,
        "done",
    ));
    runtime.message_cursors.lock().unwrap().push(2);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        2,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));
    warm.application
        .settle_activity_observed(
            &session.id,
            active.activity_epoch,
            Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: active.activity_epoch,
                thread_id: ThreadId(session.id.clone()),
                run_id,
                lifecycle_cursor: lifecycle_cursor(2),
                source_commit_cursor: 2,
            }),
        )
        .await
        .expect("C2 closes the durable aggregate interval");
    warm.refresh_committed_events(&session.id).await.unwrap();

    let expected = vec![
        "user.message",
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ];
    let cold = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    for _ in 0..7 {
        cold.next_event_id();
    }
    cold.refresh_committed_events(&session.id).await.unwrap();
    let mut observations = Vec::new();
    for (rule, state) in [("W1-W2", &warm), ("C1-C2", &cold)] {
        state.refresh_committed_events(&session.id).await.unwrap();
        let primary_id = state
            .list_threads(&session.id)
            .expect("primary Thread")
            .into_iter()
            .find(|thread| thread.parent_thread_id.is_none())
            .map(|thread| thread.id)
            .expect("primary Thread projection");
        assert!(primary_id.starts_with("sthr_"), "{rule}/E5");
        assert_ne!(primary_id, session.id, "{rule}/E5");
        let first_events = state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data;
        let first = first_events
            .iter()
            .map(|event| event.type_str().to_string())
            .collect::<Vec<_>>();
        assert_eq!(first, expected, "{rule}/E1-E3");
        assert!(
            first_events
                .iter()
                .filter_map(|event| match &event.kind {
                    OutboundKind::SessionThreadStatusRunning {
                        session_thread_id, ..
                    }
                    | OutboundKind::SessionThreadStatusIdle {
                        session_thread_id, ..
                    } => Some(session_thread_id),
                    _ => None,
                })
                .all(|thread_id| thread_id == &primary_id),
            "{rule}/E5"
        );
        observations.push(
            first_events
                .iter()
                .map(|event| (event.type_str().to_string(), event.id.clone()))
                .collect::<Vec<_>>(),
        );
        state.refresh_committed_events(&session.id).await.unwrap();
        let replay = state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .into_iter()
            .map(|event| event.type_str().to_string())
            .collect::<Vec<_>>();
        assert_eq!(replay, expected, "{rule}/E4");
    }
    assert_eq!(observations[0], observations[1], "X1/E6 full history");
    for (_, cursor) in observations[0].iter().take(observations[0].len() - 1) {
        let warm_page = warm
            .list_events(&session.id, Some(cursor), Some(2), false)
            .expect("X1 warm continuation");
        let cold_page = cold
            .list_events(&session.id, Some(cursor), Some(2), false)
            .expect("X1 cursor remains valid on cold peer");
        let warm_page_events = warm_page
            .data
            .iter()
            .map(|event| (event.type_str(), event.id.as_str()))
            .collect::<Vec<_>>();
        let cold_page_events = cold_page
            .data
            .iter()
            .map(|event| (event.type_str(), event.id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            cold_page_events, warm_page_events,
            "X1/E6 page after {cursor}"
        );
        assert_eq!(
            cold_page.next_page, warm_page.next_page,
            "X1/E6 next page after {cursor}"
        );
    }
}

#[tokio::test]
async fn terminal_only_recovery_brackets_primary_output_with_one_aggregate_running() {
    // Cause/effect graph: C1 a fast primary Run has already committed output
    // and Completed before the protocol's first read; C2 the recovery source
    // therefore exposes the terminal lifecycle fact but no opening fact; C3
    // the disposable projection is read repeatedly. Effects: E1 the terminal
    // cursor deterministically reconstructs aggregate Running, then primary
    // Thread Running; E2 both precede output and the closing Idle edge; E3 C3
    // never duplicates the fallback. Decision table: opening present is owned
    // by the adjacent warm/cold test; opening absent + terminal present =>
    // exactly one terminal-prefix aggregate bracket; neither fact => no bracket.
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let run_id = RunId("terminal-only-recovery".into());
    runtime.messages.lock().unwrap().push(Message::text(
        MessageId("terminal-only-output".into()),
        Role::Assistant,
        "done",
    ));
    runtime.message_cursors.lock().unwrap().push(2);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        2,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));

    state.refresh_committed_events(&session.id).await.unwrap();
    let first = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let types = first.iter().map(Event::type_str).collect::<Vec<_>>();
    let aggregate = types
        .iter()
        .position(|kind| *kind == "session.status_running")
        .expect("E1 aggregate Running");
    let thread = types
        .iter()
        .position(|kind| *kind == "session.thread_status_running")
        .expect("E1 primary Thread Running");
    let output = types
        .iter()
        .position(|kind| *kind == "agent.message")
        .expect("E2 output");
    let idle = types
        .iter()
        .position(|kind| *kind == "session.status_idle")
        .expect("E2 aggregate Idle");
    assert!(
        aggregate < thread && thread < output && output < idle,
        "E1-E2"
    );
    assert_eq!(
        types
            .iter()
            .filter(|kind| **kind == "session.status_running")
            .count(),
        1,
        "E3"
    );

    state.refresh_committed_events(&session.id).await.unwrap();
    let replay = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        serde_json::to_value(first).unwrap(),
        "E3"
    );
}

#[tokio::test]
async fn first_interval_lineage_preserves_the_issued_legacy_lifecycle_prefix() {
    // Cause/effect graph: C1 a pre-interval Session has one completed Run and
    // has already issued every legacy lifecycle-derived cursor; C2 the same Run
    // later opens, Resumes inside the first retained interval, and closes. E3 a
    // later warm refreshes read the same durable owners. Effects: E1
    // LifecyclePrefix Usage/Idle use the exact first terminal source; E2 the
    // first interval replaces only lifecycle events from its latest Resumed
    // opening, never the legacy Running; E3 every old cursor keeps the identical
    // prefix and the new interval is an append-only suffix; E4 replay is stable.
    // A cold peer cannot reconstruct pre-cutover aggregate brackets that old
    // writers never retained; the adjacent fresh-Session two-turn test owns the
    // cross-replica guarantee.
    //
    // | Rule | Interval phase | Immutable opening | Effect |
    // |---|---|---|---|
    // | L1 | absent | legacy terminal cursor 2 | E1 |
    // | L2 | begun, no Runtime opening | none | preserve the entire L1 prefix |
    // | L3 | Resumed at cursor 3 | exact cursor 3 | E2+E3 |
    // | L4 | closed at cursor 4, warm replay | exact cursors 3..4 | E3+E4 |
    //
    // This is the lineage cutover floor; no guessed revision or process-local
    // watermark participates.
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let run_id = RunId("legacy-lineage-run".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    state.refresh_committed_events(&session.id).await.unwrap();
    let legacy = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert!(
        legacy
            .iter()
            .any(|event| event.type_str() == "session.status_idle"),
        "L1/E1"
    );
    let legacy_cursors = legacy
        .iter()
        .map(|event| event.id.clone())
        .collect::<Vec<_>>();

    let active = state.application.begin_activity(&session.id).await.unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();
    let begun_without_opening = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        serde_json::to_value(&begun_without_opening).unwrap(),
        serde_json::to_value(&legacy).unwrap(),
        "L2 withholds an unanchored first interval without replacing legacy ids"
    );

    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Resumed,
        RunState::Running,
    ));
    state.refresh_committed_events(&session.id).await.unwrap();
    let resumed = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        serde_json::to_value(&resumed[..legacy.len()]).unwrap(),
        serde_json::to_value(&legacy).unwrap(),
        "L3/E2 keeps every issued legacy event in place"
    );
    assert!(
        resumed.len() > legacy.len(),
        "L3/E3 appends the new opening"
    );

    runtime.lifecycle.lock().unwrap().push(lifecycle(
        4,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));
    state
        .application
        .settle_activity_observed(
            &session.id,
            active.activity_epoch,
            Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: active.activity_epoch,
                thread_id: ThreadId(session.id.clone()),
                run_id,
                lifecycle_cursor: lifecycle_cursor(4),
                source_commit_cursor: 4,
            }),
        )
        .await
        .unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();
    let final_events = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        serde_json::to_value(&final_events[..resumed.len()]).unwrap(),
        serde_json::to_value(&resumed).unwrap(),
        "L4/E3 closing the interval preserves the previously issued prefix"
    );
    assert!(
        final_events.len() > legacy.len(),
        "L4/E3 append-only suffix"
    );
    state.refresh_committed_events(&session.id).await.unwrap();
    let replayed = state.list_events(&session.id, None, None, false).unwrap();
    assert_eq!(
        serde_json::to_value(&replayed.data).unwrap(),
        serde_json::to_value(&final_events).unwrap(),
        "L4/E4 warm replay is idempotent"
    );
    for cursor in &legacy_cursors {
        let suffix = state
            .list_events(&session.id, Some(cursor), Some(2), false)
            .unwrap();
        assert!(
            suffix
                .data
                .iter()
                .any(|event| event.type_str() == "session.status_running")
                || suffix.next_page.is_some(),
            "L4/E3 later interval remains reachable after {cursor}"
        );
    }
}

#[tokio::test]
async fn two_closed_turns_have_one_canonical_payload_and_every_cross_replica_suffix() {
    // Cause/effect graph: C1 two ordinary batches admit overlapping root Runs
    // into one interval and a third batch owns the following interval; C2 both
    // intervals retain exact observations; C3 message and lifecycle facts expose backend-wide commit
    // cursors; C4 the warm projector saw each turn incrementally while an
    // independent cold projector starts after both; C5 every durable Event id is
    // reused as a cursor on the other replica. Effects: E1 each retained inbound
    // batch is assigned by admitted revision plus its exact Run opening; E2
    // each interval is ordered input -> Session/Thread Running -> output ->
    // Thread Idle -> exact Usage/Session Idle; E3 full serialized payloads and
    // every suffix are byte-for-byte equal; E4 replay adds nothing.
    //
    // | Rule | Turns | View | Cursor | Effects |
    // | R1 | two overlapping batches | warm incremental | none | E1+E2 |
    // | R2 | two intervals | warm replay | none | E1+E2+E4 |
    // | R3 | two intervals | cold rebuild | none | E1+E2+E3 |
    // | R4 | two intervals | warm -> cold | every prior id | E3 |
    //
    // Constraint: admitted_revision is written by the existing root CAS;
    // interval observations and message/lifecycle cursors remain existing
    // Runtime facts. The disposable Managed vector owns no persistence.
    let (runtime, repository, warm, session_id, first_visible_prefix, first_turn_cursor) =
        Box::pin(async move {
            let runtime = LifecycleRuntime::default();
            let repository = Arc::new(ephemeral_session_repo());
            let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
            let session = warm
                .create_session(
                    serde_json::from_value(serde_json::json!({
                        "agent":"coder", "environment_id":"env_local"
                    }))
                    .unwrap(),
                    None,
                )
                .await
                .unwrap();

            let first_batch = warm
                .application
                .append_session_event_batch(
                    &session.id,
                    vec![SessionEventInput::UserMessage {
                        content: vec![ContentBlock::text("turn one")],
                    }],
                    None,
                    None,
                )
                .await
                .unwrap();
            let first_activity = warm.application.begin_activity(&session.id).await.unwrap();
            let SessionEventCommand::UserMessage {
                run_id: first_run, ..
            } = &first_batch.events[0].event
            else {
                unreachable!("first batch owns a User Run")
            };
            let first_run = first_run.clone();
            let overlapping_batch = warm
                .application
                .append_session_event_batch(
                    &session.id,
                    vec![SessionEventInput::UserMessage {
                        content: vec![ContentBlock::text("turn one overlap")],
                    }],
                    None,
                    None,
                )
                .await
                .unwrap();
            let overlapping_activity = warm.application.begin_activity(&session.id).await.unwrap();
            let SessionEventCommand::UserMessage {
                run_id: overlapping_run,
                ..
            } = &overlapping_batch.events[0].event
            else {
                unreachable!("overlapping batch owns a User Run")
            };
            let overlapping_run = overlapping_run.clone();
            runtime.lifecycle.lock().unwrap().extend([
                lifecycle(
                    10,
                    &session.id,
                    &first_run,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                ),
                lifecycle(
                    12,
                    &session.id,
                    &overlapping_run,
                    RunLifecycleEventKind::Running,
                    RunState::Running,
                ),
                lifecycle(
                    14,
                    &session.id,
                    &first_run,
                    RunLifecycleEventKind::Completed,
                    RunState::Ended(EndCause::NaturalEnd),
                ),
                lifecycle(
                    16,
                    &session.id,
                    &overlapping_run,
                    RunLifecycleEventKind::Completed,
                    RunState::Ended(EndCause::NaturalEnd),
                ),
            ]);
            *runtime.messages.lock().unwrap() = vec![
                Message::new(
                    MessageId::session_event_input(
                        &session.id,
                        first_batch.events[0].event.operation_id(),
                    ),
                    Role::User,
                    vec![ContentBlock::text("turn one")],
                ),
                Message::text(
                    MessageId("canonical-turn-one-output".into()),
                    Role::Assistant,
                    "one complete",
                ),
                Message::new(
                    MessageId::session_event_input(
                        &session.id,
                        overlapping_batch.events[0].event.operation_id(),
                    ),
                    Role::User,
                    vec![ContentBlock::text("turn one overlap")],
                ),
                Message::text(
                    MessageId("canonical-turn-one-overlap-output".into()),
                    Role::Assistant,
                    "overlap complete",
                ),
            ];
            *runtime.message_cursors.lock().unwrap() = vec![9, 11, 11, 13];
            persist_batch_projection_anchors(&warm, &session.id, &first_batch.batch_id, &[9]).await;
            persist_batch_projection_anchors(
                &warm,
                &session.id,
                &overlapping_batch.batch_id,
                &[11],
            )
            .await;
            warm.application
                .settle_activity_observed(
                    &session.id,
                    first_activity.activity_epoch,
                    Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                        activity_epoch: first_activity.activity_epoch,
                        thread_id: ThreadId(session.id.clone()),
                        run_id: first_run,
                        lifecycle_cursor: lifecycle_cursor(14),
                        source_commit_cursor: 14,
                    }),
                )
                .await
                .unwrap();
            warm.application
                .settle_activity_observed(
                    &session.id,
                    overlapping_activity.activity_epoch,
                    Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                        activity_epoch: overlapping_activity.activity_epoch,
                        thread_id: ThreadId(session.id.clone()),
                        run_id: overlapping_run,
                        lifecycle_cursor: lifecycle_cursor(16),
                        source_commit_cursor: 16,
                    }),
                )
                .await
                .unwrap();
            warm.refresh_committed_events(&session.id).await.unwrap();
            let first_visible_prefix = warm
                .list_events(&session.id, None, None, false)
                .unwrap()
                .data;
            let first_turn_cursor = first_visible_prefix
                .last()
                .expect("R1 first closed interval cursor")
                .id
                .clone();
            (
                runtime,
                repository,
                warm,
                session.id,
                first_visible_prefix,
                first_turn_cursor,
            )
        })
        .await;

    Box::pin(async move {
        let second_batch = warm
            .application
            .append_session_event_batch(
                &session_id,
                vec![SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("turn two")],
                }],
                None,
                None,
            )
            .await
            .unwrap();
        let second_activity = warm.application.begin_activity(&session_id).await.unwrap();
        let SessionEventCommand::UserMessage {
            run_id: second_run, ..
        } = &second_batch.events[0].event
        else {
            unreachable!("second batch owns a User Run")
        };
        let second_run = second_run.clone();
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                20,
                &session_id,
                &second_run,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                22,
                &session_id,
                &second_run,
                RunLifecycleEventKind::Completed,
                RunState::Ended(EndCause::NaturalEnd),
            ),
        ]);
        runtime.messages.lock().unwrap().extend([
            Message::new(
                MessageId::session_event_input(
                    &session_id,
                    second_batch.events[0].event.operation_id(),
                ),
                Role::User,
                vec![ContentBlock::text("turn two")],
            ),
            Message::text(
                MessageId("canonical-turn-two-output".into()),
                Role::Assistant,
                "two complete",
            ),
        ]);
        *runtime.message_cursors.lock().unwrap() = vec![9, 11, 11, 13, 19, 21];
        persist_batch_projection_anchors(&warm, &session_id, &second_batch.batch_id, &[19]).await;
        warm.application
            .settle_activity_observed(
                &session_id,
                second_activity.activity_epoch,
                Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                    activity_epoch: second_activity.activity_epoch,
                    thread_id: ThreadId(session_id.clone()),
                    run_id: second_run,
                    lifecycle_cursor: lifecycle_cursor(22),
                    source_commit_cursor: 22,
                }),
            )
            .await
            .unwrap();
        warm.refresh_committed_events(&session_id).await.unwrap();
        warm.refresh_committed_events(&session_id).await.unwrap();

        let cold = ManagedState::new(runtime).with_session_repo(repository);
        cold.refresh_committed_events(&session_id).await.unwrap();
        let warm_events = warm
            .list_events(&session_id, None, None, false)
            .unwrap()
            .data;
        let cold_events = cold
            .list_events(&session_id, None, None, false)
            .unwrap()
            .data;
        assert_eq!(
            &warm_events[..first_visible_prefix.len()],
            first_visible_prefix.as_slice(),
            "R1-R2 first-visible prefix is immutable after a later turn"
        );
        assert!(
            !warm
                .list_events(&session_id, Some(&first_turn_cursor), None, false)
                .unwrap()
                .data
                .is_empty(),
            "R2 late turn remains in the suffix of an already-issued cursor"
        );
        assert_eq!(
            serde_json::to_value(&cold_events).unwrap(),
            serde_json::to_value(&warm_events).unwrap(),
            "R3/E3 full ordered payload"
        );
        assert_eq!(
            warm_events.iter().map(Event::type_str).collect::<Vec<_>>(),
            [
                "user.message",
                "session.status_running",
                "session.thread_status_running",
                "user.message",
                "agent.message",
                "session.thread_status_running",
                "agent.message",
                "session.thread_status_idle",
                "session.thread_status_idle",
                "session.usage",
                "session.status_idle",
                "user.message",
                "session.status_running",
                "session.thread_status_running",
                "agent.message",
                "session.thread_status_idle",
                "session.usage",
                "session.status_idle",
            ],
            "R1-R3/E1+E2"
        );
        for cursor in warm_events.iter().map(|event| event.id.as_str()) {
            let warm_suffix = warm
                .list_events(&session_id, Some(cursor), None, false)
                .unwrap();
            let cold_suffix = cold
                .list_events(&session_id, Some(cursor), None, false)
                .unwrap();
            assert_eq!(
                serde_json::to_value(cold_suffix).unwrap(),
                serde_json::to_value(warm_suffix).unwrap(),
                "R4/E3 suffix after {cursor}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn late_compaction_uses_its_state_commit_and_preserves_an_issued_prefix() {
    // Cause/effect graph: C1 a completed Run and its message are listable at
    // commits 10..20; C2 a compaction StateCommand becomes visible later at
    // commit 30; C3 a peer cold-rebuilds from the final facts. Effects: E1 the
    // pre-C2 payload is an immutable prefix; E2 the new marker is present in
    // every page after the already-issued tail cursor; E3 warm/cold ordered
    // payloads and suffixes are identical. Decision table: L1=C1=>E1 baseline;
    // L2=C1+C2=>E1+E2; L3=C1+C2+C3=>E3. Constraint: state_command's existing
    // commit_sequence is the only marker anchor; lifecycle opening is never a
    // fallback.
    let runtime = LifecycleRuntime::default();
    runtime
        .include_runs_in_snapshot
        .store(true, Ordering::SeqCst);
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let run_id = RunId("late-compaction-run".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            10,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            20,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.messages.lock().unwrap().push(Message::text(
        MessageId("late-compaction-output".into()),
        Role::Assistant,
        "visible before compaction",
    ));
    runtime.message_cursors.lock().unwrap().push(19);
    warm.refresh_committed_events(&session.id).await.unwrap();
    let prefix = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let cursor = prefix.last().expect("L1 issued cursor").id.clone();

    runtime
        .state
        .lock()
        .unwrap()
        .push(awaken_runtime_contract::compaction::RunCompactionMarker::command(&run_id.0));
    runtime.state_cursors.lock().unwrap().push(30);
    warm.refresh_committed_events(&session.id).await.unwrap();
    let final_warm = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(&final_warm[..prefix.len()], prefix.as_slice(), "L2/E1");
    let warm_suffix = warm
        .list_events(&session.id, Some(&cursor), None, false)
        .unwrap();
    assert!(
        warm_suffix
            .data
            .iter()
            .any(|event| event.type_str() == "agent.thread_context_compacted"),
        "L2/E2 suffix: {warm_suffix:?}"
    );

    let cold = ManagedState::new(runtime).with_session_repo(repository);
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        cold.list_events(&session.id, None, None, false)
            .unwrap()
            .data,
        final_warm,
        "L3/E3 full order"
    );
    assert_eq!(
        cold.list_events(&session.id, Some(&cursor), None, false)
            .unwrap(),
        warm_suffix,
        "L3/E3 old cursor suffix"
    );
}

#[test]
fn outcome_projection_requires_a_terminal_fence_and_anchors_synthetic_interrupts() {
    // Cause/effect graph: C1 a terminal state command has no same-commit active
    // pointer removal; C2 an ordinary evaluation has its immutable command; C3
    // cancellation synthesizes a final interrupted cycle with no Grade command;
    // C4 a non-interrupted cycle lacks its evaluation command. Effects: E1 C1
    // remains invisible; E2 C2 uses the evaluation cursor; E3 C3 uses the exact
    // terminal cursor; E4 C4 fails closed. Decision table: R1=C1=>E1;
    // R2=terminal+C2+C3=>E2+E3; R3=terminal+C4=>E4.
    // Invariants: admission and ordering share one anchored value; no JSON state
    // payload is decoded, and a prior non-terminal state update cannot anchor a
    // newer terminal observation read through a different port.
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};

    let outcome_id = "outcome-anchor";
    let set = |key: String| {
        Command::set(
            Scope::Thread,
            MergePolicy::Disjoint,
            key,
            serde_json::json!({"opaque": true}),
        )
    };
    let terminal_commands = || {
        vec![
            (set(format!("outcome/{outcome_id}/evaluation/0")), 10),
            (set(format!("outcome/{outcome_id}/state")), 20),
            (
                Command::remove(Scope::Thread, MergePolicy::Disjoint, "outcome/active"),
                20,
            ),
        ]
    };
    let snapshot = |entries: Vec<(Command, u64)>| {
        let (state, state_commit_cursors) = entries.into_iter().unzip();
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            thread_id: ThreadId("outcome-root".into()),
            claimed_run_id: RunId("outcome-state".into()),
            runs: Vec::new(),
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            state,
            state_commit_cursors,
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 20,
            next_commit_ordinal: 0,
        }
    };
    let projection = |iterations: &[(u32, &str)]| DurableOutcomeProjection {
        outcome_id: outcome_id.into(),
        terminal: awaken_session_contract::CommittedOutcomeProjection::Completed(
            awaken_session_contract::OutcomeReport {
                iterations: iterations
                    .iter()
                    .map(
                        |(iteration, result)| awaken_session_contract::OutcomeIteration {
                            messages: Vec::new(),
                            outcome_id: outcome_id.into(),
                            description: "ship".into(),
                            iteration: *iteration,
                            result: (*result).into(),
                            explanation: "test".into(),
                        },
                    )
                    .collect(),
            },
        ),
    };

    let without_terminal = snapshot(vec![(set(format!("outcome/{outcome_id}/state")), 10)]);
    assert!(
        projection(&[(0, "satisfied")])
            .anchor(Some(&without_terminal))
            .is_none(),
        "R1/E1"
    );

    let terminal = snapshot(terminal_commands());
    let anchored = projection(&[(0, "needs_revision"), (1, "interrupted")])
        .anchor(Some(&terminal))
        .expect("R2 terminal projection");
    assert_eq!(anchored.evaluation_commit_cursors.get(&0), Some(&10));
    assert_eq!(anchored.evaluation_commit_cursors.get(&1), Some(&20));
    assert_eq!(anchored.terminal_commit_cursor, 20, "R2/E2+E3");

    assert!(
        projection(&[(0, "needs_revision"), (1, "satisfied")])
            .anchor(Some(&terminal))
            .is_none(),
        "R3/E4"
    );
}

#[tokio::test]
async fn late_outcome_terminal_uses_its_state_commit_and_preserves_an_issued_prefix() {
    // Cause/effect graph: C1 the accepted DefineOutcome and an ordinary Run
    // prefix are already listable; C2 the Outcome terminal is visible before
    // its recovery snapshot anchor; C3 the owner commits evaluation state at
    // cursor 30 while the next terminal observation occurs; C4 a cold peer
    // reads all final facts. E1 C1 remains an immutable prefix; E2 C2 is held
    // outside the public projection; E3 the evaluation trio appears only after
    // the issued cursor; E4 warm/cold full order and old-cursor suffix match.
    // Decision table: O1=C1=>E1; O2=C1+C2=>E1+E2;
    // O3=C1+C2+C3=>E1+E3; O4=C1+C3+C4=>E4. The state-command commit_sequence is the only
    // evaluation anchor; DefineOutcome admission is not reused as a fallback.
    let runtime = LifecycleRuntime::default();
    runtime
        .include_runs_in_snapshot
        .store(true, Ordering::SeqCst);
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let batch = warm
        .application
        .append_session_event_batch(
            &session.id,
            vec![SessionEventInput::DefineOutcome {
                description: "ship the result".into(),
                rubric: SessionOutcomeRubric::Text {
                    content: "correct".into(),
                },
                max_iterations: Some(1),
            }],
            None,
            None,
        )
        .await
        .unwrap();
    let SessionEventCommand::DefineOutcome { outcome_id, .. } = &batch.events[0].event else {
        panic!("O1 retained DefineOutcome")
    };
    let outcome_id = outcome_id.clone();
    persist_batch_projection_anchors(&warm, &session.id, &batch.batch_id, &[5]).await;
    let run_id = RunId("late-outcome-run".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            10,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            20,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.messages.lock().unwrap().push(Message::text(
        MessageId("late-outcome-output".into()),
        Role::Assistant,
        "visible before evaluation",
    ));
    runtime.message_cursors.lock().unwrap().push(19);
    warm.refresh_committed_events(&session.id).await.unwrap();
    let prefix = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let cursor = prefix.last().expect("O1 issued cursor").id.clone();

    runtime.outcome_projections.lock().unwrap().insert(
        outcome_id.clone(),
        awaken_session_contract::CommittedOutcomeProjection::Completed(
            awaken_session_contract::OutcomeReport {
                iterations: vec![awaken_session_contract::OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: outcome_id.clone(),
                    description: "ship the result".into(),
                    iteration: 1,
                    result: "satisfied".into(),
                    explanation: "correct".into(),
                }],
            },
        ),
    );
    warm.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        warm.list_events(&session.id, None, None, false)
            .unwrap()
            .data,
        prefix,
        "O2/E1-E2 unanchored terminal is not public"
    );

    let state_command = |key| {
        awaken_agent_contract::agent::state::Command::set(
            awaken_agent_contract::agent::state::Scope::Thread,
            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
            key,
            serde_json::json!({"committed": true}),
        )
    };
    *runtime.outcome_commit_on_read.lock().unwrap() = vec![
        (
            state_command(format!("outcome/{outcome_id}/evaluation/1")),
            30,
        ),
        (state_command(format!("outcome/{outcome_id}/state")), 30),
        (
            awaken_agent_contract::agent::state::Command::remove(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                "outcome/active",
            ),
            30,
        ),
    ];
    warm.refresh_committed_events(&session.id).await.unwrap();
    let final_warm = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(&final_warm[..prefix.len()], prefix.as_slice(), "O3/E1");
    let warm_suffix = warm
        .list_events(&session.id, Some(&cursor), None, false)
        .unwrap();
    assert_eq!(
        warm_suffix
            .data
            .iter()
            .filter(|event| event.type_str().starts_with("span.outcome_evaluation"))
            .count(),
        3,
        "O3/E3"
    );

    let cold = ManagedState::new(runtime).with_session_repo(repository);
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        cold.list_events(&session.id, None, None, false)
            .unwrap()
            .data,
        final_warm,
        "O4/E4 full order"
    );
    assert_eq!(
        cold.list_events(&session.id, Some(&cursor), None, false)
            .unwrap(),
        warm_suffix,
        "O4/E4 old cursor suffix"
    );
}

#[tokio::test]
async fn unanchored_accepted_command_holds_a_late_runtime_suffix_until_root_cas() {
    // Cause/effect graph: C1 a first committed Run prefix is already listable;
    // C2 a second User command is accepted in the Session root without its
    // processed+anchor CAS; C3 the second Runtime terminal prefix becomes
    // readable; C4 the effect owner then commits the exact root anchor. E1 the
    // C1 prefix gains only the pending User receipt while C2+C3 race; E2 after
    // C4 the same User id is marked processed and Runtime facts appear only in
    // the old-cursor suffix; E3 a cold peer agrees.
    //
    // | Rule | unanchored root | Runtime suffix | Effect |
    // | U1 | no | first prefix | issue cursor |
    // | U2 | yes | visible | E1, publish pending receipt only |
    // | U3 | anchored | visible | E2+E3 |
    //
    // This is the read-skew gate: the final revision reread detects mutation
    // during reads, while the earliest unanchored command prevents publication
    // even when its Runtime effect won before the projector began.
    let runtime = LifecycleRuntime::default();
    runtime
        .include_runs_in_snapshot
        .store(true, Ordering::SeqCst);
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let first_activity = warm.application.begin_activity(&session.id).await.unwrap();
    let first_run = RunId("anchor-gate-first".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            5,
            &session.id,
            &first_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            6,
            &session.id,
            &first_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.messages.lock().unwrap().push(Message::text(
        MessageId("anchor-gate-first-output".into()),
        Role::Assistant,
        "first",
    ));
    runtime.message_cursors.lock().unwrap().push(4);
    warm.application
        .settle_activity_observed(
            &session.id,
            first_activity.activity_epoch,
            Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: first_activity.activity_epoch,
                thread_id: ThreadId(session.id.clone()),
                run_id: first_run,
                lifecycle_cursor: lifecycle_cursor(6),
                source_commit_cursor: 6,
            }),
        )
        .await
        .unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    let prefix = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let cursor = prefix.last().expect("U1 issued cursor").id.clone();

    let batch = warm
        .application
        .append_session_event_batch(
            &session.id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("second")],
            }],
            None,
            None,
        )
        .await
        .unwrap();
    let second_activity = warm.application.begin_activity(&session.id).await.unwrap();
    let second_run = RunId("anchor-gate-second".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            20,
            &session.id,
            &second_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            22,
            &session.id,
            &second_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.messages.lock().unwrap().extend([
        Message::new(
            MessageId::session_event_input(&session.id, batch.events[0].event.operation_id()),
            Role::User,
            vec![ContentBlock::text("second")],
        ),
        Message::text(
            MessageId("anchor-gate-second-output".into()),
            Role::Assistant,
            "second",
        ),
    ]);
    *runtime.message_cursors.lock().unwrap() = vec![4, 19, 21];
    warm.application
        .settle_activity_observed(
            &session.id,
            second_activity.activity_epoch,
            Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: second_activity.activity_epoch,
                thread_id: ThreadId(session.id.clone()),
                run_id: second_run,
                lifecycle_cursor: lifecycle_cursor(22),
                source_commit_cursor: 22,
            }),
        )
        .await
        .unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    let pending_prefix = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(&pending_prefix[..prefix.len()], prefix.as_slice(), "U2/E1");
    assert_eq!(pending_prefix.len(), prefix.len() + 1, "U2/E1");
    assert_eq!(
        pending_prefix.last().unwrap().type_str(),
        "user.message",
        "U2/E1"
    );
    assert!(
        pending_prefix.last().unwrap().processed_at.is_none(),
        "U2/E1"
    );
    let pending_cold = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    pending_cold
        .refresh_committed_events(&session.id)
        .await
        .unwrap();
    let cold_pending_events = pending_cold
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(cold_pending_events.len(), 1, "U2/E1 cold anchor barrier");
    assert_eq!(
        cold_pending_events[0],
        *pending_prefix.last().unwrap(),
        "U2/E1 cold replica retains the root-owned pending receipt"
    );

    persist_batch_projection_anchors(&warm, &session.id, &batch.batch_id, &[19]).await;
    warm.refresh_committed_events(&session.id).await.unwrap();
    let final_warm = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(&final_warm[..prefix.len()], prefix.as_slice(), "U3/E2");
    assert_eq!(
        final_warm[prefix.len()].id,
        pending_prefix[prefix.len()].id,
        "U3/E2 pending receipt upgrades in place"
    );
    assert!(final_warm[prefix.len()].processed_at.is_some(), "U3/E2");
    let warm_suffix = warm
        .list_events(&session.id, Some(&cursor), None, false)
        .unwrap();
    assert!(
        warm_suffix
            .data
            .iter()
            .any(|event| event.type_str() == "user.message"),
        "U3/E2"
    );

    let cold = ManagedState::new(runtime).with_session_repo(repository);
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        cold.list_events(&session.id, None, None, false)
            .unwrap()
            .data,
        final_warm,
        "U3/E3 full order"
    );
    assert_eq!(
        cold.list_events(&session.id, Some(&cursor), None, false)
            .unwrap(),
        warm_suffix,
        "U3/E3 suffix"
    );
}

#[tokio::test]
async fn local_session_update_waits_for_its_accepted_input_predecessor() {
    // Cause/effect graph: C1 `/events` has returned an accepted User receipt but
    // its effect anchor is not committed; C2 PATCH commits session.updated on
    // this process; C3 the User effect and anchor later commit. E1 the transient
    // update is withheld while C1 is unanchored; E2 after C3 local order is
    // User -> session.updated -> later Runtime facts. Decision rules:
    // S1=C1+C2=>E1; S2=C1+C2+C3=>E2. Cross-replica reconstruction of
    // session.updated remains explicitly out of scope; this test preserves the
    // pre-existing same-process behavior without minting another durable log.
    let runtime = LifecycleRuntime::default();
    runtime
        .include_runs_in_snapshot
        .store(true, Ordering::SeqCst);
    let repository = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(runtime.clone()).with_session_repo(repository);
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let batch = state
        .application
        .append_session_event_batch(
            &session.id,
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("accepted")],
            }],
            None,
            None,
        )
        .await
        .unwrap();
    state
        .update_session(
            &session.id,
            awaken_session_application::SessionUpdateCommand {
                title: Some(awaken_session_application::SessionFieldUpdate::Replace(
                    "renamed".into(),
                )),
                metadata: None,
                budget: None,
                tools: None,
                mcp_update: None,
                idempotency_key: None,
                request_fingerprint: awaken_session_contract::stable_fingerprint(&(
                    "local-update-after-receipt",
                    &session.id,
                )),
                if_match: None,
            },
        )
        .await
        .unwrap();
    assert!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .all(|event| event.type_str() != "session.updated"),
        "S1/E1"
    );

    let run_id = RunId("local-update-run".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            20,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            22,
            &session.id,
            &run_id,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.messages.lock().unwrap().extend([
        Message::new(
            MessageId::session_event_input(&session.id, batch.events[0].event.operation_id()),
            Role::User,
            vec![ContentBlock::text("accepted")],
        ),
        Message::text(
            MessageId("local-update-output".into()),
            Role::Assistant,
            "done",
        ),
    ]);
    *runtime.message_cursors.lock().unwrap() = vec![19, 21];
    persist_batch_projection_anchors(&state, &session.id, &batch.batch_id, &[19]).await;
    state.refresh_committed_events(&session.id).await.unwrap();
    let types = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .iter()
        .map(Event::type_str)
        .collect::<Vec<_>>();
    let user = types
        .iter()
        .position(|event_type| *event_type == "user.message")
        .unwrap();
    let updated = types
        .iter()
        .position(|event_type| *event_type == "session.updated")
        .unwrap();
    let running = types
        .iter()
        .position(|event_type| *event_type == "session.status_running")
        .unwrap();
    assert_eq!((user + 1, updated + 1), (updated, running), "S2/E2");
}

#[tokio::test]
async fn primary_reschedule_projects_one_complete_sequence_warm_and_after_restart() {
    // Causes: the fixtures below establish `primary reschedule` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the root Run commits Running; C2 it commits a
    // claim-fenced Rescheduled transition; C3 the replacement commits output
    // and Completed; C4 the projector is live/warm, replayed, or rebuilt cold.
    // Effects: E1 C1 emits primary Running; E2 C2 emits primary Rescheduled
    // then replacement Running; E3 C3 emits primary Idle after output; E4
    // every view has the same ordered deterministic ids exactly once; E5 all
    // payloads use the listed public `sthr_` id while recovery reads the
    // internal Session key.
    //
    // | Rule | C1 | C2 | C3 | View | Effects |
    // | R1 | T | F | F | warm | E1,E5 |
    // | R2 | T | T | F | live/warm | E1,E2,E5 |
    // | R3 | T | T | T | warm/replay | E1-E5 |
    // | R4 | T | T | T | cold restart | E1-E5 |
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let active = warm.application.begin_activity(&session.id).await.unwrap();
    let run_id = RunId("root-rescheduled-run".into());
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    warm.refresh_committed_events(&session.id).await.unwrap();

    let (_snapshot, mut receiver) = warm.stream_subscribe(&session.id).unwrap();
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        2,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Rescheduled,
        RunState::Running,
    ));
    warm.refresh_committed_events(&session.id).await.unwrap();
    let live = std::iter::from_fn(|| receiver.try_recv().ok())
        .filter_map(|event| {
            matches!(
                event.kind,
                OutboundKind::SessionThreadStatusRescheduled { .. }
                    | OutboundKind::SessionThreadStatusRunning { .. }
            )
            .then(|| event.type_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        live,
        vec![
            "session.thread_status_rescheduled",
            "session.thread_status_running"
        ],
        "R2/E2"
    );

    runtime.replace_committed_messages(vec![(
        3,
        Message::text(
            MessageId("root-rescheduled-output".into()),
            Role::Assistant,
            "done after retry",
        ),
    )]);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        &session.id,
        &run_id,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));
    warm.application
        .settle_activity(&session.id, active.activity_epoch)
        .await
        .unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();

    let status_projection = |state: &ManagedState| {
        let primary_id = state
            .list_threads(&session.id)
            .unwrap()
            .into_iter()
            .find(|thread| thread.parent_thread_id.is_none())
            .map(|thread| thread.id)
            .unwrap();
        let statuses = state
            .list_thread_events(&session.id, &primary_id, None, None)
            .unwrap()
            .data
            .into_iter()
            .filter_map(|event| {
                let session_thread_id = match &event.kind {
                    OutboundKind::SessionThreadStatusRunning {
                        session_thread_id, ..
                    }
                    | OutboundKind::SessionThreadStatusRescheduled {
                        session_thread_id, ..
                    }
                    | OutboundKind::SessionThreadStatusIdle {
                        session_thread_id, ..
                    } => session_thread_id.clone(),
                    _ => return None,
                };
                Some((event.type_str(), event.id, session_thread_id))
            })
            .collect::<Vec<_>>();
        (primary_id, statuses)
    };
    let (warm_primary, warm_statuses) = status_projection(&warm);
    assert!(warm_primary.starts_with("sthr_"), "R1-R3/E5");
    assert_eq!(
        warm_statuses
            .iter()
            .map(|(kind, _, _)| *kind)
            .collect::<Vec<_>>(),
        vec![
            "session.thread_status_running",
            "session.thread_status_rescheduled",
            "session.thread_status_running",
            "session.thread_status_idle",
        ],
        "R3/E1-E3"
    );
    assert!(
        warm_statuses
            .iter()
            .all(|(_, _, thread_id)| thread_id == &warm_primary),
        "R3/E5"
    );
    warm.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(status_projection(&warm).1, warm_statuses, "R3/E4 replay");

    let cold = ManagedState::new(runtime).with_session_repo(repository);
    cold.refresh_committed_events(&session.id).await.unwrap();
    let (cold_primary, cold_statuses) = status_projection(&cold);
    assert_eq!(cold_primary, warm_primary, "R4/E5");
    assert_eq!(cold_statuses, warm_statuses, "R4/E4");
}

#[tokio::test]
async fn ordinary_root_failure_keeps_its_event_id_and_cursor_across_restart() {
    // Causes: the fixtures below establish `ordinary root failure` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a Session has no coordinated child link; C2 the
    // root commits a failed lifecycle boundary; C3 the disposable Managed
    // cache is rebuilt with a different local event counter; C4 a client reuses the warm
    // error-event cursor. Effects: E1 the root error is keyed by C2's durable
    // lifecycle cursor; E2 warm/cold error ids are identical; E3 C4 selects
    // the same following page rather than becoming an unknown cursor.
    // Decision table: R1(C1+C2,!C3)->E1; R2(C1+C2+C3)->E2;
    // R3(C1+C2+C3+C4)->E3. Ordinary and coordinated root lifecycle now share
    // one deterministic event-id owner; there is no process-local fallback.
    let runtime = RehydrateFake::default();
    let repo = Arc::new(ephemeral_session_repo());
    let state = ManagedState::new(runtime.clone()).with_session_repo(repo.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let root_run = RunId("run-root-failure".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Failed,
            RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::Inference {
                    code: "provider_unavailable".into(),
                    message: "ordinary root failed".into(),
                },
            )),
        ),
    ]);

    for _ in 0..2 {
        state.next_event_id();
    }
    state.refresh_committed_events(&session.id).await.unwrap();
    let warm_events = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let warm_error_id = warm_events
        .iter()
        .find(|event| event.type_str() == "session.error")
        .expect("R1 root failure event")
        .id
        .clone();
    assert!(
        warm_error_id.starts_with(MANAGED_MULTIAGENT_EVENT_ID_PREFIX),
        "R1/E1"
    );
    let warm_page = state
        .list_events(&session.id, Some(&warm_error_id), Some(3), false)
        .expect("R1 warm error cursor");

    let restarted = ManagedState::new(runtime).with_session_repo(repo);
    for _ in 0..5 {
        restarted.next_event_id();
    }
    restarted.ensure_session(&session.id).await.unwrap();
    let cold_events = restarted
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let cold_error_id = cold_events
        .iter()
        .find(|event| event.type_str() == "session.error")
        .expect("R2 cold root failure event")
        .id
        .clone();
    assert_eq!(cold_error_id, warm_error_id, "R2/E2");
    let cold_page = restarted
        .list_events(&session.id, Some(&warm_error_id), Some(3), false)
        .expect("R3 warm cursor remains valid cold");
    assert_eq!(
        cold_page
            .data
            .iter()
            .map(|event| (event.type_str(), event.id.as_str()))
            .collect::<Vec<_>>(),
        warm_page
            .data
            .iter()
            .map(|event| (event.type_str(), event.id.as_str()))
            .collect::<Vec<_>>(),
        "R3/E3"
    );
    assert_eq!(cold_page.next_page, warm_page.next_page, "R3/E3");
}

#[tokio::test]
async fn child_reschedule_projects_running_rescheduled_running_idle_live_warm_and_cold() {
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Causes: C1 a coordinated child is already Running; C2 its expired
    // dispatch is reclaimed and the claim-fenced RunRescheduled fact commits;
    // C3 the replacement attempt completes; C4 projection is consumed live,
    // refreshed repeatedly warm, or rebuilt cold. Effects: E1 C2 publishes
    // Rescheduled then replacement Running; E2 C3 closes Idle; E3 the final
    // child state is Idle; E4 every view preserves the same ordered ids once.
    //
    // | Rule | C1 | C2 | C3 | View | Effects |
    // |---|---|---|---|---|---|
    // | R1 | T | T | F | live | E1,E3(Running) |
    // | R2 | T | T | T | warm | E1,E2,E3,E4 |
    // | R3 | T | T | T | repeat warm | E4 |
    // | R4 | T | T | T | cold | E1-E4 |
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let repo = Arc::new(ephemeral_session_repo());
    let state = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repo.clone()));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .expect("R1 Session");
    let child_id = "thread-rescheduled";
    let child_run = RunId("run-rescheduled".into());
    runtime.install_agent_coordination_prefix(
        &session.id,
        RunId("root".into()),
        0,
        "rescheduled-child",
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &RunId("root".into()),
                0,
                "rescheduled-child",
            ),
            latest_run_id: Some(child_run.clone()),
        },
    );
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        child_id,
        &child_run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("R1 initial Running");
    let (_snapshot, mut receiver) = state
        .stream_subscribe(&session.id)
        .expect("R1 live subscription");

    runtime.lifecycle.lock().unwrap().push(lifecycle(
        2,
        child_id,
        &child_run,
        RunLifecycleEventKind::Rescheduled,
        RunState::Running,
    ));
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("R1 rescheduled projection");
    let live = std::iter::from_fn(|| receiver.try_recv().ok())
        .filter(|event| {
            matches!(
                &event.kind,
                OutboundKind::SessionThreadStatusRescheduled {
                    session_thread_id,
                    ..
                } | OutboundKind::SessionThreadStatusRunning {
                    session_thread_id,
                    ..
                } if session_thread_id == child_id
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        live.iter().map(Event::type_str).collect::<Vec<_>>(),
        vec![
            "session.thread_status_rescheduled",
            "session.thread_status_running"
        ],
        "R1/E1"
    );
    assert_eq!(
        state.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Running,
        "R1/E3"
    );

    runtime.committed_by_thread.lock().unwrap().insert(
        child_id.into(),
        vec![Message::text(
            MessageId::assistant(&child_run, 0),
            Role::Assistant,
            "recovered answer",
        )],
    );
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        child_id,
        &child_run,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("R2 terminal projection");
    let selected = |events: &[Event]| {
        events
            .iter()
            .filter(|event| {
                matches!(
                    event.type_str(),
                    "session.thread_status_running"
                        | "session.thread_status_rescheduled"
                        | "session.thread_status_idle"
                )
            })
            .map(|event| (event.type_str(), event.id.clone()))
            .collect::<Vec<_>>()
    };
    let warm = selected(
        &state
            .list_thread_events(&session.id, child_id, None, None)
            .expect("R2 warm events")
            .data,
    );
    assert_eq!(
        warm.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
        vec![
            "session.thread_status_running",
            "session.thread_status_rescheduled",
            "session.thread_status_running",
            "session.thread_status_idle",
        ],
        "R2/E1-E2"
    );
    assert_eq!(
        state.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Idle,
        "R2/E3"
    );
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("R3 repeat warm");
    assert_eq!(
        selected(
            &state
                .list_thread_events(&session.id, child_id, None, None)
                .unwrap()
                .data
        ),
        warm,
        "R3/E4"
    );

    let restarted = ManagedState::new(runtime).with_session_repo(repo);
    restarted
        .ensure_session(&session.id)
        .await
        .expect("R4 cold rebuild");
    assert_eq!(
        selected(
            &restarted
                .list_thread_events(&session.id, child_id, None, None)
                .expect("R4 cold events")
                .data
        ),
        warm,
        "R4/E4"
    );
    assert_eq!(
        restarted.get_thread(&session.id, child_id).unwrap().status,
        SessionThreadStatus::Idle,
        "R4/E3"
    );
}

#[tokio::test]
async fn shared_budget_child_boundaries_project_with_terminal_priority_live_warm_and_cold() {
    // Causes: the fixtures below establish `shared budget child boundaries project with terminal
    // priority live warm and cold` with the concrete inputs, state, dependencies, and failure
    // triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the shared root budget is reached after three
    // ordinary children have already started; C2 their exact committed
    // boundaries are Completed, Awaiting with an answerable ticket, and
    // Failed; C3 projection is live, warm, or cold. Effects: E1 Completed
    // remains child Idle(end_turn)—crossing a cap never rewrites Run truth;
    // E2 Awaiting retains Idle(requires_action) and its exact public tool id;
    // E3 Failed remains error→Terminated; E4 requires_action outranks the
    // aggregate budget pause; E5 warm/cold identity is stable. Thread truth
    // comes only from lifecycle; generic cap-transition provenance owns the
    // later aggregate budget projection without rewriting any child.
    //
    // | Rule | Boundary | Budget | Pending | View | Effects |
    // |---|---|---|---|---|---|
    // | B1 | Completed | reached | no | live/warm | E1 |
    // | B2 | Awaiting | reached | yes | live/warm | E2+E4 |
    // | B3 | Failed | reached | no | live/warm | E3+E4 |
    // | B4 | B1-B3 | reached | as above | repeat/cold | E1-E5 |
    let runtime = RehydrateFake::default();
    runtime.delegate_ids.lock().unwrap().extend([
        "researcher".into(),
        "reviewer".into(),
        "critic".into(),
    ]);
    let repo = Arc::new(ephemeral_session_repo());
    let profiles = Arc::new(FrozenToolFamilyProfiles::native_roster(
        "coder",
        &["researcher", "reviewer", "critic"],
    ));
    let state = Arc::new(
        ManagedState::new(runtime.clone())
            .with_config_source(profiles.clone())
            .with_session_repo(repo.clone())
            .with_managed_list_price_provider(Arc::new(ThreadPriceProvider)),
    );
    let session = state
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
            None,
        )
        .await
        .expect("B1 Session");
    let completed = (
        "thread-budget-completed",
        "researcher",
        RunId("run-budget-completed".into()),
    );
    let awaiting = (
        "thread-budget-awaiting",
        "reviewer",
        RunId("run-budget-awaiting".into()),
    );
    let failed = (
        "thread-budget-failed",
        "critic",
        RunId("run-budget-failed".into()),
    );
    for (ordinal, (thread_id, agent_id, run_id)) in
        [&completed, &awaiting, &failed].into_iter().enumerate()
    {
        let coordination_call_id = format!("budget-child-{ordinal}");
        runtime.install_agent_coordination_prefix(
            &session.id,
            RunId("root".into()),
            ordinal,
            &coordination_call_id,
            CoordinatedThreadLink {
                session_id: session.id.clone(),
                thread_id: ThreadId((*thread_id).into()),
                target: CoordinatedThreadTarget::Agent {
                    agent_id: (*agent_id).into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &RunId("root".into()),
                    ordinal,
                    &coordination_call_id,
                ),
                latest_run_id: Some(run_id.clone()),
            },
        );
        runtime.lifecycle.lock().unwrap().push(lifecycle(
            ordinal as u64 + 1,
            thread_id,
            run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ));
    }
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("B1-B3 Running prefix");
    let (_snapshot, mut receiver) = state
        .stream_subscribe(&session.id)
        .expect("B1-B3 live subscription");

    let awaiting_call = "budget-awaiting-call";
    runtime.committed_by_thread.lock().unwrap().insert(
        awaiting.0.into(),
        vec![Message::new(
            MessageId("message-budget-awaiting".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                awaiting_call,
                "client_lookup",
                serde_json::json!({"query":"continue current Run"}),
            )],
        )],
    );
    runtime.pending_by_thread.lock().unwrap().insert(
        awaiting.0.into(),
        Pending {
            tool_use_id: awaiting_call.into(),
            name: "client_lookup".into(),
            input: serde_json::json!({"query":"continue current Run"}),
            client_executed: true,
        },
    );
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            4,
            completed.0,
            &completed.2,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
        lifecycle(
            5,
            awaiting.0,
            &awaiting.2,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
        lifecycle(
            6,
            failed.0,
            &failed.2,
            RunLifecycleEventKind::Failed,
            RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::Inference {
                    code: "provider_error".into(),
                    message: "failed child wins over budget pause".into(),
                },
            )),
        ),
    ]);

    let mut persisted = state
        .application
        .session(&session.id)
        .await
        .expect("B1-B3 persisted Session");
    let awaken_session_contract::SessionBudgetState::Active {
        max_list_cost_minor,
        consumed_numerator,
        ..
    } = &mut persisted.budget
    else {
        panic!("budgeted fixture must persist an active budget")
    };
    *consumed_numerator = u128::from(*max_list_cost_minor)
        * awaken_session_contract::SessionBudgetState::MICROS_PER_MINOR_USD
        * awaken_session_contract::SessionBudgetState::COST_DENOMINATOR;
    persisted
        .budget
        .record_reach_transition()
        .expect("B1-B3 valid transition")
        .expect("B1-B3 first transition");
    state
        .commit_session_snapshot(
            DEFAULT_SCOPE,
            persisted,
            "test-shared-budget-child-provenance",
            Vec::new(),
        )
        .await
        .expect("B1-B3 durable budget provenance");
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("B1-B3 terminal projection");

    let live = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        live.iter().any(|event| matches!(
            &event.kind,
            OutboundKind::SessionThreadStatusIdle {
                session_thread_id,
                stop_reason: StopReason::EndTurn,
                ..
            } if session_thread_id == completed.0
        )),
        "B1/E1 live"
    );
    assert!(
        live.iter().any(|event| matches!(
            &event.kind,
            OutboundKind::SessionThreadStatusIdle {
                session_thread_id,
                stop_reason: StopReason::RequiresAction { event_ids },
                ..
            } if session_thread_id == awaiting.0 && event_ids.len() == 1
        )),
        "B2/E2 live"
    );
    let live_error = live
        .iter()
        .position(|event| event.type_str() == "session.error")
        .expect("B3/E3 live error");
    let live_failed = live
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                OutboundKind::SessionThreadStatusTerminated {
                    session_thread_id,
                    ..
                } if session_thread_id == failed.0
            )
        })
        .expect("B3/E3 live termination");
    assert!(live_error < live_failed, "B3/E3 error precedes termination");
    assert!(
        live.iter().all(|event| !matches!(
            &event.kind,
            OutboundKind::SessionThreadStatusIdle {
                session_thread_id,
                ..
            } if session_thread_id == failed.0
        )),
        "B3/E3 Failed never becomes budget Idle"
    );
    assert!(
        live.iter().any(|event| matches!(
            &event.kind,
            OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids }
            } if event_ids.len() == 1
        )),
        "B2-B3/E4 aggregate requires_action priority"
    );

    let stable_projection = |state: &ManagedState| {
        state
            .list_events(&session.id, None, None, false)
            .expect("stable projection")
            .data
            .into_iter()
            .filter(|event| {
                event.id.starts_with(MANAGED_MULTIAGENT_EVENT_ID_PREFIX)
                    || decode_managed_tool_event_id(&event.id).is_some()
            })
            .map(|event| serde_json::to_value(event).unwrap())
            .collect::<Vec<_>>()
    };
    let warm = stable_projection(&state);
    let warm_count = warm.len();
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("B4 repeat warm");
    assert_eq!(stable_projection(&state), warm, "B4/E5 warm idempotence");
    assert_eq!(stable_projection(&state).len(), warm_count, "B4/E5");

    let restarted = ManagedState::new(runtime)
        .with_config_source(profiles)
        .with_session_repo(repo);
    restarted
        .ensure_session(&session.id)
        .await
        .expect("B4 cold rebuild");
    assert_eq!(stable_projection(&restarted), warm, "B4/E5 cold identity");
    assert_eq!(
        restarted
            .get_thread(&session.id, completed.0)
            .expect("B4 completed child")
            .status,
        SessionThreadStatus::Idle,
        "B4/E1"
    );
    assert_eq!(
        restarted
            .get_thread(&session.id, failed.0)
            .expect("B4 failed child")
            .status,
        SessionThreadStatus::Terminated,
        "B4/E3"
    );
}

#[tokio::test]
async fn ordinary_child_failure_is_error_then_terminated_live_warm_and_cold() {
    // Causes: the fixtures below establish `ordinary child failure` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 an ordinary coordinated child has a committed
    // Running prefix; C2 its latest Run commits terminal Error/Failed; C3 an
    // assistant response exists at that failed boundary; C4 projection is
    // observed in the primary Session, child Thread, live feed, refreshed
    // warm, or rebuilt cold. Effects: E1 publish one child-owned
    // session.error followed by thread_status_terminated in both public
    // views; E2 never publish Idle or a normal thread_message_received
    // report; E3 the Thread remains Terminated; E4 warm/cold replay preserves
    // ids exactly. Constraint: primary visibility reuses the one committed
    // error Event; it must not mint an aggregate duplicate.
    //
    // | Rule | Child | Boundary | View | Effects |
    // |---|---|---|---|---|
    // | F1 | ordinary | Running | warm setup | no terminal |
    // | F2 | ordinary | Failed(Error) | primary+child+live | E1+E2+E3 |
    // | F3 | ordinary | same prefix | warm refresh | E1 once+E4 |
    // | F4 | ordinary | same prefix | cold rebuild | E1-E4 |
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let repo = Arc::new(ephemeral_session_repo());
    let state = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repo.clone()));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .expect("F1 Session");
    let child_id = "thread-terminal-failure";
    let child_run = RunId("run-terminal-failure".into());
    runtime.install_agent_coordination_prefix(
        &session.id,
        RunId("root".into()),
        0,
        "failed-child",
        awaken_session_contract::CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: awaken_session_contract::CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &RunId("root".into()),
                0,
                "failed-child",
            ),
            latest_run_id: Some(child_run.clone()),
        },
    );
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        child_id,
        &child_run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("F1 Running projection");
    assert_eq!(
        state
            .get_thread(&session.id, child_id)
            .expect("F1 child")
            .status,
        SessionThreadStatus::Running,
        "F1 setup"
    );

    let (_snapshot, mut receiver) = state
        .stream_subscribe(&session.id)
        .expect("F2 live subscription");
    runtime.committed_by_thread.lock().unwrap().insert(
        child_id.into(),
        vec![Message::text(
            MessageId::assistant(&child_run, 0),
            Role::Assistant,
            "partial response must not become a successful report",
        )],
    );
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        2,
        child_id,
        &child_run,
        RunLifecycleEventKind::Failed,
        RunState::Ended(EndCause::Error(
            awaken_agent_contract::agent::run::Failure::Inference {
                code: "unauthorized".into(),
                message: "child provider rejected credentials".into(),
            },
        )),
    ));
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("F2 Failed projection");
    let live = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    let live_error = live
        .iter()
        .position(|event| event.type_str() == "session.error")
        .expect("F2/E1 live error");
    let live_terminated = live
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                OutboundKind::SessionThreadStatusTerminated {
                    session_thread_id,
                    ..
                } if session_thread_id == child_id
            )
        })
        .expect("F2/E1 live termination");
    assert!(
        live_error < live_terminated,
        "F2/E1 error precedes terminal"
    );
    assert!(
        live.iter().all(|event| !matches!(
            &event.kind,
            OutboundKind::SessionThreadStatusIdle {
                session_thread_id,
                ..
            } if session_thread_id == child_id
        )),
        "F2/E2 no false reusable Idle"
    );
    assert!(
        live.iter().all(|event| !matches!(
            &event.kind,
            OutboundKind::AgentThreadMessageReceived {
                from_session_thread_id,
                ..
            } if from_session_thread_id == child_id
        )),
        "F2/E2 failed response is not a normal report"
    );
    assert_eq!(
        state
            .get_thread(&session.id, child_id)
            .expect("F2 child")
            .status,
        SessionThreadStatus::Terminated,
        "F2/E3"
    );

    let warm = state
        .list_thread_events(&session.id, child_id, None, None)
        .expect("F3 warm events")
        .data;
    let selected = |events: &[Event]| {
        events
            .iter()
            .filter(|event| {
                matches!(
                    event.type_str(),
                    "session.error" | "session.thread_status_terminated"
                )
            })
            .map(|event| (event.type_str(), event.id.clone()))
            .collect::<Vec<_>>()
    };
    let warm_selected = selected(&warm);
    assert_eq!(warm_selected.len(), 2, "F3/E1 once each");
    let primary_warm = state
        .list_events(&session.id, None, None, false)
        .expect("F3 primary warm events")
        .data;
    assert_eq!(
        selected(&primary_warm),
        warm_selected,
        "F2-F3/E1 primary Session history retains the child error boundary"
    );
    state
        .refresh_committed_events(&session.id)
        .await
        .expect("F3 repeated refresh");
    assert_eq!(
        selected(
            &state
                .list_thread_events(&session.id, child_id, None, None)
                .expect("F3 repeated events")
                .data
        ),
        warm_selected,
        "F3/E4"
    );

    let restarted = ManagedState::new(runtime).with_session_repo(repo);
    restarted
        .ensure_session(&session.id)
        .await
        .expect("F4 cold rebuild");
    assert_eq!(
        restarted
            .get_thread(&session.id, child_id)
            .expect("F4 child")
            .status,
        SessionThreadStatus::Terminated,
        "F4/E3"
    );
    assert_eq!(
        selected(
            &restarted
                .list_thread_events(&session.id, child_id, None, None)
                .expect("F4 cold events")
                .data
        ),
        warm_selected,
        "F4/E4"
    );
    assert_eq!(
        selected(
            &restarted
                .list_events(&session.id, None, None, false)
                .expect("F4 primary cold events")
                .data
        ),
        warm_selected,
        "F4/E1-E4 primary cold history is causally complete"
    );
}

#[tokio::test]
async fn child_pending_tool_projects_and_replies_through_the_parent_partition() {
    // Causes: the fixtures below establish `child pending tool` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C0 the frozen researcher Agent declares
    // `client_lookup` as a custom client tool; C1 a real coordinated child
    // is Running; C2 the primary terminal arrives while C1 remains active;
    // C3 the child ToolUse Message becomes visible before either result or
    // ticket; C4 Awaiting then becomes visible before its exact client-executed
    // ticket; C5 that ticket catches up; C6 the client replies with the matching
    // qualified Event id; C7 the same reply is stale/duplicate; C8 its
    // validation read may also observe unrelated committed Runtime facts.
    // Effects: E1 primary idle is deferred; E2 C3 emits neither a premature
    // tool event nor terminal and retains the source occurrence; E3 C4 keeps
    // lifecycle and transcript behind the ticket fence; E4 C5 emits one
    // child-owned tool event, cross-posted to primary, and aggregate idle names
    // that exact id; E5 C6 uses the parent-partition typed reply port and the
    // child stream sees the input; E6 C7 is rejected before another matching
    // receipt or delivery, independently of C8. K1 ResumeTicket is the pending
    // authority; K2 root and child reuse the same occurrence classifier.
    // Total event count is not an atomicity oracle because the sole
    // committed projector remains live during validation.
    // Decision table:
    // | Rule | Message | Awaiting | Ticket | Reply | Duplicate | Effect |
    // | P1 | absent | no | no | no | no | E1 |
    // | P2 | present | no | no | no | no | E2 |
    // | P3 | present | yes | no | no | no | E3 |
    // | P4 | present | yes | exact | no | no | E4 |
    // | P5 | present | yes | exact | exact | no | E5 |
    // | P6 | present | yes | consumed | repeat | yes | E6 |
    let (runtime, state, session_id, child_id, call_id, public_call_id) = Box::pin(async {
        let runtime = RehydrateFake::default();
        let state = Arc::new(
            ManagedState::new(runtime.clone()).with_config_source(Arc::new(
                FrozenToolFamilyProfiles::uniform(
                    "coder",
                    &["researcher"],
                    FrozenTestToolFamily::Custom,
                    "client_lookup",
                ),
            )),
        );
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent":"coder", "environment_id":"env_local"
                }))
                .unwrap(),
                None,
            )
            .await
            .unwrap();
        let child_id = "sthr_pending_child";
        let child_run = RunId("run-pending-child".into());
        runtime.install_agent_coordination_prefix(
            &session.id,
            RunId("root".into()),
            0,
            "call-child",
            CoordinatedThreadLink {
                session_id: session.id.clone(),
                thread_id: ThreadId(child_id.into()),
                target: CoordinatedThreadTarget::Agent {
                    agent_id: "researcher".into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &RunId("root".into()),
                    0,
                    "call-child",
                ),
                latest_run_id: Some(child_run.clone()),
            },
        );
        let root_run = RunId("run-primary-with-active-child".into());
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                1,
                child_id,
                &child_run,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                2,
                &session.id,
                &root_run,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                3,
                &session.id,
                &root_run,
                RunLifecycleEventKind::Completed,
                RunState::Ended(EndCause::NaturalEnd),
            ),
        ]);
        state.refresh_committed_events(&session.id).await.unwrap();
        assert!(
            !state
                .list_events(&session.id, None, None, false)
                .unwrap()
                .data
                .iter()
                .any(|event| event.type_str() == "session.status_idle"),
            "P1/E1"
        );

        let call_id = "call-child-client";
        runtime.committed_by_thread.lock().unwrap().insert(
            child_id.into(),
            vec![Message::new(
                MessageId("child-client-tool".into()),
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    call_id,
                    "client_lookup",
                    serde_json::json!({"query":"facts"}),
                )],
            )],
        );
        state.refresh_committed_events(&session.id).await.unwrap();
        assert_eq!(
            state.lifecycle_cursor(&session.id).unwrap(),
            lifecycle_cursor(3),
            "P2/E2 message-only prefix advances no lifecycle"
        );
        let message_first = state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data;
        assert!(
            message_first.iter().all(|event| {
                !matches!(
                    event.type_str(),
                    "agent.custom_tool_use"
                        | "agent.tool_use"
                        | "agent.mcp_tool_use"
                        | "session.thread_status_idle"
                )
            }),
            "P2/E2 no premature tool or terminal"
        );
        runtime.lifecycle.lock().unwrap().push(lifecycle(
            4,
            child_id,
            &child_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ));
        state.refresh_committed_events(&session.id).await.unwrap();

        assert_eq!(
            state.lifecycle_cursor(&session.id).unwrap(),
            lifecycle_cursor(3),
            "P3/E3 Awaiting remains behind the exact-ticket fence"
        );
        assert!(
            state
                .list_events(&session.id, None, None, false)
                .unwrap()
                .data
                .iter()
                .all(|event| event.type_str() != "agent.custom_tool_use"),
            "P3/E3 transcript cannot be marked/projected ahead of its ticket"
        );
        runtime.pending_by_thread.lock().unwrap().insert(
            child_id.into(),
            Pending {
                tool_use_id: call_id.into(),
                name: "client_lookup".into(),
                input: serde_json::json!({"query":"facts"}),
                client_executed: true,
            },
        );
        state.refresh_committed_events(&session.id).await.unwrap();

        let primary =
            serde_json::to_value(state.list_events(&session.id, None, None, false).unwrap())
                .unwrap();
        let primary_tool = primary["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| {
                event["type"] == "agent.custom_tool_use" && event["session_thread_id"] == child_id
            })
            .expect("P4/E4 primary cross-post");
        let public_call_id = primary_tool["id"].as_str().unwrap().to_string();
        assert_eq!(primary_tool["type"], "agent.custom_tool_use", "P4/E4");
        assert_eq!(primary_tool["session_thread_id"], child_id, "P4/E4");
        assert_eq!(
            decode_managed_tool_event_id(&public_call_id)
                .expect("P4/E4 qualified id")
                .call_id,
            call_id,
            "P4/E4"
        );
        let aggregate_idle = primary["data"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|event| event["type"] == "session.status_idle")
            .expect("P4/E4 aggregate idle");
        assert_eq!(
            aggregate_idle["stop_reason"]["event_ids"],
            serde_json::json!([public_call_id.clone()]),
            "P4/E4"
        );
        let child = serde_json::to_value(
            state
                .list_thread_events(&session.id, child_id, None, None)
                .unwrap(),
        )
        .unwrap();
        let child_tool = child["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["id"] == public_call_id)
            .expect("P4/E4 child owner projection");
        assert!(
            child_tool.get("session_thread_id").is_none(),
            "P4/E4 child-local event needs no routing hint"
        );

        (
            runtime,
            state,
            session.id,
            child_id,
            call_id,
            public_call_id,
        )
    })
    .await;

    Box::pin(async {
        let reply = || SendEventsRequest {
            events: vec![InboundEvent::UserCustomToolResult {
                custom_tool_use_id: public_call_id.clone(),
                content: Some(vec![ContentBlock::text("found")]),
                is_error: false,
            }],
        };
        state.send_events(&session_id, reply()).await.unwrap();
        {
            let replies = runtime.thread_tool_replies.lock().unwrap();
            assert_eq!(replies.len(), 1, "P5/E5");
            assert_eq!(replies[0].session_id, session_id, "P5/E5");
            assert_eq!(
                replies[0].target,
                SessionThreadTarget::Child(ThreadId(child_id.into())),
                "P5/E5"
            );
            assert_eq!(replies[0].tool_use_id, call_id, "P5/E5");
            assert!(
                matches!(
                    &replies[0].reply,
                    SessionThreadToolReply::Custom { content, is_error }
                        if !is_error && content == &vec![ContentBlock::text("found")]
                ),
                "P5/E5"
            );
        }
        assert!(
            state
                .list_thread_events(&session_id, child_id, None, None)
                .unwrap()
                .data
                .iter()
                .any(|event| matches!(
                    &event.kind,
                    OutboundKind::UserCustomToolResult {
                        session_thread_id: Some(target), ..
                    } if target == child_id
                )),
            "P5/E5"
        );

        let before = state
            .list_events(&session_id, None, None, false)
            .unwrap()
            .data
            .iter()
            .filter(|event| event.type_str() == "user.custom_tool_result")
            .count();
        assert!(
            state.send_events(&session_id, reply()).await.is_err(),
            "P6/E6"
        );
        assert_eq!(
            state
                .list_events(&session_id, None, None, false)
                .unwrap()
                .data
                .iter()
                .filter(|event| event.type_str() == "user.custom_tool_result")
                .count(),
            before,
            "P6/E6 no duplicate matching receipt"
        );
        assert_eq!(
            runtime.thread_tool_replies.lock().unwrap().len(),
            1,
            "P6/E6"
        );
    })
    .await;
}

#[tokio::test]
async fn completed_tool_only_child_revisits_withheld_occurrences_before_results() {
    // Cause/effect graph: C1 a coordinated child commits one tool-only Assistant
    // Message with two ordered ToolUse occurrences; C2 its first Awaiting ticket
    // classifies only the first occurrence; C3 interruption later commits two
    // ordered error results and Completed, but no non-empty assistant report;
    // C4 the terminal prefix is refreshed warm, replayed, or rebuilt cold.
    // Effects: E1 C1+C2 emits exactly one ask and retains the source Message; E2
    // C3 falls through report classification, appends only the missing sibling
    // before both exact results, consumes the source, and emits no
    // thread_message_received; E3 C4 preserves the two stable occurrence ids and
    // result references without duplicates. K1 canonical non-empty
    // `session_agent_report_text` alone owns report continuation; K2 the shared
    // occurrence projector and append-only Event log own partial replay; K3
    // ToolResult identity validation remains strict.
    //
    // | Rule | Ticket | Results | Report text | Projection | Effects |
    // |---|---|---|---|---|---|
    // | T1 | first exact | absent | empty | first warm | E1 |
    // | T2 | consumed | both exact | empty | warm/replay | E2,E3 |
    // | T3 | consumed | both exact | empty | cold full | E2,E3 |
    let runtime = RehydrateFake::default();
    let profiles = Arc::new(FrozenToolFamilyProfiles::uniform(
        "coder",
        &["researcher"],
        FrozenTestToolFamily::AgentAlwaysAsk,
        "write",
    ));
    let repo = Arc::new(ephemeral_session_repo());
    let warm = ManagedState::new(runtime.clone())
        .with_config_source(profiles.clone())
        .with_session_repo(repo.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let child_id = "thread-tool-only-terminal";
    let child_run = RunId("run-tool-only-terminal".into());
    let first_call = "tool-only-first";
    let sibling_call = "tool-only-sibling";
    runtime.install_agent_coordination_prefix(
        &session.id,
        RunId("root".into()),
        0,
        "tool-only-terminal",
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &RunId("root".into()),
                0,
                "tool-only-terminal",
            ),
            latest_run_id: Some(child_run.clone()),
        },
    );
    let tool_message = Message::new(
        MessageId::assistant(&child_run, 0),
        Role::Assistant,
        vec![
            ContentBlock::tool_use(first_call, "write", serde_json::json!({"path":"first.txt"})),
            ContentBlock::tool_use(sibling_call, "bash", serde_json::json!({"command":"false"})),
        ],
    );
    runtime
        .committed_by_thread
        .lock()
        .unwrap()
        .insert(child_id.into(), vec![tool_message.clone()]);
    runtime.pending_by_thread.lock().unwrap().insert(
        child_id.into(),
        Pending {
            tool_use_id: first_call.into(),
            name: "write".into(),
            input: serde_json::json!({"path":"first.txt"}),
            client_executed: false,
        },
    );
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            child_id,
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            child_id,
            &child_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);

    warm.refresh_committed_events(&session.id).await.unwrap();
    let initial = warm
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data;
    let initial_tools = initial
        .iter()
        .filter(|event| event.type_str() == "agent.tool_use")
        .collect::<Vec<_>>();
    assert_eq!(initial_tools.len(), 1, "T1/E1 exact classified ask only");
    assert!(
        matches!(
            initial_tools[0].kind,
            OutboundKind::AgentToolUse {
                evaluated_permission: Some(EvaluatedPermission::Ask),
                ..
            }
        ),
        "T1/E1"
    );
    assert_eq!(
        decode_managed_tool_event_id(&initial_tools[0].id)
            .expect("T1/E1 source-qualified first ask")
            .call_id,
        first_call,
        "T1/E1"
    );
    assert!(
        !warm.sessions.lock().unwrap()[&session.id]
            .message_was_projected(child_id, &tool_message.id.0),
        "T1/E1 withheld sibling retains the source"
    );

    runtime.committed_by_thread.lock().unwrap().insert(
        child_id.into(),
        vec![
            tool_message.clone(),
            Message::new(
                MessageId("tool-only-first-result".into()),
                Role::Tool,
                vec![ContentBlock::tool_result_with_error(
                    first_call,
                    vec![ContentBlock::text("interrupted")],
                    true,
                )],
            ),
            Message::new(
                MessageId("tool-only-sibling-result".into()),
                Role::Tool,
                vec![ContentBlock::tool_result_with_error(
                    sibling_call,
                    vec![ContentBlock::text("interrupted")],
                    true,
                )],
            ),
        ],
    );
    runtime.pending_by_thread.lock().unwrap().remove(child_id);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        child_id,
        &child_run,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));

    warm.refresh_committed_events(&session.id).await.unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    let warm_child = warm
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data;
    let warm_tools = warm_child
        .iter()
        .filter(|event| event.type_str() == "agent.tool_use")
        .collect::<Vec<_>>();
    let warm_results = warm_child
        .iter()
        .filter(|event| event.type_str() == "agent.tool_result")
        .collect::<Vec<_>>();
    assert_eq!(warm_tools.len(), 2, "T2/E2 both occurrences exactly once");
    assert_eq!(warm_results.len(), 2, "T2/E2 both results exactly once");
    assert_eq!(
        warm_tools
            .iter()
            .map(|event| {
                decode_managed_tool_event_id(&event.id)
                    .expect("T2/E2 source-qualified call")
                    .call_id
            })
            .collect::<Vec<_>>(),
        vec![first_call, sibling_call],
        "T2/E2 original ToolBatch order"
    );
    assert_eq!(
        warm_results
            .iter()
            .map(|event| match &event.kind {
                OutboundKind::AgentToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => (tool_use_id.as_str(), *is_error),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        vec![
            (warm_tools[0].id.as_str(), Some(true)),
            (warm_tools[1].id.as_str(), Some(true)),
        ],
        "T2/E2 strict results reference their preceding public occurrences"
    );
    assert!(
        warm.sessions.lock().unwrap()[&session.id]
            .message_was_projected(child_id, &tool_message.id.0),
        "T2/E2 fully classified source is consumed"
    );
    assert!(
        warm.list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .all(|event| !matches!(event.kind, OutboundKind::AgentThreadMessageReceived { .. })),
        "T2/E2 empty tool-only output is not a coordination report"
    );

    let signature = |events: &[Event]| {
        events
            .iter()
            .filter_map(|event| match &event.kind {
                OutboundKind::AgentToolUse { .. } => {
                    Some((event.type_str(), event.id.clone(), None, None))
                }
                OutboundKind::AgentToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some((
                    event.type_str(),
                    event.id.clone(),
                    Some(tool_use_id.clone()),
                    *is_error,
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let warm_signature = signature(&warm_child);
    warm.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        signature(
            &warm
                .list_thread_events(&session.id, child_id, None, None)
                .unwrap()
                .data
        ),
        warm_signature,
        "T2/E3 warm replay"
    );

    let cold = ManagedState::new(runtime)
        .with_config_source(profiles)
        .with_session_repo(repo);
    cold.ensure_session(&session.id).await.unwrap();
    let cold_child = cold
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data;
    assert_eq!(
        signature(&cold_child),
        warm_signature,
        "T3/E3 cold identity"
    );
    assert!(
        cold.sessions.lock().unwrap()[&session.id]
            .message_was_projected(child_id, &tool_message.id.0),
        "T3/E2 cold source consumed"
    );
    assert!(
        cold.list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .all(|event| !matches!(event.kind, OutboundKind::AgentThreadMessageReceived { .. })),
        "T3/E2 no cold empty report"
    );
}

#[tokio::test]
async fn batch_local_tool_ids_are_thread_qualified_stable_and_reply_reversible() {
    // Causes: the fixtures below establish `batch local tool ids` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C0 the frozen root/researcher/reviewer Agents all
    // declare `client_lookup` as the same-named custom client tool; C1 root
    // and two coordinated children reuse one Runtime call id; C2 both
    // children also reuse one provider MessageId; C3 all three are current
    // client-executed waits; C4 a fresh process rebuilds from the same
    // durable facts; C5 the client replies after that restart using child
    // A's public event id and omits the optional selector; C6 a warm
    // pagination cursor is reused cold.
    // Effects: E1
    // three distinct tool ids/owners plus distinct non-tool child event ids
    // and unique pagination cursors; E2 the aggregate requires_action lists
    // those exact ids; E3 a fresh process reconstructs every multiagent/tool
    // `(type,id)` in the same parent and child order; E4 C5 decodes only at
    // admission and delivers the original Runtime call id to child A,
    // without consuming root or child B; E5 C6 names the same next page.
    // Generic inbound/usage/preview ids are intentionally outside E3: their
    // owners expose no durable per-event coordinate and this test must not
    // disguise that limitation with a projector helper.
    // Decision table:
    // | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | Effects |
    // | Q1 | T | T | T | T | F | F | F | E1,E2 |
    // | Q2 | T | T | T | T | T | F | T | E1,E3,E5 |
    // | Q3 | T | T | T | T | T | T | T | E4 |
    let runtime = RehydrateFake::default();
    let profiles = Arc::new(FrozenToolFamilyProfiles::uniform(
        "coder",
        &["researcher", "reviewer"],
        FrozenTestToolFamily::Custom,
        "client_lookup",
    ));
    let repo = Arc::new(ephemeral_session_repo());
    let state = Arc::new(
        ManagedState::new(runtime.clone())
            .with_config_source(profiles.clone())
            .with_session_repo(repo.clone()),
    );
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let runtime_call_id = "batch-local-call";
    let root_run = RunId("run-qualified-root".into());
    let child_a = ("thread-qualified-a", "researcher", "run-qualified-a");
    let child_b = ("thread-qualified-b", "reviewer", "run-qualified-b");
    *runtime.committed.lock().unwrap() = Some(vec![Message::new(
        MessageId("message-qualified-root".into()),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            runtime_call_id,
            "client_lookup",
            serde_json::json!({"owner":"root"}),
        )],
    )]);
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: runtime_call_id.into(),
        name: "client_lookup".into(),
        input: serde_json::json!({"owner":"root"}),
        client_executed: true,
    });
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);
    for (ordinal, (thread_id, agent_id, run_id)) in [child_a, child_b].into_iter().enumerate() {
        let run_id = RunId(run_id.into());
        runtime.install_agent_coordination_prefix(
            &session.id,
            RunId("root".into()),
            ordinal,
            &format!("spawn-{thread_id}"),
            CoordinatedThreadLink {
                session_id: session.id.clone(),
                thread_id: ThreadId(thread_id.into()),
                target: CoordinatedThreadTarget::Agent {
                    agent_id: agent_id.into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &RunId("root".into()),
                    ordinal,
                    &format!("spawn-{thread_id}"),
                ),
                latest_run_id: Some(run_id.clone()),
            },
        );
        runtime.committed_by_thread.lock().unwrap().insert(
            thread_id.into(),
            vec![Message::new(
                MessageId("provider-reused-child-message".into()),
                Role::Assistant,
                vec![
                    ContentBlock::text(format!("answer from {thread_id}")),
                    ContentBlock::tool_use(
                        runtime_call_id,
                        "client_lookup",
                        serde_json::json!({"owner":thread_id}),
                    ),
                ],
            )],
        );
        runtime.pending_by_thread.lock().unwrap().insert(
            thread_id.into(),
            Pending {
                tool_use_id: runtime_call_id.into(),
                name: "client_lookup".into(),
                input: serde_json::json!({"owner":thread_id}),
                client_executed: true,
            },
        );
        let cursor = 3 + ordinal as u64 * 2;
        runtime.lifecycle.lock().unwrap().extend([
            lifecycle(
                cursor,
                thread_id,
                &run_id,
                RunLifecycleEventKind::Running,
                RunState::Running,
            ),
            lifecycle(
                cursor + 1,
                thread_id,
                &run_id,
                RunLifecycleEventKind::Awaiting,
                RunState::Awaiting,
            ),
        ]);
    }

    state.refresh_committed_events(&session.id).await.unwrap();
    let primary_events = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let tool_events = primary_events
        .iter()
        .filter(|event| event.type_str() == "agent.custom_tool_use")
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 3, "Q1/E1");
    let public_ids = tool_events
        .iter()
        .map(|event| event.id.clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(public_ids.len(), 3, "Q1/E1");
    assert_eq!(
        primary_events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        primary_events.len(),
        "Q1/E1 pagination ids"
    );
    for event in &tool_events {
        assert_eq!(
            decode_managed_tool_event_id(&event.id)
                .expect("Q1/E1 qualified identity")
                .call_id,
            runtime_call_id,
            "Q1/E1"
        );
    }
    let requires_action = primary_events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids },
            } => Some(event_ids.clone()),
            _ => None,
        })
        .expect("Q1/E2 aggregate requires_action");
    assert_eq!(
        requires_action
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>(),
        public_ids,
        "Q1/E2"
    );

    let child_message_ids = [child_a.0, child_b.0].map(|thread_id| {
        state
            .list_thread_events(&session.id, thread_id, None, None)
            .unwrap()
            .data
            .into_iter()
            .find(|event| event.type_str() == "agent.message")
            .expect("Q1/E1 child message")
            .id
    });
    assert_ne!(child_message_ids[0], child_message_ids[1], "Q1/E1");

    let stable_signature = |events: &[Event]| {
        events
            .iter()
            .filter(|event| {
                event.id.starts_with(MANAGED_MULTIAGENT_EVENT_ID_PREFIX)
                    || decode_managed_tool_event_id(&event.id).is_some()
            })
            .map(|event| (event.type_str().to_string(), event.id.clone()))
            .collect::<Vec<_>>()
    };
    let warm_primary_signature = stable_signature(&primary_events);
    let warm_child_signatures = [child_a.0, child_b.0].map(|thread_id| {
        state
            .list_thread_events(&session.id, thread_id, None, None)
            .unwrap()
            .data
            .into_iter()
            .map(|event| (event.type_str().to_string(), event.id))
            .collect::<Vec<_>>()
    });
    let warm_cursor = primary_events
        .iter()
        .find(|event| event.type_str() == "session.thread_created")
        .expect("Q2/C6 stable parent cursor")
        .id
        .clone();
    let warm_page = state
        .list_events(&session.id, Some(&warm_cursor), Some(3), false)
        .expect("Q2/C6 warm page");
    let warm_page_signature = warm_page
        .data
        .iter()
        .map(|event| (event.type_str().to_string(), event.id.clone()))
        .collect::<Vec<_>>();
    let warm_next_page = warm_page.next_page.clone();

    let restarted = Arc::new(
        ManagedState::new(runtime.clone())
            .with_config_source(profiles)
            .with_session_repo(repo.clone()),
    );
    restarted.ensure_session(&session.id).await.unwrap();
    let recovered_ids = restarted
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .filter(|event| event.type_str() == "agent.custom_tool_use")
        .map(|event| event.id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(recovered_ids, public_ids, "Q2/E3");
    let cold_primary = restarted
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        stable_signature(&cold_primary),
        warm_primary_signature,
        "Q2/E3 parent type/id/order"
    );
    for (index, thread_id) in [child_a.0, child_b.0].into_iter().enumerate() {
        let cold_child = restarted
            .list_thread_events(&session.id, thread_id, None, None)
            .unwrap()
            .data
            .into_iter()
            .map(|event| (event.type_str().to_string(), event.id))
            .collect::<Vec<_>>();
        assert_eq!(
            cold_child, warm_child_signatures[index],
            "Q2/E3 child type/id/order"
        );
    }
    let cold_page = restarted
        .list_events(&session.id, Some(&warm_cursor), Some(3), false)
        .expect("Q2/E5 warm cursor remains valid cold");
    assert_eq!(
        cold_page
            .data
            .iter()
            .map(|event| (event.type_str().to_string(), event.id.clone()))
            .collect::<Vec<_>>(),
        warm_page_signature,
        "Q2/E5"
    );
    assert_eq!(cold_page.next_page, warm_next_page, "Q2/E5");
    if let Some(cursor) = warm_next_page.as_deref() {
        restarted
            .list_events(&session.id, Some(cursor), Some(3), false)
            .expect("Q2/E5 old next-page cursor remains valid");
    }

    let child_a_public_id = restarted
        .list_thread_events(&session.id, child_a.0, None, None)
        .unwrap()
        .data
        .into_iter()
        .find(|event| event.type_str() == "agent.custom_tool_use")
        .expect("Q3 child A tool event")
        .id;
    restarted
        .send_events(
            &session.id,
            SendEventsRequest {
                events: vec![InboundEvent::UserCustomToolResult {
                    custom_tool_use_id: child_a_public_id,
                    content: Some(vec![ContentBlock::text("child A result")]),
                    is_error: false,
                }],
            },
        )
        .await
        .unwrap();
    let replies = runtime.thread_tool_replies.lock().unwrap();
    assert_eq!(replies.len(), 1, "Q3/E4");
    assert_eq!(
        replies[0].target,
        SessionThreadTarget::Child(ThreadId(child_a.0.into())),
        "Q3/E4"
    );
    assert_eq!(replies[0].tool_use_id, runtime_call_id, "Q3/E4");
    assert!(
        runtime
            .pending_by_thread
            .lock()
            .unwrap()
            .contains_key(child_b.0),
        "Q3/E4 child B remains pending"
    );
    assert!(
        runtime.pending.lock().unwrap().is_some(),
        "Q3/E4 root remains pending"
    );
}

#[tokio::test]
async fn accepted_agent_cross_post_is_anchored_to_the_late_tool_result() {
    // Cause/effect graph: C1 a root send_to_agent ToolUse is listable at
    // cursor 10 with no accepted link; C2 an accepted ToolResult and its
    // Runtime-owned link become visible at cursor 30; C3 a cold peer reads the
    // final facts. E1 C1 remains an immutable prefix; E2 thread-created and
    // message-sent first appear in the old-cursor suffix; E3 warm/cold full
    // order and suffix match. Decision table: A1=C1=>E1/no link events;
    // A2=C1+C2=>E1+E2; A3=C1+C2+C3=>E3. ToolUse is intent only: the existing
    // accepted ToolResult message commit is the immutable cross-post anchor.
    let repository = Arc::new(ephemeral_session_repo());
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let profiles = Arc::new(FrozenToolFamilyProfiles::native_roster(
        "coder",
        &["researcher"],
    ));
    let warm = ManagedState::new(runtime.clone())
        .with_config_source(profiles.clone())
        .with_session_repo(repository.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let root_run = RunId("late-accepted-root".into());
    let child_run = RunId("late-accepted-child".into());
    let child_id = "late-accepted-thread";
    let call_id = "late-accepted-call";
    let root_activity = warm.application.begin_activity(&session.id).await.unwrap();
    let tool_use = Message::new(
        MessageId::assistant(&root_run, 0),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            call_id,
            SEND_TO_AGENT,
            serde_json::json!({
                "agent_id":"researcher",
                "message":"investigate"
            }),
        )],
    );
    *runtime.committed.lock().unwrap() = Some(vec![tool_use.clone()]);
    runtime
        .message_cursors_by_thread
        .lock()
        .unwrap()
        .insert(session.id.clone(), vec![10]);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        5,
        &session.id,
        &root_run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    warm.refresh_committed_events(&session.id).await.unwrap();
    let prefix = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert!(prefix.iter().all(|event| !matches!(
        event.type_str(),
        "session.thread_created" | "agent.thread_message_sent"
    )));
    let cursor = prefix.last().expect("A1 issued cursor").id.clone();

    let accepted = Message::new(
        MessageId("late-accepted-result".into()),
        Role::Tool,
        vec![ContentBlock::tool_result(
            call_id,
            vec![ContentBlock::text(
                serde_json::json!({
                    "accepted":true,
                    "session_thread_id":child_id
                })
                .to_string(),
            )],
        )],
    );
    *runtime.committed.lock().unwrap() = Some(vec![tool_use, accepted]);
    runtime
        .message_cursors_by_thread
        .lock()
        .unwrap()
        .insert(session.id.clone(), vec![10, 30]);
    runtime
        .coordinated
        .lock()
        .unwrap()
        .push(CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(&root_run, 0, call_id),
            latest_run_id: Some(child_run.clone()),
        });
    let child_activity = warm.application.begin_activity(&session.id).await.unwrap();
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            31,
            child_id,
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            32,
            child_id,
            &child_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
        lifecycle(
            33,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    warm.application
        .settle_activity_observed(
            &session.id,
            child_activity.activity_epoch,
            Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: child_activity.activity_epoch,
                thread_id: ThreadId(child_id.into()),
                run_id: child_run.clone(),
                lifecycle_cursor: lifecycle_cursor(32),
                source_commit_cursor: 32,
            }),
        )
        .await
        .unwrap();
    warm.application
        .settle_activity_observed(
            &session.id,
            root_activity.activity_epoch,
            Some(awaken_session_contract::SessionRuntimeIntervalObservation {
                activity_epoch: root_activity.activity_epoch,
                thread_id: ThreadId(session.id.clone()),
                run_id: root_run.clone(),
                lifecycle_cursor: lifecycle_cursor(33),
                source_commit_cursor: 33,
            }),
        )
        .await
        .unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    let final_warm = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(&final_warm[..prefix.len()], prefix.as_slice(), "A2/E1");
    let warm_suffix = warm
        .list_events(&session.id, Some(&cursor), None, false)
        .unwrap();
    for event_type in ["session.thread_created", "agent.thread_message_sent"] {
        assert_eq!(
            warm_suffix
                .data
                .iter()
                .filter(|event| event.type_str() == event_type)
                .count(),
            1,
            "A2/E2 {event_type}"
        );
    }

    let cold = ManagedState::new(runtime)
        .with_config_source(profiles)
        .with_session_repo(repository);
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        cold.list_events(&session.id, None, None, false)
            .unwrap()
            .data,
        final_warm,
        "A3/E3 full order"
    );
    assert_eq!(
        cold.list_events(&session.id, Some(&cursor), None, false)
            .unwrap(),
        warm_suffix,
        "A3/E3 old cursor suffix"
    );
}

#[tokio::test]
async fn child_message_projection_is_independent_of_refresh_batch_grouping() {
    // Causes: the fixtures below establish `child message projection` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 one append-only child transcript contains an
    // assistant thinking/message/MCP call, its later tool result, and a
    // second assistant report; C2 a warm projector first observes only the
    // ToolUse Message, with neither result nor ticket, and then the full
    // transcript; C3 a cold projector observes
    // the full transcript in one read; C4 MCP classification must cross the
    // Message boundary; C5 root and child committed usage snapshots differ;
    // C6 the Session owns one immutable price snapshot; C7 aggregate usage
    // contains root plus the ordinary child exactly once; C8 the terminal
    // aggregate is projected again warm and then rebuilt cold.
    // Effects: E0 the C2 prefix emits its non-tool thinking/message exactly
    // once but no unclassified MCP call; E1 child `(type,id)` order is identical;
    // E2 primary multiagent/tool `(type,id)` order, including both reports,
    // is identical; E3 every child id is durable-derived and unique; E4 the
    // result references the same qualified tool id in both projections; E5
    // E5 primary/child usage stays attributed while Session total sums once;
    // E6 each Thread is priced and rounded independently from C6; E7 the
    // retrieved Session and its terminal `session.usage` both price C7 as 15
    // from C6 in warm, replay, and cold views. E7 instantiates pricing rule
    // R4 from the shared helper test: cumulative prefix + frozen creation
    // snapshot + replay must remain one pure stable value.
    //
    // Decision table:
    // K1 ToolResult is the only executed-call authority; K2 a source Message
    // remains revisitable until every ToolUse occurrence is classified.
    // | Rule | Transcript | Refresh grouping | MCP crosses Message | C5,C6 | Effect |
    // | B0 | call-only prefix | first warm read | pending | T | E0 |
    // | B1 | same | prefix then full | yes | T | E0,E1,E2,E3,E4,E5,E6,E7(R4 warm/replay) |
    // | B2 | same | full cold read | yes | T | E1,E2,E3,E4,E5,E6,E7(R4 cold) |
    let repo = Arc::new(ephemeral_session_repo());
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let profiles = Arc::new(FrozenToolFamilyProfiles::native_roster(
        "coder",
        &["researcher"],
    ));
    let warm = ManagedState::new(runtime.clone())
        .with_config_source(profiles.clone())
        .with_session_repo(repo.clone())
        .with_managed_list_price_provider(Arc::new(ThreadPriceProvider));
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder",
                "environment_id":"env_local",
                "budget": {
                    "type":"limit",
                    "max_list_cost":{"amount":"100","currency":"USD"}
                }
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let child_id = "thread-batch-independent";
    let child_run = RunId("run-batch-independent".into());
    let parent_call_id = "batch-independent";
    runtime.install_agent_coordination_prefix(
        &session.id,
        RunId("root".into()),
        0,
        parent_call_id,
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &RunId("root".into()),
                0,
                "batch-independent",
            ),
            latest_run_id: Some(child_run.clone()),
        },
    );
    runtime.usage_by_thread.lock().unwrap().extend([
        (
            session.id.clone(),
            awaken_session_contract::SessionUsage {
                input_tokens: 10,
                by_model: std::collections::BTreeMap::from([(
                    "served".into(),
                    awaken_session_contract::SessionModelUsage {
                        input_tokens: 10,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        ),
        (
            child_id.into(),
            awaken_session_contract::SessionUsage {
                input_tokens: 5,
                by_model: std::collections::BTreeMap::from([(
                    "served".into(),
                    awaken_session_contract::SessionModelUsage {
                        input_tokens: 5,
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            },
        ),
    ]);
    *runtime.committed.lock().unwrap() = Some(vec![
        Message::new(
            MessageId("batch-independent-parent-call".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                parent_call_id,
                SEND_TO_AGENT,
                serde_json::json!({
                    "agent_id":"researcher",
                    "message":"research durable identity"
                }),
            )],
        ),
        Message::new(
            MessageId("batch-independent-parent-result".into()),
            Role::Tool,
            vec![ContentBlock::tool_result(
                parent_call_id,
                vec![ContentBlock::text(
                    serde_json::json!({
                        "accepted":true,
                        "session_thread_id":child_id
                    })
                    .to_string(),
                )],
            )],
        ),
    ]);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        child_id,
        &child_run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    let mcp_call_id = "provider-batch-local-mcp";
    let first = Message::new(
        MessageId::assistant(&child_run, 0),
        Role::Assistant,
        vec![
            ContentBlock::thinking("private reasoning"),
            ContentBlock::text("first report"),
            ContentBlock::tool_use(
                mcp_call_id,
                "mcp__search__query",
                serde_json::json!({"q":"durable identity"}),
            ),
        ],
    );
    let result = Message::new(
        MessageId("batch-independent-message-2".into()),
        Role::Tool,
        vec![ContentBlock::tool_result(
            mcp_call_id,
            vec![ContentBlock::text("result")],
        )],
    );
    let second = Message::text(
        MessageId::assistant(&child_run, 1),
        Role::Assistant,
        "second report",
    );
    runtime
        .committed_by_thread
        .lock()
        .unwrap()
        .insert(child_id.into(), vec![first.clone()]);
    warm.refresh_committed_events(&session.id).await.unwrap();
    let prefix_events = warm
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data;
    assert!(
        prefix_events
            .iter()
            .all(|event| event.type_str() != "agent.mcp_tool_use"),
        "B0/E0 no ToolResult or ticket means no irreversible MCP classification"
    );
    assert_eq!(
        prefix_events
            .iter()
            .filter(|event| event.type_str() == "agent.thinking")
            .count(),
        1,
        "B0/E0 stable thinking"
    );
    assert_eq!(
        prefix_events
            .iter()
            .filter(|event| event.type_str() == "agent.message")
            .count(),
        1,
        "B0/E0 stable message"
    );
    runtime
        .committed_by_thread
        .lock()
        .unwrap()
        .insert(child_id.into(), vec![first, result, second]);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        2,
        child_id,
        &child_run,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));
    warm.refresh_committed_events(&session.id).await.unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();

    let signature = |events: Vec<Event>| {
        events
            .into_iter()
            .map(|event| (event.type_str().to_string(), event.id))
            .collect::<Vec<_>>()
    };
    let warm_child_events = warm
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data;
    for kind in ["agent.thinking", "agent.message", "agent.mcp_tool_use"] {
        assert_eq!(
            warm_child_events
                .iter()
                .filter(|event| event.type_str() == kind)
                .count(),
            1,
            "B1/E0-E4 partial replay keeps {kind} exact-once"
        );
    }
    assert!(
        warm_child_events.iter().all(|event| {
            event.id.starts_with(MANAGED_MULTIAGENT_EVENT_ID_PREFIX)
                || decode_managed_tool_event_id(&event.id).is_some()
        }),
        "B1/E3"
    );
    assert_eq!(
        warm_child_events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        warm_child_events.len(),
        "B1/E3"
    );
    let warm_mcp_use_id = warm_child_events
        .iter()
        .find(|event| event.type_str() == "agent.mcp_tool_use")
        .expect("B1/E4 MCP use")
        .id
        .clone();
    assert!(
        warm_child_events.iter().any(|event| {
            matches!(
                &event.kind,
                OutboundKind::AgentMcpToolResult { mcp_tool_use_id, .. }
                    if mcp_tool_use_id == &warm_mcp_use_id
            )
        }),
        "B1/E4"
    );
    let warm_child = signature(warm_child_events);
    let stable_primary_signature = |state: &ManagedState| {
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .into_iter()
            .filter(|event| {
                event.id.starts_with(MANAGED_MULTIAGENT_EVENT_ID_PREFIX)
                    || decode_managed_tool_event_id(&event.id).is_some()
            })
            .map(|event| (event.type_str().to_string(), event.id))
            .collect::<Vec<_>>()
    };
    let warm_primary = stable_primary_signature(&warm);
    for expected in [
        "session.thread_created",
        "session.thread_status_running",
        "agent.thread_message_sent",
        "agent.thread_message_received",
        "session.thread_status_idle",
        "session.status_idle",
    ] {
        assert!(
            warm_primary.iter().any(|(kind, _)| kind == expected),
            "B1/E2 missing {expected}"
        );
    }
    let warm_threads = warm.list_threads(&session.id).unwrap();
    assert_eq!(
        warm_threads
            .iter()
            .find(|thread| thread.parent_thread_id.is_none())
            .and_then(|thread| thread.usage.as_ref())
            .and_then(|usage| usage.input_tokens),
        Some(10),
        "B1/C5/E5 primary attribution"
    );
    assert_eq!(
        warm_threads
            .iter()
            .find(|thread| thread.parent_thread_id.is_none())
            .and_then(|thread| thread.usage.as_ref())
            .and_then(|usage| usage.list_cost.as_ref())
            .map(|cost| cost.amount.as_str()),
        Some("10"),
        "B1/C6/E6 primary independent price"
    );
    assert_eq!(
        warm_threads
            .iter()
            .find(|thread| thread.id == child_id)
            .and_then(|thread| thread.usage.as_ref())
            .and_then(|usage| usage.input_tokens),
        Some(5),
        "B1/C5/E5 child attribution"
    );
    assert_eq!(
        warm_threads
            .iter()
            .find(|thread| thread.id == child_id)
            .and_then(|thread| thread.usage.as_ref())
            .and_then(|usage| usage.list_cost.as_ref())
            .map(|cost| cost.amount.as_str()),
        Some("5"),
        "B1/C6/E6 child independent price"
    );
    assert_eq!(
        warm.get_session(&session.id).unwrap().usage.input_tokens,
        Some(15),
        "B1/C5/E5 aggregate counts each Thread once"
    );
    assert_eq!(
        warm.get_session(&session.id)
            .unwrap()
            .usage
            .list_cost
            .map(|cost| cost.amount),
        Some("15".into()),
        "B1/C6-C8/E7,R4 aggregate retrieve price"
    );
    let warm_usage_costs = warm
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .filter_map(|event| match event.kind {
            OutboundKind::SessionUsage { usage, .. } => usage.list_cost.map(|cost| cost.amount),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        warm_usage_costs,
        vec!["15".to_string()],
        "B1/C6-C8/E7,R4 terminal usage is priced once across warm replay"
    );

    let cold = ManagedState::new(runtime)
        .with_config_source(profiles)
        .with_session_repo(repo);
    cold.ensure_session(&session.id).await.unwrap();
    assert_eq!(
        signature(
            cold.list_thread_events(&session.id, child_id, None, None)
                .unwrap()
                .data,
        ),
        warm_child,
        "B1-B2/E1,E4"
    );
    assert_eq!(stable_primary_signature(&cold), warm_primary, "B1-B2/E2");
    assert_eq!(
        cold.get_thread(&session.id, child_id)
            .unwrap()
            .usage
            .and_then(|usage| usage.input_tokens),
        Some(5),
        "B2/C5/E5 cold child usage"
    );
    assert_eq!(
        cold.get_thread(&session.id, child_id)
            .unwrap()
            .usage
            .and_then(|usage| usage.list_cost)
            .map(|cost| cost.amount),
        Some("5".into()),
        "B2/C6/E6 cold child price"
    );
    assert_eq!(
        cold.get_session(&session.id)
            .unwrap()
            .usage
            .list_cost
            .map(|cost| cost.amount),
        Some("15".into()),
        "B2/C6-C8/E7,R4 cold aggregate retrieve price"
    );
    let cold_usage_costs = cold
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .filter_map(|event| match event.kind {
            OutboundKind::SessionUsage { usage, .. } => usage.list_cost.map(|cost| cost.amount),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        cold_usage_costs, warm_usage_costs,
        "B2/C6-C8/E7,R4 cold terminal usage matches warm replay"
    );
}

#[tokio::test]
async fn child_assistant_identity_rebuilds_from_committed_coordinates() {
    // Causes: the fixtures below establish `child assistant identity rebuilds from committed
    // coordinates` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a built-in Runtime ordinary assistant step
    // carries `(run,step)` in its canonical id and invokes a tool; C2 a
    // later complete step is the terminal report; C3 lifecycle names that
    // Run; C4 a cold projector has no live allocation record. Effects: E1
    // warm projection gives only C1 the deterministic response-0 preview
    // id; E2 C2 is received/sent rather than a duplicate `agent.message`;
    // E3 C4 rebuilds E1 exactly. Constraint: no preview receipt/cache is
    // persisted. Decision table: P1=C1+C2+C3 warm=>E1,E2;
    // P2=C1+C2+C3+C4 cold=>E3.
    let repo = Arc::new(ephemeral_session_repo());
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repo.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let child_id = "thread-preview-identity";
    let child_run = RunId("run-preview-identity".into());
    runtime.install_agent_coordination_prefix(
        &session.id,
        RunId("root".into()),
        0,
        "preview",
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &RunId("root".into()),
                0,
                "preview",
            ),
            latest_run_id: Some(child_run.clone()),
        },
    );
    runtime.committed_by_thread.lock().unwrap().insert(
        child_id.into(),
        vec![
            Message::new(
                MessageId::assistant(&child_run, 0),
                Role::Assistant,
                vec![
                    ContentBlock::text("previewable progress"),
                    ContentBlock::tool_use(
                        "preview-proof",
                        "read",
                        serde_json::json!({"path":"docs"}),
                    ),
                ],
            ),
            Message::text(
                MessageId::assistant(&child_run, 1),
                Role::Assistant,
                "terminal report",
            ),
        ],
    );
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            child_id,
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            child_id,
            &child_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);

    warm.refresh_committed_events(&session.id).await.unwrap();
    let projected_id = |state: &ManagedState| {
        state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data
            .into_iter()
            .find(|event| event.type_str() == "agent.message")
            .expect("child message")
            .id
    };
    let expected =
        managed_assistant_event_id(&session.id, child_id, &child_run.0, 0, 0, "agent.message");
    assert_eq!(projected_id(&warm), expected, "P1/E1");

    let cold = ManagedState::new(runtime).with_session_repo(repo);
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(projected_id(&cold), expected, "P2/E2");
}

#[tokio::test]
async fn root_assistant_identity_is_equal_warm_cold_and_active_active() {
    // Causes: the fixtures below establish `root assistant identity` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 one root Run commits response 0 as a truncated
    // Thinking+Text Message, C2 response 1 as a second truncated Text, C3
    // the final Message for the same Step follows, C4 warm/cold/restarted
    // replicas read only committed Message ids plus Run records, C5 two
    // disposable projectors
    // are live over the same durable Session. Effects: E1 roles and response
    // ordinals have four distinct deterministic ids; E2 every C4/C5
    // view produce the exact same ids; E3 repeated refresh emits no duplicate.
    //
    // | Rule | Responses | Projection | Replica count | Effects |
    // |---|---|---|---|---|
    // | R1 | C1-C3 | first warm refresh | 1 | E1,E2 |
    // | R2 | C1-C3 | repeated warm | 1 | E2,E3 |
    // | R3 | C1-C3 | cold/restart | 1 | E2 |
    // | R4 | C1-C3 | cold | 2 active | E2,E3 |
    let repo = Arc::new(ephemeral_session_repo());
    let runtime = RehydrateFake::default();
    let warm = ManagedState::new(runtime.clone()).with_session_repo(repo.clone());
    let session = warm
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let root_run = RunId("run-root-response-identity".into());
    let messages = vec![
        Message::new(
            MessageId::assistant_truncated(&root_run, 4, 0),
            Role::Assistant,
            vec![
                ContentBlock::thinking("private partial"),
                ContentBlock::text("first partial"),
            ],
        ),
        Message::text(
            MessageId::assistant_truncated(&root_run, 4, 1),
            Role::Assistant,
            "second partial",
        ),
        Message::text(
            MessageId::assistant(&root_run, 4),
            Role::Assistant,
            "final response",
        ),
    ];
    runtime
        .committed_by_thread
        .lock()
        .unwrap()
        .insert(session.id.clone(), messages.clone());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);

    warm.refresh_committed_events(&session.id).await.unwrap();
    let assistant_signature = |state: &ManagedState| {
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .into_iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    OutboundKind::AgentThinking {} | OutboundKind::AgentMessage { .. }
                )
            })
            .map(|event| (event.type_str(), event.id))
            .collect::<Vec<_>>()
    };
    let expected = vec![
        (
            "agent.thinking",
            managed_assistant_event_id(
                &session.id,
                &session.id,
                &root_run.0,
                4,
                0,
                "agent.thinking",
            ),
        ),
        (
            "agent.message",
            managed_assistant_event_id(
                &session.id,
                &session.id,
                &root_run.0,
                4,
                0,
                "agent.message",
            ),
        ),
        (
            "agent.message",
            managed_assistant_event_id(
                &session.id,
                &session.id,
                &root_run.0,
                4,
                1,
                "agent.message",
            ),
        ),
        (
            "agent.message",
            managed_assistant_event_id(
                &session.id,
                &session.id,
                &root_run.0,
                4,
                2,
                "agent.message",
            ),
        ),
    ];
    assert_eq!(assistant_signature(&warm), expected, "R1/E1-E2");

    warm.refresh_committed_events(&session.id).await.unwrap();
    warm.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(assistant_signature(&warm), expected, "R2/E2-E3");

    let restarted = ManagedState::new(runtime.clone()).with_session_repo(repo.clone());
    restarted.ensure_session(&session.id).await.unwrap();
    assert_eq!(assistant_signature(&restarted), expected, "R3/E2");

    let peer = ManagedState::new(runtime).with_session_repo(repo);
    peer.ensure_session(&session.id).await.unwrap();
    peer.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(assistant_signature(&peer), expected, "R4/E2-E3");
    assert_eq!(assistant_signature(&restarted), expected, "R4/E2-E3");
}

#[tokio::test]
async fn advisor_link_projects_the_closed_union_and_self_terminal_lifecycle_warm_or_cold() {
    // Causes: the fixtures below establish `advisor link` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the frozen coordinator roster contains one
    // advisor model; C2 the Runtime derives an Advisor target with a real
    // child Thread and ordinary Run; C3 that child transcript contains both
    // copied parent context and its canonical Run response while still
    // Running; C4 the parent Advisor ToolUse and its ToolResult become visible
    // in separate warm refreshes; C5 the advisor's one Run ends successfully;
    // C6 the disposable cache is lost; C7 the cold link omits its model
    // because the mutable catalog is absent; C8 root and advisor usage are
    // separately readable and the sole Session fold includes each once;
    // C9 the configured advisor family has client-redacted results.
    // Effects: E1 `thread.agent` is exactly `{type:"advisor",model}`; E2 the
    // primary emits created/running/received/idle/terminated once; E3 advisor
    // identity is never padded into a full Agent snapshot; E4 C5 emits one
    // self-terminal after idle without a generic archive fact; E5 cold
    // recovery reconstructs E1-E4 with identical event ids/order from the
    // same link/transcript/lifecycle facts; E6 C7 resolves
    // the unique advisor from the frozen Session profile; E7 advisor Thread
    // usage is non-null warm/cold and Session usage is root plus advisor
    // exactly once;
    // E8 every client event surface receives one redacted block while the
    // committed Runtime transcript retains plaintext; E9 C3 emits no advice
    // before C5, and C5 selects only the advisor Run response (never copied
    // context) as one receive.
    // Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | Effects |
    // | A1 | T | T | T | F | F | F | F | F | T | E1,E2,E3,E9(no receive) |
    // | A2 | T | T | T | T | F | F | F | F | T | E2 once; no tool/sent |
    // | A3 | T | T | T | T | T | F | F | T | T | E4,E7,E8,E9(receive) |
    // | A4 | T | T | T | F | T | T | T | T | T | E1,E3-E9 |
    let runtime = RehydrateFake::default();
    let state = ManagedState::new(runtime.clone()).with_config_source(Arc::new(AdvisorProfile));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let child_id = "sthr_advisor";
    let child_run = RunId("run-advisor".into());
    let root_run = RunId("advisor-root".into());
    runtime.usage_by_thread.lock().unwrap().extend([
        (
            session.id.clone(),
            awaken_session_contract::SessionUsage {
                input_tokens: 12,
                ..Default::default()
            },
        ),
        (
            child_id.into(),
            awaken_session_contract::SessionUsage {
                input_tokens: 7,
                ..Default::default()
            },
        ),
    ]);
    runtime
        .coordinated
        .lock()
        .unwrap()
        .push(CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Advisor {
                model: "claude-opus-5".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(&root_run, 0, "advisor-call"),
            latest_run_id: Some(child_run.clone()),
        });
    let advisor_call = Message::new(
        MessageId::assistant(&root_run, 0),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "advisor-call",
            awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
            serde_json::json!({}),
        )],
    );
    *runtime.committed.lock().unwrap() = Some(vec![advisor_call.clone()]);
    runtime.committed_by_thread.lock().unwrap().insert(
        child_id.into(),
        vec![
            Message::text(
                MessageId::assistant(&RunId("advisor-seed-parent-run".into()), 0),
                Role::Assistant,
                "copied root context",
            ),
            Message::text(
                MessageId::assistant(&child_run, 0),
                Role::Assistant,
                "independent advice",
            ),
        ],
    );
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            child_id,
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
    ]);

    state.refresh_committed_events(&session.id).await.unwrap();
    assert!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .all(|event| !matches!(event.kind, OutboundKind::AgentThreadMessageReceived { .. })),
        "A1/C3/E9 Running holds seed and partial output"
    );
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        child_id,
        &child_run,
        RunLifecycleEventKind::Completed,
        RunState::Ended(EndCause::NaturalEnd),
    ));
    state.refresh_committed_events(&session.id).await.unwrap();
    *runtime.committed.lock().unwrap() = Some(vec![
        advisor_call,
        Message::new(
            MessageId("advisor-parent-result".into()),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "advisor-call",
                vec![ContentBlock::text("independent advice")],
            )],
        ),
    ]);
    state.refresh_committed_events(&session.id).await.unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();
    let threads = state.list_threads(&session.id).unwrap();
    assert_eq!(threads.len(), 2, "A1/E1");
    let advisor = threads.iter().find(|thread| thread.id == child_id).unwrap();
    assert!(advisor.agent.as_agent().is_none(), "A1/E3");
    assert_eq!(advisor.status, SessionThreadStatus::Terminated, "A3/E4");
    assert_eq!(
        advisor.usage.as_ref().and_then(|usage| usage.input_tokens),
        Some(7),
        "A3/C8/E7 advisor accounting"
    );
    assert_eq!(
        state.get_session(&session.id).unwrap().usage.input_tokens,
        Some(19),
        "A3/C8/E7 Session fold is root 12 plus advisor 7 exactly once"
    );
    assert_eq!(
        serde_json::to_value(&advisor.agent).unwrap(),
        serde_json::json!({"type":"advisor","model":"claude-opus-5"}),
        "A1/E1"
    );
    let primary_events = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let is_advisor_primary_event = |event: &Event| match &event.kind {
        OutboundKind::SessionThreadCreated {
            session_thread_id, ..
        }
        | OutboundKind::SessionThreadStatusRunning {
            session_thread_id, ..
        }
        | OutboundKind::SessionThreadStatusIdle {
            session_thread_id, ..
        }
        | OutboundKind::SessionThreadStatusTerminated {
            session_thread_id, ..
        } => session_thread_id == child_id,
        OutboundKind::AgentThreadMessageReceived {
            from_session_thread_id,
            ..
        } => from_session_thread_id == child_id,
        _ => false,
    };
    for expected in [
        "session.thread_created",
        "session.thread_status_running",
        "agent.thread_message_received",
        "session.thread_status_idle",
        "session.thread_status_terminated",
    ] {
        assert_eq!(
            primary_events
                .iter()
                .filter(|event| { event.type_str() == expected && is_advisor_primary_event(event) })
                .count(),
            1,
            "A1-A3/E2/E4: {expected}"
        );
    }
    assert!(
        primary_events.iter().all(|event| {
            !matches!(
                &event.kind,
                OutboundKind::AgentThreadMessageSent { .. }
                    | OutboundKind::AgentToolUse { .. }
                    | OutboundKind::AgentCustomToolUse { .. }
                    | OutboundKind::AgentMcpToolUse { .. }
                    | OutboundKind::AgentToolResult { .. }
                    | OutboundKind::AgentMcpToolResult { .. }
            )
        }),
        "A1-A3 official advisor consultations expose no sent/tool event"
    );
    assert!(
        serde_json::to_string(&primary_events)
            .unwrap()
            .contains("anthropic.advisor"),
        "A1/E2"
    );
    assert_eq!(
        primary_events.iter().find_map(|event| match &event.kind {
            OutboundKind::AgentThreadMessageReceived { content, .. } => Some(content),
            _ => None,
        }),
        Some(&vec![ContentBlock::Redacted]),
        "A1-A3/C9/E8 warm client projection"
    );
    let advisor_primary_signature = primary_events
        .iter()
        .filter(|event| is_advisor_primary_event(event))
        .map(|event| (event.type_str().to_string(), event.id.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        advisor_primary_signature
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        vec![
            "session.thread_created",
            "session.thread_status_running",
            "agent.thread_message_received",
            "session.thread_status_idle",
            "session.thread_status_terminated",
        ],
        "A3/E2/E4 exact consultation lifecycle order"
    );
    let primary_json = serde_json::to_string(&primary_events).unwrap();
    assert!(!primary_json.contains("independent advice"), "A3/C9/E8");
    assert!(!primary_json.contains("copied root context"), "A3/E9");
    let committed_child_json = serde_json::to_string(
        runtime
            .committed_by_thread
            .lock()
            .unwrap()
            .get(child_id)
            .unwrap(),
    )
    .unwrap();
    assert!(
        committed_child_json.contains("independent advice")
            && committed_child_json.contains("copied root context"),
        "A3/E8 redaction does not mutate committed Runtime truth"
    );
    let child_event_signature = state
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data
        .into_iter()
        .map(|event| (event.type_str().to_string(), event.id))
        .collect::<Vec<_>>();
    assert_eq!(
        child_event_signature
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        vec![
            "session.thread_status_running",
            "session.thread_status_idle",
            "session.thread_status_terminated",
        ],
        "A1-A3 advisor delivery is primary-only"
    );
    assert!(runtime.archive_commits.lock().unwrap().is_empty(), "A3/E4");

    runtime.coordinated.lock().unwrap()[0].target = CoordinatedThreadTarget::Advisor {
        model: String::new(),
    };
    state.sessions.lock().unwrap().remove(&session.id);
    state.ensure_session(&session.id).await.unwrap();
    let recovered = state.get_thread(&session.id, child_id).unwrap();
    assert_eq!(recovered.status, SessionThreadStatus::Terminated, "A4/E5");
    assert_eq!(
        recovered.usage.and_then(|usage| usage.input_tokens),
        Some(7),
        "A4/C8/E7 cold advisor accounting"
    );
    assert_eq!(
        state.get_session(&session.id).unwrap().usage.input_tokens,
        Some(19),
        "A4/C8/E7 cold Session fold preserves root 12 plus advisor 7"
    );
    assert_eq!(
        serde_json::to_value(&recovered.agent).unwrap(),
        serde_json::json!({"type":"advisor","model":"claude-opus-5"}),
        "A4/E1/E3/E5/E6"
    );
    assert_eq!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .find_map(|event| match &event.kind {
                OutboundKind::AgentThreadMessageReceived { content, .. } => Some(content),
                _ => None,
            }),
        Some(&vec![ContentBlock::Redacted]),
        "A4/C9/E8 cold client projection"
    );
    let recovered_signature = state
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data
        .into_iter()
        .map(|event| (event.type_str().to_string(), event.id))
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_signature, child_event_signature,
        "A4/E5 child type/id/order"
    );
    let recovered_primary = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .filter(|event| is_advisor_primary_event(event))
        .map(|event| (event.type_str().to_string(), event.id))
        .collect::<Vec<_>>();
    assert_eq!(
        recovered_primary, advisor_primary_signature,
        "A4/E5 primary type/id/order"
    );
}

#[tokio::test]
async fn advisor_failed_or_cancelled_terminal_never_delivers_partial_advice() {
    // Causes: the fixtures below establish `advisor failed or cancelled terminal` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 an advisor child has copied seed context plus
    // canonical partial assistant output; C2 its one Run ends Failed with a
    // provider detail; C3 instead it ends Cancelled (including a targeted
    // advisor interruption); C4 a cold projector rebuilds from the same
    // committed prefix. Effects: E1 neither terminal emits an advisor
    // receive or any transcript content; E2 Failed emits only the generic
    // advisor notice on the child surface; E3 both become idle then
    // self-terminated without terminating the primary Session; E4 cold and
    // warm projections agree. The shared interrupt-selector matrix owns
    // ordinary-target and global fanout; this test owns advisor projection.
    //
    // Decision table:
    // | Rule | partial | terminal  | cold | Effects       |
    // | T1   | yes     | Failed    | no   | E1,E2,E3      |
    // | T2   | yes     | Cancelled | no   | E1,E3         |
    // | T3   | yes     | either    | yes  | E1,E2?,E3,E4  |
    let runtime = RehydrateFake::default();
    let state = ManagedState::new(runtime.clone()).with_config_source(Arc::new(AdvisorProfile));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let failed_thread = "sthr_advisor_failed";
    let failed_run = RunId("run-advisor-failed".into());
    let cancelled_thread = "sthr_advisor_cancelled";
    let cancelled_run = RunId("run-advisor-cancelled".into());
    let root_run = RunId("advisor-terminal-root".into());
    runtime
        .run_ids_by_thread
        .lock()
        .unwrap()
        .insert(session.id.clone(), root_run.clone());
    runtime.committed_by_thread.lock().unwrap().insert(
        session.id.clone(),
        ["advisor-failed-call", "advisor-cancelled-call"]
            .into_iter()
            .enumerate()
            .map(|(step, call_id)| {
                Message::new(
                    MessageId::assistant(&root_run, step),
                    Role::Assistant,
                    vec![ContentBlock::tool_use(
                        call_id,
                        awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                        serde_json::json!({}),
                    )],
                )
            })
            .collect(),
    );
    runtime
        .message_cursors_by_thread
        .lock()
        .unwrap()
        .insert(session.id.clone(), vec![1, 2]);
    runtime.coordinated.lock().unwrap().extend([
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(failed_thread.into()),
            target: CoordinatedThreadTarget::Advisor {
                model: "claude-opus-5".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &root_run,
                0,
                "advisor-failed-call",
            ),
            latest_run_id: Some(failed_run.clone()),
        },
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(cancelled_thread.into()),
            target: CoordinatedThreadTarget::Advisor {
                model: "claude-opus-5".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &root_run,
                1,
                "advisor-cancelled-call",
            ),
            latest_run_id: Some(cancelled_run.clone()),
        },
    ]);
    for (thread_id, run_id, secret) in [
        (failed_thread, &failed_run, "failed partial secret"),
        (cancelled_thread, &cancelled_run, "cancelled partial secret"),
    ] {
        runtime.committed_by_thread.lock().unwrap().insert(
            thread_id.into(),
            vec![
                Message::text(
                    MessageId::assistant(&RunId(format!("seed:{thread_id}")), 0),
                    Role::Assistant,
                    format!("copied context for {thread_id}"),
                ),
                Message::text(MessageId::assistant(run_id, 0), Role::Assistant, secret),
            ],
        );
    }
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            failed_thread,
            &failed_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            failed_thread,
            &failed_run,
            RunLifecycleEventKind::Failed,
            RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::Inference {
                    code: "unauthorized".into(),
                    message: "sensitive advisor provider detail".into(),
                },
            )),
        ),
        lifecycle(
            3,
            cancelled_thread,
            &cancelled_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            4,
            cancelled_thread,
            &cancelled_run,
            RunLifecycleEventKind::Cancelled,
            RunState::Ended(EndCause::Cancelled),
        ),
    ]);

    let assert_projection = |state: &ManagedState, rule: &str| {
        let primary = state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data;
        assert!(
            primary.iter().all(|event| !matches!(
                &event.kind,
                OutboundKind::AgentThreadMessageReceived { .. }
                    | OutboundKind::SessionStatusTerminated { .. }
            )),
            "{rule}/E1/E3"
        );
        let failed = state
            .list_thread_events(&session.id, failed_thread, None, None)
            .unwrap()
            .data;
        let cancelled = state
            .list_thread_events(&session.id, cancelled_thread, None, None)
            .unwrap()
            .data;
        assert_eq!(
            failed.iter().find_map(|event| match &event.kind {
                OutboundKind::SessionError { error } => Some(error.message.as_str()),
                _ => None,
            }),
            Some(ADVISOR_FAILURE_NOTICE),
            "{rule}/E2"
        );
        for (events, terminal_rule) in [(&failed, "T1"), (&cancelled, "T2")] {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event.type_str(),
                        "session.thread_status_idle" | "session.thread_status_terminated"
                    ))
                    .map(Event::type_str)
                    .collect::<Vec<_>>(),
                vec![
                    "session.thread_status_idle",
                    "session.thread_status_terminated"
                ],
                "{rule}/{terminal_rule}/E3"
            );
        }
        let public = format!(
            "{}{}{}",
            serde_json::to_string(&primary).unwrap(),
            serde_json::to_string(&failed).unwrap(),
            serde_json::to_string(&cancelled).unwrap()
        );
        for private in [
            "failed partial secret",
            "cancelled partial secret",
            "copied context",
            "sensitive advisor provider detail",
        ] {
            assert!(!public.contains(private), "{rule}/E1/E2: {private}");
        }
    };

    state.refresh_committed_events(&session.id).await.unwrap();
    assert_projection(&state, "T1-T2 warm");
    state.sessions.lock().unwrap().remove(&session.id);
    state.ensure_session(&session.id).await.unwrap();
    assert_projection(&state, "T3 cold");
}

#[tokio::test]
async fn lifecycle_scope_uses_the_root_fence_and_later_relationship_snapshot() {
    // Causes: the fixtures below establish `lifecycle scope` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 an unknown child lifecycle commit is newer than
    // the root recovery fence; C2 its coordinated link is not visible in the
    // later relationship read; C3 the root fence and link catch up; C4 an
    // root Run terminates; C5 a later unknown internal/other-Session lifecycle
    // is inside the root fence but remains absent from that relationship
    // snapshot. Effects: E1 C1+C2 retain the unknown cursor and
    // create no false Thread; E2 C3 replays the same lifecycle into the real
    // advisor Thread; E3 C5 advances without a public Thread; E4 C4 still
    // projects the root terminal bracket before C5; E5 aggregate Usage/Idle
    // identity remains owned by C4's terminal cursor, never by C5's unrelated
    // feed high-water. No Thread-id spelling or separate legacy-delegation
    // query participates.
    // Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    // | L1 | T | T | F | F | F | E1 |
    // | L2 | T | T | T | F | F | E2 |
    // | L3 | F | T | F | T | T | E3,E4,E5 |
    let runtime = RehydrateFake::default();
    let state = ManagedState::new(runtime.clone()).with_config_source(Arc::new(AdvisorProfile));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let advisor_thread_id = "run-advisor-race";
    let advisor_run = RunId(advisor_thread_id.into());
    let root_run = RunId("run-root-lifecycle-scope".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            advisor_thread_id,
            &advisor_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            3,
            advisor_thread_id,
            &advisor_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.committed_by_thread.lock().unwrap().insert(
        session.id.clone(),
        vec![Message::new(
            MessageId::assistant(&root_run, 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "advisor-race",
                awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                serde_json::json!({}),
            )],
        )],
    );
    runtime
        .message_cursors_by_thread
        .lock()
        .unwrap()
        .insert(session.id.clone(), vec![1]);
    *runtime.root_store_cursor_override.lock().unwrap() = Some(1);

    state.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        state.lifecycle_cursor(&session.id).unwrap(),
        lifecycle_cursor(1),
        "L1/E1"
    );
    assert_eq!(state.list_threads(&session.id).unwrap().len(), 1, "L1/E1");

    *runtime.root_store_cursor_override.lock().unwrap() = None;
    runtime
        .coordinated
        .lock()
        .unwrap()
        .push(CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(advisor_thread_id.into()),
            target: CoordinatedThreadTarget::Advisor {
                model: "claude-opus-5".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(&root_run, 0, "advisor-race"),
            latest_run_id: Some(advisor_run),
        });
    state.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        state.lifecycle_cursor(&session.id).unwrap(),
        lifecycle_cursor(3),
        "L2/E2"
    );
    assert_eq!(
        state
            .get_thread(&session.id, advisor_thread_id)
            .unwrap()
            .status,
        SessionThreadStatus::Terminated,
        "L2/E2"
    );

    let internal_run = RunId("run-internal-compactor".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            4,
            &session.id,
            &root_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
        lifecycle(
            5,
            "internal-auxiliary-thread",
            &internal_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    state.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        state.lifecycle_cursor(&session.id).unwrap(),
        lifecycle_cursor(5),
        "L3/E3/E4"
    );
    assert!(
        state
            .get_thread(&session.id, "internal-auxiliary-thread")
            .is_err(),
        "L3/E3"
    );
    let rendered = serde_json::to_string(
        &state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data,
    )
    .unwrap();
    assert!(rendered.contains("session.thread_status_idle"), "L3/E4");
    assert!(rendered.contains("session.status_idle"), "L3/E4");
    let terminal_cursor = lifecycle_cursor(4);
    let expected_usage_id = managed_multiagent_event_id(
        &session.id,
        &session.id,
        "aggregate-usage",
        ManagedMultiagentEventProvenance::LifecyclePrefix {
            cursor: terminal_cursor.0,
        },
    );
    let expected_idle_id = managed_multiagent_event_id(
        &session.id,
        &session.id,
        "aggregate-status-idle",
        ManagedMultiagentEventProvenance::LifecyclePrefix {
            cursor: terminal_cursor.0,
        },
    );
    let events = state
        .list_events(&session.id, None, None, false)
        .expect("L3 committed Session history")
        .data;
    assert!(
        events.iter().any(|event| event.id == expected_usage_id),
        "L3/E5 Usage uses the terminal cursor"
    );
    assert!(
        events.iter().any(|event| event.id == expected_idle_id),
        "L3/E5 Idle uses the terminal cursor"
    );
}

#[tokio::test]
async fn terminal_read_cannot_overtake_the_child_transcript_snapshot() {
    // Causes: the fixtures below establish `terminal read` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a coordinated child is Running; C2 a refresh
    // takes the old child transcript snapshot; C3 the final assistant message
    // and Completed lifecycle commit immediately after that snapshot; C4 the
    // same public refresh retries after comparing the final child prefix; C5 recovery store cursors
    // count commits while lifecycle cursors use the independent event
    // sequence domain. Effects: E1 C2+C3 cannot emit idle or misclassify the
    // candidate as an ordinary message over the old transcript; E2 C4 emits
    // the terminal report as child-sent before idle exactly once, never as
    // `agent.message`; E3 C5 fences with the snapshot event cursor rather
    // than comparing incompatible numeric domains.
    // Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    // | S1 | T | T | T | F | T | discard mixed attempt |
    // | S2 | T | T | T | T | T | E1,E2,E3 |
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let state = ManagedState::new(runtime.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let child_id = "thread-terminal-skew";
    let child_run = RunId("run-terminal-skew".into());
    let root_run = RunId("run-terminal-skew-root".into());
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &root_run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    runtime.committed_by_thread.lock().unwrap().insert(
        session.id.clone(),
        vec![
            Message::new(
                MessageId::assistant(&root_run, 0),
                Role::Assistant,
                vec![
                    ContentBlock::ToolUse {
                        id: "terminal-skew".into(),
                        name: SEND_TO_AGENT.into(),
                        input: serde_json::json!({"agent_id":"researcher"}),
                    },
                    ContentBlock::ToolUse {
                        id: "terminal-skew-b".into(),
                        name: SEND_TO_AGENT.into(),
                        input: serde_json::json!({"agent_id":"researcher"}),
                    },
                ],
            ),
            Message::new(
                MessageId::tool_result("terminal-skew"),
                Role::Tool,
                vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "terminal-skew".into(),
                        content: vec![ContentBlock::text(
                            serde_json::json!({
                                "accepted": true,
                                "session_thread_id": child_id
                            })
                            .to_string(),
                        )],
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "terminal-skew-b".into(),
                        content: vec![ContentBlock::text(
                            serde_json::json!({
                                "accepted": true,
                                "session_thread_id": "thread-terminal-skew-b"
                            })
                            .to_string(),
                        )],
                        is_error: false,
                    },
                ],
            ),
        ],
    );
    runtime
        .message_cursors_by_thread
        .lock()
        .unwrap()
        .insert(session.id.clone(), vec![1, 2]);
    runtime
        .coordinated
        .lock()
        .unwrap()
        .push(CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &root_run,
                0,
                "terminal-skew",
            ),
            latest_run_id: Some(child_run.clone()),
        });
    runtime
        .coordinated
        .lock()
        .unwrap()
        .push(CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId("thread-terminal-skew-b".into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &root_run,
                0,
                "terminal-skew-b",
            ),
            latest_run_id: Some(RunId("run-terminal-skew-b".into())),
        });
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        3,
        child_id,
        &child_run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    state.refresh_committed_events(&session.id).await.unwrap();
    let issued_prefix = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();

    *runtime.child_commit_after_history_snapshot.lock().unwrap() = Some((
        child_id.into(),
        vec![Message::text(
            MessageId::assistant(&child_run, 0),
            Role::Assistant,
            "final child answer",
        )],
        vec![
            lifecycle(
                8,
                child_id,
                &child_run,
                RunLifecycleEventKind::Completed,
                RunState::Ended(EndCause::NaturalEnd),
            ),
            lifecycle(
                9,
                "thread-terminal-skew-b",
                &RunId("run-terminal-skew-b".into()),
                RunLifecycleEventKind::Completed,
                RunState::Ended(EndCause::NaturalEnd),
            ),
        ],
    ));
    let reads_before = runtime
        .order
        .lock()
        .unwrap()
        .iter()
        .filter(|read| **read == "child_snapshot")
        .count();
    state.refresh_committed_events(&session.id).await.unwrap();
    let reads_after = runtime
        .order
        .lock()
        .unwrap()
        .iter()
        .filter(|read| **read == "child_snapshot")
        .count();
    assert!(
        reads_after >= reads_before + 8,
        "S1 mixed child prefix is discarded and retried"
    );
    assert_eq!(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data
            .iter()
            .take(issued_prefix.len())
            .map(|event| event.id.clone())
            .collect::<Vec<_>>(),
        issued_prefix,
        "S1 issued prefix remains immutable"
    );
    assert_eq!(
        state
            .get_thread(&session.id, "thread-terminal-skew-b")
            .unwrap()
            .status,
        SessionThreadStatus::Idle,
        "S2 child B commit is published only with child A"
    );
    let child_events = state
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data;
    let report_position = child_events
        .iter()
        .position(|event| matches!(event.kind, OutboundKind::AgentThreadMessageSent { .. }))
        .expect("S2/E2 terminal report");
    let idle_position = child_events
        .iter()
        .position(|event| event.type_str() == "session.thread_status_idle")
        .expect("S2/E2 terminal idle");
    assert!(report_position < idle_position, "S2/E2");
    assert_eq!(
        child_events
            .iter()
            .filter(|event| matches!(event.kind, OutboundKind::AgentThreadMessageSent { .. }))
            .count(),
        1,
        "S2/E2 one report"
    );
    assert!(
        child_events
            .iter()
            .all(|event| !matches!(event.kind, OutboundKind::AgentMessage { .. })),
        "S2/E2 report is not duplicated as a message"
    );
    state.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        state
            .list_thread_events(&session.id, child_id, None, None)
            .unwrap()
            .data,
        child_events,
        "S2/E2 stable replay"
    );
    let warm_history = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    state.sessions.lock().unwrap().remove(&session.id);
    state.ensure_session(&session.id).await.unwrap();
    let cold_history = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(cold_history, warm_history, "S2 warm/cold full order");
    for cursor in &issued_prefix {
        let position = warm_history
            .iter()
            .position(|event| &event.id == cursor)
            .expect("issued cursor remains in warm history");
        assert_eq!(
            state
                .list_events(&session.id, Some(cursor), None, false)
                .unwrap()
                .data,
            warm_history[position + 1..],
            "S2 cold suffix after {cursor}"
        );
    }
}

#[tokio::test]
async fn cold_historical_awaiting_never_fabricates_empty_requires_action() {
    // Causes: the fixtures below establish `cold historical awaiting never fabricates empty` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a child committed Awaiting with an answerable
    // ticket; C2 that same Run later resumed/completed and consumed the
    // ticket; C3 Managed starts cold with the full lifecycle but only the
    // current recovery snapshot. Effects: E1 consume the durable lifecycle
    // through Completed; E2 project the current EndTurn idle; E3 omit the
    // unreconstructable historical Awaiting instead of publishing the false
    // `requires_action([])` state. Durable historical ticket correlation is
    // deliberately not replaced by a Managed side registry.
    //
    // | Rule | Historical await | Current ticket | Later terminal | Effect |
    // |---|---|---|---|---|
    // | H1 | yes | absent | Completed | E1,E2,E3 |
    let repo = Arc::new(ephemeral_session_repo());
    let runtime = RehydrateFake::default();
    runtime
        .delegate_ids
        .lock()
        .unwrap()
        .push("researcher".into());
    let original = ManagedState::new(runtime.clone()).with_session_repo(repo.clone());
    let session = original
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    drop(original);

    let child_id = "thread-historical-await";
    let child_run = RunId("run-historical-await".into());
    runtime.install_agent_coordination_prefix(
        &session.id,
        RunId("root".into()),
        0,
        "historical-await",
        CoordinatedThreadLink {
            session_id: session.id.clone(),
            thread_id: ThreadId(child_id.into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: ToolBatch::operation_id_for_step(
                &RunId("root".into()),
                0,
                "historical-await",
            ),
            latest_run_id: Some(child_run.clone()),
        },
    );
    runtime.committed_by_thread.lock().unwrap().insert(
        child_id.into(),
        vec![
            Message::new(
                MessageId("historical-await-tool".into()),
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    "historical-call",
                    "client_lookup",
                    serde_json::json!({"query":"old"}),
                )],
            ),
            Message::new(
                MessageId("historical-await-result".into()),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "historical-call".into(),
                    content: vec![ContentBlock::text("resolved")],
                    is_error: false,
                }],
            ),
            Message::text(
                MessageId("historical-await-final".into()),
                Role::Assistant,
                "completed",
            ),
        ],
    );
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            child_id,
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            child_id,
            &child_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
        lifecycle(
            3,
            child_id,
            &child_run,
            RunLifecycleEventKind::Resumed,
            RunState::Running,
        ),
        lifecycle(
            4,
            child_id,
            &child_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);

    let restarted = ManagedState::new(runtime).with_session_repo(repo);
    restarted.ensure_session(&session.id).await.unwrap();
    assert_eq!(
        restarted.lifecycle_cursor(&session.id).unwrap(),
        lifecycle_cursor(4),
        "H1/E1"
    );
    let child_events = restarted
        .list_thread_events(&session.id, child_id, None, None)
        .unwrap()
        .data;
    let reasons = child_events
        .iter()
        .filter_map(|event| match &event.kind {
            OutboundKind::SessionThreadStatusIdle { stop_reason, .. } => Some(stop_reason),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(reasons, vec![&StopReason::EndTurn], "H1/E2-E3");
}

#[tokio::test]
async fn synthetic_pending_tool_uses_the_awaiting_commit_as_its_projection_anchor() {
    // Cause/effect graph: C1 a committed remote Awaiting lifecycle owns one
    // exact pending ticket; C2 its ToolUse transcript message has not committed;
    // C3 the projection is read warm and then rebuilt cold. Effects: E1 publish
    // one source-qualified answerable tool event; E2 order it at the Awaiting
    // commit before both requires_action idle edges; E3 warm/cold reads retain
    // the same public id without a missing-anchor failure or duplicate. The
    // lifecycle feed remains the sole commit-coordinate owner; no synthetic
    // cursor or parallel pending ledger is introduced.
    //
    // | Rule | Awaiting | ticket | transcript | cache | Effects |
    // |---|---|---|---|---|---|
    // | S1 | exact | exact | absent | warm | E1+E2 |
    // | S2 | exact | exact | absent | cold | E1+E2+E3 |
    // | S3 | exact | absent | absent | any | withhold (neighboring fence test) |
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone()).with_config_source(Arc::new(
        FrozenToolFamilyProfiles::uniform(
            "coder",
            &[],
            FrozenTestToolFamily::Custom,
            "agent_input",
        ),
    ));
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let run = RunId("run-synthetic-pending".into());
    let call_id = "remote-input-task";
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: call_id.into(),
        name: "agent_input".into(),
        input: serde_json::json!({"prompt":"which file?"}),
        client_executed: true,
    });
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            10,
            &session.id,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            20,
            &session.id,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);

    state.refresh_committed_events(&session.id).await.unwrap();
    let warm = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let tool_index = warm
        .iter()
        .position(|event| matches!(event.kind, OutboundKind::AgentCustomToolUse { .. }))
        .expect("S1/E1 synthetic answerable tool");
    let tool_id = warm[tool_index].id.clone();
    let identity = decode_managed_tool_event_id(&tool_id).expect("S1/E1 qualified identity");
    assert_eq!(identity.source_id, run.0, "S1/E1 durable Run source");
    let idle_indices = warm
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(
                event.kind,
                OutboundKind::SessionStatusIdle { .. }
                    | OutboundKind::SessionThreadStatusIdle { .. }
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(idle_indices.len(), 2, "S1/E2 exact idle pair");
    assert!(
        idle_indices.iter().all(|index| tool_index < *index),
        "S1/E2"
    );

    state.sessions.lock().unwrap().remove(&session.id);
    state.refresh_committed_events(&session.id).await.unwrap();
    let cold = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        cold.iter().filter(|event| event.id == tool_id).count(),
        1,
        "S2/E3 stable cold identity"
    );
}

#[tokio::test]
async fn committed_lifecycle_closes_cross_protocol_managed_projection_once() {
    // Causes: the fixtures below establish `committed lifecycle` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C0 the frozen root Agent declares
    // `design_submit_artifact` as a custom client tool; C1 the Session cache
    // is warm; C2 a Run is committed through another protocol; C3 latest
    // lifecycle is Running; C4 latest lifecycle is Awaiting with its exact
    // pending ticket; C5 latest lifecycle is Ended; C6 the same read refresh
    // repeats; C7 a cold Awaiting transition is visible before its exact
    // ticket and the ticket later catches up; C8 the same Run resumes and
    // reaches a second Awaiting terminal. Effects: E1 Running alone
    // fabricates no terminal; E2 Awaiting appends one
    // running→requires_action bracket with the public call id; E3 Ended
    // appends one running→idle bracket; E4 repeated reads duplicate neither
    // messages nor terminals; E5 the lifecycle cursor fences projection;
    // E6 cold recovery freezes transcript and lifecycle until the ticket
    // classifies the call; E7 each reused-Run terminal occurrence has one
    // bracket; E8 C0 keeps every answerable call in the custom family.
    // Decision table:
    // | Rule | Cache | Latest | Repeat | Reused Run | Effect |
    // |---|---|---|---|---|---|
    // | R1 | warm | Running | no | no | E1,E5 |
    // | R2 | warm | Awaiting+ticket | yes | no | E2,E4,E5,E8 |
    // | R3 | warm | Ended | yes | no | E3,E4,E5 |
    // | R4 | warm | Awaiting+ticket | yes | yes | E2,E4,E5,E7,E8 |
    // | R5 | cold | Awaiting-ticket | no | no | E5,E6 |
    // | R6 | cold | Awaiting+ticket | yes | no | E2,E4,E5,E6,E8 |
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone()).with_config_source(Arc::new(
        FrozenToolFamilyProfiles::uniform(
            "coder",
            &[],
            FrozenTestToolFamily::Custom,
            "design_submit_artifact",
        ),
    ));
    let request = serde_json::from_value(serde_json::json!({
        "agent":"coder", "environment_id":"env_local"
    }))
    .unwrap();
    let session = state.create_session(request, None).await.unwrap();
    let thread = session.id;
    let first = Message::text(MessageId("cross-user".into()), Role::User, "build");
    runtime.push_committed_message(10, first);

    let run = RunId("run-cross-1".into());
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        10,
        &thread,
        &run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    state.refresh_committed_events(&thread).await.unwrap();
    assert_eq!(
        state.get_session(&thread).unwrap().status,
        SessionStatus::Idle,
        "R1"
    );
    let rendered =
        serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
            .unwrap();
    assert!(!rendered.contains("session.status_idle"), "R1");

    let call_id = "call-cross-submit";
    runtime.push_committed_message(
        20,
        Message::new(
            MessageId("cross-tool".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                call_id,
                "design_submit_artifact",
                serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            )],
        ),
    );
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: call_id.into(),
        name: "design_submit_artifact".into(),
        input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
        client_executed: true,
    });
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        20,
        &thread,
        &run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    ));
    state.refresh_committed_events(&thread).await.unwrap();
    state.refresh_committed_events(&thread).await.unwrap();
    let rendered =
        serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
            .unwrap();
    assert_eq!(
        rendered.matches(call_id).count(),
        3,
        "R2: the custom_tool_use, primary Thread requires_action, and aggregate Session requires_action each carry the same public identity exactly once"
    );
    assert_eq!(rendered.matches("session.status_idle").count(), 1, "R2/E4");

    let second_run = RunId("run-cross-2".into());
    *runtime.pending.lock().unwrap() = None;
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            30,
            &thread,
            &second_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            40,
            &thread,
            &second_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.push_committed_message(
        40,
        Message::text(MessageId("cross-final".into()), Role::Assistant, "done"),
    );
    state.refresh_committed_events(&thread).await.unwrap();
    state.refresh_committed_events(&thread).await.unwrap();
    let rendered =
        serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
            .unwrap();
    assert_eq!(rendered.matches("session.status_idle").count(), 2, "R3/E4");
    assert_eq!(rendered.matches("done").count(), 1, "R3/E4");

    let local_run = RunId("run-local-3".into());
    let local_message = Message::text(
        MessageId("local-final".into()),
        Role::Assistant,
        "local done",
    );
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            50,
            &thread,
            &local_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            60,
            &thread,
            &local_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    runtime.push_committed_message(60, local_message);
    state.refresh_committed_events(&thread).await.unwrap();
    state.refresh_committed_events(&thread).await.unwrap();
    let rendered =
        serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
            .unwrap();
    assert_eq!(rendered.matches("session.status_idle").count(), 3, "R3/E4");
    assert_eq!(rendered.matches("local done").count(), 1, "R3/E4");

    let resumed_call_id = "call-local-resumed";
    let resumed_message = Message::new(
        MessageId("local-resumed-tool".into()),
        Role::Assistant,
        vec![ContentBlock::tool_use(
            resumed_call_id,
            "design_submit_artifact",
            serde_json::json!({"manifest_path":"artifact-manifest.json"}),
        )],
    );
    let resumed_pending = Pending {
        tool_use_id: resumed_call_id.into(),
        name: "design_submit_artifact".into(),
        input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
        client_executed: true,
    };
    // R4/C8 race partition: the resumed opening is consumed while the durable
    // aggregate is still Idle, then the terminal/ticket prefix catches up in a
    // later refresh. The terminal projector must recover the opening from the
    // complete accepted lifecycle prefix; the process-local cursor may not make
    // the second aggregate Running edge unconstructable.
    *runtime.pending.lock().unwrap() = None;
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        70,
        &thread,
        &local_run,
        RunLifecycleEventKind::Resumed,
        RunState::Running,
    ));
    state.refresh_committed_events(&thread).await.unwrap();
    *runtime.pending.lock().unwrap() = Some(resumed_pending);
    runtime.push_committed_message(80, resumed_message);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        80,
        &thread,
        &local_run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    ));
    state.refresh_committed_events(&thread).await.unwrap();
    state.refresh_committed_events(&thread).await.unwrap();
    let rendered =
        serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
            .unwrap();
    assert_eq!(rendered.matches("session.status_idle").count(), 4, "R4/E7");
    assert_eq!(
        rendered.matches("session.status_running").count(),
        4,
        "R4/E7 every terminal bracket retains its exact opening across split refreshes"
    );
    assert_eq!(
        rendered.matches(resumed_call_id).count(),
        3,
        "R4/E2/E4: the custom_tool_use, primary Thread requires_action, and aggregate Session requires_action each carry the same public identity exactly once"
    );

    let cold_run = RunId("run-cold-4".into());
    let cold_call_id = "call-cold-submit";
    runtime.replace_committed_messages(vec![(
        100,
        Message::new(
            MessageId("cold-tool".into()),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                cold_call_id,
                "design_submit_artifact",
                serde_json::json!({"manifest_path":"artifact-manifest.json"}),
            )],
        ),
    )]);
    let cold_pending = Pending {
        tool_use_id: cold_call_id.into(),
        name: "design_submit_artifact".into(),
        input: serde_json::json!({"manifest_path":"artifact-manifest.json"}),
        client_executed: true,
    };
    *runtime.pending.lock().unwrap() = None;
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            90,
            &thread,
            &cold_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            100,
            &thread,
            &cold_run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);
    state.sessions.lock().unwrap().remove(&thread);
    state.refresh_committed_events(&thread).await.unwrap();
    assert_eq!(
        state.lifecycle_cursor(&thread).unwrap(),
        lifecycle_cursor(90),
        "R5/E6 cold lifecycle waits behind the missing exact ticket"
    );
    let before_ticket =
        serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
            .unwrap();
    assert!(
        !before_ticket.contains(cold_call_id),
        "R5/E6 cold base record cannot consume the transcript first"
    );
    *runtime.pending.lock().unwrap() = Some(cold_pending);
    state.refresh_committed_events(&thread).await.unwrap();
    state.refresh_committed_events(&thread).await.unwrap();
    let rendered =
        serde_json::to_string(&state.list_events(&thread, None, None, false).unwrap().data)
            .unwrap();
    assert_eq!(
        rendered.matches("agent.custom_tool_use").count(),
        1,
        "R6/E6"
    );
    assert!(!rendered.contains("\"type\":\"agent.tool_use\""), "R6/E6");
    assert_eq!(
        rendered.matches(cold_call_id).count(),
        3,
        "R6/E2/E4: the custom_tool_use, primary Thread requires_action, and aggregate Session requires_action each carry the same public identity exactly once"
    );
}

#[tokio::test]
async fn committed_tool_occurrences_project_only_after_exact_disposition_evidence() {
    // Cause/effect graph: C1 a root Running snapshot exposes an Assistant
    // Message with stable text and two built-in ToolUse occurrences before any
    // result/ticket; C2 the first occurrence later owns the exact Awaiting
    // ResumeTicket while its sibling remains unclassified; C3 a cold projector
    // reads that same C2 prefix; C4 the first result and a resumed Awaiting ticket
    // for the sibling catch up; C5 a separate first read sees one ToolUse with
    // its matching ToolResult and Completed lifecycle together. Effects: E1 C1
    // emits stable non-tool content once but neither a tool nor terminal; E2 C2
    // emits exactly one ask event and both requires_action boundaries name its
    // exact source-qualified id without consuming the source Message; E3 C3
    // rebuilds the same public id and classification; E4 C4 emits only the
    // missing sibling ask plus the first result, never duplicate text/tool, then
    // consumes the now-classified source Message; E5 C5 emits one allow/result
    // pair and EndTurn, proving completed work cannot remain frozen.
    // K1 ResumeTicket and ToolResult are the only disposition authorities; K2
    // the append-only Event log is the partial-projection idempotency ledger; K3
    // root and child call the same occurrence classifier and existing encoder.
    //
    // | Rule | First result | First ticket | Sibling ticket | First read | Effect |
    // |---|---|---|---|---|---|
    // | O1 | no | no | no | warm | E1 |
    // | O2 | no | exact | no | warm | E2 |
    // | O3 | no | exact | no | cold | E3 |
    // | O4 | yes | consumed | exact | warm replay | E4 |
    // | O5 | yes | no | absent | cold/full | E5 |
    let repo = Arc::new(ephemeral_session_repo());
    let runtime = LifecycleRuntime::default();
    let profiles = Arc::new(FrozenToolFamilyProfiles::uniform(
        "coder",
        &[],
        FrozenTestToolFamily::AgentAlwaysAsk,
        "write",
    ));
    let state = ManagedState::new(runtime.clone())
        .with_config_source(profiles.clone())
        .with_session_repo(repo.clone());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let run = RunId("run-occurrence-evidence".into());
    let first_call = "call-occurrence-first";
    let sibling_call = "call-occurrence-sibling";
    let tool_message = Message::new(
        MessageId::assistant(&run, 0),
        Role::Assistant,
        vec![
            ContentBlock::text("two writes need evidence"),
            ContentBlock::tool_use(first_call, "write", serde_json::json!({"path":"first.txt"})),
            ContentBlock::tool_use(
                sibling_call,
                "write",
                serde_json::json!({"path":"second.txt"}),
            ),
        ],
    );
    runtime.replace_committed_messages(vec![(1, tool_message.clone())]);
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &run,
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));

    state.refresh_committed_events(&session.id).await.unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();
    let message_first = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        message_first
            .iter()
            .filter(|event| event.type_str() == "agent.message")
            .count(),
        1,
        "O1/E1 stable non-tool content"
    );
    assert!(
        message_first.iter().all(|event| {
            !matches!(
                event.type_str(),
                "agent.tool_use" | "session.thread_status_idle" | "session.status_idle"
            )
        }),
        "O1/E1 no inferred tool disposition or terminal"
    );
    assert!(
        !state.sessions.lock().unwrap()[&session.id]
            .message_was_projected(&session.id, &tool_message.id.0),
        "O1/E1 source remains revisitable"
    );

    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: first_call.into(),
        name: "write".into(),
        input: serde_json::json!({"path":"first.txt"}),
        client_executed: false,
    });
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        2,
        &session.id,
        &run,
        RunLifecycleEventKind::Awaiting,
        RunState::Awaiting,
    ));
    state.refresh_committed_events(&session.id).await.unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();
    let awaiting = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let asks = awaiting
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                OutboundKind::AgentToolUse {
                    evaluated_permission: Some(EvaluatedPermission::Ask),
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(asks.len(), 1, "O2/E2 only ready occurrence");
    let first_public_id = asks[0].id.clone();
    assert_eq!(
        decode_managed_tool_event_id(&first_public_id)
            .expect("O2/E2 source-qualified id")
            .call_id,
        first_call,
        "O2/E2"
    );
    let requires_action_ids = awaiting
        .iter()
        .filter_map(|event| match &event.kind {
            OutboundKind::SessionThreadStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids },
                ..
            }
            | OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction { event_ids },
            } => Some(event_ids.as_slice()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        requires_action_ids,
        vec![
            &[first_public_id.clone()][..],
            &[first_public_id.clone()][..]
        ],
        "O2/E2 Thread and aggregate reference the exact answerable Event"
    );
    assert!(
        !state.sessions.lock().unwrap()[&session.id]
            .message_was_projected(&session.id, &tool_message.id.0),
        "O2/E2 unresolved sibling retains the source Message"
    );

    let cold = ManagedState::new(runtime.clone())
        .with_config_source(profiles.clone())
        .with_session_repo(repo);
    cold.ensure_session(&session.id).await.unwrap();
    let cold_asks = cold
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .filter(|event| {
            matches!(
                event.kind,
                OutboundKind::AgentToolUse {
                    evaluated_permission: Some(EvaluatedPermission::Ask),
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(cold_asks.len(), 1, "O3/E3");
    assert_eq!(cold_asks[0].id, first_public_id, "O3/E3 stable id");

    runtime.replace_committed_messages(vec![
        (1, tool_message.clone()),
        (
            3,
            Message::new(
                MessageId("occurrence-first-result".into()),
                Role::Tool,
                vec![ContentBlock::tool_result(
                    first_call,
                    vec![ContentBlock::text("first complete")],
                )],
            ),
        ),
    ]);
    *runtime.pending.lock().unwrap() = Some(Pending {
        tool_use_id: sibling_call.into(),
        name: "write".into(),
        input: serde_json::json!({"path":"second.txt"}),
        client_executed: false,
    });
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            3,
            &session.id,
            &run,
            RunLifecycleEventKind::Resumed,
            RunState::Running,
        ),
        lifecycle(
            4,
            &session.id,
            &run,
            RunLifecycleEventKind::Awaiting,
            RunState::Awaiting,
        ),
    ]);
    state.refresh_committed_events(&session.id).await.unwrap();
    state.refresh_committed_events(&session.id).await.unwrap();
    let caught_up = state
        .list_events(&session.id, None, None, false)
        .unwrap()
        .data;
    let tool_events = caught_up
        .iter()
        .filter(|event| event.type_str() == "agent.tool_use")
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 2, "O4/E4 exact two occurrences");
    assert_eq!(
        caught_up
            .iter()
            .filter(|event| event.type_str() == "agent.message")
            .count(),
        1,
        "O4/E4 non-tool content remains exact-once"
    );
    assert_eq!(
        caught_up
            .iter()
            .filter(|event| event.type_str() == "agent.tool_result")
            .count(),
        1,
        "O4/E4 first result exact-once"
    );
    assert!(
        state.sessions.lock().unwrap()[&session.id]
            .message_was_projected(&session.id, &tool_message.id.0),
        "O4/E4 fully classified source advances exactly once"
    );

    let completed_runtime = LifecycleRuntime::default();
    let completed = ManagedState::new(completed_runtime.clone()).with_config_source(profiles);
    let completed_session = completed
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent":"coder", "environment_id":"env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    let completed_run = RunId("run-occurrence-completed".into());
    let completed_call = "call-occurrence-completed";
    completed_runtime.replace_committed_messages(vec![
        (
            2,
            Message::new(
                MessageId::assistant(&completed_run, 0),
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    completed_call,
                    "write",
                    serde_json::json!({"path":"done.txt"}),
                )],
            ),
        ),
        (
            2,
            Message::new(
                MessageId("occurrence-completed-result".into()),
                Role::Tool,
                vec![ContentBlock::tool_result(
                    completed_call,
                    vec![ContentBlock::text("done")],
                )],
            ),
        ),
    ]);
    completed_runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            &completed_session.id,
            &completed_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            &completed_session.id,
            &completed_run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    completed
        .refresh_committed_events(&completed_session.id)
        .await
        .unwrap();
    completed
        .refresh_committed_events(&completed_session.id)
        .await
        .unwrap();
    let completed_events = completed
        .list_events(&completed_session.id, None, None, false)
        .unwrap()
        .data;
    assert_eq!(
        completed_events
            .iter()
            .filter(|event| matches!(
                event.kind,
                OutboundKind::AgentToolUse {
                    evaluated_permission: Some(EvaluatedPermission::Allow),
                    ..
                }
            ))
            .count(),
        1,
        "O5/E5 resolved call projects allow once"
    );
    assert_eq!(
        completed_events
            .iter()
            .filter(|event| event.type_str() == "agent.tool_result")
            .count(),
        1,
        "O5/E5 matching result once"
    );
    assert!(
        completed_events.iter().any(|event| matches!(
            &event.kind,
            OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::EndTurn
            }
        )),
        "O5/E5 completed path reaches EndTurn"
    );
}

#[tokio::test]
async fn model_request_spans_are_paired_and_keep_required_zero_usage_on_error() {
    // Causes: the fixtures below establish `model request spans` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    use awaken_agent_contract::agent::run::Record as RunRecord;
    use awaken_agent_contract::audit::kind::Kind as AuditKind;
    use awaken_agent_contract::audit::record::Record as AuditRecord;
    use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
    use awaken_runtime_contract::compaction::RunCompactionMarker;
    use awaken_runtime_contract::llm::{ModelRequestObservation, TokenUsage};

    let state = ManagedState::new(LifecycleRuntime::default());
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder",
                "environment_id": "env_local"
            }))
            .unwrap(),
            None,
        )
        .await
        .unwrap();
    // Cause/effect graph: C1 one successful committed model observation has
    // usage; C2 one failed observation has no usage; C3 the same recovery
    // prefix records compaction for this exact Run; C4 warm replay supplies
    // the identical prefix; C5 the same facts belong to a child Thread.
    // Effects: E1 each observation yields one stable
    // start/end pair; E2 all required zero-valued usage fields remain; E3
    // compaction is emitted once before spans; E4 replay adds nothing; E5
    // child facts appear only on that child stream.
    // Decision rules: R1=C1=>E1; R2=C2=>E1+E2;
    // R3=C3=>E3; R4=C1+C2+C3+C4=>E4; R5=C5=>E5.
    let run_id = RunId("run-model-observations".into());
    let thread_id = ThreadId(session.id.clone());
    let observations = [
        ModelRequestObservation {
            is_error: false,
            usage: TokenUsage {
                prompt_tokens: 11,
                completion_tokens: 7,
                cache_read_tokens: 3,
                cache_creation_tokens: 2,
            },
            retry_count: 0,
        },
        ModelRequestObservation {
            is_error: true,
            usage: TokenUsage::default(),
            retry_count: 1,
        },
    ];
    let snapshot = RunRecoverySnapshot {
        thread_id: thread_id.clone(),
        claimed_run_id: run_id.clone(),
        runs: vec![RunRecord {
            id: run_id.clone(),
            thread_id,
            state: RunState::Ended(EndCause::NaturalEnd),
        }],
        latest_run_id: Some(run_id.clone()),
        messages: Vec::new(),
        message_commit_cursors: Vec::new(),
        state: vec![RunCompactionMarker::command(&run_id.0)],
        state_commit_cursors: vec![40],
        events: observations
            .into_iter()
            .enumerate()
            .map(|(index, observation)| AuditRecord {
                sequence: index as u64 + 41,
                run_id: run_id.clone(),
                kind: AuditKind::ModelRequestCompleted,
                payload: serde_json::to_value(observation).unwrap(),
            })
            .collect(),
        resume_tickets: Vec::new(),
        thread_version: 1,
        store_cursor: 1,
        next_commit_ordinal: 1,
    };
    let child_events = {
        let mut sessions = state.sessions.lock().unwrap();
        let record = sessions.get_mut(&session.id).unwrap();
        ManagedState::append_run_observation_projections(record, &session.id, &snapshot).unwrap();
        let once = record.events.len();
        ManagedState::append_run_observation_projections(record, &session.id, &snapshot).unwrap();
        assert_eq!(record.events.len(), once, "R4/E4");

        let child_id = "sthr_observation_child";
        let child_run_id = RunId("run-child-model-observations".into());
        let mut child_snapshot = snapshot.clone();
        child_snapshot.thread_id = ThreadId(child_id.into());
        child_snapshot.claimed_run_id = child_run_id.clone();
        child_snapshot.latest_run_id = Some(child_run_id.clone());
        child_snapshot.runs[0].id = child_run_id.clone();
        child_snapshot.runs[0].thread_id = ThreadId(child_id.into());
        child_snapshot.state = vec![RunCompactionMarker::command(&child_run_id.0)];
        for event in &mut child_snapshot.events {
            event.run_id = child_run_id.clone();
        }
        let child_start = record.events.len();
        ManagedState::append_run_observation_projections(record, child_id, &child_snapshot)
            .unwrap();
        record.events[child_start..]
            .iter()
            .cloned()
            .map(|event| {
                let owner = record.event_thread_owners.get(&event.id).cloned();
                (event, owner)
            })
            .collect::<Vec<_>>()
    };
    assert!(
        child_events.iter().all(|(event, owner)| {
            ManagedState::project_event_for_thread(
                &session.id,
                "sthr_observation_child",
                event.clone(),
                owner.as_deref(),
                false,
            )
            .is_some()
                && ManagedState::project_event_for_thread(
                    &session.id,
                    &session.id,
                    event.clone(),
                    owner.as_deref(),
                    false,
                )
                .is_none()
        }),
        "R5/E5"
    );

    let values = serde_json::to_value(
        state
            .list_events(&session.id, None, None, false)
            .unwrap()
            .data,
    )
    .unwrap();
    let events = values.as_array().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "agent.thread_context_compacted")
            .count(),
        1,
        "R3/E3"
    );
    let starts = events
        .iter()
        .filter(|event| event["type"] == "span.model_request_start")
        .collect::<Vec<_>>();
    let ends = events
        .iter()
        .filter(|event| event["type"] == "span.model_request_end")
        .collect::<Vec<_>>();
    assert_eq!((starts.len(), ends.len()), (2, 2), "R1/R2 E1");
    for (start, end) in starts.iter().zip(&ends) {
        assert_eq!(end["model_request_start_id"], start["id"], "E1");
    }
    assert_eq!(ends[0]["is_error"], false);
    assert_eq!(ends[0]["model_usage"]["input_tokens"], 11);
    assert_eq!(ends[0]["model_usage"]["cache_creation_input_tokens"], 2);
    assert_eq!(ends[1]["is_error"], true);
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        assert_eq!(
            ends[1]["model_usage"][field], 0,
            "R2/E2 {field} must be present"
        );
    }
}

fn retained_outcome_fixture(
    session_id: &str,
    batch_id: &str,
    max_iterations: u32,
) -> (awaken_session_contract::PersistedSession, String) {
    let mut persisted = crate::state::tests::sample_persisted(session_id);
    let mut batch = awaken_session_contract::SessionEventBatch::compile(
        session_id,
        batch_id,
        vec![SessionEventInput::DefineOutcome {
            description: "ship".into(),
            rubric: SessionOutcomeRubric::Text {
                content: "FINAL".into(),
            },
            max_iterations: Some(max_iterations),
        }],
    )
    .expect("retained Outcome command");
    batch.events[0].processed = true;
    let outcome_id = match &batch.events[0].event {
        SessionEventCommand::DefineOutcome { outcome_id, .. } => outcome_id.clone(),
        _ => unreachable!("compiled DefineOutcome"),
    };
    persisted.event_batches.push(batch);
    (persisted, outcome_id)
}

#[tokio::test]
async fn durable_outcome_projector_is_warm_cold_restart_and_active_active_stable() {
    // Causes: the fixtures below establish `durable outcome projector` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: committed Session, Run, and transcript facts are the only durable
    // truth; live, warm, and cold projections may not diverge or mint a second lifecycle.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 retained root DefineOutcome exists; C2 its exact
    // Thread aggregate is nonterminal (query returns None); C3 it becomes
    // completed with two ordered evaluations and one already-committed
    // assistant message; C4 two refreshers observe C3 concurrently; C5 the
    // disposable Managed cache is replaced once/cold and again/restart.
    // Effects: E1 C2 emits no evaluation fact; E2 C3 emits exactly one
    // start/ongoing/end per iteration and the latest Session summary; E3 C4
    // duplicates nothing; E4 C5 rebuilds the same stable span ids/summary;
    // E5 the transcript projector emits C3's message exactly once and the
    // Outcome projector never copies report messages. Decision table:
    // P1=C1+C2=>E1; P2=C1+C3=>E2+E5; P3=C1+C3+C4=>E3;
    // P4=C1+C3+C5=>E4+E5.
    let session_id = "session-durable-outcome";
    let repository = Arc::new(ephemeral_session_repo());
    let runtime = RehydrateFake::default();
    let (persisted, outcome_id) =
        retained_outcome_fixture(session_id, "outcome-projector-batch", 2);
    crate::state::test_support::create_session_fixture(
        repository.as_ref(),
        DEFAULT_SCOPE,
        persisted,
    )
    .await;

    let warm = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repository.clone()));
    warm.refresh_committed_events(session_id).await.unwrap();
    assert!(
        warm.list_events(session_id, None, None, false)
            .unwrap()
            .data
            .iter()
            .all(|event| !event.type_str().starts_with("span.outcome_evaluation_")),
        "P1/E1"
    );

    let final_message = Message::text(
        MessageId("outcome-final-message".into()),
        Role::Assistant,
        "FINAL",
    );
    *runtime.committed.lock().unwrap() = Some(vec![final_message.clone()]);
    runtime.outcome_projections.lock().unwrap().insert(
        (session_id.into(), outcome_id.clone()),
        CommittedOutcomeProjection::Completed(OutcomeReport {
            iterations: vec![
                OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: outcome_id.clone(),
                    description: "ship".into(),
                    iteration: 0,
                    result: "needs_revision".into(),
                    explanation: "add FINAL".into(),
                },
                OutcomeIteration {
                    messages: vec![final_message],
                    outcome_id: outcome_id.clone(),
                    description: "ship".into(),
                    iteration: 1,
                    result: "satisfied".into(),
                    explanation: "done".into(),
                },
            ],
        }),
    );
    let (left, right) = tokio::join!(
        warm.refresh_committed_events(session_id),
        warm.refresh_committed_events(session_id)
    );
    left.unwrap();
    right.unwrap();

    let outcome_observation = |state: &ManagedState| {
        let events = state
            .list_events(session_id, None, None, false)
            .unwrap()
            .data;
        let span_ids = events
            .iter()
            .filter(|event| event.type_str().starts_with("span.outcome_evaluation_"))
            .map(|event| event.id.clone())
            .collect::<Vec<_>>();
        let message_count = events
            .iter()
            .filter(|event| matches!(event.kind, OutboundKind::AgentMessage { .. }))
            .count();
        let evaluations = state.get_session(session_id).unwrap().outcome_evaluations;
        (span_ids, message_count, evaluations)
    };
    let warm_observation = outcome_observation(&warm);
    assert_eq!(warm_observation.0.len(), 6, "P2-P3/E2-E3");
    assert_eq!(
        warm_observation
            .0
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        6,
        "P3/E3"
    );
    assert_eq!(warm_observation.1, 1, "P2-P3/E5");
    assert_eq!(warm_observation.2.len(), 1, "P2/E2");
    assert_eq!(warm_observation.2[0].iteration, 1, "P2/E2 latest");

    for label in ["cold", "restart"] {
        let rebuilt = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
        rebuilt.refresh_committed_events(session_id).await.unwrap();
        let rebuilt_observation = outcome_observation(&rebuilt);
        assert_eq!(rebuilt_observation, warm_observation, "P4/E4-E5 {label}");
    }
}

#[tokio::test]
async fn errored_outcome_projection_is_visible_once_and_preserves_unrelated_root_errors() {
    // Causes: the fixtures below establish `errored outcome projection is visible once and` with
    // the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a retained Outcome commits typed Errored;
    // C2 no ordinary failure lifecycle is yet visible; C3 the exact
    // `source_run_id` later appears as a root lifecycle failure; C4 warm
    // refreshers race and disposable state is rebuilt cold/restarted; C5 a
    // root failure has no committed Outcome owner. Effects: E1 C1 emits one
    // stable classified session.error in both Session and primary Thread;
    // E2 Errored emits no partial Outcome spans or summary; E3 C3 does not
    // duplicate E1; E4 C4 preserves the same id/payload and count; E5 C5
    // remains owned by the ordinary lifecycle projector. Constraint: only
    // exact typed RunId equality transfers ownership; message/code parsing,
    // a protocol failure registry, and rubric `failed` are not involved.
    //
    // | Rule | Outcome terminal | lifecycle relation | cache       | Effect |
    // | R1   | Errored          | absent             | warm        | E1+E2  |
    // | R2   | Errored          | exact source Run   | warm/racing | E1-E3  |
    // | R3   | Errored          | exact source Run   | cold/restart| E1-E4  |
    // | R4   | none             | ordinary root Run  | warm        | E5      |
    let session_id = "session-errored-outcome";
    let repository = Arc::new(ephemeral_session_repo());
    let runtime = RehydrateFake::default();
    let (persisted, outcome_id) = retained_outcome_fixture(session_id, "errored-outcome-batch", 1);
    crate::state::test_support::create_session_fixture(
        repository.as_ref(),
        DEFAULT_SCOPE,
        persisted,
    )
    .await;
    let source_run_id = RunId(format!("outcome/{outcome_id}/grader/0/run"));
    runtime.outcome_projections.lock().unwrap().insert(
        (session_id.into(), outcome_id),
        CommittedOutcomeProjection::Errored(OutcomeFailure {
            code: "outcome_invalid_grader_output".into(),
            message: "Judge returned invalid JSON".into(),
            source_run_id: Some(source_run_id.clone()),
        }),
    );
    let warm = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repository.clone()));

    let observe = |state: &ManagedState| {
        let session_events = state
            .list_events(session_id, None, None, false)
            .unwrap()
            .data;
        let session_errors = session_events
            .iter()
            .filter_map(|event| match &event.kind {
                OutboundKind::SessionError { error } => Some((
                    event.id.clone(),
                    error.kind,
                    error.message.clone(),
                    serde_json::to_value(&error.retry_status).unwrap(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        let primary_id = public_thread_id(session_id, session_id);
        let primary_errors = state
            .list_thread_events(session_id, &primary_id, None, None)
            .unwrap()
            .data
            .into_iter()
            .filter_map(|event| match event.kind {
                OutboundKind::SessionError { error } => Some((
                    event.id,
                    error.kind,
                    error.message,
                    serde_json::to_value(error.retry_status).unwrap(),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        let outcome_span_count = session_events
            .iter()
            .filter(|event| event.type_str().starts_with("span.outcome_evaluation_"))
            .count();
        let outcome_summary_count = state
            .get_session(session_id)
            .unwrap()
            .outcome_evaluations
            .len();
        (
            session_errors,
            primary_errors,
            outcome_span_count,
            outcome_summary_count,
        )
    };

    warm.refresh_committed_events(session_id).await.unwrap();
    let initial = observe(&warm);
    assert_eq!(initial.0.len(), 1, "R1/E1");
    assert_eq!(initial.0, initial.1, "R1/E1 Session/primary parity");
    assert_eq!(initial.0[0].1, "unknown_error", "R1/E1 classification");
    assert_eq!(initial.0[0].2, "Judge returned invalid JSON", "R1/E1");
    assert_eq!((initial.2, initial.3), (0, 0), "R1/E2");

    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            session_id,
            &source_run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            session_id,
            &source_run_id,
            RunLifecycleEventKind::Failed,
            RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::Inference {
                    code: "provider_request_failed".into(),
                    message: "duplicate lifecycle failure must not surface".into(),
                },
            )),
        ),
    ]);
    let (left, right) = tokio::join!(
        warm.refresh_committed_events(session_id),
        warm.refresh_committed_events(session_id)
    );
    left.unwrap();
    right.unwrap();
    assert_eq!(observe(&warm), initial, "R2/E1-E3");

    for label in ["cold", "restart"] {
        let rebuilt = ManagedState::new(runtime.clone()).with_session_repo(repository.clone());
        rebuilt.refresh_committed_events(session_id).await.unwrap();
        assert_eq!(observe(&rebuilt), initial, "R3/E1-E4 {label}");
    }

    let ordinary_session_id = "session-ordinary-root-error";
    let ordinary_repository = Arc::new(ephemeral_session_repo());
    crate::state::test_support::create_session_fixture(
        ordinary_repository.as_ref(),
        DEFAULT_SCOPE,
        crate::state::tests::sample_persisted(ordinary_session_id),
    )
    .await;
    let ordinary_runtime = RehydrateFake::default();
    let ordinary_run_id = RunId("ordinary-root-run".into());
    ordinary_runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            1,
            ordinary_session_id,
            &ordinary_run_id,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            2,
            ordinary_session_id,
            &ordinary_run_id,
            RunLifecycleEventKind::Failed,
            RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::Inference {
                    code: "provider_request_failed".into(),
                    message: "ordinary root failure".into(),
                },
            )),
        ),
    ]);
    let ordinary = ManagedState::new(ordinary_runtime).with_session_repo(ordinary_repository);
    ordinary
        .refresh_committed_events(ordinary_session_id)
        .await
        .unwrap();
    let ordinary_errors = ordinary
        .list_events(ordinary_session_id, None, None, false)
        .unwrap()
        .data
        .into_iter()
        .filter_map(|event| match event.kind {
            OutboundKind::SessionError { error } => Some(error.message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ordinary_errors, vec!["ordinary root failure"], "R4/E5");
}
