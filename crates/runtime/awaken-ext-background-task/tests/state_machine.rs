use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_background_task::{
    BackgroundInvocation, BackgroundTask, BackgroundTaskEnd, BackgroundTaskError, BackgroundTaskId,
    BackgroundTaskLifecycle, BackgroundTaskOrigin, BackgroundWait, TaskClaim, TaskExecutionPolicy,
    TaskFence,
};
use awaken_runtime_contract::tool::{
    ToolCall, ToolConcurrency, ToolRecoveryMode, ToolRecoveryPolicy, ToolTaskHandle,
};

fn task() -> BackgroundTask {
    BackgroundTask::requested(
        BackgroundTaskId::new("task-1").expect("fixture task id is valid"),
        BackgroundTaskOrigin {
            thread_id: ThreadId("thread-1".into()),
            run_id: RunId("run-1".into()),
            operation_id: "operation-1".into(),
        },
        BackgroundInvocation {
            call: ToolCall {
                call_id: "call-1".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({"command":"sleep 1"}),
            },
        },
    )
}

fn replay_policy() -> TaskExecutionPolicy {
    TaskExecutionPolicy {
        recovery: ToolRecoveryPolicy::replay_safe(),
        concurrency: ToolConcurrency::Parallel,
    }
}

fn execution_task(phase: usize) -> BackgroundTask {
    let mut task = task();
    if phase == 0 {
        return task;
    }
    let fence = task.start("worker", 0, 10, replay_policy()).unwrap();
    if matches!(phase, 2 | 3) {
        task.wait(
            &fence,
            BackgroundWait::Remote(ToolTaskHandle {
                owner: "adapter".into(),
                binding: "binding".into(),
                task_id: "remote".into(),
                poll_interval_ms: Some(1),
            }),
        )
        .unwrap();
    }
    if matches!(phase, 5 | 6) {
        task.wait(&fence, BackgroundWait::ExternalInput).unwrap();
    }
    if matches!(phase, 3 | 6) {
        task.request_cancel().unwrap();
    }
    if phase == 4 {
        task.finish(&fence, BackgroundTaskEnd::Cancelled).unwrap();
    }
    task
}

#[test]
fn execution_admission_is_phase_monotone_and_lease_bounded() {
    // Causes: expected/current Requested, Running, remote Waiting/Cancelling,
    // Ended, local Waiting/Cancelling; clock before/equal/after deadline/MAX.
    // R1 only Running->Running starts; R2 remote Waiting->Waiting/Cancelling
    // observes the same task; R3 remote Cancelling->Cancelling continues cancel;
    // R4 all other pairs or now>=deadline reject. 7*7*4 = 196 rules. Rejection
    // and admission are read-only; a later heartbeat can renew the same claim.
    for expected_phase in 0..7 {
        for current_phase in 0..7 {
            for now in [9, 10, 11, u64::MAX] {
                let expected = execution_task(expected_phase);
                let current = execution_task(current_phase);
                let allowed_phase = (expected_phase == 1 && current_phase == 1)
                    || (expected_phase == 2 && matches!(current_phase, 2 | 3))
                    || (expected_phase == 3 && current_phase == 3);
                let before = current.clone();
                assert_eq!(
                    current.admits_execution_of(&expected, now),
                    now < 10 && allowed_phase,
                    "expected={expected_phase} current={current_phase} now={now}"
                );
                assert_eq!(current, before);
            }
        }
    }
    let expected = execution_task(2);
    let mut renewed = expected.clone();
    renewed
        .heartbeat(&expected.attempt().unwrap().fence(), 9, 10)
        .unwrap();
    assert!(renewed.admits_execution_of(&expected, 10));
}

#[test]
fn execution_admission_preserves_frozen_identity_and_remote_target() {
    // Each identity axis is an independent cause: task/thread/run/operation,
    // call/tool/arguments, worker/epoch, recovery/concurrency, remote
    // owner/binding/task. R1 any one mismatch -> reject; R2 only recommended poll
    // delay changes -> admit (not remote identity). R3 unchanged -> admit.
    // Mutations target both expected and current snapshots to expose asymmetric
    // comparisons; neither comparison may mutate committed state.
    for mutate_expected in [false, true] {
        for axis in 0..16 {
            let mut expected = execution_task(2);
            let mut current = expected.clone();
            let target = if mutate_expected {
                &mut expected
            } else {
                &mut current
            };
            match axis {
                0 => target.id = BackgroundTaskId::new("other").unwrap(),
                1 => target.origin.thread_id = ThreadId("other".into()),
                2 => target.origin.run_id = RunId("other".into()),
                3 => target.origin.operation_id = "other".into(),
                4 => target.invocation.call.call_id = "other".into(),
                5 => target.invocation.call.tool_id = "other".into(),
                6 => target.invocation.call.arguments = serde_json::json!({"command":"other"}),
                _ => {
                    let BackgroundTaskLifecycle::Waiting {
                        attempt,
                        wait: BackgroundWait::Remote(handle),
                    } = &mut target.lifecycle
                    else {
                        unreachable!()
                    };
                    match axis {
                        7 => attempt.worker_id = "other".into(),
                        8 => attempt.epoch = 2,
                        9 => attempt.policy.recovery = ToolRecoveryPolicy::default(),
                        10 => attempt.policy.concurrency = ToolConcurrency::Serial,
                        11 => handle.owner = "other".into(),
                        12 => handle.binding = "other".into(),
                        13 => handle.task_id = "other".into(),
                        14 => handle.poll_interval_ms = Some(99),
                        _ => {}
                    }
                }
            }
            assert_eq!(
                current.admits_execution_of(&expected, 9),
                axis >= 14,
                "axis={axis} mutate_expected={mutate_expected}"
            );
        }
    }
}

