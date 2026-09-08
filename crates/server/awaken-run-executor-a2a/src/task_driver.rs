//! The one owned A2A observation/cancellation path; never spawn cleanup on drop.

use std::future::Future;
use std::sync::Arc;

use awaken_agent_contract::agent::run::RunState;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_protocol_a2a::client::{self as a2a, Transport};
use awaken_protocol_a2a::{Task, TaskState};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result, verify_attempt_ownership};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use super::{TaskReference, validate_reattached_task};

const MAX_TASK_POLLS: usize = 600;
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Adapt this protocol to the existing attempt-local veto. This encloses the
/// send/reattach, task-reference commit and observation, but not the final
/// boundary commit: losing that commit cannot undo an observed remote boundary.
/// Protocol errors carry no positive no-dispatch/termination evidence. Preserve
/// their diagnostic while vetoing ACK before returning; drop/unwind uses the
/// same Runtime observer as Native, MCP and Hand, not an A2A cleanup task.
pub(super) async fn observe_remote_operation<T>(
    context: &RuntimeRunContext,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    context
        .physical_quiescence
        .observe_return(async {
            let result = operation.await;
            if result.is_err() {
                context.physical_quiescence.mark_uncertain();
            }
            result
        })
        .await
}

/// Cancellation delivery is not a stop receipt. Reobserve the exact task even
/// after a lost cancel response; only its terminal state acknowledges stop.
/// A waiting task is a normal execution boundary, but is NOT a terminal cancel.
pub(super) async fn cancel_and_observe(
    transport: &dyn Transport,
    reference: &TaskReference,
    context: &RuntimeRunContext,
) -> Result<Task> {
    verify_attempt_ownership(context.ownership.as_deref()).await?;
    let delivery = a2a::try_cancel_task(transport, &reference.task_id).await;
    verify_attempt_ownership(context.ownership.as_deref()).await?;
    let observed = a2a::get_task(transport, &reference.task_id).await;
    if let Ok(task) = &observed {
        validate_reattached_task(reference, task)?;
        if task.status.state.is_terminal() {
            return Ok(observed.expect("matched successful observation"));
        }
    }
    // Keep the delivery failure when neither response proves termination.
    delivery.map_err(|error| Error::Execution(error.to_string()))?;
    observed.map_err(|error| Error::Execution(error.to_string()))?;
    Err(Error::Execution(
        "remote A2A cancellation has no exact terminal observation".to_string(),
    ))
}

/// Root and delegated Runs use the same bounded task driver. All GETs remain
/// bound to the initial identity, including every poll (not only reattachment).
pub(super) async fn poll_to_boundary(
    transport: Arc<dyn Transport>,
    reference: &TaskReference,
    mut task: Task,
    context: &RuntimeRunContext,
) -> Result<Task> {
    for poll in 0..=MAX_TASK_POLLS {
        validate_reattached_task(reference, &task)?;
        if !matches!(
            task.status.state,
            TaskState::Submitted | TaskState::Working | TaskState::Unknown
        ) {
            return Ok(task);
        }
        if context
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return cancel_and_observe(transport.as_ref(), reference, context).await;
        }
        if poll == MAX_TASK_POLLS {
            return Err(Error::Execution(
                "remote A2A task did not reach a boundary in time".to_string(),
            ));
        }
        match &context.cancellation {
            Some(token) => {
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                    _ = token.cancelled() => {
                        return cancel_and_observe(transport.as_ref(), reference, context).await;
                    }
                }
            }
            None => tokio::time::sleep(POLL_INTERVAL).await,
        }
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        task = a2a::get_task(transport.as_ref(), &reference.task_id)
            .await
            .map_err(|error| Error::Execution(error.to_string()))?;
    }
    unreachable!("bounded A2A poll loop always returns")
}

