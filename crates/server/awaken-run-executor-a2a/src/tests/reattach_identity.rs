//! T10-C03: a rebuilt executor resubmits the same operation identity (the
//! same Run). Only a committed remote-task binding lets the A2A adapter
//! reconnect to the original operation; without that evidence it refuses with
//! zero remote requests instead of starting a new operation.

use super::*;

const COMPLETED: &str = r#"{"kind":"task","id":"remote-9","contextId":"ctx-9","status":{"state":"completed","message":{"kind":"message","messageId":"m","role":"agent","parts":[{"kind":"text","text":"recovered"}]}}}"#;

fn paths(transport: &ScriptedTransport) -> Vec<String> {
    transport
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, path, _)| path.clone())
        .collect()
}

/// The recovery oracle: a rebuilt executor addressing the original operation
/// issues no initiating `message:send` and only reads the committed task.
fn recovery_oracle(requests: &[String], committed_task: &str) -> std::result::Result<(), String> {
    let sends = requests
        .iter()
        .filter(|path| path.ends_with("message:send"))
        .count();
    if sends != 0 {
        return Err(format!("{sends} new message:send during recovery"));
    }
    let expected = format!("/v1/a2a/tasks/{committed_task}");
    if requests.is_empty() || requests.iter().any(|path| path != &expected) {
        return Err(format!(
            "recovery did not reattach to {expected}: {requests:?}"
        ));
    }
    Ok(())
}

async fn accepted_context(task_id: &str) -> (Arc<Rec>, RunActivation) {
    let rec = Arc::new(Rec::default());
    let activation = activation("a2a:http://remote.invalid");
    let context = RuntimeRunContext::new()
        .with_commit(rec.clone())
        .with_reader(rec.clone());
    commit_boundary(
        &context,
        &activation,
        RunDisposition::running(activation.run_id.clone()),
        Vec::new(),
        vec![
            accepted_reference_state(&TaskReference {
                endpoint: "http://remote.invalid".into(),
                task_id: task_id.into(),
                context_id: "ctx-9".into(),
            })
            .unwrap(),
        ],
    )
    .await
    .unwrap();
    (rec, activation)
}

fn context_over(rec: &Arc<Rec>) -> RuntimeRunContext {
    RuntimeRunContext::new()
        .with_commit(rec.clone())
        .with_reader(rec.clone())
}

#[tokio::test]
async fn rebuilt_executor_reattaches_the_original_operation_only_with_committed_evidence() {
    // | Row | committed evidence      | adapter decision            | remote effect          |
    // | R1  | Accepted task remote-9  | continue durable operation  | GET remote-9 only      |
    // | R2  | Prepared, send unknown  | require predecessor fence   | refused, zero requests |
    // NC  | no evidence (fresh Rec)  | starts a new operation      | oracle reports the send |

    // R1
    let (rec, accepted) = accepted_context("remote-9").await;
    let rebuilt = scripted_executor(Arc::new(ScriptedTransport::new(Vec::new())));
    assert_eq!(
        rebuilt.physical_attempt_recovery(&accepted, &context_over(&rec)),
        AttemptRecoveryMode::ContinueDurableRemoteOperation,
        "R1 only the capable adapter offers continuation"
    );
    let transport = Arc::new(ScriptedTransport::new(vec![response(COMPLETED)]));
    let state = scripted_executor(transport.clone())
        .execute(accepted.clone(), context_over(&rec))
        .await
        .expect("R1 reattaches");
    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd), "R1");
    recovery_oracle(&paths(&transport), "remote-9").expect("R1 recovery oracle");
    assert_eq!(rec.run_state(&accepted.run_id), Some(state), "R1 same Run");

    // R2: the send's outcome is unknown, so the adapter has no evidence.
    let rec = Arc::new(Rec::default());
    let activation = activation("a2a:http://remote.invalid");
    let prepared =
        prepare_remote_operation("http://remote.invalid", "ctx-9", None, "message-1", "go")
            .unwrap();
    commit_boundary(
        &context_over(&rec),
        &activation,
        RunDisposition::running(activation.run_id.clone()),
        Vec::new(),
        vec![prepared_operation_state(&prepared).unwrap()],
    )
    .await
    .unwrap();
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let executor = scripted_executor(transport.clone());
    assert_eq!(
        executor.physical_attempt_recovery(&activation, &context_over(&rec)),
        AttemptRecoveryMode::RequirePredecessorQuiescence,
        "R2"
    );
    let error = executor
        .execute(activation.clone(), context_over(&rec))
        .await
        .expect_err("R2 an unconfirmed operation is refused");
    assert!(
        error.to_string().contains("no committed remote task"),
        "R2 the refusal names the missing evidence: {error}"
    );
    assert!(paths(&transport).is_empty(), "R2 zero remote prompts");

    // NC: an adapter without evidence that just starts a new operation.
    let fresh = Arc::new(Rec::default());
    let transport = Arc::new(ScriptedTransport::new(vec![response(
        r#"{"task":{"kind":"task","id":"remote-new","contextId":"ctx-new","status":{"state":"completed","message":{"kind":"message","messageId":"n","role":"agent","parts":[{"kind":"text","text":"fresh"}]}}}}"#,
    )]));
    let _ = scripted_executor(transport.clone())
        .execute(activation, context_over(&fresh))
        .await;
    let violation = recovery_oracle(&paths(&transport), "remote-9")
        .expect_err("NC: starting a new operation is not recovery");
    assert!(
        violation.contains("message:send"),
        "NC detected: {violation}"
    );
}

#[tokio::test]
#[ignore = "product gap: an incapable A2A recovery is refused with an untyped Error::Execution string (kind Internal), not an actionable typed rejection"]
async fn incapable_recovery_is_a_typed_actionable_rejection() {
    let rec = Arc::new(Rec::default());
    let activation = activation("a2a:http://remote.invalid");
    let prepared =
        prepare_remote_operation("http://remote.invalid", "ctx-9", None, "message-1", "go")
            .unwrap();
    commit_boundary(
        &context_over(&rec),
        &activation,
        RunDisposition::running(activation.run_id.clone()),
        Vec::new(),
        vec![prepared_operation_state(&prepared).unwrap()],
    )
    .await
    .unwrap();
    let error = scripted_executor(Arc::new(ScriptedTransport::new(Vec::new())))
        .execute(activation, context_over(&rec))
        .await
        .expect_err("refused");
    assert!(
        matches!(error, Error::Diagnostic(_)),
        "expected a typed diagnostic the operator can act on, got {error:?} ({:?})",
        error.kind()
    );
}
