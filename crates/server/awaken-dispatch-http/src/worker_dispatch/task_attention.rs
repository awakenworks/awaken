// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: HTTP anti-corruption adapter: authenticates one registered Worker incarnation,
// @formal reason: binds the Task holder to it, forwards the attention to the Session
// @formal reason: application's existing admission and answers the holder's committed Task
// @formal reason: tenure from committed recovery reads. It owns no Task or Session state.
//! Durable delivery of a registered Worker's Task observations, and the
//! holder-bound read of a held Task's committed tenure.
//!
//! A BackgroundTask runs in the Worker that holds its tenure, after the Run
//! that started it has settled, so no Run claim authorizes its result. The
//! authority is the Task holder: a registered Worker's holder names exactly
//! its own (Worker, incarnation), so an authenticated Worker can deliver only
//! the observations of Tasks it holds. The Session application then admits the
//! background Run that carries the observation; the response is the durable
//! acknowledgement.
//!
//! After the Run that launched a Task settles, the Worker still checks before
//! every tool effect that the committed slot admits its attempt. No claim
//! authorizes that read either: the same holder check does, and the slot is
//! returned only when its current attempt names that holder.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::{Json, Router, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::background_task::{committed_task_holder, committed_task_slot};
use awaken_dispatch::{WorkerDirectory, WorkerIdentity};
use awaken_session_application_contract::{
    SessionRunBackgroundApplication, TaskAttention, TaskReadPartition, TaskTenureRead,
    TaskTenureRecord, WORKER_TASK_ATTENTION_PATH, WORKER_TASK_TENURE_PATH, registered_task_holder,
};
use awaken_time::{WallClock, WallClockExt};
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity,
};

// The same committed recovery read the claimed recovery route serves.
use super::{ApplicationError, RunRecoverySource, respond};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskAttentionReq {
    identity: WorkerIdentity,
    attention: TaskAttention,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskTenureReq {
    identity: WorkerIdentity,
    read: TaskTenureRead,
}

/// The Task-holder check (decision table rows A2/A3):
/// | Rule | holder | effect |
/// |---|---|---|
/// | A2 | exactly this Worker incarnation's registered holder | admitted |
/// | A3 | another Worker's or incarnation's holder, or a process holder | refused |
fn admit_task_holder(identity: &WorkerIdentity, holder: &str) -> Result<(), ApplicationError> {
    if holder == registered_task_holder(&identity.worker_id, &identity.incarnation_id) {
        Ok(())
    } else {
        Err(ApplicationError::invalid(
            "the Task attention names a Task this Worker incarnation does not hold",
        ))
    }
}

/// The committed slot a holder may read (rows H2/H3, after A2 admitted the
/// holder):
/// | Rule | committed slot of the Task | effect |
/// |---|---|---|
/// | H2 | its current attempt names the holder | the slot |
/// | H3 | absent, Requested, Ended, or another holder's attempt | none |
/// | H5 | does not decode, or names another Task | refused (internal) |
fn held_task_slot(
    state: &[StateCommand],
    read: &TaskTenureRead,
) -> Result<Option<Value>, ApplicationError> {
    let Some(slot) = committed_task_slot(state, &read.task) else {
        return Ok(None);
    };
    let holder = committed_task_holder(&read.task, &slot).map_err(ApplicationError::internal)?;
    Ok((holder.as_deref() == Some(read.holder.as_str())).then_some(slot))
}

/// The Session partition a tenure read addresses (rows P1-P3): `None` for a
/// Thread that is its own partition, the Session Thread for a child Thread.
fn partition_read(read: &TaskTenureRead) -> Result<Option<ThreadId>, ApplicationError> {
    match &read.partition {
        TaskReadPartition::Thread {} => Ok(None),
        TaskReadPartition::Session { session_thread } if session_thread == &read.thread => {
            Err(ApplicationError::invalid(
                "a Task on its Session's own Thread is read with the `thread` partition",
            ))
        }
        TaskReadPartition::Session { session_thread } => Ok(Some(ThreadId(session_thread.clone()))),
    }
}

/// Whether a committed read answered from the partition the Task was committed
/// in (row P4): the read names the Thread, and the Task's origin Run is
/// committed there. A read of another partition is refused rather than taken
/// as a Thread whose Task slot is absent.
fn admit_partition_snapshot(
    snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    thread: &ThreadId,
    run: &RunId,
) -> Result<(), ApplicationError> {
    if &snapshot.thread_id != thread {
        return Err(ApplicationError::internal(
            "committed read answered for another Thread",
        ));
    }
    match snapshot.run_record(run) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(ApplicationError::invalid(
            "the Task's origin Run is not committed in the addressed Session partition",
        )),
        Err(error) => Err(ApplicationError::internal(error.to_string())),
    }
}

