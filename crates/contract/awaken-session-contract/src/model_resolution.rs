//! Authority port for resolving a Session-scoped model selection.

use awaken_runtime_contract::resolved::ResolvedModelCandidate;

/// Complete, secret-free model publication frozen by a Session override.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionModelPublication {
    pub primary: ResolvedModelCandidate,
    #[serde(default)]
    pub candidates: Vec<ResolvedModelCandidate>,
}

/// Complete Session-local model replacement. An equal public model id reuses
/// the Agent's route (`publication: None`) but still replaces inference controls;
/// a different id carries the complete newly resolved route.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<Box<SessionModelPublication>>,
    #[serde(default)]
    pub inference: awaken_runtime_contract::agent_bindings::InferenceOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionModelResolutionError {
    #[error("invalid Session model reference: {0}")]
    Invalid(String),
    #[error("Session model resolution is unavailable: {0}")]
    Unavailable(String),
}

/// Resolves an edge-owned model reference through the installation's one model
/// catalog/credential authority. Implementations must return a complete route;
/// callers may never combine the result with a different Agent publication.
#[async_trait::async_trait]
pub trait SessionModelPublicationResolver: Send + Sync {
    async fn resolve_session_model(
        &self,
        workspace_id: &str,
        model_reference: &str,
    ) -> Result<SessionModelPublication, SessionModelResolutionError>;
}
