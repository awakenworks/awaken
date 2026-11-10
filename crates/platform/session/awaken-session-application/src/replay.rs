// @formal surface requirement=analytics.observability.projection_only
// @formal reason: Defines the Session-scoped read contract and evidence DTOs for committed history.
use awaken_agent_contract::thread::read::replay::CommittedThreadPrefix;
use awaken_agent_contract::{RunId, ThreadId};
use awaken_dispatch_contract::run_dispatch::ArchivedRunActivation;
use awaken_session_contract::PersistedSession;
use awaken_session_types::SessionRevision;

use crate::SessionAccess;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ReplayPointId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReplayPoint {
    pub id: ReplayPointId,
    pub revision: SessionRevision,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayRunActivation {
    pub first_commit: u64,
    pub configuration: ArchivedRunActivation,
}

/// Each edge identifies an exact Run and input. The two later observations
/// advance independently in the committed source and target Threads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReplayAgentMessage {
    pub source_thread: ThreadId,
    pub source_run: RunId,
    pub target_thread: ThreadId,
    pub target_run: RunId,
    pub source: ReplayAgentMessageSource,
    pub target_first_commit: Option<u64>,
    pub target_input_commit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ReplayAgentMessageSource {
    Send {
        call_id: String,
        operation_id: String,
        requested_at: u64,
        receipt_at: Option<u64>,
    },
    Report {
        completed_at: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionReplayInputs {
    pub point: SessionReplayPoint,
    pub session: PersistedSession,
    pub threads: Vec<CommittedThreadPrefix>,
    pub activations: Vec<ReplayRunActivation>,
    pub messages: Vec<ReplayAgentMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionReplayError {
    #[error("Invalid Session diagnostic query")]
    InvalidQuery,
    #[error("Session or replay point was not found")]
    NotFound,
    #[error("Session replay history is unavailable: {0}")]
    HistoryUnavailable(String),
    #[error("Session replay storage is unavailable: {0}")]
    Unavailable(String),
}

#[async_trait::async_trait]
pub trait SessionReplayReads: Send + Sync {
    async fn contains_thread(
        &self,
        access: &SessionAccess,
        thread: &ThreadId,
    ) -> Result<bool, SessionReplayError>;

    async fn timeline(
        &self,
        access: &SessionAccess,
        query: &TimelineQuery,
        cursor: Option<&TimelineCursor>,
    ) -> Result<TimelinePage, SessionReplayError>;

    async fn run_evidence_at(
        &self,
        access: &SessionAccess,
        run: &RunId,
        snapshot: &TimelineSnapshot,
    ) -> Result<RunEvidence, SessionReplayError>;

    async fn load_at(
        &self,
        access: &SessionAccess,
        point: &ReplayPointId,
    ) -> Result<SessionReplayInputs, SessionReplayError>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TimelineSnapshot(pub String);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TimelineCursor(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Session,
    Dispatch,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimelineQuery {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub thread_id: Option<ThreadId>,
    pub run_id: Option<RunId>,
    pub operation_id: Option<String>,
    pub source: Option<EvidenceSource>,
    #[serde(default = "default_timeline_limit")]
    pub limit: usize,
}

fn default_timeline_limit() -> usize {
    50
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EvidenceRef {
    pub source: EvidenceSource,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TimelineEntry {
    pub evidence: EvidenceRef,
    pub recorded_at_ms: Option<i64>,
    pub thread_id: Option<ThreadId>,
    pub run_id: Option<RunId>,
    pub operation_id: Option<String>,
    pub kind: String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TimelinePage {
    pub entries: Vec<TimelineEntry>,
    pub snapshot: TimelineSnapshot,
    pub next_cursor: Option<TimelineCursor>,
}

/// The immutable activation selection. Runtime model calls remain separate
/// evidence, since fallback can select a different model from the default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunActivationEvidence {
    pub agent_id: String,
    pub agent_revision: Option<u64>,
    pub publication_fingerprint: String,
    pub selected_model: String,
    pub backend: RunBackendEvidence,
    pub resource_revision: Option<u64>,
    pub first_commit: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunBackendEvidence {
    Native,
    Acp { cli: String },
    A2a,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RunEvidence {
    pub run_id: RunId,
    pub thread_id: ThreadId,
    pub parent_run_id: Option<RunId>,
    pub session_revision: SessionRevision,
    pub snapshot: TimelineSnapshot,
    pub admission: Option<TimelineEntry>,
    pub activation: Option<RunActivationEvidence>,
    pub runtime: Option<TimelineEntry>,
    pub dispatch: Option<TimelineEntry>,
    pub messages: Vec<ReplayAgentMessage>,
    pub more_messages: bool,
}
