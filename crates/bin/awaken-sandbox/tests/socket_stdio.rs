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

fn spawn_channel(
    role: &str,
    socket: &std::path::Path,
    socket_stdio: bool,
) -> (
    tokio::process::Child,
    Box<dyn tokio::io::AsyncWrite + Unpin>,
    Box<dyn tokio::io::AsyncRead + Unpin>,
) {
    if socket_stdio {
        use std::os::fd::OwnedFd;
        let pair = || {
            let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
            parent.set_nonblocking(true).unwrap();
            (
                tokio::net::UnixStream::from_std(parent).unwrap(),
                Stdio::from(OwnedFd::from(child)),
            )
        };
        let (input, child_input) = pair();
        let (output, child_output) = pair();
        let child = command(role, socket)
            .stdin(child_input)
            .stdout(child_output)
            .spawn()
            .unwrap();
        (child, Box::new(input), Box::new(output))
    } else {
        let mut child = command(role, socket).spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        (child, Box::new(input), Box::new(output))
    }
}

#[tokio::test]
async fn real_process_half_close_drains_the_reverse_response_and_exits() {
    // Causes: C1 listener stdio pipe/socket; C2 connector stdio pipe/socket;
    // C3 client stdin EOF; C4 host replies only after request EOF.
    // Decision table: R1 pipe/pipe, R2 pipe/socket (container + SDK),
    // R3 socket/pipe, R4 socket/socket. Every C1/C2 pair with C3 must deliver
    // exact request + physical host stdout EOF; C4 + host stdin EOF delivers
    // exact response + both successful process exits + owned socket removal.
    // A socket-shaped listener alone cannot certify the SDK connector role.
    // This single matrix owns the former duplicate pipe/socket roundtrips;
    // timeout is failure and kill-on-drop covers only these owned children.
    for (rule, host_socket, client_socket) in [
        ("R1", false, false),
        ("R2", false, true),
        ("R3", true, false),
        ("R4", true, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("mcp.sock");
        let (host, mut host_input, mut host_output) =
            spawn_channel("channel-listen", &socket, host_socket);
        let mut ready = vec![0; awaken_connection_plan::socket_stdio::READY.len()];
        tokio::time::timeout(Duration::from_secs(3), host_output.read_exact(&mut ready))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready, awaken_connection_plan::socket_stdio::READY, "{rule}");
        let (client, mut client_input, mut client_output) =
            spawn_channel("channel-connect", &socket, client_socket);
        client_input.write_all(b"request\0\n").await.unwrap();
        client_input.shutdown().await.unwrap();
        drop(client_input);
        let mut request = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(3),
            host_output.read_to_end(&mut request),
        )
        .await
        .expect("physical stdout EOF")
        .unwrap();
        assert_eq!(request, b"request\0\n", "{rule}");
        host_input.write_all(b"response\0\n").await.unwrap();
        host_input.shutdown().await.unwrap();
        drop(host_input);
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(3),
            client_output.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response, b"response\0\n", "{rule}");
        for mut child in [host, client] {
            assert!(
                tokio::time::timeout(Duration::from_secs(3), child.wait())
                    .await
                    .unwrap()
                    .unwrap()
                    .success(),
                "{rule}"
            );
        }
        assert!(!socket.exists(), "{rule}");
    }
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
