use super::*;
use awaken_ext_goal::controller::{CommittedOutcome, IterationReport, Report};
use awaken_ext_goal::outcome::{Definition, EvaluationResult, ExecutionFailure, Id, State};
use awaken_ext_goal::state::{Binding, ThreadOutcomeState, worker_run_id};

#[tokio::test]
async fn outcome_queries_need_no_frozen_runtime_or_environment_and_preserve_commits() {
    // Cause/effect graph: C1 Session has no installed frozen projection; C2 its
    // runtime envelope is cold or already cached; C3 Outcome is absent, Defined,
    // Errored, or Completed; C4 physical admission rejects an unfenced realization.
    // Q1 absent => None; Q2 Defined => None; Q3 Errored => exact failure/source
    // Run; Q4 interrupted completion => exact report without invented usage.
    // EACH Q1-Q4 x cold/cached => zero Environment admission/persistence,
    // zero inference, unchanged full recovery snapshot, and the same cached
    // context (or no context). C4 makes an accidental execution-context build
    // fail rather than invisibly installing a redundant physical Environment.
    // Graded completion decoding remains with the Controller Q3/Q4 table;
    // Deployment SDK owns the real Preparing-to-Frozen process boundary.
    for cached in [false, true] {
        let model = Arc::new(GatedModel {
            gate: Arc::new(tokio::sync::Notify::new()),
            reached: Arc::new(tokio::sync::Notify::new()),
            calls: AtomicUsize::new(0),
        });
        let host = Arc::new(SharedHost::new(model.clone(), "stub").await);
        let managed = install_test_dispatch_runtime(&host);
        let thread = ThreadId(format!("outcome-pure-query-cached-{cached}"));
        if cached {
            host.ctx_for_session_coordination(&thread.0, None)
                .await
                .expect("cache a coordination envelope only");
        }
        let sink = Arc::new(BindingOrderSink {
            host: Arc::downgrade(&host),
            authorize_calls: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            observed_before_publish: AtomicBool::new(false),
            fail: true,
            require_realization: true,
            owned_session_id: None,
            committed_environment: None,
        });
        managed
            .realization()
            .unwrap()
            .install_environment_binding_sink(sink.clone());
        let commit = host.commit_for_read(&thread.0).await.unwrap();
        let state = ThreadOutcomeState::new(&thread, commit.as_ref(), commit.as_ref());
        let mut id = Id("query-only".into());
        let mut aggregate = State::new(id.clone(), 0);
        let worker = worker_run_id(&id, 0);
        for rule in 1..=4 {
            if rule == 4 {
                id = Id("query-completed".into());
                aggregate = State::new(id.clone(), 0);
            }
            if rule == 2 || rule == 4 {
                let snapshot = ExecutableAgentSnapshot::builder("query-worker")
                    .model(test_model_binding())
                    .build();
                state
                    .create(
                        &Definition::new("ship", "verified", 2).unwrap(),
                        &Binding {
                            worker: snapshot.clone(),
                            grader: snapshot,
                        },
                        &aggregate,
                    )
                    .await
                    .unwrap();
                if rule == 4 {
                    let expected = aggregate.version;
                    assert!(aggregate.interrupt());
                    state
                        .commit_if_current(expected, &aggregate, None)
                        .await
                        .unwrap();
                }
            } else if rule == 3 {
                let expected = aggregate.version;
                aggregate.start(worker.clone()).unwrap();
                aggregate
                    .fail(ExecutionFailure::WorkerFailed(
                        "exact retained failure".into(),
                    ))
                    .unwrap();
                state
                    .commit_if_current(expected, &aggregate, None)
                    .await
                    .unwrap();
            }
            let before = commit.recovery_snapshot(&thread, &worker).await.unwrap();
            let projection = host
                .committed_outcome_projection(&thread.0, &id.0)
                .await
                .unwrap();
            if rule < 3 {
                assert!(projection.is_none(), "Q{rule} cached={cached}");
            } else if rule == 3 {
                assert_eq!(
                    projection,
                    Some(CommittedOutcome::Errored {
                        failure: ExecutionFailure::WorkerFailed("exact retained failure".into()),
                        source_run_id: Some(worker.clone()),
                    }),
                    "Q3 exact retained failure"
                );
            } else {
                assert_eq!(
                    projection,
                    Some(CommittedOutcome::Completed(Report {
                        iterations: vec![IterationReport {
                            messages: Vec::new(),
                            outcome_id: id.clone(),
                            description: "ship".into(),
                            iteration: 0,
                            result: EvaluationResult::Interrupted,
                            explanation: "the outcome was interrupted".into(),
                            usage: None,
                        }],
                    })),
                    "Q4 exact terminal report without executing a grader"
                );
            }
            let after = commit.recovery_snapshot(&thread, &worker).await.unwrap();
            assert_eq!(
                before, after,
                "Q{rule} query preserves the full committed prefix"
            );
            assert_eq!(
                sink.authorize_calls.load(Ordering::SeqCst),
                0,
                "Q{rule} no Environment admission"
            );
            assert_eq!(
                sink.calls.load(Ordering::SeqCst),
                0,
                "Q{rule} no Environment persistence"
            );
            assert_eq!(
                model.calls.load(Ordering::SeqCst),
                0,
                "Q{rule} no inference"
            );
            assert!(
                host.session_environment(&thread.0).await.is_none(),
                "Q{rule} no Environment"
            );
            assert_eq!(
                host.session_slots
                    .read(&thread.0, |slot| slot.runtime.is_some()),
                Some(cached),
                "Q{rule} no execution context construction"
            );
        }
    }
}
