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
fn non_replayable_expiry_and_remote_wait_fail_closed() {
    // Boundary partitions: expired non-replayable ownership becomes
    // Indeterminate; an incomplete remote continuation cannot enter durable
    // Waiting; an exact MCP continuation can. No empty string is interpreted as
    // an external identity during recovery.
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
        .start("worker", 0, 10, replay_policy())
        .expect("fresh remote task is claimable");
    assert_eq!(
        remote.wait(
            &fence,
            BackgroundWait::Remote(RemoteContinuation {
                protocol: RemoteProtocol::Mcp,
                server_binding: String::new(),
                task_id: "remote-1".into()
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
            }),
        )
        .expect("complete remote identity may wait");
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
