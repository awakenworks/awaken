//! End-to-end OAuth refresh over the HTTP transport, all on localhost:
//!
//! expired bearer → MCP server answers 401 + `WWW-Authenticate` → the host's
//! [`CredentialRefresher`] performs a real RFC 6749 `refresh_token` grant
//! against a mock token endpoint → the transport retries with the fresh token
//! and the call succeeds.
//!
//! This exercises the exact seam the managed-agents vault design cuts: the
//! transport owns challenge detection and retry; the refresher owns the OAuth
//! mechanics. Both sides run for real here — only the two servers are mocks.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_ext_mcp::{
    AuthChallenge, Credential, CredentialRefresher, HttpTransportBuilder, McpToolTransport,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Read one HTTP request: headers, then the `Content-Length` body.
async fn read_request(socket: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            return String::from_utf8_lossy(&request).to_string();
        }
        request.extend_from_slice(&buf[..n]);
        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&request).to_string()
}

async fn respond(socket: &mut TcpStream, status_line: &str, extra_headers: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status_line}\r\n{extra_headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
    socket.shutdown().await.ok();
}

/// A host-side refresher doing the real refresh_token grant over HTTP.
struct OAuthRefresher {
    token_endpoint: String,
    refresh_token: String,
    client_id: String,
    seen: Mutex<Vec<AuthChallenge>>,
}

#[async_trait]
impl CredentialRefresher for OAuthRefresher {
    async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential> {
        self.seen.lock().unwrap().push(challenge.clone());
        let response = reqwest::Client::new()
            .post(&self.token_endpoint)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
            ])
            .send()
            .await
            .ok()?;
        let body: serde_json::Value = response.json().await.ok()?;
        Some(Credential::Bearer(
            body.get("access_token")?.as_str()?.to_string(),
        ))
    }
}

#[tokio::test]
async fn expired_token_is_refreshed_via_the_token_endpoint_and_retried() {
    // Mock authorization server: one refresh_token grant → a fresh token.
    let token_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_endpoint = format!(
        "http://{}/oauth/token",
        token_listener.local_addr().unwrap()
    );
    let token_server = tokio::spawn(async move {
        let (mut socket, _) = token_listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        respond(
            &mut socket,
            "200 OK",
            "",
            r#"{"access_token":"fresh-token","token_type":"Bearer","expires_in":3600}"#, // awaken-allow: secret
        )
        .await;
        request
    });

    // Mock MCP server: rejects the expired bearer with a challenge, accepts the
    // fresh one.
    let mcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_url = format!("http://{}", mcp_listener.local_addr().unwrap());
    let mcp_server = tokio::spawn(async move {
        let mut captured = Vec::new();
        loop {
            let (mut socket, _) = mcp_listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            let authorized = request
                .to_ascii_lowercase()
                .contains("authorization: bearer fresh-token");
            captured.push(request);
            if authorized {
                respond(
                    &mut socket,
                    "200 OK",
                    "",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
                )
                .await;
                return captured;
            }
            respond(
                &mut socket,
                "401 Unauthorized",
                "WWW-Authenticate: Bearer resource_metadata=\"https://mcp.example/.well-known/oauth-protected-resource\"\r\n",
                "",
            )
            .await;
        }
    });

    let refresher = Arc::new(OAuthRefresher {
        token_endpoint,
        refresh_token: "rt-secret".to_string(), // awaken-allow: secret
        client_id: "client-abc".to_string(),
        seen: Mutex::new(Vec::new()),
    });
    let transport = HttpTransportBuilder::new(mcp_url)
        .credential(Credential::Bearer("expired-token".to_string()))
        .refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>)
        .build();

    // The call succeeds despite starting with an expired token.
    let tools = transport
        .list_tools()
        .await
        .expect("refresh + retry succeeds");
    assert!(tools.is_empty());

    // The refresher received the RFC 9728 discovery pointer from the challenge.
    {
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one refresh for one challenge");
        assert_eq!(seen[0].status, 401);
        assert!(
            seen[0]
                .www_authenticate
                .as_deref()
                .unwrap()
                .contains("resource_metadata"),
        );
    }

    // The token endpoint saw a well-formed refresh_token grant.
    let grant = token_server.await.unwrap();
    assert!(grant.contains("grant_type=refresh_token"), "{grant}");
    assert!(grant.contains("refresh_token=rt-secret"), "{grant}");
    assert!(grant.contains("client_id=client-abc"), "{grant}");

    // The MCP server saw the expired token first, then the fresh one.
    let captured = mcp_server.await.unwrap();
    assert_eq!(captured.len(), 2);
    assert!(
        captured[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer expired-token")
    );
    assert!(
        captured[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer fresh-token")
    );
}

/// A refresher whose token endpoint is unreachable: the challenge must surface
/// as an error instead of hanging or retrying forever.
#[tokio::test]
async fn unreachable_token_endpoint_surfaces_the_challenge() {
    let mcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp_url = format!("http://{}", mcp_listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut socket, _) = mcp_listener.accept().await.unwrap();
        read_request(&mut socket).await;
        respond(
            &mut socket,
            "401 Unauthorized",
            "WWW-Authenticate: Bearer realm=\"mcp\"\r\n",
            "",
        )
        .await;
    });

    let refresher = Arc::new(OAuthRefresher {
        // A closed port: the grant fails, refresh returns None.
        token_endpoint: "http://127.0.0.1:1/oauth/token".to_string(),
        refresh_token: "rt".to_string(),
        client_id: "c".to_string(),
        seen: Mutex::new(Vec::new()),
    });
    let transport = HttpTransportBuilder::new(mcp_url)
        .credential(Credential::Bearer("expired".to_string()))
        .refresher(refresher as Arc<dyn CredentialRefresher>)
        .build();

    let err = transport.list_tools().await.expect_err("401 surfaces");
    let message = err.to_string();
    assert!(message.contains("auth challenge: HTTP 401"), "{message}");
    assert!(message.contains("Bearer realm=\"mcp\""), "{message}");
}

