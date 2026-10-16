// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Test-only original transport/catalog owner observations; local byte-channel EOF is not remote process-stop proof.
//! Real MCP framing and channel EOF distinguish physical lifetime from flags.

use super::*;
use awaken_runtime_contract::plugin::Plugin as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn connected_stdio() -> (crate::StdioTransport, tokio::task::JoinHandle<Vec<String>>) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let peer = tokio::spawn(async move {
        let (reader, mut writer) = tokio::io::split(server);
        let mut lines = BufReader::new(reader).lines();
        let mut methods = Vec::new();
        while let Some(line) = lines.next_line().await.expect("read actual MCP framing") {
            let message: Value = serde_json::from_str(&line).expect("actual JSON RPC request");
            let Some(id) = message.get("id").cloned() else {
                continue;
            };
            let method = message["method"].as_str().expect("MCP method");
            methods.push(method.to_owned());
            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "serverInfo": {"name": "lifetime", "version": "1"}
                }),
                "tools/list" => serde_json::json!({"tools": [
                    {"name": "probe", "inputSchema": {"type": "object"}}
                ]}),
                other => panic!("unexpected dispatched MCP request: {other}"),
            };
            let reply = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
            writer
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .expect("reply on original channel");
        }
        methods
    });
    let transport = crate::StdioTransport::connect_stream(
        client,
        None,
        std::time::Duration::from_secs(5),
        None,
    )
    .await
    .expect("real MCP client initialization");
    (transport, peer)
}

async fn observe_original_eof(peer: &mut tokio::task::JoinHandle<Vec<String>>) -> bool {
    match tokio::time::timeout(std::time::Duration::from_secs(3), peer).await {
        Ok(result) => {
            let methods = result.expect("peer completed at EOF");
            assert_eq!(methods.first().map(String::as_str), Some("initialize"));
            assert!(methods.iter().any(|method| method == "tools/list"));
            true
        }
        Err(_) => false,
    }
}

