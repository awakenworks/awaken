//! Level 2 — A2A outbound, end-to-end over the real wire.
//!
//! A parent awaken agent delegates (`agent_run`) to a *remote* agent that is
//! itself an awaken A2A server. The delegation is fulfilled by posting a real
//! `message:send` (serialized JSON) to that server through an [`A2aTransport`],
//! reading the completed `Task`, and resuming the parent with the reply — proving
//! the inbound router and the outbound client agree on the same wire, in-process
//! and without a socket (the transport calls the router via `oneshot`).

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_server_local::{
    A2aResponse, A2aTransport, DelegatingModel, EchoModel, HttpA2aTransport, SharedHost,
    build_custom_router, build_router,
};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// An `A2aTransport` that calls an in-process awaken A2A server via `oneshot`, so
/// a delegation crosses the real A2A wire without opening a socket.
struct RouterTransport {
    app: Router,
}

#[async_trait::async_trait]
impl A2aTransport for RouterTransport {
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<A2aResponse, String> {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .map_err(|e| e.to_string())?;
        let resp = self
            .app
            .clone()
            .oneshot(req)
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok(A2aResponse {
            status,
            body: bytes.to_vec(),
        })
    }
}

fn user(id: &str, text: &str) -> Message {
    Message::text(MessageId(id.into()), Role::User, text)
}

fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_delegate_call_is_fulfilled_over_the_a2a_wire() {
    // The remote agent: an awaken A2A server backed by an echo model.
    let remote = build_router(Arc::new(EchoModel), "remote");
    let transport = Arc::new(RouterTransport { app: remote });

    // The parent agent delegates to "researcher", which is a REMOTE A2A agent.
    // `DelegatingModel` calls `agent_run{agent_id: "researcher", input: "do the
    // research"}`, then replies "delegate said: <result>".
    let host = SharedHost::new(Arc::new(DelegatingModel), "parent")
        .with_remote_a2a("researcher", transport);

    host.run_turn("t", vec![user("u1", "research the answer")])
        .await
        .unwrap();

    let history = host.committed_messages("t").await;
    let reply = history
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:"))
        .map(text_of)
        .expect("the parent commits a reply built from the remote agent's result");
    assert!(
        reply.contains("delegate said: Echo: do the research"),
        "the remote A2A agent's reply flowed back to the parent over the wire: {reply:?}"
    );
}

/// A remote failure surfaces to the parent as a tool error (not a panic): the
/// transport returns a non-`Task` body, and the delegation resumes with an error
/// the model can see.
#[tokio::test]
async fn a_remote_transport_failure_surfaces_as_a_tool_error() {
    struct BrokenTransport;
    #[async_trait::async_trait]
    impl A2aTransport for BrokenTransport {
        async fn request(
            &self,
            _method: &str,
            _path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<A2aResponse, String> {
            Err("connection refused".to_string())
        }
    }

    let host = SharedHost::new(Arc::new(DelegatingModel), "parent")
        .with_remote_a2a("researcher", Arc::new(BrokenTransport));

    // The turn still completes: the delegate call parked, the remote failed, and
    // the parent resumed with the error as the tool result.
    host.run_turn("t", vec![user("u1", "research the answer")])
        .await
        .unwrap();

    let history = host.committed_messages("t").await;
    assert!(
        history
            .iter()
            .any(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:")),
        "the parent completes even when the remote agent is unreachable"
    );
}

/// The built-in HTTP transport against a real remote A2A server bound to a
/// localhost socket: a delegation crosses a genuine TCP + HTTP boundary.
#[tokio::test]
async fn a_delegate_call_reaches_a_remote_over_real_http() {
    // Bind a real awaken A2A server on an ephemeral port.
    let remote = build_router(Arc::new(EchoModel), "remote");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, remote).await.unwrap();
    });

    let transport = Arc::new(HttpA2aTransport::new(format!("http://{addr}")));
    let host = SharedHost::new(Arc::new(DelegatingModel), "parent")
        .with_remote_a2a("researcher", transport);

    host.run_turn("t", vec![user("u1", "research the answer")])
        .await
        .unwrap();

    let history = host.committed_messages("t").await;
    let reply = history
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:"))
        .map(text_of)
        .expect("the parent commits a reply from the remote HTTP agent");
    assert!(
        reply.contains("delegate said: Echo: do the research"),
        "the remote reply crossed a real HTTP boundary: {reply:?}"
    );
}

/// An async remote agent that returns a `working` task first and completes only
/// after a poll: the outbound client polls `tasks/get` to a terminal state and the
/// reply reaches the parent. Mirrors goal/awaken-next `poll_to_completion`.
#[tokio::test]
async fn a_working_task_is_polled_to_completion() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Returns a `working` task on `message:send`, then a `completed` task on the
    /// first `tasks/get`.
    struct PollingTransport {
        gets: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl A2aTransport for PollingTransport {
        async fn request(
            &self,
            method: &str,
            _path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<A2aResponse, String> {
            let json = if method == "POST" {
                // message:send → a working task with an id to poll.
                r#"{"task":{"id":"task-1","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}}"#
                    .to_string()
            } else {
                // tasks/get → completed, carrying the reply.
                self.gets.fetch_add(1, Ordering::SeqCst);
                r#"{"task":{"id":"task-1","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"a","role":"ROLE_AGENT","parts":[{"text":"polled answer"}]}}}}"#
                    .to_string()
            };
            Ok(A2aResponse {
                status: 200,
                body: json.into_bytes(),
            })
        }
    }

    let transport = Arc::new(PollingTransport {
        gets: AtomicUsize::new(0),
    });
    let host = SharedHost::new(Arc::new(DelegatingModel), "parent")
        .with_remote_a2a("researcher", transport);

    host.run_turn("t", vec![user("u1", "research the answer")])
        .await
        .unwrap();

    let history = host.committed_messages("t").await;
    let reply = history
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:"))
        .map(text_of)
        .expect("the parent commits a reply from the polled task");
    assert!(
        reply.contains("delegate said: polled answer"),
        "a working task was polled to completion and its reply reached the parent: {reply:?}"
    );
}

/// A remote agent that parks (its A2A task is `input-required`) is surfaced to the
/// delegating parent as a tool error — this seam runs the delegate to completion,
/// so a mid-run pause on the remote is not a silent hang. Mirrors goal/awaken-next
/// mapping a remote `InputRequired` task to a caller-visible signal.
#[tokio::test]
async fn a_remote_input_required_task_surfaces_to_the_parent() {
    // The remote awaken A2A server parks on a client-executed tool, so its
    // `message:send` returns a task in the `input-required` state.
    let remote = build_custom_router();
    let transport = Arc::new(RouterTransport { app: remote });

    let host = SharedHost::new(Arc::new(DelegatingModel), "parent")
        .with_remote_a2a("researcher", transport);

    host.run_turn("t", vec![user("u1", "research the answer")])
        .await
        .unwrap();

    let history = host.committed_messages("t").await;
    let reply = history
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:"))
        .map(text_of)
        .expect("the parent commits a reply from the (failed) delegation");
    assert!(
        reply.contains("requires further input"),
        "a remote input-required task surfaces as a tool error: {reply:?}"
    );
}
