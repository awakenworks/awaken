//! Integration tests for NATS protocol services using testcontainers.
//!
//! These tests spin up a real NATS server in Docker and verify that
//! explicit AG-UI and AI SDK NATS services correctly handle requests.
//!
//! Requires Docker to be running. Run with:
//! ```bash
//! cargo test --package tirea-agentos-server --test nats_gateway -- --nocapture
//! ```

use futures::StreamExt;
use serde_json::json;
use std::sync::Arc;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::nats::Nats;
use tirea_agentos::contracts::storage::{ThreadReader, ThreadStore};
use tirea_agentos::composition::AgentDefinition;
use tirea_agentos::composition::AgentOsBuilder;
use tirea_agentos_server::nats::NatsConfig;
use tirea_agentos_server::protocol;
use tirea_store_adapters::MemoryStore;

mod common;

use common::TerminatePlugin;

fn make_os(storage: Arc<dyn ThreadStore>) -> tirea_agentos::runtime::AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_test".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_test",
            Arc::new(TerminatePlugin::new("terminate_behavior_requested_test")),
        )
        .with_agent("test", def)
        .with_agent_state_store(storage)
        .build()
        .unwrap()
}

async fn start_nats() -> Option<(testcontainers::ContainerAsync<Nats>, String)> {
    let container = match Nats::default().start().await {
        Ok(container) => container,
        Err(err) => {
            eprintln!("ignoring nats_gateway test: unable to start NATS container ({err})");
            return None;
        }
    };
    let host = container.get_host().await.expect("failed to get host");
    let port = container
        .get_host_port_ipv4(4222)
        .await
        .expect("failed to get port");
    let url = format!("{host}:{port}");
    Some((container, url))
}

/// Spawn protocol services and return the NATS client for publishing test requests.
async fn setup_gateway(nats_url: &str) -> (Arc<MemoryStore>, async_nats::Client) {
    let storage = Arc::new(MemoryStore::new());
    let os = Arc::new(make_os(storage.clone()));
    spawn_protocol_services(nats_url, os).await;

    // Give the gateway a moment to subscribe.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let client = async_nats::connect(nats_url)
        .await
        .expect("failed to connect test client to NATS");

    (storage, client)
}

async fn spawn_gateway_with_storage(
    nats_url: &str,
    storage: Arc<MemoryStore>,
) -> tokio::task::JoinHandle<()> {
    let os = Arc::new(make_os(storage));
    let nats_url = nats_url.to_string();
    tokio::spawn(async move {
        spawn_protocol_services(&nats_url, os).await;
    })
}

async fn spawn_protocol_services(nats_url: &str, os: Arc<tirea_agentos::runtime::AgentOs>) {
    let nats_config = NatsConfig::new(nats_url.to_string());
    let transport = nats_config
        .connect()
        .await
        .expect("failed to connect protocol service to NATS");
    let os_for_agui = os.clone();
    let os_for_aisdk = os.clone();
    let agui_transport = transport.clone();
    let agui_subject = nats_config.ag_ui_subject.clone();
    let aisdk_subject = nats_config.ai_sdk_subject;

    tokio::spawn(async move {
        let _ = protocol::ag_ui::nats::serve(agui_transport, os_for_agui, agui_subject).await;
    });
    tokio::spawn(async move {
        let _ = protocol::ai_sdk_v6::nats::serve(transport, os_for_aisdk, aisdk_subject).await;
    });
}

