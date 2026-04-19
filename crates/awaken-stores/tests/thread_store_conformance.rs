#![allow(dead_code)]

use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, ThreadRunStore};

pub fn make_run(run_id: &str, thread_id: &str, status: RunStatus) -> RunRecord {
    RunRecord {
        run_id: run_id.to_string(),
        thread_id: thread_id.to_string(),
        agent_id: "agent".to_string(),
        parent_run_id: None,
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

pub async fn load_run_returns_none_for_unknown<S: ThreadRunStore>(store: &S) {
    assert!(store.load_run("nonexistent-run").await.unwrap().is_none());
}
