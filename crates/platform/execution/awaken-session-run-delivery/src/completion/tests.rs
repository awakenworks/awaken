use super::*;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::{
    Event as StreamEvent, Observation as StreamObservation,
};
use awaken_runtime_contract::runs::RunError;
use awaken_store_inmem::MemoryStreamSink;

fn text_event(run_id: &str, delta: &str) -> StreamEvent {
    StreamEvent {
        run_id: RunId(run_id.into()),
        kind: AgentEvent::Delta(Delta::TextDelta {
            delta: delta.into(),
        }),
    }
}

/// A3: dropping the guard (caller future dropped / timed out) removes the
/// waiter, so a run that never settles does not leak an entry.
#[tokio::test]
async fn dropping_the_guard_removes_the_registration() {
    let registry = Arc::new(CompletionRegistry::default());
    let (rx, guard) = registry
        .register(&RunId("r".into()), None)
        .expect("healthy registry");
    assert_eq!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .len(),
        1
    );
    drop(guard);
    drop(rx);
    assert!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .is_empty(),
        "the guard removed the leaked waiter"
    );
}

/// C1 a settlement wakes the caller once; C2 its guard still owns the live
/// observation; C3 canonical completion drops the guard. E1 notification does
/// not remove C2; E2 C3 removes the slot and live route. Rules C1+C2=>E1,
/// C1+C2+C3=>E2; a repeated notification consumes no second sender.
#[tokio::test]
async fn settled_notifies_once_and_the_guard_clears_the_slot() {
    let registry = Arc::new(CompletionRegistry::default());
    let (rx, guard) = registry
        .register(&RunId("r".into()), None)
        .expect("healthy registry");
    registry.settled(&RunId("r".into()), &RunState::Awaiting);
    assert!(matches!(rx.await, Ok(RunState::Awaiting)));
    assert!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .get("r")
            .unwrap()[0]
            .settled
            .is_none(),
        "E1 sender consumed, route retained"
    );
    registry.settled(&RunId("r".into()), &RunState::Awaiting);
    drop(guard);
    assert!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .is_empty()
    );
}

