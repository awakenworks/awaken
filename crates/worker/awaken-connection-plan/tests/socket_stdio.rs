#![cfg(unix)]

use awaken_connection_plan::socket_stdio::{READY, connect, listen};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn socket(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("awaken-socket-{}-{label}.sock", std::process::id()))
}

#[tokio::test]
async fn attached_channel_and_local_client_preserve_bytes_and_half_closes() {
    // Graph: fresh bound Unix socket -> ready -> local client connects -> the
    // same attached channel carries opaque bytes. R1 request half-close retains
    // the reverse response; R2 response half-close drains bytes then both bridge
    // tasks finish; R3 only the owned rendezvous is removed at completion.
    let path = socket("roundtrip");
    let (mut host, server) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(server);
    let server_path = path.clone();
    let server =
        tokio::spawn(async move { listen(server_path.to_str().unwrap(), read, write).await });
    let mut ready = vec![0; READY.len()];
    host.read_exact(&mut ready).await.unwrap();
    assert_eq!(ready, READY);
    let (mut client, pipe) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(pipe);
    let client_path = path.clone();
    let client_bridge =
        tokio::spawn(async move { connect(client_path.to_str().unwrap(), read, write).await });
    client.write_all(b"request\0\n").await.unwrap();
    client.shutdown().await.unwrap();
    let mut request = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), host.read_to_end(&mut request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request, b"request\0\n");
    host.write_all(b"response\0\n").await.unwrap();
    host.shutdown().await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, b"response\0\n");
    for task in [server, client_bridge] {
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert!(!path.exists());
}

#[tokio::test]
async fn host_disconnection_before_client_reclaims_the_rendezvous() {
    // R4 ready + no client + attached channel EOF -> no indefinite accept;
    // terminal success and physical socket removal, without another client.
    let path = socket("disconnect");
    let (mut host, server) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(server);
    let server_path = path.clone();
    let task =
        tokio::spawn(async move { listen(server_path.to_str().unwrap(), read, write).await });
    let mut ready = vec![0; READY.len()];
    host.read_exact(&mut ready).await.unwrap();
    drop(host);
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn occupied_paths_and_missing_peers_fail_without_replacing_bytes() {
    // R5 existing regular entry -> bind fails and original bytes remain;
    // R6 absent peer -> connect fails; neither case reports ready or success.
    let path = socket("occupied");
    std::fs::write(&path, b"original").unwrap();
    let (input, output) = tokio::io::split(tokio::io::duplex(64).0);
    assert!(listen(path.to_str().unwrap(), input, output).await.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    std::fs::remove_file(&path).unwrap();
    let (input, output) = tokio::io::split(tokio::io::duplex(64).0);
    assert!(
        connect(path.to_str().unwrap(), input, output)
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn a_prefix_without_a_client_has_a_bounded_readiness_failure() {
    // R7 ready + host sends bytes + no Unix client (with or without host EOF)
    // -> the same ten-second readiness deadline fails and removes the socket.
    // No unbounded second accept may be hidden behind the buffered prefix.
    for eof in [false, true] {
        let path = socket(if eof { "prefix-eof" } else { "prefix-live" });
        let (mut host, server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(server);
        let server_path = path.clone();
        let task =
            tokio::spawn(async move { listen(server_path.to_str().unwrap(), read, write).await });
        let mut ready = vec![0; READY.len()];
        host.read_exact(&mut ready).await.unwrap();
        host.write_all(b"prefix").await.unwrap();
        if eof {
            host.shutdown().await.unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(
            task.await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("readiness timed out")
        );
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn cancellation_removes_only_the_owned_socket_entry() {
    // R8 cancellation before a client -> owned entry removed. R9 another owner
    // replaces the path before cancellation -> replacement bytes survive. A
    // path string alone never authorizes removing somebody else's new entry.
    for replaced in [false, true] {
        let path = socket(if replaced { "replaced" } else { "cancelled" });
        let (mut host, server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(server);
        let server_path = path.clone();
        let task =
            tokio::spawn(async move { listen(server_path.to_str().unwrap(), read, write).await });
        let mut ready = vec![0; READY.len()];
        host.read_exact(&mut ready).await.unwrap();
        if replaced {
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, b"replacement").unwrap();
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        if replaced {
            assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
            std::fs::remove_file(&path).unwrap();
        } else {
            assert!(!path.exists());
        }
    }
}
