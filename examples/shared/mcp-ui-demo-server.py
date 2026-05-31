"""
Minimal MCP server over stdio with UI resource support.

Exposes a `dashboard_view` tool with `_meta.ui.resourceUri` metadata.
When the tool is executed, the agent fetches HTML via `resources/read`
and attaches it to the ToolResult metadata for frontend iframe rendering.

Usage:
    python3 -u mcp-ui-demo-server.py
"""

import json
import sys

DASHBOARD_HTML = """\
<html>
<head>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { font-family: system-ui, -apple-system, sans-serif; padding: 20px; color: #1e293b; }
  h2 { font-size: 1.25rem; margin-bottom: 12px; }
  .metric { display: inline-block; background: #f1f5f9; border: 1px solid #e2e8f0;
            border-radius: 8px; padding: 12px 16px; margin: 4px; min-width: 120px; }
  .metric .value { font-size: 1.5rem; font-weight: 700; color: #0f172a; }
  .metric .label { font-size: 0.75rem; color: #64748b; margin-top: 2px; }
  .bar { height: 8px; border-radius: 4px; margin-top: 8px; }
  .section { margin-top: 16px; }
</style>
</head>
<body>
  <h2>Dashboard</h2>
  <div>
    <div class="metric"><div class="value">1,247</div><div class="label">Active Users</div></div>
    <div class="metric"><div class="value">89.3%</div><div class="label">Uptime</div></div>
    <div class="metric"><div class="value">42ms</div><div class="label">Avg Latency</div></div>
  </div>
  <div class="section">
    <strong>Request Volume (last 24h)</strong>
    <div class="bar" style="width:85%;background:#3b82f6"></div>
    <div style="font-size:0.75rem;color:#64748b;margin-top:4px">12,847 requests</div>
  </div>
  <div class="section">
    <strong>Error Rate</strong>
    <div class="bar" style="width:3%;background:#ef4444"></div>
    <div style="font-size:0.75rem;color:#64748b;margin-top:4px">0.3%</div>
  </div>
</body>
</html>
"""


def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()


for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    message = json.loads(raw)
    method = message.get("method")
    msg_id = message.get("id")

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "protocolVersion": "2025-11-25",
                "serverInfo": {"name": "mcp-ui-demo", "version": "0.1.0"},
                "capabilities": {
                    "tools": {},
                    "resources": {}
                }
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "tools": [{
                    "name": "dashboard_view",
                    "title": "Dashboard View",
                    "description": "Show a live dashboard with metrics. Returns dashboard data and renders an interactive UI.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Optional filter query for dashboard metrics"
                            }
                        }
                    },
                    "_meta": {
                        "ui": {
                            "resourceUri": "ui://demo/dashboard"
                        }
                    }
                }]
            }
        })
    elif method == "tools/call":
        params = message.get("params") or {}
        arguments = params.get("arguments") or {}
        query = arguments.get("query", "all")
        send({
            "jsonrpc": "2.0",
            "id": msg_id,
            "result": {
                "content": [{
                    "type": "text",
                    "text": json.dumps({
                        "active_users": 1247,
                        "uptime_percent": 89.3,
                        "avg_latency_ms": 42,
                        "requests_24h": 12847,
                        "error_rate": 0.003,
                        "query": query
                    })
                }]
            }
        })
    elif method == "resources/read":
        params = message.get("params") or {}
        uri = params.get("uri", "")
        if uri == "ui://demo/dashboard":
            send({
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "contents": [{
                        "uri": "ui://demo/dashboard",
                        "text": DASHBOARD_HTML,
                        "mimeType": "text/html"
                    }]
                }
            })
        else:
            send({
                "jsonrpc": "2.0",
                "id": msg_id,
                "error": {
                    "code": -32602,
                    "message": f"Unknown resource: {uri}"
                }
            })
    else:
        if msg_id is not None:
            send({"jsonrpc": "2.0", "id": msg_id, "result": {}})
