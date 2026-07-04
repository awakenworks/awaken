//! ACP client-side drive loop.
//!
//! Connects to an ACP agent over a pair of async byte streams, runs the
//! `initialize → new_session → prompt` lifecycle, and projects every
//! `session/update` notification that arrives into [`AgentEvent`]s forwarded
//! to an [`EventSink`].
//!
//! Synthetic lifecycle events emitted around the ACP turn:
//! - `RunStart` emitted before the prompt so downstream sinks (e.g. `DurableEventSink`)
//!   can open the run record.
//! - `RunFinish` emitted after all events have been forwarded, carrying the
//!   `TerminationReason` mapped from the ACP stop reason.
//!
//! Decoder mappings (ACP → internal):
//! - `AgentMessageChunk`         → `TextDelta`
//! - `AgentThoughtChunk`         → `ReasoningDelta`
//! - `ToolCall`                  → `ToolCallReady` (args from `raw_input`)
//! - `ToolCallUpdate` (done)     → `ToolCallDone / Succeeded`
//! - `ToolCallUpdate` (failed)   → `ToolCallDone / Failed`
//! - `Plan`                      → `ActivitySnapshot { activity_type: "acp_plan" }`
//! - everything else             → silently ignored

use std::sync::Arc;

use agent_client_protocol::{self as acp, Agent as _};
use awaken_server_contract::contract::event::AgentEvent;
use awaken_server_contract::contract::event_sink::EventSink;
use awaken_server_contract::contract::lifecycle::TerminationReason;
use awaken_server_contract::contract::suspension::ToolCallOutcome;
use awaken_server_contract::contract::tool::ToolResult;
use futures::{AsyncRead, AsyncWrite};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// Errors that can occur during an ACP client-side drive loop.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("ACP protocol error: {0}")]
    Acp(#[from] acp::Error),
    #[error("ACP I/O setup error: {0}")]
    Io(String),
}

impl DriverError {
    fn io(msg: impl std::fmt::Display) -> Self {
        Self::Io(msg.to_string())
    }
}

/// Stop reason returned after a completed ACP turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStopReason {
    EndTurn,
    MaxTokens,
    Cancelled,
    Refusal,
}

impl From<acp::StopReason> for TurnStopReason {
    fn from(r: acp::StopReason) -> Self {
        match r {
            acp::StopReason::EndTurn | acp::StopReason::MaxTurnRequests => Self::EndTurn,
            acp::StopReason::MaxTokens => Self::MaxTokens,
            acp::StopReason::Cancelled => Self::Cancelled,
            acp::StopReason::Refusal => Self::Refusal,
            _ => Self::EndTurn,
        }
    }
}

/// Parameters for a single ACP turn.
pub struct TurnParams {
    /// Working directory reported to the agent in `session/new`.
    pub cwd: String,
    /// Prompt content sent in `session/prompt`.
    pub prompt: Vec<acp::ContentBlock>,
    /// Thread identifier included in synthetic `RunStart`/`RunFinish` events.
    pub thread_id: String,
    /// Run identifier included in synthetic `RunStart`/`RunFinish` events.
    pub run_id: String,
}

impl TurnParams {
    /// Create params with a single text prompt and auto-generated run identity.
    pub fn new(cwd: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            prompt: vec![acp::ContentBlock::from(text.into())],
            thread_id: uuid::Uuid::now_v7().to_string(),
            run_id: uuid::Uuid::now_v7().to_string(),
        }
    }
}

/// ACP `Client` implementation that pipes `session/update` notifications into
/// an mpsc channel as projected [`AgentEvent`]s, and auto-approves every
/// permission request so turns can complete without operator interaction.
struct NotificationClient {
    tx: mpsc::UnboundedSender<AgentEvent>,
}

impl NotificationClient {
    fn new(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }

