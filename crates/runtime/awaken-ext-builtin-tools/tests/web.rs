//! The static network surface contains only `web_fetch`; configurable search is
//! owned by `WebSearchPlugin` and covered by its provider-contract tests.

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
    assert_eq!(out.text(), "hello from the web");
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

// SECURITY/BOUNDARY: a hostile or runaway server must not blow up the transcript.
// `WebFetchTool` caps the read at `MAX_BODY` (1 MiB) via `.take(MAX_BODY)`; a body
// larger than the cap must be truncated to exactly 1 MiB, never returned whole.
#[tokio::test]
async fn web_fetch_caps_the_body_at_one_mebibyte() {
    const MAX_BODY: usize = 1 << 20; // must match web.rs
    // Serve slightly more than the cap so truncation is observable but the small
    // residual (past what the client drains) fits in the socket buffers.
    let body_len = MAX_BODY + 4096;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    // Detached: the client stops reading at the cap, so the tail may never be
    // drained — never join on the writer, and ignore its write error.
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let header =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n");
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(&vec![b'a'; body_len]);
    });

    let url = format!("http://{addr}/");
    let out = tool("web_fetch")
        .invoke(call("web_fetch", serde_json::json!({ "url": url })))
        .await
        .expect("fetch");
    assert!(!out.is_error);
    let content = out.text();
    assert_eq!(
        content.len(),
        MAX_BODY,
        "an over-cap body is truncated to exactly the 1 MiB cap, not returned whole"
    );
    assert!(
        content.bytes().all(|b| b == b'a'),
        "the capped prefix is the served body"
    );
}