#[tokio::test]
async fn session_user_run_observation_covers_activation_and_peer_recovery() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_agent_contract::agent::run::EndCause;
    use awaken_session_execution_contract::SessionRunActivation;

    // Cause/effect graph: C1 activation returns Activated/AlreadyActivated/
    // RecoveryClaimed/Completed; C2 settlement is signalled locally during
    // activation or appears only in peer committed truth. Effects: E1 the
    // observer exists before activation; E2 a synchronous local settlement
    // is not lost; E3 peer truth releases the same wait; E4 only committed
    // Awaiting/Ended crosses the boundary.
    //
    // | Rule | Activation | Settlement source | Effect |
    // | O1 | Activated | local during activation | E1+E2+E4 |
    // | O2 | AlreadyActivated | peer committed fallback | E1+E3+E4 |
    // | O3 | RecoveryClaimed | peer committed fallback | E1+E3+E4 |
    // | O4 | Completed | immediate committed read | E1+E3+E4 |
    let rules = [
        (
            "O1",
            SessionRunActivation::Activated,
            Some(RunState::Awaiting),
            None,
        ),
        (
            "O2",
            SessionRunActivation::AlreadyActivated {
                session_activity_epoch: 9,
            },
            None,
            Some(RunState::Awaiting),
        ),
        (
            "O3",
            SessionRunActivation::RecoveryClaimed,
            None,
            Some(RunState::Awaiting),
        ),
        (
            "O4",
            SessionRunActivation::Completed,
            None,
            Some(RunState::Ended(EndCause::NaturalEnd)),
        ),
    ];

    for (rule, activation, local_state, peer_state) in rules {
        let registry = Arc::new(CompletionRegistry::default());
        let run_id = RunId(format!("session-user-{rule}"));
        let activation_registry = registry.clone();
        let activation_run = run_id.clone();
        let expected_truth = peer_state.clone().or_else(|| local_state.clone());
        let state = activate_and_await_session_run::<RunError, _, _, _, _>(
            &registry,
            &run_id,
            None,
            std::time::Duration::from_millis(2),
            move || async move {
                assert_eq!(
                    activation_registry
                        .waiters
                        .lock()
                        .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                        .get(&activation_run.0)
                        .map(Vec::len),
                    Some(1),
                    "{rule}/E1 register before activation"
                );
                if let Some(local_state) = local_state {
                    activation_registry.settled(&activation_run, &local_state);
                }
                Ok(activation)
            },
            move || {
                let expected_truth = expected_truth.clone();
                async move { Ok(expected_truth) }
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{rule}: {error}"));

        assert!(
            matches!(state, RunState::Awaiting | RunState::Ended(_)),
            "{rule}/E4"
        );
        assert!(
            registry
                .waiters
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                .is_empty(),
            "{rule}"
        );
    }
}

#[tokio::test]
async fn dropping_session_user_run_observation_releases_only_its_waiter() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    use awaken_session_execution_contract::SessionRunActivation;

    // Cause/effect graph: C1 an activated Run remains Running in committed
    // truth; C2 its foreground caller is dropped. Effects: E1 the observation
    // remains pending while C1 holds; E2 C2 removes its temporary registration;
    // E3 no Run/Dispatch state is changed. Decision rule D1=C1+C2=>E1+E2+E3.
    let registry = Arc::new(CompletionRegistry::default());
    let run_id = RunId("session-user-drop".into());
    let mut observation = Box::pin(activate_and_await_session_run::<RunError, _, _, _, _>(
        &registry,
        &run_id,
        None,
        std::time::Duration::from_millis(2),
        || std::future::ready(Ok(SessionRunActivation::Activated)),
        || std::future::ready(Ok(None)),
    ));

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(12), observation.as_mut())
            .await
            .is_err(),
        "D1/E1"
    );
    assert_eq!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .get(&run_id.0)
            .map(Vec::len),
        Some(1),
        "D1/E1"
    );
    drop(observation);
    assert!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .is_empty(),
        "D1/E2"
    );
}

#[tokio::test]
async fn exact_run_replay_observers_do_not_replace_each_other() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 two foreground callers observe the same stable
    // Run; C2 either caller drops or the Run settles. Effects: E1 both share
    // the one registry Run slot; E2 one drop preserves the peer; E3 settlement
    // wakes every remaining observer. Rules M1=C1=>E1, M2=C1+drop=>E2,
    // M3=C1+settle=>E3.
    let registry = Arc::new(CompletionRegistry::default());
    let run_id = RunId("session-user-replay".into());
    let (first, first_guard) = registry.register(&run_id, None).expect("healthy registry");
    let (second, second_guard) = registry.register(&run_id, None).expect("healthy registry");
    assert_eq!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .get(&run_id.0)
            .map(Vec::len),
        Some(2),
        "M1/E1"
    );

    drop(first_guard);
    assert!(first.await.is_err(), "M2 dropped observer closes");
    assert_eq!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .get(&run_id.0)
            .map(Vec::len),
        Some(1),
        "M2/E2"
    );
    registry.settled(&run_id, &RunState::Awaiting);
    assert!(matches!(second.await, Ok(RunState::Awaiting)), "M3/E3");
    drop(second_guard);
    assert!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .is_empty(),
        "M3/E3"
    );
}

