//! Authority port for resolving a Session-scoped model selection.

use awaken_runtime_contract::resolved::ResolvedModelCandidate;

/// Whether a Session model override may reuse the immutable Agent publication
/// or must resolve and freeze a complete replacement publication.
///
/// A mismatched public model identity always takes the fail-closed
/// `ResolveComplete` path. Callers may not combine a different model identity
/// with the Agent's existing route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionModelOverrideDecision {
    ReusePublished,
    ResolveComplete,
}

/// Decide the complete-publication boundary from exact model identity.
///
/// The generic identity comparison lets the same production kernel be
/// exhaustively checked over a bounded symbolic identity domain. Production
/// callers use model-id strings.
#[must_use]
pub fn session_model_override_decision<T: PartialEq + ?Sized>(
    requested_model: &T,
    published_model: &T,
) -> SessionModelOverrideDecision {
    if requested_model == published_model {
        SessionModelOverrideDecision::ReusePublished
    } else {
        SessionModelOverrideDecision::ResolveComplete
    }
}

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

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn session_model_override_reuses_only_the_same_identity_and_resolves_every_mismatch() {
        let requested_model = kani::any::<u8>();
        let published_model = kani::any::<u8>();
        let decision = session_model_override_decision(&requested_model, &published_model);

        assert_eq!(
            decision,
            if requested_model == published_model {
                SessionModelOverrideDecision::ReusePublished
            } else {
                SessionModelOverrideDecision::ResolveComplete
            }
        );
        match decision {
            SessionModelOverrideDecision::ReusePublished => {
                assert_eq!(requested_model, published_model);
            }
            SessionModelOverrideDecision::ResolveComplete => {
                assert_ne!(requested_model, published_model);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_public_model_identity_reuses_the_agent_publication() {
        assert_eq!(
            session_model_override_decision("published-model", "published-model"),
            SessionModelOverrideDecision::ReusePublished
        );
    }

    #[test]
    fn different_public_model_identity_requires_a_complete_resolution() {
        assert_eq!(
            session_model_override_decision("requested-model", "published-model"),
            SessionModelOverrideDecision::ResolveComplete
        );
    }
}
