//! A minimal JSON-RPC 2.0 peer over an async byte stream.
//!
//! The MCP SDK's client transport only resolves numeric-id responses and drops
//! server notifications and server->client requests — so `progress`,
//! `tools/list_changed`, `resources/updated`, and `sampling` cannot flow through
//! it. This peer demuxes all three:
//!
//! - **response** (`id`, no `method`) resolves the matching pending request;
//! - **notification** (`method`, no `id`) is forwarded to a channel;
//! - **peer request** (`id` + `method`) is dispatched to a handler and its
//!   reply written back.
//!
//! The peer is symmetric, so it serves both directions: the MCP *client*
//! (`awaken-ext-mcp`) drives a spawned server through it, and the MCP *server*
//! (`awaken-protocol-mcp`) serves a connected client over stdio with the same
//! demux — there, the "server" in [`ServerNotification`]/[`ServerRequestHandler`]
//! reads as "the other side". Incoming requests are handled on their own task,
//! so a long-running handler (a slow `tools/call`) never blocks the read loop
//! or the notifications interleaved with it.
//!
//! It is generic over the byte stream, so the demux logic is tested with an
//! in-memory duplex rather than a real subprocess.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mcp::transport::McpTransportError;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A notification initiated by the other side: a `method` with `params` and no
/// id.
#[derive(Debug, Clone)]
pub struct ServerNotification {
    pub method: String,
    pub params: Value,
}

/// Handles a request initiated by the other side (for a client:
/// `sampling/createMessage`, `roots/list`; for a server: every client request).
/// Returns the JSON `result` to reply with, or an error mapped to a JSON-RPC
/// error reply.
#[async_trait]
pub trait ServerRequestHandler: Send + Sync {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, ServerRequestError>;
}

/// A peer request failed; becomes a JSON-RPC error reply.
#[derive(Debug, Clone)]
pub struct ServerRequestError {
    pub code: i64,
    pub message: String,
}

impl ServerRequestError {
    /// The JSON-RPC "method not found" error (-32601).
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
        }
    }

    /// The JSON-RPC "invalid params" error (-32602).
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    /// The JSON-RPC "internal error" (-32603).
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
        }
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpTransportError>>>>>;

/// A JSON-RPC peer over a byte stream: sends requests/notifications and demuxes
/// incoming responses, notifications, and peer requests.
pub struct JsonRpcPeer {
    write_tx: mpsc::Sender<String>,
    pending: Pending,
    next_id: AtomicI64,
    closed: CancellationToken,
}

/// A cheap handle for sending notifications through a peer's write queue —
/// what a request handler holds so it can emit `notifications/progress` while
/// its call runs (the handler cannot hold the peer itself: the peer is built
/// *around* the handler).
#[derive(Clone)]
pub struct JsonRpcNotifier {
    write_tx: mpsc::Sender<String>,
}

impl JsonRpcNotifier {
    /// Send a notification (no id, no reply expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), McpTransportError> {
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "method": method, "params": params })
        );
        self.write_tx
            .send(line)
            .await
            .map_err(|_| McpTransportError::ConnectionClosed)
    }
}

