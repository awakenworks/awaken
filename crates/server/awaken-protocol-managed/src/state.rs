//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here; a tool-use event keeps
//! the tool call's own id so a `user.tool_confirmation` can reference it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;

use crate::dto::{
    ConfirmResult, CreateSessionRequest, Event, EventReceipt, InboundEvent, ListEventsResponse,
    OutboundKind, SendEventsRequest, SendEventsResponse, Session, SessionAgent, StopReason,
};
use crate::project::{project_messages, project_turn};

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// The tool a run parked on: its id, model-visible name/input, and whether it is
/// client-executed (projected as `agent.custom_tool_use`) or a built-in awaiting
/// confirmation (`agent.tool_use{ask}`).
pub struct Pending {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// The result of running one step (a new turn, or a resume). `pending` is set when
/// `stop` is `RequiresAction`.
pub struct TurnOutcome {
    pub messages: Vec<Message>,
    pub stop: StopReason,
    pub pending: Option<Pending>,
}

/// A human-in-the-loop tool decision, delivered by `user.tool_confirmation`.
pub struct Decision {
    pub allow: bool,
    pub note: Option<String>,
}

/// One evaluation round of a goal: the agent's revision messages committed this
/// round (empty when grading the existing deliverable), and the verdict.
pub struct OutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The result of `user.define_outcome`: the ordered evaluation rounds. The loop
/// always ends idle (`end_turn`).
pub struct OutcomeReport {
    pub iterations: Vec<OutcomeIteration>,
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Run one user turn on `thread` to its first pause or end.
    async fn run_turn(
        &self,
        agent: &str,
        thread: &str,
        user_text: &str,
    ) -> Result<TurnOutcome, RunError>;

    /// Answer a built-in tool the run parked on (allow/deny) and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending built-in tool.
    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: Decision,
    ) -> Result<TurnOutcome, RunError>;

    /// Deliver a client-executed tool's result to the parked run and continue.
    /// `tool_use_id` is the client's asserted target; implementations must fail
    /// closed when it does not name the run's pending client-executed tool.
    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
    ) -> Result<TurnOutcome, RunError>;

    /// Buffer a system message; it is prepended to the next turn's input.
    async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError>;

    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        Ok(())
    }

    /// Define an outcome and drive the grade->revise loop over `thread`, bounded by
    /// `max_iterations`; `rubric` is the normalized requirement text.
    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError>;

    /// The model id to echo in the session's agent object.
    fn model(&self) -> String;
}

/// A runtime failure. `kind` classifies who is at fault so the router can map it
/// to the right HTTP status: a `BadRequest` is the caller's (an unknown park, a
/// mismatched id, a wrong-binding resume); `Internal` is the runtime's.
#[derive(Debug, thiserror::Error)]
#[error("run failed: {message}")]
pub struct RunError {
    pub message: String,
    pub kind: RunErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunErrorKind {
    Internal,
    BadRequest,
}

impl RunError {
    /// A runtime-side failure (provider error, corrupt state) — maps to `500`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::Internal,
        }
    }

    /// A caller-side failure (bad id, wrong binding, no park) — maps to `400`.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RunErrorKind::BadRequest,
        }
    }
}

struct SessionRecord {
    agent_id: String,
    session: Session,
    events: Vec<Event>,
}

/// The adapter's in-memory session store plus the runtime port.
pub struct ManagedState {
    runtime: Box<dyn SessionRuntime>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    session_seq: AtomicU64,
    event_seq: AtomicU64,
}

/// Why a session operation failed (mapped to an HTTP status by the router).
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("session not found")]
    NotFound,
    #[error(transparent)]
    Run(#[from] RunError),
}

impl ManagedState {
    pub fn new(runtime: impl SessionRuntime + 'static) -> Self {
        Self {
            runtime: Box::new(runtime),
            sessions: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(0),
            event_seq: AtomicU64::new(0),
        }
    }

    fn next_event_id(&self) -> String {
        format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
    }

