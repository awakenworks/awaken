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
use awaken_protocol_a2a::{HttpTransport, Task, Transport};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::{Error, Result, RunExecutor};
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

#[async_trait]
impl RunExecutor for A2aRunExecutor {
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
                finish(
                    &context,
                    &activation,
                    messages,
                    Phase::Ended(EndCause::NaturalEnd),
                )
                .await
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
        awaken_agent_contract::commit::commit_run_turn(
            coordinator.as_ref(),
            &activation.thread_id,
            &activation.run_id,
            messages,
            phase.clone(),
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
    use awaken_agent_contract::commit::coordinator::{Coordinator, Error as CommitError};
    use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
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
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: vec![Message::text(MessageId("u".into()), Role::User, "go")],
            trace: Default::default(),
        }
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
}
