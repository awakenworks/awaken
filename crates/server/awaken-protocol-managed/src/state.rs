//! Adapter state: the session store, id minting, and the `SessionRuntime` port.
//!
//! The adapter drives one runtime seam and owns no kernel construction. The
//! server implements [`SessionRuntime`] over the runtime; tests implement it with
//! a fake. Public ids (`sesn_*`, `evt_*`) are minted here; tool-use ids are stable
//! Thread/source-qualified encodings that a user reply can safely echo and the
//! command boundary can reverse to the Runtime's batch-local call id.

use std::sync::Arc;
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
use awaken_session_repository_contract::SessionRepository;
use awaken_session_state_contract::PersistedSession;
#[cfg(test)]
use awaken_session_state_contract::SessionDisposition;
#[cfg(test)]
use awaken_session_state_contract::{ManagedLifecycleFact, SessionExecutionState};

mod application;
pub(crate) use application::agent_mcp_candidate;
mod constants;
mod deployment_sessions;
mod error;
mod events;
pub(crate) use events::{
    CommittedEventDelivery, ProjectionObservationDecision, managed_assistant_event_id,
};
mod helpers;
mod managed_state;
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
mod work_dispatch;
pub(crate) mod work_session_access;

pub use error::StateError;
pub use managed_state::ManagedState;
pub(crate) use managed_state::SessionApplicationRoles;

#[cfg(test)]
use awaken_session_application_contract::StepOutcome;
use awaken_session_budget_contract::SessionUsage;
use awaken_session_configuration_contract::AgentCapabilities;
use awaken_session_execution_contract::CommittedOutcomeProjection;
#[cfg(test)]
use awaken_session_execution_contract::SessionRuntime;
#[cfg(test)]
use awaken_session_execution_contract::{OutcomeDrive, OutcomeFailure, OutcomeReport};
use awaken_session_types::RunError;
pub use constants::COMMITTED_PREFIX_READ_CONFLICT;
pub(crate) use constants::LEGACY_UNRECORDED_AT;
pub(crate) use helpers::{
    durable_inbound_event_id, is_transient_event_id, rubric_text, session_thread_usage_value,
    session_usage_value,
};
pub(crate) use resource::{resolved_resource_dto, resource_binding_id};
use session_record::{
    ProjectionPublishDecision, RootTimeEvidence, SessionRecord, ThreadTimeEvidence,
    decide_projection_publish,
};
pub(crate) use session_record::{
    RootTimeEvidence as SessionRootTimeEvidence, ThreadTimeEvidence as SessionThreadTimeEvidence,
};

#[cfg(test)]
mod tests;

pub(crate) use resources::observe_resource_manifest;
