//! `A2aRunExecutor`: a remote A2A agent (Coze / any A2A HTTP endpoint) driven as a
//! peer [`RunExecutor`]. Like the ACP executor it is *a second implementation* of
//! the one execution port — no local process, no model loop of our own: it dials the
//! endpoint on `Backend::Remote { endpoint }`, sends the run's prompt as an A2A
//! `message:send`, and commits the returned task's reply through the same commit
//! boundary as the native and ACP paths.
//!
//! Boundaries: runtime plane; depends only on the foundation contracts + the A2A
//! protocol crate. The dial endpoint comes from the resolved backend, so this
//! executor stays config-free; a [`TransportFactory`] is the one injected seam
//! (an `HttpTransport` in production, a mock in tests).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Phase};
use awaken_protocol_a2a::client::send_message;
use awaken_protocol_a2a::{HttpTransport, Task, TaskState, Transport};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{
    Cancellation, Error, ExecutorCapabilities, Result, RunExecutor, Wait,
};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

/// Builds a [`Transport`] for a dial endpoint. Injectable so a test can substitute a
/// mock for the `HttpTransport`.
pub type TransportFactory = Arc<dyn Fn(&str) -> Arc<dyn Transport> + Send + Sync>;

/// Drives a remote A2A agent as a [`RunExecutor`].
pub struct A2aRunExecutor {
    transport_for: TransportFactory,
}

impl A2aRunExecutor {
    #[must_use]
    pub fn new(transport_for: TransportFactory) -> Self {
        Self { transport_for }
    }

    /// The production executor: dials each `Backend::Remote { endpoint }` over HTTP.
    #[must_use]
    pub fn over_http() -> Self {
        Self {
            transport_for: Arc::new(|url| Arc::new(HttpTransport::new(url)) as Arc<dyn Transport>),
        }
    }
}

