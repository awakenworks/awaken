use super::*;

#[derive(Clone, Copy)]
enum Phase {
    Events,
    Resources,
}

#[tokio::test]
async fn scan_interests_skip_only_phases_absent_from_the_observed_prefix() {
    // Cause/effect graph: C1 the atomic repository scan classified Resource,
    // Event, Environment-continuation and local-realization work absent; C2 the root
    // remains available; C3 a later mutation may enqueue work after this
    // prefix. Effects: E1 all four phase schedulers perform zero duplicate
    // root reads; E2 no effect or settlement is reported; E3 C3 is deliberately
    // outside this candidate and is retained by the supervisor wake/deadline.
    // Decision table: H1=(!resource,!event,!continuation,!local)=>E1,E2; each true-hint
    // row is owned by the existing phase success/failure tests, which require a
    // fresh root before effects. A false hint can skip scheduling only; it
    // carries no payload, receipt or durable cursor.
    let durable =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(durable.as_ref(), persisted("hint-skip", true, "idle")).await;
    let observed = Arc::new(FaultingSessionRepository::new(durable));
    let app = application(
        observed.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let candidates = SessionRecoveryCandidates {
        sessions: vec![SessionRecoveryCandidate {
            workspace_id: "test".into(),
            session_id: "hint-skip".into(),
            interests: SessionRecoveryInterests {
                resources: false,
                event_batches: false,
                environment_continuation: false,
                local_realization: false,
            },
        }],
        quarantined: Vec::new(),
    };

    let resources = app.reconcile_resource_activations_from(&candidates).await;
    assert!(resources.settled.is_empty(), "H1/E2 resources");
    assert!(resources.failures.is_empty(), "H1/E2 resources");
    let events = app.reconcile_event_batches_from(&candidates).await;
    assert_eq!((events.pending, events.settled), (0, 0), "H1/E2 events");
    assert_eq!(
        app.reconcile_environment_continuations_from(&candidates, 0)
            .await,
        0,
        "H1/E2 continuation"
    );
    let realizations = app.reconcile_session_realizations_from(&candidates).await;
    assert!(realizations.settled.is_empty(), "H1/E2 realization");
    assert!(realizations.failures.is_empty(), "H1/E2 realization");
    assert!(observed.observed_gets.lock().unwrap().is_empty(), "H1/E1");
}

#[test]
fn resource_reference_audit_is_periodic_while_new_work_is_immediate() {
    // Causes: C1 a stable active File reference exists; C2 recovery is an
    // external wake or scheduled audit; C3 a new Resource generation is pending.
    // Effects: R1 C1+external => no duplicate reference read; R2 C1+scheduled =>
    // audit the existing retention edge; R3 C3+either => reconcile immediately.
    // The hint carries no Resource payload or receipt. Every admitted phase still
    // reloads the root and uses the existing reference/effect authority.
    let mut stable = persisted("resource-interest", true, "idle");
    stable.resources =
        awaken_session_contract::SessionResourceState::from_active(file_resources("stable"));
    assert!(!stable.needs_resource_reconciliation(), "R1 precondition");
    assert!(stable.resources.has_references(), "R1 precondition");
    assert!(
        !SessionRecoveryInterests::from_session(&stable, false).resources,
        "R1"
    );
    assert!(
        SessionRecoveryInterests::from_session(&stable, true).resources,
        "R2"
    );
    stable
        .resources
        .prepare("resource-interest", file_resources("pending"))
        .unwrap();
    assert!(stable.needs_resource_reconciliation(), "R3 precondition");
    assert!(
        SessionRecoveryInterests::from_session(&stable, false).resources,
        "R3"
    );
}

impl Phase {
    // Normalize only the assertions, not the production reports or decisions.
    async fn reconcile(
        self,
        app: &SessionApplication,
        candidates: &SessionRecoveryCandidates,
    ) -> (usize, usize, usize, Vec<(String, String)>) {
        match self {
            Self::Events => {
                let report = app.reconcile_event_batches_from(candidates).await;
                (
                    report.pending,
                    report.settled,
                    report.quarantined,
                    report.failures,
                )
            }
            Self::Resources => {
                let report = app.reconcile_resource_activations_from(candidates).await;
                (
                    report.pending,
                    report.settled.len(),
                    report.quarantined.len(),
                    report
                        .failures
                        .into_iter()
                        .map(|failure| (failure.session_id, failure.message))
                        .collect(),
                )
            }
        }
    }
}

#[tokio::test]
async fn recovery_sweeps_isolate_slow_sessions_with_a_bounded_owned_sweep() {
    // Cause/effect graph: C1 one/four independent root reads are held; C2
    // another keyset candidate is ready; C3 a held read completes; C4 the
    // caller is cancelled. R1 one held + five ready => every ready root is
    // inspected before the held root completes. R2 four held => no fifth
    // read until C3; then that freed slot admits the remaining roots. R3
    // cancellation drops all held futures; releasing their gates afterwards
    // cannot resume recovery. Each keyset identity occurs once; the original
    // per-Session FIFO/CAS and effect tests own intra-root ordering/replay.
    // These are scheduling barriers, not machine-speed assertions.
    // The same decision table runs through both actual phase entrypoints.
    for (phase, held) in [Phase::Events, Phase::Resources]
        .into_iter()
        .flat_map(|phase| [1, 4].map(|held| (phase, held)))
    {
        let durable = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        let ids = (0..6).map(|n| format!("sweep-{n}")).collect::<Vec<_>>();
        for id in &ids {
            create(durable.as_ref(), persisted(id, true, "idle")).await;
        }
        let observed = Arc::new(FaultingSessionRepository::new(durable));
        let gates = (0..held)
            .map(|index| {
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                observed
                    .paused_gets
                    .lock()
                    .unwrap()
                    .insert(ids[index].clone(), gate.clone());
                gate
            })
            .collect::<Vec<_>>();
        let app = Arc::new(application(
            observed.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        ));
        let candidates = SessionRecoveryCandidates {
            sessions: ids
                .iter()
                .map(|id| SessionRecoveryCandidate {
                    workspace_id: "test".into(),
                    session_id: id.clone(),
                    interests: SessionRecoveryInterests::default(),
                })
                .collect(),
            quarantined: Vec::new(),
        };
        let task = tokio::spawn(async move { phase.reconcile(&app, &candidates).await });
        let wait_for_read = |id: String| {
            let observed = observed.clone();
            async move {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    loop {
                        if observed.observed_gets.lock().unwrap().contains(&id) {
                            break;
                        }
                        observed.get_entered.notified().await;
                    }
                })
                .await
                .expect("an independent root must reach its read barrier");
            }
        };
        wait_for_read(ids[held - 1].clone()).await;
        if held == 4 {
            assert_eq!(observed.observed_gets.lock().unwrap().len(), 4, "R2 bound");
            gates[0].add_permits(1);
        }
        wait_for_read(ids[5].clone()).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if observed.completed_gets.lock().unwrap().contains(&ids[5]) {
                    break;
                }
                observed.get_entered.notified().await;
            }
        })
        .await
        .expect("R1/R2 ready root must finish its read");
        let before = observed.completed_gets.lock().unwrap().clone();
        task.abort();
        assert!(
            task.await.unwrap_err().is_cancelled(),
            "R3 joined cancellation"
        );
        for gate in gates {
            gate.add_permits(1);
        }
        tokio::task::yield_now().await;
        assert_eq!(
            *observed.completed_gets.lock().unwrap(),
            before,
            "R3 no detached reads"
        );
    }
}

