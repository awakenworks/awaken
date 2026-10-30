// @formal evidence background_task.cancellation_absorption
// @formal evidence background_task.expiry_reclaim_advances_epoch_or_terminates
// @formal evidence background_task.never_replay_and_attempt_exhaustion_fail_closed
// @formal evidence background_task.remote_continuation_survives_wait_cancel_and_reclaim
// @formal evidence background_task.single_fenced_attempt
// @formal evidence background_task.stale_fence_never_finishes_or_heartbeats
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_background_task::{
    BackgroundInvocation, BackgroundTask, BackgroundTaskEnd, BackgroundTaskError, BackgroundTaskId,
    BackgroundTaskLifecycle, BackgroundTaskOrigin, BackgroundWait, TaskClaim, TaskExecutionPolicy,
    TaskHeartbeatCheckpointError, TaskHeartbeatCheckpointOutcome,
};
use awaken_lease_contract::fence::{Epoch, Tenure};
use awaken_tool_contract::tool::{
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

fn heartbeat_task(phase: usize) -> BackgroundTask {
    // Reuse the original phase fixture and add the remaining wait/None forms.
    if phase < 7 {
        return execution_task(phase);
    }
    let mut task = execution_task(1);
    let fence = task.tenure().unwrap();
    if matches!(phase, 8 | 9) {
        task.wait(&fence, BackgroundWait::Resource).unwrap();
    }
    if matches!(phase, 7 | 9) {
        task.request_cancel().unwrap();
    }
    task
}

fn expected_heartbeat(mut task: BackgroundTask, target_ms: u64) -> BackgroundTask {
    // Full-aggregate oracle: deadline and revision are the only changed fields.
    match &mut task.lifecycle {
        BackgroundTaskLifecycle::Running { attempt }
        | BackgroundTaskLifecycle::Waiting { attempt, .. }
        | BackgroundTaskLifecycle::Cancelling { attempt, .. } => {
            attempt.lease_expires_at_ms = target_ms;
        }
        _ => panic!("only an attempt-bearing fixture can apply a heartbeat"),
    }
    task.revision += 1;
    task
}

#[test]
fn checkpoint_errors_preserve_the_existing_aggregate_error_contract() {
    // Compatibility cause/effect table: C1 each original aggregate error;
    // C2 direct or checkpoint-wrapped use. R1 an unchanged exhaustive external
    // match must compile for C1; R2 both C2 forms retain the original Display.
    // R3 checkpoint expiry is a separate negative error with its exact message,
    // not an added variant of the existing enum. This is a downstream source
    // contract witness; the checkpoint decision table below owns admission.
    fn original_error_text(error: &BackgroundTaskError) -> &'static str {
        match error {
            BackgroundTaskError::InvalidIdentity => "background task id must not be empty",
            BackgroundTaskError::InvalidClaim => {
                "background task claim requires a worker and a positive lease"
            }
            BackgroundTaskError::Busy => "background task is already owned",
            BackgroundTaskError::Terminal => "background task is terminal",
            BackgroundTaskError::InvalidTransition => "background task transition is invalid",
            BackgroundTaskError::StaleFence => "background task fence is stale",
            BackgroundTaskError::NonMonotonicLease => {
                "background task lease must advance monotonically"
            }
            BackgroundTaskError::InvalidContinuation => {
                "remote continuation requires an owner, binding, task id, and a positive poll interval when present"
            }
            BackgroundTaskError::ClockOverflow => "background task clock overflow",
            BackgroundTaskError::EpochOverflow => "background task epoch overflow",
            BackgroundTaskError::RevisionOverflow => "background task revision overflow",
            BackgroundTaskError::InvalidPersistedState => {
                "background task persisted state violates its aggregate invariants"
            }
        }
    }
    for error in [
        BackgroundTaskError::InvalidIdentity,
        BackgroundTaskError::InvalidClaim,
        BackgroundTaskError::Busy,
        BackgroundTaskError::Terminal,
        BackgroundTaskError::InvalidTransition,
        BackgroundTaskError::StaleFence,
        BackgroundTaskError::NonMonotonicLease,
        BackgroundTaskError::InvalidContinuation,
        BackgroundTaskError::ClockOverflow,
        BackgroundTaskError::EpochOverflow,
        BackgroundTaskError::RevisionOverflow,
        BackgroundTaskError::InvalidPersistedState,
    ] {
        let expected = original_error_text(&error);
        assert_eq!(error.to_string(), expected, "R1/R2 original error");
        let wrapped = TaskHeartbeatCheckpointError::from(error.clone());
        assert_eq!(
            wrapped.to_string(),
            expected,
            "R2 transparent checkpoint error"
        );
        assert_eq!(wrapped, TaskHeartbeatCheckpointError::Task(error));
    }
    assert_eq!(
        TaskHeartbeatCheckpointError::LeaseExpired.to_string(),
        "background task lease expired before the heartbeat checkpoint applied",
        "R3 separate negative checkpoint error"
    );
}

