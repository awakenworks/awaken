// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: registered-Worker HTTP anti-corruption projection for the existing
// @formal reason: terminal Repository publication control. Dynamic path: the authenticated exact
// @formal reason: realization lease gates command polling and mutually exclusive receipt/rejection
// @formal reason: recording through the canonical SessionRealizationControl root CAS; this module owns no
// @formal reason: Session state, queue, retry, or Git effect. Evidence: session.lifecycle.session_state,
// @formal reason: ADR-0075, and signed Worker transport tests; this is a product-requirement
// @formal reason: classification, not a direct formal-proof claim.
//! Registered-Worker HTTP projection for one terminal Repository publication.
//!
//! The parent dispatch router owns authentication and the existing realization
//! control. These handlers only forward the aggregate-derived command and its
//! mutually exclusive receipt/rejection outcome; they own no queue or Session
//! state.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_dispatch::WorkerIdentity;
use awaken_worker_transport_security::VerifiedWorkerContext;

use super::session_realization::{terminal_cleanup_control, verify_terminal_cleanup_authority};
use super::session_requests::SessionCleanupPollReq;
use super::{RealizationHttpError, WorkerDispatchService, respond_realization};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionRepositoryPublicationCompleteReq {
    identity: WorkerIdentity,
    session_id: String,
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
    receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionRepositoryPublicationRejectReq {
    identity: WorkerIdentity,
    session_id: String,
    lease: awaken_session_configuration_contract::resource::SessionRealizationLease,
    rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
}

pub(super) async fn session_repository_publication_poll(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionCleanupPollReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(&service, &worker, &request.identity, &request.lease)
            .await?;
        let projection = terminal_cleanup_control(&service)
            .map_err(RealizationHttpError::from)?
            .terminal_repository_publication_command(
                &awaken_session_types::SessionId::from(request.session_id),
                &request.lease,
            )
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "projection": projection }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn session_repository_publication_complete(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionRepositoryPublicationCompleteReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(&service, &worker, &request.identity, &request.lease)
            .await?;
        terminal_cleanup_control(&service)
            .map_err(RealizationHttpError::from)?
            .record_terminal_repository_publication_receipt(
                &awaken_session_types::SessionId::from(request.session_id),
                &request.lease,
                request.receipt,
            )
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "recorded": true }))
    }
    .await;
    respond_realization(result)
}

pub(super) async fn session_repository_publication_reject(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<SessionRepositoryPublicationRejectReq>,
) -> (StatusCode, Json<Value>) {
    let result: Result<Value, RealizationHttpError> = async {
        verify_terminal_cleanup_authority(&service, &worker, &request.identity, &request.lease)
            .await?;
        terminal_cleanup_control(&service)
            .map_err(RealizationHttpError::from)?
            .record_terminal_repository_publication_rejection(
                &awaken_session_types::SessionId::from(request.session_id),
                &request.lease,
                request.rejection,
            )
            .await
            .map_err(RealizationHttpError::from)?;
        Ok(json!({ "recorded": true }))
    }
    .await;
    respond_realization(result)
}