#[tokio::test]
async fn durable_foreground_registry_routes_only_the_current_runs_live_stream() {
    // Cause/effect graph: C1 a foreground durable Run registers a live sink;
    // C2 an emitted StreamEvent carries the exact vs another Run id; C3 the
    // registration remains active vs settles/drops. Effects: E1 exact active
    // progress reaches that connection; E2 foreign progress is ignored; E3
    // settlement notifies; its observer then drops the guard, removing the
    // waiter and live route; E4 later progress is ignored. Constraint: StreamEvent remains
    // best-effort observation and never changes committed RunState.
    //
    // | Rule | registered | event id | phase | Effects |
    // | R1 | yes | exact | active | E1 |
    // | R2 | yes | foreign | active | E2 |
    // | R3 | yes | exact | guard dropped after notification | E3+E4 |
    // | R4 | no/dropped | any | any | E2 |
    let registry = Arc::new(CompletionRegistry::default());
    let downstream = Arc::new(MemoryStreamSink::new());
    let run = RunId("run-live".into());
    let (settled, guard) = registry
        .register(&run, Some(downstream.clone() as Arc<dyn StreamSink>))
        .expect("healthy registry");

    registry
        .send(text_event("run-foreign", "ignored"))
        .await
        .unwrap();
    assert!(downstream.events().is_empty(), "R2");

    registry
        .send(text_event("run-live", "forwarded"))
        .await
        .unwrap();
    assert_eq!(
        downstream.events(),
        vec![text_event("run-live", "forwarded")],
        "R1"
    );

    registry.settled(&run, &RunState::Awaiting);
    assert!(matches!(settled.await, Ok(RunState::Awaiting)), "R3 state");
    drop(guard);
    registry
        .send(text_event("run-live", "too-late"))
        .await
        .unwrap();
    assert_eq!(downstream.events().len(), 1, "R3/R4 route removed");
}

#[tokio::test(start_paused = true)]
async fn an_older_step_notification_cannot_end_the_resumed_observers_live_route() {
    // C1 peer truth concluded an earlier step before its local notification;
    // C2 the reply registered the same Run and committed truth is not advanced;
    // C3 that old Awaiting notification arrives; C4 resumed live progress;
    // C5 the resumed step commits and its observer drops its guard.
    // E1 C2+C3 keeps the canonical observer pending; E2 C4 reaches the SAME
    // registration despite its consumed one-shot; E3 foreign Run progress is
    // ignored; E4 C5 ends the route. R1=C1+C2+C3=>E1,
    // R2=R1+C4=>E2+E3, R3=R2+C5=>E4. No second live-route map is admitted.
    use awaken_agent_contract::agent::run::EndCause;
    let registry = Arc::new(CompletionRegistry::default());
    let run = RunId("resumed-live".into());
    let sink = Arc::new(MemoryStreamSink::new());
    let (receiver, guard) = registry
        .register(&run, Some(sink.clone() as Arc<dyn StreamSink>))
        .unwrap();
    let truth = Arc::new(std::sync::Mutex::new(None));
    let observed = truth.clone();
    let future = await_completion_state::<RunError, _, _>(
        receiver,
        std::time::Duration::from_millis(2),
        move || std::future::ready(Ok(observed.lock().unwrap().clone())),
    );
    tokio::pin!(future);
    registry.settled(&run, &RunState::Awaiting);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(5), &mut future)
            .await
            .is_err(),
        "R1 old notification is not completion truth"
    );
    registry
        .send(text_event("resumed-live", "resumed progress"))
        .await
        .unwrap();
    registry
        .send(text_event("foreign", "ignored"))
        .await
        .unwrap();
    assert_eq!(
        sink.events(),
        vec![text_event("resumed-live", "resumed progress")],
        "R2"
    );
    *truth.lock().unwrap() = Some(RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(future.await.unwrap(), RunState::Ended(EndCause::NaturalEnd));
    drop(guard);
    registry
        .send(text_event("resumed-live", "too late"))
        .await
        .unwrap();
    assert_eq!(sink.events().len(), 1, "R3 route ended with its observer");
    assert!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .is_empty(),
        "R3 no retained registration"
    );
}

