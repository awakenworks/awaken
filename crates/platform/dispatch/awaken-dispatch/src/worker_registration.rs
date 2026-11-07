// @formal entrypoint worker.incarnation_generation_fences_stale_processes
//! Worker Registry commands of the Coordinator application.
//!
//! The Registry transitions decide admission (protocol floor, incarnation
//! history, slot replacement). This module orders the provider effects around
//! them: request admission before any provider I/O, the provider-observed
//! incarnation, settlement of a replaced incarnation, physical evidence and its
//! retention. Transport authentication and response shapes stay in adapters.

use std::time::Duration;

use awaken_dispatch_execution::PhysicalAttemptReads;
use awaken_session_application_contract::SessionWorkerLeaseAuthority;
use awaken_session_execution_contract::ApplicationError;
use awaken_time::{WallClock, WallClockExt};
use awaken_worker_contract::{
    RegisteredWorker, RegistrationCredential, RegistrationIncarnation, RegistryError,
    RegistryMutation, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerRegistration,
    WorkerState, registration_incarnation_admission, worker_heartbeat_protocol_admission,
    worker_registration_request_admission,
};
use tracing::Instrument;

use crate::application::{
    WorkerExecutionBindingSource, WorkerExecutionStopSource, reconcile_closed_idle_worker_evidence,
    release_closed_idle_worker_evidence,
};

/// Bound on each provider call made while a Worker registers.
const PROVIDER_EFFECT_TIMEOUT: Duration = Duration::from_secs(5);

/// One registration request as the authenticated transport presented it.
#[derive(Debug, Clone)]
pub struct RegisterWorker {
    pub registration: WorkerRegistration,
    pub credential: RegistrationCredential,
    /// The authenticated execution domain is identified by its provider (an
    /// authenticated Kubernetes Pod): only the observed incarnation is admitted.
    pub provider_identified: bool,
}

/// The Coordinator's Worker Registry commands over its existing ports.
pub struct WorkerRegistryCommands<'a> {
    pub directory: &'a dyn WorkerDirectory,
    pub dispatch: &'a dyn PhysicalAttemptReads,
    pub bindings: Option<&'a dyn WorkerExecutionBindingSource>,
    pub stops: Option<&'a dyn WorkerExecutionStopSource>,
    pub clock: &'a dyn WallClock,
    pub ttl_ms: u64,
}

fn refused(error: RegistryError) -> ApplicationError {
    ApplicationError::invalid(error.to_string())
}

