use awaken_server::protocols::acp::stdio::{JsonRpcResponse, parse_request, serialize_response};

#[test]
fn stdio_jsonrpc_roundtrip() {
    let req = parse_request(r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#).unwrap();
    assert_eq!(req.method, "initialize");
    let out = serialize_response(&JsonRpcResponse::success(
        req.id,
        serde_json::json!({"ok":true}),
    ));
    assert!(out.contains("\"ok\":true"));
}
