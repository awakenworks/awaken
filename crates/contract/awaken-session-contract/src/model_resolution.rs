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

impl SessionModelPublication {
    /// Validate the complete route returned by the installation's model
    /// authority. This mirrors Agent publication fencing so a Session override
    /// cannot smuggle a duplicate binding, another Workspace's credentials, or
    /// an unproved ACP/A2A route into durable truth.
    pub fn validate_for_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<(), SessionModelResolutionError> {
        let mut bindings = std::collections::BTreeSet::new();
        for candidate in std::iter::once(&self.primary).chain(self.candidates.iter()) {
            if !bindings.insert(candidate.binding().clone()) {
                return Err(SessionModelResolutionError::Invalid(format!(
                    "duplicate model candidate {:?}",
                    candidate.binding()
                )));
            }
            match candidate.provisioning() {
                awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                    scope_id, ..
                }
                | awaken_runtime_contract::resolved::ModelProvisioning::Remote {
                    scope_id, ..
                } if scope_id.as_str() != workspace_id => {
                    return Err(SessionModelResolutionError::Invalid(format!(
                        "model candidate belongs to Workspace {scope_id}, not {workspace_id}"
                    )));
                }
                awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned {
                    acp, ..
                } if acp.capability_fingerprint.trim().is_empty()
                    || acp.capability_adapter_version.trim().is_empty() =>
                {
                    return Err(SessionModelResolutionError::Invalid(
                        "backend-owned model candidate lacks an exact ACP capability pin".into(),
                    ));
                }
                awaken_runtime_contract::resolved::ModelProvisioning::Remote {
                    security_fingerprint,
                    ..
                } if security_fingerprint.trim().is_empty() => {
                    return Err(SessionModelResolutionError::Invalid(
                        "remote model candidate lacks an exact Agent Card security fingerprint"
                            .into(),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
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
    use awaken_runtime_contract::resolved::{
        AcpSessionConfiguration, BackendModelSelection, ModelBinding,
    };

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

    #[test]
    fn session_publication_validation_fences_duplicates_scope_and_runtime_proofs() {
        let host = |provider: &str, model: &str, backend: &str| {
            ResolvedModelCandidate::host(ModelBinding::new(provider, model, backend))
        };
        let valid = SessionModelPublication {
            primary: host("third-party/gateway", "model-a", "genai"),
            candidates: vec![host("third-party/gateway", "model-b", "acp:codex")],
        };
        valid
            .validate_for_workspace("workspace")
            .expect("V1 opaque third-party provider and mixed runtime roster");

        let duplicate = SessionModelPublication {
            primary: host("provider", "model", "genai"),
            candidates: vec![host("provider", "model", "genai")],
        };
        assert!(
            duplicate.validate_for_workspace("workspace").is_err(),
            "V2 duplicate complete binding"
        );

        let endpoint = awaken_runtime_contract::InferenceEndpoint {
            adapter_kind: "third-party".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://models.example/v1".into(),
            upstream_model: "model".into(),
            processing_placement: None,
        };
        let wrong_provider_scope = SessionModelPublication {
            primary: ResolvedModelCandidate::try_provider(
                ModelBinding::new("third-party", "model", "genai"),
                "third-party@1",
                "open-ai-chat@1",
                "other-workspace",
                None,
                endpoint,
            )
            .expect("coherent cross-Workspace provider candidate"),
            candidates: Vec::new(),
        };
        assert!(
            wrong_provider_scope
                .validate_for_workspace("workspace")
                .is_err(),
            "V3 provider credential scope"
        );

        let unproved_acp = ResolvedModelCandidate::try_backend_owned(
            ModelBinding::new("local", "", "acp:codex"),
            awaken_runtime_contract::CredentialRef {
                id: "codex-login".into(),
                revision: 1,
            },
            BackendModelSelection::Default,
            "",
            "",
            AcpSessionConfiguration::default(),
        );
        assert!(unproved_acp.is_err(), "V4 ACP capability proof");

        let unproved_a2a = ResolvedModelCandidate::try_remote(
            ModelBinding::new("", "", "a2a:https://agent.example"),
            "workspace",
            None,
            "",
        );
        assert!(unproved_a2a.is_err(), "V5 A2A Agent Card proof");
    }
}