#[test]
fn fixed_target_heartbeat_checkpoint_follows_the_complete_decision_table() {
    // Causes: C1 Requested/Running/remote or both local wait forms/Cancelling
    // intent with no wait/Ended;
    // C2 exact tenure or foreign task/holder/epoch; C3 target zero/below/equal/
    // above current D=10/MAX; C4 commit time before/at/after D/MAX; C5 revision
    // has capacity/is MAX. Effects: E1 unchanged ACK; E2 only deadline=T and
    // revision+1, preserving the complete phase/wait/identity/policy; E3 typed
    // error with complete aggregate equality. Constraints: C1 without attempt
    // and C2 mismatch precede all other causes; no caller supplies liveness.
    // Checkpoint-only LeaseExpired remains a negative result. Other failures
    // wrap the unchanged BackgroundTaskError; legacy heartbeat/finish callers
    // retain that original exhaustive enum and its exact errors/display.
    //
    // | Rule | Current attempt/tenure | T | now | revision | Effect |
    // | C1 | absent or mismatched | any | any | any | StaleFence/E3 |
    // | C2 | exact | 0 | any | any | InvalidClaim/E3 |
    // | C3 | exact | 0<T<=D | any | any | ACK/E1 |
    // | C4 | exact | T>D | >=D | any | LeaseExpired/E3 |
    // | C5 | exact | T>D | <D | MAX | RevisionOverflow/E3 |
    // | C6 | exact | T>D | <D | capacity | Applied/E2 |
    // C9 applies E2's full-field oracle to every wait form and Cancelling(None).
    for phase in 0..10 {
        for foreign in 0..4 {
            for revision_max in [false, true] {
                for now_ms in [9, 10, 11, u64::MAX] {
                    for target_ms in [0, 9, 10, 11, 20, u64::MAX] {
                        let mut current = heartbeat_task(phase);
                        if revision_max {
                            current.revision = u64::MAX;
                        }
                        let fence = Tenure::granted(
                            if foreign == 1 {
                                BackgroundTaskId::new("another-task").unwrap()
                            } else {
                                current.id.clone()
                            },
                            if foreign == 2 {
                                "another-holder"
                            } else {
                                "worker"
                            }
                            .into(),
                            if foreign == 3 {
                                Epoch::FIRST.next().unwrap()
                            } else {
                                Epoch::FIRST
                            },
                        );
                        let before = current.clone();
                        let expected = if matches!(phase, 0 | 4) || foreign != 0 {
                            Err(TaskHeartbeatCheckpointError::Task(
                                BackgroundTaskError::StaleFence,
                            ))
                        } else if target_ms == 0 {
                            Err(TaskHeartbeatCheckpointError::Task(
                                BackgroundTaskError::InvalidClaim,
                            ))
                        } else if target_ms <= 10 {
                            Ok(TaskHeartbeatCheckpointOutcome::Acknowledged)
                        } else if now_ms >= 10 {
                            Err(TaskHeartbeatCheckpointError::LeaseExpired)
                        } else if revision_max {
                            Err(TaskHeartbeatCheckpointError::Task(
                                BackgroundTaskError::RevisionOverflow,
                            ))
                        } else {
                            Ok(TaskHeartbeatCheckpointOutcome::Applied)
                        };
                        let result = current.heartbeat_checkpoint(&fence, target_ms, now_ms);
                        assert_eq!(
                            result, expected,
                            "phase={phase} foreign={foreign} T={target_ms} now={now_ms} max={revision_max}"
                        );
                        let after = if result == Ok(TaskHeartbeatCheckpointOutcome::Applied) {
                            expected_heartbeat(before, target_ms)
                        } else {
                            before
                        };
                        assert_eq!(current, after, "C1-C6 complete aggregate effect");
                    }
                }
            }
        }
    }
}

