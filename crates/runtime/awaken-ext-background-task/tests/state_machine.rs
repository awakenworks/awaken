use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_ext_background_task::{
    BackgroundInvocation, BackgroundTask, BackgroundTaskEnd, BackgroundTaskError, BackgroundTaskId,
    BackgroundTaskLifecycle, BackgroundTaskOrigin, BackgroundWait, RemoteContinuation,
    RemoteProtocol, TaskClaim, TaskExecutionPolicy, TaskFence,
};
use awaken_runtime_contract::tool::{
    ToolCall, ToolConcurrency, ToolRecoveryMode, ToolRecoveryPolicy,
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
            BackgroundWait::Remote(RemoteContinuation {
                protocol: RemoteProtocol::Mcp,
                server_binding: String::new(),
                task_id: "remote-1".into(),
                poll_interval_ms: Some(100),
            })
        ),
        Err(BackgroundTaskError::InvalidContinuation)
    );
    remote
        .wait(
            &fence,
            BackgroundWait::Remote(RemoteContinuation {
                protocol: RemoteProtocol::Mcp,
                server_binding: "mcp-server".into(),
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
            BackgroundWait::Remote(RemoteContinuation {
                protocol: RemoteProtocol::Mcp,
                server_binding: "mcp-server".into(),
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
    // Failure-mode effects analysis:
    // C1 clock overflow; C2 cancellation revision overflow; C3 invalid wait;
    // C4 stale finish; C5 heartbeat revision overflow; C6 empty durable call
    // identity; C7 impossible Requested revision. E1 every transition error is
    // a stutter; E2 malformed persisted aggregates are rejected. This prevents
    // callers from accidentally committing a half-applied mutation.
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
            BackgroundWait::Remote(RemoteContinuation {
                protocol: RemoteProtocol::A2a,
                server_binding: String::new(),
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
        "C4/E1"
    );
    assert_eq!(active, before, "C4/E1");

    active.revision = u64::MAX;
    let before = active.clone();
    assert_eq!(
        active.heartbeat(&fence, 10, 10),
        Err(BackgroundTaskError::RevisionOverflow),
        "C5/E1"
    );
    assert_eq!(active, before, "C5/E1");

    let mut mismatched_call = serde_json::to_value(task()).expect("task serializes");
    mismatched_call["invocation"]["call"]["call_id"] = serde_json::json!("");
    assert!(
        serde_json::from_value::<BackgroundTask>(mismatched_call).is_err(),
        "C6/E2"
    );

    let mut impossible_revision = serde_json::to_value(task()).expect("task serializes");
    impossible_revision["revision"] = serde_json::json!(1);
    assert!(
        serde_json::from_value::<BackgroundTask>(impossible_revision).is_err(),
        "C7/E2"
    );
}

#[test]
fn remote_wait_reclaim_preserves_coordinates_and_fences_stale_observers() {
    // Cause/effect decision table:
    // R1 Running + complete remote coordinates -> Waiting with the same fence.
    // R2 Waiting + same remote identity + a new positive poll interval -> only
    //    the interval and aggregate revision advance.
    // R3 Waiting + changed binding/task identity or zero interval -> reject and
    //    leave the aggregate byte-for-byte unchanged.
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
    let initial = RemoteContinuation {
        protocol: RemoteProtocol::Mcp,
        server_binding: "mcp/server@generation-7".into(),
        task_id: "remote-1".into(),
        poll_interval_ms: Some(100),
    };
    remote
        .wait(&first, BackgroundWait::Remote(initial.clone()))
        .expect("R1 complete continuation is durable");

    let refreshed = RemoteContinuation {
        poll_interval_ms: Some(250),
        ..initial.clone()
    };
    let revision = remote.revision;
    remote
        .wait(&first, BackgroundWait::Remote(refreshed.clone()))
        .expect("R2 the same remote request may refresh its poll delay");
    assert_eq!(remote.revision, revision + 1, "R2");

    for invalid in [
        RemoteContinuation {
            server_binding: "mcp/server@generation-8".into(),
            ..refreshed.clone()
        },
        RemoteContinuation {
            task_id: "remote-2".into(),
            ..refreshed.clone()
        },
        RemoteContinuation {
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
    let continuation = RemoteContinuation {
        protocol: RemoteProtocol::Mcp,
        server_binding: "mcp/server@generation-7".into(),
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

    let continuation = RemoteContinuation {
        protocol: RemoteProtocol::Mcp,
        server_binding: "mcp/server@generation-7".into(),
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
            BackgroundWait::Remote(RemoteContinuation {
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
    // Compatibility partition: historical Cancelling values did not carry a
    // wait field and historical Remote values did not carry a poll interval.
    // Both absences decode as None; a present incomplete remote wait is
    // rejected. This preserves the old wire without weakening new recovery
    // coordinates.
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
            BackgroundWait::Remote(RemoteContinuation {
                protocol: RemoteProtocol::Mcp,
                server_binding: "mcp-server".into(),
                task_id: "remote-1".into(),
                poll_interval_ms: Some(100),
            }),
        )
        .expect("fixture waits");
    let mut legacy_wait = serde_json::to_value(&waiting).expect("task serializes");
    legacy_wait["lifecycle"]["wait"]
        .as_object_mut()
        .expect("remote wait is an object")
        .remove("poll_interval_ms");
    let decoded_wait = serde_json::from_value::<BackgroundTask>(legacy_wait)
        .expect("historical remote wait remains readable");
    assert!(matches!(
        decoded_wait.lifecycle,
        BackgroundTaskLifecycle::Waiting {
            wait: BackgroundWait::Remote(RemoteContinuation {
                poll_interval_ms: None,
                ..
            }),
            ..
        }
    ));

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
