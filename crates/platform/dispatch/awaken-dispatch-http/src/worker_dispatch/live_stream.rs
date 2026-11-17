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
        .lock_commit_epoch(claim)
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
    awaken_dispatch_contract::with_preview_claim_epoch(claim, async {
        for observation in observations {
            let _ = service.stream_sink.send_observation(observation).await;
        }
    })
    .await;
    // Keep the exact epoch guarded through publication, as on the single path.
    drop(guard);
    Ok(json!({ "accepted": true }))
}
