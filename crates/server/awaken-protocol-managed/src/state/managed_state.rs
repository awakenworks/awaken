// @formal surface exclude=projection_state_holder
// @formal owner crates/server/awaken-protocol-managed/src/state/rehydration.rs
// @formal reason: Declares disposable in-memory wire projection fields only; it implements no locking
// @formal reason: protocol or canonical Session transition.
//! Wire projection state for the Managed protocol adapter.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use awaken_session_application::{
    DeploymentSessionEnvironment, ProfiledSessionCommands, ProfiledSessionReplies,
    SessionApplication, SessionCreation, SessionCredentialCommands, SessionEventBatches,
    SessionObservation, SessionProjectionReads, SessionResourceCommands, SessionRootReads,
    SessionTermination, SessionUpdates,
};
use awaken_time::WallClock;

use crate::types::Event;

use super::SessionRecord;

/// The Session application roles this adapter calls, each held as its own
/// port. Product composition hands over the one canonical application and the
/// adapter keeps only these narrowed views of it.
pub(crate) struct SessionApplicationRoles {
    pub(crate) creation: Arc<dyn SessionCreation>,
    pub(crate) credentials: Arc<dyn SessionCredentialCommands>,
    pub(crate) roots: Arc<dyn SessionRootReads>,
    pub(crate) projection: Arc<dyn SessionProjectionReads>,
    pub(crate) events: Arc<dyn SessionEventBatches>,
    pub(crate) profiled: Arc<dyn ProfiledSessionCommands>,
    pub(crate) profiled_replies: Arc<dyn ProfiledSessionReplies>,
    pub(crate) termination: Arc<dyn SessionTermination>,
    pub(crate) updates: Arc<dyn SessionUpdates>,
    pub(crate) resources: Arc<dyn SessionResourceCommands>,
    pub(crate) observation: Arc<dyn SessionObservation>,
    pub(crate) deployments: Arc<dyn DeploymentSessionEnvironment>,
    /// The application's injected clock, so wire observations share its time line.
    pub(crate) clock: Arc<dyn WallClock>,
}

impl SessionApplicationRoles {
    pub(super) fn of(application: &Arc<SessionApplication>) -> Self {
        Self {
            creation: application.creation_commands(),
            credentials: application.credentials(),
            roots: application.queries(),
            projection: application.queries(),
            events: application.clone(),
            profiled: application.run_admissions(),
            profiled_replies: application.clone(),
            termination: application.termination_commands(),
            updates: application.update_commands(),
            resources: application.resource_manifests(),
            observation: application.observations(),
            deployments: application.clone(),
            clock: application.clock(),
        }
    }
}

/// Disposable Managed wire projections over the canonical Session application.
pub struct ManagedState {
    pub(crate) roles: SessionApplicationRoles,
    /// The fixture composition's own application, kept so tests can drive it
    /// directly and builders can finish configuring it before it is shared.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) application: Arc<SessionApplication>,
    /// Exact Coordinator Environment application shared by Session compilation
    /// and the authenticated Worker warmup projection. Keeping the concrete
    /// application Arc here lets process composition expose the existing owner;
    /// it does not introduce a second catalog, queue, or warmup state.
    pub(super) environments:
        Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    /// Disposable record→wire projection; durable truth lives in the application repository.
    pub(super) sessions: Mutex<HashMap<String, SessionRecord>>,
    /// Edge-owned Session-to-Workspace wire projection.
    pub(super) owners: Mutex<HashMap<String, String>>,
    pub(super) session_seq: AtomicU64,
    /// Per-Session committed wire-event channels. Runtime live observations use
    /// the ThreadEventHub subscription and never enter this map.
    pub(super) live: Mutex<HashMap<String, broadcast::Sender<Event>>>,
    /// Current Workspace geography policy. Hosted composition supplies this
    /// authority; self-managed mode leaves it absent and retains local policy.
    pub(super) inference_geo_policy: Option<Arc<dyn crate::ManagedInferenceGeoPolicy>>,
    /// The Host's content-capture ceiling and policy sources, so the
    /// Session's `content_capture` projection reports the decision its Runs
    /// make (ADR-0050 D8).
    pub(crate) capture_policy: awaken_data_governance_contract::CapturePolicy,
    /// Whether every published refresh is checked against a cold rebuild of
    /// the same committed prefix. Only a test that counts source reads turns
    /// it off, because the check reads the prefix a second time.
    #[cfg(test)]
    pub(super) warm_cold_oracle: std::sync::atomic::AtomicBool,
}

impl ManagedState {
    pub(super) async fn authorize_inference_geo(
        &self,
        workspace_id: &str,
        inference_geo: Option<crate::types::ModelInferenceGeo>,
        checkpoint: crate::InferenceGeoCheckpoint,
    ) -> Result<(), super::StateError> {
        let Some(policy) = &self.inference_geo_policy else {
            return Ok(());
        };
        policy
            .authorize(workspace_id, inference_geo, checkpoint)
            .await
            .map_err(|error| match error {
                crate::InferenceGeoPolicyError::Denied { .. } => super::StateError::Run(
                    awaken_session_types::RunError::bad_request(error.to_string()),
                ),
                crate::InferenceGeoPolicyError::Unavailable(_) => super::StateError::Run(
                    awaken_session_types::RunError::unavailable(error.to_string()),
                ),
            })
    }
}
