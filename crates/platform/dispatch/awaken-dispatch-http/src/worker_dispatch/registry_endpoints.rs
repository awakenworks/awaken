// @formal entrypoint worker.incarnation_generation_fences_stale_processes
//! Authenticated Worker Registry commands through the existing Directory.
//! The parent router retains authentication, lease policy and physical-domain
//! composition; these handlers introduce no registry or lifecycle authority.

use super::{
    ApplicationError, Arc, Extension, HeartbeatWorkerReq, Json, RegisterWorkerReq, State,
    StatusCode, Value, VerifiedWorkerContext, WorkerDispatchService, WorkerIdentityReq, directory,
    json, registry_commands, release_closed_idle_worker_evidence, respond, verify_worker_id,
    verify_worker_identity,
};
use awaken_time::WallClockExt;
use awaken_worker_contract::{
    CURRENT_RUNTIME_PROTOCOL_VERSION, RegistrationCredential, RegistryMutation,
};
use tracing::Instrument;

pub(super) async fn register_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<RegisterWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.registration.worker_id)?;
        let credential = if worker.identity().is_some() {
            RegistrationCredential::Incarnation
        } else {
            RegistrationCredential::Bootstrap
        };
        let record = registry_commands(&service)?
            .register(awaken_dispatch::RegisterWorker {
                registration: request.registration,
                credential,
                provider_identified: worker.requires_provider_incarnation(),
            })
            .await?;
        Ok(json!({
            "worker": record,
            "lease_ttl_ms": service.registry_ttl_ms,
            "runtime_protocol_version": CURRENT_RUNTIME_PROTOCOL_VERSION,
        }))
    }
    .await;
    respond(result)
}

pub(super) async fn heartbeat_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<HeartbeatWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_identity(&worker, &request.identity).map_err(ApplicationError::invalid)?;
        let mutation = registry_commands(&service)?
            .heartbeat(
                &request.identity,
                request.heartbeat,
                service.session_work.as_deref(),
            )
            .await?;
        let lease_ttl_ms =
            (mutation == RegistryMutation::Applied).then_some(service.registry_ttl_ms);
        Ok(json!({
            "mutation": mutation,
            "lease_ttl_ms": lease_ttl_ms,
        }))
    }
    .await;
    respond(result)
}

pub(super) async fn drain_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<WorkerIdentityReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_identity(&worker, &request.identity).map_err(ApplicationError::invalid)?;
        let deadline = request.deadline_ms.unwrap_or_else(|| {
            service
                .clock
                .now_ms()
                .saturating_add(service.registry_ttl_ms)
        });
        let mutation = directory(&service)?
            .begin_drain(&request.identity, deadline)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
}

pub(super) async fn quiesce_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<WorkerIdentityReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_identity(&worker, &request.identity).map_err(ApplicationError::invalid)?;
        let mutation = directory(&service)?
            .mark_quiesced(&request.identity)
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        if mutation == RegistryMutation::Applied {
            release_closed_idle_worker_evidence(&service, &request.identity).await?;
        }
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
}

pub(super) async fn deregister_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<WorkerIdentityReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_identity(&worker, &request.identity).map_err(ApplicationError::invalid)?;
        if let Some(session_work) = service.session_work.as_ref() {
            session_work
                .release_worker_session_work(&request.identity.lease_owner())
                .instrument(tracing::info_span!("worker.session_work.release"))
                .await
                .map_err(|error| ApplicationError::internal(error.to_string()))?;
        }
        let mutation = directory(&service)?
            .deregister(&request.identity)
            .instrument(tracing::info_span!("worker.registry.deregister"))
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        if mutation == RegistryMutation::Applied {
            release_closed_idle_worker_evidence(&service, &request.identity).await?;
        }
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
}
