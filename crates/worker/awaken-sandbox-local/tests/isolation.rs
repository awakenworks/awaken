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

/// bwrap userns works AND a real `bash` is on PATH (the loopback probe uses `bash`'s
/// `/dev/tcp`, which `sh`/dash lacks). Both are needed; self-skips otherwise — same
/// discipline as the DNS variant, but with NO external dependency (deterministic).
async fn bwrap_and_bash_available() -> bool {
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
    tokio::process::Command::new("bash")
        .args(["-c", "exit 0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Deterministic twin of `deny_egress_blocks_bash_network_but_unrestricted_allows_it`
/// with NO live DNS: a real host loopback listener is the target, so the assertion turns
/// only on whether the sandbox shares the host network namespace or gets a fresh
/// (down-loopback) one. The kernel completes the handshake into the listener's backlog,
/// so a reachable `/dev/tcp` connect succeeds — proving the netns, not a dead port, is
/// what denies the `deny_egress` bash tool. Mirrors the `pairwise_matrix` loopback proof.
#[tokio::test]
async fn deny_egress_blocks_a_host_loopback_listener_deterministically() {
    if !bwrap_and_bash_available().await {
        eprintln!("skipping: bwrap/userns or bash unavailable on this host");
        return;
    }
    // A real listening socket on the host loopback (std, so no tokio `net` feature).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // `bash -c` for `/dev/tcp` (the deny path execs `/bin/sh -c`, which may be dash).
    let probe = format!(
        "bash -c '(exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo NET-UP || echo NET-DOWN'"
    );

    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());

    // Egress denied → the rooted bash runs inside `bwrap --unshare-net`; the fresh netns
    // has only a down loopback, so the host's 127.0.0.1:port is unreachable.
    let denied = provider
        .create_sandbox(&spec("iso-lo", true))
        .await
        .unwrap();
    let out = invoke(
        &denied.rooted_tools(),
        "bash",
        serde_json::json!({ "command": probe }),
    )
    .await
    .unwrap();
    assert!(
        out.content.contains("NET-DOWN"),
        "deny_egress must not reach the host loopback listener: {}",
        out.content
    );

    // Egress allowed → shares the host netns → the same listener is reachable (proves the
    // netns unshare, not the port, is the cause).
    let allowed = provider
        .create_sandbox(&spec("open-lo", false))
        .await
        .unwrap();
    let out = invoke(
        &allowed.rooted_tools(),
        "bash",
        serde_json::json!({ "command": probe }),
    )
    .await
    .unwrap();
    assert!(
        out.content.contains("NET-UP"),
        "unrestricted egress must reach the host loopback listener: {}",
        out.content
    );
    drop(listener);
}

/// CHARACTERIZATION — the Workdir bash jail is *lexical*, not enforced. `jail_args`
/// only prefixes `cd '<root>' && <cmd>`; it does not rebase the paths inside a bash
/// command, so the command can `cd /` (or `cd ..`) and operate entirely outside the
/// sandbox root. This documents the current behavior; the path-tools (`read`/`write`/…)
/// ARE jailed (proven above) — only `bash` is escapable, because it runs an opaque shell.
///
// KNOWN BUG (adjudicate): the Workdir tier's `deny_egress = false` bash tool does not
// confine the command to the root — a `cd /` / `cd ..` reaches host paths above the
// jail. This is why an OPAQUE agent is refused on the Workdir tier and must use the
// Namespace/Container tier (OS-enforced); a trusted single-machine `bash` is the
// documented ceiling here. Enforcing it would require running every Workdir bash under
// a namespace (as the `deny_egress = true` path already does via `bwrap`).
#[tokio::test]
async fn workdir_bash_cd_escapes_the_lexical_jail() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path();
    // A secret ABOVE the sandbox root, on the host — never inside any jail.
    std::fs::write(base.join("secret-above.txt"), "ABOVE-ROOT-SECRET").unwrap();

    let provider = LocalProvider::new(base);
    let sandbox = provider.create_sandbox(&spec("jail", false)).await.unwrap();
    let tools = sandbox.rooted_tools();

    // `cd /` wins: the effective command is `cd '<base>/jail' && cd / && pwd`, so pwd is
    // the host root, not the sandbox root — the lexical `cd` prefix is overridden.
    let pwd = invoke(
        &tools,
        "bash",
        serde_json::json!({ "command": "cd / && pwd" }),
    )
    .await
    .unwrap();
    assert_eq!(
        pwd.content.trim(),
        "/",
        "cd / escapes the lexical Workdir jail (KNOWN BUG): {}",
        pwd.content
    );

    // Worse: a relative climb reads a host file ABOVE the root — a real boundary escape
    // the path-tools' fail-closed `..` rejection does NOT cover for bash.
    let leak = invoke(
        &tools,
        "bash",
        serde_json::json!({ "command": "cat ../secret-above.txt" }),
    )
    .await
    .unwrap();
    assert!(
        leak.content.contains("ABOVE-ROOT-SECRET"),
        "bash `cd ..`/relative path reads above the root (KNOWN BUG): {}",
        leak.content
    );

    sandbox.dispose().await.unwrap();
}
