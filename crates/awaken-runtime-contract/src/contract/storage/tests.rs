use super::*;
use std::collections::HashMap;
use std::sync::RwLock;

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

// ── Mock ThreadStore ──

#[derive(Debug, Default)]
struct MockThreadStore {
    threads: RwLock<HashMap<String, Thread>>,
    messages: RwLock<HashMap<String, Vec<Message>>>,
}

#[async_trait]
impl ThreadStore for MockThreadStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        let guard = self
            .threads
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(guard.get(thread_id).cloned())
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let mut guard = self
            .threads
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        guard.insert(thread.id.clone(), thread.clone());
        Ok(())
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        let mut threads = self
            .threads
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let mut messages = self
            .messages
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        threads.remove(thread_id);
        messages.remove(thread_id);
        Ok(())
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let guard = self
            .threads
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let mut ids: Vec<String> = guard.keys().cloned().collect();
        ids.sort();
        Ok(ids.into_iter().skip(offset).take(limit).collect())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self
            .messages
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(guard.get(thread_id).cloned())
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        let mut guard = self
            .messages
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        guard.insert(thread_id.to_owned(), messages.to_vec());
        Ok(())
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        let threads = self
            .threads
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        if !threads.contains_key(thread_id) {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }
        drop(threads);
        let mut guard = self
            .messages
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        guard.remove(thread_id);
        Ok(())
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: crate::thread::ThreadMetadata,
    ) -> Result<(), StorageError> {
        let mut guard = self
            .threads
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let thread = guard
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound(id.to_owned()))?;
        thread.metadata = metadata;
        Ok(())
    }
}

#[tokio::test]
async fn thread_store_save_and_load() {
    let store = MockThreadStore::default();
    let thread = Thread::with_id("t-1");

    store.save_thread(&thread).await.unwrap();
    let loaded = store.load_thread("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.id, "t-1");
}

#[tokio::test]
async fn thread_store_load_nonexistent() {
    let store = MockThreadStore::default();
    let result = store.load_thread("missing").await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn thread_store_list_paginated() {
    let store = MockThreadStore::default();
    for i in 0..5 {
        let thread = Thread::with_id(format!("t-{i}"));
        store.save_thread(&thread).await.unwrap();
    }
    let page1 = store.list_threads(0, 3).await.unwrap();
    assert_eq!(page1.len(), 3);
    let page2 = store.list_threads(3, 3).await.unwrap();
    assert_eq!(page2.len(), 2);
}

#[tokio::test]
async fn thread_store_default_query_filters_lineage() {
    let store = MockThreadStore::default();
    store
        .save_thread(
            &Thread::with_id("match")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .unwrap();
    store
        .save_thread(
            &Thread::with_id("wrong-resource")
                .with_resource_id("resource-b")
                .with_parent_thread_id("parent-1"),
        )
        .await
        .unwrap();
    store
        .save_thread(
            &Thread::with_id("wrong-parent")
                .with_resource_id("resource-a")
                .with_parent_thread_id("parent-2"),
        )
        .await
        .unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some("resource-a".to_string()),
            parent_filter: ThreadParentFilter::Parent("parent-1".to_string()),
        })
        .await
        .unwrap();

    assert_eq!(page.items, vec!["match"]);
    assert_eq!(page.total, 1);
    assert!(!page.has_more);
}

#[tokio::test]
async fn thread_store_query_normalizes_lineage_filters() {
    let store = MockThreadStore::default();
    let mut thread = Thread::with_id("match");
    thread.resource_id = Some(" resource-a ".to_string());
    thread.parent_thread_id = Some(" parent-1 ".to_string());
    store.save_thread(&thread).await.unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some(" resource-a ".to_string()),
            parent_filter: ThreadParentFilter::Parent(" parent-1 ".to_string()),
        })
        .await
        .unwrap();

    assert_eq!(page.items, vec!["match"]);
    assert_eq!(page.total, 1);
}

#[tokio::test]
async fn thread_store_query_zero_limit_returns_empty_terminated_page() {
    let store = MockThreadStore::default();
    store.save_thread(&Thread::with_id("t-1")).await.unwrap();
    store.save_thread(&Thread::with_id("t-2")).await.unwrap();

    let query = ThreadQuery {
        offset: 0,
        limit: 0,
        ..Default::default()
    };
    let page = store.list_threads_query(&query).await.unwrap();

    assert!(page.items.is_empty());
    assert!(!page.has_more);
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn thread_store_query_filters_root_threads() {
    let store = MockThreadStore::default();
    store
        .save_thread(&Thread::with_id("root-a").with_resource_id("resource-a"))
        .await
        .unwrap();
    store
        .save_thread(
            &Thread::with_id("child")
                .with_resource_id("resource-a")
                .with_parent_thread_id("root-a"),
        )
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("root-b").with_resource_id("resource-b"))
        .await
        .unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some("resource-a".to_string()),
            parent_filter: ThreadParentFilter::Root,
        })
        .await
        .unwrap();

    assert_eq!(page.items, vec!["root-a"]);
    assert_eq!(page.total, 1);
    assert!(!page.has_more);
}

