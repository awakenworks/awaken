#![cfg(all(unix, feature = "hand"))]

use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn command(role: &str, socket: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_awaken-sandbox"));
    command
        .args([role, "--unix"])
        .arg(socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn real_process_half_close_drains_the_reverse_response_and_exits() {
    // Causes: C1 two real role processes with pipes (not duplex substitutes);
    // C2 client stdin EOF; C3 host responds only after observing request EOF.
    // Decision R1 C1+C2 -> exact request + physical host stdout EOF;
    // R2 C3 + host stdin EOF -> exact response + both processes exit successfully.
    // Constraint: a shutdown() that leaves fd 1 open must fail R1; timeout is a
    // failure, with kill-on-drop limited to these two owned test children.
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("mcp.sock");
    let mut host = command("channel-listen", &socket).spawn().unwrap();
    let mut output = host.stdout.take().unwrap();
    let mut ready = vec![0; awaken_connection_plan::socket_stdio::READY.len()];
    tokio::time::timeout(Duration::from_secs(3), output.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready, awaken_connection_plan::socket_stdio::READY);
    let mut client = command("channel-connect", &socket).spawn().unwrap();
    let mut input = client.stdin.take().unwrap();
    input.write_all(b"request\0\n").await.unwrap();
    drop(input);
    let mut request = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), output.read_to_end(&mut request))
        .await
        .expect("R1 physical stdout EOF")
        .unwrap();
    assert_eq!(request, b"request\0\n");
    let mut input = host.stdin.take().unwrap();
    input.write_all(b"response\0\n").await.unwrap();
    drop(input);
    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(3),
        client.stdout.take().unwrap().read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response, b"response\0\n");
    for mut child in [host, client] {
        assert!(
            tokio::time::timeout(Duration::from_secs(3), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
    assert!(!socket.exists());
}

#[tokio::test]
async fn real_listener_exits_on_host_eof_or_term_without_a_client() {
    // R3 ready + host EOF -> exit and unlink without a client. R4 ready + live
    // host stdin + TERM -> cancel the async read/accept, unlink and exit; an
    // uncancellable blocking stdin task must not hold runtime shutdown open.
    for terminate in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("mcp.sock");
        let mut host = command("channel-listen", &socket).spawn().unwrap();
        let mut ready = vec![0; awaken_connection_plan::socket_stdio::READY.len()];
        tokio::time::timeout(
            Duration::from_secs(3),
            host.stdout.as_mut().unwrap().read_exact(&mut ready),
        )
        .await
        .unwrap()
        .unwrap();
        if terminate {
            let status = tokio::process::Command::new("kill")
                .args(["-TERM", &host.id().unwrap().to_string()])
                .status()
                .await
                .unwrap();
            assert!(status.success());
        } else {
            drop(host.stdin.take());
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(3), host.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert!(!socket.exists());
    }
}

#[tokio::test]
async fn socket_pair_stdio_preserves_the_same_half_close_contract() {
    // R5 Node/libuv-shaped child stdio (Unix socket pairs, not FIFO pipes) ->
    // identical readiness, exact bytes, half-close/reply, process exit and unlink.
    // This row prevents container exec pipe tests from falsely certifying the
    // connector process that the Claude SDK launches inside the container.
    use std::os::fd::OwnedFd;
    let pair = || {
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        parent.set_nonblocking(true).unwrap();
        (
            tokio::net::UnixStream::from_std(parent).unwrap(),
            Stdio::from(OwnedFd::from(child)),
        )
    };
    let (mut input, child_input) = pair();
    let (mut output, child_output) = pair();
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("mcp.sock");
    let mut child = command("channel-listen", &socket)
        .stdin(child_input)
        .stdout(child_output)
        .spawn()
        .unwrap();
    let mut ready = vec![0; awaken_connection_plan::socket_stdio::READY.len()];
    tokio::time::timeout(Duration::from_secs(3), output.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    let mut client = tokio::net::UnixStream::connect(&socket).await.unwrap();
    client.write_all(b"request").await.unwrap();
    client.shutdown().await.unwrap();
    let mut request = vec![];
    tokio::time::timeout(Duration::from_secs(3), output.read_to_end(&mut request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request, b"request");
    input.write_all(b"response").await.unwrap();
    input.shutdown().await.unwrap();
    let mut response = vec![];
    tokio::time::timeout(Duration::from_secs(3), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, b"response");
    assert!(
        tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!socket.exists());
}
