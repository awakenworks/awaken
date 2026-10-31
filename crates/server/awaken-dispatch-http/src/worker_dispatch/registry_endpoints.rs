// @formal entrypoint worker.incarnation_generation_fences_stale_processes
//! Authenticated Worker Registry commands through the existing Directory.
//! The parent router retains authentication, lease policy and physical-domain
//! composition; these handlers introduce no registry or lifecycle authority.

use super::{
    ApplicationError, Arc, Extension, HeartbeatWorkerReq, Json, RegisterWorkerReq, State,
    StatusCode, Value, VerifiedWorkerContext, WorkerDispatchService, WorkerIdentityReq, directory,
    json, release_closed_idle_worker_evidence, respond, settle_previous_incarnation,
    verify_worker_id, verify_worker_identity,
};
use awaken_time::WallClockExt;
use tracing::Instrument;

fn heartbeat_changes_placement(
    previous: Option<&awaken_dispatch::RegisteredWorker>,
    heartbeat: &awaken_dispatch::WorkerHeartbeat,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    let snapshot = &previous.snapshot;
    snapshot.state.accepts_work() != heartbeat.ready
        || snapshot.in_flight != heartbeat.in_flight
        || snapshot.warm_environment_shapes != heartbeat.warm_environment_shapes
        || snapshot.credential_observations != heartbeat.credential_observations
        || snapshot.acp_capability_observations != heartbeat.acp_capability_observations
}

pub(super) async fn register_worker(
    State(service): State<Arc<WorkerDispatchService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(mut request): Json<RegisterWorkerReq>,
) -> (StatusCode, Json<Value>) {
    let result = async {
        verify_worker_id(&worker, &request.registration.worker_id)?;
        if worker.identity().is_some() {
            return Err(ApplicationError::invalid(
                "worker registration requires a bootstrap credential",
            ));
        }
        if !request
            .registration
            .manifest
            .supports_current_runtime_protocol()
        {
            return Err(ApplicationError::invalid(
                "Worker manifest does not explicitly support the current runtime protocol",
            ));
        }
        if let Some(source) = &service.execution_bindings
            && let Some(incarnation) = source
                .registration_incarnation(&request.registration)
                .await?
        {
            // The provider identifies the process (a Kubernetes container
            // instance); a process-proposed incarnation is never trusted. A
            // different current incarnation of this Worker ID is a restarted
            // process: settle it first from its retained exact evidence, or
            // leave it Unknown, before the Registry may admit the new one.
            let previous = directory(&service)?
                .current(worker.worker_id())
                .await
                .map_err(|error| ApplicationError::unavailable(error.to_string()))?
                .filter(|current| current.snapshot.identity.incarnation_id != incarnation);
            if let Some(previous) = previous {
                settle_previous_incarnation(&service, &previous.snapshot.identity).await?;
            }
            request.registration.incarnation_id = incarnation;
        }
        if let Some(source) = &service.execution_bindings {
            request.registration.create_ledger = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                source.prepare_registration_physical_evidence(&request.registration),
            )
            .await
            .map_err(|_| {
                ApplicationError::unavailable("Worker physical evidence preparation timed out")
            })??;
        }
        let record = match directory(&service)?
            .register(
                request.registration,
                service.clock.now_ms(),
                service.registry_ttl_ms,
            )
            .await
        {
            Ok(record) => record,
            Err(awaken_dispatch::RegistryError::SlotOccupied {
                worker_id,
                generation,
            }) => {
                return Err(ApplicationError::conflict(format!(
                    "worker slot {worker_id} is occupied by generation {generation}"
                )));
            }
            Err(error) => return Err(ApplicationError::invalid(error.to_string())),
        };
        if record.snapshot.state == awaken_dispatch::WorkerState::Starting
            && let Some(source) = &service.execution_bindings
        {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                source.retain_registered_worker_evidence(
                    &record.snapshot.identity,
                    record.create_ledger.as_ref(),
                ),
            )
            .await
            .map_err(|_| ApplicationError::unavailable("Worker evidence retention timed out"))??;
            // Provider I/O can outlive closure. Re-read durable truth before
            // replying, then converge retention if admission closed meanwhile.
            release_closed_idle_worker_evidence(&service, &record.snapshot.identity).await?;
        }
        let identity = record.snapshot.identity.clone();
        let record = directory(&service)?
            .current(&identity.worker_id)
            .await
            .map_err(|e| ApplicationError::unavailable(e.to_string()))?
            .filter(|current| current.snapshot.identity == identity)
            .ok_or_else(|| ApplicationError::unavailable("registered Worker disappeared"))?;
        Ok(json!({
            "worker": record,
            "lease_ttl_ms": service.registry_ttl_ms,
            "runtime_protocol_version": awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION,
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
        let worker_directory = directory(&service)?;
        // Placement evidence is read only to decide whether to nudge the one
        // existing Session lifecycle supervisor. The registry mutation below
        // remains the authority; a rejected heartbeat never publishes a hint.
        let previous = worker_directory
            .current(&request.identity.worker_id)
            .instrument(tracing::info_span!("worker.registry.current"))
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        // The immutable incarnation manifest cannot gain protocol support by
        // heartbeat. Old incarnations may still drain/deregister, but cannot
        // extend authority indefinitely across a Control upgrade. A stale
        // identity keeps the registry's normal non-mutating rejection below.
        if previous.as_ref().is_some_and(|record| {
            record.snapshot.identity == request.identity
                && !record.snapshot.manifest.supports_current_runtime_protocol()
        }) {
            return Err(ApplicationError::invalid(
                "Worker manifest does not explicitly support the current runtime protocol",
            ));
        }
        let placement_changed = previous.as_ref().is_some_and(|previous| {
            heartbeat_changes_placement(Some(previous), &request.heartbeat)
        });
        let mutation = worker_directory
            .heartbeat(
                &request.identity,
                request.heartbeat,
                service.clock.now_ms(),
                service.registry_ttl_ms,
            )
            .instrument(tracing::info_span!("worker.registry.heartbeat"))
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        if mutation == awaken_dispatch::RegistryMutation::Applied
            && placement_changed
            && let Some(session_work) = service.session_work.as_ref()
        {
            tracing::debug!(
                worker_id = %request.identity.worker_id,
                "Worker placement evidence changed; waking Session lifecycle reconciliation"
            );
            session_work.request_lifecycle_reconciliation();
        }
        let lease_ttl_ms = (mutation == awaken_dispatch::RegistryMutation::Applied)
            .then_some(service.registry_ttl_ms);
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
        if mutation == awaken_dispatch::RegistryMutation::Applied {
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
        if mutation == awaken_dispatch::RegistryMutation::Applied {
            release_closed_idle_worker_evidence(&service, &request.identity).await?;
        }
        Ok(json!({ "mutation": mutation }))
    }
    .await;
    respond(result)
}
