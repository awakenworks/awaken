//! Integration: [`SandboxChannelSource`] launches a REAL agent process under
//! bubblewrap and drives it over ACP to a terminal phase — and the egress policy
//! is enforced by the OS, not by convention: the same in-agent network probe that
//! reaches a host loopback listener from an unrestricted sandbox cannot reach it
//! from a deny-egress one (`--unshare-net` gives the agent its own, empty network
//! namespace — even loopback is not the host's).
//!
//! Both tests self-skip when bwrap/unprivileged userns is unavailable on this
//! host (mirrors `awaken-sandbox-local/tests/isolation.rs`).

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_run_executor_acp::{AcpLaunch, AcpRunExecutor};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{SandboxChannelSource, ThreadEgress};
use awaken_store_inmem::MemoryCommitCoordinator;

async fn bwrap_available() -> bool {
    use std::process::Stdio;
    tokio::process::Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

fn activation(thread: &str) -> RunActivation {
    RunActivation {
        run_id: RunId(format!("run-{thread}")),
        thread_id: ThreadId(thread.to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "be helpful".into(),
                max_steps: 8,
                model_binding: ModelBinding::new("prov", "model", "acp:test"),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        input: vec![Message::text(MessageId("u1".into()), Role::User, "go")],
        trace: Default::default(),
    }
}

fn sandbox_base(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("awaken-sbx-src-{label}-{}", std::process::id()))
}

/// A tiny ACP agent, launched INSIDE the bwrap sandbox: read the prompt line,
/// emit one message + turn_end.
const SANDBOXED_ACP_SCRIPT: &str = "read _p; \
    printf '%s\\n' '{\"type\":\"message\",\"text\":\"sandboxed reply\"}'; \
    printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";

#[tokio::test]
async fn sandboxed_source_drives_a_real_acp_agent_to_natural_end() {
    if !bwrap_available().await {
        eprintln!("skipping: bwrap/userns unavailable on this host");
        return;
    }
    let launch = AcpLaunch::custom(
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            SANDBOXED_ACP_SCRIPT.to_string(),
        ],
        vec![],
    );
    let source = SandboxChannelSource::new(sandbox_base("e2e"), launch);
    let exec = AcpRunExecutor::new(Arc::new(source));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = exec.execute(activation("t-e2e"), ctx).await.expect("run");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    assert!(
        commit
            .committed()
            .messages
            .iter()
            .any(|m| m.text_content().contains("sandboxed reply")),
        "the OS-confined agent's reply was committed"
    );
}

/// The agent probes a listener on the HOST loopback and reports what it saw. An
/// unrestricted sandbox shares the host network namespace (probe connects); a
/// deny-egress sandbox runs under `--unshare-net` with its own empty namespace,
/// so even the host's 127.0.0.1 is unreachable.
const PROBE_ACP_SCRIPT: &str = "read _p; \
    if (exec 3<>/dev/tcp/127.0.0.1/$PROBE_PORT) 2>/dev/null; then r=NET-UP; else r=NET-DOWN; fi; \
    printf '%s\\n' \"{\\\"type\\\":\\\"message\\\",\\\"text\\\":\\\"$r\\\"}\"; \
    printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";

async fn probe_reply(source: Arc<SandboxChannelSource>, thread: &str) -> String {
    let exec = AcpRunExecutor::new(source);
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let ctx = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = exec.execute(activation(thread), ctx).await.expect("run");
    assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    commit
        .committed()
        .messages
        .iter()
        .map(Message::text_content)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn deny_egress_confines_the_sandboxed_agent_network() {
    if !bwrap_available().await {
        eprintln!("skipping: bwrap/userns unavailable on this host");
        return;
    }
    // A live listener on the host loopback — reachable only from the host netns.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind probe listener");
    let port = listener.local_addr().unwrap().port();

    let launch = AcpLaunch::custom(
        vec![
            "/bin/bash".to_string(),
            "-c".to_string(),
            PROBE_ACP_SCRIPT.to_string(),
        ],
        vec![("PROBE_PORT".to_string(), port.to_string())],
    );
    let egress = ThreadEgress::new();
    egress.set("iso", true); // the session's networking policy denies egress
    let source = Arc::new(
        SandboxChannelSource::new(sandbox_base("egress"), launch).with_thread_egress(egress),
    );

    // Deny-egress thread: the OS gives the agent an empty network namespace.
    let denied = probe_reply(source.clone(), "iso").await;
    assert!(
        denied.contains("NET-DOWN"),
        "deny-egress agent must not reach the host loopback: {denied}"
    );

    // Unregistered thread: shares the host network — proves the policy (not the
    // sandbox itself) is what blocked the probe.
    let open = probe_reply(source, "open").await;
    assert!(
        open.contains("NET-UP"),
        "unrestricted agent must reach the host loopback: {open}"
    );

    drop(listener);
}