impl JsonRpcPeer {
    /// Drive `reader`/`writer`. Peer notifications are forwarded to the
    /// returned receiver; peer requests go to `request_handler` (or are
    /// rejected with method-not-found if it is `None`).
    pub fn new<R, W>(
        reader: R,
        writer: W,
        request_handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> (Self, mpsc::Receiver<ServerNotification>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (write_tx, mut write_rx) = mpsc::channel::<String>(256);
        let (notif_tx, notif_rx) = mpsc::channel::<ServerNotification>(256);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = CancellationToken::new();

        // Writer task: drain queued lines to the stream.
        let closed_w = closed.clone();
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = write_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err()
                {
                    closed_w.cancel();
                    break;
                }
            }
        });

        // Reader task: classify each line and dispatch.
        let pending_r = Arc::clone(&pending);
        let closed_r = closed.clone();
        let write_tx_r = write_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        dispatch(value, &pending_r, &notif_tx, &request_handler, &write_tx_r).await;
                    }
                    _ => {
                        closed_r.cancel();
                        break;
                    }
                }
            }
            // On stream close, drop every pending sender so waiters unblock with
            // a connection-closed error rather than hanging until timeout.
            pending_r.lock().await.clear();
        });

        (
            Self {
                write_tx,
                pending,
                next_id: AtomicI64::new(1),
                closed,
            },
            notif_rx,
        )
    }

    /// Whether the underlying stream is still open.
    pub fn is_alive(&self) -> bool {
        !self.closed.is_cancelled()
    }

    /// Resolve when the underlying stream closes (either half). A stdio server
    /// awaits this to exit when its client disconnects.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }

    /// A cheap cloneable handle for sending notifications through this peer.
    pub fn notifier(&self) -> JsonRpcNotifier {
        JsonRpcNotifier {
            write_tx: self.write_tx.clone(),
        }
    }

    /// Send a request and await its response, up to `timeout`.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpTransportError> {
        if !self.is_alive() {
            return Err(McpTransportError::ConnectionClosed);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        );
        if self.write_tx.send(line).await.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(McpTransportError::ConnectionClosed);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpTransportError::ConnectionClosed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpTransportError::Timeout(format!("{method} timed out")))
            }
        }
    }

    /// Send a notification (no id, no reply expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), McpTransportError> {
        self.notifier().notify(method, params).await
    }
}