/// Map the application's refusal once: a definitive refusal stays definitive
/// (the Worker stops retrying, row K4); anything else is retryable (K5).
fn attention_failure(error: awaken_session_types::RunError) -> ApplicationError {
    match error.kind {
        awaken_session_types::RunErrorKind::BadRequest => {
            ApplicationError::invalid(error.to_string())
        }
        awaken_session_types::RunErrorKind::Conflict
        | awaken_session_types::RunErrorKind::Internal
        | awaken_session_types::RunErrorKind::Unavailable => {
            ApplicationError::unavailable(error.to_string())
        }
    }
}

/// Authenticated Task control routes: attention and tenure. `attention` is
/// the Coordinator's Session Run application; `committed` its committed
/// recovery reads; `clock` judges the Worker's registered incarnation.
pub fn worker_task_control_router(
    attention: Arc<dyn SessionRunBackgroundApplication>,
    committed: Arc<dyn RunRecoverySource>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    clock: Arc<dyn WallClock>,
) -> Router {
    #[derive(Clone)]
    struct AttentionState {
        attention: Arc<dyn SessionRunBackgroundApplication>,
        committed: Arc<dyn RunRecoverySource>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WallClock>,
    }

    async fn deliver(
        State(state): State<AttentionState>,
        Extension(worker): Extension<VerifiedWorkerContext>,
        Json(request): Json<TaskAttentionReq>,
    ) -> (StatusCode, Json<Value>) {
        let result = async {
            verify_current_worker_identity(
                state.directory.as_ref(),
                &worker,
                &request.identity,
                state.clock.now_ms(),
                false,
            )
            .await
            .map_err(ApplicationError::invalid)?;
            admit_task_holder(&request.identity, &request.attention.holder)?;
            let ack = state
                .attention
                .submit_task_attention(request.attention)
                .await
                .map_err(attention_failure)?;
            Ok(json!({ "ack": ack }))
        }
        .await;
        respond(result)
    }

    async fn tenure(
        State(state): State<AttentionState>,
        Extension(worker): Extension<VerifiedWorkerContext>,
        Json(request): Json<TaskTenureReq>,
    ) -> (StatusCode, Json<Value>) {
        let result = async {
            verify_current_worker_identity(
                state.directory.as_ref(),
                &worker,
                &request.identity,
                state.clock.now_ms(),
                false,
            )
            .await
            .map_err(ApplicationError::invalid)?;
            // H4: only this incarnation's own holder is read.
            admit_task_holder(&request.identity, &request.read.holder)?;
            let thread = ThreadId(request.read.thread.clone());
            let run = RunId(request.read.run.clone());
            // P1/P2 address the partition the Task was committed in; P3 refused.
            let snapshot = match partition_read(&request.read)? {
                None => state.committed.recovery_snapshot(&thread, &run).await,
                Some(session_thread) => {
                    state
                        .committed
                        .recovery_snapshot_in_session(&session_thread, &thread, &run)
                        .await
                }
            }
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?;
            // P4: the origin Run must be committed in the addressed partition.
            admit_partition_snapshot(&snapshot, &thread, &run)?;
            let slot = held_task_slot(&snapshot.state, &request.read)?;
            Ok(json!({ "tenure": slot.map(TaskTenureRecord::new) }))
        }
        .await;
        respond(result)
    }

    let state = AttentionState {
        attention,
        committed,
        directory,
        authenticator,
        clock,
    };
    Router::new()
        .route(WORKER_TASK_ATTENTION_PATH, post(deliver))
        .route(WORKER_TASK_TENURE_PATH, post(tenure))
        .route_layer(axum::middleware::from_fn_with_state(
            state.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(worker: &str, incarnation: &str) -> WorkerIdentity {
        WorkerIdentity {
            worker_id: worker.into(),
            incarnation_id: incarnation.into(),
            generation: 1,
        }
    }

    #[test]
    fn only_the_holding_worker_incarnation_may_deliver_task_attention() {
        // A2: the exact registered holder of the authenticated incarnation.
        let current = identity("worker-a", "incarnation-1");
        assert!(
            admit_task_holder(
                &current,
                &registered_task_holder("worker-a", "incarnation-1")
            )
            .is_ok()
        );
        // A3: another Worker, a previous incarnation, a co-located process
        // holder, or an empty holder.
        for holder in [
            registered_task_holder("worker-b", "incarnation-1"),
            registered_task_holder("worker-a", "incarnation-0"),
            "background-worker-process-fingerprint".to_owned(),
            String::new(),
        ] {
            assert!(admit_task_holder(&current, &holder).is_err(), "{holder}");
        }
    }

    #[test]
    fn a_holder_reads_only_the_slot_its_current_attempt_names() {
        use awaken_agent_contract::agent::state::{Action, Key, MergePolicy, Scope};
        use awaken_agent_contract::thread::background_task::background_task_state_key;
        let holder = registered_task_holder("worker-a", "incarnation-1");
        let read = TaskTenureRead {
            thread: "thread".into(),
            partition: TaskReadPartition::Thread {},
            run: "run".into(),
            task: "task-1".into(),
            holder: holder.clone(),
        };
        let slot = |task: &str, lifecycle: Value| json!({"id": task, "lifecycle": lifecycle, "revision": 1});
        let attempt = |worker: &str| json!({"worker_id": worker, "epoch": 2, "lease_expires_at_ms": 10, "policy": {}});
        let state = |value: Value| {
            vec![StateCommand {
                key: Key(background_task_state_key("task-1")),
                scope: Scope::Thread,
                merge: MergePolicy::Exclusive,
                run_id: None,
                action: Action::Set(value),
            }]
        };
        // H2
        let held = slot(
            "task-1",
            json!({"state": "waiting", "attempt": attempt(&holder)}),
        );
        assert_eq!(
            held_task_slot(&state(held.clone()), &read).unwrap(),
            Some(held),
            "H2"
        );
        // H3
        assert_eq!(held_task_slot(&[], &read).unwrap(), None, "H3 absent");
        for lifecycle in [
            json!({"state": "requested"}),
            json!({"state": "ended", "end": {"outcome": "cancelled"}}),
            json!({"state": "running", "attempt": attempt("another-holder")}),
        ] {
            assert_eq!(
                held_task_slot(&state(slot("task-1", lifecycle.clone())), &read).unwrap(),
                None,
                "H3 {lifecycle}"
            );
        }
        // H5: a malformed slot or one naming another Task fails closed.
        for value in [
            json!("garbage"),
            slot(
                "task-2",
                json!({"state": "running", "attempt": attempt(&holder)}),
            ),
        ] {
            assert_eq!(
                held_task_slot(&state(value), &read).unwrap_err().kind,
                awaken_session_execution_contract::ApplicationErrorKind::Internal,
                "H5"
            );
        }
    }

    #[test]
    fn a_tenure_read_addresses_the_partition_its_task_was_committed_in() {
        use awaken_agent_contract::agent::run::{Record as RunRecord, RunState};
        use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
        use awaken_session_execution_contract::ApplicationErrorKind;
        let read = |thread: &str, partition: TaskReadPartition| TaskTenureRead {
            thread: thread.into(),
            partition,
            run: "origin".into(),
            task: "task".into(),
            holder: "holder".into(),
        };
        // P1 root Thread, P2 child Thread in a Session, P3 a Session naming the
        // Thread itself.
        assert_eq!(
            partition_read(&read("root", TaskReadPartition::Thread {})).unwrap(),
            None,
            "P1"
        );
        assert_eq!(
            partition_read(&read(
                "child",
                TaskReadPartition::Session {
                    session_thread: "session".into()
                }
            ))
            .unwrap(),
            Some(ThreadId("session".into())),
            "P2"
        );
        assert_eq!(
            partition_read(&read(
                "root",
                TaskReadPartition::Session {
                    session_thread: "root".into()
                }
            ))
            .unwrap_err()
            .kind,
            ApplicationErrorKind::InvalidRequest,
            "P3"
        );
        // P4: the origin Run must be committed in the answered partition.
        let snapshot = |thread: &str, runs: Vec<RunRecord>| RunRecoverySnapshot {
            thread_id: ThreadId(thread.into()),
            claimed_run_id: RunId("origin".into()),
            runs,
            latest_run_id: None,
            messages: Vec::new(),
            message_commit_cursors: Vec::new(),
            commit_timestamps: Default::default(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 0,
            store_cursor: 0,
            next_commit_ordinal: 0,
        };
        let origin = RunRecord {
            id: RunId("origin".into()),
            thread_id: ThreadId("child".into()),
            state: RunState::Running,
        };
        let child = ThreadId("child".into());
        let run = RunId("origin".into());
        assert!(
            admit_partition_snapshot(&snapshot("child", vec![origin]), &child, &run).is_ok(),
            "P4 the Task's partition"
        );
        assert_eq!(
            admit_partition_snapshot(&snapshot("child", Vec::new()), &child, &run)
                .unwrap_err()
                .kind,
            ApplicationErrorKind::InvalidRequest,
            "P4 another partition answers empty: refused, not an absent slot"
        );
        assert_eq!(
            admit_partition_snapshot(&snapshot("other", Vec::new()), &child, &run)
                .unwrap_err()
                .kind,
            ApplicationErrorKind::Internal,
            "another Thread"
        );
    }

    #[test]
    fn a_definitive_application_refusal_stays_definitive() {
        // K4 BadRequest stays a refusal; K5 every other kind is retryable.
        use awaken_session_types::RunError;
        let invalid = attention_failure(RunError::bad_request("gone"));
        assert_eq!(
            invalid.kind,
            awaken_session_execution_contract::ApplicationErrorKind::InvalidRequest
        );
        for error in [RunError::unavailable("busy"), RunError::internal("store")] {
            assert_eq!(
                attention_failure(error).kind,
                awaken_session_execution_contract::ApplicationErrorKind::Unavailable
            );
        }
    }
}
