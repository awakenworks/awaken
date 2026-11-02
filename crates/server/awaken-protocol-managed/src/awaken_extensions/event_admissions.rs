// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: The native close router and DTO project the existing Session admission outcome,
// @formal reason: including deletion and optional legacy observation coordinates; they do not submit
// @formal reason: events or execute work. This is a protocol product boundary, not a direct proof of route
// @formal reason: installation, HTTP delivery or deletion retention.
//! Final non-admission proof for response-loss reconciliation. Ordinary event
//! submission remains exclusively on the Anthropic Managed Agents surface.

use axum::{Router, handler::Handler, routing::post};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventAdmissionStatus {
    Accepted,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventAdmissionReceipt {
    pub session_id: String,
    pub idempotency_key: String,
    pub status: EventAdmissionStatus,
    #[serde(flatten)]
    pub proof: EventAdmissionProof,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "proof", rename_all = "snake_case")]
pub enum EventAdmissionProof {
    #[serde(rename = "event_noop")]
    EventNoOp {
        event_ids: [String; 0],
    },
    EventBatch {
        batch_id: String,
        /// Exact standard Managed event ids; omitted only for legacy retained
        /// receipts. These prove admission identity, not execution completion.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event_ids: Option<Vec<String>>,
    },
    SessionDeleted {
        deleted_revision: u64,
    },
}

/// The caller supplies the exact original Managed events body and retry key.
/// Acceptance and final close compete on the existing Session batch identity.
pub fn session_event_admission_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route(
        "/v1/awaken/sessions/{id}/event-admissions/close",
        post(handler),
    )
}
