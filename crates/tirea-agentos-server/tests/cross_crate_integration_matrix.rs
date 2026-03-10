use futures::StreamExt;
use std::sync::Arc;
use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea_agentos::contracts::storage::ThreadReader;
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::runtime::AgentOs;
use tirea_agentos_server::transport::{Endpoint, RuntimeEndpoint, TranscoderEndpoint};
use tirea_protocol_ai_sdk_v6::{AiSdkEncoder, AiSdkV6RunRequest};
use tirea_store_adapters::MemoryStore;

mod common;

use common::TerminatePlugin;

fn make_os(store: Arc<MemoryStore>) -> AgentOs {
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_behavior_requested_cross_crate_matrix".into()],
        ..Default::default()
    };

    AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_behavior_requested_cross_crate_matrix",
            Arc::new(TerminatePlugin::new(
                "terminate_behavior_requested_cross_crate_matrix",
            )),
        )
        .with_agent_spec(AgentDefinitionSpec::local_with_id("test", def))
        .with_agent_state_store(store)
        .build()
        .expect("failed to build AgentOs")
}

#[tokio::test]
async fn cross_crate_integration_matrix_24() {
    let store = Arc::new(MemoryStore::new());
    let os = make_os(store.clone());

    let thread_cases = ["", " ", "cross-a", "cross-b", "session-42", "\t"];
    let input_cases = ["hello", "input-42", "{\"x\":1}", "line1\\nline2"];

    let mut executed = 0usize;

    for thread_id in thread_cases {
        for input in input_cases {
            let req = AiSdkV6RunRequest::from_thread_input(thread_id, input);
            let run_request: RunRequest = req.into_runtime_run_request("test".to_string());

            let run = os
                .run_stream(run_request)
                .await
                .expect("run_stream should succeed");

            let resolved_thread_id = run.thread_id.clone();
            let resolved_run_id = run.run_id.clone();

            let encoder = AiSdkEncoder::new();
            let runtime_ep = Arc::new(RuntimeEndpoint::from_run_stream(run));
            let transcoder = TranscoderEndpoint::new(runtime_ep, encoder);

            let stream = transcoder.recv().await.expect("transcoder recv");
            let encoded: Vec<serde_json::Value> = stream
                .map(|r| {
                    serde_json::to_value(r.expect("stream item")).expect("must be serializable")
                })
                .collect()
                .await;

            assert!(
                encoded
                    .iter()
                    .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("start")),
                "missing ai-sdk start event for thread={resolved_thread_id}, run={resolved_run_id}"
            );
            assert!(
                encoded
                    .iter()
                    .any(|e| e.get("type").and_then(|v| v.as_str()) == Some("finish")),
                "missing ai-sdk finish event for thread={resolved_thread_id}, run={resolved_run_id}"
            );

            if thread_id.trim().is_empty() {
                assert!(
                    !resolved_thread_id.trim().is_empty(),
                    "auto-generated thread id must not be empty"
                );
            } else {
                assert_eq!(resolved_thread_id, thread_id);
            }

            assert!(
                !resolved_run_id.trim().is_empty(),
                "resolved run id must not be empty"
            );

            let saved = store
                .load_thread(&resolved_thread_id)
                .await
                .expect("load should not fail")
                .expect("thread must be persisted");
            assert!(
                saved.messages.iter().any(|m| m.content == input),
                "persisted thread should contain user input"
            );

            executed += 1;
        }
    }

    assert_eq!(executed, 24, "cross-crate scenario count drifted");
}
