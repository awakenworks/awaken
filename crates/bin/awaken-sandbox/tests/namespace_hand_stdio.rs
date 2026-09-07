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
        process_tree_supervision: false,
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
#[ignore = "provider-owner participant invoked by the abrupt-death test"]
async fn namespace_owner_crash_participant() {
    use tokio::io::AsyncReadExt;
    let root = std::env::var_os("AWAKEN_NAMESPACE_CRASH_ROOT").expect("fixture root");
    let port: u16 = std::env::var("AWAKEN_NAMESPACE_CRASH_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let provider = NamespaceProvider::new(std::path::PathBuf::from(root));
    provider
        .probe_ready()
        .await
        .expect("usable native Namespace is required");
    let sandbox = provider
        .create_sandbox_for_effect(&spec(), &effect_fence("namespace-crash"), None)
        .await
        .expect("create exact provider-owned Namespace");
    // $0 carries this literal script to the next node without shell-generated
    // quoting. Both descendants start a new session/process group. Their open
    // sockets acknowledge that all three nodes are running inside the sandbox.
    let script = r#"
set -eu
test "$(cat /mnt/session/uploads/file-1)" = namespace-hand-token
case "$1" in
  leader) setsid /bin/bash -c "$0" "$0" child & ;;
  child) setsid /bin/bash -c "$0" "$0" grandchild & ;;
esac
exec 3<>/dev/tcp/127.0.0.1/$AWAKEN_NAMESPACE_CRASH_PORT
read -r pid comm state ppid pgrp session rest < /proc/$$/stat
printf '%s %s %s %s\n' "$1" "$pid" "$pgrp" "$session" >&3
# A failing parent test closes its sockets; the timeout is an additional
# fixture backstop, not the success oracle (which must finish within 5 s).
read -r -t 30 -u 3 stop || true
"#;
    let mut command = pc::Command::new(["/bin/bash", "-c", script, script, "leader"]);
    command.cwd = "/workspace".into();
    command.stdio = pc::Stdio::Null;
    command.env.push(pc::EnvVar {
        name: "AWAKEN_NAMESPACE_CRASH_PORT".into(),
        value: pc::EnvValue::Inline {
            value: port.to_string(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let _process = sandbox.spawn(command).await.expect("launch native tree");
    // The parent keeps stdin open until SIGKILL. EOF is only failure cleanup;
    // the asserted path cannot use LocalProcess Drop or Sandbox::dispose.
    let mut byte = [0_u8; 1];
    let _ = tokio::io::stdin().read(&mut byte).await;
}

#[tokio::test]
async fn namespace_owner_sigkill_terminates_descendants_in_separate_process_groups() {
    /*
     * Crash cause/effect table N1 (Linux native substrate, no successful skip):
     * C1 production Namespace provider has usable bwrap/user/PID namespaces;
     * C2 leader -> child -> grandchild each verifies the mounted File;
     * C3 child and grandchild call setsid; C4 all nodes acknowledge live sockets;
     * C5 parent kills the separate provider-owner process with SIGKILL, while
     * keeping every control socket open. C1+C2+C3+C4+C5 => all sockets close
     * within 5 s, before the 30 s fixture backstop. There is no graceful dispose,
     * protocol cancellation, process-group-only kill or PID-scan oracle.
     * !C1 => fail, not skip. Missing node/early exit/retained connection => fail.
     * N2 removing only --unshare-pid must hit the exact 5 s termination timeout,
     * not a startup/parse error; restoring the flag restores N1's passing result.
     * Scope: real Namespace owner death and this detached three-node tree; not
     * an Awaken cloud failover test or a TrustedHost/macOS/Windows guarantee.
     */
    use std::collections::BTreeSet;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut owner = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "namespace_owner_crash_participant",
            "--ignored",
            "--nocapture",
        ])
        .env("AWAKEN_NAMESPACE_CRASH_ROOT", root.path())
        .env(
            "AWAKEN_NAMESPACE_CRASH_PORT",
            listener.local_addr().unwrap().port().to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut nodes = Vec::new();
    let mut roles = BTreeSet::new();
    let mut groups = BTreeSet::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut role = String::new();
            assert_ne!(stream.read_line(&mut role).await.unwrap(), 0);
            let fields: Vec<_> = role.split_whitespace().collect();
            assert_eq!(fields.len(), 4, "role, PID, process group, session");
            assert!(roles.insert(fields[0].to_string()), "unique tree node");
            let pid: u32 = fields[1].parse().unwrap();
            let group: u32 = fields[2].parse().unwrap();
            let session: u32 = fields[3].parse().unwrap();
            if fields[0] != "leader" {
                assert_eq!(pid, group, "setsid made a new process group");
                assert_eq!(pid, session, "setsid made a new session");
            }
            assert!(
                groups.insert(group),
                "each node has a distinct process group"
            );
            nodes.push(stream);
        }
    })
    .await
    .expect("three live Namespace tree nodes");
    assert_eq!(
        roles,
        BTreeSet::from(["leader".into(), "child".into(), "grandchild".into()])
    );
    assert!(
        owner.try_wait().unwrap().is_none(),
        "owner still alive before fault"
    );
    for node in &nodes {
        assert_eq!(
            node.get_ref().try_read(&mut [0_u8; 1]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "node is still connected before SIGKILL"
        );
    }
    owner
        .kill()
        .await
        .expect("SIGKILL provider owner, not graceful disposal");
    assert_eq!(owner.wait().await.unwrap().signal(), Some(9));
    tokio::time::timeout(Duration::from_secs(5), async {
        for mut node in nodes {
            let mut byte = [0_u8; 1];
            match node.read(&mut byte).await {
                Ok(0) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                result => panic!("node must close after owner SIGKILL: {result:?}"),
            }
        }
    })
    .await
    .expect("PID namespace closes detached descendants after owner death");
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
     * Rules: P1 !C1=>qualification failure, never a passing skip;
     * P2 C1+C2+C3+C4=>E1+E2;
     * P3 C1+C2+C3+C5=>E3; P4 C1+C2+C6=>E4;
     * P5 C1+C3+C4 on writable workspace=>E1+E2+E5.
     */
    let base = tempfile::tempdir().expect("sandbox root");
    let provider = NamespaceProvider::new(base.path());
    provider
        .probe_ready()
        .await
        .expect("P1 real Native path qualification requires a usable namespace");
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
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("prepare fenced namespace disposal");
    adopted
        .dispose_for_effect(&disposal_authorization(&prepared))
        .await
        .expect("dispose fenced namespace");
}
