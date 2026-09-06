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
    pub batch_id: String,
    pub status: EventAdmissionStatus,
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
