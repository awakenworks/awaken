//! WebFetch is resolved through the same configurable provider catalog as
//! WebSearch; no static network execution path remains.

use awaken_ext_builtin_tools::{
    WebDomainFilter, WebFetchExecutionConfiguration, WebFetchPlugin,
    WebSearchExecutionConfiguration, WebSearchPlugin, WebSearchProviderRegistry,
    WebSearchUserLocation,
};
use awaken_runtime_contract::tool::{RawTool, ToolCall};
use std::io::{Read, Write};
use std::sync::Arc;

fn tool(id: &str) -> Arc<dyn RawTool> {
    let plugin = WebFetchPlugin::new(WebSearchProviderRegistry::builtins(), None);
    let (_, tool) = plugin.configured_tool(None).expect("default fetch route");
    assert_eq!(tool.id(), id);
    tool
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
    // Cause/effect rule R1: direct provider plus a reachable body below the raw
    // 1 MiB ceiling returns the complete text with `is_error=false`. This test
    // owns transport only; Agent domain/context policy remains in the configured plugin.
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
async fn domain_filtered_web_fetch_rejects_redirect_before_second_request() {
    // Redirect cause/effect decision table: C1 Agent domain policy is active;
    // C2 the initial `127.0.0.1` URL matches it; C3 the server redirects to the
    // excluded `localhost` host. R4 C1+C2+C3 -> E1 exactly one network request,
    // E2 a typed policy error, E3 no redirected I/O. Constraint K2: the
    // configured plugin remains the sole initial-URL policy owner, while the
    // direct transport fails closed instead of following an unvalidated hop.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let redirected_url = format!("http://localhost:{}/blocked", addr.port());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("initial accept");
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: {redirected_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("redirect response");
        drop(stream);

        listener.set_nonblocking(true).expect("nonblocking");
        for _ in 0..100 {
            match listener.accept() {
                Ok((mut redirected, _)) => {
                    let _ = redirected.read(&mut request);
                    redirected
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nredirected",
                        )
                        .expect("redirected response");
                    return 2;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => panic!("redirect accept: {error}"),
            }
        }
        1
    });

    let plugin = WebFetchPlugin::new(WebSearchProviderRegistry::builtins(), None)
        .with_execution_configuration(Some(WebFetchExecutionConfiguration {
            domains: Some(WebDomainFilter::Allow(vec!["127.0.0.1".into()])),
            max_content_tokens: None,
        }));
    let (_, filtered_tool) = plugin.configured_tool(None).expect("direct route");
    let error = filtered_tool
        .invoke(call(
            "web_fetch",
            serde_json::json!({ "url": format!("http://{addr}/allowed") }),
        ))
        .await
        .expect_err("redirect must fail closed");
    assert!(
        error
            .to_string()
            .contains("redirect is disabled while domain policy is active"),
        "R4/E2"
    );
    assert_eq!(server.join().expect("server thread"), 1, "R4/E1/E3");
}

#[tokio::test]
async fn web_fetch_unreachable_host_is_a_typed_error() {
    // Cause/effect rule R2: direct provider + connection failure -> typed error;
    // no undeclared fallback or alternate provider is guessed, and the call
    // fails without panicking. No Agent policy is configured on this transport owner.
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
    // configured plugin.
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

#[test]
fn provider_server_web_tools_reject_agent_policy_they_cannot_enforce() {
    // Cause/effect decision table: C1 realization=provider-server; C2 Agent
    // execution policy=absent/empty/restrictive; C3 tool=Fetch/Search. E1 an
    // absent or empty policy preserves the provider-server realization; E2 a
    // domain, content, or location restriction fails during configuration,
    // before inference. Provider-server calls never pass through the host RawTool,
    // so silently accepting C2 would create a second, weaker policy path.
    //
    // | Rule | tool | policy | Effect |
    // | P1 | Fetch/Search | absent or empty | E1 configured |
    // | P2 | Fetch | domains | E2 rejected |
    // | P3 | Fetch | content cap | E2 rejected |
    // | P4 | Search | domains | E2 rejected |
    // | P5 | Search | location | E2 rejected |
    let config = serde_json::json!({ "provider_id": "openrouter", "options": {} });
    let registry = WebSearchProviderRegistry::server_builtins();
    assert!(
        WebFetchPlugin::new(registry.clone(), None)
            .validate_config(Some(&config))
            .is_ok(),
        "P1 absent"
    );
    assert!(
        WebFetchPlugin::new(registry.clone(), None)
            .with_execution_configuration(Some(WebFetchExecutionConfiguration::default()))
            .validate_config(Some(&config))
            .is_ok(),
        "P1 empty"
    );
    assert!(
        WebSearchPlugin::new(registry.clone(), None)
            .validate_config(Some(&config))
            .is_ok(),
        "P1 search absent"
    );
    assert!(
        WebSearchPlugin::new(registry.clone(), None)
            .with_execution_configuration(Some(WebSearchExecutionConfiguration::default()))
            .validate_config(Some(&config))
            .is_ok(),
        "P1 search empty"
    );
    for (rule, policy) in [
        (
            "P2",
            WebFetchExecutionConfiguration {
                domains: Some(WebDomainFilter::Allow(vec!["docs.example.com".into()])),
                max_content_tokens: None,
            },
        ),
        (
            "P3",
            WebFetchExecutionConfiguration {
                domains: None,
                max_content_tokens: Some(1024),
            },
        ),
    ] {
        assert!(
            WebFetchPlugin::new(registry.clone(), None)
                .with_execution_configuration(Some(policy))
                .validate_config(Some(&config))
                .is_err(),
            "{rule}"
        );
    }
    assert!(
        WebSearchPlugin::new(registry.clone(), None)
            .with_execution_configuration(Some(WebSearchExecutionConfiguration {
                domains: Some(WebDomainFilter::Block(vec!["example.net".into()])),
                user_location: None,
            }))
            .validate_config(Some(&config))
            .is_err(),
        "P4"
    );
    assert!(
        WebSearchPlugin::new(registry, None)
            .with_execution_configuration(Some(WebSearchExecutionConfiguration {
                domains: None,
                user_location: Some(WebSearchUserLocation {
                    city: None,
                    country: Some("US".into()),
                    region: None,
                    timezone: None,
                }),
            }))
            .validate_config(Some(&config))
            .is_err(),
        "P5"
    );
}
