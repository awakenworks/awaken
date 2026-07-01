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
    build_router,
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

/// A parent interrupt during a remote delegation cancels the remote task and the
/// delegation ends as a tool error — mirrors goal's `RemoteAbort` / tasks:cancel.
#[tokio::test]
async fn a_parent_interrupt_cancels_the_remote_task() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    const WORKING: &str =
        r#"{"task":{"id":"task-1","contextId":"c","status":{"state":"TASK_STATE_WORKING"}}}"#;

    /// A remote whose task never completes; it records a `tasks:cancel` and signals
    /// each poll so the test can interrupt mid-flight.
    struct HangingTransport {
        polled: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }
    #[async_trait::async_trait]
    impl A2aTransport for HangingTransport {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<A2aResponse, String> {
            if path.ends_with(":cancel") {
                self.cancelled.store(true, Ordering::SeqCst);
                return Ok(A2aResponse {
                    status: 200,
                    body: b"{}".to_vec(),
                });
            }
            if method == "GET" {
                self.polled.notify_one();
            }
            Ok(A2aResponse {
                status: 200,
                body: WORKING.as_bytes().to_vec(),
            })
        }
    }

    let polled = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let transport = Arc::new(HangingTransport {
        polled: polled.clone(),
        cancelled: cancelled.clone(),
    });
    let host = Arc::new(
        SharedHost::new(Arc::new(DelegatingModel), "parent")
            .with_remote_a2a("researcher", transport),
    );

    let driver = host.clone();
    let task =
        tokio::spawn(async move { driver.run_turn("t", vec![user("u1", "research")]).await });

    // Once the remote task has been polled, interrupt the parent thread.
    polled.notified().await;
    host.interrupt("t").await.unwrap();

    task.await
        .unwrap()
        .expect("the turn completes after cancel");
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the remote task received tasks:cancel"
    );
    let history = host.committed_messages("t").await;
    assert!(
        history
            .iter()
            .any(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:")),
        "the parent resumed with the cancellation as a tool result"
    );
}

/// A remote agent that asks for input parks the *parent* for the user (rather than
/// erroring): delivering input via `resume` forwards a follow-up `message:send`,
/// and the remote then completes. Mirrors goal/awaken-next `InputRequired` →
/// user-visible wait.
#[tokio::test]
async fn a_remote_input_required_parks_the_parent_then_resumes() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// First `message:send` → input-required; the second (the user's delivered
    /// input) → completed.
    struct TwoStepTransport {
        sends: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl A2aTransport for TwoStepTransport {
        async fn request(
            &self,
            method: &str,
            _path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<A2aResponse, String> {
            assert_eq!(method, "POST", "only message:send is used (no polling)");
            let json = if self.sends.fetch_add(1, Ordering::SeqCst) == 0 {
                r#"{"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_INPUT_REQUIRED"}}}"#
            } else {
                r#"{"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"a","role":"ROLE_AGENT","parts":[{"text":"final answer"}]}}}}"#
            };
            Ok(A2aResponse {
                status: 200,
                body: json.as_bytes().to_vec(),
            })
        }
    }

    let host = SharedHost::new(Arc::new(DelegatingModel), "parent").with_remote_a2a(
        "researcher",
        Arc::new(TwoStepTransport {
            sends: AtomicUsize::new(0),
        }),
    );

    // The turn parks: the remote asked for input, so the parent waits for the user.
    let turn = host
        .run_turn("t", vec![user("u1", "research the answer")])
        .await
        .unwrap();
    let pending = turn
        .pending
        .expect("the parent parks awaiting remote input");
    assert!(
        pending.client_executed,
        "the park asks the user to supply input"
    );

    // The user supplies the input; it is forwarded and the remote completes.
    host.resume(
        "t",
        &pending.tool_use_id,
        awaken_server_local::HostResume::ClientResult {
            content: "the missing detail".into(),
            is_error: false,
        },
    )
    .await
    .unwrap();

    let history = host.committed_messages("t").await;
    let reply = history
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:"))
        .map(text_of)
        .expect("the parent completes after the user delivers input");
    assert!(
        reply.contains("delegate said: final answer"),
        "the forwarded input let the remote complete: {reply:?}"
    );
}

/// A remote agent's A2A agent card is discoverable through the transport (outbound
/// discovery), so a coordinator can inspect a remote before delegating.
#[tokio::test]
async fn a_remote_agent_card_is_discoverable() {
    let remote = build_router(Arc::new(EchoModel), "remote");
    let transport = Arc::new(RouterTransport { app: remote });
    let host =
        SharedHost::new(Arc::new(EchoModel), "parent").with_remote_a2a("researcher", transport);

    let card = host.remote_agent_card("researcher").await.unwrap();
    assert!(!card.name.is_empty(), "the card advertises a name");
    assert!(
        !card.protocol_version.is_empty(),
        "the card advertises the A2A protocol version"
    );
}

/// A completed task's artifacts (A2A `TextAndArtifacts`) are folded into the
/// delegate reply, not dropped.
#[tokio::test]
async fn remote_artifacts_are_included_in_the_reply() {
    struct ArtifactTransport;
    #[async_trait::async_trait]
    impl A2aTransport for ArtifactTransport {
        async fn request(
            &self,
            _method: &str,
            _path: &str,
            _body: Option<Vec<u8>>,
        ) -> Result<A2aResponse, String> {
            let json = r#"{"task":{"id":"t","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"a","role":"ROLE_AGENT","parts":[{"text":"summary"}]}},"artifacts":[{"parts":[{"text":"the report body"}]}]}}"#;
            Ok(A2aResponse {
                status: 200,
                body: json.as_bytes().to_vec(),
            })
        }
    }

    let host = SharedHost::new(Arc::new(DelegatingModel), "parent")
        .with_remote_a2a("researcher", Arc::new(ArtifactTransport));

    host.run_turn("t", vec![user("u1", "research the answer")])
        .await
        .unwrap();

    let history = host.committed_messages("t").await;
    let reply = history
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant) && text_of(m).contains("delegate said:"))
        .map(text_of)
        .expect("the parent commits a reply from the completed task");
    assert!(
        reply.contains("summary") && reply.contains("the report body"),
        "the task message and its artifact both reach the parent: {reply:?}"
    );
}
