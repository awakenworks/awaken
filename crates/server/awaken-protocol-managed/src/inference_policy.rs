//! Workspace-owned inference geography admission.

use crate::types::ModelInferenceGeo;

/// The lifecycle checkpoint at which Managed Agents requires the current
/// Workspace allowlist to be evaluated. Keeping it explicit prevents callers
/// from accidentally treating Session creation as a once-for-ever grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceGeoCheckpoint {
    AgentSave,
    SessionCreate,
    Run,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InferenceGeoPolicyError {
    #[error("inference_geo `{geo}` is not allowed in Workspace `{workspace_id}`")]
    Denied {
        workspace_id: String,
        geo: &'static str,
    },
    #[error("Workspace inference geography policy is unavailable: {0}")]
    Unavailable(String),
}

/// Dynamic policy port owned by the hosted Workspace control plane.
///
/// `None` is the model/service default geography and is normalized to
/// `global`; an explicit `global` is intentionally indistinguishable from it
/// for admission. Implementations must read current policy on every call.
#[async_trait::async_trait]
pub trait ManagedInferenceGeoPolicy: Send + Sync {
    async fn authorize(
        &self,
        workspace_id: &str,
        inference_geo: Option<ModelInferenceGeo>,
        checkpoint: InferenceGeoCheckpoint,
    ) -> Result<(), InferenceGeoPolicyError>;
}

#[must_use]
pub fn inference_geo_name(inference_geo: Option<ModelInferenceGeo>) -> &'static str {
    match inference_geo {
        None | Some(ModelInferenceGeo::Global) => "global",
        Some(ModelInferenceGeo::Us) => "us",
    }
}
