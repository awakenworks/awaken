// @formal surface requirement=protocol.adapters.adapter_projection
// @formal reason: The close adapter delegates persisted ownership and exact-key/fingerprint reconciliation
// @formal reason: to SessionApplication and lowers its closed outcome enum to the native receipt. Existing
// @formal reason: adapter tests own executable route evidence; this classification does not claim a formal
// @formal reason: refinement of authentication, header parsing or transport.
use crate::common::scope::RequiredWorkspaceScope;
use std::sync::Arc;

use super::{WireErr, error_response, event_data_subject, parse_idempotency_key};
use crate::{
    state::{ManagedState, RunError, StateError},
    types::SendEventsRequest,
};
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};

/// Native-only final-close proof; no projection rebuild, Run activation, or
/// execution retry is performed. Ownership is checked before reading receipts.
pub async fn close_event_admission(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
    headers: HeaderMap,
    body: Result<Json<SendEventsRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<awaken_protocol_awaken::EventAdmissionReceipt>, WireErr> {
    let Json(req) = body.map_err(|rejection| {
        error_response(StateError::Run(RunError::bad_request(
            crate::routes::extractors::managed_json_message(rejection.body_text()),
        )))
    })?;
    // The application reads persisted ownership for live AND deleted Sessions;
    // a second warm-cache ownership check cannot be the deletion authority.
    let key = parse_idempotency_key(&headers)?.ok_or_else(|| {
        error_response(StateError::Run(RunError::bad_request(
            "event admission close requires Idempotency-Key",
        )))
    })?;
    let fingerprint =
        awaken_agent_contract::stable_fingerprint(&(&req, &event_data_subject(&headers)?));
    let admission = state
        .session_application()
        .close_session_event_admission(
            &owner_scope,
            &awaken_session_types::SessionId::from(&id),
            key.clone(),
            fingerprint,
        )
        .await
        .map_err(|error| {
            error_response(if error.code == "idempotency_conflict" {
                StateError::IdempotencyMismatch
            } else if error.code == "session_not_found" {
                StateError::NotFound
            } else {
                StateError::Run(error)
            })
        })?;
    use awaken_protocol_awaken::{EventAdmissionProof, EventAdmissionStatus};
    let (status, proof) = match admission {
        awaken_session_application_contract::SessionEventAdmission::Accepted {
            batch_id,
            operation_ids,
        } => (
            EventAdmissionStatus::Accepted,
            EventAdmissionProof::EventBatch {
                batch_id,
                event_ids: operation_ids.map(|ids| {
                    ids.iter()
                        .map(|operation_id| {
                            crate::state::durable_inbound_event_id(&id, operation_id)
                        })
                        .collect()
                }),
            },
        ),
        awaken_session_application_contract::SessionEventAdmission::Closed { batch_id } => (
            EventAdmissionStatus::Closed,
            EventAdmissionProof::EventBatch {
                batch_id,
                event_ids: Some(vec![]),
            },
        ),
        awaken_session_application_contract::SessionEventAdmission::Deleted {
            deleted_revision,
        } => (
            EventAdmissionStatus::Closed,
            EventAdmissionProof::SessionDeleted {
                deleted_revision: deleted_revision.0,
            },
        ),
    };
    Ok(Json(awaken_protocol_awaken::EventAdmissionReceipt {
        session_id: id,
        idempotency_key: key,
        status,
        proof,
    }))
}
