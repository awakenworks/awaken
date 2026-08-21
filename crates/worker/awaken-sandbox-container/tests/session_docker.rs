//! Production-image proof that one live Docker environment serves Native, ACP-like
//! stdio, and the remote hand without recreating the container.
#![cfg(feature = "docker")]

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::ToolExecutor;
use awaken_sandbox_container::docker::DockerRuntime;
use awaken_sandbox_container::{
    ContainerEnvironment, ContainerProvider, ContainerRuntime, ContainerState, EnvironmentFile,
};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn native_acp_and_hand_share_one_production_container() {
    // Cause/effect coverage: a configured production image and reachable Docker
    // daemon create one shared container; mounted files and Native/ACP/hand
    // commands observe the same state, structured hand content preserves its
    // text result, and output harvesting remains exact.
    let Ok(image) = std::env::var("AWAKEN_TEST_SESSION_IMAGE") else {
        eprintln!("skipping: AWAKEN_TEST_SESSION_IMAGE is not set");
        return;
    };
    let runtime = Arc::new(DockerRuntime::connect_local(8080).expect("docker client"));
    runtime.ping().await.expect("reachable Docker daemon");
    let provider = ContainerProvider::new(runtime.clone(), image);
    let scope = format!("session-real-{}", std::process::id());
    let spec = pc::SandboxSpec {
        scope,
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "seed".into(),
            source: pc::MountSource::Inline {
                contents: "mounted-seed".into(),
            },
            mount_path: "/workspace/.mnt/seed.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    };
    let sandbox = provider
        .create_container(&spec)
        .await
        .expect("create Session environment despite the image's legacy entrypoint");
    let handle = pc::Sandbox::handle(&sandbox);
    let container_id = handle
        .container_payload()
        .expect("typed container handle")
        .container_id
        .clone();
    assert_eq!(
        ContainerEnvironment::read_files(&sandbox, "/workspace/.mnt")
            .await
            .unwrap(),
        vec![EnvironmentFile {
            path: "seed.txt".into(),
            bytes: b"mounted-seed".to_vec(),
        }]
    );

    let mut write = pc::Command::new([
        "sh",
        "-c",
        "printf real-shared-state > /workspace/session-marker",
    ]);
    write.cwd = "/workspace".into();
    assert_eq!(
        pc::Sandbox::spawn(&sandbox, write)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap()
            .code,
        Some(0)
    );

    let mut read = pc::Command::new(["sh", "-c", "cat /workspace/session-marker"]);
    read.cwd = "/workspace".into();
    let mut acp = sandbox.spawn_agent(read).await.unwrap();
    let mut output = String::new();
    acp.channel.read_to_string(&mut output).await.unwrap();
    assert_eq!(acp.process.wait().await.unwrap().code, Some(0));
    assert_eq!(output, "real-shared-state");

    let hand = sandbox
        .spawn_agent(pc::Command {
            argv: vec![
                "/usr/local/bin/awaken-sandbox".into(),
                "hand".into(),
                "--stdio".into(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .unwrap();
    let hand_process: Arc<dyn pc::ProcessHandle> = Arc::from(hand.process);
    let executor = awaken_tool_relay::RemoteToolExecutor::new(hand.channel)
        .with_operation_scope(pc::Sandbox::id(&sandbox));
    let result = executor
        .invoke(&ToolCall {
            call_id: "real-bound-hand".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({
                "command": "test \"$(cat /workspace/session-marker)\" = real-shared-state && printf real-hand-ok"
            }),
        })
        .await
        .expect("Native tool routed through the in-container hand");
    assert!(awaken_runtime_contract::extract_text(&result.content).contains("real-hand-ok"));

    let harvest = pc::Command::new([
        "sh",
        "-c",
        "mkdir -p /workspace/outputs/nested && printf text > /workspace/outputs/z.txt && printf '\\000\\377' > /workspace/outputs/nested/binary",
    ]);
    assert_eq!(
        pc::Sandbox::spawn(&sandbox, harvest)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap()
            .code,
        Some(0)
    );
    assert_eq!(
        ContainerEnvironment::read_files(&sandbox, "/workspace/outputs")
            .await
            .unwrap(),
        vec![
            EnvironmentFile {
                path: "nested/binary".into(),
                bytes: vec![0, 0xff],
            },
            EnvironmentFile {
                path: "z.txt".into(),
                bytes: b"text".to_vec(),
            },
        ]
    );

    hand_process.signal(pc::Signal::Term).await.unwrap();
    assert!(
        hand_process.wait().await.unwrap().signaled || hand_process.poll().await.unwrap().is_some()
    );
    pc::Sandbox::dispose(&sandbox).await.unwrap();
    assert!(matches!(
        runtime.inspect(&container_id).await,
        Err(_) | Ok(ContainerState::Gone)
    ));
}