    fn send(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for NotificationClient {
    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        for event in project_session_update(args.update) {
            self.send(event);
        }
        Ok(())
    }

    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let chosen = args
            .options
            .iter()
            .find(|o| {
                matches!(
                    o.kind,
                    acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::AllowAlways
                )
            })
            .map(|o| o.option_id.clone())
            .unwrap_or_else(|| acp::PermissionOptionId::new("opt_allow_once"));

        Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(chosen)),
        ))
    }
}

/// Extract a plain text string from an ACP `ContentBlock`.
///
/// Non-text content blocks (images, audio, …) produce an empty string.
fn content_block_text(block: &acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(t) => t.text.clone(),
        _ => String::new(),
    }
}

/// Project a single [`acp::SessionUpdate`] into zero or more [`AgentEvent`]s.
///
/// This is the decoder direction (ACP → internal), the reverse of
/// [`super::encoder::AcpEncoder`] which maps internal events to ACP types.
pub(super) fn project_session_update(update: acp::SessionUpdate) -> Vec<AgentEvent> {
    match update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            let delta = content_block_text(&chunk.content);
            if delta.is_empty() {
                vec![]
            } else {
                vec![AgentEvent::TextDelta { delta }]
            }
        }

        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            let delta = content_block_text(&chunk.content);
            if delta.is_empty() {
                vec![]
            } else {
                vec![AgentEvent::ReasoningDelta { delta }]
            }
        }

        acp::SessionUpdate::ToolCall(tc) => {
            let id = tc.tool_call_id.0.as_ref().to_string();
            let arguments = tc.raw_input.clone().unwrap_or(Value::Null);
            vec![AgentEvent::ToolCallReady {
                id,
                name: tc.title.clone(),
                arguments,
            }]
        }

        acp::SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.as_ref().to_string();
            match update.fields.status {
                Some(acp::ToolCallStatus::Completed) => {
                    let data = update.fields.raw_output.clone().unwrap_or(Value::Null);
                    vec![AgentEvent::ToolCallDone {
                        id: id.clone(),
                        message_id: String::new(),
                        result: ToolResult::success(id, data),
                        outcome: ToolCallOutcome::Succeeded,
                    }]
                }
                Some(acp::ToolCallStatus::Failed) => {
                    vec![AgentEvent::ToolCallDone {
                        id: id.clone(),
                        message_id: String::new(),
                        result: ToolResult::error(id, "tool call failed"),
                        outcome: ToolCallOutcome::Failed,
                    }]
                }
                _ => vec![],
            }
        }

        acp::SessionUpdate::Plan(plan) => {
            let content = serde_json::to_value(&plan).unwrap_or(Value::Null);
            vec![AgentEvent::ActivitySnapshot {
                message_id: String::new(),
                activity_type: "acp_plan".to_string(),
                content,
                replace: Some(true),
            }]
        }

        _ => vec![],
    }
}

/// Map an ACP [`TurnStopReason`] to an internal [`TerminationReason`].
fn stop_reason_to_termination(stop: &TurnStopReason) -> TerminationReason {
    match stop {
        TurnStopReason::EndTurn => TerminationReason::NaturalEnd,
        TurnStopReason::MaxTokens => TerminationReason::stopped("max_rounds_reached"),
        TurnStopReason::Cancelled => TerminationReason::Cancelled,
        TurnStopReason::Refusal => TerminationReason::Blocked("acp_refusal".into()),
    }
}

