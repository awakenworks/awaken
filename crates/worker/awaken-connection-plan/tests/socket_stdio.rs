#![cfg(unix)]

use awaken_connection_plan::MultiplexedChannel;
use awaken_connection_plan::socket_stdio::{READY, connect, listen};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn socket(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("awaken-socket-{}-{label}.sock", std::process::id()))
}

#[tokio::test]
async fn attached_channel_and_local_clients_preserve_independent_half_closes() {
    // R1 two concurrent Unix clients -> two independent substreams, including
    // identical protocol IDs. R2 one EOF drains only its own response, leaving
    // the listener and other client alive. R3 a later connection works on the
    // same socket; only physical Host closure ends the listener and unlinks it.
    let path = socket("roundtrip");
    let (mut host, physical) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(physical);
    let server_path = path.clone();
    let server =
        tokio::spawn(async move { listen(server_path.to_str().unwrap(), read, write).await });
    let mut ready = vec![0; READY.len()];
    host.read_exact(&mut ready).await.unwrap();
    assert_eq!(ready, READY);
    let mut host = MultiplexedChannel::server(host);
    let mut clients = Vec::new();
    for id in 0..2 {
        let (mut client, pipe) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(pipe);
        let client_path = path.clone();
        let task =
            tokio::spawn(async move { connect(client_path.to_str().unwrap(), read, write).await });
        client.write_all(&[id]).await.unwrap();
        let mut peer = host.accept().await.unwrap().unwrap();
        let mut byte = [0];
        peer.read_exact(&mut byte).await.unwrap();
        assert_eq!(byte, [id], "R1");
        clients.push((client, peer, task));
    }
    for (mut client, mut peer, task) in clients {
        client.shutdown().await.unwrap();
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0, "R2 stream EOF");
        peer.write_all(b"response\0\n").await.unwrap();
        peer.shutdown().await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, b"response\0\n");
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!server.is_finished(), "R2 physical listener remains live");
    }
    let mut later = tokio::net::UnixStream::connect(&path).await.unwrap();
    later.write_all(b"later").await.unwrap();
    let mut peer = host.accept().await.unwrap().unwrap();
    let mut bytes = [0; 5];
    peer.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, *b"later", "R3");
    drop(host);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!path.exists(), "R3");
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
async fn no_client_has_a_bounded_readiness_failure() {
    // R7 ready + live physical Host + no Unix client -> ten-second setup
    // failure and owned socket removal. Physical EOF is the separate R4 rule;
    // arbitrary raw prefixes are invalid multiplex frames (covered by M6).
    let path = socket("no-client");
    let (mut host, physical) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(physical);
    let server_path = path.clone();
    let task =
        tokio::spawn(async move { listen(server_path.to_str().unwrap(), read, write).await });
    let mut ready = vec![0; READY.len()];
    host.read_exact(&mut ready).await.unwrap();
    let _host = MultiplexedChannel::server(host);
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
