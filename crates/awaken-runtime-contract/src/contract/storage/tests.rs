use super::*;

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