/// Drive one ACP turn over the provided async byte streams, forwarding all
/// projected [`AgentEvent`]s to `sink` in real time.
///
/// Emits synthetic `RunStart` before the prompt and `RunFinish` (with the
/// appropriate [`TerminationReason`]) after all events have been drained, so
/// that durable sinks can open and close the run record correctly.
///
/// **Must be called from within a [`tokio::task::LocalSet`]** (or from a
/// `LocalSet::run_until` context).  Both `acp::ClientSideConnection` and the
/// prompt task use `spawn_local`, so a `LocalSet` must already be active on
/// the calling thread.
///
/// # Arguments
/// - `outgoing` / `incoming`: write/read byte streams connected to the ACP agent.
/// - `params`: identity, working directory, and prompt for this turn.
/// - `sink`: destination for every projected [`AgentEvent`], including the
///   synthetic lifecycle events.
pub async fn run_turn<W, R>(
    outgoing: W,
    incoming: R,
    params: TurnParams,
    sink: Arc<dyn EventSink>,
) -> Result<TurnStopReason, DriverError>
where
    W: AsyncWrite + Unpin + 'static,
    R: AsyncRead + Unpin + 'static,
{
    let thread_id = params.thread_id.clone();
    let run_id = params.run_id.clone();

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let client = NotificationClient::new(event_tx);

    let (conn, io_task) = acp::ClientSideConnection::new(client, outgoing, incoming, |future| {
        tokio::task::spawn_local(future);
    });
    tokio::task::spawn_local(io_task);

    conn.initialize(acp::InitializeRequest::new(acp::ProtocolVersion::V1))
        .await
        .map_err(DriverError::Acp)?;

    let session = conn
        .new_session(acp::NewSessionRequest::new(&params.cwd))
        .await
        .map_err(DriverError::Acp)?;

    // Synthetic RunStart so durable sinks can open the run record.
    sink.emit(AgentEvent::RunStart {
        thread_id: thread_id.clone(),
        run_id: run_id.clone(),
        identity: None,
        parent_run_id: None,
    })
    .await;

    // Spawn the prompt as a LocalSet task so we can drain notifications in
    // real time while it is in progress.  conn is !Send but spawn_local does
    // not require Send.
    let (prompt_tx, mut prompt_rx) = oneshot::channel::<acp::Result<acp::PromptResponse>>();
    tokio::task::spawn_local(async move {
        let result = conn
            .prompt(acp::PromptRequest::new(session.session_id, params.prompt))
            .await;
        let _ = prompt_tx.send(result);
    });

    // Forward notification events as they arrive; stop when the prompt task
    // signals completion.
    let stop_reason = loop {
        tokio::select! {
            biased;
            // Prefer draining events over checking prompt completion so that
            // events that arrive concurrently with the final response are not
            // dropped.
            Some(event) = event_rx.recv() => {
                sink.emit(event).await;
            }
            result = &mut prompt_rx => {
                // Drain any remaining buffered events before closing.
                while let Ok(event) = event_rx.try_recv() {
                    sink.emit(event).await;
                }
                match result {
                    Ok(Ok(resp)) => break TurnStopReason::from(resp.stop_reason),
                    Ok(Err(e)) => return Err(DriverError::Acp(e)),
                    Err(_) => return Err(DriverError::io("prompt task dropped before completion")),
                }
            }
        }
    };

    // Synthetic RunFinish for lifecycle completion.
    let termination = stop_reason_to_termination(&stop_reason);
    sink.emit(AgentEvent::RunFinish {
        thread_id,
        run_id,
        identity: None,
        result: None,
        termination,
    })
    .await;

    Ok(stop_reason)
}

