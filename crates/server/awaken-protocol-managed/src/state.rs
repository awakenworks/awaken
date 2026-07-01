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
    SendEventsRequest, SendEventsResponse, Session, SessionAgent, StopReason,
};
use crate::project::project_turn;

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// The result of running one step (a new turn, or a resume). `pending` is the
/// tool-use id the run parked on, set when `stop` is `RequiresAction`.
pub struct TurnOutcome {
    pub messages: Vec<Message>,
    pub stop: StopReason,
    pub pending: Option<String>,
}

/// A human-in-the-loop tool decision, delivered by `user.tool_confirmation`.
pub struct Decision {
    pub allow: bool,
    pub note: Option<String>,
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

    /// Answer the tool the run parked on and continue to the next pause or end.
    async fn resume(&self, thread: &str, decision: Decision) -> Result<TurnOutcome, RunError>;

    /// The model id to echo in the session's agent object.
    fn model(&self) -> String;
}

#[derive(Debug, thiserror::Error)]
#[error("run failed: {0}")]
pub struct RunError(pub String);

struct SessionRecord {
    agent_id: String,
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
                events: Vec::new(),
            },
        );
        session
    }

    /// Append one step's projected events to the session, minting ids where the
    /// projection did not supply one.
    fn append_turn(&self, session_id: &str, outcome: TurnOutcome) -> Result<(), StateError> {
        let projected = project_turn(&outcome.messages, outcome.stop, outcome.pending.as_deref());
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
                    result,
                    deny_message,
                    ..
                } => {
                    let decision = Decision {
                        allow: matches!(result, ConfirmResult::Allow),
                        note: deny_message.clone(),
                    };
                    let outcome = self.runtime.resume(session_id, decision).await?;
                    self.append_turn(session_id, outcome)?;
                }
                // Other inbound events are accepted (receipt minted) and wired in
                // later milestones.
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
