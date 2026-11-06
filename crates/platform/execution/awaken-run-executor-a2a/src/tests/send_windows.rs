// @formal evidence remote_attempt.prepared_operation_precedes_send_and_blocks_unknown_replay
//! Observe the real executor at the existing commit and transport seams.

use super::*;

pub(super) type RequestObserver = dyn Fn(&str, Option<&[u8]>) + Send + Sync;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    RejectPrepared,
    LosePreparedReply,
    LoseSendReply,
    RejectAccepted,
    LoseAcceptedReply,
    Delivered,
}

struct CommitCut {
    recorded: Arc<Rec>,
    cut: Mutex<Option<Cut>>,
}

#[async_trait]
impl Coordinator for CommitCut {
    async fn commit(&self, commit: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
        // Delegate durable state to the existing Rec; inject only an observed
        // commit-return cut, with no second repository or transition logic.
        let phase = commit.state.iter().find_map(|command| {
            if command.key != clear_remote_operation_state().key {
                return None;
            }
            match &command.action {
                awaken_agent_contract::agent::state::Action::Set(value) => {
                    value.get("phase").and_then(serde_json::Value::as_str)
                }
                _ => None,
            }
        });
        let cut = {
            let mut pending = self.cut.lock().unwrap();
            let matches = matches!(
                (*pending, phase),
                (
                    Some(Cut::RejectPrepared | Cut::LosePreparedReply),
                    Some("prepared")
                ) | (
                    Some(Cut::RejectAccepted | Cut::LoseAcceptedReply),
                    Some("accepted")
                )
            );
            matches.then(|| pending.take().unwrap())
        };
        if matches!(cut, Some(Cut::RejectPrepared | Cut::RejectAccepted)) {
            return Err(CommitError::Rejected("injected before commit".into()));
        }
        let receipt = self.recorded.commit(commit).await?;
        if matches!(cut, Some(Cut::LosePreparedReply | Cut::LoseAcceptedReply)) {
            return Err(CommitError::Rejected("injected lost commit reply".into()));
        }
        Ok(receipt)
    }
}

fn waiting_task(id: &str, envelope: bool) -> std::result::Result<Response, String> {
    let task = serde_json::json!({
        "kind": "task", "id": id, "contextId": "ctx", "status": {
            "state": "input-required", "message": {
                "kind": "message", "messageId": format!("reply-{id}"), "role": "agent",
                "parts": [{"kind": "text", "text": format!("question-{id}")}]
            }
        }
    });
    response(
        &if envelope {
            serde_json::json!({"task": task})
        } else {
            task
        }
        .to_string(),
    )
}

async fn invoke(
    executor: &A2aRunExecutor,
    activation: &RunActivation,
    resume: Option<&ResumeCommand>,
    context: RuntimeRunContext,
) -> Result<RunState> {
    match resume {
        Some(command) => {
            executor
                .resume(activation.clone(), command.clone(), context)
                .await
        }
        None => executor.execute(activation.clone(), context).await,
    }
}