#[tokio::test]
async fn thread_store_list_child_threads_returns_direct_children() {
    let store = MockThreadStore::default();
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child-a").with_parent_thread_id("root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("child-b").with_parent_thread_id("root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("grandchild").with_parent_thread_id("child-a"))
        .await
        .unwrap();

    let mut children = store.list_child_threads("root").await.unwrap();
    children.sort_by(|left, right| left.id.cmp(&right.id));

    assert_eq!(
        children
            .into_iter()
            .map(|thread| thread.id)
            .collect::<Vec<_>>(),
        vec!["child-a", "child-b"]
    );
}

#[tokio::test]
async fn thread_store_validate_thread_hierarchy_rejects_missing_parent() {
    let store = MockThreadStore::default();

    let err = store
        .validate_thread_hierarchy("child", Some("missing"))
        .await
        .expect_err("missing parent should be rejected");

    assert!(
        matches!(err, StorageError::Validation(message) if message == "parent thread not found: missing")
    );
}

#[tokio::test]
async fn thread_store_validate_thread_hierarchy_treats_blank_parent_as_absent() {
    let store = MockThreadStore::default();

    store
        .validate_thread_hierarchy("child", Some("   "))
        .await
        .expect("blank lineage ids should normalize to absent");
}

#[tokio::test]
async fn thread_store_validate_thread_hierarchy_rejects_cycle() {
    let store = MockThreadStore::default();
    store.save_thread(&Thread::with_id("a")).await.unwrap();
    store
        .save_thread(&Thread::with_id("b").with_parent_thread_id("a"))
        .await
        .unwrap();

    let err = store
        .validate_thread_hierarchy("a", Some("b"))
        .await
        .expect_err("cycle should be rejected");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("cycle detected")));
}

#[tokio::test]
async fn thread_store_delete_with_reject_preserves_tree() {
    let store = MockThreadStore::default();
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
        .await
        .unwrap();

    let err = store
        .delete_thread_with_strategy("root", ChildThreadDeleteStrategy::Reject)
        .await
        .expect_err("reject strategy should fail when children exist");

    assert!(matches!(err, StorageError::Validation(message) if message.contains("child threads")));
    assert!(store.load_thread("root").await.unwrap().is_some());
    assert!(store.load_thread("child").await.unwrap().is_some());
}

#[tokio::test]
async fn thread_store_delete_with_detach_clears_direct_child_parent() {
    let store = MockThreadStore::default();
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("grandchild").with_parent_thread_id("child"))
        .await
        .unwrap();

    store
        .delete_thread_with_strategy("root", ChildThreadDeleteStrategy::Detach)
        .await
        .unwrap();

    assert!(store.load_thread("root").await.unwrap().is_none());
    assert_eq!(
        store
            .load_thread("child")
            .await
            .unwrap()
            .and_then(|thread| thread.parent_thread_id),
        None
    );
    assert_eq!(
        store
            .load_thread("grandchild")
            .await
            .unwrap()
            .and_then(|thread| thread.parent_thread_id),
        Some("child".to_string())
    );
}

#[tokio::test]
async fn thread_store_delete_with_cascade_removes_descendants() {
    let store = MockThreadStore::default();
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
        .await
        .unwrap();
    store
        .save_thread(&Thread::with_id("grandchild").with_parent_thread_id("child"))
        .await
        .unwrap();

    store
        .delete_thread_with_strategy("root", ChildThreadDeleteStrategy::Cascade)
        .await
        .unwrap();

    assert!(store.load_thread("root").await.unwrap().is_none());
    assert!(store.load_thread("child").await.unwrap().is_none());
    assert!(store.load_thread("grandchild").await.unwrap().is_none());
}

