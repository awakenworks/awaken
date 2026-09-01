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

use awaken_run_ingress::WorkerIdentity;
use awaken_worker_transport_security::VerifiedWorkerContext;

use super::session_realization::{
    SessionCleanupPollReq, session_control, verify_terminal_cleanup_authority,
};
use super::{RealizationHttpError, WorkerDispatchService, respond_realization};

#[derive(Deserialize)]
pub(super) struct SessionRepositoryPublicationCompleteReq {
    identity: WorkerIdentity,
    session_id: String,
    lease: awaken_session_contract::SessionRealizationLease,
    receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
}

#[derive(Deserialize)]
pub(super) struct SessionRepositoryPublicationRejectReq {
    identity: WorkerIdentity,
    session_id: String,
    lease: awaken_session_contract::SessionRealizationLease,
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
        let projection = session_control(&service)
            .map_err(RealizationHttpError::from)?
            .terminal_repository_publication_command(&request.session_id, &request.lease)
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
        session_control(&service)
            .map_err(RealizationHttpError::from)?
            .record_terminal_repository_publication_receipt(
                &request.session_id,
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
        session_control(&service)
            .map_err(RealizationHttpError::from)?
            .record_terminal_repository_publication_rejection(
                &request.session_id,
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