/// The turn's prompt: the concatenated text of the activation's input.
fn prompt_of(input: &[Message]) -> String {
    input
        .iter()
        .map(Message::text_content)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The agent's reply from a returned task: its durable artifacts, else the terminal
/// status message, else the last history message.
fn task_reply(task: &Task) -> String {
    let artifacts: Vec<String> = task
        .artifacts
        .iter()
        .map(|a| a.text())
        .filter(|t| !t.is_empty())
        .collect();
    if !artifacts.is_empty() {
        return artifacts.join("\n");
    }
    if let Some(message) = &task.status.message {
        let text = message.text();
        if !text.is_empty() {
            return text;
        }
    }
    task.history.last().map(|m| m.text()).unwrap_or_default()
}

/// The run's terminus derived from the returned task's lifecycle state. Do not
/// collapse every returned task to a natural end: a `failed` remote task is an
/// execution fault and a `canceled` one is a cancellation, while a still-`working`
/// or `*-required` task is non-terminal and this executor neither polls nor parks
/// (`Wait::None`), so its outcome is unknown and must never be projected as success
/// (G26).
fn end_cause_of(state: &TaskState) -> EndCause {
    match state {
        TaskState::Completed => EndCause::NaturalEnd,
        TaskState::Failed => EndCause::Error(Failure::Inference {
            code: "a2a_task_failed".to_string(),
            message: "remote A2A task ended in the failed state".to_string(),
        }),
        TaskState::Canceled => EndCause::Cancelled,
        TaskState::Working | TaskState::InputRequired | TaskState::AuthRequired => {
            EndCause::Indeterminate
        }
    }
}

#[async_trait]
impl RunExecutor for A2aRunExecutor {
    fn capabilities(&self) -> ExecutorCapabilities {
        // A single request/await against the remote endpoint: no in-flight abort
        // and no park-and-resume are wired, so both axes are fail-closed off.
        ExecutorCapabilities {
            cancellation: Cancellation::None,
            wait: Wait::None,
        }
    }

    async fn execute(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<Phase> {
        let backend =
            Backend::from_ref(&activation.snapshot.resolved_spec.model_binding.backend_ref);
        let Some(endpoint) = backend.remote_endpoint() else {
            // Reached without a remote backend — a wiring fault; fail closed.
            return finish(
                &context,
                &activation,
                vec![Message::text(
                    MessageId("a2a-err-1".to_string()),
                    Role::Assistant,
                    "backend is not an A2A endpoint".to_string(),
                )],
                Phase::Ended(EndCause::Error(Failure::Inference {
                    code: "a2a_config".to_string(),
                    message: "backend is not a2a".to_string(),
                })),
            )
            .await;
        };

        let transport = (self.transport_for)(endpoint);
        let prompt = prompt_of(&activation.input);
        let message_id = format!("a2a-msg-{}", activation.run_id.0);

        match send_message(
            transport.as_ref(),
            None,
            &activation.thread_id.0,
            &message_id,
            &prompt,
        )
        .await
        {
            Ok(task) => {
                let messages = vec![Message::text(
                    MessageId(format!("a2a-{}", activation.run_id.0)),
                    Role::Assistant,
                    task_reply(&task),
                )];
                let phase = Phase::Ended(end_cause_of(&task.status.state));
                finish(&context, &activation, messages, phase).await
            }
            Err(err) => {
                let messages = vec![Message::text(
                    MessageId("a2a-err-1".to_string()),
                    Role::Assistant,
                    format!("remote agent error: {err}"),
                )];
                finish(
                    &context,
                    &activation,
                    messages,
                    Phase::Ended(EndCause::Error(Failure::Inference {
                        code: "a2a_error".to_string(),
                        message: err.to_string(),
                    })),
                )
                .await
            }
        }
    }
}

/// Commit the turn's messages + terminal phase through the one boundary (G13).
async fn finish(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    messages: Vec<Message>,
    phase: Phase,
) -> Result<Phase> {
    if let Some(coordinator) = &context.commit {
        awaken_agent_contract::thread::commit::commit_run(
            coordinator.as_ref(),
            &activation.thread_id,
            &activation.run_id,
            messages,
            phase.clone(),
            None,
            Vec::new(),
        )
        .await
        .map_err(|e| Error::Commit(e.to_string()))?;
    }
    Ok(phase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error as CommitError};
    use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
    use awaken_protocol_a2a::Artifact;
    use awaken_protocol_a2a::types::{
        Message as A2aMessage, MessageRole, Part as A2aPart, TaskStatus,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec(Mutex<Vec<ThreadCommit>>);
    #[async_trait]
    impl Coordinator for Rec {
        async fn commit(&self, c: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
            self.0.lock().unwrap().push(c);
            Ok(CommitRecord { sequence: 1 })
        }
    }

    fn activation(backend_ref: &str) -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("t".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    model_binding: ModelBinding::new("p", "m", backend_ref),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: vec![Message::text(MessageId("u".into()), Role::User, "go")],
            model_ref_override: None,
        }
    }

    // ---- pure translation units (prompt_of / task_reply / end_cause_of) ----

    /// A text agent message for the A2A wire shape (helper for task fixtures).
    fn a2a_msg(text: &str) -> A2aMessage {
        A2aMessage {
            kind: None,
            task_id: None,
            context_id: None,
            message_id: "m".into(),
            role: MessageRole::Agent,
            parts: vec![A2aPart::text(text)],
        }
    }

    /// A returned task with the given status message, history, and artifacts (each
    /// artifact is its list of text parts). Every fixture completes; task_reply
    /// selection is what these exercise.
    fn task_with(status_msg: Option<&str>, history: &[&str], artifacts: &[&[&str]]) -> Task {
        Task {
            kind: None,
            id: "t".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: status_msg.map(a2a_msg),
            },
            history: history.iter().map(|t| a2a_msg(t)).collect(),
            artifacts: artifacts
                .iter()
                .map(|parts| Artifact {
                    name: None,
                    parts: parts.iter().map(|t| A2aPart::text(*t)).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn prompt_of_joins_inputs_with_newlines_and_drops_empty_text() {
        // Multi-turn input: the non-empty texts are joined by "\n"; a message whose
        // text is empty contributes nothing (not a blank line).
        let input = vec![
            Message::text(MessageId("u0".into()), Role::User, "first"),
            Message::text(MessageId("u1".into()), Role::User, ""),
            Message::text(MessageId("u2".into()), Role::Assistant, "second"),
        ];
        assert_eq!(prompt_of(&input), "first\nsecond");
    }

    #[test]
    fn prompt_of_empty_input_is_the_empty_prompt() {
        assert_eq!(prompt_of(&[]), "");
    }

    #[test]
    fn task_reply_joins_multiple_artifacts_with_newlines() {
        // Two durable artifacts: their texts are the reply, joined by "\n".
        assert_eq!(
            task_reply(&task_with(None, &[], &[&["a1"], &["a2"]])),
            "a1\na2"
        );
    }

    #[test]
    fn task_reply_skips_text_empty_artifacts_and_falls_to_status() {
        // An artifact with no text parts is not a reply → fall to the status message.
        assert_eq!(
            task_reply(&task_with(Some("status body"), &["h"], &[&[]])),
            "status body"
        );
    }

    #[test]
    fn task_reply_prefers_status_message_over_history() {
        // No artifacts, but a non-empty status message wins over the last history msg.
        assert_eq!(
            task_reply(&task_with(Some("status"), &["h0", "h1"], &[])),
            "status"
        );
    }

    #[test]
    fn task_reply_empty_status_text_falls_through_to_last_history() {
        // A status message present but text-empty is not a reply → last history msg.
        assert_eq!(
            task_reply(&task_with(Some(""), &["h0", "last"], &[])),
            "last"
        );
    }

    #[test]
    fn task_reply_of_an_empty_task_is_the_empty_string() {
        // No artifacts, no status message, no history → empty reply (no panic).
        assert_eq!(task_reply(&task_with(None, &[], &[])), "");
    }

    #[test]
    fn end_cause_maps_each_task_state_honestly() {
        // Only a completed remote task is a natural end; the rest must not be
        // projected as success (G26).
        assert_eq!(end_cause_of(&TaskState::Completed), EndCause::NaturalEnd);
        assert_eq!(end_cause_of(&TaskState::Canceled), EndCause::Cancelled);
        assert_eq!(end_cause_of(&TaskState::Working), EndCause::Indeterminate);
        assert_eq!(
            end_cause_of(&TaskState::InputRequired),
            EndCause::Indeterminate
        );
        assert_eq!(
            end_cause_of(&TaskState::AuthRequired),
            EndCause::Indeterminate
        );
        assert!(matches!(
            end_cause_of(&TaskState::Failed),
            EndCause::Error(Failure::Inference { ref code, .. }) if code == "a2a_task_failed"
        ));
    }

    /// Drives the real executor against a **real A2A HTTP server** over a real TCP
    /// socket (no mock transport): `over_http()` dials the endpoint on
    /// `Backend::Remote`, the server answers a real A2A task, and the reply is
    /// committed. The server's fixed reply stands for the external remote agent —
    /// the transport, socket, and parse path are all real.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dials_a_real_http_a2a_server_and_commits_the_reply() {
        const REPLY: &str = r#"{"task":{"id":"t-1","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"a","role":"ROLE_AGENT","parts":[{"text":"real remote reply"}]}}}}"#;

        // A real HTTP server on an ephemeral port; a fallback answers the A2A
        // `message:send` POST (the `:` in the path is a matchit param char, so a
        // fallback is simpler than a literal route and just as real).
        let app = axum::Router::new().fallback(|| async { REPLY });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        // The production executor dials it over the real `HttpTransport` (ureq).
        let exec = A2aRunExecutor::over_http();
        let rec = Arc::new(Rec::default());
        let phase = exec
            .execute(
                activation(&format!("a2a:http://{addr}")),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();

        assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
        let commits = rec.0.lock().unwrap();
        assert_eq!(commits[0].messages[0].text_content(), "real remote reply");
    }

    #[test]
    fn advertises_no_in_flight_control() {
        // A single request/await backend: neither cancellation nor park-and-resume
        // is wired, so the host must not offer them for an A2A run (ADR-0055).
        let caps = A2aRunExecutor::over_http().capabilities();
        assert_eq!(caps.cancellation, Cancellation::None);
        assert_eq!(caps.wait, Wait::None);
    }

    #[derive(Default)]
    struct FailingRec;
    #[async_trait]
    impl Coordinator for FailingRec {
        async fn commit(&self, _c: ThreadCommit) -> std::result::Result<CommitRecord, CommitError> {
            Err(CommitError::Rejected("boom".into()))
        }
    }

    /// A real A2A HTTP server answering every `message:send` with `reply`; returns
    /// the `a2a:` backend ref that dials it.
    async fn serve(reply: &'static str) -> String {
        let app = axum::Router::new().fallback(move || async move { reply });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("a2a:http://{addr}")
    }

    #[tokio::test]
    async fn a_non_remote_backend_fails_closed() {
        // Reached without a remote endpoint is a wiring fault → fail closed, no dial.
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation("native"),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            phase,
            Phase::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_config"
        ));
        assert_eq!(
            rec.0.lock().unwrap()[0].messages[0].text_content(),
            "backend is not an A2A endpoint"
        );
    }

    #[tokio::test]
    async fn a_commit_error_propagates() {
        let err = A2aRunExecutor::over_http()
            .execute(
                activation("native"),
                RuntimeRunContext::new().with_commit(Arc::new(FailingRec)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Commit(_)),
            "a rejected commit surfaces as Error::Commit"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn artifacts_are_preferred_over_status_and_history() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed","message":{"messageId":"m","role":"agent","parts":[{"text":"status msg"}]}},"artifacts":[{"parts":[{"text":"artifact body"}]}]}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
        assert_eq!(
            rec.0.lock().unwrap()[0].messages[0].text_content(),
            "artifact body"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_last_is_the_final_reply_fallback() {
        let backend = serve(
            r#"{"task":{"kind":"task","id":"t","contextId":"c","status":{"state":"completed"},"history":[{"messageId":"h0","role":"agent","parts":[{"text":"first"}]},{"messageId":"h1","role":"agent","parts":[{"text":"last history"}]}]}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
        assert_eq!(
            rec.0.lock().unwrap()[0].messages[0].text_content(),
            "last history"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transport_error_ends_with_a2a_error() {
        // Bind then drop the listener: the port now refuses connections, so the dial
        // fails and the executor ends on a classified a2a_error (not a panic).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&format!("a2a:http://{addr}")),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            phase,
            Phase::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_error"
        ));
        assert!(
            rec.0.lock().unwrap()[0].messages[0]
                .text_content()
                .contains("remote agent error")
        );
    }

    /// A real A2A server that records the inbound request (uri + body) and answers a
    /// completed task; returns the backend ref and the shared capture slot.
    async fn serve_capturing() -> (String, Arc<Mutex<Option<(String, String)>>>) {
        const REPLY: &str = r#"{"task":{"id":"t-1","contextId":"c","status":{"state":"TASK_STATE_COMPLETED","message":{"messageId":"a","role":"ROLE_AGENT","parts":[{"text":"ok"}]}}}}"#;
        let cap: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let slot = cap.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let slot = slot.clone();
            async move {
                let uri = req.uri().to_string();
                let bytes = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                *slot.lock().unwrap() = Some((uri, String::from_utf8_lossy(&bytes).into_owned()));
                REPLY
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("a2a:http://{addr}"), cap)
    }

    /// The outbound `message:send` carries the A2A correlation fields the remote
    /// agent keys on — a wrong contextId/messageId silently breaks real agents while
    /// a body-ignoring server test still passes, so assert the real serialized wire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_outbound_message_send_carries_the_correlation_fields() {
        let (backend, cap) = serve_capturing().await;
        let rec = Arc::new(Rec::default());
        A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();

        let (uri, body) = cap.lock().unwrap().clone().expect("a request was captured");
        assert!(uri.ends_with("/v1/a2a/message:send"), "path: {uri}");
        assert!(
            body.contains(r#""contextId":"t""#),
            "contextId = thread id: {body}"
        );
        assert!(
            body.contains(r#""messageId":"a2a-msg-r""#),
            "messageId = a2a-msg-<run_id>: {body}"
        );
        assert!(body.contains(r#""role":"ROLE_USER""#), "user role: {body}");
        assert!(body.contains(r#""text":"go""#), "the prompt text: {body}");
        assert!(
            body.contains(r#""agentId":null"#),
            "agentId is null: {body}"
        );
    }

    /// A real A2A server that answers every request with a fixed HTTP status + body.
    async fn serve_status(status: u16, body: &'static str) -> String {
        let code = axum::http::StatusCode::from_u16(status).unwrap();
        let app = axum::Router::new().fallback(move || async move { (code, body) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("a2a:http://{addr}")
    }

    /// A non-2xx HTTP status from the remote is classified `a2a_error` (not a panic),
    /// mirroring the connection-refused test but for a reachable-but-erroring server.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_http_500_ends_with_a2a_error() {
        let backend = serve_status(500, "upstream boom").await;
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            phase,
            Phase::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_error"
        ));
    }

    /// A 2xx whose body is not a valid A2A task fails the parse and is classified
    /// `a2a_error` — a garbage remote reply fails closed rather than panicking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_malformed_2xx_body_ends_with_a2a_error() {
        let backend = serve("this is not a2a json at all").await;
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            phase,
            Phase::Ended(EndCause::Error(Failure::Inference { ref code, .. })) if code == "a2a_error"
        ));
    }

    /// With no commit coordinator on the context, `execute` still returns the
    /// terminal phase (the commit boundary is a no-op, not a failure).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_commit_coordinator_still_returns_the_terminal_phase() {
        let backend = serve(
            r#"{"task":{"id":"t","contextId":"c","status":{"state":"completed","message":{"messageId":"m","role":"agent","parts":[{"text":"ok"}]}}}}"#,
        )
        .await;
        // RuntimeRunContext::new() carries no commit coordinator.
        let phase = A2aRunExecutor::over_http()
            .execute(activation(&backend), RuntimeRunContext::new())
            .await
            .unwrap();
        assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    }

    /// A remote task returned in the `failed` state is an execution fault, not a
    /// natural end — regression against silently projecting a remote failure as
    /// success (G26). The agent's message is still committed for the reader.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_remote_task_ends_in_error_not_success() {
        let backend = serve(
            r#"{"task":{"id":"t","contextId":"c","status":{"state":"failed","message":{"messageId":"m","role":"agent","parts":[{"text":"model exploded"}]}}}}"#,
        )
        .await;
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert!(matches!(
            phase,
            Phase::Ended(EndCause::Error(Failure::Inference { ref code, .. }))
                if code == "a2a_task_failed"
        ));
        assert_eq!(
            rec.0.lock().unwrap()[0].messages[0].text_content(),
            "model exploded"
        );
    }

    /// A remote task returned in the `canceled` state ends as a cancellation, not a
    /// natural end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_canceled_remote_task_ends_cancelled() {
        let backend =
            serve(r#"{"task":{"id":"t","contextId":"c","status":{"state":"canceled"}}}"#).await;
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(phase, Phase::Ended(EndCause::Cancelled));
    }

    /// A still-`working` remote task is non-terminal; this executor neither polls
    /// nor parks (`Wait::None`), so the outcome is `Indeterminate` — never projected
    /// as a successful natural end (G26).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_still_working_remote_task_is_indeterminate_not_success() {
        let backend =
            serve(r#"{"task":{"id":"t","contextId":"c","status":{"state":"working"}}}"#).await;
        let rec = Arc::new(Rec::default());
        let phase = A2aRunExecutor::over_http()
            .execute(
                activation(&backend),
                RuntimeRunContext::new().with_commit(rec.clone()),
            )
            .await
            .unwrap();
        assert_eq!(phase, Phase::Ended(EndCause::Indeterminate));
    }
}
