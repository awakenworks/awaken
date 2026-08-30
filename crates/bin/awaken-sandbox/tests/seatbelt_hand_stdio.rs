//! macOS proof that the real Hand projects Managed mount paths through Seatbelt.
#![cfg(all(feature = "hand", target_os = "macos"))]

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{Sandbox as _, SandboxProvider as _};
use awaken_runtime_contract::llm::ToolCall;
use awaken_sandbox_local::NamespaceProvider;
use awaken_tool_relay::{RemoteToolExecutor, wire::HandResult};

#[tokio::test]
async fn seatbelt_hand_reads_the_session_mount_when_the_host_mnt_exists() {
    // C1 Seatbelt has no mount namespace; C2 macOS may expose a host /mnt;
    // C3 the provider materializes the Managed File below the Session root.
    // C1+C2+C3 must make the logical Managed path read the Session bytes.
    let base = tempfile::tempdir().expect("sandbox root");
    let provider = NamespaceProvider::new(base.path());
    if provider.probe_ready().await.is_err() {
        eprintln!("skipping: Seatbelt/sandbox-exec unavailable");
        return;
    }
    let spec = pc::SandboxSpec {
        scope: "seatbelt-hand-path-contract".into(),
        isolation: pc::IsolationClass::Namespace,
        mounts: vec![pc::MountRequirement {
            mount_id: "managed-file".into(),
            mount_path: "/mnt/session/uploads/file-1".into(),
            source: pc::MountSource::InlineBytes {
                contents: b"seatbelt-hand-token".to_vec(),
                content_hash: None,
            },
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::Session,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::None,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        control_services: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    };
    let sandbox = provider.create_sandbox(&spec).await.expect("namespace");
    let mut command = pc::Command::new([env!("CARGO_BIN_EXE_awaken-sandbox"), "hand", "--stdio"]);
    command.cwd = "/workspace".into();
    let (process, channel) = sandbox.spawn_agent(command).await.expect("spawn Hand");
    let executor = RemoteToolExecutor::new(channel);
    let result = executor
        .call_hand(&ToolCall {
            call_id: "seatbelt-managed-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({
                "file_path": "/mnt/session/uploads/file-1"
            }),
        })
        .await;
    let output = match result {
        HandResult::Ok { output } => serde_json::to_string(&output).expect("serialize output"),
        other => panic!("Hand call failed: {other:?}"),
    };
    assert!(output.contains("seatbelt-hand-token"), "{output}");

    let write = executor
        .call_hand(&ToolCall {
            call_id: "seatbelt-managed-write".into(),
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
        "read-only mount accepted a write: {write:?}"
    );
    let unchanged = executor
        .call_hand(&ToolCall {
            call_id: "seatbelt-managed-reread".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({
                "file_path": "/mnt/session/uploads/file-1"
            }),
        })
        .await;
    let unchanged = match unchanged {
        HandResult::Ok { output } => serde_json::to_string(&output).expect("serialize output"),
        other => panic!("Hand reread failed: {other:?}"),
    };
    assert!(unchanged.contains("seatbelt-hand-token"), "{unchanged}");
    assert!(!unchanged.contains("mutated"), "{unchanged}");

    let outside = executor
        .call_hand(&ToolCall {
            call_id: "seatbelt-outside-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({ "file_path": "/etc/passwd" }),
        })
        .await;
    assert!(
        matches!(outside, HandResult::Err { ref error } if error.message.contains("escapes workdir")),
        "outside path was not rejected: {outside:?}"
    );

    drop(executor);
    let _ = process.signal(pc::Signal::Term).await;
    let _ = process.wait().await;
    sandbox.dispose().await.expect("dispose namespace");
}
