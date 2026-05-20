//! `McpServerSpec` (stdio + HTTP) round-trip — pins the connection-config
//! shape `how-to/use-mcp-tools.md` cites.

use awaken::registry_spec::{McpServerSpec, McpTransportKind};

fn main() {
    // Stdio transport — child-process MCP server.
    let stdio = McpServerSpec {
        id: "filesystem".into(),
        transport: McpTransportKind::Stdio,
        command: Some("python".into()),
        args: vec!["-m".into(), "mcp_filesystem".into()],
        env: [("PYTHONUNBUFFERED".into(), "1".into())]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let json = serde_json::to_value(&stdio).expect("encode stdio");
    let parsed: McpServerSpec = serde_json::from_value(json).expect("decode stdio");
    assert_eq!(parsed.id, "filesystem");
    assert!(matches!(parsed.transport, McpTransportKind::Stdio));
    assert_eq!(parsed.args.len(), 2);

    // HTTP transport variant — `command/args` get skipped, `url` is set.
    let http = McpServerSpec {
        id: "remote-mcp".into(),
        transport: McpTransportKind::Http,
        url: Some("https://mcp.example.com/rpc".into()),
        ..Default::default()
    };
    let http_json = serde_json::to_value(&http).expect("encode http");
    assert_eq!(http_json["transport"], "http");
    assert_eq!(http_json["url"], "https://mcp.example.com/rpc");
    assert!(http_json.get("command").is_none(), "command must be elided");
}
