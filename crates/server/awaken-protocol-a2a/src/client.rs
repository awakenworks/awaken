//! The A2A outbound client: call a *remote* A2A agent.
//!
//! The mirror of the inbound [`router`](crate::router): the same wire types and the
//! same `/v1/a2a/...` paths, in the same bounded context. A caller supplies a
//! [`Transport`] (HTTP, or in-process for tests) and gets neutral `Task` /
//! `AgentCard` results — all A2A protocol knowledge (routes, message shape, the
//! error envelope) stays here. Polling a `working` task to a terminal state and
//! mapping its state to a domain outcome are the *caller's* orchestration, not the
//! wire's.
//!
//! Authentication follows the A2A spec's transport-layer model, with the shared
//! [`awaken_credential`] seam: a [`Credential`] (bearer or header — the `http`
//! and `apiKey` card schemes) rides on every request, rotatable in place via
//! [`set_credential`](HttpTransport::set_credential); a 401/403 surfaces its
//! `WWW-Authenticate` as a structured [`AuthChallenge`], and a host-registered
//! [`CredentialRefresher`] is consulted once per failed request (the OAuth
//! flow that produces a fresh bearer runs host-side, behind that callback).

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use awaken_credential::{AuthChallenge, Credential, CredentialRefresher};
use serde_json::json;

#[cfg(test)]
use crate::types::TaskState;
use crate::types::{AgentCard, ErrorResponse, SendMessageResponse, Task};

/// The `message:send` route (shared with the inbound router).
pub const MESSAGE_SEND_PATH: &str = "/v1/a2a/message:send";
/// The agent-card route (shared with the inbound router).
pub const AGENT_CARD_PATH: &str = "/v1/a2a/agent-card";

/// The `tasks/get` route for a task.
pub fn task_path(task_id: &str) -> String {
    format!("/v1/a2a/tasks/{task_id}")
}

/// The `tasks:cancel` route for a task.
pub fn task_cancel_path(task_id: &str) -> String {
    format!("/v1/a2a/tasks/{task_id}:cancel")
}

/// A raw A2A HTTP+JSON response: the status code, the body bytes, and — on a
/// 401/403 — the server's `WWW-Authenticate` challenge.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// `WWW-Authenticate` when the server answered 401/403; `None` otherwise.
    pub www_authenticate: Option<String>,
}

impl Response {
    /// A response without an auth challenge (the common case; in-process test
    /// transports use this).
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            www_authenticate: None,
        }
    }
}

/// The transport seam for the outbound client: performs an A2A HTTP+JSON request.
/// The composition root supplies the impl (HTTP with credentials, or an in-process
/// router for tests) — the client never names the wire mechanism. A non-2xx status
/// is returned (not raised) so the A2A error envelope reaches the caller; the
/// `Err` string is reserved for requests that never completed (connect/IO).
#[async_trait]
pub trait Transport: Send + Sync {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, String>;
}

/// A client-side A2A failure, structured by what the caller can do about it.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server rejected our credential (401/403). The challenge carries the
    /// `WWW-Authenticate` contents — for OAuth servers, the discovery pointer
    /// the host needs to (re)authorize.
    #[error("A2A {what} unauthorized: {challenge}")]
    Unauthorized {
        what: &'static str,
        challenge: AuthChallenge,
    },
    /// Any other non-2xx reply; `message` is the A2A error envelope's message
    /// when the body carries one, else a body excerpt.
    #[error("A2A {what} failed: HTTP {status}: {message}")]
    Http {
        what: &'static str,
        status: u16,
        message: String,
    },
    /// The request never completed (connect / IO / task failure).
    #[error("A2A transport error: {0}")]
    Transport(String),
    /// A 2xx body that does not decode as the expected A2A shape.
    #[error("A2A decode error: {0}")]
    Decode(String),
}

fn ok_status(response: &Response, what: &'static str) -> Result<(), ClientError> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    if response.status == 401 || response.status == 403 {
        return Err(ClientError::Unauthorized {
            what,
            challenge: AuthChallenge {
                status: response.status,
                www_authenticate: response.www_authenticate.clone(),
            },
        });
    }
    Err(ClientError::Http {
        what,
        status: response.status,
        message: envelope_message(&response.body),
    })
}