#[tokio::test]
async fn a_late_old_guard_preserves_a_new_registration_of_the_same_run() {
    // G1 an earlier step was notified but its guard drops late; G2 a new
    // observer of the SAME Run owns a different registration id. E1 dropping
    // G1 preserves G2's live sink and notification; E2 dropping G2 removes it.
    // Rules G1+G2=>E1; G1+G2+drop(G2)=>E2. The id scopes removal, never Run id alone.
    let registry = Arc::new(CompletionRegistry::default());
    let run = RunId("late-guard".into());
    let (old, old_guard) = registry.register(&run, None).unwrap();
    registry.settled(&run, &RunState::Awaiting);
    assert_eq!(old.await.unwrap(), RunState::Awaiting);
    let sink = Arc::new(MemoryStreamSink::new());
    let (new, new_guard) = registry
        .register(&run, Some(sink.clone() as Arc<dyn StreamSink>))
        .unwrap();
    drop(old_guard);
    registry
        .send(text_event("late-guard", "new observer"))
        .await
        .unwrap();
    registry.settled(&run, &RunState::Awaiting);
    assert_eq!(
        new.await.unwrap(),
        RunState::Awaiting,
        "E1 new sender retained"
    );
    assert_eq!(
        sink.events(),
        vec![text_event("late-guard", "new observer")],
        "E1"
    );
    drop(new_guard);
    assert!(
        registry
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| panic!("{poisoned}"))
            .is_empty(),
        "E2"
    );
}

/// Live observation cause/effect table: C1 a background Run has no
/// foreground waiter; C2 its neutral event carries child Thread identity; C3
/// an event omits Thread identity. E1 C1+C2 publishes once on the existing
/// child Hub channel; E2 the primary/foreign channel sees nothing; E3 C3 is
/// not guessed or published. Rules H1=C1+C2=>E1+E2, H2=C1+C3=>E3. This
/// proves observation does not depend on a second background-run registry.
/// Constraint/Invariant: exact Thread identity selects the existing Hub
/// channel; absence never falls back to another Thread. Decision rule: H1 and
/// H2 cover exact and absent Thread identity.
#[tokio::test]
async fn background_live_progress_uses_the_exact_thread_hub_channel() {
    let hub = Arc::new(crate::ThreadEventHub::new());
    let registry = CompletionRegistry::new(hub.clone());
    let mut child = hub.subscribe("child-thread").expect("healthy registry");
    let mut primary = hub.subscribe("session-root").expect("healthy registry");
    let event = StreamObservation::assistant_delta(
        RunId("background-run".into()),
        ThreadId("child-thread".into()),
        0,
        0,
        AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
    );

    registry.send_observation(event.clone()).await.unwrap();
    assert!(
        matches!(
            child.try_recv(),
            Ok(crate::ThreadEvent::Live(observed)) if observed == event
        ),
        "H1/E1"
    );
    assert!(primary.try_recv().is_err(), "H1/E2");

    registry
        .send(StreamEvent {
            run_id: RunId("background-run".into()),
            kind: AgentEvent::Delta(Delta::TextDelta {
                delta: "ambiguous".into(),
            }),
        })
        .await
        .unwrap();
    assert!(child.try_recv().is_err(), "H2/E3");
}