async fn publish_agui_and_collect(
    client: &async_nats::Client,
    reply_subject: &str,
    thread_id: &str,
    run_id: &str,
    content: &str,
) -> Vec<String> {
    let mut sub = client.subscribe(reply_subject.to_string()).await.unwrap();
    let payload = json!({
        "agentId": "test",
        "replySubject": reply_subject,
        "request": {
            "threadId": thread_id,
            "runId": run_id,
            "messages": [{"role": "user", "content": content}],
            "tools": []
        }
    });
    client
        .publish(
            "agentos.ag-ui.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        tokio::select! {
            msg = sub.next() => {
                match msg {
                    Some(m) => {
                        let text = String::from_utf8_lossy(&m.payload).to_string();
                        events.push(text);
                        if events.last().is_some_and(|e| e.contains("RUN_FINISHED")) {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!("timed out waiting for NATS agui reply events; got {} so far: {:?}", events.len(), events);
            }
        }
    }
    events
}

// ============================================================================
// AG-UI over NATS — happy path
// ============================================================================

#[tokio::test]
async fn test_nats_agui_happy_path() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (storage, client) = setup_gateway(&nats_url).await;

    let reply_subject = "test.reply.agui.1";
    let mut sub = client.subscribe(reply_subject).await.unwrap();

    let payload = json!({
        "agentId": "test",
        "replySubject": reply_subject,
        "request": {
            "threadId": "nats-agui-1",
            "runId": "r1",
            "messages": [
                {"role": "user", "content": "hello via nats"}
            ],
            "tools": []
        }
    });

    client
        .publish(
            "agentos.ag-ui.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    // Collect reply events with a timeout.
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        tokio::select! {
            msg = sub.next() => {
                match msg {
                    Some(m) => {
                        let text = String::from_utf8_lossy(&m.payload).to_string();
                        events.push(text);
                        // RUN_FINISHED signals end.
                        if events.last().is_some_and(|e| e.contains("RUN_FINISHED")) {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!("timed out waiting for NATS agui reply events; got {} so far: {:?}", events.len(), events);
            }
        }
    }

    let all = events.join("\n");
    assert!(all.contains("RUN_STARTED"), "missing RUN_STARTED in: {all}");
    assert!(
        all.contains("RUN_FINISHED"),
        "missing RUN_FINISHED in: {all}"
    );

    // Wait for checkpoint persistence.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let saved = storage.load_thread("nats-agui-1").await.unwrap();
    assert!(saved.is_some(), "thread not persisted");
    let saved = saved.unwrap();
    assert!(
        saved
            .messages
            .iter()
            .any(|m| m.content.contains("hello via nats")),
        "user message not found in persisted thread"
    );
}

// ============================================================================
// AI SDK over NATS — happy path
// ============================================================================

#[tokio::test]
async fn test_nats_aisdk_happy_path() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (storage, client) = setup_gateway(&nats_url).await;

    let reply_subject = "test.reply.aisdk.1";
    let mut sub = client.subscribe(reply_subject).await.unwrap();

    let payload = json!({
        "agentId": "test",
        "sessionId": "nats-sdk-1",
        "input": "hi from nats sdk",
        "runId": "r1",
        "replySubject": reply_subject
    });

    client
        .publish(
            "agentos.ai-sdk.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    // Collect reply events.
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        tokio::select! {
            msg = sub.next() => {
                match msg {
                    Some(m) => {
                        let text = String::from_utf8_lossy(&m.payload).to_string();
                        events.push(text);
                        if events.last().is_some_and(|e| e.contains("\"type\":\"finish\"")) {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!("timed out waiting for NATS aisdk reply events; got {} so far: {:?}", events.len(), events);
            }
        }
    }

    let all = events.join("\n");
    assert!(
        all.contains("\"type\":\"start\""),
        "missing start in: {all}"
    );
    // text-start/text-end are lazy — only emitted when TextDelta events occur.
    // This test terminates before inference, so no text is produced.
    assert!(
        all.contains("\"type\":\"finish\""),
        "missing finish in: {all}"
    );

    // Wait for checkpoint persistence.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let saved = storage.load_thread("nats-sdk-1").await.unwrap();
    assert!(saved.is_some(), "thread not persisted");
    let saved = saved.unwrap();
    assert!(
        saved
            .messages
            .iter()
            .any(|m| m.content.contains("hi from nats sdk")),
        "user message not found in persisted thread"
    );
}

// ============================================================================
// Error paths
// ============================================================================

#[tokio::test]
async fn test_nats_agui_agent_not_found() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    let reply_subject = "test.reply.agui.err.1";
    let mut sub = client.subscribe(reply_subject).await.unwrap();

    let payload = json!({
        "agentId": "no_such_agent",
        "replySubject": reply_subject,
        "request": {
            "threadId": "t1",
            "runId": "r1",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": []
        }
    });

    client
        .publish(
            "agentos.ag-ui.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    tokio::select! {
        msg = sub.next() => {
            let m = msg.expect("expected a reply");
            let text = String::from_utf8_lossy(&m.payload).to_string();
            assert!(
                text.contains("RUN_ERROR") || text.contains("not found"),
                "expected error reply for unknown agent: {text}"
            );
        }
        _ = tokio::time::sleep_until(deadline) => {
            panic!("timed out waiting for error reply");
        }
    }
}

#[tokio::test]
async fn test_nats_aisdk_agent_not_found() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (storage, client) = setup_gateway(&nats_url).await;

    let reply_subject = "test.reply.aisdk.err.1";
    let mut sub = client.subscribe(reply_subject).await.unwrap();

    let payload = json!({
        "agentId": "no_such_agent",
        "sessionId": "s1",
        "input": "hi",
        "replySubject": reply_subject
    });

    client
        .publish(
            "agentos.ai-sdk.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    tokio::select! {
        msg = sub.next() => {
            let m = msg.expect("expected a reply");
            let text = String::from_utf8_lossy(&m.payload).to_string();
            assert!(
                text.contains("error") || text.contains("not found"),
                "expected error reply for unknown agent: {text}"
            );
        }
        _ = tokio::time::sleep_until(deadline) => {
            panic!("timed out waiting for error reply");
        }
    }

    // Unknown agent should fail fast without persisting user input.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let saved = storage.load_thread("s1").await.unwrap();
    assert!(
        saved.is_none(),
        "thread should not be persisted when agent is missing"
    );
}

#[tokio::test]
async fn test_nats_agui_bad_json() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    // Bad JSON — handler should return Err (logged), no reply expected.
    client
        .publish("agentos.ag-ui.runs", bytes::Bytes::from_static(b"not json"))
        .await
        .unwrap();

    // Give it a moment to process; no panic = success.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_nats_aisdk_bad_json() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    client
        .publish("agentos.ai-sdk.runs", bytes::Bytes::from_static(b"{bad"))
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_nats_aisdk_empty_input() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    // Valid JSON but empty input — should error out (no reply since no reply subject).
    let payload = json!({
        "agentId": "test",
        "sessionId": "s1",
        "input": "  "
    });

    client
        .publish(
            "agentos.ai-sdk.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_nats_agui_missing_reply_subject() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    // No reply subject in payload or NATS message header.
    let payload = json!({
        "agentId": "test",
        "request": {
            "threadId": "t1",
            "runId": "r1",
            "messages": [],
            "tools": []
        }
    });

    client
        .publish(
            "agentos.ag-ui.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    // Handler should error with "missing reply subject", no panic.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_nats_aisdk_missing_reply_subject() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    let payload = json!({
        "agentId": "test",
        "sessionId": "s1",
        "input": "hi"
    });

    client
        .publish(
            "agentos.ai-sdk.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

#[tokio::test]
async fn test_nats_agui_reply_subject_from_nats_reply_header() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    let reply_subject = "test.reply.agui.header.1";
    let mut sub = client.subscribe(reply_subject).await.unwrap();

    let payload = json!({
        "agentId": "test",
        "request": {
            "threadId": "nats-agui-header-reply",
            "runId": "nats-agui-header-run",
            "messages": [{"role": "user", "content": "hello header reply"}],
            "tools": []
        }
    });

    client
        .publish_with_reply(
            "agentos.ag-ui.runs",
            reply_subject.to_string(),
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut all = String::new();
    loop {
        tokio::select! {
            msg = sub.next() => {
                match msg {
                    Some(m) => {
                        let text = String::from_utf8_lossy(&m.payload).to_string();
                        all.push_str(&text);
                        all.push('\n');
                        if text.contains("RUN_FINISHED") {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!("timed out waiting for agui header-reply response; seen={all}");
            }
        }
    }

    assert!(all.contains("RUN_STARTED"), "missing RUN_STARTED: {all}");
    assert!(all.contains("RUN_FINISHED"), "missing RUN_FINISHED: {all}");
}

#[tokio::test]
async fn test_nats_aisdk_reply_subject_from_nats_reply_header() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (_storage, client) = setup_gateway(&nats_url).await;

    let reply_subject = "test.reply.aisdk.header.1";
    let mut sub = client.subscribe(reply_subject).await.unwrap();

    let payload = json!({
        "agentId": "test",
        "sessionId": "nats-aisdk-header-reply",
        "input": "hello from header reply",
        "runId": "nats-aisdk-header-run"
    });

    client
        .publish_with_reply(
            "agentos.ai-sdk.runs",
            reply_subject.to_string(),
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut all = String::new();
    loop {
        tokio::select! {
            msg = sub.next() => {
                match msg {
                    Some(m) => {
                        let text = String::from_utf8_lossy(&m.payload).to_string();
                        all.push_str(&text);
                        all.push('\n');
                        if text.contains("\"type\":\"finish\"") {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!("timed out waiting for aisdk header-reply response; seen={all}");
            }
        }
    }

    assert!(
        all.contains("\"type\":\"start\""),
        "missing start event: {all}"
    );
    assert!(
        all.contains("\"type\":\"finish\""),
        "missing finish event: {all}"
    );
}

#[tokio::test]
async fn test_nats_gateway_restart_still_handles_requests() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };

    let storage = Arc::new(MemoryStore::new());
    let handle1 = spawn_gateway_with_storage(&nats_url, storage.clone()).await;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let client = async_nats::connect(&nats_url).await.unwrap();

    let events1 = publish_agui_and_collect(
        &client,
        "test.reply.restart.1",
        "nats-restart-1",
        "restart-run-1",
        "hello-before-restart",
    )
    .await;
    let all1 = events1.join("\n");
    assert!(all1.contains("RUN_STARTED"), "missing RUN_STARTED: {all1}");
    assert!(
        all1.contains("RUN_FINISHED"),
        "missing RUN_FINISHED: {all1}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    let first_saved = storage.load_thread("nats-restart-1").await.unwrap();
    assert!(first_saved.is_some(), "first request should be persisted");

    handle1.abort();
    let _ = handle1.await;

    let handle2 = spawn_gateway_with_storage(&nats_url, storage.clone()).await;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let events2 = publish_agui_and_collect(
        &client,
        "test.reply.restart.2",
        "nats-restart-2",
        "restart-run-2",
        "hello-after-restart",
    )
    .await;
    let all2 = events2.join("\n");
    assert!(all2.contains("RUN_STARTED"), "missing RUN_STARTED: {all2}");
    assert!(
        all2.contains("RUN_FINISHED"),
        "missing RUN_FINISHED: {all2}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    let second_saved = storage.load_thread("nats-restart-2").await.unwrap();
    assert!(second_saved.is_some(), "second request should be persisted");

    handle2.abort();
    let _ = handle2.await;
}

#[tokio::test]
async fn test_nats_agui_concurrent_24_requests_all_persisted() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (storage, client) = setup_gateway(&nats_url).await;

    let total = 24usize;
    let mut handles = Vec::with_capacity(total);
    for i in 0..total {
        let client = client.clone();
        handles.push(tokio::spawn(async move {
            let thread_id = format!("nats-burst-{i}");
            let run_id = format!("burst-run-{i}");
            let reply_subject = format!("test.reply.agui.burst.{i}");
            let content = format!("hello-burst-{i}");
            let events =
                publish_agui_and_collect(&client, &reply_subject, &thread_id, &run_id, &content)
                    .await;
            (thread_id, content, events)
        }));
    }

    let mut results = Vec::with_capacity(total);
    for handle in handles {
        let (thread_id, content, events) = handle.await.expect("task should complete");
        let all = events.join("\n");
        assert!(all.contains("RUN_STARTED"), "missing RUN_STARTED: {all}");
        assert!(all.contains("RUN_FINISHED"), "missing RUN_FINISHED: {all}");
        results.push((thread_id, content));
    }

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    for (thread_id, content) in results {
        let saved = storage
            .load_thread(&thread_id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("thread should be persisted: {thread_id}"));
        assert!(
            saved.messages.iter().any(|m| m.content == content),
            "persisted thread {thread_id} missing content {content}"
        );
    }
}

#[tokio::test]
async fn test_nats_agui_slow_consumer_still_finishes_and_persists() {
    let Some((_container, nats_url)) = start_nats().await else {
        return;
    };
    let (storage, client) = setup_gateway(&nats_url).await;

    let reply_subject = "test.reply.agui.slow.1";
    let mut sub = client.subscribe(reply_subject).await.unwrap();
    let payload = json!({
        "agentId": "test",
        "replySubject": reply_subject,
        "request": {
            "threadId": "nats-slow-consumer",
            "runId": "nats-slow-run",
            "messages": [{"role": "user", "content": "hello slow"}],
            "tools": []
        }
    });

    client
        .publish(
            "agentos.ag-ui.runs",
            serde_json::to_vec(&payload).unwrap().into(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut all = String::new();
    loop {
        tokio::select! {
            msg = sub.next() => {
                match msg {
                    Some(m) => {
                        let text = String::from_utf8_lossy(&m.payload).to_string();
                        all.push_str(&text);
                        all.push('\n');
                        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                        if text.contains("RUN_FINISHED") {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                panic!("timed out waiting for slow-consumer completion; seen={all}");
            }
        }
    }

    assert!(all.contains("RUN_STARTED"), "missing RUN_STARTED: {all}");
    assert!(all.contains("RUN_FINISHED"), "missing RUN_FINISHED: {all}");

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let saved = storage
        .load_thread("nats-slow-consumer")
        .await
        .unwrap()
        .expect("thread should be persisted");
    assert!(
        saved.messages.iter().any(|m| m.content == "hello slow"),
        "persisted thread missing user message"
    );
}
