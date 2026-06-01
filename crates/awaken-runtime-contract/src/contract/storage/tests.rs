use super::*;

fn valid_run_record(status: RunStatus) -> RunRecord {
    let mut run = RunRecord {
        run_id: "run-1".to_string(),
        thread_id: "thread-1".to_string(),
        agent_id: "agent-1".to_string(),
        status,
        created_at: 1,
        updated_at: 1,
        ..Default::default()
    };
    if status == RunStatus::Waiting {
        run.waiting = Some(RunWaitingState {
            reason: WaitingReason::UserInput,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        });
    }
    if status == RunStatus::Done {
        run.finished_at = Some(1);
    }
    run
}

#[test]
fn run_record_validate_rejects_waiting_without_waiting_state() {
    let mut run = valid_run_record(RunStatus::Waiting);
    run.waiting = None;

    let err = run.validate_for_persist().unwrap_err();

    assert!(matches!(err, StorageError::Validation(message)
            if message.contains("waiting") && message.contains("waiting state")));
}

#[test]
fn run_record_validate_rejects_done_without_finished_at() {
    let mut run = valid_run_record(RunStatus::Done);
    run.finished_at = None;

    let err = run.validate_for_persist().unwrap_err();

    assert!(matches!(err, StorageError::Validation(message)
            if message.contains("done") && message.contains("finished_at")));
}

#[test]
fn run_record_validate_rejects_mismatched_activation_thread() {
    let mut run = valid_run_record(RunStatus::Running);
    run.activation = Some(crate::contract::run::RunActivationSnapshot {
        intent: crate::contract::run::RunIntent::new("other-thread"),
        input: crate::contract::run::RunInputSnapshot {
            thread_id: "other-thread".into(),
            ..Default::default()
        },
        options: crate::contract::run::RunOptions::default(),
        trace: crate::contract::run::RunTraceContext::default(),
        seeded_decisions: Vec::new(),
        resolution_id: None,
    });

    let err = run.validate_for_persist().unwrap_err();

    assert!(matches!(err, StorageError::Validation(message)
            if message.contains("activation thread_id") && message.contains("run thread_id")));
}

#[test]
fn run_record_validate_rejects_mismatched_output_thread() {
    let mut run = valid_run_record(RunStatus::Running);
    run.output = Some(RunMessageOutput {
        thread_id: "other-thread".to_string(),
        range: MessageSeqRange::new(1, 1),
        message_ids: vec!["m-1".to_string()],
    });

    let err = run.validate_for_persist().unwrap_err();

    assert!(matches!(err, StorageError::Validation(message)
            if message.contains("run output thread_id")));
}

#[test]
fn run_record_validate_rejects_mismatched_terminal_outcome() {
    let mut run = valid_run_record(RunStatus::Done);
    run.termination_reason = Some(TerminationReason::NaturalEnd);
    run.outcome = Some(RunOutcome {
        termination_reason: TerminationReason::Cancelled,
        final_output: None,
        error_payload: None,
    });

    let err = run.validate_for_persist().unwrap_err();

    assert!(matches!(err, StorageError::Validation(message)
            if message.contains("termination_reason")));
}

#[test]
fn merge_checkpoint_append_messages_rejects_existing_id() {
    let mut existing = vec![
        Message::user("first").with_id("msg-1".to_string()),
        Message::assistant("old").with_id("msg-2".to_string()),
    ];
    let delta = vec![Message::assistant("new").with_id("msg-2".to_string())];

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &delta)
        .expect_err("same-id append must be rejected");

    assert!(
        matches!(err, StorageError::Validation(message) if message.contains("already committed"))
    );
    assert_eq!(existing.len(), 2, "committed log must remain untouched");
    assert_eq!(existing[1].text(), "old");
}

#[test]
fn merge_checkpoint_append_messages_rejects_duplicate_delta_id() {
    let mut existing = vec![Message::user("first").with_id("msg-1".to_string())];
    let delta = vec![
        Message::user("second").with_id("msg-2".to_string()),
        Message::assistant("duplicate").with_id("msg-2".to_string()),
    ];

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &delta)
        .expect_err("append delta must reject duplicate ids");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("duplicate")));
    assert_eq!(existing.len(), 1, "committed log must remain untouched");
    assert_eq!(existing[0].text(), "first");
}

#[test]
fn merge_checkpoint_append_messages_rejects_missing_delta_id() {
    let mut existing = vec![Message::user("first").with_id("msg-1".to_string())];
    let mut missing_id = Message::assistant("missing id");
    missing_id.id = None;

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &[missing_id])
        .expect_err("append delta must reject messages without ids");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("id")));
    assert_eq!(existing.len(), 1, "committed log must remain untouched");
}

#[test]
fn merge_checkpoint_append_messages_rejects_tool_without_call_id() {
    let mut existing = Vec::new();
    let mut tool = Message::tool("call-1", "result").with_id("msg-1".to_string());
    tool.tool_call_id = None;

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &[tool])
        .expect_err("tool messages must carry a tool_call_id");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("tool_call_id")));
    assert!(existing.is_empty(), "committed log must remain untouched");
}

#[test]
fn merge_checkpoint_append_messages_rejects_non_tool_call_id() {
    let mut existing = Vec::new();
    let mut user = Message::user("bad").with_id("msg-1".to_string());
    user.tool_call_id = Some("call-1".to_string());

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &[user])
        .expect_err("non-tool messages must not carry tool_call_id");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("tool_call_id")));
    assert!(existing.is_empty(), "committed log must remain untouched");
}