struct StaticRefresher {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl CredentialRefresher for StaticRefresher {
    async fn refresh(&self, _challenge: &AuthChallenge) -> Option<Credential> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(Credential::Bearer("fresh-but-not-replayed".to_string()))
    }
}

#[tokio::test]
async fn tools_call_rotates_after_auth_challenge_but_is_never_replayed() {
    // Cause/effect graph: C1=effectful tools/call; C2=upstream returns 401;
    // C3=refresh succeeds. Effects E1=credential rotates for a later request;
    // E2=the current call surfaces the challenge; E3=upstream receives exactly
    // one call. Decision rule MR2=C1+C2+C3=>E1+E2+E3. FMECA: blindly treating
    // 401 as proof of no effect could duplicate an external mutation.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await;
        respond(
            &mut socket,
            "401 Unauthorized",
            "WWW-Authenticate: Bearer realm=\"mcp\"\r\n",
            "",
        )
        .await;
        let replayed =
            tokio::time::timeout(std::time::Duration::from_millis(150), listener.accept())
                .await
                .is_ok();
        (request, replayed)
    });
    let refresher = Arc::new(StaticRefresher {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let transport = HttpTransportBuilder::new(url)
        .credential(Credential::Bearer("expired".to_string()))
        .refresher(refresher.clone() as Arc<dyn CredentialRefresher>)
        .build();

    let error = transport
        .call_tool("mutate", serde_json::json!({"value": 1}))
        .await
        .expect_err("MR2 challenge surfaces");
    assert!(
        error.to_string().contains("auth challenge: HTTP 401"),
        "MR2/E2"
    );
    let (request, replayed) = server.await.unwrap();
    assert!(request.contains("tools/call"), "MR2/C1");
    assert!(!replayed, "MR2/E3");
    assert_eq!(
        refresher.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "MR2/E1"
    );
}