#[test]
fn cancellation_preserves_unknown_outcomes_and_rejected_finishes_are_atomic() {
    // C1 cancel intent; C2 observed Completed/Failed/Cancelled/Indeterminate;
    // C3 exact owner+epoch; C4 revision has capacity; C5 finish/heartbeat;
    // C6 heartbeat lease is valid/zero/overflowing. R1 !C3 + either C5 + any
    // C6 => stale, full aggregate stutter; R2 C3&&!C4 => overflow, stutter;
    // R3 C3&&C4&&unknown => exact unknown;
    // R4 C3&&C4&&known&&C1 => Cancelled; R5 known&&!C1 => exact observation.
    // Cancel intent is not evidence of remote termination. This crosses Running
    // and Cancelling with both independent fence mismatches; the existing remote
    // wait table owns the same heartbeat/finish checks for Waiting.
    for cancelling in [false, true] {
        for end in [
            BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
            BackgroundTaskEnd::Failed {
                message: "failed".into(),
            },
            BackgroundTaskEnd::Cancelled,
            BackgroundTaskEnd::Indeterminate {
                message: "cancel response lost".into(),
            },
        ] {
            for mismatch in 0..3 {
                for exhausted in [false, true] {
                    let mut task = task();
                    let mut fence = task.start("worker", 0, 10, replay_policy()).unwrap();
                    if cancelling {
                        task.request_cancel().unwrap();
                    }
                    if mismatch == 1 {
                        fence.worker_id = "other".into();
                    }
                    if mismatch == 2 {
                        fence.epoch += 1;
                    }
                    if exhausted {
                        task.revision = u64::MAX;
                    }
                    let before = task.clone();
                    if mismatch != 0 {
                        for (now_ms, lease_ms) in [(0, 1), (0, 0), (u64::MAX, 1)] {
                            assert_eq!(
                                task.heartbeat(&fence, now_ms, lease_ms),
                                Err(BackgroundTaskError::StaleFence),
                                "R1 stale heartbeat"
                            );
                            assert_eq!(task, before, "R1 stale heartbeat stutter");
                        }
                    }
                    let result = task.finish(&fence, end.clone());
                    if mismatch != 0 {
                        assert_eq!(result, Err(BackgroundTaskError::StaleFence), "R1");
                        assert_eq!(task, before, "R1");
                    } else if exhausted {
                        assert_eq!(result, Err(BackgroundTaskError::RevisionOverflow), "R2");
                        assert_eq!(task, before, "R2");
                    } else {
                        assert_eq!(result, Ok(()));
                        let expected = if cancelling
                            && !matches!(end, BackgroundTaskEnd::Indeterminate { .. })
                        {
                            BackgroundTaskEnd::Cancelled
                        } else {
                            end.clone()
                        };
                        assert_eq!(
                            task.lifecycle,
                            BackgroundTaskLifecycle::Ended { end: expected },
                            "R3-R5"
                        );
                        assert_eq!(task.revision, before.revision + 1);
                        assert!(task.same_request_as(&before));
                    }
                }
            }
        }
    }
}

