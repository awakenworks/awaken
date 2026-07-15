//! Integration test: two Workdir sandboxes' rooted tools are isolated, and a path
//! escape fails closed — exercised through the real built-in `read`/`write` over the
//! pc `SandboxProvider` (`LocalProvider` → `LocalSandbox::rooted_tools`).

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::Sandbox as _;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use awaken_sandbox_local::{LocalProvider, LocalSandbox};

/// A bare Workdir spec, egress denied when `deny` (carried on the opaque `extra`).
fn spec(scope: &str, deny: bool) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: deny.then(|| serde_json::json!({ "deny_egress": true })),
    }
}

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
async fn sandboxes_are_isolated_and_escapes_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    let provider = LocalProvider::new(base);

    let env_a: LocalSandbox = provider.create_sandbox(&spec("A", false)).await.unwrap();
    let env_b: LocalSandbox = provider.create_sandbox(&spec("B", false)).await.unwrap();
    let tools_a = env_a.rooted_tools();
    let tools_b = env_b.rooted_tools();

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

    env_a.dispose().await.unwrap();
    env_b.dispose().await.unwrap();
}

/// True only when bwrap + unprivileged userns work AND this host can actually reach
/// external DNS — both are needed to demonstrate the on/off egress difference. When
/// either is missing the egress test self-skips (mirrors the namespace-tier probe).
async fn bwrap_and_net_available() -> bool {
    use std::process::Stdio;
    let bwrap_ok = tokio::process::Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !bwrap_ok {
        return false;
    }
    tokio::process::Command::new("getent")
        .args(["hosts", "example.com"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `deny_egress` is real: the same bash command that resolves an external host in an
/// unrestricted sandbox cannot reach the network in a `deny_egress` one (it runs
/// inside a `bwrap --unshare-net` namespace).
#[tokio::test]
async fn deny_egress_blocks_bash_network_but_unrestricted_allows_it() {
    if !bwrap_and_net_available().await {
        eprintln!("skipping: bwrap/userns or host DNS unavailable on this host");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let probe = "getent hosts example.com >/dev/null 2>&1 && echo NET-UP || echo NET-DOWN";

    // Egress denied → the bash tool has no route to the network.
    let denied = provider.create_sandbox(&spec("iso", true)).await.unwrap();
    let out = invoke(
        &denied.rooted_tools(),
        "bash",
        serde_json::json!({ "command": probe }),
    )
    .await
    .unwrap();
    assert!(
        out.content.contains("NET-DOWN"),
        "deny_egress must block network: {}",
        out.content
    );

    // Same command, egress allowed → resolution works (proves the flag is the cause).
    let allowed = provider.create_sandbox(&spec("open", false)).await.unwrap();
    let out = invoke(
        &allowed.rooted_tools(),
        "bash",
        serde_json::json!({ "command": probe }),
    )
    .await
    .unwrap();
    assert!(
        out.content.contains("NET-UP"),
        "unrestricted egress must reach the network: {}",
        out.content
    );
}
