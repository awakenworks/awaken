// @formal surface obligation=config.inference_access_resolved_once_at_publication
// @formal surface obligation=model_readiness.brokered_and_direct_routes_require_exact_access_evidence
// @formal surface obligation=model_route.fallback_preserves_the_complete_publication_pin
// @formal reason: Hosted/direct composition consumes only publication-pinned candidates, preserves the
// @formal reason: complete authored fallback order, rejects duplicate bindings, and fails closed on
// @formal reason: authority errors without owning another catalog.
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

pub(super) fn brokered_inference_client(
    cloud_models_enabled: bool,
    remote_iam: Option<&Arc<awaken_control::RemoteManagementAuthz>>,
    developer_key_file: Option<&std::path::Path>,
    cloud_api_base_url: Option<&str>,
    execution_workspace: &str,
    client_instance_id: &str,
) -> Option<Arc<awaken_inference_endpoint::brokered_inference::HttpBrokeredInferenceClient>> {
    if !cloud_models_enabled {
        return None;
    }
    let token_source: Arc<awaken_agent_contract::RedactedStringSource> =
        if let Some(path) = developer_key_file {
            let path = path.to_owned();
            Arc::new(move || {
                let value = std::fs::read_to_string(&path)
                    .map_err(|error| format!("read Cloud Developer Key file: {error}"))?;
                let value = value.trim();
                if !value.starts_with("sk-awaken-") || value.chars().any(char::is_whitespace) {
                    return Err("Cloud Developer Key file does not contain one valid key".into());
                }
                Ok(awaken_agent_contract::RedactedString::new(value.to_owned()))
            })
        } else {
            let authz = Arc::clone(remote_iam?);
            Arc::new(move || {
                authz
                    .cloud_user_token()?
                    .ok_or_else(|| "Cloud login credential is unavailable".to_string())
            })
        };
    let base_url =
        cloud_api_base_url.expect("Awaken Cloud identity requires a Cloud inference API URL");
    Some(Arc::new(
        awaken_inference_endpoint::brokered_inference::HttpBrokeredInferenceClient::new(
            base_url,
            token_source,
            execution_workspace,
            client_instance_id,
        )
        .unwrap_or_else(|error| panic!("Cloud inference configuration: {error}")),
    ))
}

pub(super) fn local_client_instance_id(workspace_root: Option<&std::path::Path>) -> String {
    let coordinate = workspace_root
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ephemeral".to_owned());
    let fingerprint = awaken_agent_contract::content_fingerprint(&coordinate)
        .unwrap_or_else(|_| "unavailable".to_owned());
    format!("awaken-installation:{fingerprint}")
}

#[cfg(test)]
mod brokered_client_boundary_tests {
    use super::*;

    #[test]
    fn projected_developer_key_and_workspace_use_distinct_client_coordinates() {
        // Cause/effect graph: C1=Cloud models enabled, C2=projected Developer
        // Key is valid, C3=Workspace and installation root are present.
        // Effects: one brokered client is constructed without an OAuth fallback;
        // Workspace identity remains distinct from the stable installation id.
        // R1 all true => client; !C2 => fail construction before network.
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("cloud-api-key");
        std::fs::write(&key, "sk-awaken-fixture.secret\n").unwrap();
        let workspace = "awaken:workspace-a";
        let instance = local_client_instance_id(Some(dir.path()));
        assert_ne!(workspace, instance);
        assert!(instance.starts_with("awaken-installation:"));
        assert!(
            brokered_inference_client(
                true,
                None,
                Some(&key),
                Some("https://api.awaken.test"),
                workspace,
                &instance,
            )
            .is_some()
        );

        std::fs::write(&key, "not-a-developer-key").unwrap();
        let result = std::panic::catch_unwind(|| {
            brokered_inference_client(
                true,
                None,
                Some(&key),
                Some("https://api.awaken.test"),
                workspace,
                &instance,
            )
        });
        assert!(result.is_err());
    }
}

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
        if matches!(
            selection,
            ModelSelection::Target { .. }
                | ModelSelection::BackendDefault { .. }
                | ModelSelection::BackendExact { .. }
        ) {
            return self.direct.resolve_models(workspace, selection, &[]).await;
        }
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
        // Profiles belong to the canonical Workspace compiler, which delegates
        // brokered candidates to Cloud after validating each authored binding.
        if matches!(selection, ModelSelection::Profile { .. }) {
            return self
                .direct
                .resolve_models(workspace, selection, fallbacks)
                .await;
        }
        // Auto retains Cloud's default policy; an empty supply may use BYOK.
        if matches!(selection, ModelSelection::Auto) {
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
        let mut seen = std::iter::once(&primary.primary)
            .chain(primary.candidates.iter())
            .map(|candidate| candidate.binding().clone())
            .collect::<HashSet<_>>();
        let mut candidates = std::mem::take(&mut primary.candidates);
        for binding in fallbacks {
            let resolved = self
                .resolve_one(workspace, &ModelSelection::Pinned(binding.clone()))
                .await?;
            for candidate in std::iter::once(resolved.primary).chain(resolved.candidates) {
                let binding = candidate.binding().clone();
                if !seen.insert(binding.clone()) {
                    return Err(PublicationResolutionError::DuplicateBinding(binding));
                }
                candidates.push(candidate);
            }
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
            if binding.identity().0 != self.admitted_provider {
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

    // Causes: authored Profile/Target versus Cloud-owned Auto; Cloud outage;
    // canonical compiler outcome. Rules: authored selections consult the
    // Workspace compiler, Auto propagates Cloud outage without trying direct.
    // The compiler's own tests cover its per-candidate delegation to Cloud.
    #[tokio::test]
    async fn authored_selections_reach_the_workspace_compiler() {
        let hosted = SourceResolver::unavailable(PublicationResolutionError::CatalogUnavailable(
            "cloud offline".into(),
        ));
        let direct = SourceResolver::unavailable(PublicationResolutionError::Invalid(
            "compiler reached".into(),
        ));
        let resolver = HostedAndDirectModelPublicationResolver {
            hosted: hosted.clone(),
            direct: direct.clone(),
        };
        let workspace = ScopeId::from("workspace-a");
        for selection in [
            ModelSelection::Profile {
                profile_id: "profile-a".into(),
            },
            ModelSelection::Target {
                target: awaken_agent_contract::ModelTarget {
                    model_id: "model-a".into(),
                    provider_id: Some("openai".into()),
                    api_dialect: None,
                    protocol_endpoint_id: None,
                    endpoint_name: None,
                },
                backend_ref: "genai".into(),
                configuration: Default::default(),
            },
        ] {
            assert!(
                matches!(resolver.resolve_models(&workspace, &selection, &[]).await,
                Err(PublicationResolutionError::Invalid(message)) if message == "compiler reached")
            );
            assert_eq!(direct.calls.lock().unwrap().last(), Some(&selection));
            assert!(hosted.calls.lock().unwrap().is_empty());
        }
        assert!(matches!(
            resolver
                .resolve_models(&workspace, &ModelSelection::Auto, &[])
                .await,
            Err(PublicationResolutionError::CatalogUnavailable(_))
        ));
        assert_eq!(direct.calls.lock().unwrap().len(), 2);
    }
}
