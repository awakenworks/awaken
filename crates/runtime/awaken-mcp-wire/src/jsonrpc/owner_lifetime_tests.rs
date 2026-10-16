// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Test-only observations of original JSON-RPC I/O ownership and cached notification admission.
// @formal reason: Actual byte-channel EOF is local evidence, not proof of remote process or tool termination.
//! The explicit close control and last-owner path must retire the same original I/O.

use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_peer_close_releases_original_stream_with_cached_notifier() {
    original_peer_scope_ends(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_peer_owner_drop_closes_original_stream_and_cached_notifier() {
    original_peer_scope_ends(false).await;
}

async fn original_peer_scope_ends(explicit_close: bool) {
    // C1 the original peer completes real JSON-RPC I/O; C2 a notifier is
    // cached; C3 explicit close or last peer-owner Drop; E1 cached writes are
    // refused, E2 no post-close bytes arrive, E3 both original stream halves
    // reach actual EOF. No shadow peer, synthetic Drop flag or new lease.
    let (client, server) = tokio::io::duplex(64 * 1024);
    let mut remote = tokio::spawn(async move {
        let (reader, mut writer) = tokio::io::split(server);
        let mut lines = BufReader::new(reader).lines();
        let mut observed = Vec::new();
        while let Some(line) = lines.next_line().await.expect("original stream read") {
            let value: Value = serde_json::from_str(&line).expect("actual JSON RPC frame");
            observed.push(value["method"].as_str().expect("method").to_owned());
            if let Some(id) = value.get("id") {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"original": true}});
                writer
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .expect("actual reply");
            }
        }
        observed
    });
    let (reader, writer) = tokio::io::split(client);
    let (peer, _notifications) = JsonRpcPeer::new(reader, writer, None);
    assert_eq!(
        peer.request("probe", Value::Null, Duration::from_secs(5))
            .await
            .unwrap(),
        json!({"original": true})
    );
    let notifier = peer.notifier();
    let retained_closed_owner = if explicit_close {
        peer.close();
        Some(peer)
    } else {
        drop(peer);
        None
    };
    let refused = matches!(
        notifier.notify("after_owner_drop", Value::Null).await,
        Err(McpTransportError::ConnectionClosed)
    );
    let observed = tokio::time::timeout(Duration::from_secs(3), &mut remote).await;
    let (eof_before_cleanup, methods) = match observed {
        Ok(result) => (true, result.expect("original peer EOF")),
        Err(_) => {
            // Settle the original existing token only after observing the
            // missing EOF. This cleanup is not evidence of last-owner release.
            notifier.closed.cancel();
            let methods = tokio::time::timeout(Duration::from_secs(5), &mut remote)
                .await
                .expect("cleanup closes original stream")
                .expect("join test-owned peer");
            (false, methods)
        }
    };
    drop(retained_closed_owner);
    assert_eq!(methods.first().map(String::as_str), Some("probe"));
    assert!(
        refused,
        "last JsonRpcPeer owner drop kept its cached notifier accepting writes"
    );
    assert!(
        !methods.iter().any(|method| method == "after_owner_drop"),
        "cached notifier wrote to the physical stream after original owner exit"
    );
    assert!(
        eof_before_cleanup,
        "last JsonRpcPeer owner drop retained the original reader/writer byte stream"
    );
}
