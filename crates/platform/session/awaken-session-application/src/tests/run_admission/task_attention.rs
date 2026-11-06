//! Task ACK ordering through the original Session activity and command owners.
use super::*;
use awaken_session_application_contract::SessionRunBackgroundApplication as _;

#[tokio::test]
async fn child_task_ack_survives_publication_loss_without_new_admission() {
    // Cause/effect rules: A1 committed child membership, complete tenure and
    // a scripted Missing observation => fresh roster lowering and one admission.
    // A2 the accepted command and real SQLite activity remain, but a cold App
    // has no publication/projection owner and its origin source is now unreadable
    // => exact replay returns the same ACK,
    // without a new projection, activity or dispatch. A3 subject/body drift =>
    // conflict and no new effects. A4 Missing after publication loss => refusal,
    // never a guessed snapshot. This fixture controls the Dispatch read result;
    // the shared Dispatch memory/SQLite/PG conformance cases independently own
    // Live/Completed receipt durability. The Session activity here is real SQL.
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("private Session repository"),
    );
    let id = "child-task-ack";
    create(
        repository.as_ref(),
        super::super::authority::coordinated_session(id),
    )
    .await;
    let runtime = Arc::new(RecordingAgentAdmissionRuntime::new([
        AgentAdmissionOutcome::Accepted,
    ]));
    let thread = awaken_agent_contract::ThreadId("task-child".into());
    runtime.commit_link(awaken_session_execution_contract::CoordinatedThreadLink {
        session_id: id.into(),
        thread_id: thread.clone(),
        target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
            agent_id: "coord-child".into(),
        },
        created_by_operation_id: "original-child".into(),
        latest_run_id: Some(awaken_agent_contract::RunId("origin-run".into())),
    });
    let app = configured_admission_application(repository.clone(), runtime.clone());
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        app.run_admissions(),
        app.coordination(),
        |_| Err(RunError::unavailable("disposable owner is absent")),
        |_| Ok(None),
    );
    let attention = awaken_session_application_contract::TaskAttention {
        operation_id: "task-terminal-notification".into(),
        tenure: awaken_session_application_contract::TaskTenureRead {
            thread: thread.0.clone(),
            partition: awaken_session_application_contract::TaskReadPartition::Session {
                session_thread: id.into(),
            },
            run: "origin-run".into(),
            task: "task".into(),
            holder: "holder".into(),
            epoch: awaken_lease_contract::fence::Epoch::from_persisted(3).unwrap(),
        },
        message: awaken_agent_contract::Message::text(
            awaken_agent_contract::agent::message::Id("task-attention-message".into()),
            awaken_agent_contract::agent::message::Role::System,
            "Task requires input",
        ),
        input: None,
    };
    runtime.set_admission_observations([Ok(None)]);
    let ack = protocol
        .submit_task_attention(attention.clone())
        .await
        .expect("A1 admission");
    assert_eq!(
        runtime.admissions.lock().unwrap().len(),
        1,
        "A1 one dispatch"
    );
    let before = repository.get(&SessionId::from(id)).await.unwrap();
    let projection_count = runtime.installed_projections.lock().unwrap().len();
    drop(protocol);
    drop(app);
    runtime
        .source_valid
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let cold = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        cold.run_admissions(),
        cold.coordination(),
        |_| Err(RunError::unavailable("no cold projection owner")),
        |_| Ok(None),
    );
    let exact = || {
        Ok(Some(
            awaken_session_execution_contract::SessionAgentMessageReceipt {
                thread_id: thread.clone(),
            },
        ))
    };
    runtime.set_admission_observations([exact(), exact()]);
    assert_eq!(
        protocol
            .task_attention_receipt(attention.clone())
            .await
            .unwrap(),
        Some(ack.clone()),
        "A2 explicit ACK read"
    );
    assert_eq!(
        protocol
            .submit_task_attention(attention.clone())
            .await
            .unwrap(),
        ack,
        "A2 local retry also reads first"
    );
    assert_eq!(
        runtime.admissions.lock().unwrap().len(),
        1,
        "A2 no new admission"
    );
    assert_eq!(
        runtime.installed_projections.lock().unwrap().len(),
        projection_count,
        "A2 no projection install"
    );
    assert_eq!(
        repository.get(&SessionId::from(id)).await.unwrap(),
        before,
        "A2 no Session mutation"
    );
    for axis in ["task", "run", "holder", "epoch", "message"] {
        let mut changed = attention.clone();
        match axis {
            "task" => changed.tenure.task.push_str("-other"),
            "run" => changed.tenure.run.push_str("-other"),
            "holder" => changed.tenure.holder.push_str("-other"),
            "epoch" => {
                changed.tenure.epoch =
                    awaken_lease_contract::fence::Epoch::from_persisted(4).unwrap()
            }
            "message" => changed.message.id.0.push_str("-other"),
            _ => unreachable!(),
        }
        runtime.set_admission_observations([exact()]);
        assert!(
            protocol.submit_task_attention(changed).await.is_err(),
            "A3 {axis}"
        );
    }
    let mut fresh = attention;
    fresh.operation_id.push_str("-fresh");
    runtime.set_admission_observations([Ok(None)]);
    assert!(
        protocol.submit_task_attention(fresh).await.is_err(),
        "A4 fresh needs publication"
    );
    assert_eq!(
        runtime.admissions.lock().unwrap().len(),
        1,
        "A3/A4 no new dispatch"
    );
    assert_eq!(
        repository.get(&SessionId::from(id)).await.unwrap(),
        before,
        "A3/A4 no new activity"
    );
}