#[tokio::test]
async fn thread_store_save_and_load_messages() {
    let store = MockThreadStore::default();
    let msgs = vec![
        Message::user("hello"),
        Message::assistant("hi").with_metadata(crate::contract::message::MessageMetadata {
            run_id: Some("run-1".to_string()),
            step_index: Some(0),
        }),
    ];
    store.save_messages("t-1", &msgs).await.unwrap();

    let loaded = store.load_messages("t-1").await.unwrap().unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].text(), "hello");
    let records = store.load_message_records("t-1").await.unwrap().unwrap();
    assert_eq!(records[0].thread_id, "t-1");
    assert_eq!(records[0].seq, 1);
    assert_eq!(records[1].seq, 2);
    assert_eq!(records[1].produced_by_run_id.as_deref(), Some("run-1"));
}

#[tokio::test]
async fn thread_store_default_message_query_filters_and_orders() {
    let store = MockThreadStore::default();
    let metadata = crate::contract::message::MessageMetadata {
        run_id: Some("run-1".to_string()),
        step_index: Some(0),
    };
    let msgs = vec![
        Message::user("input"),
        Message::assistant("first").with_metadata(metadata.clone()),
        Message::internal_system("hidden").with_metadata(metadata.clone()),
        Message::assistant("second").with_metadata(metadata),
    ];
    store.save_messages("t-1", &msgs).await.unwrap();

    let page = store
        .list_message_records(
            "t-1",
            &MessageQuery {
                offset: 0,
                limit: 10,
                after: Some(1),
                before: None,
                order: MessageOrder::Desc,
                visibility: MessageVisibilityFilter::External,
                run_id: Some("run-1".to_string()),
            },
        )
        .await
        .unwrap();

    let texts: Vec<String> = page
        .records
        .iter()
        .map(|record| record.message.text())
        .collect();
    assert_eq!(texts, vec!["second", "first"]);
    assert_eq!(page.total, 2);
    assert!(!page.has_more);
}

#[tokio::test]
async fn thread_store_message_query_zero_limit_returns_empty_terminated_page() {
    let store = MockThreadStore::default();
    store
        .save_messages("t-1", &[Message::user("one"), Message::assistant("two")])
        .await
        .unwrap();

    let query = MessageQuery {
        limit: 0,
        ..Default::default()
    };
    let page = store.list_message_records("t-1", &query).await.unwrap();

    assert!(page.records.is_empty());
    assert!(!page.has_more);
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn thread_store_load_messages_nonexistent() {
    let store = MockThreadStore::default();
    let result = store.load_messages("missing").await.unwrap();
    assert!(result.is_none());
}

// ── Mock RunStore ──

#[derive(Debug, Default)]
struct MockRunStore {
    runs: RwLock<HashMap<String, RunRecord>>,
}

#[async_trait]
impl RunStore for MockRunStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        let mut guard = self
            .runs
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        if guard.contains_key(&record.run_id) {
            return Err(StorageError::AlreadyExists(record.run_id.clone()));
        }
        guard.insert(record.run_id.clone(), record.clone());
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self
            .runs
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(guard.get(run_id).cloned())
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self
            .runs
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(guard
            .values()
            .filter(|r| r.thread_id == thread_id)
            .max_by_key(|r| r.updated_at)
            .cloned())
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        let guard = self
            .runs
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let mut filtered: Vec<RunRecord> = guard
            .values()
            .filter(|r| query.thread_id.as_deref().is_none_or(|t| r.thread_id == t))
            .filter(|r| query.status.is_none_or(|s| r.status == s))
            .cloned()
            .collect();
        filtered.sort_by_key(|r| r.created_at);
        let total = filtered.len();
        let offset = query.offset.min(total);
        let limit = query.limit.clamp(1, 200);
        let items: Vec<RunRecord> = filtered.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + items.len() < total;
        Ok(RunPage {
            items,
            total,
            has_more,
        })
    }
}

fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
    RunRecord {
        run_id: run_id.to_owned(),
        thread_id: thread_id.to_owned(),
        agent_id: "agent-1".to_owned(),
        parent_run_id: None,
        resolution_id: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status: RunStatus::Running,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: updated_at,
        started_at: None,
        finished_at: None,
        updated_at,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

#[tokio::test]
async fn run_store_create_and_load() {
    let store = MockRunStore::default();
    let run = make_run("run-1", "t-1", 100);
    store.create_run(&run).await.unwrap();

    let loaded = store.load_run("run-1").await.unwrap().unwrap();
    assert_eq!(loaded.thread_id, "t-1");
}

#[tokio::test]
async fn run_store_create_duplicate_errors() {
    let store = MockRunStore::default();
    let run = make_run("run-1", "t-1", 100);
    store.create_run(&run).await.unwrap();
    let err = store.create_run(&run).await.unwrap_err();
    assert!(matches!(err, StorageError::AlreadyExists(_)));
}

#[tokio::test]
async fn run_store_latest_run() {
    let store = MockRunStore::default();
    store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();
    store.create_run(&make_run("r3", "t-2", 300)).await.unwrap();

    let latest = store.latest_run("t-1").await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r2");
}

#[tokio::test]
async fn run_store_list_with_filter() {
    let store = MockRunStore::default();
    store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
    store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();
    store.create_run(&make_run("r3", "t-2", 300)).await.unwrap();

    let page = store
        .list_runs(&RunQuery {
            thread_id: Some("t-1".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 2);
}

#[test]
fn message_seq_range_rejects_empty_or_zero_based_ranges() {
    assert!(MessageSeqRange::new(0, 1).is_none());
    assert!(MessageSeqRange::new(2, 1).is_none());
    let range = MessageSeqRange::new(2, 4).unwrap();
    assert_eq!(range.len(), 3);
    assert!(!range.is_empty());
}

#[test]
fn run_record_waiting_reason_prefers_structured_state() {
    let mut run = make_run("r1", "t-1", 42);
    run.status = RunStatus::Waiting;
    run.waiting = Some(RunWaitingState {
        reason: WaitingReason::ToolPermission,
        ticket_ids: vec!["ticket-1".to_string()],
        tickets: Vec::new(),
        since_dispatch_id: None,
        message: None,
    });

    assert_eq!(run.waiting_reason(), Some(WaitingReason::ToolPermission));
    assert!(run.is_resumable_waiting());
    assert!(!run.is_background_task_waiting());
}

#[test]
fn run_record_waiting_reason_uses_structured_state() {
    let mut run = make_run("r1", "t-1", 42);
    run.status = RunStatus::Waiting;
    run.waiting = Some(RunWaitingState {
        reason: WaitingReason::BackgroundTasks,
        ticket_ids: Vec::new(),
        tickets: Vec::new(),
        since_dispatch_id: None,
        message: None,
    });
    assert_eq!(run.waiting_reason(), Some(WaitingReason::BackgroundTasks));
    assert!(run.is_background_task_waiting());

    run.waiting.as_mut().unwrap().reason = WaitingReason::ToolPermission;
    assert_eq!(run.waiting_reason(), Some(WaitingReason::ToolPermission));

    run.waiting.as_mut().unwrap().reason = WaitingReason::UserInput;
    assert_eq!(run.waiting_reason(), Some(WaitingReason::UserInput));
}

#[test]
fn run_record_done_ignores_waiting_state() {
    let mut run = make_run("r1", "t-1", 42);
    run.status = RunStatus::Done;
    run.waiting = Some(RunWaitingState {
        reason: WaitingReason::BackgroundTasks,
        ticket_ids: Vec::new(),
        tickets: Vec::new(),
        since_dispatch_id: None,
        message: None,
    });

    assert_eq!(run.waiting_reason(), None);
    assert!(!run.is_resumable_waiting());
    assert!(!run.is_background_task_waiting());
}

#[test]
fn run_request_origin_serde_roundtrip() {
    for origin in [
        RunRequestOrigin::User,
        RunRequestOrigin::A2A,
        RunRequestOrigin::Internal,
    ] {
        let json = serde_json::to_string(&origin).unwrap();
        let parsed: RunRequestOrigin = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, origin);
    }
}

// ── Query types ──

#[test]
fn message_query_default() {
    let q = MessageQuery::default();
    assert_eq!(q.offset, 0);
    assert_eq!(q.limit, 50);
}

#[test]
fn run_query_default() {
    let q = RunQuery::default();
    assert_eq!(q.offset, 0);
    assert_eq!(q.limit, 50);
    assert!(q.thread_id.is_none());
    assert!(q.status.is_none());
}

#[test]
fn run_page_serde_roundtrip() {
    let page = RunPage {
        items: vec![make_run("r1", "t-1", 100)],
        total: 1,
        has_more: false,
    };
    let json = serde_json::to_string(&page).unwrap();
    let parsed: RunPage = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.total, 1);
    assert!(!parsed.has_more);
}

#[test]
fn storage_error_display() {
    assert_eq!(
        StorageError::Validation("bad lineage".into()).to_string(),
        "validation error: bad lineage"
    );
    assert_eq!(
        StorageError::NotFound("x".into()).to_string(),
        "not found: x"
    );
    assert_eq!(
        StorageError::AlreadyExists("x".into()).to_string(),
        "already exists: x"
    );
    assert_eq!(
        StorageError::VersionConflict {
            expected: 1,
            actual: 2,
        }
        .to_string(),
        "version conflict: expected 1, actual 2"
    );
}