#[test]
fn fixed_target_checkpoint_replay_is_inert_even_after_expiry() {
    // C7 an exact checkpoint first applies while live, then the response is
    // lost and the SAME target is retried before/at/after its deadline/MAX.
    // E1 every retry ACKs without moving deadline, revision or any other field;
    // E2 late ACK never authorizes execution. Cross all attempt-bearing phase
    // forms so a Cancelling phase or exact remote handle cannot be rewritten.
    for phase in [1, 2, 3, 5, 6, 7, 8, 9] {
        let expected = heartbeat_task(phase);
        let fence = expected.tenure().unwrap();
        let mut current = expected.clone();
        assert_eq!(
            current.heartbeat_checkpoint(&fence, 20, 9),
            Ok(TaskHeartbeatCheckpointOutcome::Applied),
            "C7 first application"
        );
        let applied = current.clone();
        for now_ms in [9, 19, 20, 21, u64::MAX] {
            assert_eq!(
                current.heartbeat_checkpoint(&fence, 20, now_ms),
                Ok(TaskHeartbeatCheckpointOutcome::Acknowledged),
                "C7/E1"
            );
            assert_eq!(current, applied, "C7/E1 exact replay stutter");
            if now_ms >= 20 {
                assert!(
                    !current.admits_execution_of(&expected, now_ms),
                    "C7/E2 historical ACK grants no effect"
                );
            }
        }
    }
}

#[test]
fn a_checkpoint_for_the_predecessor_cannot_ack_a_successor_tenure() {
    // C8 the same Task has reached T under epoch 1, then canonical reclaim
    // assigns epoch 2 with a later deadline. E1 the epoch-1 retry refuses even
    // though current deadline>=T; E2 no successor field changes. Reclaim keeps
    // remote Cancelling intact; it remains the sole ownership-transfer owner.
    for phase in [1, 2, 3] {
        let mut current = execution_task(phase);
        let prior = current.tenure().unwrap();
        current.heartbeat_checkpoint(&prior, 20, 9).unwrap();
        let TaskClaim::Acquired(successor) = current.reclaim("next-worker", 20, 30).unwrap() else {
            panic!("C8 canonical reclaim must advance this replayable/remote attempt")
        };
        assert!(successor.epoch() > prior.epoch(), "C8 successor tenure");
        let before = current.clone();
        assert_eq!(
            current.heartbeat_checkpoint(&prior, 20, 50),
            Err(TaskHeartbeatCheckpointError::Task(
                BackgroundTaskError::StaleFence
            )),
            "C8/E1 tenure precedes historical ACK"
        );
        assert_eq!(current, before, "C8/E2 successor is unchanged");
    }
}

