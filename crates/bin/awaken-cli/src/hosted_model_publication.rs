//! Hosted model-supply composition over the canonical publication port.
//!
//! Cloud remains authoritative for brokered offerings and the open catalog /
//! Credential Vault remain authoritative for direct Workspace offerings. This
//! adapter owns no catalog or persistence; it only partitions one publication
//! request across those existing authorities and fails closed on authority
//! outages or malformed state.

use std::collections::HashSet;
use std::sync::Arc;

use awaken_agent_config::ModelSelection;
use awaken_config_service::{
    ModelPublicationResolver, PublicationResolutionError, ResolvedPublicationModels,
};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_tenancy::ScopeId;

/// The legal startup modes are deliberately disjoint. Hosted composition may
/// combine the injected brokered resolver with the ordinary direct resolver;
/// deterministic scenarios retain their one explicit host executor.
pub(super) enum PublicationModelSupply {
    PublishedProviders,
    HostedPublication {
        resolver: Arc<dyn ModelPublicationResolver>,
        direct_credential_execution:
            Option<crate::managed_platform::HostedProviderCredentialExecution>,
    },
    #[cfg(any(test, feature = "test-support"))]
    Host {
        executor: Arc<dyn awaken_runtime_contract::llm::LlmExecutor>,
        binding: ModelBinding,
    },
}

impl PublicationModelSupply {
    pub(super) fn needs_interactive_brokered_client(&self) -> bool {
        matches!(self, Self::PublishedProviders)
    }
}

pub(super) struct HostedAndDirectModelPublicationResolver {
    pub(super) hosted: Arc<dyn ModelPublicationResolver>,
    pub(super) direct: Arc<dyn ModelPublicationResolver>,
}

impl HostedAndDirectModelPublicationResolver {
    async fn resolve_one(
        &self,
        workspace: &ScopeId,
        selection: &ModelSelection,
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        match self.hosted.resolve_models(workspace, selection, &[]).await {
            Ok(resolved) => Ok(resolved),
            Err(
                PublicationResolutionError::MissingPrimary
                | PublicationResolutionError::CandidateUnavailable { .. },
            ) => self.direct.resolve_models(workspace, selection, &[]).await,
            Err(error) => Err(error),
        }
    }
}