#[test]
fn reclaim_boundary_decision_table_preserves_rejection_and_uncertainty() {
    // Cause/effect decision table: phase/handle/recovery determine safe
    // recovery; expiry gates ownership transfer; budget determines acquire/end;
    // candidate clock and revision capacity gate mutation. R1 invalid lease ->
    // InvalidClaim; R2 clock overflow -> ClockOverflow; R3 Requested/Ended ->
    // InvalidTransition/Terminal; R4 live but not expired -> Busy; R5 admitted
    // revision exhausted -> RevisionOverflow; R6 unsafe recovery or exhausted
    // remote observation -> Indeterminate; R7 exhausted replayable local ->
    // Failed; R8 otherwise acquire exact epoch+1 with unchanged phase/handle/
    // policy. Every R1-R5 is a complete aggregate stutter; every R6-R8 applies
    // exactly one production reclaim plan. This finite matrix crosses semantic
    // classes and boundaries; the paired Kani harness exhausts scalar domains.
    for phase in 0..3 {
        for remote in [false, true] {
            if phase == 0 && remote {
                continue;
            }
            for mode in [
                ToolRecoveryMode::NeverReplay,
                ToolRecoveryMode::ReplaySafe,
                ToolRecoveryMode::Idempotent,
                ToolRecoveryMode::DurableRequest,
            ] {
                for epoch in [1, 2, u64::MAX] {
                    for revision in [10, u64::MAX] {
                        for (now, lease) in [(9, 1), (10, 1), (11, 1), (10, 0), (u64::MAX, 1)] {
                            let mut task = task();
                            let fence = task
                                .start(
                                    "a",
                                    0,
                                    10,
                                    TaskExecutionPolicy {
                                        recovery: ToolRecoveryPolicy::try_new(mode, 2).unwrap(),
                                        concurrency: ToolConcurrency::Parallel,
                                    },
                                )
                                .unwrap();
                            if phase != 0 {
                                let wait = if remote {
                                    BackgroundWait::Remote(ToolTaskHandle {
                                        owner: "mcp".into(),
                                        binding: "binding".into(),
                                        task_id: "remote".into(),
                                        poll_interval_ms: Some(1),
                                    })
                                } else {
                                    BackgroundWait::Resource
                                };
                                task.wait(&fence, wait).unwrap();
                            }
                            if phase == 2 {
                                task.request_cancel().unwrap();
                            }
                            match &mut task.lifecycle {
                                BackgroundTaskLifecycle::Running { attempt }
                                | BackgroundTaskLifecycle::Waiting { attempt, .. }
                                | BackgroundTaskLifecycle::Cancelling { attempt, .. } => {
                                    attempt.epoch = epoch
                                }
                                _ => unreachable!(),
                            }
                            task.revision = revision;
                            let before = task.clone();
                            let result = task.reclaim("b", now, lease);
                            let error = if lease == 0 {
                                Some(BackgroundTaskError::InvalidClaim)
                            } else if now == u64::MAX {
                                Some(BackgroundTaskError::ClockOverflow)
                            } else if now < 10 {
                                Some(BackgroundTaskError::Busy)
                            } else if revision == u64::MAX {
                                Some(BackgroundTaskError::RevisionOverflow)
                            } else {
                                None
                            };
                            if let Some(error) = error {
                                assert_eq!(result, Err(error), "R1/R2/R4/R5");
                                assert_eq!(task, before, "rejected mutation is inert");
                            } else {
                                assert_eq!(task.revision, revision + 1);
                                assert!(task.same_request_as(&before));
                                if (!remote
                                    && (mode == ToolRecoveryMode::NeverReplay || phase == 2))
                                    || (remote && epoch >= 2)
                                {
                                    assert_eq!(result, Ok(TaskClaim::EndedIndeterminate), "R6");
                                    assert!(matches!(
                                        task.lifecycle,
                                        BackgroundTaskLifecycle::Ended {
                                            end: BackgroundTaskEnd::Indeterminate { .. }
                                        }
                                    ));
                                } else if epoch >= 2 {
                                    assert_eq!(result, Ok(TaskClaim::EndedAttemptsExhausted), "R7");
                                    assert!(matches!(
                                        task.lifecycle,
                                        BackgroundTaskLifecycle::Ended {
                                            end: BackgroundTaskEnd::Failed { .. }
                                        }
                                    ));
                                } else {
                                    assert_eq!(
                                        result,
                                        Ok(TaskClaim::Acquired(TaskFence {
                                            worker_id: "b".into(),
                                            epoch: epoch + 1
                                        })),
                                        "R8"
                                    );
                                    let mut expected = before;
                                    expected.revision += 1;
                                    match &mut expected.lifecycle {
                                        BackgroundTaskLifecycle::Running { attempt }
                                        | BackgroundTaskLifecycle::Waiting { attempt, .. }
                                        | BackgroundTaskLifecycle::Cancelling { attempt, .. } => {
                                            attempt.worker_id = "b".into();
                                            attempt.epoch += 1;
                                            attempt.lease_expires_at_ms = now + lease;
                                        }
                                        _ => unreachable!(),
                                    }
                                    assert_eq!(task, expected, "exact recovery frame");
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut requested = task();
    let before = requested.clone();
    assert_eq!(
        requested.reclaim("replacement", 10, 10),
        Err(BackgroundTaskError::InvalidTransition),
        "R3 Requested"
    );
    assert_eq!(requested, before, "R3 Requested stutter");

    let mut ended = task();
    let fence = ended.start("worker", 0, 10, replay_policy()).unwrap();
    ended.finish(&fence, BackgroundTaskEnd::Cancelled).unwrap();
    let before = ended.clone();
    assert_eq!(
        ended.reclaim("replacement", 10, 10),
        Err(BackgroundTaskError::Terminal),
        "R3 Ended"
    );
    assert_eq!(ended, before, "R3 Ended stutter");
}

#[test]
fn claim_lease_recovery_and_fence_form_one_monotone_authority() {
    // Causal graph: C1 Requested claim -> epoch 1; C2 live competing claim ->
    // Busy; C3 heartbeat extends the same fence; C4 expired replay claim ->
    // epoch 2; C5 epoch-1 completion -> StaleFence; C6 epoch-2 completion ->
    // terminal. These are the safety edges an external Driver composes with
    // Runtime CAS; none requires a task table.
    let mut task = task();
    let first = task
        .start("worker-a", 10, 20, replay_policy())
        .expect("fresh task is claimable");
    assert_eq!(first.epoch, 1, "C1");
    assert_eq!(
        task.reclaim("worker-b", 20, 20),
        Err(BackgroundTaskError::Busy),
        "C2"
    );
    task.heartbeat(&first, 20, 20)
        .expect("matching fence extends the lease");
    assert_eq!(
        task.reclaim("worker-b", 39, 20),
        Err(BackgroundTaskError::Busy),
        "C3"
    );
    let TaskClaim::Acquired(second) = task
        .reclaim("worker-b", 40, 20)
        .expect("expired replay-safe task is reclaimable")
    else {
        panic!("C4 must reclaim the task")
    };
    assert_eq!(second.epoch, 2, "C4");
    assert_eq!(
        task.finish(
            &first,
            BackgroundTaskEnd::Failed {
                message: "stale".into()
            }
        ),
        Err(BackgroundTaskError::StaleFence),
        "C5"
    );
    task.finish(
        &second,
        BackgroundTaskEnd::Completed {
            content: Vec::new(),
            is_error: false,
        },
    )
    .expect("matching current fence can finish");
    assert!(task.lifecycle.is_terminal(), "C6");
}

#[test]
fn cancellation_absorbs_a_racing_success_and_is_idempotent() {
    // Decision table: Requested cancel ends immediately; Running cancel enters
    // Cancelling; a matching late success is projected to Cancelled; repeated
    // cancel does not bump revision. Cancellation therefore has one absorbing
    // outcome independent of arrival order.
    let mut queued = task();
    queued
        .request_cancel()
        .expect("requested task is cancellable");
    assert!(matches!(
        queued.lifecycle,
        BackgroundTaskLifecycle::Ended {
            end: BackgroundTaskEnd::Cancelled
        }
    ));
    let revision = queued.revision;
    queued
        .request_cancel()
        .expect("terminal cancellation is idempotent");
    assert_eq!(queued.revision, revision);

    let mut running = task();
    let fence = running
        .start("worker", 0, 10, replay_policy())
        .expect("fresh task is claimable");
    running
        .request_cancel()
        .expect("running task is cancellable");
    running
        .finish(
            &fence,
            BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
        )
        .expect("matching worker acknowledges cancellation");
    assert!(matches!(
        running.lifecycle,
        BackgroundTaskLifecycle::Ended {
            end: BackgroundTaskEnd::Cancelled
        }
    ));
}

#[test]
fn never_replay_crash_window_changes_only_after_remote_identity_is_durable() {
    // Crash-window decision table: C1 NeverReplay Running expires before a
    // remote id is committed -> Indeterminate; C2 incomplete remote coordinates
    // cannot close that window; C3 exact coordinates enter durable Waiting;
    // C4 the same NeverReplay policy may now reclaim Waiting at epoch+1 because
    // recovery addresses the committed id rather than replaying the start.
    let mut uncertain = task();
    uncertain
        .start(
            "worker",
            0,
            10,
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("fresh task is claimable");
    assert_eq!(
        uncertain.reclaim("replacement", 10, 10),
        Ok(TaskClaim::EndedIndeterminate)
    );
    assert!(matches!(
        uncertain.lifecycle,
        BackgroundTaskLifecycle::Ended {
            end: BackgroundTaskEnd::Indeterminate { .. }
        }
    ));

    let mut remote = task();
    let fence = remote
        .start(
            "worker",
            0,
            10,
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("fresh remote task is claimable");
    assert_eq!(
        remote.wait(
            &fence,
            BackgroundWait::Remote(ToolTaskHandle {
                owner: "mcp".into(),
                binding: String::new(),
                task_id: "remote-1".into(),
                poll_interval_ms: Some(100),
            })
        ),
        Err(BackgroundTaskError::InvalidContinuation)
    );
    remote
        .wait(
            &fence,
            BackgroundWait::Remote(ToolTaskHandle {
                owner: "mcp".into(),
                binding: "mcp-server".into(),
                task_id: "remote-1".into(),
                poll_interval_ms: Some(100),
            }),
        )
        .expect("complete remote identity may wait");
    assert!(matches!(
        remote.lifecycle,
        BackgroundTaskLifecycle::Waiting { .. }
    ));
    let TaskClaim::Acquired(recovered) = remote
        .reclaim("replacement", 10, 10)
        .expect("C4 committed remote id is reconnectable")
    else {
        panic!("C4 must acquire a replacement fence")
    };
    assert_eq!(recovered.epoch, 2, "C4");
    assert!(matches!(
        remote.lifecycle,
        BackgroundTaskLifecycle::Waiting { .. }
    ));
}

#[test]
fn persisted_recovery_policy_is_the_only_reclaim_authority() {
    // Decision table: the frozen policy admits exactly two total attempts.
    // Initial claim consumes epoch 1, one reclaim consumes epoch 2, and the
    // next expiry commits a failed terminal outcome. No caller-supplied replay
    // flag can disagree with or bypass the persisted attempt budget.
    let mut task = task();
    let policy = TaskExecutionPolicy {
        recovery: ToolRecoveryPolicy::try_new(ToolRecoveryMode::ReplaySafe, 2)
            .expect("two is a valid non-zero attempt budget"),
        concurrency: ToolConcurrency::Parallel,
    };
    assert_eq!(
        task.start("worker-1", 0, 10, policy)
            .expect("fresh task starts"),
        TaskFence {
            worker_id: "worker-1".into(),
            epoch: 1
        }
    );
    assert!(matches!(
        task.reclaim("worker-2", 10, 10),
        Ok(TaskClaim::Acquired(TaskFence { epoch: 2, .. }))
    ));
    assert_eq!(
        task.reclaim("worker-3", 20, 10),
        Ok(TaskClaim::EndedAttemptsExhausted)
    );
    assert!(matches!(
        task.lifecycle,
        BackgroundTaskLifecycle::Ended {
            end: BackgroundTaskEnd::Failed { .. }
        }
    ));
}

#[test]
fn exhausted_remote_observation_budget_never_claims_remote_failure() {
    // Cause/effect decision table: R1 an expired replayable local effect at its
    // frozen attempt limit -> Failed; R2 an expired Waiting remote continuation
    // at the same limit -> Indeterminate because transport exhaustion cannot
    // prove the server task failed or stopped. Explicit remote Failed/Cancelled
    // observations still enter through finish and remain distinct.
    let mut remote = task();
    let fence = remote
        .start(
            "worker",
            0,
            10,
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::try_new(ToolRecoveryMode::NeverReplay, 1)
                    .expect("one observation attempt"),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("remote start");
    remote
        .wait(
            &fence,
            BackgroundWait::Remote(ToolTaskHandle {
                owner: "mcp".into(),
                binding: "mcp-server".into(),
                task_id: "remote-at-limit".into(),
                poll_interval_ms: Some(100),
            }),
        )
        .expect("remote identity committed");

    assert_eq!(
        remote.reclaim("replacement", 10, 10),
        Ok(TaskClaim::EndedIndeterminate),
        "R2"
    );
    assert!(matches!(
        remote.lifecycle,
        BackgroundTaskLifecycle::Ended {
            end: BackgroundTaskEnd::Indeterminate { .. }
        }
    ));
}

#[test]
fn persisted_shape_drift_and_empty_task_id_fail_closed() {
    assert!(BackgroundTaskId::new(" ").is_err());
    let mut value = serde_json::to_value(task()).expect("task serializes");
    value
        .as_object_mut()
        .expect("serialized task is an object")
        .insert("unknown".into(), serde_json::json!(true));
    assert!(serde_json::from_value::<BackgroundTask>(value).is_err());
}

#[test]
fn failed_transitions_are_atomic_and_persisted_invariants_fail_closed() {
    // Cause/effect decision table: C1 start clock overflow; C2 cancellation
    // revision overflow; C3 invalid wait; C4 stale finish; C5 exact heartbeat
    // with zero lease/clock overflow/non-advancing lease/revision overflow;
    // C6 stale heartbeat/finish combined with otherwise-invalid command input;
    // C7 empty durable call identity; C8 impossible Requested revision.
    // R1 any stale owner/epoch returns StaleFence before command validation;
    // R2 an exact heartbeat reports its first command-specific error in the C5
    // order; R3 every rejected transition leaves the complete aggregate equal;
    // R4 malformed persisted aggregates fail closed. These rules cross the
    // actual heartbeat/finish methods that consume the shared production plan.
    let mut clock_overflow = task();
    let before = clock_overflow.clone();
    assert_eq!(
        clock_overflow.start("worker", u64::MAX, 1, replay_policy()),
        Err(BackgroundTaskError::ClockOverflow),
        "C1/E1"
    );
    assert_eq!(clock_overflow, before, "C1/E1");

    let mut revision_overflow = task();
    revision_overflow.revision = u64::MAX;
    let before = revision_overflow.clone();
    assert_eq!(
        revision_overflow.request_cancel(),
        Err(BackgroundTaskError::RevisionOverflow),
        "C2/E1"
    );
    assert_eq!(revision_overflow, before, "C2/E1");

    let mut active = task();
    let fence = active
        .start("worker", 0, 10, replay_policy())
        .expect("fresh task starts");
    let before = active.clone();
    assert_eq!(
        active.wait(
            &fence,
            BackgroundWait::Remote(ToolTaskHandle {
                owner: "mcp".into(),
                binding: String::new(),
                task_id: "remote".into(),
                poll_interval_ms: Some(100),
            }),
        ),
        Err(BackgroundTaskError::InvalidContinuation),
        "C3/E1"
    );
    assert_eq!(active, before, "C3/E1");
    assert_eq!(
        active.finish(
            &TaskFence {
                worker_id: "other".into(),
                epoch: fence.epoch,
            },
            BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
        ),
        Err(BackgroundTaskError::StaleFence),
        "C4/R1"
    );
    assert_eq!(active, before, "C4/R3");

    for (now_ms, lease_ms, error) in [
        (0, 0, BackgroundTaskError::InvalidClaim),
        (u64::MAX, 1, BackgroundTaskError::ClockOverflow),
        (0, 1, BackgroundTaskError::NonMonotonicLease),
    ] {
        let mut rejected = active.clone();
        let before = rejected.clone();
        assert_eq!(
            rejected.heartbeat(&fence, now_ms, lease_ms),
            Err(error),
            "C5/R2"
        );
        assert_eq!(rejected, before, "C5/R3");
    }

    active.revision = u64::MAX;
    let before = active.clone();
    assert_eq!(
        active.heartbeat(&fence, 10, 10),
        Err(BackgroundTaskError::RevisionOverflow),
        "C5/R2"
    );
    assert_eq!(active, before, "C5/R3");
    let stale = TaskFence {
        worker_id: "other".into(),
        epoch: fence.epoch.wrapping_add(1),
    };
    assert_eq!(
        active.heartbeat(&stale, u64::MAX, 0),
        Err(BackgroundTaskError::StaleFence),
        "C6/R1 heartbeat"
    );
    assert_eq!(active, before, "C6/R3 heartbeat");
    assert_eq!(
        active.finish(&stale, BackgroundTaskEnd::Cancelled),
        Err(BackgroundTaskError::StaleFence),
        "C6/R1 finish"
    );
    assert_eq!(active, before, "C6/R3 finish");

    let mut mismatched_call = serde_json::to_value(task()).expect("task serializes");
    mismatched_call["invocation"]["call"]["call_id"] = serde_json::json!("");
    assert!(
        serde_json::from_value::<BackgroundTask>(mismatched_call).is_err(),
        "C7/R4"
    );

    let mut impossible_revision = serde_json::to_value(task()).expect("task serializes");
    impossible_revision["revision"] = serde_json::json!(1);
    assert!(
        serde_json::from_value::<BackgroundTask>(impossible_revision).is_err(),
        "C8/R4"
    );
}

#[test]
fn remote_wait_reclaim_preserves_coordinates_and_fences_stale_observers() {
    // Cause/effect decision table:
    // R1 Running + complete remote coordinates -> Waiting with the same fence.
    // R2 Waiting + same remote identity + a new positive poll interval -> only
    //    the interval and aggregate revision advance.
    // R3 Waiting + changed owner/binding/task identity or zero interval ->
    //    reject and leave the aggregate byte-for-byte unchanged.
    // R4 expired NeverReplay Waiting with a committed remote id -> Waiting at
    //    epoch+1 with identical continuation coordinates; recovery polls that
    //    id and must not replay the original effect.
    // R5 any epoch-1 heartbeat/wait/finish after R4 -> StaleFence and no state
    //    mutation; epoch 2 remains the sole settlement authority.
    let mut remote = task();
    let first = remote
        .start(
            "worker-1",
            0,
            10,
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("fresh durable request starts");
    let initial = ToolTaskHandle {
        owner: "mcp".into(),
        binding: "mcp/server@generation-7".into(),
        task_id: "remote-1".into(),
        poll_interval_ms: Some(100),
    };
    remote
        .wait(&first, BackgroundWait::Remote(initial.clone()))
        .expect("R1 complete continuation is durable");

    let refreshed = ToolTaskHandle {
        poll_interval_ms: Some(250),
        ..initial.clone()
    };
    let revision = remote.revision;
    remote
        .wait(&first, BackgroundWait::Remote(refreshed.clone()))
        .expect("R2 the same remote request may refresh its poll delay");
    assert_eq!(remote.revision, revision + 1, "R2");

    for invalid in [
        ToolTaskHandle {
            owner: "another-durable-adapter".into(),
            ..refreshed.clone()
        },
        ToolTaskHandle {
            owner: "mcp".into(),
            binding: "mcp/server@generation-8".into(),
            ..refreshed.clone()
        },
        ToolTaskHandle {
            task_id: "remote-2".into(),
            ..refreshed.clone()
        },
        ToolTaskHandle {
            poll_interval_ms: Some(0),
            ..refreshed.clone()
        },
    ] {
        let before = remote.clone();
        assert!(
            remote
                .wait(&first, BackgroundWait::Remote(invalid))
                .is_err(),
            "R3 invalid replacement is rejected"
        );
        assert_eq!(remote, before, "R3 rejection is atomic");
    }

    let TaskClaim::Acquired(second) = remote
        .reclaim("worker-2", 10, 10)
        .expect("R4 committed remote id makes the wait reconnectable")
    else {
        panic!("R4 must acquire a replacement fence")
    };
    assert_eq!(second.epoch, 2, "R4");
    assert!(matches!(
        &remote.lifecycle,
        BackgroundTaskLifecycle::Waiting {
            attempt,
            wait: BackgroundWait::Remote(continuation),
        } if attempt.owns(&second) && continuation == &refreshed
    ));

    let before = remote.clone();
    assert_eq!(
        remote.heartbeat(&first, 10, 10),
        Err(BackgroundTaskError::StaleFence),
        "R5 heartbeat"
    );
    assert_eq!(remote, before, "R5 heartbeat is inert");
    assert_eq!(
        remote.wait(&first, BackgroundWait::Remote(refreshed.clone())),
        Err(BackgroundTaskError::StaleFence),
        "R5 wait"
    );
    assert_eq!(remote, before, "R5 wait is inert");
    assert_eq!(
        remote.finish(
            &first,
            BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
        ),
        Err(BackgroundTaskError::StaleFence),
        "R5 finish"
    );
    assert_eq!(remote, before, "R5 finish is inert");
}

#[test]
fn remote_cancel_and_reclaim_retain_the_single_remote_request() {
    // Cause/effect decision table:
    // R1 Waiting(remote) + cancel -> Cancelling(Some(remote)); R2 live-lease
    // reclaim -> Busy and no mutation; R3 expired NeverReplay Cancelling with
    // a committed id -> the same Cancelling wait at epoch+1; R4 stale
    // completion -> StaleFence and no mutation; R5 current completion racing
    // cancel -> terminal Cancelled.
    // This proves cancellation never degrades into a blind local cancellation
    // or a second remote task creation.
    let mut remote = task();
    let first = remote
        .start(
            "worker-1",
            0,
            10,
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("fresh durable request starts");
    let continuation = ToolTaskHandle {
        owner: "mcp".into(),
        binding: "mcp/server@generation-7".into(),
        task_id: "remote-1".into(),
        poll_interval_ms: Some(200),
    };
    remote
        .wait(&first, BackgroundWait::Remote(continuation.clone()))
        .expect("remote request is durably waiting");
    remote.request_cancel().expect("R1 cancel intent commits");
    assert!(matches!(
        &remote.lifecycle,
        BackgroundTaskLifecycle::Cancelling {
            attempt,
            wait: Some(BackgroundWait::Remote(actual)),
        } if attempt.owns(&first) && actual == &continuation
    ));

    let before = remote.clone();
    assert_eq!(
        remote.reclaim("worker-2", 9, 10),
        Err(BackgroundTaskError::Busy),
        "R2"
    );
    assert_eq!(remote, before, "R2");

    let TaskClaim::Acquired(second) = remote
        .reclaim("worker-2", 10, 10)
        .expect("R3 expired cancellation is reclaimable")
    else {
        panic!("R3 must acquire a replacement fence")
    };
    assert!(matches!(
        &remote.lifecycle,
        BackgroundTaskLifecycle::Cancelling {
            attempt,
            wait: Some(BackgroundWait::Remote(actual)),
        } if attempt.owns(&second) && actual == &continuation
    ));

    let before = remote.clone();
    assert_eq!(
        remote.finish(
            &first,
            BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
        ),
        Err(BackgroundTaskError::StaleFence),
        "R4"
    );
    assert_eq!(remote, before, "R4");

    remote
        .finish(
            &second,
            BackgroundTaskEnd::Completed {
                content: Vec::new(),
                is_error: false,
            },
        )
        .expect("R5 current worker settles cancellation");
    assert!(matches!(
        remote.lifecycle,
        BackgroundTaskLifecycle::Ended {
            end: BackgroundTaskEnd::Cancelled
        }
    ));
}

#[test]
fn remote_start_candidate_attaches_after_cancel_without_losing_cancel_intent() {
    // Interleaving decision table for the start-response/StepStart boundary:
    // R1 start is durably Running, user cancel commits first ->
    // Cancelling(None); R2 the matching process candidate later carries the
    // newly-created remote id -> Cancelling(Some(remote)); R3 a stale candidate
    // cannot attach; R4 a second/different remote id cannot replace the first.
    // Thus cancellation wins the lifecycle race while the exact remote task
    // remains available to the cancellation driver.
    let mut remote = task();
    let fence = remote
        .start(
            "worker-1",
            0,
            10,
            TaskExecutionPolicy {
                recovery: ToolRecoveryPolicy::default(),
                concurrency: ToolConcurrency::Parallel,
            },
        )
        .expect("fresh task starts");
    remote
        .request_cancel()
        .expect("R1 cancel commits before the start candidate");
    assert!(matches!(
        remote.lifecycle,
        BackgroundTaskLifecycle::Cancelling { wait: None, .. }
    ));

    let continuation = ToolTaskHandle {
        owner: "mcp".into(),
        binding: "mcp/server@generation-7".into(),
        task_id: "remote-1".into(),
        poll_interval_ms: Some(100),
    };
    remote
        .wait(&fence, BackgroundWait::Remote(continuation.clone()))
        .expect("R2 matching start candidate attaches without reopening Waiting");
    assert!(matches!(
        &remote.lifecycle,
        BackgroundTaskLifecycle::Cancelling {
            wait: Some(BackgroundWait::Remote(actual)),
            ..
        } if actual == &continuation
    ));

    let before = remote.clone();
    assert_eq!(
        remote.wait(
            &TaskFence {
                worker_id: "worker-2".into(),
                epoch: fence.epoch,
            },
            BackgroundWait::Remote(continuation.clone()),
        ),
        Err(BackgroundTaskError::StaleFence),
        "R3"
    );
    assert_eq!(remote, before, "R3");

    assert_eq!(
        remote.wait(
            &fence,
            BackgroundWait::Remote(ToolTaskHandle {
                task_id: "remote-2".into(),
                ..continuation
            }),
        ),
        Err(BackgroundTaskError::InvalidTransition),
        "R4"
    );
    assert_eq!(remote, before, "R4");
}

#[test]
fn legacy_cancelling_wire_without_wait_remains_compatible() {
    // Cause/effect compatibility table: R1 historical Cancelling without wait
    // -> None; R2 historical Remote protocol/server_binding and no poll delay
    // -> the canonical opaque owner/binding handle; R3 reserialization -> only
    // owner/binding, so one current wire is written; R4 incomplete historical
    // coordinates -> reject. This reads the old wire without retaining a
    // second aggregate continuation type or weakening current validation.
    let mut running = task();
    running
        .start("worker", 0, 10, replay_policy())
        .expect("fixture starts");
    running.request_cancel().expect("fixture cancels");
    let mut legacy = serde_json::to_value(&running).expect("task serializes");
    legacy["lifecycle"]
        .as_object_mut()
        .expect("lifecycle is an object")
        .remove("wait");
    let decoded = serde_json::from_value::<BackgroundTask>(legacy)
        .expect("historical cancelling state remains readable");
    assert!(matches!(
        decoded.lifecycle,
        BackgroundTaskLifecycle::Cancelling { wait: None, .. }
    ));

    let mut waiting = task();
    let waiting_fence = waiting
        .start("worker", 0, 10, replay_policy())
        .expect("fixture starts");
    waiting
        .wait(
            &waiting_fence,
            BackgroundWait::Remote(ToolTaskHandle {
                owner: "mcp".into(),
                binding: "mcp-server".into(),
                task_id: "remote-1".into(),
                poll_interval_ms: Some(100),
            }),
        )
        .expect("fixture waits");
    let mut legacy_wait = serde_json::to_value(&waiting).expect("task serializes");
    let legacy_wait_object = legacy_wait["lifecycle"]["wait"]
        .as_object_mut()
        .expect("remote wait is an object");
    let owner = legacy_wait_object.remove("owner").expect("canonical owner");
    legacy_wait_object.insert("protocol".into(), owner);
    let binding = legacy_wait_object
        .remove("binding")
        .expect("canonical binding");
    legacy_wait_object.insert("server_binding".into(), binding);
    legacy_wait_object.remove("poll_interval_ms");
    let decoded_wait = serde_json::from_value::<BackgroundTask>(legacy_wait)
        .expect("historical remote wait remains readable");
    assert!(matches!(
        &decoded_wait.lifecycle,
        BackgroundTaskLifecycle::Waiting {
            wait: BackgroundWait::Remote(handle),
            ..
        } if handle.owner == "mcp"
            && handle.binding == "mcp-server"
            && handle.poll_interval_ms.is_none()
    ));
    let current = serde_json::to_value(&decoded_wait).expect("current task serializes");
    let current_wait = current["lifecycle"]["wait"]
        .as_object()
        .expect("current remote wait is an object");
    assert!(current_wait.contains_key("owner"), "R3 canonical owner");
    assert!(current_wait.contains_key("binding"), "R3 canonical binding");
    assert!(!current_wait.contains_key("protocol"), "R3 old owner alias");
    assert!(
        !current_wait.contains_key("server_binding"),
        "R3 old binding alias"
    );

    let mut malformed = serde_json::to_value(&decoded).expect("task serializes");
    malformed["lifecycle"]["wait"] = serde_json::json!({
        "reason": "remote",
        "protocol": "mcp",
        "server_binding": "",
        "task_id": "remote-1",
        "poll_interval_ms": 100
    });
    assert!(serde_json::from_value::<BackgroundTask>(malformed).is_err());
}