/// Spawn a Claude Code subprocess and drive one ACP turn via its stdio.
///
/// Looks for the `claude` binary on `PATH` (or the path in `AWAKEN_CLAUDE_BIN`)
/// and passes `--acp-stdio` to put it into ACP server mode.
///
/// # Errors
/// Returns [`DriverError::Io`] when the process cannot be spawned, and
/// [`DriverError::Acp`] on ACP protocol failures.
pub async fn run_turn_subprocess(
    params: TurnParams,
    sink: Arc<dyn EventSink>,
    extra_env: &[(String, String)],
) -> Result<TurnStopReason, DriverError> {
    use tokio::process::Command;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    let claude_bin = std::env::var("AWAKEN_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());

    let mut child = Command::new(&claude_bin)
        .arg("--acp-stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .envs(extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .spawn()
        .map_err(|e| DriverError::io(format!("failed to spawn {claude_bin}: {e}")))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| DriverError::io("failed to open child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DriverError::io("failed to open child stdout"))?;

    let local_set = tokio::task::LocalSet::new();
    let result = local_set
        .run_until(run_turn(
            stdin.compat_write(),
            stdout.compat(),
            params,
            sink,
        ))
        .await;

    let _ = child.kill().await;
    let _ = child.wait().await;

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use agent_client_protocol_schema::{ContentBlock, ContentChunk, TextContent, ToolCallId};
    use awaken_server_contract::contract::suspension::ToolCallOutcome;
    use awaken_server_contract::contract::tool::ToolStatus;

    #[derive(Default)]
    struct CollectingSink {
        events: Arc<Mutex<Vec<AgentEvent>>>,
    }

    impl CollectingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn events(&self) -> Vec<AgentEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl EventSink for CollectingSink {
        async fn emit(&self, event: AgentEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    // ── Unit tests for project_session_update ────────────────────────────────

    #[test]
    fn project_agent_message_chunk_to_text_delta() {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hello")));
        let events = project_session_update(acp::SessionUpdate::AgentMessageChunk(chunk));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], AgentEvent::TextDelta { delta } if delta == "hello"),
            "unexpected: {:?}",
            events
        );
    }

    #[test]
    fn project_agent_thought_chunk_to_reasoning_delta() {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("thinking...")));
        let events = project_session_update(acp::SessionUpdate::AgentThoughtChunk(chunk));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], AgentEvent::ReasoningDelta { delta } if delta == "thinking..."),
            "unexpected: {:?}",
            events
        );
    }

    #[test]
    fn project_empty_chunk_emits_nothing() {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("")));
        let events = project_session_update(acp::SessionUpdate::AgentMessageChunk(chunk));
        assert!(events.is_empty());
    }

    #[test]
    fn project_tool_call_preserves_raw_input() {
        use agent_client_protocol_schema::{ToolCall, ToolCallStatus, ToolKind};
        use serde_json::json;

        let input = json!({"cmd": "ls -la"});
        let tc = ToolCall::new(ToolCallId::new("call_1"), "bash")
            .kind(ToolKind::Execute)
            .status(ToolCallStatus::Pending)
            .raw_input(input.clone());

        let events = project_session_update(acp::SessionUpdate::ToolCall(tc));
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCallReady {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(arguments, &input);
            }
            other => panic!("expected ToolCallReady, got: {other:?}"),
        }
    }

    #[test]
    fn project_tool_call_without_raw_input_yields_null() {
        use agent_client_protocol_schema::ToolCall;

        let tc = ToolCall::new(ToolCallId::new("call_2"), "read_file");
        let events = project_session_update(acp::SessionUpdate::ToolCall(tc));
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCallReady { arguments, .. } => {
                assert!(arguments.is_null(), "expected Null, got: {arguments}");
            }
            other => panic!("expected ToolCallReady, got: {other:?}"),
        }
    }

    #[test]
    fn project_tool_call_update_completed_succeeds() {
        use agent_client_protocol_schema::{ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields};
        use serde_json::json;

        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .raw_output(json!({"result": "ok"}));
        let update = ToolCallUpdate::new(ToolCallId::new("call_1"), fields);
        let events = project_session_update(acp::SessionUpdate::ToolCallUpdate(update));
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCallDone {
                outcome, result, ..
            } => {
                assert_eq!(*outcome, ToolCallOutcome::Succeeded);
                assert_eq!(result.status, ToolStatus::Success);
            }
            other => panic!("expected ToolCallDone, got: {other:?}"),
        }
    }

    #[test]
    fn project_tool_call_update_failed_emits_failed() {
        use agent_client_protocol_schema::{ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields};

        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let update = ToolCallUpdate::new(ToolCallId::new("call_2"), fields);
        let events = project_session_update(acp::SessionUpdate::ToolCallUpdate(update));
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::ToolCallDone {
                outcome, result, ..
            } => {
                assert_eq!(*outcome, ToolCallOutcome::Failed);
                assert_eq!(result.status, ToolStatus::Error);
            }
            other => panic!("expected ToolCallDone, got: {other:?}"),
        }
    }

    #[test]
    fn project_tool_call_update_in_progress_emits_nothing() {
        use agent_client_protocol_schema::{ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields};

        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        let update = ToolCallUpdate::new(ToolCallId::new("call_3"), fields);
        let events = project_session_update(acp::SessionUpdate::ToolCallUpdate(update));
        assert!(
            events.is_empty(),
            "in-progress should be silent: {events:?}"
        );
    }

    #[test]
    fn project_plan_emits_activity_snapshot() {
        use agent_client_protocol_schema::Plan;

        let plan = Plan::new(vec![]);
        let events = project_session_update(acp::SessionUpdate::Plan(plan));
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0],
                AgentEvent::ActivitySnapshot { activity_type, .. }
                if activity_type == "acp_plan"
            ),
            "unexpected: {:?}",
            events
        );
    }

    // ── Integration tests: client driver ↔ ACP stdio server ──────────────────

    /// Verify `run_turn` emits synthetic `RunStart` and `RunFinish` lifecycle
    /// events around the projected turn events, and that text notifications are
    /// forwarded in real time when connected to the ACP stdio server.
    #[tokio::test]
    async fn run_turn_emits_lifecycle_and_text_events() {
        use tokio::io::{BufReader, split};
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

        use crate::protocols::acp::stdio::serve_stdio_io;
        use awaken_runtime::builder::AgentRuntimeBuilder;
        use awaken_server_contract::ModelSpec;
        use awaken_server_contract::contract::content::ContentBlock as RuntimeContentBlock;
        use awaken_server_contract::contract::executor::{
            InferenceExecutionError, InferenceRequest,
        };
        use awaken_server_contract::contract::inference::{
            StopReason as RuntimeStopReason, StreamResult, TokenUsage,
        };
        use awaken_server_contract::registry_spec::AgentSpec;

        struct EchoExecutor;

        #[async_trait::async_trait]
        impl awaken_server_contract::contract::executor::LlmExecutor for EchoExecutor {
            async fn execute(
                &self,
                request: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                let user_text = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| {
                        if m.role == awaken_server_contract::contract::message::Role::User {
                            Some(m.text())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                Ok(StreamResult {
                    content: vec![RuntimeContentBlock::text(format!("echo: {user_text}"))],
                    tool_calls: vec![],
                    usage: Some(TokenUsage::default()),
                    stop_reason: Some(RuntimeStopReason::EndTurn),
                    has_incomplete_tool_calls: false,
                })
            }

            fn name(&self) -> &str {
                "echo-driver"
            }
        }

        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_model(ModelSpec::new("test-model", "echo-driver", "echo-driver-m"))
                .with_provider("echo-driver", Arc::new(EchoExecutor))
                .with_agent_spec(AgentSpec {
                    id: "echo".into(),
                    model_id: "test-model".into(),
                    system_prompt: "You are an echo bot".into(),
                    max_rounds: 2,
                    ..Default::default()
                })
                .build()
                .expect("build runtime"),
        );

        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async move {
                let sink = CollectingSink::new();
                let (client_stream, server_stream) = tokio::io::duplex(16 * 1024);
                let (client_reader, client_writer) = split(client_stream);
                let (server_reader, server_writer) = split(server_stream);

                let server_task = tokio::task::spawn_local(serve_stdio_io(
                    runtime,
                    BufReader::new(server_reader),
                    server_writer,
                ));

                let params = TurnParams::new("/tmp", "hello acp");
                let expected_thread_id = params.thread_id.clone();
                let expected_run_id = params.run_id.clone();

                let stop = run_turn(
                    client_writer.compat_write(),
                    client_reader.compat(),
                    params,
                    Arc::clone(&sink) as Arc<dyn EventSink>,
                )
                .await
                .expect("run_turn should succeed");

                assert_eq!(stop, TurnStopReason::EndTurn);

                let events = sink.events();

                // RunStart must be the first event
                assert!(
                    matches!(&events[0],
                        AgentEvent::RunStart { thread_id, run_id, .. }
                        if thread_id == &expected_thread_id && run_id == &expected_run_id
                    ),
                    "first event must be RunStart with correct ids, got: {:?}",
                    events.first()
                );

                // RunFinish must be the last event
                assert!(
                    matches!(events.last(),
                        Some(AgentEvent::RunFinish { thread_id, run_id, .. })
                        if thread_id == &expected_thread_id && run_id == &expected_run_id
                    ),
                    "last event must be RunFinish with correct ids, got: {:?}",
                    events.last()
                );

                // At least one TextDelta with the echoed content
                let has_echo = events.iter().any(|e| {
                    matches!(e, AgentEvent::TextDelta { delta } if delta.contains("echo: hello acp"))
                });
                assert!(
                    has_echo,
                    "expected TextDelta containing echo reply, got: {events:?}"
                );

                server_task.abort();
                let _ = server_task.await;
            })
            .await;
    }

    /// Verify that `run_turn` correctly projects tool call events through the
    /// full driver loop (not just the decoder unit tests).
    #[tokio::test]
    async fn run_turn_projects_tool_call_events() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{BufReader, split};
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

        use crate::protocols::acp::stdio::serve_stdio_io;
        use awaken_runtime::builder::AgentRuntimeBuilder;
        use awaken_server_contract::ModelSpec;
        use awaken_server_contract::contract::content::ContentBlock as RuntimeContentBlock;
        use awaken_server_contract::contract::executor::{
            InferenceExecutionError, InferenceRequest,
        };
        use awaken_server_contract::contract::inference::{
            StopReason as RuntimeStopReason, StreamResult, TokenUsage,
        };
        use awaken_server_contract::contract::message::ToolCall as RuntimeToolCall;
        use awaken_server_contract::contract::tool::{
            Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult as RT,
        };
        use awaken_server_contract::registry_spec::AgentSpec;
        use serde_json::json;

        struct ToolCallMockExecutor {
            call_count: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl awaken_server_contract::contract::executor::LlmExecutor for ToolCallMockExecutor {
            async fn execute(
                &self,
                _request: InferenceRequest,
            ) -> Result<StreamResult, InferenceExecutionError> {
                let count = self.call_count.fetch_add(1, Ordering::Relaxed);
                if count == 0 {
                    Ok(StreamResult {
                        content: vec![],
                        tool_calls: vec![RuntimeToolCall::new(
                            "call_tool_1",
                            "ping",
                            json!({"host": "localhost"}),
                        )],
                        usage: Some(TokenUsage::default()),
                        stop_reason: Some(RuntimeStopReason::ToolUse),
                        has_incomplete_tool_calls: false,
                    })
                } else {
                    Ok(StreamResult {
                        content: vec![RuntimeContentBlock::text("pong")],
                        tool_calls: vec![],
                        usage: Some(TokenUsage::default()),
                        stop_reason: Some(RuntimeStopReason::EndTurn),
                        has_incomplete_tool_calls: false,
                    })
                }
            }

            fn name(&self) -> &str {
                "tool-mock"
            }
        }

        struct PingTool;

        #[async_trait::async_trait]
        impl Tool for PingTool {
            fn descriptor(&self) -> ToolDescriptor {
                ToolDescriptor::new("ping", "ping", "Ping a host")
            }

            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolCallContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(RT::success("ping", json!({"reply": "pong"})).into())
            }
        }

        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_model(ModelSpec::new("test-model", "tool-mock", "tool-mock-m"))
                .with_provider(
                    "tool-mock",
                    Arc::new(ToolCallMockExecutor {
                        call_count: AtomicUsize::new(0),
                    }),
                )
                .with_tool("ping", Arc::new(PingTool))
                .with_agent_spec(AgentSpec {
                    id: "pinger".into(),
                    model_id: "test-model".into(),
                    system_prompt: "You use the ping tool".into(),
                    max_rounds: 3,
                    ..Default::default()
                })
                .build()
                .expect("build runtime"),
        );

        let local_set = tokio::task::LocalSet::new();
        local_set
            .run_until(async move {
                let sink = CollectingSink::new();
                let (client_stream, server_stream) = tokio::io::duplex(32 * 1024);
                let (client_reader, client_writer) = split(client_stream);
                let (server_reader, server_writer) = split(server_stream);

                let server_task = tokio::task::spawn_local(serve_stdio_io(
                    runtime,
                    BufReader::new(server_reader),
                    server_writer,
                ));

                let stop = run_turn(
                    client_writer.compat_write(),
                    client_reader.compat(),
                    TurnParams::new("/tmp", "ping localhost"),
                    Arc::clone(&sink) as Arc<dyn EventSink>,
                )
                .await
                .expect("run_turn with tool call should succeed");

                assert_eq!(stop, TurnStopReason::EndTurn);

                let events = sink.events();

                // Must start with RunStart
                assert!(
                    matches!(&events[0], AgentEvent::RunStart { .. }),
                    "first event must be RunStart, got: {:?}",
                    events.first()
                );

                // Must end with RunFinish
                assert!(
                    matches!(events.last(), Some(AgentEvent::RunFinish { .. })),
                    "last event must be RunFinish, got: {:?}",
                    events.last()
                );

                // Must contain a ToolCallReady for "ping"
                let tool_ready = events
                    .iter()
                    .any(|e| matches!(e, AgentEvent::ToolCallReady { name, .. } if name == "ping"));
                assert!(
                    tool_ready,
                    "expected ToolCallReady for ping tool, got: {events:?}"
                );

                // Must contain a ToolCallDone
                let tool_done = events
                    .iter()
                    .any(|e| matches!(e, AgentEvent::ToolCallDone { .. }));
                assert!(tool_done, "expected ToolCallDone, got: {events:?}");

                server_task.abort();
                let _ = server_task.await;
            })
            .await;
    }

    /// Verify the subprocess driver can spawn a binary that speaks ACP stdio.
    ///
    /// Requires `AWAKEN_CLAUDE_BIN` to point to a Claude Code binary that
    /// supports `--acp-stdio`.  Skipped automatically when the binary is
    /// absent or the env var is unset.
    #[tokio::test]
    async fn run_turn_subprocess_with_real_binary() {
        let claude_bin = match std::env::var("AWAKEN_CLAUDE_BIN") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("skipping subprocess e2e test: AWAKEN_CLAUDE_BIN not set");
                return;
            }
        };

        // Skip gracefully when the binary path does not exist on disk.
        if !std::path::Path::new(&claude_bin).exists() {
            eprintln!("skipping subprocess e2e test: {claude_bin} not found");
            return;
        }

        let sink = Arc::new(CollectingSink::default());
        let params = TurnParams::new("/tmp", "Reply with exactly: hello");
        let expected_thread_id = params.thread_id.clone();
        let expected_run_id = params.run_id.clone();

        let result =
            run_turn_subprocess(params, Arc::clone(&sink) as Arc<dyn EventSink>, &[]).await;

        match result {
            Ok(stop) => {
                let events = sink.events();
                assert!(
                    matches!(events.first(), Some(AgentEvent::RunStart { thread_id, run_id, .. })
                        if thread_id == &expected_thread_id && run_id == &expected_run_id),
                    "first event must be RunStart, got: {events:?}"
                );
                assert!(
                    matches!(events.last(), Some(AgentEvent::RunFinish { .. })),
                    "last event must be RunFinish, got: {events:?}"
                );
                assert_eq!(
                    stop,
                    TurnStopReason::EndTurn,
                    "subprocess turn should complete normally"
                );
            }
            Err(DriverError::Io(msg)) if msg.contains("failed to spawn") => {
                eprintln!("skipping subprocess e2e test: spawn failed — {msg}");
            }
            Err(e) => panic!("unexpected driver error: {e}"),
        }
    }
}
