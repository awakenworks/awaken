//! Real process-level proof for the container Session hand transport.
#![cfg(feature = "hand")]

use std::process::Stdio;

use awaken_runtime_contract::llm::ToolCall;
use awaken_tool_relay::{RemoteToolExecutor, wire::HandResult};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;

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
    let mut child = Command::new(env!("CARGO_BIN_EXE_awaken-sandbox"))
        .args(["hand", "--stdio"])
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
        .await;

    child.kill().await.expect("terminate hand");
    match result {
        HandResult::Ok { output } => {
            let rendered = serde_json::to_string(&output).unwrap();
            assert!(rendered.contains("stdio-hand-ok"), "{rendered}");
        }
        other => panic!("stdio hand failed: {other:?}"),
    }
}