#[tokio::test(start_paused = true)]
async fn a_live_run_outlives_foreground_reconciliation_without_a_transport_timeout() {
    // Virtual time proves repeated intervals independently of native timer
    // granularity and scheduler load; the completion signal stays real.
    // Durable foreground completion cause/effect table:
    // C1=the local completion sender remains open; C2=committed Run truth is
    // still Running (`read_settled -> None`); C3=multiple reconciliation
    // intervals pass; C4=the runtime later commits/sends Awaiting. Effects:
    // E1=the foreground waiter remains pending through C3; E2=no transport
    // timeout invents Error/Ended; E3=C4 alone releases the waiter with the
    // exact authoritative state. Rule F1=C1+C2+C3=>E1+E2;
    // F2=F1+C4=>E3. Caller cancellation is covered by the existing guard-drop
    // test and is intentionally independent of Run terminal authority.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = reads.clone();
    let truth = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed_truth = truth.clone();
    let future = await_completion_state::<RunError, _, _>(
        receiver,
        std::time::Duration::from_millis(2),
        move || {
            observed_reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(observed_truth
                .load(std::sync::atomic::Ordering::SeqCst)
                .then_some(RunState::Awaiting)))
        },
    );
    tokio::pin!(future);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(15), &mut future)
            .await
            .is_err(),
        "F1/F2: live committed truth must keep the foreground wait open"
    );
    assert!(
        reads.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "F1: reconciliation observed live truth repeatedly"
    );

    truth.store(true, std::sync::atomic::Ordering::SeqCst);
    sender
        .send(RunState::Awaiting)
        .expect("waiter remains live");
    let state = tokio::time::timeout(std::time::Duration::from_millis(100), &mut future)
        .await
        .expect("authoritative settlement wakes promptly")
        .expect("settlement succeeds");
    assert!(matches!(state, RunState::Awaiting), "F2");
}

#[tokio::test(start_paused = true)]
async fn a_resumed_observer_concludes_only_from_committed_truth_past_its_answer() {
    // Resumed-step observation cause/effect table: C1 the local signal carries
    // the settlement of the step the reply answered while committed truth still
    // shows that answer (`read_settled -> None`); C2 committed truth later
    // advances; C3 the signal arrives when committed truth has already advanced;
    // C4 the signal closes while committed truth still shows the answer.
    // Effects: E1 the observer stays pending through C1 and returns C2's
    // committed state, never the signal's; E2 C3 returns that committed state at
    // the signal; E3 C4 refuses with CompletionSignalClosed.
    // | Rule | Causes | Effects |
    // | R1   | C1+C2  | E1      |
    // | R2   | C3     | E2      |
    // | R3   | C4     | E3      |
    use awaken_agent_contract::agent::run::EndCause;
    let interval = std::time::Duration::from_millis(2);
    let observer = |truth: Arc<std::sync::Mutex<Option<RunState>>>,
                    receiver: tokio::sync::oneshot::Receiver<RunState>| {
        await_completion_state::<CompletionSignalClosed, _, _>(receiver, interval, move || {
            std::future::ready(Ok(truth
                .lock()
                .unwrap_or_else(|poisoned| panic!("{poisoned}"))
                .clone()))
        })
    };

    let truth = Arc::new(std::sync::Mutex::new(None));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let future = observer(truth.clone(), receiver);
    tokio::pin!(future);
    sender
        .send(RunState::Awaiting)
        .expect("R1 the observer is registered");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(15), &mut future)
            .await
            .is_err(),
        "R1 the answered step's settlement must not conclude the resumed step"
    );
    *truth.lock().unwrap_or_else(|poisoned| panic!("{poisoned}")) =
        Some(RunState::Ended(EndCause::NaturalEnd));
    let state = tokio::time::timeout(std::time::Duration::from_millis(100), &mut future)
        .await
        .expect("R1 committed truth releases the observer")
        .expect("R1 settlement succeeds");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd), "R1");

    let truth = Arc::new(std::sync::Mutex::new(Some(RunState::Awaiting)));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let future = observer(truth, receiver);
    tokio::pin!(future);
    sender
        .send(RunState::Ended(EndCause::NaturalEnd))
        .expect("R2 the observer is registered");
    let state = tokio::time::timeout(std::time::Duration::from_millis(1), &mut future)
        .await
        .expect("R2 the signal wakes the observer at once")
        .expect("R2 settlement succeeds");
    assert_eq!(
        state,
        RunState::Awaiting,
        "R2 committed truth, not the signal"
    );

    let (sender, receiver) = tokio::sync::oneshot::channel::<RunState>();
    drop(sender);
    let refused = observer(Arc::new(std::sync::Mutex::new(None)), receiver).await;
    assert_eq!(refused, Err(CompletionSignalClosed), "R3");
}

