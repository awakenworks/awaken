#![allow(deprecated)] // ADR-0038 D7: integration tests exercise the legacy checkpoint API directly
#![allow(dead_code)]

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::{Message, MessageMetadata};
use awaken_contract::contract::storage::{
    ChildThreadDeleteStrategy, MessageOrder, MessageQuery, MessageVisibilityFilter, RunRecord,
    RunRequestSnapshot, StorageError, ThreadParentFilter, ThreadQuery, ThreadRunStore,
};
use awaken_contract::thread::Thread;

pub fn make_run(run_id: &str, thread_id: &str, status: RunStatus) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: "agent".to_string(),
        parent_run_id: None,
        registry_manifest: None,
        activation: None,
        request: None,
        input: None,
        output: None,
        status,
        termination_reason: None,
        final_output: None,
        error_payload: None,
        dispatch_id: None,
        session_id: None,
        transport_request_id: None,
        waiting: None,
        outcome: None,
        created_at: 1,
        started_at: None,
        finished_at: None,
        updated_at: 1,
        steps: 0,
        input_tokens: 0,
        output_tokens: 0,
        state: None,
    }
}

pub async fn checkpoint_persists_messages_and_run<S: ThreadRunStore>(store: &S) {
    let thread_id = "t-ckpt";
    let messages = vec![Message::user("hello"), Message::assistant("hi there")];
    let run = make_run("r1", thread_id, RunStatus::Done);
    store.checkpoint(thread_id, &messages, &run).await.unwrap();

    let loaded_msgs = store.load_messages(thread_id).await.unwrap().unwrap();
    assert_eq!(loaded_msgs.len(), 2);
    let loaded_run = store.load_run("r1").await.unwrap().unwrap();
    assert_eq!(loaded_run.run_id, "r1");
    assert_eq!(loaded_run.thread_id, thread_id);
}

pub async fn load_messages_returns_none_for_unknown_thread<S: ThreadRunStore>(store: &S) {
    let result = store.load_messages("unknown-thread").await.unwrap();
    assert!(result.is_none() || result.unwrap().is_empty());
}

pub async fn latest_run_returns_most_recent<S: ThreadRunStore>(store: &S) {
    let thread_id = "t-latest";
    let r1 = RunRecord {
        created_at: 100,
        updated_at: 100,
        ..make_run("r1", thread_id, RunStatus::Done)
    };
    let r2 = RunRecord {
        created_at: 200,
        updated_at: 200,
        ..make_run("r2", thread_id, RunStatus::Done)
    };
    store.checkpoint(thread_id, &[], &r1).await.unwrap();
    store.checkpoint(thread_id, &[], &r2).await.unwrap();
    let latest = store.latest_run(thread_id).await.unwrap().unwrap();
    assert_eq!(latest.run_id, "r2");
}

pub async fn checkpoint_overwrites_messages<S: ThreadRunStore>(store: &S) {
    let thread_id = "t-overwrite";
    let r = make_run("r1", thread_id, RunStatus::Created);
    store
        .checkpoint(thread_id, &[Message::user("first")], &r)
        .await
        .unwrap();
    store
        .checkpoint(
            thread_id,
            &[Message::user("first"), Message::assistant("second")],
            &r,
        )
        .await
        .unwrap();
    let msgs = store.load_messages(thread_id).await.unwrap().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1].text(), "second");
}

pub async fn load_thread_reflects_checkpoint<S: ThreadRunStore>(store: &S) {
    let thread_id = "t-meta";
    let r = make_run("r1", thread_id, RunStatus::Done);
    store.checkpoint(thread_id, &[], &r).await.unwrap();
    let thread = store.load_thread(thread_id).await.unwrap();
    assert!(thread.is_some());
    assert_eq!(thread.unwrap().id, thread_id);
}

pub async fn append_message_records_assigns_seq<S: ThreadRunStore>(store: &S) {
    let thread_id = "t-append";
    let records = store
        .append_message_records(thread_id, &[Message::user("a"), Message::user("b")])
        .await
        .unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].seq, 1);
    assert_eq!(records[1].seq, 2);
}

pub async fn list_threads_query_filters_lineage<S: ThreadRunStore>(store: &S) {
    let mut matching = Thread::with_id("t-filter-match")
        .with_resource_id("resource-a")
        .with_parent_thread_id("parent-1");
    matching.metadata.updated_at = Some(300);
    let mut wrong_resource = Thread::with_id("t-filter-resource")
        .with_resource_id("resource-b")
        .with_parent_thread_id("parent-1");
    wrong_resource.metadata.updated_at = Some(200);
    let mut wrong_parent = Thread::with_id("t-filter-parent")
        .with_resource_id("resource-a")
        .with_parent_thread_id("parent-2");
    wrong_parent.metadata.updated_at = Some(100);

    store.save_thread(&matching).await.unwrap();
    store.save_thread(&wrong_resource).await.unwrap();
    store.save_thread(&wrong_parent).await.unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some("resource-a".to_string()),
            parent_filter: ThreadParentFilter::Parent("parent-1".to_string()),
        })
        .await
        .unwrap();

    assert_eq!(page.items, vec!["t-filter-match"]);
    assert_eq!(page.total, 1);
    assert!(!page.has_more);
}