#[tokio::test]
async fn recovery_sweeps_retain_read_failures_and_missing_root_semantics() {
    // R4 independent candidates: current root with no queued Event or local
    // Resource work => skip;
    // deleted root => skip; repository unavailable => retain exact failure;
    // quarantined scan entry => retain count without executing it. Concurrent
    // completion must keep diagnostics in original candidate order. No failed
    // sibling may stop later candidates or manufacture a settlement.
    for phase in [Phase::Events, Phase::Resources] {
        let durable = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
        );
        create(durable.as_ref(), persisted("ready", true, "idle")).await;
        let observed = Arc::new(FaultingSessionRepository::new(durable));
        observed
            .failed_gets
            .lock()
            .unwrap()
            .extend(["error-z".into(), "error-a".into()]);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        observed
            .paused_gets
            .lock()
            .unwrap()
            .insert("error-z".into(), gate.clone());
        let app = Arc::new(application(
            observed.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        ));
        let candidates = SessionRecoveryCandidates {
            sessions: ["error-z", "deleted", "ready", "error-a"]
                .into_iter()
                .map(|id| SessionRecoveryCandidate {
                    workspace_id: "test".into(),
                    session_id: id.into(),
                    interests: SessionRecoveryInterests::default(),
                })
                .collect(),
            quarantined: vec![SessionRecoveryQuarantine {
                session_id: "corrupt".into(),
                reason: "fixture".into(),
            }],
        };
        let task = tokio::spawn(async move { phase.reconcile(&app, &candidates).await });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if observed
                    .observed_gets
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|id| id == "error-a")
                {
                    break;
                }
                observed.get_entered.notified().await;
            }
        })
        .await
        .expect("R4 later failure must not wait for earlier failure");
        gate.add_permits(1);
        let (pending, settled, quarantined, failures) = task.await.expect("R4 sweep must join");
        // Resource reports count every selected root; Event reports count only
        // roots with queued Event work. Preserve both existing meanings.
        assert_eq!(
            pending,
            if matches!(phase, Phase::Resources) {
                4
            } else {
                0
            }
        );
        assert_eq!(settled, 0);
        assert_eq!(quarantined, 1);
        assert_eq!(
            failures
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["error-z", "error-a"]
        );
        assert!(
            failures
                .iter()
                .all(|(_, error)| error.contains("injected Session read failure"))
        );
        assert_eq!(observed.observed_gets.lock().unwrap().len(), 4);
    }
}
