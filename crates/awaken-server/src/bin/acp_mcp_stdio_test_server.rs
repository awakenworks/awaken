use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }

        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            continue;
        };

        let response = match method {
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {
                    "protocolVersion": mcp::MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "acp-mcp-stdio-test-server",
                        "version": "1.0.0"
                    }
                }
            })),
            "notifications/initialized" => None,
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {
                    "tools": [{
                        "name": "echo",
                        "title": "Echo",
                        "description": "Echoes the message argument",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "message": { "type": "string" }
                            },
                            "required": ["message"]
                        }
                    }]
                }
            })),
            "tools/call" => {
                let message = request["params"]["arguments"]["message"]
                    .as_str()
                    .unwrap_or_default();
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": {
                        "content": [{
                            "type": "text",
                            "text": message
                        }],
                        "structuredContent": {
                            "message": message
                        }
                    }
                }))
            }
            _ => Some(json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {}
            })),
        };

        if let Some(response) = response {
            let _ = writeln!(stdout, "{response}");
            let _ = stdout.flush();
        }
    }
}
