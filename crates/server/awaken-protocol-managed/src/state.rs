//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here; a tool-use event keeps
//! the tool call's own id so a `user.tool_confirmation` can reference it.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::page::paginate_by_id;

use crate::preview::{PreviewAllocations, PreviewSink};
use crate::project::{self, project_messages, project_messages_with_mcp_ids, project_step};
use crate::types::{
    ConfirmResult, Event, EventReceipt, InboundEvent, ListEventsResponse, ModelConfig,
    ModelOverride, OutboundKind, SendEventsRequest, SendEventsResponse, Session, SessionAgent,
    SessionCreateParams, SessionError, SessionStats, SessionStatus, SessionThread,
    SessionThreadAgent, SessionThreadStatus, StopReason, StreamFrame, Usage,
};
#[cfg(test)]
use awaken_session_contract::ManagedSessionRepository;
use awaken_session_contract::{ManagedLifecycleFact, PersistedSession, SessionExecutionState};
#[cfg(test)]
use awaken_session_contract::{SessionDisposition, SessionInit};
#[cfg(test)]
use awaken_session_store::SqliteManagedSessionRepository;

mod application;
pub(crate) use application::agent_mcp_candidate;
mod constants;
mod deployment_sessions;
mod environment;
mod error;
mod events;
mod helpers;
#[path = "state/lifecycle_event.rs"]
pub mod lifecycle_event;
mod managed_state;
#[cfg(any(test, feature = "test-support"))]
mod mcp_attachment;
mod realization;
mod rehydration;
mod resource;
mod resources;
mod session_create_idempotency;
mod session_mcp_projection;
mod session_record;
mod session_service;
mod session_update;
mod sessions;
#[cfg(test)]
mod test_support;
mod threads;
mod types;
mod work_dispatch;

pub use error::StateError;
pub use managed_state::ManagedState;

pub(crate) use constants::{DEFAULT_SCOPE, MEMORY_CREATE_ONLY, PROCESSED_AT};
pub(crate) use helpers::{content_text, lifecycle_fact, rubric_text, session_usage_value};
pub(crate) use resource::{
    ParsedInputTarget, ParsedSessionInput, input_binding, resolved_resource_dto,
    resource_binding_id,
};
use session_record::SessionRecord;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use types::SessionRuntime;
pub(crate) use types::{
    AgentCapabilities, CustomTool, DelegatedRun, OutcomeIteration, OutcomeReport, RunError,
    RunErrorKind, SessionUsage, StepOutcome, ToolPermissionDecision,
};

#[cfg(test)]
mod tests;