#[async_trait::async_trait]
impl ModelPublicationResolver for HostedAndDirectModelPublicationResolver {
    async fn resolve_models(
        &self,
        workspace: &ScopeId,
        selection: &ModelSelection,
        fallbacks: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
        // Auto and Profile own their fallback policy inside one authoritative
        // resolver. Splitting them here would duplicate catalog/profile logic.
        if matches!(
            selection,
            ModelSelection::Auto | ModelSelection::Profile { .. }
        ) {
            return match self
                .hosted
                .resolve_models(workspace, selection, fallbacks)
                .await
            {
                Ok(resolved) => Ok(resolved),
                Err(
                    PublicationResolutionError::MissingPrimary
                    | PublicationResolutionError::CandidateUnavailable { .. },
                ) => {
                    self.direct
                        .resolve_models(workspace, selection, fallbacks)
                        .await
                }
                Err(error) => Err(error),
            };
        }

        let mut primary = self.resolve_one(workspace, selection).await?;
        if !primary.candidates.is_empty() {
            return Err(PublicationResolutionError::Invalid(
                "a single-candidate resolver returned undeclared fallbacks".into(),
            ));
        }

        let mut seen = HashSet::from([primary.primary.binding().clone()]);
        let mut candidates = Vec::with_capacity(fallbacks.len());
        for binding in fallbacks {
            let resolved = self
                .resolve_one(workspace, &ModelSelection::Pinned(binding.clone()))
                .await?;
            if !resolved.candidates.is_empty() {
                return Err(PublicationResolutionError::Invalid(
                    "a single-candidate resolver returned undeclared fallbacks".into(),
                ));
            }
            let candidate_binding = resolved.primary.binding().clone();
            if !seen.insert(candidate_binding.clone()) {
                return Err(PublicationResolutionError::DuplicateBinding(
                    candidate_binding,
                ));
            }
            candidates.push(resolved.primary);
        }
        primary.candidates = candidates;
        Ok(primary)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_runtime_contract::resolved::ResolvedModelCandidate;

    use super::*;

    struct SourceResolver {
        admitted_provider: &'static str,
        calls: Mutex<Vec<ModelSelection>>,
        authority_error: Option<PublicationResolutionError>,
    }

    impl SourceResolver {
        fn available(provider: &'static str) -> Arc<Self> {
            Arc::new(Self {
                admitted_provider: provider,
                calls: Mutex::new(Vec::new()),
                authority_error: None,
            })
        }

        fn unavailable(error: PublicationResolutionError) -> Arc<Self> {
            Arc::new(Self {
                admitted_provider: "hosted",
                calls: Mutex::new(Vec::new()),
                authority_error: Some(error),
            })
        }
    }

    #[async_trait::async_trait]
    impl ModelPublicationResolver for SourceResolver {
        async fn resolve_models(
            &self,
            _workspace: &ScopeId,
            selection: &ModelSelection,
            fallbacks: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, PublicationResolutionError> {
            self.calls.lock().unwrap().push(selection.clone());
            if let Some(error) = &self.authority_error {
                return Err(error.clone());
            }
            let binding = selection
                .resolved()
                .cloned()
                .ok_or(PublicationResolutionError::MissingPrimary)?;
            if binding.provider_identity_ref != self.admitted_provider {
                return Err(PublicationResolutionError::CandidateUnavailable {
                    binding,
                    reason: "owned by the other model-supply authority".into(),
                });
            }
            if !fallbacks.is_empty() {
                return Err(PublicationResolutionError::Invalid(
                    "test resolver expects one candidate".into(),
                ));
            }
            Ok(ResolvedPublicationModels {
                primary: ResolvedModelCandidate::host(binding),
                candidates: Vec::new(),
                context_window: Some(100_000),
                max_output_tokens: Some(4_096),
            })
        }
    }

    /// Cause/effect graph and decision table:
    /// C1 hosted owns primary, C2 direct owns fallback, C3 hosted authority is
    /// unavailable. E1 preserve exact order, E2 never consult direct after a
    /// hosted success, E3 fail closed on authority failure.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// | --- | --- | --- | --- | --- |
    /// | R1 | yes | no | no | hosted result; direct untouched |
    /// | R2 | yes | yes | no | mixed exact candidates in authored order |
    /// | R3 | any | any | yes | authority error; direct untouched |
    #[tokio::test]
    async fn partitions_exact_candidates_and_fails_closed_on_authority_errors() {
        let workspace = ScopeId::from("workspace-a");
        let hosted_binding = ModelBinding::new("hosted", "model-a", "genai");
        let direct_binding = ModelBinding::new("direct", "model-b", "genai");

        let hosted = SourceResolver::available("hosted");
        let direct = SourceResolver::available("direct");
        let resolver = HostedAndDirectModelPublicationResolver {
            hosted: hosted.clone(),
            direct: direct.clone(),
        };
        let resolved = resolver
            .resolve_models(
                &workspace,
                &ModelSelection::Pinned(hosted_binding.clone()),
                &[],
            )
            .await
            .expect("R1");
        assert_eq!(resolved.primary.binding(), &hosted_binding, "R1");
        assert!(direct.calls.lock().unwrap().is_empty(), "R1/E2");

        let resolved = resolver
            .resolve_models(
                &workspace,
                &ModelSelection::Pinned(hosted_binding.clone()),
                std::slice::from_ref(&direct_binding),
            )
            .await
            .expect("R2");
        assert_eq!(resolved.primary.binding(), &hosted_binding, "R2");
        assert_eq!(resolved.candidates[0].binding(), &direct_binding, "R2/E1");

        let unavailable = SourceResolver::unavailable(
            PublicationResolutionError::CatalogUnavailable("offline".into()),
        );
        let untouched = SourceResolver::available("direct");
        let resolver = HostedAndDirectModelPublicationResolver {
            hosted: unavailable,
            direct: untouched.clone(),
        };
        assert!(
            matches!(
                resolver
                    .resolve_models(&workspace, &ModelSelection::Pinned(hosted_binding), &[])
                    .await,
                Err(PublicationResolutionError::CatalogUnavailable(_))
            ),
            "R3/E3"
        );
        assert!(untouched.calls.lock().unwrap().is_empty(), "R3/E2");
    }
}