async fn settle_after_failed_observation(peer: &mut tokio::task::JoinHandle<Vec<String>>) {
    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut *peer)
        .await
        .is_err()
    {
        // Test-owned peer cleanup only after the actual missing EOF observation.
        peer.abort();
        let _ = peer.await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unwrapped_stdio_last_owner_drop_releases_the_original_channel() {
    let (transport, mut peer) = connected_stdio().await;
    assert_eq!(transport.list_tools().await.unwrap()[0].name, "probe");
    assert!(!peer.is_finished(), "the actual channel is initially live");
    drop(transport);
    let eof = observe_original_eof(&mut peer).await;
    if !eof {
        settle_after_failed_observation(&mut peer).await;
    }
    assert!(
        eof,
        "unwrapped stdio owner drop did not release the original channel"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generation_drain_releases_original_stdio_despite_cached_transport() {
    // D1 initialize and discover over the real wire; D2 retain a cached wrapper;
    // D3 close the original generation and observe actual peer EOF before D2
    // is dropped. This is local byte-channel shutdown, not remote process death.
    let (transport, mut peer) = connected_stdio().await;
    let (cached, fence) = revocable_transport(Arc::new(transport));
    assert_eq!(cached.list_tools().await.unwrap()[0].name, "probe");
    fence.close_and_wait().await;
    assert!(
        matches!(
            cached.list_tools().await,
            Err(McpTransportError::NotDispatched(_))
        ),
        "the cached consumer is closed to new effects"
    );
    let eof = observe_original_eof(&mut peer).await;
    drop(cached);
    drop(fence);
    if !eof {
        settle_after_failed_observation(&mut peer).await;
    }
    assert!(
        eof,
        "drained generation retained the original physical stdio channel through its cached transport"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_registry_owner_drop_releases_the_original_stdio_channel() {
    // R1 the server and its explicit plugin share one catalog owner; R2 the
    // plugin continues refreshing after the server handle drops; R3 the last
    // explicit owner drops while the external event source remains alive.
    // R3 must release the original wire, without waiting for a bus event.
    let (transport, mut peer) = connected_stdio().await;
    let (changes, receiver) = tokio::sync::broadcast::channel(8);
    let server = crate::McpServer::start("lifetime", Arc::new(transport), receiver)
        .await
        .unwrap();
    let plugin = server.plugin();
    assert_eq!(plugin.live_version(), Some(1));
    drop(server);
    changes
        .send(ListChangedKind::Tools)
        .expect("explicit plugin owner retains refresh subscription");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while plugin.live_version() != Some(2) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the explicit plugin owner still refreshes the actual catalog");
    assert!(
        !peer.is_finished(),
        "explicit plugin still owns the original wire"
    );
    drop(plugin);
    let eof = observe_original_eof(&mut peer).await;
    // Close only the test-owned event source to settle the old detached loop.
    drop(changes);
    if !eof {
        settle_after_failed_observation(&mut peer).await;
    }
    assert!(
        eof,
        "last registry owner drop left the original physical stdio channel owned by its detached observer"
    );
}

struct RetainedNotification;

#[async_trait]
impl crate::notification::McpNotificationHandler for RetainedNotification {
    async fn receive(&self, _method: &str, _params: Value) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_generation_owner_releases_stdio_with_cached_notification_consumer() {
    // O1 the generation owns a live original channel; O2 its transport owns a
    // notification callback; O3 cached consumers outlive the last explicit
    // generation owner. O2 must not make an ownership cycle, and O3 grants no
    // authority to reconnect or send a new request.
    let (transport, mut peer) = connected_stdio().await;
    let (cached, fence) = revocable_transport(Arc::new(transport));
    assert_eq!(cached.list_tools().await.unwrap()[0].name, "probe");
    cached
        .set_notification_handler(
            vec!["notifications/phone/input".into()],
            Some(Arc::new(RetainedNotification)),
        )
        .expect("install a real original transport callback");
    let other_owner = fence.clone();
    drop(fence);
    assert_eq!(cached.list_tools().await.unwrap()[0].name, "probe");
    drop(other_owner);
    let eof = observe_original_eof(&mut peer).await;
    assert!(!cached.is_alive());
    assert!(matches!(
        cached.list_tools().await,
        Err(McpTransportError::NotDispatched(_))
    ));
    drop(cached);
    if !eof {
        settle_after_failed_observation(&mut peer).await;
    }
    assert!(
        eof,
        "the last generation owner must release the original channel despite cached callbacks"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_close_waiter_releases_stdio_when_original_permit_settles() {
    // W1 the original local fence has a counted admitted permit; W2 close has
    // actually cancelled that permit; W3 its waiter is cancelled before the
    // permit settles. The last permit must complete local release without a
    // replacement waiter. This proves neither remote rollback nor task death.
    let (transport, mut peer) = connected_stdio().await;
    let (cached, fence) = revocable_transport(Arc::new(transport));
    assert_eq!(cached.list_tools().await.unwrap()[0].name, "probe");
    let permit = fence.try_enter().expect("original counted admission");
    let closing = tokio::spawn({
        let fence = fence.clone();
        async move { fence.close_and_wait().await }
    });
    permit.cancelled().await;
    closing.abort();
    assert!(closing.await.unwrap_err().is_cancelled());
    assert!(
        !peer.is_finished(),
        "the original admitted permit still owns I/O"
    );
    assert!(matches!(
        cached.list_tools().await,
        Err(McpTransportError::NotDispatched(_))
    ));
    drop(permit);
    let eof = observe_original_eof(&mut peer).await;
    drop(cached);
    drop(fence);
    if !eof {
        settle_after_failed_observation(&mut peer).await;
    }
    assert!(
        eof,
        "local release must survive cancellation of its close waiter"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_generation_owner_rejects_cached_calls_while_original_permit_is_retained() {
    // P1 a real connection has completed MCP initialization and discovery;
    // P2 an original admitted permit still retains its allocation; P3 the last
    // generation owner exits. P2 may retain cleanup memory, never P3's admission
    // authority. Observe real request refusal, cancellation and eventual EOF.
    let (transport, mut peer) = connected_stdio().await;
    let (cached, fence) = revocable_transport(Arc::new(transport));
    assert_eq!(cached.list_tools().await.unwrap()[0].name, "probe");
    let permit = fence.try_enter().expect("original counted admission");
    assert!(!permit.is_cancelled());
    drop(fence);
    let refused = matches!(
        cached.list_tools().await,
        Err(McpTransportError::NotDispatched(_))
    );
    let original_permit_cancelled = permit.is_cancelled();
    assert!(
        !peer.is_finished(),
        "the original permit retains local cleanup"
    );
    drop(permit);
    let tool_requests =
        match tokio::time::timeout(std::time::Duration::from_secs(3), &mut peer).await {
            Ok(result) => {
                let methods = result.expect("join original MCP peer at EOF");
                assert_eq!(methods.first().map(String::as_str), Some("initialize"));
                Some(
                    methods
                        .iter()
                        .filter(|method| method.as_str() == "tools/list")
                        .count(),
                )
            }
            Err(_) => None,
        };
    let eof = tool_requests.is_some();
    drop(cached);
    if !eof {
        settle_after_failed_observation(&mut peer).await;
    }
    assert!(
        refused,
        "cached consumer regained admission from a retained permit after the last generation owner exited"
    );
    assert!(
        original_permit_cancelled,
        "owner exit cancels the original permit"
    );
    assert_eq!(
        tool_requests,
        Some(1),
        "no request is emitted after owner exit"
    );
    assert!(
        eof,
        "the settled original permit releases I/O despite cached consumers"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_generation_owner_cancels_original_stdio_call_without_reclassifying_dispatch() {
    // A1 tools/call is observed on the original byte channel, with its reply
    // deliberately outstanding; A2 the last original generation owner exits.
    // The original local future must settle as revoked/unknown, no later
    // request may be sent, and the original channel must reach EOF. This is
    // not proof that an already-dispatched remote tool effect was undone.
    let (client, server) = tokio::io::duplex(64 * 1024);
    let entered = Arc::new(Notify::new());
    let mut peer = tokio::spawn({
        let entered = entered.clone();
        async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader).lines();
            let mut methods = Vec::new();
            while let Some(line) = lines.next_line().await.expect("original MCP read") {
                let frame: Value = serde_json::from_str(&line).expect("actual JSON RPC frame");
                let Some(id) = frame.get("id").cloned() else {
                    continue;
                };
                let method = frame["method"].as_str().expect("MCP method");
                methods.push(method.to_owned());
                let result = match method {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2025-06-18", "capabilities": {},
                        "serverInfo": {"name": "pending-owner", "version": "1"}
                    }),
                    "tools/list" => serde_json::json!({"tools": []}),
                    "tools/call" => {
                        assert_eq!(frame["params"]["name"], "probe");
                        entered.notify_one();
                        // The actual request is dispatched. Leave its result
                        // unknown while continuing to observe later frames/EOF.
                        continue;
                    }
                    other => panic!("unexpected original MCP method: {other}"),
                };
                let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("reply on original MCP channel");
            }
            methods
        }
    });
    let transport = crate::StdioTransport::connect_stream(
        client,
        None,
        std::time::Duration::from_secs(30),
        None,
    )
    .await
    .unwrap();
    let (cached, fence) = revocable_transport(Arc::new(transport));
    let mut call = tokio::spawn({
        let cached = cached.clone();
        async move { cached.call_tool("probe", Value::Null).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .expect("tools/call actually dispatched on the original channel");
    drop(fence);
    let refused = matches!(
        cached.list_tools().await,
        Err(McpTransportError::NotDispatched(_))
    );
    let original_settled_unknown =
        match tokio::time::timeout(std::time::Duration::from_secs(3), &mut call).await {
            Ok(result) => matches!(
                result.unwrap(),
                Err(McpTransportError::TransportError(message)) if message.contains("revoked")
            ),
            Err(_) => {
                // Test-owned cleanup only AFTER observing the missing settlement.
                call.abort();
                let _ = call.await;
                false
            }
        };
    let observed = tokio::time::timeout(std::time::Duration::from_secs(3), &mut peer).await;
    let methods = match observed {
        Ok(result) => Some(result.expect("original peer reached EOF")),
        Err(_) => None,
    };
    drop(cached);
    if methods.is_none() {
        settle_after_failed_observation(&mut peer).await;
    }
    assert!(
        refused,
        "cached consumer sent a later request while the original dispatched call retained allocation after owner exit"
    );
    assert!(
        original_settled_unknown,
        "last generation owner exit failed to revoke the original dispatched local future"
    );
    assert_eq!(
        methods,
        Some(vec!["initialize".into(), "tools/call".into()])
    );
}