#[tokio::test]
async fn prepared_send_and_acceptance_windows_preserve_exact_operation_authority() {
    // Causes: C1 fresh/Resume; C2 Prepared commit rejected/committed-lost;
    // C3 send response lost; C4 Accepted commit rejected/committed-lost;
    // C5 all delivered; C6 recovery execute/cancel/repeated Resume.
    // R1 rejected Prepared -> zero send, unchanged previous state, retry allowed.
    // R2 committed Prepared/lost reply -> zero send, unknown, C6 zero IO.
    // R3 send loss or rejected Accepted -> one send, unknown, C6 zero IO.
    // R4 committed Accepted/lost reply -> one send, exact-task GET recovery.
    // R5 delivered -> one send, Awaiting, next distinct Resume allowed.
    // Every send observes its exact Prepared in Rec BEFORE transport entry,
    // matches the actual wire, and occurs once per operation/message identity.
    // Both C1 values cross all six cuts. No wall-clock sleeps or peer dedup
    // stand in for commit/IO evidence; pre-dispatch transport resolution is not
    // an outbound send. Existing tests own malformed state and endpoint denial.
    for is_resume in [false, true] {
        for cut in [
            Cut::RejectPrepared,
            Cut::LosePreparedReply,
            Cut::LoseSendReply,
            Cut::RejectAccepted,
            Cut::LoseAcceptedReply,
            Cut::Delivered,
        ] {
            let activation = activation("a2a:http://remote.invalid");
            let rec = Arc::new(Rec::default());
            let commits = Arc::new(CommitCut {
                recorded: rec.clone(),
                cut: Mutex::new(None),
            });
            let context = || {
                RuntimeRunContext::new()
                    .with_commit(commits.clone())
                    .with_reader(rec.clone())
            };
            let sends = Arc::new(Mutex::new(Vec::<RemoteOperationIntent>::new()));
            let observed_rec = rec.clone();
            let observed_activation = activation.clone();
            let observed_sends = sends.clone();
            let mut scripted = ScriptedTransport::new(vec![]);
            scripted.before_request = Some(Arc::new(move |path, body| {
                if path != "/v1/a2a/message:send" {
                    return;
                }
                let read = RuntimeRunContext::new().with_reader(observed_rec.clone());
                let Some(RemoteOperationState::Prepared(intent)) =
                    restored_remote_operation(&read, &observed_activation).unwrap()
                else {
                    panic!("send must observe its committed Prepared operation");
                };
                let wire: serde_json::Value = serde_json::from_slice(body.unwrap()).unwrap();
                assert_eq!(
                    wire["message"]["messageId"], intent.message_id,
                    "send uses committed message identity"
                );
                assert_eq!(
                    wire["message"]["contextId"], intent.context_id,
                    "send uses committed context"
                );
                let input = wire["message"]["parts"][0]["text"].as_str().unwrap();
                assert_eq!(
                    prepare_remote_operation(
                        &intent.endpoint,
                        &intent.context_id,
                        intent.predecessor_task_id.as_deref(),
                        &intent.message_id,
                        input
                    )
                    .unwrap(),
                    intent,
                    "send payload retains the complete committed operation identity"
                );
                let mut sent = observed_sends.lock().unwrap();
                assert!(
                    sent.iter()
                        .all(|prior| prior.operation_id != intent.operation_id
                            && prior.message_id != intent.message_id),
                    "one send per exact prepared operation"
                );
                sent.push(intent);
            }));
            let transport = Arc::new(scripted);
            let executor = scripted_executor(transport.clone());
            let resume = if is_resume {
                transport
                    .replies
                    .lock()
                    .unwrap()
                    .push_back(waiting_task("seed", true));
                assert_eq!(
                    executor
                        .execute(activation.clone(), context())
                        .await
                        .unwrap(),
                    RunState::Awaiting
                );
                Some(ResumeCommand::from_ticket(
                    &rec.resume_ticket(&activation.run_id).unwrap(),
                    ResumeResult::Input("answer-one".into()),
                    0,
                ))
            } else {
                None
            };
            let before = restored_remote_operation(&context(), &activation).unwrap();
            let before_commits = rec.0.lock().unwrap().len();
            let before_requests = transport.seen.lock().unwrap().len();
            let before_sends = sends.lock().unwrap().len();
            *commits.cut.lock().unwrap() = Some(cut);
            transport
                .replies
                .lock()
                .unwrap()
                .push_back(if cut == Cut::LoseSendReply {
                    Err("injected send response loss".into())
                } else {
                    waiting_task("target", true)
                });
            let result = invoke(&executor, &activation, resume.as_ref(), context()).await;
            assert_eq!(
                result.is_ok(),
                cut == Cut::Delivered,
                "{is_resume:?}/{cut:?}: return cut"
            );
            let sent = usize::from(!matches!(cut, Cut::RejectPrepared | Cut::LosePreparedReply));
            assert_eq!(
                transport.seen.lock().unwrap().len() - before_requests,
                sent,
                "cut controls exact send count"
            );
            assert_eq!(sends.lock().unwrap().len() - before_sends, sent);
            let restored = restored_remote_operation(&context(), &activation).unwrap();
            match cut {
                Cut::RejectPrepared => {
                    assert_eq!(restored, before, "rejected Prepared stutters");
                    assert_eq!(rec.0.lock().unwrap().len(), before_commits);
                    assert_eq!(
                        invoke(&executor, &activation, resume.as_ref(), context())
                            .await
                            .unwrap(),
                        RunState::Awaiting,
                        "rejected pre-send commit permits one retry"
                    );
                }
                Cut::LosePreparedReply | Cut::LoseSendReply | Cut::RejectAccepted => {
                    let Some(RemoteOperationState::Prepared(intent)) = restored else {
                        panic!("unknown retains Prepared");
                    };
                    assert_eq!(
                        intent.predecessor_task_id.as_deref(),
                        is_resume.then_some("seed")
                    );
                    let count = transport.seen.lock().unwrap().len();
                    let committed = rec.0.lock().unwrap().len();
                    for _ in 0..2 {
                        let retry = context();
                        let quiescence = retry.physical_quiescence.clone();
                        assert!(
                            executor.execute(activation.clone(), retry).await.is_err(),
                            "unknown execute must reject"
                        );
                        assert!(
                            !quiescence.can_acknowledge_return(),
                            "unknown execute cannot acknowledge quiescence"
                        );
                        assert!(
                            executor
                                .cancel(activation.clone(), context())
                                .await
                                .is_err(),
                            "unknown cancel must reject"
                        );
                        assert_eq!(
                            transport.seen.lock().unwrap().len(),
                            count,
                            "unknown recovery has zero outbound IO"
                        );
                        if let Some(command) = &resume {
                            assert!(
                                executor
                                    .resume(activation.clone(), command.clone(), context())
                                    .await
                                    .is_err()
                            );
                        }
                    }
                    assert_eq!(
                        transport.seen.lock().unwrap().len(),
                        count,
                        "unknown recovery has zero outbound IO"
                    );
                    assert_eq!(
                        rec.0.lock().unwrap().len(),
                        committed,
                        "unknown recovery never rearms durable state"
                    );
                    continue;
                }
                Cut::LoseAcceptedReply => {
                    let Some(RemoteOperationState::Accepted(accepted)) = restored else {
                        panic!("lost reply preserves Accepted");
                    };
                    assert_eq!(accepted.reference.task_id, "target");
                    transport
                        .replies
                        .lock()
                        .unwrap()
                        .push_back(waiting_task("target", false));
                    assert_eq!(
                        executor
                            .execute(activation.clone(), context())
                            .await
                            .unwrap(),
                        RunState::Awaiting,
                        "committed Accepted allows exact GET recovery"
                    );
                    assert_eq!(
                        transport.seen.lock().unwrap().last().unwrap().1,
                        "/v1/a2a/tasks/target"
                    );
                    assert_eq!(
                        sends.lock().unwrap().len() - before_sends,
                        1,
                        "Accepted recovery never resends"
                    );
                }
                Cut::Delivered => assert_eq!(result.unwrap(), RunState::Awaiting),
            }
            // R6 a NEW ticket is a NEW operation even on the same Run. This
            // positive path prevents treating at-most-once as one send per Run.
            let next = ResumeCommand::from_ticket(
                &rec.resume_ticket(&activation.run_id).unwrap(),
                ResumeResult::Input("answer-two".into()),
                0,
            );
            transport
                .replies
                .lock()
                .unwrap()
                .push_back(waiting_task("next", true));
            assert_eq!(
                executor
                    .resume(activation.clone(), next, context())
                    .await
                    .unwrap(),
                RunState::Awaiting,
                "new Resume operation remains executable"
            );
            assert_eq!(
                sends.lock().unwrap().len() - before_sends,
                2,
                "two distinct operations each send once"
            );
        }
    }
}
