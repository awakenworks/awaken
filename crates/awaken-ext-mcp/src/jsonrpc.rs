//! A minimal JSON-RPC 2.0 peer over an async byte stream.
//!
//! The MCP SDK's client transport only resolves numeric-id responses and drops
//! server notifications and server->client requests — so `progress`,
//! `tools/list_changed`, `resources/updated`, and `sampling` cannot flow through
//! it. This peer demuxes all three:
//!
//! - **response** (`id`, no `method`) resolves the matching pending request;
//! - **notification** (`method`, no `id`) is forwarded to a channel;
//! - **server request** (`id` + `method`) is dispatched to a handler and its
//!   reply written back.
//!
//! It is generic over the byte stream, so the demux logic is tested with an
//! in-memory duplex rather than a real subprocess.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mcp::transport::McpTransportError;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

/// A server-initiated notification: a `method` with `params` and no id.
#[derive(Debug, Clone)]
pub struct ServerNotification {
    pub method: String,
    pub params: Value,
}

/// Handles a server->client request (e.g. `sampling/createMessage`,
/// `roots/list`). Returns the JSON `result` to reply with, or an error mapped to
/// a JSON-RPC error reply.
#[async_trait]
pub trait ServerRequestHandler: Send + Sync {
    async fn handle(&self, method: &str, params: Value) -> Result<Value, ServerRequestError>;
}

/// A server->client request failed; becomes a JSON-RPC error reply.
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
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, McpTransportError>>>>>;

/// A JSON-RPC peer over a byte stream: sends requests/notifications and demuxes
/// incoming responses, notifications, and server requests.
pub struct JsonRpcPeer {
    write_tx: mpsc::Sender<String>,
    pending: Pending,
    next_id: AtomicI64,
    alive: Arc<AtomicBool>,
}

impl JsonRpcPeer {
    /// Drive `reader`/`writer`. Server notifications are forwarded to the
    /// returned receiver; server requests go to `request_handler` (or are
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
        let alive = Arc::new(AtomicBool::new(true));

        // Writer task: drain queued lines to the stream.
        let alive_w = Arc::clone(&alive);
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = write_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err()
                {
                    alive_w.store(false, Ordering::SeqCst);
                    break;
                }
            }
        });

        // Reader task: classify each line and dispatch.
        let pending_r = Arc::clone(&pending);
        let alive_r = Arc::clone(&alive);
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
                        alive_r.store(false, Ordering::SeqCst);
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
                alive,
            },
            notif_rx,
        )
    }

    /// Whether the underlying stream is still open.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
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
        // Server -> client request: dispatch and reply.
        (Some(method), Some(id)) => {
            let method = method.to_string();
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let reply = match request_handler {
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
}
