//! `A2aRemoteAgent` — the A2A adapter of the kernel's neutral [`RemoteAgent`]
//! interface (the second peer implementation of delegation, beside a native sub-run).
//!
//! It owns the A2A wire entirely: a delegation turn is an A2A `message:send` polled to
//! a terminal state and mapped to a neutral [`DelegationStep`]; discovery is an A2A
//! `agent_card` serialized to JSON. The host holds this behind `dyn RemoteAgent` and
//! names no A2A type — the leak this crate absorbs (ADR: neutral-core / Phase 2).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_protocol_a2a::client::{self as a2a, Transport};
use awaken_protocol_a2a::{Task, TaskState};
use awaken_runtime_contract::CancellationToken;
use awaken_runtime_contract::delegation::{DelegationExecutionError, DelegationStep, RemoteAgent};
use awaken_runtime_contract::llm::ThreadUsage;
use serde_json::{Value, json};

/// Bound on task polling before giving up, so a stuck remote cannot hang a delegation
/// forever.
const MAX_TASK_POLLS: usize = 600;
/// Delay between task polls while a remote task is still `working`.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Drives one registered A2A remote agent (bound to its `transport`) as a neutral
/// [`RemoteAgent`]. The composition root builds one per remote `agent_id`.
pub struct A2aRemoteAgent {
    transport: Arc<dyn Transport>,
}

impl A2aRemoteAgent {
    /// Bind the delegate to the transport that reaches the remote agent's A2A endpoint.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

#[async_trait]
impl RemoteAgent for A2aRemoteAgent {
    async fn run(
        &self,
        agent_id: &str,
        request_id: &str,
        input: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<DelegationStep, DelegationExecutionError> {
        remote_run(
            self.transport.as_ref(),
            agent_id,
            request_id,
            input,
            cancellation,
        )
        .await
    }

    async fn card(&self, _agent_id: &str) -> Result<Value, DelegationExecutionError> {
        let card = a2a::agent_card(self.transport.as_ref())
            .await
            .map_err(|e| DelegationExecutionError::new(e.to_string()))?;
        serde_json::to_value(card).map_err(|e| DelegationExecutionError::new(e.to_string()))
    }
}

fn completed_reply(task: &Task) -> String {
    let mut reply = task
        .status
        .message
        .as_ref()
        .map(|message| message.text())
        .unwrap_or_default();
    for artifact in &task.artifacts {
        let text = artifact.text();
        if !text.is_empty() {
            if !reply.is_empty() {
                reply.push('\n');
            }
            reply.push_str(&text);
        }
    }
    reply
}

/// Map a non-working task to a delegation step: completed → done; input/auth
/// required → awaiting (the parent awaits for the user, resumed via the handle);
/// failed → error.
fn step_from_task(agent_id: &str, task: Task) -> Result<DelegationStep, DelegationExecutionError> {
    match task.status.state {
        // A remote (A2A) delegate runs on another host: its token spend is not
        // observable over the A2A wire, so no usage rolls into the parent tally.
        TaskState::Completed => Ok(DelegationStep::Ended {
            text: completed_reply(&task),
            usage: ThreadUsage::default(),
        }),
        TaskState::InputRequired | TaskState::AuthRequired => Ok(DelegationStep::Awaiting {
            continuation: json!({ "agent_id": agent_id, "task_id": task.id }),
        }),
        TaskState::Failed => Err(DelegationExecutionError::new("remote A2A agent failed")),
        TaskState::Canceled => Err(DelegationExecutionError::new(
            "remote A2A task was canceled",
        )),
        TaskState::Working => Err(DelegationExecutionError::new(
            "remote A2A task did not reach a terminal state in time",
        )),
    }
}

/// Run one remote-agent turn: `message:send`, poll `working` to a terminal state
/// (bounded, cancellation-aware; a parent interrupt cancels the remote task), then
/// map the task to a step.
async fn remote_run(
    transport: &dyn Transport,
    agent_id: &str,
    request_id: &str,
    input: &str,
    cancellation: Option<&CancellationToken>,
) -> Result<DelegationStep, DelegationExecutionError> {
    // A recovery retry reuses both ids, so an A2A peer can deduplicate the
    // durable request instead of spawning a second task.
    let context_id = format!("deleg-{request_id}");
    let message_id = format!("delegation-message-{request_id}");
    let mut task = a2a::send_message(transport, Some(agent_id), &context_id, &message_id, input)
        .await
        .map_err(|e| DelegationExecutionError::new(e.to_string()))?;

    let mut polls = 0usize;
    while matches!(task.status.state, TaskState::Working) {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            a2a::cancel_task(transport, &task.id).await;
            return Err(DelegationExecutionError::new(
                "remote A2A delegation was cancelled",
            ));
        }
        if polls >= MAX_TASK_POLLS {
            break;
        }
        polls += 1;
        task = a2a::get_task(transport, &task.id)
            .await
            .map_err(|e| DelegationExecutionError::new(e.to_string()))?;
        if matches!(task.status.state, TaskState::Working) {
            match cancellation {
                Some(token) => {
                    tokio::select! {
                        _ = tokio::time::sleep(POLL_INTERVAL) => {}
                        _ = token.cancelled() => {
                            a2a::cancel_task(transport, &task.id).await;
                            return Err(DelegationExecutionError::new("remote A2A delegation was cancelled"));
                        }
                    }
                }
                None => tokio::time::sleep(POLL_INTERVAL).await,
            }
        }
    }
    step_from_task(agent_id, task)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_protocol_a2a::client::Response;

    use super::*;

    /// Records the requests it saw and replies with a queued body, so the adapter's
    /// routing (which A2A path) is asserted without a socket.
    struct MockTransport {
        seen: Mutex<Vec<(String, String)>>,
        status: u16,
        reply: String,
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn request(
            &self,
            method: &str,
            path: &str,
            _body: Option<Vec<u8>>,
        ) -> std::result::Result<Response, String> {
            self.seen
                .lock()
                .unwrap()
                .push((method.to_string(), path.to_string()));
            Ok(Response::new(self.status, self.reply.clone().into_bytes()))
        }
    }

    /// Cause-effect on the discovery card: a 200 card body → the neutral JSON `Value`
    /// the host echoes, fetched over the A2A `agent-card` GET route. Pins the wire the
    /// host no longer names (the leak this adapter absorbs).
    #[tokio::test]
    async fn card_fetches_the_agent_card_route_and_returns_neutral_json() {
        let transport = Arc::new(MockTransport {
            seen: Mutex::new(Vec::new()),
            status: 200,
            reply: r#"{"name":"researcher","description":"a remote agent","version":"1.2.3","protocolVersion":"1.0","capabilities":{"streaming":false,"pushNotifications":false}}"#
                .to_string(),
        });
        let delegate = A2aRemoteAgent::new(transport.clone());

        let card = delegate.card("researcher").await.expect("card");
        assert_eq!(card["name"], "researcher");
        assert_eq!(card["protocolVersion"], "1.0");
        let seen = transport.seen.lock().unwrap();
        assert_eq!(
            seen.as_slice(),
            &[("GET".into(), "/v1/a2a/agent-card".into())]
        );
    }

    /// A non-2xx card fetch fails (not a bogus card).
    #[tokio::test]
    async fn card_surfaces_a_non_2xx_as_an_error() {
        let transport = Arc::new(MockTransport {
            seen: Mutex::new(Vec::new()),
            status: 500,
            reply: r#"{"error":{"message":"boom"}}"#.to_string(),
        });
        let delegate = A2aRemoteAgent::new(transport);
        assert!(delegate.card("flaky").await.is_err());
    }
}
