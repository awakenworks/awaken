//! Integration test: two environments' rooted tools are isolated, and a path
//! escape fails closed — exercised through the real built-in `read`/`write`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use awaken_sandbox_local::{LocalSandboxProvider, SandboxProvider, SandboxSpec, rooted_hand_tools};

static SEQ: AtomicU64 = AtomicU64::new(0);

async fn invoke(
    tools: &[Arc<dyn RawTool>],
    tool_id: &str,
    args: serde_json::Value,
) -> Result<ToolOutput, ToolError> {
    let tool = tools
        .iter()
        .find(|t| t.id() == tool_id)
        .expect("tool present");
    tool.invoke(ToolCall {
        call_id: "c".into(),
        tool_id: tool_id.into(),
        arguments: args,
    })
    .await
}

#[tokio::test]
async fn environments_are_isolated_and_escapes_fail_closed() {
    let base = std::env::temp_dir().join(format!(
        "awaken-sbx-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let provider = LocalSandboxProvider::new(&base);

    let env_a = provider.create(&SandboxSpec::new("A")).await.unwrap();
    let env_b = provider.create(&SandboxSpec::new("B")).await.unwrap();
    let tools_a = rooted_hand_tools(env_a.root.clone());
    let tools_b = rooted_hand_tools(env_b.root.clone());

    // A writes a secret via the rooted write tool; it lands inside A's root.
    invoke(
        &tools_a,
        "write",
        serde_json::json!({ "path": "secret.txt", "content": "A-SECRET" }),
    )
    .await
    .unwrap();
    assert!(base.join("A/secret.txt").exists());
    assert!(!base.join("B/secret.txt").exists());

    // A can read its own file back.
    let read_a = invoke(
        &tools_a,
        "read",
        serde_json::json!({ "path": "secret.txt" }),
    )
    .await
    .unwrap();
    assert!(read_a.content.contains("A-SECRET"));

    // B cannot see A's file by the same logical path (isolation).
    assert!(
        invoke(
            &tools_b,
            "read",
            serde_json::json!({ "path": "secret.txt" })
        )
        .await
        .is_err()
    );

    // B cannot climb out to reach A's file — the escape fails closed before any IO.
    let escape = invoke(
        &tools_b,
        "read",
        serde_json::json!({ "path": "../A/secret.txt" }),
    )
    .await;
    assert!(escape.is_err(), "escape should fail closed");

    // An absolute path is rebased under B's root, not the host root.
    assert!(
        invoke(
            &tools_b,
            "read",
            serde_json::json!({ "path": "/etc/hostname" })
        )
        .await
        .is_err()
    );

    provider.teardown("A").await.unwrap();
    provider.teardown("B").await.unwrap();
}
