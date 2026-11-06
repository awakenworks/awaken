// @formal surface exclude=test_support_crate
// @formal owner crates/platform/dispatch/awaken-dispatch-testkit/src/lib.rs
// @formal reason: Task command identity conformance, called by the original backend suite.
/// Task admission identity causes/effects (the original Memory/SQLite/PG suite):
/// T1 root/child + complete meta-only tenure -> the original activity/admission;
/// T2 same immutable command Live/Completed -> one Run, no new work;
/// T3 any Task/thread/partition/origin/holder/epoch/body change -> conflict,
/// preserving original row/completion; T4 malformed Task-child SessionCommand
/// marker -> reject before replay, never reduce FullDispatch; T5 ordinary child
/// remains complete canonical identity; T6 child may not use root reservation.
async fn task_attention_identity_survives_live_and_completed(
    store: &dyn ConformanceDispatch,
    ns: &str,
    capabilities: ConformanceCapabilities,
    clock: &dyn ConformanceClock,
) {
    if !capabilities.local_commit_guard {
        return;
    }
    for child in [false, true] {
        let tag = if child { "task-child" } else { "task-root" };
        let parent = thread_id(ns, &format!("{tag}-parent"));
        let thread = if child {
            thread_id(ns, tag)
        } else {
            parent.clone()
        };
        let mut request = dispatch(ns, tag, tag).for_session(parent.clone());
        request.activation.thread_id = thread.clone();
        let partition = if child {
            serde_json::json!({"kind":"session", "session_thread":parent.0})
        } else {
            serde_json::json!({"kind":"thread"})
        };
        // Serde invokes the canonical PrivateTaskInput/TaskTenureRead types;
        // no test-only parser, state machine or alternative fingerprint exists.
        request.private_task_input = Some(
            serde_json::from_value(serde_json::json!({
                "version":1, "observations":[], "tenure": {
                    "thread":thread.0, "partition":partition, "run":format!("{ns}-origin"),
                    "task":format!("{ns}-{tag}"), "holder":"held-worker", "epoch":3,
                }
            }))
            .unwrap(),
        );
        request = request.with_worker_codec_requirements();
        let operation = format!("{ns}-{tag}-operation");
        request = current_session_command_with_operation(request, operation.clone());
        if child {
            request = request
                .with_session_activity_epoch(7)
                .with_session_command_identity();
            assert!(
                store
                    .reserve_session_run(request.clone(), 60_000)
                    .await
                    .is_err(),
                "T6 root reservation stays root-only"
            );
            store
                .enqueue_session_child(request.clone(), SessionChildAdmission::new(100, Vec::new()))
                .await
                .unwrap();
        } else {
            assert_eq!(
                store
                    .reserve_session_run(request.clone(), 60_000)
                    .await
                    .unwrap(),
                SessionRunReservationOutcome::Reserved,
                "T1"
            );
            store
                .activate_session_run_reservation(request.run_id(), &parent, 7)
                .await
                .unwrap();
            request = request.with_session_command_identity();
        }
        for completed in [false, true] {
            if completed {
                clock.set(500_000);
                let claimed = store
                    .claim_run(
                        request.run_id(),
                        "task-owner",
                        LEASE_MS,
                        &Default::default(),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    store
                        .settle(
                            request.run_id(),
                            claimed.lease.epoch(),
                            DispatchOutcome::Done,
                            &[]
                        )
                        .await
                        .unwrap(),
                    SettleOutcome::Applied
                );
            }
            let before = store
                .observe_session_run_admission(request.run_id())
                .await
                .unwrap();
            let run_count = store.list_dispatches().await.unwrap().len();
            let completion_count = store
                .completion_events_after(0, usize::MAX)
                .await
                .unwrap()
                .len();
            if child {
                store
                    .enqueue_session_child(
                        request.clone(),
                        SessionChildAdmission::new(1, Vec::new()),
                    )
                    .await
                    .unwrap();
            } else {
                assert_eq!(
                    store
                        .reserve_session_run(request.clone(), 60_000)
                        .await
                        .unwrap(),
                    if completed {
                        SessionRunReservationOutcome::Completed
                    } else {
                        SessionRunReservationOutcome::AlreadyActivated {
                            session_activity_epoch: 7,
                        }
                    },
                    "T2"
                );
            }
            for axis in [
                "task",
                "thread",
                "run",
                "holder",
                "epoch",
                "partition",
                "body",
            ] {
                let mut changed = request.clone();
                let mut wire =
                    serde_json::to_value(changed.private_task_input.as_ref().unwrap()).unwrap();
                if axis == "epoch" {
                    wire["tenure"][axis] = serde_json::json!(4);
                } else if axis == "partition" {
                    wire["tenure"][axis] =
                        serde_json::json!({"kind":"session", "session_thread":"foreign-session"});
                } else if axis == "body" {
                    wire["observations"] = serde_json::json!([{"version":1, "changed":true}]);
                } else {
                    wire["tenure"][axis] = serde_json::json!(format!("changed-{axis}"));
                }
                changed.private_task_input = Some(serde_json::from_value(wire).unwrap());
                changed = current_session_command_with_operation(changed, operation.clone());
                let failed = if child {
                    store
                        .enqueue_session_child(changed, SessionChildAdmission::new(100, Vec::new()))
                        .await
                        .is_err()
                } else {
                    store.reserve_session_run(changed, 60_000).await.unwrap()
                        == SessionRunReservationOutcome::Conflict
                };
                assert!(failed, "T3 {axis} child={child} completed={completed}");
                assert_eq!(
                    store
                        .observe_session_run_admission(request.run_id())
                        .await
                        .unwrap(),
                    before,
                    "T3 original exact fact"
                );
            }
            if child {
                for missing in 0..4 {
                    let mut invalid = request.clone();
                    match missing {
                        0 => invalid.private_task_input = None,
                        1 => invalid.session_activity_epoch = None,
                        2 => invalid.session_command_fingerprint = None,
                        _ => invalid.session_thread_id = Some(thread_id(ns, "foreign")),
                    }
                    assert!(
                        store
                            .enqueue_session_child(
                                invalid,
                                SessionChildAdmission::new(100, Vec::new())
                            )
                            .await
                            .is_err(),
                        "T4 missing={missing}"
                    );
                }
            }
            assert_eq!(
                store.list_dispatches().await.unwrap().len(),
                run_count,
                "T2/T3 no new live work"
            );
            assert_eq!(
                store
                    .completion_events_after(0, usize::MAX)
                    .await
                    .unwrap()
                    .len(),
                completion_count,
                "T2/T3 no new completion"
            );
        }
    }
}
