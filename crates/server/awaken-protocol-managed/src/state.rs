//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here and mapped to neutral
//! thread/run identity by the runtime side.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;

use crate::dto::{
    CreateSessionRequest, Event, EventReceipt, InboundEvent, ListEventsResponse, SendEventsRequest,
    SendEventsResponse, Session, SessionAgent, StopReason,
};
use crate::project::project_turn;

/// A fixed projection timestamp (M1). Real per-event timestamps arrive with a
/// clock port; the wire only needs a valid RFC 3339 value here.
const PROCESSED_AT: &str = "2026-01-01T00:00:00Z";

/// The result of running one user turn.
pub struct TurnOutcome {
    pub messages: Vec<Message>,
    pub stop: StopReason,
}

/// The runtime seam the adapter drives (DDD port). Implemented by the server over
/// the kernel; the adapter never constructs a runtime.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    /// Run one user turn on `thread`, returning the messages committed this turn
    /// and the terminal stop reason. `agent` is the session's configured agent id.
    async fn run_turn(
        &self,
        agent: &str,
        thread: &str,
        user_text: &str,
    ) -> Result<TurnOutcome, RunError>;

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

    /// `POST /v1/sessions/{id}/events`. Mints a receipt per inbound event and, for
    /// a `user.message`, runs one turn and appends the projected events.
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

            if let InboundEvent::UserMessage { content, .. } = inbound {
                let text = content_text(content);
                // No lock held across the await (the store is a plain Mutex).
                let outcome = self.runtime.run_turn(&agent_id, session_id, &text).await?;
                let projected = project_turn(&outcome.messages, outcome.stop);
                let mut sessions = self.sessions.lock().unwrap();
                let record = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
                for kind in projected {
                    record.events.push(Event {
                        id: self.next_event_id(),
                        kind,
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                }
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