/// The A2A error envelope's message when the body carries one, else a bounded
/// excerpt of the raw body (so a proxy's HTML error page cannot flood a log).
fn envelope_message(body: &[u8]) -> String {
    if let Ok(envelope) = serde_json::from_slice::<ErrorResponse>(body) {
        return envelope.error.message;
    }
    let text = String::from_utf8_lossy(body);
    let mut excerpt: String = text.chars().take(200).collect();
    if text.chars().count() > 200 {
        excerpt.push('…');
    }
    excerpt
}

/// Read a `Task` from a response, accepting either a bare task or a
/// `{ "task": ... }` envelope (`message:send` uses the envelope).
fn read_task(body: &[u8]) -> Result<Task, ClientError> {
    if let Ok(response) = serde_json::from_slice::<SendMessageResponse>(body) {
        return Ok(response.task);
    }
    serde_json::from_slice::<Task>(body).map_err(|e| ClientError::Decode(e.to_string()))
}

/// Post a `message:send` and return the initial task. `context_id` continues a
/// prior conversation; `message_id` must be unique per message.
pub async fn send_message(
    transport: &dyn Transport,
    agent_id: Option<&str>,
    context_id: &str,
    message_id: &str,
    text: &str,
) -> Result<Task, ClientError> {
    let request = json!({
        "agentId": agent_id,
        "message": {
            "messageId": message_id,
            "contextId": context_id,
            "role": "ROLE_USER",
            "parts": [{ "text": text }],
        }
    });
    let body = serde_json::to_vec(&request).map_err(|e| ClientError::Decode(e.to_string()))?;
    let response = transport
        .request("POST", MESSAGE_SEND_PATH, Some(body))
        .await
        .map_err(ClientError::Transport)?;
    ok_status(&response, "message:send")?;
    read_task(&response.body)
}

/// Fetch a task by id (`tasks/get`) — to reattach to or poll an in-flight task.
pub async fn get_task(transport: &dyn Transport, task_id: &str) -> Result<Task, ClientError> {
    let response = transport
        .request("GET", &task_path(task_id), None)
        .await
        .map_err(ClientError::Transport)?;
    ok_status(&response, "tasks/get")?;
    read_task(&response.body)
}

/// Cancel a task (`tasks:cancel`) and surface delivery failure so a durable
/// caller can retain and retry its cancellation intent.
pub async fn try_cancel_task(transport: &dyn Transport, task_id: &str) -> Result<(), ClientError> {
    let response = transport
        .request("POST", &task_cancel_path(task_id), None)
        .await
        .map_err(ClientError::Transport)?;
    ok_status(&response, "tasks:cancel")
}

/// Best-effort compatibility wrapper for callers that have no durable retry
/// owner. Delegation uses [`try_cancel_task`] instead.
pub async fn cancel_task(transport: &dyn Transport, task_id: &str) {
    let _ = try_cancel_task(transport, task_id).await;
}

/// Fetch the remote agent's discovery card (`agent-card`).
pub async fn agent_card(transport: &dyn Transport) -> Result<AgentCard, ClientError> {
    let response = transport
        .request("GET", AGENT_CARD_PATH, None)
        .await
        .map_err(ClientError::Transport)?;
    ok_status(&response, "agent-card")?;
    serde_json::from_slice::<AgentCard>(&response.body)
        .map_err(|e| ClientError::Decode(e.to_string()))
}

/// A real HTTP [`Transport`] to a remote A2A agent. `ureq` is synchronous, so
/// each request runs on a blocking thread.
///
/// Carries a rotatable [`Credential`] plus custom headers on every request; on
/// a 401/403 an optional [`CredentialRefresher`] is consulted once and the
/// request retried with the fresh credential. Without a refresher (or when it
/// declines) the 401/403 response is returned as-is — the client functions map
/// it to [`ClientError::Unauthorized`] with the challenge attached.
pub struct HttpTransport {
    base_url: String,
    credential: RwLock<Credential>,
    /// Custom headers sent on every request, alongside the credential header.
    headers: Vec<(String, String)>,
    refresher: Option<Arc<dyn CredentialRefresher>>,
}

