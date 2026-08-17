//! Session model overrides through the canonical publication resolver.

use std::sync::Arc;

use awaken_session_contract::{
    SessionModelPublication, SessionModelPublicationResolver, SessionModelResolutionError,
};
use awaken_tenancy::ScopeId;

use crate::{ModelPublicationResolver, PublicationResolutionError, parse_managed_model_id};

/// Adapter from one Managed model reference to the existing model publication
/// authority. It adds no parser, catalog, credential lookup, or fallback path.
pub struct ConfigSessionModelPublicationResolver {
    resolver: Arc<dyn ModelPublicationResolver>,
}

impl ConfigSessionModelPublicationResolver {
    #[must_use]
    pub fn new(resolver: Arc<dyn ModelPublicationResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait::async_trait]
impl SessionModelPublicationResolver for ConfigSessionModelPublicationResolver {
    async fn resolve_session_model(
        &self,
        workspace_id: &str,
        model_reference: &str,
    ) -> Result<SessionModelPublication, SessionModelResolutionError> {
        let selection = parse_managed_model_id(model_reference)
            .map_err(|error| SessionModelResolutionError::Invalid(error.to_string()))?;
        let resolved = self
            .resolver
            .resolve_models(&ScopeId::from(workspace_id), &selection, &[])
            .await
            .map_err(|error| match error {
                PublicationResolutionError::CatalogUnavailable(_)
                | PublicationResolutionError::CredentialInventoryUnavailable(_) => {
                    SessionModelResolutionError::Unavailable(error.to_string())
                }
                PublicationResolutionError::MissingPrimary
                | PublicationResolutionError::CandidateUnavailable { .. }
                | PublicationResolutionError::DuplicateBinding(_)
                | PublicationResolutionError::Invalid(_) => {
                    SessionModelResolutionError::Invalid(error.to_string())
                }
            })?;
        Ok(SessionModelPublication {
            primary: resolved.primary,
            candidates: resolved.candidates,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_agent_config::ModelSelection;
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};

    use super::*;
    use crate::ResolvedPublicationModels;

    struct RecordingResolver {
        calls: Mutex<Vec<(ScopeId, ModelSelection, Vec<ModelBinding>)>>,
        result: Result<ResolvedPublicationModels, PublicationResolutionError>,
    }

    #[async_trait::async_trait]
    impl ModelPublicationResolver for RecordingResolver {
        async fn resolve_models(
            &self,
            workspace: &ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            self.calls.lock().unwrap().push((
                workspace.clone(),
                selection.clone(),
                candidates.to_vec(),
            ));
            self.result.clone()
        }
    }

    fn complete_models() -> ResolvedPublicationModels {
        ResolvedPublicationModels {
            primary: ResolvedModelCandidate::host(ModelBinding::new(
                "account",
                "upstream-model",
                "genai",
            )),
            candidates: vec![ResolvedModelCandidate::host(ModelBinding::new(
                "fallback",
                "upstream-model",
                "genai",
            ))],
            context_window: Some(200_000),
            max_output_tokens: Some(8_192),
        }
    }

    #[tokio::test]
    async fn adapter_reuses_parser_and_publication_resolver_with_typed_failures() {
        // Cause/effect decision table:
        // | Rule | Model id | Resolver result | Effect |
        // | R1 | valid | complete | exact selection/workspace, route projected |
        // | R2 | invalid | unused | Invalid, resolver not called |
        // | R3 | valid | catalog down | Unavailable |
        // | R4 | valid | candidate invalid | Invalid |
        // Context-window facts are intentionally excluded: the Session override
        // replaces only the model route within the already-published Agent spec.
        let expected = complete_models();
        let resolver = Arc::new(RecordingResolver {
            calls: Mutex::new(Vec::new()),
            result: Ok(expected.clone()),
        });
        let adapter = ConfigSessionModelPublicationResolver::new(resolver.clone());
        let model_ids = [
            "claude-sonnet-4-5;provider=anthropic;api=anthropic;endpoint=primary",
            "vendor/model;provider=third-party%2Fgateway;api=vendor_messages_v9;endpoint=regional%2Fedge;executor=acp:opencode",
            "executor=a2a:https://third-party.example/agents/research",
        ];
        for model_id in model_ids {
            let publication = adapter
                .resolve_session_model("workspace-a", model_id)
                .await
                .expect("R1");
            assert_eq!(publication.primary, expected.primary, "R1");
            assert_eq!(publication.candidates, expected.candidates, "R1");
        }
        let calls = resolver.calls.lock().unwrap();
        assert_eq!(calls.len(), model_ids.len(), "R1");
        for (call, model_id) in calls.iter().zip(model_ids) {
            assert_eq!(call.0, ScopeId::from("workspace-a"), "R1");
            assert_eq!(call.1, parse_managed_model_id(model_id).unwrap(), "R1");
            assert!(call.2.is_empty(), "R1");
        }
        drop(calls);

        let invalid = adapter
            .resolve_session_model("workspace-a", "model;api=anthropic")
            .await
            .expect_err("R2");
        assert!(
            matches!(invalid, SessionModelResolutionError::Invalid(_)),
            "R2"
        );
        assert_eq!(resolver.calls.lock().unwrap().len(), model_ids.len(), "R2");

        for (rule, source, unavailable) in [
            (
                "R3",
                PublicationResolutionError::CatalogUnavailable("offline".into()),
                true,
            ),
            (
                "R4",
                PublicationResolutionError::CandidateUnavailable {
                    binding: ModelBinding::new("account", "model", "genai"),
                    reason: "disabled".into(),
                },
                false,
            ),
        ] {
            let adapter = ConfigSessionModelPublicationResolver::new(Arc::new(RecordingResolver {
                calls: Mutex::new(Vec::new()),
                result: Err(source),
            }));
            let error = adapter
                .resolve_session_model("workspace-a", "model")
                .await
                .expect_err(rule);
            assert_eq!(
                matches!(error, SessionModelResolutionError::Unavailable(_)),
                unavailable,
                "{rule}"
            );
        }
    }
}
