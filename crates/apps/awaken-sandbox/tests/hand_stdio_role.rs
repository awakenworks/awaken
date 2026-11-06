//! Real process-level proof for the container Session hand transport.
#![cfg(feature = "hand")]

use std::process::Stdio;

use awaken_tool_contract::tool::ToolCall;
use awaken_tool_relay::{RemoteToolExecutor, wire::HandResult};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;

mod binary_support;

struct ProcessChannel {
    read: tokio::process::ChildStdout,
    write: tokio::process::ChildStdin,
}

impl AsyncRead for ProcessChannel {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProcessChannel {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.write).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn hand_stdio_executes_a_real_tool_over_the_child_process_channel() {
    // H1: real child + writable native ledger root + one tool call => the Hand
    // returns the actual output only after claim and completion directories
    // flush. H2: a failed flush must return an error, never a successful reply;
    // the ledger's persistence-fault test owns that negative branch.
    let ledger =
        std::env::temp_dir().join(format!("awaken-hand-stdio-role-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ledger);
    let mut child = Command::new(binary_support::awaken_sandbox())
        .args(["hand", "--stdio"])
        .env("AWAKEN_HAND_LEDGER_DIR", &ledger)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stdio hand");
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let executor = RemoteToolExecutor::new(ProcessChannel {
        read: stdout,
        write: stdin,
    });

    let result = executor
        .call_hand(&ToolCall {
            call_id: "stdio-1".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({ "command": "printf stdio-hand-ok" }),
        })
        .await
        .expect("request reached the dispatch boundary");

    child.kill().await.expect("terminate hand");
    let _ = child.wait().await;
    let _ = std::fs::remove_dir_all(&ledger);
    match result {
        HandResult::Ok { output } => {
            let rendered = serde_json::to_string(&output).unwrap();
            assert!(rendered.contains("stdio-hand-ok"), "{rendered}");
        }
        other => panic!("stdio hand failed: {other:?}"),
    }
}

#[tokio::test]
async fn explicit_mount_projection_survives_nested_cwd_and_separate_resource_roots() {
    // Cause/effect decision table: P4 physical mount != cwd sibling + nested
    // cwd with spaces/Unicode => logical File/Memory/Repository paths read the
    // exact mounted bytes; P5 writes => only the configured Memory/output tree
    // changes; P6 outside absolute path => denied. A decoy cwd-sibling mount
    // detects the former heuristic. These are real Hand process/protocol/path
    // tests, NOT Memory harvest, Repository publication or credential proofs.
    // P7 explicit workspace + nested cwd => logical /workspace and cwd-relative
    // reads reach one tree; Bash inherits the exact physical cwd. This is the
    // same CLI consumed by TrustedHost's Session-owned Hand, without a mount
    // namespace or host-root symlinks.
    // P4/P5/P7 require Ok(HandResult::Ok); P6 requires Ok(HandResult::Err).
    // P8 a native directory completion fence must succeed before any Ok reply.
    // An outer Err means unavailable before dispatch, not a path denial, and
    // HandResult::Indeterminate cannot establish either successful I/O or
    // confinement. Both must fail every row rather than be flattened to Err.
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let cwd = root.join("workspace/深层 working dir/nested");
    let mount = root.join("explicit 资源/mnt");
    std::fs::create_dir_all(&cwd).unwrap();
    let resources = [
        ("session/uploads/file-1", "file-bytes"),
        ("memory/store-1/note.txt", "memory-bytes"),
        ("session/repositories/repo-1/README.md", "repository-bytes"),
    ];
    for (relative, bytes) in resources {
        let actual = mount.join(relative);
        let decoy = cwd.parent().unwrap().join("mnt").join(relative);
        for path in [&actual, &decoy] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(actual, bytes).unwrap();
        std::fs::write(decoy, "wrong-generation").unwrap();
    }
    std::fs::create_dir_all(mount.join("session/outputs")).unwrap();
    let outside = root.join("outside-secret.txt");
    std::fs::write(&outside, "not-authorized").unwrap();
    std::fs::write(cwd.join("native.txt"), "workspace-bytes").unwrap();
    let mut child = Command::new(binary_support::awaken_sandbox())
        .args(["hand", "--stdio", "--mount-root"])
        .arg(&mount)
        .arg("--workspace-root")
        .arg(root.join("workspace"))
        .current_dir(&cwd)
        .env("AWAKEN_HAND_LEDGER_DIR", root.join("ledger"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let executor = RemoteToolExecutor::new(ProcessChannel {
        read: child.stdout.take().unwrap(),
        write: child.stdin.take().unwrap(),
    });
    for file_path in [
        "native.txt",
        "/workspace/深层 working dir/nested/native.txt",
    ] {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            executor.call_hand(&ToolCall {
                call_id: format!("workspace-{file_path}"),
                tool_id: "read".into(),
                arguments: serde_json::json!({"file_path": file_path}),
            }),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Ok(HandResult::Ok { ref output })
            if !output.is_error && output.text().contains("workspace-bytes")),
            "{result:?}"
        );
    }
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        executor.call_hand(&ToolCall {
            call_id: "physical-bash-cwd".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command": "pwd; cat native.txt"}),
        }),
    )
    .await
    .unwrap();
    assert!(
        matches!(result, Ok(HandResult::Ok { ref output })
        if !output.is_error && output.text().contains("workspace-bytes")
            && output.text().contains("深层 working dir/nested")),
        "{result:?}"
    );
    for (relative, expected) in resources {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            executor.call_hand(&ToolCall {
                call_id: format!("projection-read-{relative}"),
                tool_id: "read".into(),
                arguments: serde_json::json!({"file_path": format!("/mnt/{relative}")}),
            }),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Ok(HandResult::Ok { ref output })
            if !output.is_error && output.text().contains(expected)),
            "{result:?}"
        );
    }
    for relative in ["memory/store-1/note.txt", "session/outputs/result.txt"] {
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), executor.call_hand(&ToolCall {
            call_id: format!("projection-write-{relative}"),
            tool_id: "write".into(),
            arguments: serde_json::json!({"path": format!("/mnt/{relative}"), "content": "updated"}),
        })).await.unwrap();
        assert!(
            matches!(result, Ok(HandResult::Ok { ref output }) if !output.is_error),
            "{result:?}"
        );
        assert_eq!(std::fs::read(mount.join(relative)).unwrap(), b"updated");
    }
    assert_eq!(
        std::fs::read(cwd.parent().unwrap().join("mnt/memory/store-1/note.txt")).unwrap(),
        b"wrong-generation"
    );
    let denied = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        executor.call_hand(&ToolCall {
            call_id: "outside-projection".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"file_path": outside}),
        }),
    )
    .await
    .unwrap();
    assert!(
        matches!(denied, Ok(HandResult::Err { ref error }) if error.message.contains("escapes workdir")),
        "{denied:?}"
    );
    drop(executor);
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
