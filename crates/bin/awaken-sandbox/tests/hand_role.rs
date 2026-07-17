//! The `hand` role end-to-end, in-process: bind the real hand on a unix socket, dial it
//! as the brain would (`DialAddr::Unix`), and run a REAL tool (`bash`) on it — proving
//! `awaken-sandbox hand` serves the executable hand tools over the transport that
//! crosses a `--network none` sandbox boundary (C5). The transport primitives and
//! `serve_hand` are tested elsewhere; this proves the role's compose (bind + accept +
//! serve loop + tool dispatch) works with the actual binary's wiring.
#![cfg(feature = "hand")]

use std::time::Duration;

use awaken_connection_plan::{ChannelFactory, ConnectionPlan, TokioChannelFactory};
use awaken_runtime_contract::llm::ToolCall;
use awaken_sandbox::hand::{HandBind, serve};
use awaken_tool_relay::{RemoteToolExecutor, wire::HandResult};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hand_role_runs_a_real_tool_over_a_unix_socket() {
    let dir = std::env::temp_dir().join(format!("awaken-hand-role-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let sock = dir.join("hand.sock");
    let sock_str = sock.to_string_lossy().into_owned();

    // Start the hand role (the same code path `awaken-sandbox hand --unix <path>` runs).
    let serve_path = sock_str.clone();
    let hand = tokio::spawn(async move { serve(HandBind::Unix(serve_path)).await });

    // Brain side: dial the unix socket, retrying while the hand binds.
    let mut channel = None;
    for _ in 0..100 {
        if let Ok(ch) = TokioChannelFactory
            .connect(&ConnectionPlan::unix_dial(&sock_str))
            .await
        {
            channel = Some(ch);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let channel = channel.expect("dial the hand's unix socket");

    // Run a real `bash` tool ON THE HAND and read its output back over the channel.
    let executor = RemoteToolExecutor::new(channel);
    let call = ToolCall {
        call_id: "c1".into(),
        tool_id: "bash".into(),
        arguments: serde_json::json!({ "command": "echo hand-ran-the-tool" }),
    };
    let result = executor.call_hand(&call).await;

    hand.abort();
    let _ = std::fs::remove_dir_all(&dir);

    match result {
        HandResult::Ok { output } => {
            let rendered = serde_json::to_string(&output).expect("output serializes");
            assert!(
                rendered.contains("hand-ran-the-tool"),
                "the hand executed bash and returned its output: {rendered}"
            );
        }
        other => panic!("expected the hand to run bash, got {other:?}"),
    }
}
