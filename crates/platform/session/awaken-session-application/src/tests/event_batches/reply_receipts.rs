//! Batch receipt qualification over a real SQLite Session root. The explicit
//! coordination-read fixture represents committed per-reply evidence; the
//! executor owner's tests qualify the actual ResumeApplied history classifier.

use super::*;

fn retained_commands(session: &PersistedSession) -> Vec<SessionThreadToolReplyCommand> {
    session.event_batches[0]
        .events
        .iter()
        .map(|entry| match &entry.event {
            SessionEventCommand::ToolReply { reply, .. } => {
                reply.delivery_command(&SessionId::from(session.session_id.as_str()), None)
            }
            _ => panic!("receipt fixture contains only ToolReply entries"),
        })
        .collect()
}

fn assert_no_reply_effects(runtime: &EventBatchRuntime) {
    assert_eq!(runtime.tool_reply_calls.load(Ordering::SeqCst), 0);
    assert!(runtime.reserved.lock().unwrap().is_empty());
    assert!(runtime.activated.lock().unwrap().is_empty());
    assert_eq!(runtime.primary_interrupt_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.child_interrupt_calls.load(Ordering::SeqCst), 0);
    assert!(runtime.trace.lock().unwrap().is_empty());
}

async fn admitted_replies(
    repository: Arc<dyn crate::SessionStore>,
    runtime: Arc<EventBatchRuntime>,
    session: &str,
) -> SessionApplication {
    create(repository.as_ref(), persisted(session, false, "idle")).await;
    let app = application_with_runtime(
        runtime,
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    // Two real logical targets share the fixture's exact awaited call shape.
    // The original admission owner validates and retains both before querying.
    app.event_batches
        .admit_session_event_batch_idempotent(
            &SessionId::from(session),
            vec![
                tool_reply_input_for_target("reply-call", SessionThreadTarget::Primary),
                tool_reply_input_for_target(
                    "reply-call",
                    SessionThreadTarget::Child(ThreadId("event-child".into())),
                ),
            ],
            None,
            None,
            Some(("reply-key".into(), "whole-request-fingerprint".into())),
        )
        .await
        .unwrap()
        .into_batch()
        .expect("real accepted reply batch");
    app
}

#[tokio::test]
async fn batch_reply_receipt_requires_every_exact_reply_and_never_drives() {
    // Acceptance, a partial committed effect, and a full committed effect are
    // distinct. Reconstructing the application retains that same distinction.
    for proven in 0..=2 {
        let repository: Arc<dyn crate::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(EventBatchRuntime::default());
        let session = format!("reply-receipt-{proven}");
        let app = admitted_replies(repository.clone(), runtime.clone(), &session).await;
        let id = SessionId::from(session.as_str());
        let before = repository.get(&id).await.unwrap();
        assert!(
            before.event_batches[0]
                .events
                .iter()
                .all(|entry| !entry.processed)
        );
        let commands = retained_commands(&before);
        for command in commands.iter().take(proven) {
            runtime
                .staged_tool_replies
                .lock()
                .unwrap()
                .insert(command.activity_operation_id(), command.clone());
        }
        for app in [
            app,
            application_with_runtime(
                runtime.clone(),
                repository.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            ),
        ] {
            assert_eq!(
                app.event_batches
                    .session_event_batch_reply_receipt(
                        "workspace",
                        &id,
                        "reply-key",
                        "whole-request-fingerprint",
                    )
                    .await
                    .unwrap(),
                proven == 2,
            );
            assert_eq!(repository.get(&id).await.unwrap(), before);
            assert_no_reply_effects(&runtime);
        }
    }
}

#[tokio::test]
async fn batch_reply_receipt_checks_key_fingerprint_owner_and_the_full_retained_reply() {
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = admitted_replies(
        repository.clone(),
        runtime.clone(),
        "reply-receipt-identity",
    )
    .await;
    let id = SessionId::from("reply-receipt-identity");
    let before = repository.get(&id).await.unwrap();
    let commands = retained_commands(&before);
    for command in &commands {
        runtime
            .staged_tool_replies
            .lock()
            .unwrap()
            .insert(command.activity_operation_id(), command.clone());
    }
    for (key, fingerprint) in [
        ("missing-key", "whole-request-fingerprint"),
        ("reply-key", "changed-last-member-fingerprint"),
    ] {
        assert!(
            !app.event_batches
                .session_event_batch_reply_receipt("workspace", &id, key, fingerprint)
                .await
                .unwrap()
        );
    }
    assert!(
        app.event_batches
            .session_event_batch_reply_receipt(
                "foreign-owner",
                &id,
                "reply-key",
                "whole-request-fingerprint"
            )
            .await
            .is_err()
    );

    let hidden = app
        .event_batches
        .session_event_batch_reply_receipt(
            "foreign-owner",
            &id,
            "reply-key",
            "whole-request-fingerprint",
        )
        .await
        .unwrap_err();
    let absent = app
        .event_batches
        .session_event_batch_reply_receipt(
            "workspace",
            &SessionId::from("absent-root"),
            "reply-key",
            "whole-request-fingerprint",
        )
        .await
        .unwrap_err();
    assert_eq!(
        hidden.kind, absent.kind,
        "invisible Roots have the same existing error class"
    );
    assert_eq!(hidden.code, absent.code);
    assert_eq!(hidden.message, absent.message);
    assert_eq!(repository.get(&id).await.unwrap(), before);
    assert_no_reply_effects(&runtime);

    let original = commands.last().unwrap();
    let mut changed = original.clone();
    changed.reply = awaken_session_contract::SessionThreadToolReply::Result {
        content: vec![ContentBlock::text("changed last reply")],
        is_error: true,
    };
    runtime
        .staged_tool_replies
        .lock()
        .unwrap()
        .insert(original.activity_operation_id(), changed);
    assert!(
        !app.event_batches
            .session_event_batch_reply_receipt(
                "workspace",
                &id,
                "reply-key",
                "whole-request-fingerprint"
            )
            .await
            .unwrap()
    );
    runtime
        .staged_tool_replies
        .lock()
        .unwrap()
        .insert(original.activity_operation_id(), original.clone());
    *runtime.recovery_read_error.lock().unwrap() =
        Some(RunError::unavailable("receipt read unavailable"));
    let error = app
        .event_batches
        .session_event_batch_reply_receipt(
            "workspace",
            &id,
            "reply-key",
            "whole-request-fingerprint",
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        awaken_runtime_contract::runs::RunErrorKind::Unavailable
    );
    *runtime.recovery_read_error.lock().unwrap() = None;
    assert_eq!(repository.get(&id).await.unwrap(), before);
    assert_no_reply_effects(&runtime);
}

#[tokio::test]
async fn terminal_processed_batch_is_unproven_until_its_reply_receipts_exist() {
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    let app = admitted_replies(
        repository.clone(),
        runtime.clone(),
        "reply-receipt-terminal",
    )
    .await;
    let id = SessionId::from("reply-receipt-terminal");
    let mut terminal = repository.get(&id).await.unwrap();
    terminal
        .transition_execution(awaken_session_contract::SessionExecutionState::Terminated)
        .unwrap();
    assert_eq!(terminal.event_batches[0].resolve_terminally(None, 42), 2);
    let _fixture = app
        .mutations
        .commit_session_snapshot_with_outcome(
            "workspace",
            awaken_session_contract::SessionAggregate::try_from(terminal).unwrap(),
            "test-terminal-reply-resolution",
            Vec::new(),
        )
        .await
        .unwrap();
    let before = repository.get(&id).await.unwrap();
    assert!(
        before.event_batches[0]
            .events
            .iter()
            .all(|entry| entry.processed)
    );
    assert!(
        !app.event_batches
            .session_event_batch_reply_receipt(
                "workspace",
                &id,
                "reply-key",
                "whole-request-fingerprint"
            )
            .await
            .unwrap()
    );
    for command in retained_commands(&before) {
        runtime
            .staged_tool_replies
            .lock()
            .unwrap()
            .insert(command.activity_operation_id(), command);
    }
    assert!(
        app.event_batches
            .session_event_batch_reply_receipt(
                "workspace",
                &id,
                "reply-key",
                "whole-request-fingerprint"
            )
            .await
            .unwrap()
    );
    assert_eq!(repository.get(&id).await.unwrap(), before);
    assert_no_reply_effects(&runtime);
}

#[tokio::test]
async fn closed_or_non_reply_batch_never_qualifies_as_a_reply_receipt() {
    for closed in [false, true] {
        let repository: Arc<dyn crate::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(EventBatchRuntime::default());
        let id = SessionId::from(format!("reply-receipt-ineligible-{closed}").as_str());
        create(repository.as_ref(), persisted(id.as_str(), false, "idle")).await;
        let app = application_with_runtime(
            runtime.clone(),
            repository.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        if closed {
            let _closed = app
                .event_batches
                .close_session_event_admission(
                    "workspace",
                    &id,
                    "reply-key".into(),
                    "fingerprint".into(),
                )
                .await
                .unwrap();
        } else {
            let _accepted = app
                .event_batches
                .admit_session_event_batch_idempotent(
                    &id,
                    vec![SessionEventInput::UserMessage {
                        content: vec![ContentBlock::text("ordinary input")],
                    }],
                    None,
                    None,
                    Some(("reply-key".into(), "fingerprint".into())),
                )
                .await
                .unwrap();
        }
        let before = repository.get(&id).await.unwrap();
        assert!(
            !app.event_batches
                .session_event_batch_reply_receipt("workspace", &id, "reply-key", "fingerprint")
                .await
                .unwrap()
        );
        assert_eq!(repository.get(&id).await.unwrap(), before);
        assert_no_reply_effects(&runtime);
    }
}

fn omit_legacy_mutation_policy(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(fields) => {
            let removed = usize::from(fields.remove("mutation_policy").is_some());
            removed
                + fields
                    .values_mut()
                    .map(omit_legacy_mutation_policy)
                    .sum::<usize>()
        }
        serde_json::Value::Array(values) => {
            values.iter_mut().map(omit_legacy_mutation_policy).sum()
        }
        _ => 0,
    }
}

#[tokio::test]
async fn legacy_raw_profiled_root_keeps_exact_run_read_reply_receipt_and_cancel() {
    // The actual e691 profiled constructor had no mode/policy field. Decode its
    // historical missing-policy shape through today's original Session decoder.
    // Exact operations keep Workspace ownership and the original Run fences.
    use crate::ProfiledSessionReplies as _;
    let repository: Arc<dyn crate::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(EventBatchRuntime::default());
    runtime.tool_reply_prior_epoch.store(1, Ordering::SeqCst);
    let mut raw = running_session_with_activity("legacy-profiled-reply", 1);
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut raw.baseline
    else {
        panic!("historical Session has a frozen baseline");
    };
    baseline.mutation_policy =
        awaken_session_configuration_contract::resource::SessionMutationPolicy::Frozen;
    let mut wire = serde_json::to_value(raw).unwrap();
    assert_eq!(omit_legacy_mutation_policy(&mut wire), 1);
    let legacy: PersistedSession = serde_json::from_value(wire.clone()).unwrap();
    assert!(
        legacy
            .frozen_baseline()
            .unwrap()
            .mutation_policy
            .is_managed()
    );
    assert_eq!(
        omit_legacy_mutation_policy(&mut wire),
        0,
        "old raw data omits the policy field"
    );
    let id = legacy.session_id.clone();
    create(repository.as_ref(), legacy).await;
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let SessionEventInput::ToolReply(reply) = tool_reply_input("legacy-call") else {
        unreachable!()
    };
    let command = reply.delivery_command(&id, None);
    let before = repository.get(&id).await.unwrap();
    let page = app
        .run_admissions
        .session_run_events_after(
            "workspace",
            &id,
            &command.expected_run_id,
            awaken_dispatch_contract::DispatchCursor(0),
            awaken_agent_contract::RunLifecycleCursor(0),
            16,
        )
        .await
        .unwrap();
    assert_eq!(page.run_id, command.expected_run_id);
    assert_eq!(page.thread_version, Some(1));
    assert!(
        !app.coordination
            .profiled_session_run_reply_receipt_for_owner("workspace", command.clone())
            .await
            .unwrap()
    );
    assert_eq!(repository.get(&id).await.unwrap(), before);
    assert_no_reply_effects(&runtime);

    app.coordination
        .reply_profiled_session_run_for_owner("workspace", command.clone())
        .await
        .unwrap();
    let after_reply = repository.get(&id).await.unwrap();
    assert_eq!(runtime.tool_reply_calls.load(Ordering::SeqCst), 1);
    assert!(
        app.coordination
            .profiled_session_run_reply_receipt_for_owner("workspace", command.clone())
            .await
            .unwrap()
    );
    let mut wrong_run = command.clone();
    wrong_run.expected_run_id = RunId("other-run".into());
    assert!(
        !app.coordination
            .profiled_session_run_reply_receipt_for_owner("workspace", wrong_run)
            .await
            .unwrap()
    );
    let mut missing_root = command.clone();
    missing_root.session_id = SessionId::from("absent-root");
    assert!(matches!(
        app.coordination
            .profiled_session_run_reply_receipt_for_owner("workspace", missing_root)
            .await,
        Err(crate::SessionProjectionRecoveryError::NotFound)
    ));
    assert!(matches!(
        app.coordination
            .profiled_session_run_reply_receipt_for_owner("foreign", command.clone())
            .await,
        Err(crate::SessionProjectionRecoveryError::NotFound)
    ));
    assert_eq!(
        repository.get(&id).await.unwrap(),
        after_reply,
        "queries leave the full current Root unchanged"
    );
    assert_eq!(runtime.tool_reply_calls.load(Ordering::SeqCst), 1);
    assert!(runtime.reserved.lock().unwrap().is_empty());
    assert!(runtime.activated.lock().unwrap().is_empty());
    let trace = runtime.trace.lock().unwrap().clone();
    assert!(
        app.coordination
            .cancel_profiled_session_run_for_owner("foreign", &id, &command.expected_run_id)
            .await
            .is_err()
    );
    assert!(
        app.coordination
            .cancel_profiled_session_run_for_owner(
                "workspace",
                &SessionId::from("absent-root"),
                &command.expected_run_id
            )
            .await
            .is_err()
    );
    assert_eq!(*runtime.trace.lock().unwrap(), trace);
    app.coordination
        .cancel_profiled_session_run_for_owner("workspace", &id, &command.expected_run_id)
        .await
        .unwrap();
    assert_eq!(
        runtime.trace.lock().unwrap().last().unwrap(),
        &format!("cancel-exact:{id}:{}", command.expected_run_id.0)
    );
    assert_eq!(repository.get(&id).await.unwrap(), after_reply);
}