/// Rows W2 and W4 of [`CompletionRegistry`]: a poisoned registry refuses a
/// new observation before anything is enqueued, and neither settlement nor
/// live relay panics; committed truth settles the waiters already registered.
#[tokio::test(start_paused = true)]
async fn a_poisoned_completion_registry_refuses_new_observations_and_stops_notifying() {
    let hub = Arc::new(crate::ThreadEventHub::new());
    let registry = Arc::new(CompletionRegistry::new(hub));
    let (mut existing, _guard) = registry.register(&RunId("existing".into()), None).unwrap();
    crate::poison_for_test(&registry.waiters);
    assert!(
        matches!(
            registry.register(&RunId("poisoned-run".into()), None),
            Err(crate::DeliveryLock::CompletionRegistry)
        ),
        "W2"
    );
    registry.settled(&RunId("existing".into()), &RunState::Awaiting);
    assert!(
        matches!(
            existing.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "W4 poison never delivers a local notification"
    );
    registry
        .send_observation(StreamObservation::assistant_delta(
            RunId("poisoned-run".into()),
            ThreadId("poisoned-thread".into()),
            0,
            0,
            AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
        ))
        .await
        .expect("W4 the live relay is best-effort");
    let state = await_completion_state::<RunError, _, _>(
        existing,
        std::time::Duration::from_millis(2),
        || std::future::ready(Ok(Some(RunState::Awaiting))),
    )
    .await
    .unwrap();
    assert_eq!(
        state,
        RunState::Awaiting,
        "W4 existing caller uses committed truth"
    );
}

#[test]
fn committed_completion_wakes_the_single_installed_convergence_owner() {
    // S1 raw SDK/Worker relay without a Session application => settle only.
    // S2 registered foreground => receiver is already settled INSIDE wakeup.
    // S3 no foreground => wake the same convergence owner once. S4 the exact
    // selected relay/wakeup are immutable; the struct's compile-fail cases
    // reject replacement or missing construction input at the public boundary.
    // These are the former executor S1-S4 rules over the same relay owner.
    use std::sync::atomic::{AtomicUsize, Ordering};
    let registry = Arc::new(CompletionRegistry::default());
    let wakes = Arc::new(AtomicUsize::new(0));
    registry.settled(&RunId("completion-unbound".into()), &RunState::Awaiting);
    assert_eq!(wakes.load(Ordering::SeqCst), 0, "S1 settle only");
    let (observer, _guard) = registry
        .register(&RunId("completion-observed".into()), None)
        .expect("healthy relay");
    let observer = Arc::new(std::sync::Mutex::new(Some(observer)));
    let received = observer.clone();
    let observed = wakes.clone();
    let wakeup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        if let Some(mut receiver) = received.lock().unwrap().take() {
            assert!(
                matches!(receiver.try_recv(), Ok(RunState::Ended(_))),
                "S2/E1 the relay must settle before convergence wakes"
            );
        }
        observed.fetch_add(1, Ordering::SeqCst);
    });
    let relay: Arc<dyn CompletionSink> = registry;
    let settlement = SettlementWakeup::new(relay.clone(), wakeup.clone());
    assert!(
        Arc::ptr_eq(&settlement.completion, &relay),
        "S4 relay identity"
    );
    assert!(
        Arc::ptr_eq(&settlement.committed_progress, &wakeup),
        "S4 wake identity"
    );
    settlement.settled(
        &RunId("completion-observed".into()),
        &RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd),
    );
    assert!(
        observer.lock().unwrap().is_none(),
        "S2/E1 observed completion"
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 1, "S2/E2");
    settlement.settled(
        &RunId("completion-wakeup".into()),
        &RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd),
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 2, "S3/E2");
}