#[test]
fn merge_checkpoint_append_messages_rejects_invalid_assistant_tool_calls() {
    let mut existing = Vec::new();
    let duplicate_calls = Message::assistant_with_tool_calls(
        "calls",
        vec![
            crate::contract::message::ToolCall::new("call-1", "search", serde_json::json!({})),
            crate::contract::message::ToolCall::new("call-1", "fetch", serde_json::json!({})),
        ],
    )
    .with_id("msg-1".to_string());

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &[duplicate_calls])
        .expect_err("assistant tool call ids must be unique");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("duplicate")));
    assert!(existing.is_empty(), "committed log must remain untouched");
}

#[test]
fn merge_checkpoint_append_messages_rejects_empty_tool_call_name() {
    let mut existing = Vec::new();
    let assistant = Message::assistant_with_tool_calls(
        "calls",
        vec![crate::contract::message::ToolCall::new(
            "call-1",
            " ",
            serde_json::json!({}),
        )],
    )
    .with_id("msg-1".to_string());

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &[assistant])
        .expect_err("assistant tool call names must be non-empty");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("name")));
    assert!(existing.is_empty(), "committed log must remain untouched");
}

#[test]
fn merge_checkpoint_append_messages_rejects_invalid_compaction_range() {
    let mut existing = Vec::new();
    let mut summary = Message::system("summary").with_id("summary-1".to_string());
    summary.metadata = Some(crate::contract::message::MessageMetadata {
        compaction: Some(crate::contract::message::CompactionMark {
            from_seq: 0,
            to_seq: 1,
        }),
        ..Default::default()
    });

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &[summary])
        .expect_err("compaction range must be 1-based and non-empty");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("compaction")));
    assert!(existing.is_empty(), "committed log must remain untouched");
}

#[test]
fn merge_checkpoint_append_messages_rejects_compaction_covering_summary() {
    let mut existing = vec![Message::user("first").with_id("msg-1".to_string())];
    let mut summary = Message::system("summary").with_id("summary-1".to_string());
    summary.metadata = Some(crate::contract::message::MessageMetadata {
        compaction: Some(crate::contract::message::CompactionMark {
            from_seq: 1,
            to_seq: 2,
        }),
        ..Default::default()
    });

    let err = message_append::merge_checkpoint_append_messages(&mut existing, &[summary])
        .expect_err("compaction range must not include the summary message itself");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("summary")));
    assert_eq!(existing.len(), 1, "committed log must remain untouched");
}

#[test]
fn merge_checkpoint_append_messages_appends_new_ids() {
    let mut existing = vec![Message::user("first").with_id("msg-1".to_string())];
    let delta = vec![Message::user("tail").with_id("msg-2".to_string())];

    message_append::merge_checkpoint_append_messages(&mut existing, &delta).unwrap();

    assert_eq!(existing.len(), 2);
    assert_eq!(existing[1].text(), "tail");
}

/// Minimal in-memory store exercising the default `load_checkpoint` composition.
struct FakeCheckpointStore {
    committed: Option<Vec<Message>>,
    latest_run: Option<RunRecord>,
    thread_state: Option<crate::state::PersistedState>,
}

#[async_trait::async_trait]
impl RuntimeCheckpointStore for FakeCheckpointStore {
    async fn load_thread(&self, _thread_id: &str) -> Result<Option<Thread>, StorageError> {
        Ok(None)
    }
    async fn load_messages(&self, _thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        Ok(self.committed.clone())
    }
    async fn load_committed_messages(
        &self,
        _thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        Ok(self.committed.clone())
    }
    async fn load_run(&self, _run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self.latest_run.clone())
    }
    async fn latest_run(&self, _thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self.latest_run.clone())
    }
    async fn load_thread_state(
        &self,
        _thread_id: &str,
    ) -> Result<Option<crate::state::PersistedState>, StorageError> {
        Ok(self.thread_state.clone())
    }
}

#[tokio::test]
async fn load_checkpoint_default_composes_reads_and_reports_raw_version() {
    // An assistant message with an unanswered tool call is filtered from the
    // effective view, but the version must reflect the raw committed count.
    let assistant_with_unpaired = Message::assistant_with_tool_calls(
        "call",
        vec![crate::contract::message::ToolCall {
            id: "call-x".to_string(),
            name: "t".to_string(),
            arguments: serde_json::json!({}),
        }],
    )
    .with_id("m-2".to_string());
    let store = FakeCheckpointStore {
        committed: Some(vec![
            Message::user("hi").with_id("m-1".to_string()),
            assistant_with_unpaired,
        ]),
        latest_run: Some(RunRecord {
            run_id: "r-1".to_string(),
            thread_id: "t-1".to_string(),
            ..Default::default()
        }),
        thread_state: Some(crate::state::PersistedState {
            revision: 4,
            extensions: Default::default(),
        }),
    };

    let snapshot = store
        .load_checkpoint("t-1")
        .await
        .unwrap()
        .expect("snapshot present");
    assert_eq!(
        snapshot.message_version, 2,
        "version is the raw committed count"
    );
    // The unpaired assistant tool call is stripped from the view, leaving its
    // text-bearing body; the version is unaffected by the view filter.
    assert!(snapshot.messages.iter().all(|m| m.tool_calls.is_none()));
    assert_eq!(snapshot.latest_run.unwrap().run_id, "r-1");
    assert_eq!(snapshot.thread_state.unwrap().revision, 4);
}

#[tokio::test]
async fn load_checkpoint_default_returns_none_for_empty_thread() {
    let store = FakeCheckpointStore {
        committed: None,
        latest_run: None,
        thread_state: None,
    };
    assert!(store.load_checkpoint("t-empty").await.unwrap().is_none());
}
