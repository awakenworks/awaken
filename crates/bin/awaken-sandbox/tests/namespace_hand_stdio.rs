//! Real bwrap proof that Namespace and Container Hands consume the same sandbox paths.
#![cfg(all(feature = "hand", target_os = "linux"))]

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{Sandbox as _, SandboxProvider as _};
use awaken_runtime_contract::llm::ToolCall;
use awaken_sandbox_local::NamespaceProvider;
use awaken_tool_relay::{RemoteToolExecutor, wire::HandResult};

mod binary_support;

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
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

fn rendered(result: HandResult) -> String {
    match result {
        HandResult::Ok { output } => serde_json::to_string(&output).expect("serialize output"),
        other => panic!("Hand call failed: {other:?}"),
    }
}

fn effect_fence(operation_id: &str) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(
        operation_id,
        "namespace-hand-owner",
        "namespace-hand-runtime",
        1,
        u64::MAX,
    )
    .expect("typed Namespace Hand effect fence")
}

fn disposal_authorization(prepared: &pc::SandboxEffectFence) -> pc::SandboxDisposalAuthorization {
    let preparation = pc::SandboxDisposalPreparation::new(
        prepared.clone(),
        format!("namespace-hand-preparation:{}", prepared.operation_id),
    )
    .expect("typed Namespace Hand disposal preparation");
    let successor = pc::SandboxEffectFence::new(
        preparation.operation_id().expect("preparation operation"),
        prepared.owner.clone(),
        prepared.runtime_incarnation.clone(),
        prepared.epoch,
        prepared.expires_at_unix_ms,
    )
    .expect("typed Namespace Hand disposal successor");
    preparation
        .authorize(successor)
        .expect("typed Namespace Hand disposal authorization")
}

