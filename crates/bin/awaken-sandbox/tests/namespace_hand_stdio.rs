//! Real bwrap proof that Namespace and Container Hands consume the same sandbox paths.
#![cfg(all(feature = "hand", target_os = "linux"))]

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{Sandbox as _, SandboxProvider as _};
use awaken_runtime_contract::llm::ToolCall;
use awaken_sandbox_local::NamespaceProvider;
use awaken_tool_relay::{RemoteToolExecutor, wire::HandResult};

fn spec() -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: "namespace-hand-path-contract".into(),
        isolation: pc::IsolationClass::Namespace,
        mounts: vec![pc::MountRequirement {
            mount_id: "managed-file".into(),
            mount_path: "/mnt/session/uploads/file-1".into(),
            source: pc::MountSource::InlineBytes {
                contents: b"namespace-hand-token".to_vec(),
                content_hash: None,
            },
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::Session,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        extra: None,
    }
}

fn rendered(result: HandResult) -> String {
    match result {
        HandResult::Ok { output } => serde_json::to_string(&output).expect("serialize output"),
        other => panic!("Hand call failed: {other:?}"),
    }
}

#[tokio::test]
async fn namespace_hand_uses_managed_absolute_paths_for_every_native_tool() {
    /*
     * Path-contract cause/effect graph and decision table.
     * Causes: C1=bwrap is available; C2=a required File is mounted read-only at
     * /mnt/session/uploads/file-1; C3=the real Hand is launched by the Namespace
     * provider; C4=read/glob/bash address the sandbox-absolute path; C5=write
     * attempts to mutate the same mount; C6=the provider adopts the durable
     * handle and reconciles the frozen mount. Effects: E1=all read-capable tools
     * see the exact same token/path; E2=no host projection path leaks; E3=the OS
     * rejects mutation and the original bytes remain authoritative; E4=the
     * adopted Hand sees the same sandbox path.
     * Rules: P1 !C1=>gated skip; P2 C1+C2+C3+C4=>E1+E2;
     * P3 C1+C2+C3+C5=>E3; P4 C1+C2+C6=>E4.
     */
    let base = tempfile::tempdir().expect("sandbox root");
    let provider = NamespaceProvider::new(base.path());
    if provider.probe_ready().await.is_err() {
        eprintln!("skipping: no usable bwrap/user namespace");
        return;
    }
    let sandbox = provider.create_sandbox(&spec()).await.expect("namespace");
    let mut command = pc::Command::new([env!("CARGO_BIN_EXE_awaken-sandbox"), "hand", "--stdio"]);
    command.cwd = "/workspace".into();
    let (process, channel) = sandbox.spawn_agent(command).await.expect("spawn Hand");
    let executor = RemoteToolExecutor::new(channel);

    let read = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "path-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({
                    "path": "/mnt/session/uploads/file-1"
                }),
            })
            .await,
    );
    assert!(read.contains("namespace-hand-token"), "P2/E1: {read}");
    assert!(
        !read.contains(base.path().to_string_lossy().as_ref()),
        "P2/E2"
    );

    let glob = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "path-glob".into(),
                tool_id: "glob".into(),
                arguments: serde_json::json!({
                    "pattern": "/mnt/session/uploads/*"
                }),
            })
            .await,
    );
    assert!(
        glob.contains("/mnt/session/uploads/file-1"),
        "P2/E1: {glob}"
    );

    let bash = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "path-bash".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({
                    "command": "cat /mnt/session/uploads/file-1"
                }),
            })
            .await,
    );
    assert!(bash.contains("namespace-hand-token"), "P2/E1: {bash}");

    let write = executor
        .call_hand(&ToolCall {
            call_id: "path-write".into(),
            tool_id: "write".into(),
            arguments: serde_json::json!({
                "path": "/mnt/session/uploads/file-1",
                "content": "mutated"
            }),
        })
        .await;
    assert!(
        matches!(
            write,
            HandResult::Err { .. }
                | HandResult::Ok {
                    output: awaken_runtime_contract::tool::ToolOutput { is_error: true, .. }
                }
        ),
        "P3/E3: {write:?}"
    );

    let unchanged = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "path-read-after-write".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({
                    "path": "/mnt/session/uploads/file-1"
                }),
            })
            .await,
    );
    assert!(
        unchanged.contains("namespace-hand-token"),
        "P3/E3: {unchanged}"
    );
    assert!(!unchanged.contains("mutated"), "P3/E3: {unchanged}");

    drop(executor);
    let _ = process.signal(pc::Signal::Term).await;
    let _ = process.wait().await;
    let handle = sandbox.handle();
    drop(sandbox);

    let adopted = provider
        .adopt_sandbox(&handle)
        .await
        .expect("adopt namespace");
    adopted
        .attach(spec().mounts.into_iter().next().unwrap())
        .await
        .expect("reconcile frozen mount");
    let mut adopted_command =
        pc::Command::new([env!("CARGO_BIN_EXE_awaken-sandbox"), "hand", "--stdio"]);
    adopted_command.cwd = "/workspace".into();
    let (adopted_process, adopted_channel) = adopted
        .spawn_agent(adopted_command)
        .await
        .expect("spawn adopted Hand");
    let adopted_executor = RemoteToolExecutor::new(adopted_channel);
    let adopted_read = rendered(
        adopted_executor
            .call_hand(&ToolCall {
                call_id: "adopted-path-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({
                    "path": "/mnt/session/uploads/file-1"
                }),
            })
            .await,
    );
    assert!(adopted_read.contains("namespace-hand-token"), "P4/E4");
    drop(adopted_executor);
    let _ = adopted_process.signal(pc::Signal::Term).await;
    let _ = adopted_process.wait().await;
    adopted.dispose().await.expect("dispose namespace");
}