impl WorkerRegistryCommands<'_> {
    /// Register one Worker incarnation and return its current durable record.
    pub async fn register(
        &self,
        command: RegisterWorker,
    ) -> Result<RegisteredWorker, ApplicationError> {
        let RegisterWorker {
            mut registration,
            credential,
            provider_identified,
        } = command;
        // Refuse before any provider effect: a refused request must neither
        // observe nor settle another incarnation.
        worker_registration_request_admission(credential, &registration.manifest)
            .map_err(refused)?;
        let observed = match self.bindings {
            Some(source) => source.registration_incarnation(&registration).await?,
            None => None,
        };
        match registration_incarnation_admission(observed, provider_identified).map_err(refused)? {
            RegistrationIncarnation::Proposed => {}
            RegistrationIncarnation::ProviderObserved(incarnation) => {
                // A different current incarnation of this Worker ID is a
                // restarted process: settle it first from its retained exact
                // evidence, or leave it Unknown, before the Registry may admit
                // the new one.
                let previous = self
                    .directory
                    .current(&registration.worker_id)
                    .await
                    .map_err(|error| ApplicationError::unavailable(error.to_string()))?
                    .filter(|current| current.snapshot.identity.incarnation_id != incarnation);
                if let Some(previous) = previous {
                    self.settle_previous_incarnation(&previous.snapshot.identity)
                        .await?;
                }
                registration.incarnation_id = incarnation;
            }
        }
        if let Some(source) = self.bindings {
            registration.create_ledger = tokio::time::timeout(
                PROVIDER_EFFECT_TIMEOUT,
                source.prepare_registration_physical_evidence(&registration),
            )
            .await
            .map_err(|_| {
                ApplicationError::unavailable("Worker physical evidence preparation timed out")
            })??;
        }
        let record = match self
            .directory
            .register(registration, self.clock.now_ms(), self.ttl_ms)
            .await
        {
            Ok(record) => record,
            Err(RegistryError::SlotOccupied {
                worker_id,
                generation,
            }) => {
                return Err(ApplicationError::conflict(format!(
                    "worker slot {worker_id} is occupied by generation {generation}"
                )));
            }
            Err(error) => return Err(refused(error)),
        };
        if record.snapshot.state == WorkerState::Starting
            && let Some(source) = self.bindings
        {
            tokio::time::timeout(
                PROVIDER_EFFECT_TIMEOUT,
                source.retain_registered_worker_evidence(
                    &record.snapshot.identity,
                    record.create_ledger.as_ref(),
                ),
            )
            .await
            .map_err(|_| ApplicationError::unavailable("Worker evidence retention timed out"))??;
            // Provider I/O can outlive closure. Re-read durable truth before
            // replying, then converge retention if admission closed meanwhile.
            self.release_closed_idle_evidence(&record.snapshot.identity)
                .await?;
        }
        let identity = record.snapshot.identity;
        self.directory
            .current(&identity.worker_id)
            .await
            .map_err(|error| ApplicationError::unavailable(error.to_string()))?
            .filter(|current| current.snapshot.identity == identity)
            .ok_or_else(|| ApplicationError::unavailable("registered Worker disappeared"))
    }

    /// Renew one exact incarnation. A changed placement input wakes the one
    /// Session lifecycle supervisor; the Registry mutation stays the authority
    /// and a rejected heartbeat never publishes that hint.
    pub async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        session_work: &dyn SessionWorkerLeaseAuthority,
    ) -> Result<RegistryMutation, ApplicationError> {
        let previous = self
            .directory
            .current(&identity.worker_id)
            .instrument(tracing::info_span!("worker.registry.current"))
            .await
            .map_err(|error| ApplicationError::internal(error.to_string()))?;
        // The Registry transition repeats this inside its slot transaction;
        // judging the read first keeps the refusal independent of the store.
        worker_heartbeat_protocol_admission(previous.as_ref(), identity).map_err(refused)?;
        let placement_changed = previous
            .as_ref()
            .is_some_and(|previous| previous.snapshot.placement_changed_by(&heartbeat));
        let mutation = self
            .directory
            .heartbeat(identity, heartbeat, self.clock.now_ms(), self.ttl_ms)
            .instrument(tracing::info_span!("worker.registry.heartbeat"))
            .await
            .map_err(|error| match error {
                RegistryError::UnsupportedRuntimeProtocol => refused(error),
                error => ApplicationError::internal(error.to_string()),
            })?;
        if mutation == RegistryMutation::Applied && placement_changed {
            tracing::debug!(
                worker_id = %identity.worker_id,
                "Worker placement evidence changed; waking Session lifecycle reconciliation"
            );
            session_work.request_lifecycle_reconciliation();
        }
        Ok(mutation)
    }

    /// Closure and final physical admission share the Registry exclusion. A
    /// closed exact incarnation cannot acquire another slot after this read.
    pub async fn release_closed_idle_evidence(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<(), ApplicationError> {
        let Some(source) = self.stops else {
            return Ok(());
        };
        tokio::time::timeout(
            PROVIDER_EFFECT_TIMEOUT,
            release_closed_idle_worker_evidence(
                self.dispatch,
                self.directory,
                source,
                identity,
                self.clock.now_ms(),
            ),
        )
        .await
        .map_err(|_| ApplicationError::unavailable("idle Worker evidence release timed out"))?
    }

    /// A restarted process re-registers under the same Worker ID. Expire the
    /// previous incarnation once its lease has lapsed and record its process
    /// end: the exact exit when the provider still holds it, else its
    /// supersession by the registering newer instance of the same process
    /// slot. Otherwise it stays Unknown and its occupied attempts keep their
    /// existing recovery. A live lease is left alone: the Registry then
    /// refuses the new incarnation.
    async fn settle_previous_incarnation(
        &self,
        previous: &WorkerIdentity,
    ) -> Result<(), ApplicationError> {
        let Some(source) = self.stops else {
            return Ok(());
        };
        tokio::time::timeout(
            PROVIDER_EFFECT_TIMEOUT,
            reconcile_closed_idle_worker_evidence(
                self.dispatch,
                self.directory,
                source,
                previous,
                self.clock.now_ms(),
            ),
        )
        .await
        .map_err(|_| {
            ApplicationError::unavailable("previous Worker incarnation settlement timed out")
        })?
    }
}