/// Commit only after the owned remote operation returned a validated boundary.
pub(super) async fn finish_task(
    endpoint: &str,
    activation: &RunActivation,
    context: &RuntimeRunContext,
    task: Task,
) -> Result<RunState> {
    let reply = super::task_reply(&task);
    let messages = (!reply.is_empty())
        .then(|| super::assistant_message(context, activation, reply))
        .into_iter()
        .collect();
    match task.status.state {
        TaskState::InputRequired | TaskState::AuthRequired => {
            let reference = TaskReference::from_task(endpoint, &task);
            super::commit_boundary(
                context,
                activation,
                RunDisposition::awaiting(super::awaiting_ticket(activation, &task)),
                messages,
                vec![super::task_reference_state(&reference)?],
            )
            .await?;
            Ok(RunState::Awaiting)
        }
        TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected => {
            super::finish_terminal(
                context,
                activation,
                messages,
                super::end_cause_of(&task.status.state),
            )
            .await
        }
        TaskState::Submitted | TaskState::Working | TaskState::Unknown => {
            unreachable!("the shared task driver returns only a boundary")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{
        OwnershipDecision, Rec, ScriptedOwnership, ScriptedTransport, activation, response,
        scripted_executor,
    };
    use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
    use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
    use awaken_protocol_a2a::client::Response;
    use awaken_runtime_contract::CancellationToken;
    use awaken_runtime_contract::execution::RunExecutor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn task(state: TaskState) -> Task {
        serde_json::from_value(serde_json::json!({
            "kind": "task", "id": "task-a", "contextId": "context-a",
            "status": { "state": state },
        }))
        .unwrap()
    }

    fn reference() -> TaskReference {
        TaskReference::from_task("http://remote.invalid", &task(TaskState::Working))
    }

    #[tokio::test]
    async fn cancel_confirmation_crosses_delivery_identity_and_every_task_state() {
        // C1 delivery={204,lost}; C2 observation covers all nine TaskStates;
        // C3 identity={exact,foreign task,foreign context}. E1 verified return,
        // E2 veto+error. Complete graph: C3 && terminal(C2) => E1 regardless
        // of C1; every other row => E2. One cancel then one GET, both task A;
        // no creation/retry, no mutation of independent B or clearing old veto.
        // Remote truth/authenticity is the A2A adapter's TCB, not proved here.
        for delivered in [true, false] {
            for state in [
                TaskState::Submitted,
                TaskState::Working,
                TaskState::Unknown,
                TaskState::InputRequired,
                TaskState::AuthRequired,
                TaskState::Completed,
                TaskState::Failed,
                TaskState::Canceled,
                TaskState::Rejected,
            ] {
                for identity in 0..3 {
                    let mut observed = task(state);
                    if identity == 1 {
                        observed.id = "task-b".into();
                    }
                    if identity == 2 {
                        observed.context_id = "context-b".into();
                    }
                    let transport = ScriptedTransport::new(vec![
                        if delivered {
                            Ok(Response::new(204, Vec::new()))
                        } else {
                            Err("cancel response lost".into())
                        },
                        response(&serde_json::to_string(&observed).unwrap()),
                    ]);
                    let context = RuntimeRunContext::new();
                    let other = RuntimeRunContext::new();
                    let result = observe_remote_operation(
                        &context,
                        cancel_and_observe(&transport, &reference(), &context),
                    )
                    .await;
                    let confirmed = identity == 0 && state.is_terminal();
                    assert_eq!(
                        result.is_ok(),
                        confirmed,
                        "{delivered}/{state:?}/{identity}"
                    );
                    assert_eq!(
                        context.physical_quiescence.can_acknowledge_return(),
                        confirmed
                    );
                    assert!(other.physical_quiescence.can_acknowledge_return());
                    let seen = transport.seen.lock().unwrap();
                    assert_eq!(seen.len(), 2);
                    assert_eq!(
                        (&*seen[0].0, &*seen[0].1),
                        ("POST", "/v1/a2a/tasks/task-a:cancel")
                    );
                    assert_eq!((&*seen[1].0, &*seen[1].1), ("GET", "/v1/a2a/tasks/task-a"));
                }
            }
        }
    }

    #[tokio::test]
    async fn failed_terminal_observation_and_old_veto_never_become_stop_proof() {
        // R1 cancel 204 + GET lost/404/malformed => error+veto. R2 old veto +
        // exact terminal GET => operation succeeds but old veto is monotonic.
        // R3 lost cancel + lost GET => error, no automatic second cancel.
        for observation in [
            Err("observation lost".into()),
            Ok(Response::new(404, Vec::new())),
            response("malformed"),
        ] {
            let context = RuntimeRunContext::new();
            let transport =
                ScriptedTransport::new(vec![Ok(Response::new(204, Vec::new())), observation]);
            assert!(
                observe_remote_operation(
                    &context,
                    cancel_and_observe(&transport, &reference(), &context),
                )
                .await
                .is_err()
            );
            assert!(!context.physical_quiescence.can_acknowledge_return());
            assert_eq!(transport.seen.lock().unwrap().len(), 2);
        }
        let context = RuntimeRunContext::new();
        context.physical_quiescence.mark_uncertain();
        let transport = ScriptedTransport::new(vec![
            Err("cancel response lost".into()),
            response(&serde_json::to_string(&task(TaskState::Canceled)).unwrap()),
        ]);
        assert!(
            observe_remote_operation(
                &context,
                cancel_and_observe(&transport, &reference(), &context),
            )
            .await
            .is_ok()
        );
        assert!(!context.physical_quiescence.can_acknowledge_return(), "R2");
        let transport =
            ScriptedTransport::new(vec![Err("cancel lost".into()), Err("get lost".into())]);
        assert!(
            observe_remote_operation(
                &context,
                cancel_and_observe(&transport, &reference(), &context),
            )
            .await
            .is_err()
        );
        assert_eq!(transport.seen.lock().unwrap().len(), 2, "R3");
    }

    #[tokio::test]
    async fn cancellation_rechecks_authority_without_a_drop_side_channel() {
        // C1 lost/unavailable before cancel => zero requests; C2 authority
        // lost after cancel => only POST, no GET or false completion. Both
        // keep the existing remote participant uncertain. Current/current is
        // covered by the terminal-observation table, not a second fixture.
        for decisions in [
            vec![OwnershipDecision::Lost],
            vec![OwnershipDecision::Unavailable],
            vec![OwnershipDecision::Current, OwnershipDecision::Lost],
        ] {
            let expected_calls = decisions.len() - 1;
            let context = RuntimeRunContext::new()
                .with_ownership(Arc::new(ScriptedOwnership::new(decisions)));
            let transport = ScriptedTransport::new(vec![Ok(Response::new(204, Vec::new()))]);
            assert!(
                observe_remote_operation(
                    &context,
                    cancel_and_observe(&transport, &reference(), &context),
                )
                .await
                .is_err()
            );
            assert!(!context.physical_quiescence.can_acknowledge_return());
            assert_eq!(transport.seen.lock().unwrap().len(), expected_calls);
            tokio::task::yield_now().await;
            assert_eq!(
                transport.seen.lock().unwrap().len(),
                expected_calls,
                "no spawned retry"
            );
        }
    }

    #[tokio::test]
    async fn every_poll_preserves_the_initial_task_and_context() {
        // C1 initial task A working; C2 next GET swaps id or context to B
        // while claiming Completed. Both => rejection+veto; never accept B's
        // terminal state, poll/cancel B, or rewrite the committed reference.
        for change_task in [true, false] {
            let mut foreign = task(TaskState::Completed);
            if change_task {
                foreign.id = "task-b".into();
            } else {
                foreign.context_id = "context-b".into();
            }
            let transport = Arc::new(ScriptedTransport::new(vec![
                response(&serde_json::to_string(&task(TaskState::Working)).unwrap()),
                response(&serde_json::to_string(&foreign).unwrap()),
            ]));
            let rec = Arc::new(Rec::default());
            let context = RuntimeRunContext::new()
                .with_commit(rec.clone())
                .with_reader(rec.clone());
            let latch = context.physical_quiescence.clone();
            let activation = activation("a2a:http://remote.invalid");
            assert!(
                scripted_executor(transport.clone())
                    .execute(activation.clone(), context.clone())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("identity changed")
            );
            assert!(!latch.can_acknowledge_return());
            let retained = crate::restored_task_reference(&context, &activation)
                .unwrap()
                .unwrap();
            assert_eq!(retained.task_id, "task-a");
            assert_eq!(retained.context_id, "context-a");
            let seen = transport.seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[1].1, "/v1/a2a/tasks/task-a");
        }
    }

    #[tokio::test]
    async fn dropping_an_owned_remote_observer_only_vetoes_its_attempt() {
        // R1 unpolled => no send, no veto. R2 polled while remote A working
        // then drop, with/without cancellation => veto A; no detached cancel
        // or background task and no effect on B. R3 retained cancellation
        // and terminal paths are covered by the executor/cancel matrix.
        for cancel in [false, true] {
            let transport = Arc::new(ScriptedTransport::new(vec![response(
                &serde_json::to_string(&task(TaskState::Working)).unwrap(),
            )]));
            let executor = scripted_executor(transport.clone());
            let token = CancellationToken::new();
            let context = RuntimeRunContext::new()
                .with_commit(Arc::new(Rec::default()))
                .with_cancellation(token.clone());
            let other = RuntimeRunContext::new();
            let latch = context.physical_quiescence.clone();
            drop(executor.execute(activation("a2a:http://remote.invalid"), context.clone()));
            assert!(latch.can_acknowledge_return(), "R1");
            assert!(transport.seen.lock().unwrap().is_empty(), "R1");
            let mut operation =
                Box::pin(executor.execute(activation("a2a:http://remote.invalid"), context));
            std::future::poll_fn(|cx| {
                assert!(operation.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            if cancel {
                token.cancel();
            }
            drop(operation);
            tokio::task::yield_now().await;
            assert!(!latch.can_acknowledge_return(), "R2");
            assert!(
                other.physical_quiescence.can_acknowledge_return(),
                "R2 independent B"
            );
            assert_eq!(
                transport.seen.lock().unwrap().len(),
                1,
                "R2 no cancellation side channel"
            );
        }
    }

    struct FailCommit(AtomicUsize, usize);
    #[async_trait::async_trait]
    impl Coordinator for FailCommit {
        async fn commit(
            &self,
            _commit: ThreadCommit,
        ) -> std::result::Result<CommitRecord, CommitError> {
            let ordinal = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            if ordinal == self.1 {
                Err(CommitError::Rejected("commit response lost".into()))
            } else {
                Ok(CommitRecord {
                    sequence: ordinal as u64,
                })
            }
        }
    }

    #[tokio::test]
    async fn commit_failure_does_not_replace_remote_execution_evidence() {
        // R1 working task + reference commit fails => error+veto (task may
        // continue, no GET/re-send). R2 terminal observed + final commit fails
        // => commit error WITHOUT inventing remote uncertainty; retry belongs
        // to the existing commit owner, not execution. One send in either row.
        for (state, failed_commit, can_ack) in [
            (TaskState::Working, 1, false),
            (TaskState::Completed, 2, true),
        ] {
            let transport = Arc::new(ScriptedTransport::new(vec![response(
                &serde_json::to_string(&task(state)).unwrap(),
            )]));
            let context = RuntimeRunContext::new()
                .with_commit(Arc::new(FailCommit(AtomicUsize::new(0), failed_commit)));
            let latch = context.physical_quiescence.clone();
            let error = scripted_executor(transport.clone())
                .execute(activation("a2a:http://remote.invalid"), context)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Commit(_)));
            assert_eq!(latch.can_acknowledge_return(), can_ack);
            assert_eq!(transport.seen.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_observation_budget_retains_one_remote_task_without_resending() {
        // C1 every GET is exact but Working through the existing 600-poll
        // budget => timeout+veto; one creation, 600 GETs, no cancel, no second
        // creation or terminal commit. Virtual time changes only scheduling,
        // not the production loop or its bound; transport/OS deadlines are
        // outside this bounded observer test.
        let body = serde_json::to_string(&task(TaskState::Working)).unwrap();
        let transport = Arc::new(ScriptedTransport::new(
            (0..=MAX_TASK_POLLS).map(|_| response(&body)).collect(),
        ));
        let rec = Arc::new(Rec::default());
        let context = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        let latch = context.physical_quiescence.clone();
        let error = scripted_executor(transport.clone())
            .execute(activation("a2a:http://remote.invalid"), context)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("did not reach a boundary"));
        assert!(!latch.can_acknowledge_return());
        assert_eq!(
            rec.0.lock().unwrap().len(),
            1,
            "reference only, no false terminal"
        );
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen.len(), MAX_TASK_POLLS + 1);
        assert_eq!(seen.iter().filter(|request| request.0 == "POST").count(), 1);
        assert!(
            seen[1..]
                .iter()
                .all(|request| request.1 == "/v1/a2a/tasks/task-a")
        );
    }

    #[tokio::test]
    async fn resolution_failure_distinguishes_fresh_work_from_an_existing_remote_task() {
        // R1 fresh + resolver rejects before dispatch => no veto.
        // R2 committed remote task + resolver rejects => veto (failed
        // credentials do not prove the task stopped). Existing route identity
        // and exact TaskReference remain the recovery owner in both rows.
        let executor =
            crate::A2aRunExecutor::new(Arc::new(crate::tests::RejectingTransportResolver));
        let fresh = RuntimeRunContext::new();
        assert!(
            executor
                .execute(activation("a2a:http://remote.invalid"), fresh.clone())
                .await
                .is_err()
        );
        assert!(fresh.physical_quiescence.can_acknowledge_return(), "R1");
        let rec = Arc::new(Rec::default());
        let initial = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(&serde_json::to_string(&task(TaskState::Working)).unwrap()),
            Err("initial observer lost".into()),
        ]));
        let activation = activation("a2a:http://remote.invalid");
        assert!(
            scripted_executor(transport)
                .execute(activation.clone(), initial)
                .await
                .is_err()
        );
        let recovered = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec);
        assert!(
            executor
                .execute(activation.clone(), recovered.clone())
                .await
                .is_err()
        );
        assert!(
            !recovered.physical_quiescence.can_acknowledge_return(),
            "R2"
        );
        assert_eq!(
            crate::restored_task_reference(&recovered, &activation)
                .unwrap()
                .unwrap()
                .task_id,
            "task-a"
        );
    }

    #[tokio::test]
    async fn resumed_send_failure_preserves_the_previous_reference_and_vetoes_ack() {
        // R1 an exact awaiting ticket resumes with a stable message identity,
        // then its send response is lost => error+veto; retain the committed
        // previous task/context, never repeat send inside this drive.
        use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
        use awaken_runtime_contract::execution::RunAttemptExecutor;
        use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
        let rec = Arc::new(Rec::default());
        let transport = Arc::new(ScriptedTransport::new(vec![
            response(&serde_json::to_string(&task(TaskState::InputRequired)).unwrap()),
            Err("resume response lost".into()),
        ]));
        let activation = activation("a2a:http://remote.invalid");
        let initial = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec.clone());
        assert_eq!(
            scripted_executor(transport.clone())
                .execute(activation.clone(), initial)
                .await
                .unwrap(),
            RunState::Awaiting
        );
        let ticket = rec.resume_ticket(&activation.run_id).unwrap();
        let command = ResumeCommand::from_ticket(&ticket, ResumeResult::Input("answer".into()), 0);
        let resumed = RuntimeRunContext::new()
            .with_commit(rec.clone())
            .with_reader(rec);
        assert!(
            scripted_executor(transport.clone())
                .resume(activation.clone(), command, resumed.clone())
                .await
                .is_err()
        );
        assert!(!resumed.physical_quiescence.can_acknowledge_return());
        assert_eq!(
            crate::restored_task_reference(&resumed, &activation)
                .unwrap()
                .unwrap()
                .task_id,
            "task-a"
        );
        assert_eq!(transport.seen.lock().unwrap().len(), 2);
    }
}
