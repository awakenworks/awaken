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
use awaken_server_local::{A2aTransport, DelegatingModel, EchoModel, SharedHost, build_router};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

/// An `A2aTransport` that posts to an in-process awaken A2A server via `oneshot`,
/// so a delegation crosses the real A2A wire without opening a socket.
struct RouterTransport {
    app: Router,
}

#[async_trait::async_trait]
impl A2aTransport for RouterTransport {
    async fn message_send(&self, body: Vec<u8>) -> Result<Vec<u8>, String> {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/a2a/message:send")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .map_err(|e| e.to_string())?;
        let resp = self
            .app
            .clone()
            .oneshot(req)
            .await
            .map_err(|e| e.to_string())?;
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok(bytes.to_vec())
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
        async fn message_send(&self, _body: Vec<u8>) -> Result<Vec<u8>, String> {
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
