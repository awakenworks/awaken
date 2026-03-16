mod common;

use std::sync::Arc;

use common::TerminatePlugin;
use serde_json::{json, Value};
use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
use tirea_agentos::runtime::AgentOs;
use tirea_agentos_server::protocol::acp::stdio::serve_io;
use tirea_store_adapters::MemoryStore;

fn make_os() -> Arc<AgentOs> {
    let store = Arc::new(MemoryStore::new());
    let def = AgentDefinition {
        id: "test".to_string(),
        behavior_ids: vec!["terminate_acp_stdio".into()],
        ..Default::default()
    };
    let os = AgentOsBuilder::new()
        .with_registered_behavior(
            "terminate_acp_stdio",
            Arc::new(TerminatePlugin::new("terminate_acp_stdio")),
        )
        .with_agent_spec(AgentDefinitionSpec::local_with_id("test", def))
        .with_agent_state_store(store)
        .build()
        .expect("build AgentOs");
    Arc::new(os)
}

fn jsonrpc_line(method: &str, params: Value, id: Option<Value>) -> String {
    let mut obj = json!({"jsonrpc": "2.0", "method": method, "params": params});
    if let Some(id) = id {
        obj["id"] = id;
    }
    let mut s = serde_json::to_string(&obj).unwrap();
    s.push('\n');
    s
}

/// Run `serve_io` with the given input string and collect all JSON output lines.
async fn run_stdio_session(os: Arc<AgentOs>, input: &str) -> Vec<Value> {
    let input_cursor = std::io::Cursor::new(input.as_bytes().to_vec());
    let (duplex_writer, duplex_reader) = tokio::io::duplex(64 * 1024);

    let serve_handle = tokio::spawn(async move {
        serve_io(os, input_cursor, duplex_writer).await;
    });

    // Read output lines from the reader side of the duplex.
    let read_handle = tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(duplex_reader);
        let mut lines = Vec::new();
        let mut buf = String::new();
        loop {
            buf.clear();
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                reader.read_line(&mut buf),
            )
            .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {
                    let trimmed = buf.trim();
                    if !trimmed.is_empty() {
                        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                            lines.push(v);
                        }
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break, // timeout
            }
        }
        lines
    });

    let _ = serve_handle.await;
    read_handle.await.unwrap()
}

#[tokio::test]
async fn session_start_emits_update_and_finished() {
    let os = make_os();
    let input = jsonrpc_line(
        "session/start",
        json!({"agent_id": "test", "messages": [{"role": "user", "content": "hi"}]}),
        Some(json!(1)),
    );

    let lines = run_stdio_session(os, &input).await;

    assert!(!lines.is_empty(), "expected output lines but got none");

    for line in &lines {
        assert_eq!(line["jsonrpc"], "2.0");
        assert!(line.get("method").is_some());
    }

    let finished_line = lines
        .iter()
        .filter(|l| l["method"] == "session/update")
        .find(|l| !l["params"]["finished"].is_null());
    assert!(
        finished_line.is_some(),
        "expected a finished event, got: {lines:?}"
    );
}

#[tokio::test]
async fn unknown_agent_returns_error() {
    let os = make_os();
    let input = jsonrpc_line(
        "session/start",
        json!({"agent_id": "nonexistent", "messages": []}),
        Some(json!(42)),
    );

    let lines = run_stdio_session(os, &input).await;

    assert!(!lines.is_empty());
    let err = &lines[0];
    assert_eq!(err["jsonrpc"], "2.0");
    assert_eq!(err["id"], 42);
    assert_eq!(err["error"]["code"], -32001);
}

#[tokio::test]
async fn unknown_method_returns_error() {
    let os = make_os();
    let input = jsonrpc_line("foo/bar", json!({}), Some(json!(7)));

    let lines = run_stdio_session(os, &input).await;

    assert!(!lines.is_empty());
    let err = &lines[0];
    assert_eq!(err["error"]["code"], -32601);
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("foo/bar"));
}

#[tokio::test]
async fn invalid_json_returns_parse_error() {
    let os = make_os();
    let input = "not valid json\n";

    let lines = run_stdio_session(os, input).await;

    assert!(!lines.is_empty());
    assert_eq!(lines[0]["error"]["code"], -32700);
}

#[tokio::test]
async fn invalid_session_start_params_returns_error() {
    let os = make_os();
    let input = jsonrpc_line("session/start", json!({"messages": []}), Some(json!(3)));

    let lines = run_stdio_session(os, &input).await;

    assert!(!lines.is_empty());
    assert_eq!(lines[0]["error"]["code"], -32602);
    assert_eq!(lines[0]["id"], 3);
}