#[test]
fn local_relative_heartbeat_retains_its_expired_renewal_and_replay_errors() {
    // L1 the original local entry receives now at/after D=10, a positive TTL
    // and revision capacity: it still applies now+TTL, including target MAX.
    // L2 repeating the same local now/TTL remains NonMonotonicLease, not an
    // ACK. E1 only deadline/revision change; E2 rejected replay is fully inert.
    // Original stale/zero/clock/monotonic/revision precedence assertions below
    // remain unchanged; checkpoint's stricter application is a distinct input.
    for phase in [1, 2, 3, 5, 6, 7, 8, 9] {
        for now_ms in [10, 11, u64::MAX - 20] {
            let mut current = heartbeat_task(phase);
            let fence = current.tenure().unwrap();
            let expected = expected_heartbeat(current.clone(), now_ms + 20);
            current.heartbeat(&fence, now_ms, 20).unwrap();
            assert_eq!(current, expected, "L1/E1 original relative heartbeat");
            let before = current.clone();
            assert_eq!(
                current.heartbeat(&fence, now_ms, 20),
                Err(BackgroundTaskError::NonMonotonicLease),
                "L2/E2 local replay retains its error"
            );
            assert_eq!(current, before, "L2/E2 complete aggregate stutter");
        }
    }
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
        .heartbeat(&expected.tenure().unwrap(), 9, 10)
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
                        8 => attempt.epoch = Epoch::from_persisted(2).unwrap(),
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
                        fence =
                            Tenure::granted(fence.subject().clone(), "other".into(), fence.epoch());
                    }
                    if mismatch == 2 {
                        fence = fence.reassigned(fence.holder().clone()).unwrap();
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
                                    attempt.epoch = Epoch::from_persisted(epoch).unwrap()
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
                                        Ok(TaskClaim::Acquired(Tenure::granted(
                                            task.id.clone(),
                                            "b".into(),
                                            Epoch::from_persisted(epoch + 1).unwrap(),
                                        ))),
                                        "R8"
                                    );
                                    let mut expected = before;
                                    expected.revision += 1;
                                    match &mut expected.lifecycle {
                                        BackgroundTaskLifecycle::Running { attempt }
                                        | BackgroundTaskLifecycle::Waiting { attempt, .. }
                                        | BackgroundTaskLifecycle::Cancelling { attempt, .. } => {
                                            attempt.worker_id = "b".into();
                                            attempt.epoch = attempt.epoch.next().unwrap();
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
    assert_eq!(first.epoch().get(), 1, "C1");
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
    assert_eq!(second.epoch().get(), 2, "C4");
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
    assert_eq!(recovered.epoch().get(), 2, "C4");
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
        Tenure::granted(task.id.clone(), "worker-1".into(), Epoch::FIRST)
    );
    assert!(matches!(
        task.reclaim("worker-2", 10, 10),
        Ok(TaskClaim::Acquired(tenure)) if tenure.epoch().get() == 2
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
            &Tenure::granted(fence.subject().clone(), "other".into(), fence.epoch()),
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
    let stale = Tenure::granted(
        fence.subject().clone(),
        "other".into(),
        fence.epoch().next().unwrap_or(Epoch::FIRST),
    );
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
    assert_eq!(second.epoch().get(), 2, "R4");
    assert!(matches!(
        &remote.lifecycle,
        BackgroundTaskLifecycle::Waiting {
            attempt,
            wait: BackgroundWait::Remote(continuation),
        } if attempt.tenure(second.subject()) == second && continuation == &refreshed
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
        } if attempt.tenure(first.subject()) == first && actual == &continuation
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
        } if attempt.tenure(second.subject()) == second && actual == &continuation
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
            &Tenure::granted(fence.subject().clone(), "worker-2".into(), fence.epoch()),
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

/// A task fence names its task: the same worker at the same epoch of another
/// task is a stale fence, not the owner.
///
/// | Rule | presented tenure                         | heartbeat / wait / finish |
/// | T1   | this task, worker, epoch 1               | accepted                  |
/// | T2   | another task, same worker, same epoch    | StaleFence, unchanged     |
#[test]
fn a_fence_minted_for_another_task_never_owns_this_one() {
    let mut task = task();
    let fence = task.start("worker", 0, 10, replay_policy()).unwrap();
    let foreign = Tenure::granted(
        BackgroundTaskId::new("task-2").unwrap(),
        fence.holder().clone(),
        fence.epoch(),
    );
    let before = task.clone();
    assert_eq!(
        task.heartbeat(&foreign, 1, 10),
        Err(BackgroundTaskError::StaleFence),
        "T2"
    );
    assert_eq!(
        task.wait(&foreign, BackgroundWait::Resource),
        Err(BackgroundTaskError::StaleFence),
        "T2"
    );
    assert_eq!(
        task.finish(&foreign, BackgroundTaskEnd::Cancelled),
        Err(BackgroundTaskError::StaleFence),
        "T2"
    );
    assert_eq!(task, before, "T2 unchanged");
    task.heartbeat(&fence, 1, 10).expect("T1");
}
