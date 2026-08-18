//! Wire projection state for the Managed protocol adapter.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use awaken_session_application::SessionApplication;

use crate::types::StreamFrame;

use super::SessionRecord;

/// Disposable Managed wire projections over the canonical Session application.
pub struct ManagedState {
    pub(super) application: Arc<SessionApplication>,
    /// Disposable record→wire projection; durable truth lives in the application repository.
    pub(super) sessions: Mutex<HashMap<String, SessionRecord>>,
    /// Edge-owned Session-to-Workspace wire projection.
    pub(super) owners: Mutex<HashMap<String, String>>,
    pub(super) session_seq: AtomicU64,
    /// Shared by preview and committed wire-event allocation.
    pub(super) event_seq: Arc<AtomicU64>,
    /// Per-Session live wire stream channels.
    pub(super) live: Mutex<HashMap<String, broadcast::Sender<StreamFrame>>>,
    /// Current Workspace geography policy. Hosted composition supplies this
    /// authority; self-managed mode leaves it absent and retains local policy.
    pub(super) inference_geo_policy: Option<Arc<dyn crate::ManagedInferenceGeoPolicy>>,
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
                    awaken_session_contract::RunError::bad_request(error.to_string()),
                ),
                crate::InferenceGeoPolicyError::Unavailable(_) => super::StateError::Run(
                    awaken_session_contract::RunError::unavailable(error.to_string()),
                ),
            })
    }
}