pub async fn list_threads_query_filters_root_threads<S: ThreadRunStore>(store: &S) {
    let mut matching_root = Thread::with_id("t-root-match").with_resource_id("resource-a");
    matching_root.metadata.updated_at = Some(300);
    let mut matching_child = Thread::with_id("t-root-child")
        .with_resource_id("resource-a")
        .with_parent_thread_id("parent-1");
    matching_child.metadata.updated_at = Some(200);
    let mut wrong_resource_root = Thread::with_id("t-root-other").with_resource_id("resource-b");
    wrong_resource_root.metadata.updated_at = Some(100);

    store.save_thread(&matching_root).await.unwrap();
    store.save_thread(&matching_child).await.unwrap();
    store.save_thread(&wrong_resource_root).await.unwrap();

    let page = store
        .list_threads_query(&ThreadQuery {
            offset: 0,
            limit: 10,
            resource_id: Some("resource-a".to_string()),
            parent_filter: ThreadParentFilter::Root,
        })
        .await
        .unwrap();

    assert_eq!(page.items, vec!["t-root-match"]);
    assert_eq!(page.total, 1);
    assert!(!page.has_more);
}

pub async fn checkpoint_rejects_missing_parent_thread<S: ThreadRunStore>(store: &S) {
    let run = RunRecord {
        request: Some(RunRequestSnapshot {
            parent_thread_id: Some("missing-parent".to_string()),
            ..Default::default()
        }),
        ..make_run("r-missing-parent", "t-missing-parent", RunStatus::Created)
    };

    let error = store
        .checkpoint("t-missing-parent", &[], &run)
        .await
        .expect_err("checkpoint should reject unknown parent thread");

    assert!(
        matches!(error, StorageError::Validation(message) if message == "parent thread not found: missing-parent")
    );
}

pub async fn checkpoint_rejects_cycle_parent_assignment<S: ThreadRunStore>(store: &S) {
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
        .await
        .unwrap();

    let run = RunRecord {
        request: Some(RunRequestSnapshot {
            parent_thread_id: Some("child".to_string()),
            ..Default::default()
        }),
        ..make_run("r-cycle-parent", "root", RunStatus::Created)
    };

    let error = store
        .checkpoint("root", &[], &run)
        .await
        .expect_err("checkpoint should reject cycles");

    assert!(
        matches!(error, StorageError::Validation(message) if message.contains("cycle detected"))
    );
}

pub async fn delete_thread_with_detach_preserves_children<S: ThreadRunStore>(store: &S) {
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

pub async fn delete_thread_with_reject_preserves_tree<S: ThreadRunStore>(store: &S) {
    store.save_thread(&Thread::with_id("root")).await.unwrap();
    store
        .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
        .await
        .unwrap();

    let error = store
        .delete_thread_with_strategy("root", ChildThreadDeleteStrategy::Reject)
        .await
        .expect_err("reject strategy should fail");

    assert!(
        matches!(error, StorageError::Validation(message) if message.contains("child threads"))
    );
    assert!(store.load_thread("root").await.unwrap().is_some());
    assert!(store.load_thread("child").await.unwrap().is_some());
}

pub async fn delete_thread_with_cascade_removes_descendants<S: ThreadRunStore>(store: &S) {
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

pub async fn list_message_records_query_filters_and_orders<S: ThreadRunStore>(store: &S) {
    let thread_id = "t-message-query";
    store
        .save_thread(&Thread::with_id(thread_id))
        .await
        .unwrap();
    let run_metadata = MessageMetadata {
        run_id: Some("run-1".to_string()),
        step_index: Some(0),
    };
    let messages = vec![
        Message::user("input"),
        Message::assistant("first").with_metadata(run_metadata.clone()),
        Message::internal_system("hidden").with_metadata(run_metadata.clone()),
        Message::assistant("second").with_metadata(run_metadata),
    ];
    store.save_messages(thread_id, &messages).await.unwrap();

    let page = store
        .list_message_records(
            thread_id,
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

pub async fn load_run_returns_none_for_unknown<S: ThreadRunStore>(store: &S) {
    assert!(store.load_run("nonexistent-run").await.unwrap().is_none());
}