#[tokio::test]
async fn namespace_hand_uses_managed_absolute_paths_for_every_native_tool() {
    /*
     * Path-contract cause/effect graph and decision table.
     * Causes: C1=bwrap is available; C2=a required File is mounted read-only at
     * /mnt/session/uploads/file-1; C3=the real Hand is launched by the Namespace
     * provider; C4=read/glob/bash/edit/git address sandbox-absolute paths; C5=write
     * attempts to mutate the same mount; C6=the provider adopts the durable
     * handle and reconciles the frozen mount. Effects: E1=all read-capable tools
     * see the exact same token/path; E2=no host projection path leaks; E3=the OS
     * rejects mutation and the original bytes remain authoritative; E4=the
     * adopted Hand sees the same sandbox path; E5=every cooperating tool and
     * the opaque Git subprocess observe `/workspace`, never the provider's
     * physical realization directory.
     * Rules: P1 !C1=>gated skip; P2 C1+C2+C3+C4=>E1+E2;
     * P3 C1+C2+C3+C5=>E3; P4 C1+C2+C6=>E4;
     * P5 C1+C3+C4 on writable workspace=>E1+E2+E5.
     */
    let base = tempfile::tempdir().expect("sandbox root");
    let provider = NamespaceProvider::new(base.path());
    if provider.probe_ready().await.is_err() {
        eprintln!("skipping: no usable bwrap/user namespace");
        return;
    }
    let sandbox_spec = spec();
    let realization = effect_fence("namespace-hand-realization");
    let sandbox = provider
        .create_sandbox_for_effect(&sandbox_spec, &realization, None)
        .await
        .expect("fenced namespace");
    let binary = binary_support::awaken_sandbox();
    let mut command = pc::Command::new([binary.as_str(), "hand", "--stdio"]);
    command.cwd = "/workspace".into();
    let (process, channel) = sandbox.spawn_agent(command).await.expect("spawn Hand");
    let executor = RemoteToolExecutor::new(channel);

    let initialize_repository = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "workspace-bash-init".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({
                    "command": "mkdir -p /workspace/repository && printf 'before\\n' > /workspace/repository/note.txt && git -C /workspace/repository init -q"
                }),
            })
            .await.expect("request reached the dispatch boundary"),
    );
    assert!(
        !initialize_repository.contains(base.path().to_string_lossy().as_ref()),
        "P5/E2: {initialize_repository}"
    );

    let workspace_read = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "workspace-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({
                    "path": "/workspace/repository/note.txt"
                }),
            })
            .await
            .expect("request reached the dispatch boundary"),
    );
    assert!(workspace_read.contains("before"), "P5/E1: {workspace_read}");
    assert!(
        !workspace_read.contains(base.path().to_string_lossy().as_ref()),
        "P5/E2: {workspace_read}"
    );

    let workspace_edit = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "workspace-edit".into(),
                tool_id: "edit".into(),
                arguments: serde_json::json!({
                    "path": "/workspace/repository/note.txt",
                    "old": "before",
                    "new": "after"
                }),
            })
            .await
            .expect("request reached the dispatch boundary"),
    );
    assert!(
        workspace_edit.contains("/workspace/repository/note.txt"),
        "P5/E1+E5: {workspace_edit}"
    );
    assert!(
        !workspace_edit.contains(base.path().to_string_lossy().as_ref()),
        "P5/E2: {workspace_edit}"
    );

    let git_status = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "workspace-git".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({
                    "command": "git -C /workspace/repository status --short && git -C /workspace/repository rev-parse --show-toplevel"
                }),
            })
            .await.expect("request reached the dispatch boundary"),
    );
    assert!(git_status.contains("?? note.txt"), "P5/E1: {git_status}");
    assert!(
        git_status.contains("/workspace/repository"),
        "P5/E5: {git_status}"
    );
    assert!(
        !git_status.contains(base.path().to_string_lossy().as_ref()),
        "P5/E2: {git_status}"
    );

    let read = rendered(
        executor
            .call_hand(&ToolCall {
                call_id: "path-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({
                    "path": "/mnt/session/uploads/file-1"
                }),
            })
            .await
            .expect("request reached the dispatch boundary"),
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
            .await
            .expect("request reached the dispatch boundary"),
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
            .await
            .expect("request reached the dispatch boundary"),
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
        .await
        .expect("request reached the dispatch boundary");
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
            .await
            .expect("request reached the dispatch boundary"),
    );
    assert!(
        unchanged.contains("namespace-hand-token"),
        "P3/E3: {unchanged}"
    );
    assert!(!unchanged.contains("mutated"), "P3/E3: {unchanged}");

    // B1 real Namespace + a successful command leaves `set -e` active;
    // B2 a later failure -> exact exit 1 and original diagnostic, no replay.
    // B3 the next command gets a clean shell but the same workspace; B4 a
    // replacement Hand below must also retain that workspace file. Temporary
    // process mounts are not used as a cross-call continuation authority.
    rendered(executor.call_hand(&ToolCall {
        call_id: "bash-errexit-setup".into(), tool_id: "bash".into(),
        arguments: serde_json::json!({"command": "printf retained > /workspace/continuation.txt; export PREVIOUS=yes; set -e"}),
    }).await.unwrap());
    let failed = executor.call_hand(&ToolCall {
        call_id: "bash-errexit-failure".into(), tool_id: "bash".into(),
        arguments: serde_json::json!({"command": "printf original-diagnostic >&2; false; touch /workspace/replayed"}),
    }).await.unwrap();
    assert!(
        matches!(failed, HandResult::Err { ref error }
        if error.message.contains("exit 1") && error.message.contains("original-diagnostic")),
        "B2 {failed:?}"
    );
    let resumed = rendered(executor.call_hand(&ToolCall {
        call_id: "bash-after-exit".into(), tool_id: "bash".into(),
        arguments: serde_json::json!({"command": "test ! -e /workspace/replayed && test -z \"$PREVIOUS\" && cat /workspace/continuation.txt"}),
    }).await.unwrap());
    assert!(resumed.contains("retained"), "B3 {resumed}");

    drop(executor);
    let _ = process.signal(pc::Signal::Term).await;
    let _ = process.wait().await;
    let handle = sandbox.handle();
    drop(sandbox);

    let adopted = provider
        .adopt_sandbox_for_effect(&sandbox_spec, &handle, &realization)
        .await
        .expect("adopt fenced namespace");
    adopted
        .attach(spec().mounts.into_iter().next().unwrap())
        .await
        .expect("reconcile frozen mount");
    let binary = binary_support::awaken_sandbox();
    let mut adopted_command = pc::Command::new([binary.as_str(), "hand", "--stdio"]);
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
            .await
            .expect("request reached the dispatch boundary"),
    );
    assert!(adopted_read.contains("namespace-hand-token"), "P4/E4");
    let retained = rendered(
        adopted_executor
            .call_hand(&ToolCall {
                call_id: "adopted-workspace-continuation".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"file_path": "/workspace/continuation.txt"}),
            })
            .await
            .unwrap(),
    );
    assert!(retained.contains("retained"), "B4 {retained}");
    drop(adopted_executor);
    let _ = adopted_process.signal(pc::Signal::Term).await;
    let _ = adopted_process.wait().await;
    let terminal = effect_fence("namespace-hand-terminal");
    let prepared = adopted
        .prepare_disposal_for_effect(&terminal)
        .await
        .expect("prepare fenced namespace disposal");
    adopted
        .dispose_for_effect(&disposal_authorization(&prepared))
        .await
        .expect("dispose fenced namespace");
}
