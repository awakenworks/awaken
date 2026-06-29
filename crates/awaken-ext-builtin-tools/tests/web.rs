//! Network tools. `web_fetch` is exercised against a hermetic local server;
//! `web_search` hits DuckDuckGo and is ignored by default (needs network).

use awaken_ext_builtin_tools::web_hand_tools;
use awaken_runtime_contract::tool::{RawTool, ToolCall};
use std::io::{Read, Write};
use std::sync::Arc;

fn tool(id: &str) -> Arc<dyn RawTool> {
    web_hand_tools()
        .into_iter()
        .find(|t| t.id() == id)
        .unwrap_or_else(|| panic!("no web tool {id}"))
}

fn call(id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: "c1".to_string(),
        tool_id: id.to_string(),
        arguments: args,
    }
}

#[tokio::test]
async fn web_fetch_returns_the_response_body() {
    // A one-shot HTTP server on an ephemeral port serves a fixed body.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let body = "hello from the web";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).expect("write");
    });

    let url = format!("http://{addr}/");
    let out = tool("web_fetch")
        .invoke(call("web_fetch", serde_json::json!({ "url": url })))
        .await
        .expect("fetch");
    assert_eq!(out.content, "hello from the web");
    assert!(!out.is_error);
    server.join().expect("server thread");
}

#[tokio::test]
async fn web_fetch_unreachable_host_is_a_typed_error() {
    // Port 0 of the discard range never accepts; the GET fails, not panics.
    let err = tool("web_fetch")
        .invoke(call(
            "web_fetch",
            serde_json::json!({ "url": "http://127.0.0.1:9/" }),
        ))
        .await
        .expect_err("connection refused");
    assert!(err.to_string().contains("fetch"));
}

#[tokio::test]
#[ignore = "requires network (DuckDuckGo)"]
async fn web_search_returns_results() {
    let out = tool("web_search")
        .invoke(call(
            "web_search",
            serde_json::json!({ "query": "rust programming language" }),
        ))
        .await
        .expect("search");
    println!("[web_search] -> {}", out.content);
    assert!(!out.content.is_empty());
}
