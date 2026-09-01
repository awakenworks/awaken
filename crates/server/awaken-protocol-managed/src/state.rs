//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here; tool-use ids are stable
//! Thread/source-qualified encodings that a user reply can safely echo and the
//! command boundary can reverse to the Runtime's batch-local call id.

use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use tokio::sync::broadcast;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::page::paginate_by_id;

use crate::project::{
    self, ProjectedEvent, decode_managed_tool_event_id, managed_tool_event_id,
    project_messages_with_mcp_ids,
};
use crate::types::{
    ConfirmResult, Event, InboundEvent, ListEventsResponse, ModelConfig, ModelOverride,
    OutboundKind, OutcomeRubric, SendEventsRequest, SendEventsResponse, Session, SessionAgent,
    SessionCreateParams, SessionError, SessionStats, SessionStatus, SessionThread,
    SessionThreadAgent, SessionThreadStatus, StopReason, Usage,
};
#[cfg(test)]
use awaken_session_contract::ManagedSessionRepository;
#[cfg(test)]
use awaken_session_contract::SessionDisposition;
use awaken_session_contract::{ManagedLifecycleFact, PersistedSession, SessionExecutionState};
#[cfg(test)]
use awaken_session_store::SqliteManagedSessionRepository;

mod application;
pub(crate) use application::agent_mcp_candidate;
mod constants;
mod deployment_sessions;
mod environment;
mod error;
mod events;
pub(crate) use events::managed_assistant_event_id;
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
pub use session_create_idempotency::managed_session_id_from_idempotency;
mod session_mcp_projection;
mod session_record;
mod session_service;
mod session_update;
mod sessions;
#[cfg(test)]
pub(crate) mod test_support;
mod threads;
pub(crate) use threads::{internal_thread_id, public_thread_id};
mod types;
mod vault_rollout;
mod work_dispatch;
pub(crate) mod work_session_access;

pub use error::StateError;
pub use managed_state::ManagedState;

pub(crate) use constants::{DEFAULT_SCOPE, MEMORY_CREATE_ONLY, PROCESSED_AT};
pub(crate) use helpers::{
    content_text, durable_inbound_event_id, is_transient_event_id, lifecycle_fact, rubric_text,
    session_thread_usage_value, session_usage_value,
};
pub(crate) use resource::{
    ParsedInputTarget, ParsedSessionInput, input_binding, resolved_resource_dto,
    resource_binding_id,
};
use session_record::{ProjectionPublishDecision, SessionRecord, decide_projection_publish};
#[cfg(test)]
pub(crate) use types::DelegatedRun;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use types::SessionRuntime;
pub(crate) use types::{
    AgentCapabilities, CommittedOutcomeProjection, CustomTool, OutcomeIteration, RunError,
    RunErrorKind, SessionUsage,
};
#[cfg(test)]
pub(crate) use types::{OutcomeDrive, OutcomeFailure, OutcomeReport, StepOutcome};

#[cfg(test)]
mod tests;
