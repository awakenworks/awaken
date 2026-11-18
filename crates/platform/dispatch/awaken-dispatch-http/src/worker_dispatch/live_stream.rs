// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: Publishes ephemeral observations only under the current Worker identity and dispatch epoch.
//! Authenticated, epoch-fenced publication of ephemeral Worker observations.

use awaken_agent_contract::event::{AgentEvent, classify};
use awaken_agent_contract::stream::event::Observation;
use awaken_dispatch_contract::{
    MAX_STREAM_PREVIEW_BATCH_ITEMS, StreamObservationRequest, StreamPreviewBatchRequest,
};

use super::{
    ApplicationError, Extension, Json, RunClaim, State, StatusCode, Value, VerifiedWorkerContext,
    WorkerDispatchService, WorkerIdentity, claim_authority, json, respond,
};
use std::sync::Arc;

pub(super) async fn stream_event(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<StreamObservationRequest>,
) -> (StatusCode, Json<Value>) {
    let request_identity = request.request.identity;
    let claim = request.request.claim;
    let observation = Observation {
        event: request.request.event,
        assistant_response: request.assistant_response,
    };
    respond(publish(&service, &worker, &request_identity, &claim, [observation]).await)
}

pub(super) async fn stream_preview_batch(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<StreamPreviewBatchRequest>,
) -> (StatusCode, Json<Value>) {
    if request.observations.is_empty()
        || request.observations.len() > MAX_STREAM_PREVIEW_BATCH_ITEMS
    {
        return respond(Err(ApplicationError::invalid(
            "invalid Worker preview batch size",
        )));
    }
    if request
        .observations
        .iter()
        .any(|item| !matches!(item.event.kind, AgentEvent::Delta(_)))
    {
        return respond(Err(ApplicationError::invalid(
            "Worker preview batches accept only deltas",
        )));
    }
    respond(
        publish(
            &service,
            &worker,
            &request.identity,
            &request.claim,
            request.observations,
        )
        .await,
    )
}

async fn publish(
    service: &WorkerDispatchService,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    claim: &RunClaim,
    observations: impl AsRef<[Observation]> + IntoIterator<Item = Observation>,
) -> Result<Value, ApplicationError> {
    let authority = claim_authority(service, worker, Some(identity), false).await?;
    if claim.owner != authority.owner
        || observations
            .as_ref()
            .iter()
            .any(|item| item.event.run_id != claim.run_id)
    {
        return Err(ApplicationError::invalid(
            "live Worker event does not match its authenticated claim",
        ));
    }
    if observations
        .as_ref()
        .iter()
        .any(|item| !classify(&item.event.kind).live)
    {
        return Err(ApplicationError::invalid(
            "Worker transport accepts only live-classified Agent events",
        ));
    }
    let current = service
        .dispatch
        .acquire_commit_authority(claim)
        .await
        .map_err(|error| ApplicationError::internal(error.to_string()))?;
    let Some(guard) = current else {
        return Ok(json!({ "accepted": false }));
    };
    let committed_thread = guard.request().thread_id();
    if observations.as_ref().iter().any(|item| {
        item.assistant_response
            .as_ref()
            .is_some_and(|coordinate| &coordinate.thread_id != committed_thread)
    }) {
        return Err(ApplicationError::invalid(
            "live Worker event does not match its claim's logical Thread",
        ));
    }
    if matches!(
        &service.claim_persistence,
        crate::WorkerClaimPersistence::Object(_)
    ) {
        guard
            .release()
            .await
            .map_err(ApplicationError::unavailable)?;
        return Ok(publish_preview(service, claim, observations).await);
    }
    let guard = guard.into_locked().map_err(ApplicationError::unavailable)?;
    let accepted = publish_preview(service, claim, observations).await;
    drop(guard);
    Ok(accepted)
}

async fn publish_preview(
    service: &WorkerDispatchService,
    claim: &RunClaim,
    observations: impl IntoIterator<Item = Observation>,
) -> Value {
    awaken_dispatch_contract::with_preview_claim_epoch(claim, async {
        for observation in observations {
            let _ = service.stream_sink.send_observation(observation).await;
        }
    })
    .await;
    json!({ "accepted": true })
}

pub(super) async fn append_progress_batch(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<awaken_dispatch_contract::AppendProgressRequest>,
) -> (StatusCode, Json<Value>) {
    use awaken_dispatch_contract::ProgressError;
    let authority = match claim_authority(&service, &worker, Some(&request.identity), false).await {
        Ok(authority) => authority,
        Err(error) => return respond(Err(error)),
    };
    if request.claim.owner != authority.owner {
        return respond(Err(ApplicationError::invalid(
            "progress does not match its authenticated claim",
        )));
    }
    let crate::WorkerClaimPersistence::Object(persistence) = &service.claim_persistence else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "durable progress is unsupported" })),
        );
    };
    match persistence
        .progress
        .append_progress_batch(&request.claim, request.batch)
        .await
    {
        Ok(receipt) => respond(Ok(json!(receipt))),
        Err(ProgressError::Unsupported) => (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "durable progress is unsupported" })),
        ),
        Err(ProgressError::Invalid) => {
            respond(Err(ApplicationError::invalid("invalid progress batch")))
        }
        Err(ProgressError::Fenced | ProgressError::Conflict) => respond(Err(
            ApplicationError::conflict("progress claim or batch conflicts with committed progress"),
        )),
        Err(ProgressError::Unavailable) => respond(Err(ApplicationError::unavailable(
            "durable progress outcome is unavailable",
        ))),
    }
}
