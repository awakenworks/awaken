//! MCP Stdio transport: line-delimited JSON-RPC over stdin/stdout.
//!
//! Reads JSON-RPC messages from stdin, dispatches them through [`McpServer`],
//! and writes responses to stdout.

use std::sync::Arc;

use mcp::protocol::{
    ClientInbound, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, ServerOutbound,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use awaken_runtime::AgentRuntime;

const JSON_RPC_VERSION: &str = "2.0";

/// Run the MCP stdio server on actual stdin/stdout.
pub async fn serve_stdio(runtime: Arc<AgentRuntime>) {
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_stdio_io(runtime, stdin, stdout).await;
}

/// Run the MCP stdio server with injectable I/O (for testing).
pub async fn serve_stdio_io<R, W>(runtime: Arc<AgentRuntime>, input: R, mut output: W)
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (_server, mut channels) = super::create_mcp_server(&runtime);
    let mut lines = input.lines();
    let mut pending_requests: usize = 0;
    let mut input_done = false;
    let mut saw_initialize = false;

    loop {
        if input_done && pending_requests == 0 {
            break;
        }

        tokio::select! {
            line_result = lines.next_line(), if !input_done => {
                match line_result {
                    Ok(Some(line)) => {
                        let line = line.trim().to_string();
                        if line.is_empty() {
                            continue;
                        }
                        match parse_line(&line) {
                            Ok(parsed) => {
                                if let Some(error) = lifecycle_error(&parsed, saw_initialize) {
                                    let _ = write_line(&mut output, &error).await;
                                    continue;
                                }

                                if matches!(&parsed, ParsedInbound::Request(request) if request.method == "initialize") {
                                    saw_initialize = true;
                                }

                                match dispatch(parsed, &channels.inbound_tx).await {
                                    Ok(expects_response) => {
                                        if expects_response {
                                            pending_requests += 1;
                                        }
                                    }
                                    Err(e) => {
                                        let resp = make_error_response(None, -32603, &e);
                                        let _ = write_line(&mut output, &resp).await;
                                    }
                                }
                            }
                            Err(err) => {
                                let resp = make_error_response(err.id, err.code, &err.message);
                                let _ = write_line(&mut output, &resp).await;
                            }
                        }
                    }
                    Ok(None) => {
                        input_done = true;
                    }
                    Err(_) => {
                        input_done = true;
                    }
                }
            }
            outbound = channels.outbound_rx.recv() => {
                match outbound {
                    Some(msg) => {
                        let line = match &msg {
                            ServerOutbound::Response(resp) => {
                                pending_requests = pending_requests.saturating_sub(1);
                                serde_json::to_string(resp).ok()
                            }
                            ServerOutbound::Notification(notif) => serde_json::to_string(notif).ok(),
                            ServerOutbound::Request(req) => serde_json::to_string(req).ok(),
                        };
                        if let Some(line) = line {
                            let _ = write_line(&mut output, &line).await;
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

#[derive(Debug)]
enum ParsedInbound {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

struct LineParseError {
    id: Option<Value>,
    code: i64,
    message: String,
}

fn parse_line(line: &str) -> Result<ParsedInbound, LineParseError> {
    let value: Value = serde_json::from_str(line).map_err(|e| LineParseError {
        id: None,
        code: -32700,
        message: format!("parse error: {e}"),
    })?;

    if !value.is_object() {
        return Err(LineParseError {
            id: None,
            code: -32600,
            message: "expected a single JSON-RPC object".to_string(),
        });
    }

    let id = value.get("id").cloned();
    match value.get("jsonrpc").and_then(Value::as_str) {
        Some(JSON_RPC_VERSION) => {}
        _ => {
            return Err(LineParseError {
                id,
                code: -32600,
                message: "JSON-RPC messages must include \"jsonrpc\": \"2.0\"".to_string(),
            });
        }
    }

    if let Some(method) = value.get("method").and_then(Value::as_str) {
        if method.is_empty() {
            return Err(LineParseError {
                id,
                code: -32600,
                message: "missing 'method' field".to_string(),
            });
        }
        match value.get("id") {
            Some(Value::Null) => Err(LineParseError {
                id,
                code: -32600,
                message: "MCP requests MUST use string or integer IDs; notifications MUST omit the id field".to_string(),
            }),
            Some(_) => serde_json::from_value(value)
                .map(ParsedInbound::Request)
                .map_err(|e| LineParseError {
                    id: None,
                    code: -32600,
                    message: format!("invalid request: {e}"),
                }),
            None => serde_json::from_value(value)
                .map(ParsedInbound::Notification)
                .map_err(|e| LineParseError {
                    id: None,
                    code: -32600,
                    message: format!("invalid notification: {e}"),
                }),
        }
    } else if value.get("result").is_some() || value.get("error").is_some() {
        if matches!(value.get("id"), Some(Value::Null)) {
            return Err(LineParseError {
                id,
                code: -32600,
                message: "JSON-RPC responses MUST use the original string or integer request id"
                    .to_string(),
            });
        }
        serde_json::from_value(value)
            .map(ParsedInbound::Response)
            .map_err(|e| LineParseError {
                id: None,
                code: -32600,
                message: format!("invalid response: {e}"),
            })
    } else {
        Err(LineParseError {
            id,
            code: -32600,
            message: "missing 'method' field".to_string(),
        })
    }
}

fn lifecycle_error(parsed: &ParsedInbound, saw_initialize: bool) -> Option<String> {
    match parsed {
        ParsedInbound::Request(request)
            if !saw_initialize && request.method != "initialize" && request.method != "ping" =>
        {
            Some(make_error_response(
                Some(json_rpc_id_to_value(&request.id)),
                -32600,
                "initialize must be the first request in an MCP stdio session",
            ))
        }
        ParsedInbound::Notification(notification)
            if !saw_initialize && notification.method == "notifications/initialized" =>
        {
            Some(make_error_response(
                None,
                -32600,
                "received notifications/initialized before initialize",
            ))
        }
        _ => None,
    }
}

async fn dispatch(
    parsed: ParsedInbound,
    inbound_tx: &tokio::sync::mpsc::Sender<ClientInbound>,
) -> Result<bool, String> {
    match parsed {
        ParsedInbound::Request(request) => {
            inbound_tx
                .send(ClientInbound::Request(request))
                .await
                .map_err(|_| "channel closed".to_string())?;
            Ok(true)
        }
        ParsedInbound::Notification(notification) => {
            inbound_tx
                .send(ClientInbound::Notification(notification))
                .await
                .map_err(|_| "channel closed".to_string())?;
            Ok(false)
        }
        ParsedInbound::Response(response) => {
            inbound_tx
                .send(ClientInbound::Response(response))
                .await
                .map_err(|_| "channel closed".to_string())?;
            Ok(false)
        }
    }
}

fn json_rpc_id_to_value(id: &JsonRpcId) -> Value {
    match id {
        JsonRpcId::String(value) => Value::String(value.clone()),
        JsonRpcId::Number(value) => serde_json::json!(*value),
        JsonRpcId::Null => Value::Null,
    }
}

fn make_error_response(id: Option<Value>, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": JSON_RPC_VERSION,
        "error": {
            "code": code,
            "message": message
        },
        "id": id
    })
    .to_string()
}

async fn write_line<W: AsyncWriteExt + Unpin>(writer: &mut W, line: &str) -> std::io::Result<()> {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime::{AgentResolver, AgentRuntime, ResolvedAgent, RuntimeError};

    struct StubResolver;
    impl AgentResolver for StubResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
        fn agent_ids(&self) -> Vec<String> {
            vec!["test-agent".into()]
        }
    }

    fn test_runtime() -> Arc<AgentRuntime> {
        Arc::new(AgentRuntime::new(Arc::new(StubResolver)))
    }

    async fn run_stdio(input: &[u8]) -> String {
        let runtime = test_runtime();
        let mut output = Vec::new();
        serve_stdio_io(runtime, input, &mut output).await;
        String::from_utf8(output).unwrap()
    }

    fn first_response(output: &str) -> Value {
        let line = output.lines().next().expect("no output");
        serde_json::from_str(line).expect("invalid JSON")
    }

    #[tokio::test]
    async fn stdio_initialize() {
        let output = run_stdio(
            b"{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1.0.0\"}},\"id\":1}\n",
        )
        .await;

        let resp = first_response(&output);
        assert!(resp["result"]["protocolVersion"].is_string());
        assert_eq!(resp["id"], 1);
    }

    #[tokio::test]
    async fn stdio_rejects_request_before_initialize() {
        let output = run_stdio(b"{\"jsonrpc\":\"2.0\",\"method\":\"tools/list\",\"id\":2}\n").await;
        let resp = first_response(&output);
        assert_eq!(resp["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn stdio_unknown_method() {
        let output = run_stdio(
            concat!(
                "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1.0.0\"}},\"id\":1}\n",
                "{\"jsonrpc\":\"2.0\",\"method\":\"unknown/foo\",\"id\":3}\n",
            )
            .as_bytes(),
        )
        .await;
        let responses: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let resp = responses
            .into_iter()
            .find(|value| value["id"] == 3)
            .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn parse_line_rejects_null_id_request() {
        let err = parse_line(r#"{"jsonrpc":"2.0","method":"tools/list","id":null}"#)
            .expect_err("null id must be rejected");
        assert_eq!(err.code, -32600);
    }

    #[test]
    fn make_error_response_preserves_id() {
        let resp = make_error_response(Some(Value::from(7)), -32600, "bad request");
        let parsed: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(parsed["id"], 7);
    }
}
