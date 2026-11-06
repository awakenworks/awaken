// @formal evidence session.runtime_projection_failed_refresh_stutters_warm_state
//! Failure-frame checks for the actual asynchronous committed-prefix refresh.

use super::*;
use crate::state::test_support::ConflictInjectingRepo;

pub(super) use crate::state::session_record::assert_record_frame_eq;

#[tokio::test]
async fn exhausted_projection_cache_revision_preserves_complete_warm_frame() {
    // Cause/effect table at the last fallible publication operation:
    // C1 warm record already exists; C2 a stable committed Running/text prefix
    // builds a changed candidate; C3 the existing cache CAS is u64::MAX.
    // R1 C1+C2+C3 => the real checked increment returns its exhaustion error,
    // preserves every CURRENT frame field, and sends no subscriber event.
    // This is the numeric boundary of the actual publisher after the full
    // private reducer, not a claim that a practical workload reaches 2^64
    // publications. Earlier IO and correlation failures have separate rules.
    let runtime = LifecycleRuntime::default();
    let state = ManagedState::new(runtime.clone()).await;
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    state
        .application
        .activities()
        .begin_activity(&awaken_session_types::SessionId::from(&session.id))
        .await
        .unwrap();
    runtime.lifecycle.lock().unwrap().push(lifecycle(
        1,
        &session.id,
        &RunId("cache-exhaustion-run".into()),
        RunLifecycleEventKind::Running,
        RunState::Running,
    ));
    runtime.replace_committed_messages(vec![(
        2,
        Message::text(
            MessageId("cache-exhaustion-message".into()),
            Role::Assistant,
            "the candidate must remain private on exhaustion",
        ),
    )]);
    let before = {
        let mut sessions = state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"));
        let current = sessions.get_mut(&session.id).unwrap();
        current.cache_revision = u64::MAX;
        current.clone()
    };
    let (_, mut live) = state.stream_subscribe(&session.id).unwrap();
    let error = state
        .refresh_committed_events(&session.id)
        .await
        .expect_err("R1 candidate increment must fail before publication");
    assert!(
        error
            .to_string()
            .contains("Managed projection cache revision exhausted"),
        "R1 exact final publication failure: {error}"
    );
    assert_record_frame_eq(
        state
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .get(&session.id)
            .unwrap(),
        &before,
    );
    assert!(
        matches!(
            live.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "R1 failed final publication must not broadcast"
    );
}

async fn read_cut_fixture(with_child: bool) -> (RehydrateFake, Arc<ManagedState>, String) {
    let runtime = RehydrateFake::default();
    runtime.delegate_ids.lock().push("researcher".into());
    let inner = Arc::new(ephemeral_session_repo().await);
    let repository = Arc::new(ConflictInjectingRepo::new(
        inner.clone(),
        0,
        runtime.reads.clone(),
    ));
    let state = Arc::new({
        let session_fixture_runtime = runtime.clone();
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
    });
    let id = if with_child {
        let session = state
            .create_session(
                serde_json::from_value(serde_json::json!({
                    "agent": "coder", "environment_id": "env_local"
                }))
                .unwrap(),
                awaken_tenancy::DEFAULT_WORKSPACE_ID,
            )
            .await
            .unwrap();
        let root_run = RunId("refresh-read-root".into());
        let child_run = RunId("refresh-read-child".into());
        runtime.install_agent_coordination_prefix(
            &session.id,
            root_run.clone(),
            0,
            "refresh-child-call",
            CoordinatedThreadLink {
                session_id: awaken_session_types::SessionId::from(&session.id),
                thread_id: ThreadId("refresh-child".into()),
                target: CoordinatedThreadTarget::Agent {
                    agent_id: "researcher".into(),
                },
                created_by_operation_id: ToolBatch::operation_id_for_step(
                    &root_run,
                    0,
                    "refresh-child-call",
                ),
                latest_run_id: Some(child_run.clone()),
            },
        );
        runtime.lifecycle.lock().push(lifecycle(
            1,
            "refresh-child",
            &child_run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ));
        runtime.committed_by_thread.lock().insert(
            "refresh-child".into(),
            vec![Message::text(
                MessageId("refresh-child-message".into()),
                Role::Assistant,
                "warm child transcript",
            )],
        );
        session.id
    } else {
        let id = "refresh-retained-outcome";
        let (persisted, _) = retained_outcome_fixture(id, "refresh-outcome", 1);
        crate::state::test_support::create_session_fixture(
            inner.as_ref(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
            persisted,
        )
        .await;
        id.into()
    };
    state
        .refresh_committed_events(&id)
        .await
        .expect("the fixture builds a real warm projection");
    runtime.reads.reset();
    (runtime, state, id)
}

#[tokio::test]
async fn every_refresh_read_failure_preserves_current_frame_after_competing_publication() {
    // Actual-call cause/effect table, not a copy of the reducer/selector:
    // C1 retained Outcome with no child, or one accepted coordinated child;
    // C2 every ordinal of the COMPLETE observed warm-refresh read trace below;
    // C3 no competing writer, or a real durable Session title update publishes
    // through the application's update path while C2's read is paused.
    // R1 each C1 x C2 x C3 preserves ENTRY through the paused read, returns its
    // exact injected Unavailable and runs no later refresh read; the full frame
    // at RELEASE (including a competitor's
    // legitimate success) survives and the failed refresh broadcasts nothing.
    // R2 remove the read fault => real refresh and replay succeed; replay keeps
    // the complete frame. The first repository cut precedes candidate cloning;
    // all others follow it. The separate orphan test owns private-reducer errors
    // and the exhaustion test owns the final publisher error.
    // Bounds: these two actual topologies, one retained
    // Outcome or one child, every listed read ordinal, both writer schedules.
    // This covers control-flow cuts, not arbitrary topology/SQL refinement.
    // Usage and lifecycle reuse the already selected root/child snapshots.
    // Independent usage/lifecycle reads and the extra Session read are absent.
    // The candidate root read now precedes its terminal Outcome query, so a
    // failed query cannot pair an older terminal with a newer root. R0 below
    // fixes every actual I/O cut in that order; R1 still injects each cut and
    // preserves the entry frame, competitor publication and zero broadcast.
    // R3 the retained Outcome has processed=true but no observed predecessor
    // anchor: its update fact persists while its marker is withheld. The child
    // topology has no pending predecessor and retains the original positive
    // broadcast assertion. The real predecessor completion case is owned by
    // local_session_update_waits_for_its_accepted_input_predecessor.
    // Final validation selects its fresh root before the
    // relation query: root-only projection reuses that exact fence, while a
    // child prefix retains the later root read after its child snapshot. Every
    // remaining actual cut is exercised, and an unexpected read fails R0.
    let mut checked_rules = 0;
    for with_child in [false, true] {
        let expected = if with_child {
            vec![
                "session",
                "root_snapshot",
                "coordination",
                "child_snapshot",
                "root_snapshot",
                "coordination",
                "child_snapshot",
                "root_snapshot",
                "session",
            ]
        } else {
            vec![
                "session",
                "root_snapshot",
                "outcome",
                "coordination",
                "root_snapshot",
                "coordination",
                "session",
            ]
        };
        let (runtime, state, id) = read_cut_fixture(with_child).await;
        // This test asserts the exact source-read trace of one refresh; the
        // warm/cold oracle would read the prefix a second time.
        state.disable_warm_cold_oracle();
        state.refresh_committed_events(&id).await.unwrap();
        assert_eq!(
            runtime.reads.trace(),
            expected,
            "R0 exact actual read boundary"
        );
        for (cut, port) in expected.iter().enumerate().rev() {
            for competing in [false, true] {
                let (runtime, state, id) = read_cut_fixture(with_child).await;
                state.disable_warm_cold_oracle();
                let entry = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                    .get(&id)
                    .unwrap()
                    .clone();
                let (_, mut live) = state.stream_subscribe(&id).unwrap();
                runtime.reads.fail_at(cut);
                let (entered, release) = runtime.reads.pause_at(cut);
                let refresh = {
                    let state = state.clone();
                    let id = id.clone();
                    tokio::spawn(async move { state.refresh_committed_events(&id).await })
                };
                tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
                    .await
                    .expect("R1 every configured read cut is actually reached");
                assert_eq!(
                    runtime.reads.trace(),
                    expected[..=cut],
                    "R1 exact prefix before pause"
                );
                assert_record_frame_eq(
                    state
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                        .get(&id)
                        .unwrap(),
                    &entry,
                );
                if competing {
                    state
                        .update_session(
                            &id,
                            awaken_session_application::SessionUpdateCommand {
                                title: Some(awaken_session_contract::SessionFieldUpdate::Replace(
                                    "competing publisher".into(),
                                )),
                                metadata: None,
                                budget: None,
                                tools: None,
                                mcp_update: None,
                                idempotency_key: None,
                                request_fingerprint: "refresh-competing-title".into(),
                                if_match: None,
                            },
                        )
                        .await
                        .expect("R1 competing durable update succeeds independently");
                    let current = state
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                        .get(&id)
                        .unwrap()
                        .clone();
                    assert!(
                        current.cache_revision > entry.cache_revision,
                        "R1 actual competing publisher advances cache"
                    );
                    assert_eq!(
                        current.session.title.as_deref(),
                        Some("competing publisher")
                    );
                    let durable = state
                        .roles
                        .roots
                        .session(&SessionId::from(&id))
                        .await
                        .unwrap();
                    let fact = awaken_session_contract::SessionUpdateHistory::for_session(&durable)
                        .unwrap()
                        .facts()
                        .last()
                        .unwrap();
                    assert!(
                        fact.changes().title(),
                        "R1 actual competing Root fact is retained"
                    );
                    if with_child {
                        assert_eq!(
                            fact.predecessor_operation_id(),
                            None,
                            "R1 no pending predecessor"
                        );
                        assert!(
                            matches!(
                                live.try_recv().unwrap().kind,
                                OutboundKind::SessionUpdated { .. }
                            ),
                            "R1 successful competing publication is visible before the failure"
                        );
                    } else {
                        let predecessor = fact
                            .predecessor_operation_id()
                            .expect("R3 retained Outcome command");
                        let entry = durable
                            .event_batches
                            .iter()
                            .flat_map(|batch| &batch.events)
                            .find(|entry| entry.event.operation_id() == predecessor)
                            .unwrap();
                        assert!(
                            entry.projection_anchor.is_none(),
                            "R3 historical effect remains unobserved"
                        );
                        assert!(
                            awaken_managed_session_events::derive_session_update_events(&durable)
                                .unwrap()
                                .is_empty(),
                            "R3 exact Root fact waits for its original predecessor"
                        );
                        assert!(
                            matches!(
                                live.try_recv(),
                                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                            ),
                            "R3 pending predecessor withholds the competing marker"
                        );
                    }
                }
                let before_failure = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                    .get(&id)
                    .unwrap()
                    .clone();
                let reads_before_release = runtime.reads.trace();
                release.notify_one();
                let error = tokio::time::timeout(std::time::Duration::from_secs(5), refresh)
                    .await
                    .unwrap()
                    .unwrap()
                    .expect_err("R1 selected actual read must fail closed");
                assert!(
                    error
                        .to_string()
                        .contains(&format!("scripted {port} read failure")),
                    "R1 exact error at topology={with_child} cut={cut} competing={competing}: {error}"
                );
                assert_eq!(
                    runtime.reads.trace(),
                    reads_before_release,
                    "R1 error terminates this refresh without later reads"
                );
                assert_record_frame_eq(
                    state
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                        .get(&id)
                        .unwrap(),
                    &before_failure,
                );
                assert!(
                    matches!(
                        live.try_recv(),
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                    ),
                    "R1 failure must not broadcast after another writer's legitimate event"
                );
                runtime.reads.reset();
                state
                    .refresh_committed_events(&id)
                    .await
                    .expect("R2 fault removal recovers without restart");
                let repaired = state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                    .get(&id)
                    .unwrap()
                    .clone();
                state
                    .refresh_committed_events(&id)
                    .await
                    .expect("R2 repaired prefix replays");
                assert_record_frame_eq(
                    state
                        .sessions
                        .lock()
                        .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                        .get(&id)
                        .unwrap(),
                    &repaired,
                );
                checked_rules += 1;
            }
        }
    }
    assert_eq!(
        checked_rules, 32,
        "R1-R2 every read cut and competing-writer schedule executes"
    );
}

#[tokio::test]
async fn aggregate_usage_reuses_fenced_snapshots_without_independent_accounting_reads() {
    // U1 zero/nonzero/saturating root+child counters x warm/cold cache -> the
    // canonical aggregate equals the sum from the same transcript prefixes.
    // U2 independent usage ports fail -> zero calls to those redundant ports,
    // while root/child snapshot errors remain covered at every cut above.
    // U3 child active clocks never add to Session active time. Restoring the old
    // extra query fails this test instead of merely changing a timing threshold.
    for (root_tokens, child_tokens) in [(0_u64, 0_u64), (12, 7), (u64::MAX, 7)] {
        for cold in [false, true] {
            let (runtime, state, id) = read_cut_fixture(true).await;
            for (thread, tokens) in [(&id[..], root_tokens), ("refresh-child", child_tokens)] {
                runtime.usage_by_thread.lock().insert(
                    thread.into(),
                    awaken_session_contract::SessionUsage {
                        input_tokens: tokens,
                        active_seconds: 999,
                        ..Default::default()
                    },
                );
            }
            runtime.reads.fail("usage", true);
            runtime.reads.fail("child_usage", true);
            if cold {
                state
                    .sessions
                    .lock()
                    .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                    .remove(&id);
            }
            state.refresh_committed_events(&id).await.unwrap();
            let session = state.get_session(&id).unwrap();
            assert_eq!(
                session.usage.input_tokens,
                Some(root_tokens.saturating_add(child_tokens))
            );
            assert_eq!(session.usage.active_seconds, Some(0));
            assert_eq!(runtime.reads.count("usage"), 0);
            assert_eq!(runtime.reads.count("child_usage"), 0);
        }
    }
}

#[tokio::test]
async fn session_usage_reads_closed_intervals_only_and_never_the_read_clock() {
    // Observable change: GET /v1/sessions/{id} `usage.active_seconds` reports
    // closed runtime intervals only, so it no longer grows while an interval
    // is open; it advances when the interval closes.
    // Causes: C1 the Session root holds an open runtime interval and no closed
    // active time; C2 the same committed prefix is read at two clock instants
    // 5 s and 65 s after the interval opened, each refresh also checked
    // against a cold rebuild. Effects: E1 both reads publish byte-identical
    // usage; E2 active time is the durable closed total (0 s), never an
    // elapsed reading of the open interval. Rules: R1 C1+C2 => E1+E2.
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_000_000));
    let state = crate::state::test_support::managed_state_from_parts(
        Arc::new(LifecycleRuntime::default()),
        None,
        awaken_protocol_managed_testkit::ephemeral_session_repository().await,
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution,
        clock.clone(),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    );
    let session = state
        .create_session(
            serde_json::from_value(serde_json::json!({
                "agent": "coder", "environment_id": "env_local"
            }))
            .unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let opened = state
        .application
        .activities()
        .begin_activity(&SessionId::from(&session.id))
        .await
        .unwrap();
    assert!(opened.running_interval.is_some(), "C1 interval is open");
    assert_eq!(opened.runtime_active_millis, 0, "C1 no closed active time");
    let mut reads = Vec::new();
    for elapsed_ms in [5_000, 60_000] {
        clock.advance(elapsed_ms);
        state.refresh_committed_events(&session.id).await.unwrap();
        reads.push(serde_json::to_value(state.get_session(&session.id).unwrap().usage).unwrap());
    }
    assert_eq!(reads[0], reads[1], "R1/E1 the read clock changes nothing");
    assert_eq!(
        reads[1]["active_seconds"],
        serde_json::json!(0),
        "R1/E2 only closed intervals count"
    );
}

#[tokio::test]
async fn historical_usage_prefix_failure_preserves_warm_frame_and_repair_rebuilds_identically() {
    // Causes: C1 an unowned lifecycle period closes at commit 25; C2 cumulative
    // usage Sets commit at 20/30; C3 recovery omits their coordinates; C4 a
    // warm reader or independent cold reader consumes the repaired prefix.
    // Effects: E1 C1+C2 publishes exactly 5 tokens at the close, not the later
    // 15-token cell; E2 C3 fails the actual candidate with its existing internal
    // fault and preserves the warm frame; E3 both C4 values publish identical
    // durable ids/payloads. Rules: U1 C1+C2+C3 => E2; U2 after coordinate repair
    // => E1+E3. This exercises the migrated usage rule through the real async
    // refresh and fallback-close adapter, not a second historical reducer.
    use awaken_agent_contract::agent::state::{Command, MergePolicy, Scope};
    let runtime = LifecycleRuntime::default();
    let repository = Arc::new(ephemeral_session_repo().await);
    let replica = || async {
        let state = {
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
        };
        state.disable_warm_cold_oracle();
        state
    };
    let warm = replica().await;
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
    let run = RunId("historical-usage".into());
    runtime.lifecycle.lock().unwrap().extend([
        lifecycle(
            10,
            &session.id,
            &run,
            RunLifecycleEventKind::Running,
            RunState::Running,
        ),
        lifecycle(
            25,
            &session.id,
            &run,
            RunLifecycleEventKind::Completed,
            RunState::Ended(EndCause::NaturalEnd),
        ),
    ]);
    *runtime.state.lock().unwrap() = [5, 15]
        .map(|input_tokens| {
            let mut usage = awaken_agent_contract::thread::usage::ThreadUsage::default();
            usage.record(
                "model",
                awaken_agent_contract::audit::model_request::TokenUsage {
                    prompt_tokens: input_tokens,
                    ..Default::default()
                },
            );
            Command::set(
                Scope::Thread,
                MergePolicy::Commutative,
                awaken_agent_contract::thread::usage::THREAD_USAGE_STATE_KEY,
                serde_json::to_value(usage).unwrap(),
            )
        })
        .into();
    let before = warm
        .sessions
        .lock()
        .unwrap_or_else(|poisoned| panic!("{poisoned}"))
        .get(&session.id)
        .unwrap()
        .clone();
    let error = warm
        .refresh_committed_events(&session.id)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("state recovery omitted commit coordinates"),
        "U1: {error}"
    );
    assert_record_frame_eq(
        warm.sessions
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .get(&session.id)
            .unwrap(),
        &before,
    );
    *runtime.state_cursors.lock().unwrap() = vec![20, 30];
    warm.refresh_committed_events(&session.id).await.unwrap();
    let history = super::timeline_scenarios::durable_history(&warm, &session.id);
    let usage = history
        .iter()
        .find(|event| event.1["type"] == "session.usage")
        .unwrap();
    assert_eq!(usage.1["usage"]["input_tokens"], 5, "U2 historical close");
    let cold = replica().await;
    cold.refresh_committed_events(&session.id).await.unwrap();
    assert_eq!(
        super::timeline_scenarios::durable_history(&cold, &session.id),
        history,
        "U2 warm/cold ids and bytes"
    );
}
