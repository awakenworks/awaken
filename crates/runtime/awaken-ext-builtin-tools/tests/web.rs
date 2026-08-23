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
    // Cause/effect rule R1: given a reachable URL serving a body below the raw
    // 1 MiB ceiling (C1), invoking the sole static `web_fetch` owner returns the
    // complete text with `is_error=false` (E1). Constraint K1: this test owns
    // raw HTTP transport only; Agent domain/context policy belongs to
    // `ConfiguredWebToolExecutor` and is not repeated here.
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
    // Cause/effect rule R2: given an unreachable endpoint (C2), raw HTTP
    // execution returns a typed fetch error rather than panicking (E2).
    // Constraint K1: no Agent policy is configured on this transport owner.
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
async fn web_fetch_caps_the_body_at_one_mebibyte() {
    // Cause/effect rule R3: given a reachable body larger than `MAX_BODY` (C3),
    // the raw transport reads exactly the 1 MiB prefix (E3), preventing a
    // hostile server from expanding the transcript. Constraint K1: this fixed
    // safety ceiling is independent of the Agent context cap owned by the
    // placement-neutral wrapper.
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