    /// `POST /v1/sessions`.
    pub fn create_session(&self, req: CreateSessionRequest) -> Session {
        let id = format!("sesn_{}", self.session_seq.fetch_add(1, Ordering::SeqCst));
        let agent_id = req.agent.id().to_string();
        let session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                model: self.runtime.model(),
                name: agent_id.clone(),
                tools: Vec::new(),
                mcp_servers: Vec::new(),
            },
            environment_id: req
                .environment_id
                .unwrap_or_else(|| "env_local".to_string()),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: req.title,
            metadata: req.metadata,
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status: "idle",
        };
        self.sessions.lock().unwrap().insert(
            id,
            SessionRecord {
                agent_id,
                session: session.clone(),
                events: Vec::new(),
            },
        );
        session
    }

    /// `GET /v1/sessions/{id}`.
    pub fn get_session(&self, id: &str) -> Result<Session, StateError> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(id)
            .map(|r| r.session.clone())
            .ok_or(StateError::NotFound)
    }

    /// Append one step's projected events to the session, minting ids where the
    /// projection did not supply one.
    fn append_turn(&self, session_id: &str, outcome: TurnOutcome) -> Result<(), StateError> {
        let pending = outcome
            .pending
            .as_ref()
            .map(|p| (p.tool_use_id.as_str(), p.client_executed));
        let projected = project_turn(&outcome.messages, outcome.stop, pending);
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        for event in projected {
            let id = event.id.unwrap_or_else(|| self.next_event_id());
            record.events.push(Event {
                id,
                kind: event.kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        }
        Ok(())
    }

    /// Append an outcome report: for each round, the agent's revision events then
    /// `span.outcome_evaluation_start` / `_end`, and a terminal `session.status_idle`.
    fn append_outcome(&self, session_id: &str, report: OutcomeReport) -> Result<(), StateError> {
        let mut sessions = self.sessions.lock().unwrap();
        let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let mut push = |id: Option<String>, kind: OutboundKind| {
            record.events.push(Event {
                id: id.unwrap_or_else(|| {
                    format!("evt_{}", self.event_seq.fetch_add(1, Ordering::SeqCst))
                }),
                kind,
                processed_at: Some(PROCESSED_AT.to_string()),
            });
        };
        for round in report.iterations {
            for event in project_messages(&round.messages, None) {
                push(event.id, event.kind);
            }
            push(
                None,
                OutboundKind::SpanOutcomeEvaluationStart {
                    outcome_id: round.outcome_id.clone(),
                    iteration: round.iteration,
                },
            );
            push(
                None,
                OutboundKind::SpanOutcomeEvaluationEnd {
                    outcome_id: round.outcome_id,
                    iteration: round.iteration,
                    result: round.result,
                    explanation: round.explanation,
                },
            );
        }
        push(
            None,
            OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::EndTurn,
            },
        );
        Ok(())
    }

    /// `POST /v1/sessions/{id}/events`. Mints a receipt per inbound event and acts
    /// on `user.message` (run a turn) and `user.tool_confirmation` (resume a
    /// parked run), appending the projected events.
    pub async fn send_events(
        &self,
        session_id: &str,
        req: SendEventsRequest,
    ) -> Result<SendEventsResponse, StateError> {
        let agent_id = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(session_id)
                .ok_or(StateError::NotFound)?
                .agent_id
                .clone()
        };

        let mut receipts = Vec::new();
        for inbound in &req.events {
            receipts.push(EventReceipt {
                id: self.next_event_id(),
                kind: inbound.type_str(),
                processed_at: None,
            });

            match inbound {
                InboundEvent::UserMessage { content, .. } => {
                    let text = content_text(content);
                    let outcome = self.runtime.run_turn(&agent_id, session_id, &text).await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserToolConfirmation {
                    tool_use_id,
                    result,
                    deny_message,
                } => {
                    let decision = Decision {
                        allow: matches!(result, ConfirmResult::Allow),
                        note: deny_message.clone(),
                    };
                    let outcome = self
                        .runtime
                        .resume(session_id, tool_use_id, decision)
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserCustomToolResult {
                    custom_tool_use_id,
                    content,
                    is_error,
                } => {
                    let text = content.as_deref().map(content_text).unwrap_or_default();
                    let outcome = self
                        .runtime
                        .resume_custom(session_id, custom_tool_use_id, &text, *is_error)
                        .await?;
                    self.append_turn(session_id, outcome)?;
                }
                InboundEvent::UserDefineOutcome {
                    description,
                    rubric,
                    max_iterations,
                } => {
                    let rubric = rubric_text(rubric);
                    let report = self
                        .runtime
                        .define_outcome(
                            session_id,
                            description,
                            &rubric,
                            max_iterations.unwrap_or(3),
                        )
                        .await?;
                    self.append_outcome(session_id, report)?;
                }
                InboundEvent::SystemMessage { content } => {
                    let text = content_text(content);
                    self.runtime.add_system(session_id, &text).await?;
                }
                // `user.interrupt`: cancel the run in flight on this thread (from a
                // concurrent request), so an in-progress outcome ends `interrupted`.
                InboundEvent::UserInterrupt { .. } => {
                    self.runtime.interrupt(session_id).await?;
                }
                // `user.pause`, `user.resume`: accept-only; the receipt is the
                // acknowledgement.
                _ => {}
            }
        }
        Ok(SendEventsResponse { data: receipts })
    }

    /// `GET /v1/sessions/{id}/events`.
    pub fn list_events(&self, session_id: &str) -> Result<ListEventsResponse, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(ListEventsResponse {
            data: record.events.clone(),
            next_page: None,
            has_more: false,
        })
    }

    /// `GET /v1/sessions/{id}/events/stream` — the events to replay as SSE.
    pub fn stream_events(&self, session_id: &str) -> Result<Vec<Event>, StateError> {
        let sessions = self.sessions.lock().unwrap();
        let record = sessions.get(session_id).ok_or(StateError::NotFound)?;
        Ok(record.events.clone())
    }
}

/// Normalize a Managed rubric (a bare string or `{type:"text",content}`) to text.
fn rubric_text(rubric: &serde_json::Value) -> String {
    match rubric {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// Concatenate the text of a content-block list.
fn content_text(content: &[awaken_agent_contract::agent::content::ContentBlock]) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
