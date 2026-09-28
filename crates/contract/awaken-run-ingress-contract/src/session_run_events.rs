//! Read-only composition of native Run truth and delivery facts.
//! Neither stream changes ownership or becomes a second persisted Run status.

use awaken_agent_contract::RunLifecyclePage;
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::run::RunState;
use serde::{Deserialize, Serialize};

use crate::DispatchPage;

/// One exact primary Run, with independently committed delivery/lifecycle
/// cursors. State, ticket, output and usage share one recovery prefix read
/// after dispatch observation. A consumer must retain the delivery settlement
/// fence; `state` alone cannot authorize downstream completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRunEventPage {
    pub session_id: String,
    pub run_id: RunId,
    /// Absent before the first committed Run prefix. A claim is delivery truth,
    /// and must never be promoted to a fabricated committed Running state.
    pub thread_version: Option<u64>,
    pub state: Option<RunState>,
    pub dispatch: DispatchPage,
    pub lifecycle: RunLifecyclePage,
    pub resume_ticket: Option<ResumeTicket>,
    pub response: String,
    /// Cumulative committed Thread usage, never an invented per-Run allocation.
    pub usage: Option<serde_json::Value>,
}
