//! The `hand` role's REVERSE-DIAL transport end-to-end, in-process: stand up a local TCP
//! listener as the brain rendezvous, start the hand in `--dial` mode so it dials BACK out
//! to us (the topology a k8s egress-fenced Pod uses when the brain cannot dial in), accept
//! that outbound connection, and run a REAL tool (`bash`) over it. Mirrors the
//! `HandBind::Unix` itest but exercises `HandBind::Dial` + `connect_with_retry`: it proves
//! the role's reverse-dial compose (dial-out + serve loop + tool dispatch) works with the
//! actual binary's wiring. The transport primitives are tested in awaken-connection-plan.
#![cfg(feature = "hand")]

use awaken_connection_plan::{ConnectionPlan, bind_tcp};
use awaken_runtime_contract::llm::ToolCall;
use awaken_sandbox::hand::{HandBind, serve};
use awaken_tool_relay::{RemoteToolExecutor, wire::HandResult};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hand_role_reverse_dials_the_brain_and_serves_a_real_tool() {
    // Brain rendezvous: bind a local TCP listener the hand will reverse-dial back to.
    // Port 0 → the OS picks a free port; the hand dials the concrete address.
    let rendezvous = bind_tcp(&ConnectionPlan::tcp_listen("127.0.0.1:0"))
        .await
        .expect("bind the brain rendezvous");
    let addr = rendezvous
        .local_addr()
        .expect("rendezvous addr")
        .to_string();

    // Start the hand role in reverse-dial mode — the exact path
    // `awaken-sandbox hand --dial <addr>` runs. It dials OUT to our rendezvous.
    let hand = tokio::spawn(async move { serve(HandBind::Dial(addr)).await });

    // Accept the hand's outbound dial-back. `accept()` blocks until the hand connects,
    // so this is the readiness signal (no sleep): reaching it proves the reverse
    // connection was established and is being served.
    let channel = rendezvous
        .accept()
        .await
        .expect("the hand reverse-dials into the brain rendezvous");

    // Run a real `bash` tool ON THE HAND over the reverse-dialed connection.
    let executor = RemoteToolExecutor::new(channel);
    let call = ToolCall {
        call_id: "c1".into(),
        tool_id: "bash".into(),
        arguments: serde_json::json!({ "command": "echo reverse-dial-ran-the-tool" }),
    };
    let result = executor.call_hand(&call).await;

    hand.abort();

    match result {
        HandResult::Ok { output } => {
            let rendered = serde_json::to_string(&output).expect("output serializes");
            assert!(
                rendered.contains("reverse-dial-ran-the-tool"),
                "the reverse-dialed hand executed bash and returned its output: {rendered}"
            );
        }
        other => panic!("expected the reverse-dialed hand to run bash, got {other:?}"),
    }
}