impl HttpTransport {
    /// A transport to the remote agent at `base_url` (e.g. `https://host`); A2A
    /// paths are appended to it.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            credential: RwLock::new(Credential::None),
            headers: Vec::new(),
            refresher: None,
        }
    }

    /// Authenticate every request with `Authorization: Bearer <token>`
    /// (shorthand for `with_credential(Credential::Bearer(..))`).
    pub fn with_bearer(self, token: impl Into<String>) -> Self {
        self.with_credential(Credential::Bearer(token.into()))
    }

    /// Authenticate with `credential` — a bearer or an arbitrary header (the
    /// card's `http`/`apiKey` header schemes).
    pub fn with_credential(self, credential: Credential) -> Self {
        *self.credential.write().unwrap() = credential;
        self
    }

    /// Add a custom header sent on every request (repeatable).
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Register the host hook consulted on 401/403.
    pub fn with_refresher(mut self, refresher: Arc<dyn CredentialRefresher>) -> Self {
        self.refresher = Some(refresher);
        self
    }

    /// Replace the credential used from the next request on — the host calls
    /// this when it rotates a token outside the 401 path.
    pub fn set_credential(&self, credential: Credential) {
        *self.credential.write().unwrap() = credential;
    }

    /// Custom headers + the current credential header.
    fn current_headers(&self) -> Vec<(String, String)> {
        let mut headers = self.headers.clone();
        if let Some(pair) = self.credential.read().unwrap().header() {
            headers.push(pair);
        }
        headers
    }

    /// One blocking HTTP attempt with the given headers.
    async fn attempt(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        headers: Vec<(String, String)>,
    ) -> Result<Response, String> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let method = method.to_string();
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut req = ureq::request(&method, &url);
            for (name, value) in &headers {
                req = req.set(name, value);
            }
            let result = match body {
                Some(bytes) => req
                    .set("content-type", "application/json")
                    .send_bytes(&bytes),
                None => req.call(),
            };
            let (status, response) = match result {
                Ok(response) => (response.status(), response),
                Err(ureq::Error::Status(code, response)) => (code, response),
                Err(err) => return Err(err.to_string()),
            };
            let www_authenticate = (status == 401 || status == 403)
                .then(|| response.header("www-authenticate").map(str::to_string))
                .flatten();
            let mut buffer = Vec::new();
            response
                .into_reader()
                .read_to_end(&mut buffer)
                .map_err(|e| e.to_string())?;
            Ok(Response {
                status,
                body: buffer,
                www_authenticate,
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response, String> {
        let response = self
            .attempt(method, path, body.clone(), self.current_headers())
            .await?;
        // Auth failure: consult the host refresher once and retry with the
        // fresh credential; otherwise return the response as-is so the client
        // layer surfaces the structured challenge.
        if response.status != 401 && response.status != 403 {
            return Ok(response);
        }
        let Some(refresher) = &self.refresher else {
            return Ok(response);
        };
        let challenge = AuthChallenge {
            status: response.status,
            www_authenticate: response.www_authenticate.clone(),
        };
        let Some(fresh) = refresher.refresh(&challenge).await else {
            return Ok(response);
        };
        *self.credential.write().unwrap() = fresh;
        self.attempt(method, path, body, self.current_headers())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records requests and replies with a queued body, so client wiring (method,
    /// path, parse) is asserted without a socket.
    struct MockTransport {
        seen: Mutex<Vec<(String, String)>>,
        reply: String,
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<Response, String> {
            self.seen
                .lock()
                .unwrap()
                .push((method.to_string(), path.to_string()));
            Ok(Response::new(200, self.reply.clone().into_bytes()))
        }
    }

    #[test]
    fn task_paths_are_built_from_the_id() {
        assert_eq!(task_path("t-1"), "/v1/a2a/tasks/t-1");
        assert_eq!(task_cancel_path("t-1"), "/v1/a2a/tasks/t-1:cancel");
    }

    #[tokio::test]
    async fn send_message_posts_and_parses_the_task() {
        let transport = MockTransport {
            seen: Mutex::new(Vec::new()),
            reply: r#"{"task":{"id":"t-1","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"a","role":"ROLE_AGENT","parts":[{"text":"hi"}]}}}}"#
                .into(),
        };
        let task = send_message(&transport, Some("agent"), "c", "m-1", "go")
            .await
            .unwrap();
        assert_eq!(task.id, "t-1");
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(transport.seen.lock().unwrap()[0].1, MESSAGE_SEND_PATH);
    }

    #[tokio::test]
    async fn get_task_uses_the_task_route() {
        let transport = MockTransport {
            seen: Mutex::new(Vec::new()),
            reply: r#"{"id":"t-9","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}"#.into(),
        };
        let task = get_task(&transport, "t-9").await.unwrap();
        assert_eq!(task.status.state, TaskState::Working);
        let seen = transport.seen.lock().unwrap();
        assert_eq!(
            seen[0],
            ("GET".to_string(), "/v1/a2a/tasks/t-9".to_string())
        );
    }

    #[tokio::test]
    async fn a_non_2xx_status_is_an_error() {
        struct Failing;
        #[async_trait]
        impl Transport for Failing {
            async fn request(
                &self,
                _: &str,
                _: &str,
                _: Option<Vec<u8>>,
            ) -> Result<Response, String> {
                Ok(Response::new(500, b"boom".to_vec()))
            }
        }
        assert!(get_task(&Failing, "t").await.is_err());
    }

    /// One canned response per status/body/challenge triple.
    struct CannedTransport {
        status: u16,
        body: &'static str,
        www_authenticate: Option<&'static str>,
    }

    #[async_trait]
    impl Transport for CannedTransport {
        async fn request(&self, _: &str, _: &str, _: Option<Vec<u8>>) -> Result<Response, String> {
            Ok(Response {
                status: self.status,
                body: self.body.as_bytes().to_vec(),
                www_authenticate: self.www_authenticate.map(str::to_string),
            })
        }
    }

    #[tokio::test]
    async fn a_401_maps_to_a_structured_challenge() {
        let transport = CannedTransport {
            status: 401,
            body: "",
            www_authenticate: Some("Bearer resource_metadata=\"https://x/.well-known\""),
        };
        let err = get_task(&transport, "t").await.expect_err("401 surfaces");
        let ClientError::Unauthorized { what, challenge } = err else {
            panic!("expected Unauthorized, got {err:?}");
        };
        assert_eq!(what, "tasks/get");
        assert_eq!(challenge.status, 401);
        assert!(
            challenge
                .www_authenticate
                .as_deref()
                .unwrap()
                .contains("resource_metadata")
        );
    }

    #[tokio::test]
    async fn a_403_maps_to_a_structured_challenge() {
        let transport = CannedTransport {
            status: 403,
            body: "",
            www_authenticate: Some("Bearer error=\"insufficient_scope\""),
        };
        let err = get_task(&transport, "t").await.expect_err("403 surfaces");
        let ClientError::Unauthorized { what, challenge } = err else {
            panic!("expected Unauthorized, got {err:?}");
        };
        assert_eq!(what, "tasks/get");
        assert_eq!(challenge.status, 403);
        assert!(
            challenge
                .www_authenticate
                .as_deref()
                .unwrap()
                .contains("insufficient_scope")
        );
    }

    #[tokio::test]
    async fn an_http_error_carries_the_envelope_message() {
        let transport = CannedTransport {
            status: 429,
            body: r#"{"error":{"code":429,"message":"slow down"}}"#,
            www_authenticate: None,
        };
        let err = get_task(&transport, "t").await.expect_err("429 surfaces");
        let ClientError::Http {
            status, message, ..
        } = err
        else {
            panic!("expected Http, got {err:?}");
        };
        assert_eq!(status, 429);
        assert_eq!(message, "slow down");
    }

    #[tokio::test]
    async fn a_non_envelope_error_body_is_excerpted() {
        let transport = CannedTransport {
            status: 502,
            body: "<html>proxy said no</html>",
            www_authenticate: None,
        };
        let err = get_task(&transport, "t").await.expect_err("502 surfaces");
        assert!(matches!(
            err,
            ClientError::Http { message, .. } if message.contains("proxy said no")
        ));
    }

    #[tokio::test]
    async fn an_undecodable_2xx_body_is_a_decode_error() {
        let transport = CannedTransport {
            status: 200,
            body: "not json",
            www_authenticate: None,
        };
        let err = get_task(&transport, "t").await.expect_err("bad body");
        assert!(matches!(err, ClientError::Decode(_)));
    }

    #[test]
    fn a_long_error_body_is_truncated_with_ellipsis() {
        let message = envelope_message("x".repeat(300).as_bytes());
        assert_eq!(message.chars().count(), 201, "200 chars plus the ellipsis");
        assert!(message.ends_with('…'), "{message}");
    }

    #[tokio::test]
    async fn agent_card_parses_the_card() {
        let transport = MockTransport {
            seen: Mutex::new(Vec::new()),
            reply: r#"{"name":"assistant","description":"Awaken agent","version":"0.0.0","protocolVersion":"1.0","capabilities":{"streaming":false,"pushNotifications":false}}"#
                .into(),
        };
        let card = agent_card(&transport).await.unwrap();
        assert_eq!(card.name, "assistant");
        assert_eq!(card.protocol_version, "1.0");
        let seen = transport.seen.lock().unwrap();
        assert_eq!(seen[0], ("GET".to_string(), AGENT_CARD_PATH.to_string()));
    }

    #[tokio::test]
    async fn agent_card_bad_body_is_decode_error() {
        let transport = CannedTransport {
            status: 200,
            body: "not a card",
            www_authenticate: None,
        };
        let err = agent_card(&transport).await.expect_err("bad body");
        assert!(matches!(err, ClientError::Decode(_)));
    }

    #[tokio::test]
    async fn cancel_task_ignores_failures() {
        // A 500 reply and a request that never completes both go unreported —
        // cancel is best-effort.
        let failing_status = CannedTransport {
            status: 500,
            body: "boom",
            www_authenticate: None,
        };
        cancel_task(&failing_status, "t").await;

        struct Down;
        #[async_trait]
        impl Transport for Down {
            async fn request(
                &self,
                _: &str,
                _: &str,
                _: Option<Vec<u8>>,
            ) -> Result<Response, String> {
                Err("connection refused".to_string())
            }
        }
        cancel_task(&Down, "t").await;
    }

    #[tokio::test]
    async fn send_message_401_labels_message_send() {
        let transport = CannedTransport {
            status: 401,
            body: "",
            www_authenticate: None,
        };
        let err = send_message(&transport, None, "c", "m-1", "go")
            .await
            .expect_err("401 surfaces");
        assert!(
            matches!(
                err,
                ClientError::Unauthorized {
                    what: "message:send",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn agent_card_401_labels_agent_card() {
        let transport = CannedTransport {
            status: 401,
            body: "",
            www_authenticate: None,
        };
        let err = agent_card(&transport).await.expect_err("401 surfaces");
        assert!(
            matches!(
                err,
                ClientError::Unauthorized {
                    what: "agent-card",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    // ---- HttpTransport auth mechanics against a local single-shot server ----

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const COMPLETED_TASK: &str =
        r#"{"task":{"id":"t","contextId":"c","status":{"state":"completed"}}}"#;

    fn ok_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn unauthorized_response() -> String {
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"a2a\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string()
    }

    /// Serve one canned response per accepted connection; returns the base URL
    /// and a handle resolving to the raw request bytes each connection sent.
    async fn serve(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let mut captured = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                captured.push(String::from_utf8_lossy(&request).to_string());
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            }
            captured
        });
        (url, handle)
    }

    struct StaticRefresher {
        fresh: Option<Credential>,
        seen: Mutex<Vec<AuthChallenge>>,
    }

    #[async_trait]
    impl CredentialRefresher for StaticRefresher {
        async fn refresh(&self, challenge: &AuthChallenge) -> Option<Credential> {
            self.seen.lock().unwrap().push(challenge.clone());
            self.fresh.clone()
        }
    }

    #[tokio::test]
    async fn http_transport_sends_credential_and_custom_headers() {
        let (url, server) = serve(vec![ok_response(COMPLETED_TASK)]).await;
        let transport = HttpTransport::new(url)
            .with_credential(Credential::Header {
                name: "X-Api-Key".to_string(),
                value: "k1".to_string(),
            })
            .with_header("X-Org-Id", "org-42");
        get_task(&transport, "t").await.expect("succeeds");
        let request = server.await.unwrap()[0].to_ascii_lowercase();
        assert!(request.contains("x-api-key: k1"), "{request}");
        assert!(request.contains("x-org-id: org-42"), "{request}");
    }

    #[tokio::test]
    async fn http_transport_refreshes_and_retries_once_on_401() {
        let (url, server) = serve(vec![unauthorized_response(), ok_response(COMPLETED_TASK)]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: Some(Credential::Bearer("fresh".to_string())),
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransport::new(url)
            .with_bearer("stale")
            .with_refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>);
        get_task(&transport, "t").await.expect("retry succeeds");

        let captured = server.await.unwrap();
        assert!(captured[0].to_ascii_lowercase().contains("bearer stale"));
        assert!(captured[1].to_ascii_lowercase().contains("bearer fresh"));
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one refresh for one challenge");
        assert_eq!(
            seen[0].www_authenticate.as_deref(),
            Some("Bearer realm=\"a2a\"")
        );
    }

    #[tokio::test]
    async fn http_transport_without_refresher_surfaces_the_challenge() {
        let (url, _server) = serve(vec![unauthorized_response()]).await;
        let transport = HttpTransport::new(url).with_bearer("stale");
        let err = get_task(&transport, "t").await.expect_err("401 surfaces");
        assert!(matches!(
            err,
            ClientError::Unauthorized { challenge, .. }
                if challenge.www_authenticate.as_deref() == Some("Bearer realm=\"a2a\"")
        ));
    }

    #[tokio::test]
    async fn http_transport_refresher_declines_surfaces_challenge() {
        let (url, _server) = serve(vec![unauthorized_response()]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: None,
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransport::new(url)
            .with_bearer("stale")
            .with_refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>);
        let err = get_task(&transport, "t").await.expect_err("401 surfaces");
        assert!(matches!(
            err,
            ClientError::Unauthorized { challenge, .. }
                if challenge.www_authenticate.as_deref() == Some("Bearer realm=\"a2a\"")
        ));
        assert_eq!(refresher.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn http_transport_retry_still_401_surfaces() {
        let second_401 =
            "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"second\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string();
        let (url, server) = serve(vec![unauthorized_response(), second_401]).await;
        let refresher = Arc::new(StaticRefresher {
            fresh: Some(Credential::Bearer("fresh".to_string())),
            seen: Mutex::new(Vec::new()),
        });
        let transport = HttpTransport::new(url)
            .with_bearer("stale")
            .with_refresher(Arc::clone(&refresher) as Arc<dyn CredentialRefresher>);
        let err = get_task(&transport, "t")
            .await
            .expect_err("retry 401 surfaces");
        // The retry's own challenge surfaces, not the first one's.
        assert!(matches!(
            err,
            ClientError::Unauthorized { challenge, .. }
                if challenge.www_authenticate.as_deref() == Some("Bearer realm=\"second\"")
        ));
        let captured = server.await.unwrap();
        assert!(captured[1].to_ascii_lowercase().contains("bearer fresh"));
        let seen = refresher.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one refresh per request");
    }

    #[tokio::test]
    async fn http_transport_set_credential_rotates_the_header() {
        let (url, server) = serve(vec![
            ok_response(COMPLETED_TASK),
            ok_response(COMPLETED_TASK),
        ])
        .await;
        let transport = HttpTransport::new(url).with_bearer("first");
        get_task(&transport, "t").await.expect("first call");
        transport.set_credential(Credential::Bearer("second".to_string()));
        get_task(&transport, "t").await.expect("second call");
        let captured = server.await.unwrap();
        assert!(captured[0].to_ascii_lowercase().contains("bearer first"));
        assert!(captured[1].to_ascii_lowercase().contains("bearer second"));
    }

    #[tokio::test]
    async fn a_connect_failure_is_a_transport_error() {
        // A closed port: the request never completes.
        let transport = HttpTransport::new("http://127.0.0.1:1");
        let err = get_task(&transport, "t").await.expect_err("no listener");
        assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
    }
}