/// Classify one decoded message and route it.
async fn dispatch(
    value: Value,
    pending: &Pending,
    notif_tx: &mpsc::Sender<ServerNotification>,
    request_handler: &Option<Arc<dyn ServerRequestHandler>>,
    write_tx: &mpsc::Sender<String>,
) {
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id").filter(|v| !v.is_null()).cloned();

    match (method, id) {
        // Peer request: dispatch on its own task (a slow handler must not stall
        // the read loop) and reply through the write queue.
        (Some(method), Some(id)) => {
            let method = method.to_string();
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let handler = request_handler.clone();
            let write_tx = write_tx.clone();
            tokio::spawn(async move {
                let reply = match handler {
                    Some(handler) => match handler.handle(&method, params).await {
                        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                        Err(err) => json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": err.code, "message": err.message },
                        }),
                    },
                    None => {
                        let err = ServerRequestError::method_not_found(&method);
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": { "code": err.code, "message": err.message },
                        })
                    }
                };
                let _ = write_tx.send(format!("{reply}\n")).await;
            });
        }
        // Notification: forward to the channel.
        (Some(method), None) => {
            let _ = notif_tx
                .send(ServerNotification {
                    method: method.to_string(),
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                })
                .await;
        }
        // Response: resolve the matching pending request.
        (None, Some(id)) => {
            if let Some(id) = id.as_i64()
                && let Some(tx) = pending.lock().await.remove(&id)
            {
                let result = if let Some(err) = value.get("error") {
                    Err(McpTransportError::ServerError(err.to_string()))
                } else {
                    Ok(value.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(result);
            }
        }
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};

    /// Wire the peer to an in-memory "server" end. Returns the peer, its
    /// notification receiver, and the server's read/write halves.
    #[allow(clippy::type_complexity)]
    fn wired(
        handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> (
        JsonRpcPeer,
        mpsc::Receiver<ServerNotification>,
        BufReader<ReadHalf<tokio::io::DuplexStream>>,
        WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (client_r, client_w) = tokio::io::split(client_side);
        let (server_r, server_w) = tokio::io::split(server_side);
        let (peer, notif_rx) = JsonRpcPeer::new(client_r, client_w, handler);
        (peer, notif_rx, BufReader::new(server_r), server_w)
    }

    async fn read_line(server_r: &mut BufReader<ReadHalf<tokio::io::DuplexStream>>) -> Value {
        let mut line = String::new();
        server_r.read_line(&mut line).await.expect("read");
        serde_json::from_str(&line).expect("json")
    }

    #[tokio::test]
    async fn request_resolves_on_matching_response() {
        let (peer, _notif, mut server_r, mut server_w) = wired(None);
        // The server echoes the request id in a result.
        let server = tokio::spawn(async move {
            let req = read_line(&mut server_r).await;
            let id = req["id"].clone();
            assert_eq!(req["method"], "ping");
            let reply = format!(
                "{}\n",
                json!({ "jsonrpc": "2.0", "id": id, "result": { "pong": true } })
            );
            server_w.write_all(reply.as_bytes()).await.unwrap();
        });
        let result = peer
            .request("ping", json!({}), Duration::from_secs(5))
            .await
            .expect("resolves");
        assert_eq!(result["pong"], true);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn notification_reaches_the_channel() {
        let (_peer, mut notif_rx, mut _server_r, mut server_w) = wired(None);
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "method": "notifications/progress",
                    "params": { "progress": 42 } })
        );
        server_w.write_all(line.as_bytes()).await.unwrap();
        let notif = notif_rx.recv().await.expect("a notification");
        assert_eq!(notif.method, "notifications/progress");
        assert_eq!(notif.params["progress"], 42);
    }

    struct FixedHandler;
    #[async_trait]
    impl ServerRequestHandler for FixedHandler {
        async fn handle(&self, method: &str, _params: Value) -> Result<Value, ServerRequestError> {
            assert_eq!(method, "sampling/createMessage");
            Ok(json!({ "role": "assistant", "content": "sampled" }))
        }
    }

    #[tokio::test]
    async fn server_request_is_handled_and_replied() {
        let (_peer, _notif, mut server_r, mut server_w) = wired(Some(Arc::new(FixedHandler)));
        // The server issues a request to the client and reads the reply.
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": 7, "method": "sampling/createMessage",
                    "params": {} })
        );
        server_w.write_all(line.as_bytes()).await.unwrap();
        let reply = read_line(&mut server_r).await;
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["result"]["content"], "sampled");
    }

    #[tokio::test]
    async fn server_request_without_a_handler_is_method_not_found() {
        let (_peer, _notif, mut server_r, mut server_w) = wired(None);
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "sampling/createMessage",
                    "params": {} })
        );
        server_w.write_all(line.as_bytes()).await.unwrap();
        let reply = read_line(&mut server_r).await;
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn request_times_out_without_a_response() {
        let (peer, _notif, _server_r, _server_w) = wired(None);
        let err = peer
            .request("ping", json!({}), Duration::from_millis(50))
            .await
            .expect_err("times out");
        assert!(matches!(err, McpTransportError::Timeout(_)));
    }

    /// A handler that blocks until told to finish, then replies — used to prove
    /// the read loop keeps dispatching while a request is being handled.
    struct SlowHandler {
        release: Mutex<Option<oneshot::Receiver<()>>>,
    }
    #[async_trait]
    impl ServerRequestHandler for SlowHandler {
        async fn handle(&self, _method: &str, _params: Value) -> Result<Value, ServerRequestError> {
            let rx = self.release.lock().await.take().expect("one slow call");
            let _ = rx.await;
            Ok(json!({ "slow": true }))
        }
    }

    #[tokio::test]
    async fn a_slow_request_does_not_block_the_read_loop() {
        let (release_tx, release_rx) = oneshot::channel();
        let (_peer, mut notif_rx, mut server_r, mut server_w) =
            wired(Some(Arc::new(SlowHandler {
                release: Mutex::new(Some(release_rx)),
            })));
        // A request that will park in the handler...
        let slow = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {} })
        );
        server_w.write_all(slow.as_bytes()).await.unwrap();
        // ...must not stop a subsequent notification from being read and routed.
        let notif = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} })
        );
        server_w.write_all(notif.as_bytes()).await.unwrap();
        let seen = notif_rx.recv().await.expect("notification while call runs");
        assert_eq!(seen.method, "notifications/initialized");
        // Release the handler; its reply arrives.
        release_tx.send(()).unwrap();
        let reply = read_line(&mut server_r).await;
        assert_eq!(reply["result"]["slow"], true);
    }

    #[tokio::test]
    async fn closed_resolves_when_the_stream_ends() {
        let (peer, _notif, _server_r, server_w) = wired(None);
        assert!(peer.is_alive());
        drop(server_w);
        drop(_server_r);
        tokio::time::timeout(Duration::from_secs(5), peer.closed())
            .await
            .expect("closed resolves");
        assert!(!peer.is_alive());
    }

    #[tokio::test]
    async fn notifier_writes_through_the_peer() {
        let (peer, _notif, mut server_r, _server_w) = wired(None);
        let notifier = peer.notifier();
        notifier
            .notify("notifications/progress", json!({ "progress": 1 }))
            .await
            .expect("sends");
        let seen = read_line(&mut server_r).await;
        assert_eq!(seen["method"], "notifications/progress");
        assert!(seen.get("id").is_none());
    }

    #[tokio::test]
    async fn response_error_maps_to_server_error() {
        // A JSON-RPC error response resolves the pending request as an Err, not
        // an Ok(Null) — the caller must not mistake a server failure for success.
        let (peer, _notif, mut server_r, mut server_w) = wired(None);
        let server = tokio::spawn(async move {
            let req = read_line(&mut server_r).await;
            let id = req["id"].clone();
            let reply = format!(
                "{}\n",
                json!({ "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32000, "message": "boom" } })
            );
            server_w.write_all(reply.as_bytes()).await.unwrap();
        });
        let err = peer
            .request("do", json!({}), Duration::from_secs(5))
            .await
            .expect_err("error response resolves as Err");
        match err {
            McpTransportError::ServerError(text) => {
                assert!(text.contains("boom"), "carries the server error: {text}");
            }
            other => panic!("expected ServerError, got {other:?}"),
        }
        server.await.unwrap();
    }

    struct FailingHandler;
    #[async_trait]
    impl ServerRequestHandler for FailingHandler {
        async fn handle(&self, _method: &str, _params: Value) -> Result<Value, ServerRequestError> {
            Err(ServerRequestError::invalid_params("bad args"))
        }
    }

    #[tokio::test]
    async fn handler_error_becomes_json_rpc_error_reply() {
        // A handler that fails must produce a well-formed JSON-RPC error reply
        // (its code + message), never a silent drop or a success result.
        let (_peer, _notif, mut server_r, mut server_w) = wired(Some(Arc::new(FailingHandler)));
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "id": 9, "method": "sampling/createMessage",
                    "params": {} })
        );
        server_w.write_all(line.as_bytes()).await.unwrap();
        let reply = read_line(&mut server_r).await;
        assert_eq!(reply["id"], 9);
        assert_eq!(reply["error"]["code"], -32602);
        assert_eq!(reply["error"]["message"], "bad args");
        assert!(reply.get("result").is_none());
    }

    #[tokio::test]
    async fn null_id_with_method_is_treated_as_notification() {
        // An explicit `"id": null` is not a real request id; a message that
        // carries a method with a null id routes to the notification channel,
        // not the peer-request handler (which would try to reply to a null id).
        let (_peer, mut notif_rx, mut _server_r, mut server_w) = wired(None);
        let line = format!(
            "{}\n",
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled",
                    "id": null, "params": { "reason": "x" } })
        );
        server_w.write_all(line.as_bytes()).await.unwrap();
        let notif = notif_rx.recv().await.expect("routed as notification");
        assert_eq!(notif.method, "notifications/cancelled");
        assert_eq!(notif.params["reason"], "x");
    }

    #[tokio::test]
    async fn malformed_line_is_skipped_then_valid_response_resolves() {
        // A garbage (non-JSON) line must not break the read loop or spuriously
        // resolve a request; the subsequent valid response still resolves it.
        let (peer, _notif, mut server_r, mut server_w) = wired(None);
        let server = tokio::spawn(async move {
            let req = read_line(&mut server_r).await;
            let id = req["id"].clone();
            // Garbage, a blank line, then the real reply.
            server_w.write_all(b"this is not json\n").await.unwrap();
            server_w.write_all(b"\n").await.unwrap();
            let reply = format!(
                "{}\n",
                json!({ "jsonrpc": "2.0", "id": id, "result": { "ok": true } })
            );
            server_w.write_all(reply.as_bytes()).await.unwrap();
        });
        let result = peer
            .request("ping", json!({}), Duration::from_secs(5))
            .await
            .expect("valid reply after garbage resolves");
        assert_eq!(result["ok"], true);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn request_after_stream_close_returns_connection_closed() {
        // Once the stream is closed, a new request fails fast with
        // ConnectionClosed rather than queuing and hanging until timeout.
        let (peer, _notif, server_r, server_w) = wired(None);
        drop(server_w);
        drop(server_r);
        tokio::time::timeout(Duration::from_secs(5), peer.closed())
            .await
            .expect("closed resolves");
        let err = peer
            .request("ping", json!({}), Duration::from_secs(5))
            .await
            .expect_err("request on a closed peer fails");
        assert!(matches!(err, McpTransportError::ConnectionClosed));
    }

    #[tokio::test]
    async fn concurrent_requests_resolve_out_of_order_to_the_right_id() {
        // The peer's core job: several in-flight requests are demuxed by id
        // through the `pending` map. Fire three concurrently, then answer them in
        // REVERSE order, so responses arrive out of order w.r.t. the requests —
        // each future must still resolve to its OWN result, never a sibling's.
        let (peer, _notif, mut server_r, mut server_w) = wired(None);
        let peer = Arc::new(peer);

        // Each request carries a distinct marker `n`; handles[i] is the task for n=i.
        let mut handles = Vec::new();
        for n in 0..3i64 {
            let peer = Arc::clone(&peer);
            handles.push(tokio::spawn(async move {
                peer.request("echo", json!({ "n": n }), Duration::from_secs(5))
                    .await
            }));
        }

        // Read all three requests, remembering each wire id -> its marker n.
        let mut seen = Vec::new();
        for _ in 0..3 {
            let req = read_line(&mut server_r).await;
            let id = req["id"].as_i64().expect("numeric id");
            let n = req["params"]["n"].as_i64().expect("marker");
            seen.push((id, n));
        }
        // Reply in reverse arrival order; the result echoes that id's own marker.
        for (id, n) in seen.into_iter().rev() {
            let reply = format!(
                "{}\n",
                json!({ "jsonrpc": "2.0", "id": id, "result": { "n": n } })
            );
            server_w.write_all(reply.as_bytes()).await.unwrap();
        }

        // Every future resolves to its own marker despite the reversed replies.
        for (i, handle) in handles.into_iter().enumerate() {
            let result = handle.await.unwrap().expect("resolves");
            assert_eq!(
                result["n"], i as i64,
                "request n={i} resolved to the wrong response"
            );
        }
    }

    #[tokio::test]
    async fn notify_does_not_check_liveness_unlike_request() {
        // Characterize the request/notify asymmetry on a closed peer: request()
        // consults is_alive() and fails fast, but notify() does not — it only
        // queues to the write task, so the first notify still returns Ok even
        // though the stream is gone (fire-and-forget, liveness unchecked).
        let (peer, _notif, server_r, server_w) = wired(None);
        drop(server_w);
        drop(server_r);
        tokio::time::timeout(Duration::from_secs(5), peer.closed())
            .await
            .expect("closed resolves");
        assert!(!peer.is_alive());

        let err = peer
            .request("ping", json!({}), Duration::from_secs(5))
            .await
            .expect_err("request rejected on a closed peer");
        assert!(matches!(err, McpTransportError::ConnectionClosed));

        peer.notify("notifications/progress", json!({}))
            .await
            .expect("notify queues despite closure — liveness is not checked");
    }
}
