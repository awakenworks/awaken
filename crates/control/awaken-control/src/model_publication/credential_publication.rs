//! Secret-free credential selection and Provider publication projection.

use awaken_config_resolver::{
    CredentialCandidateSet, CredentialSelectionContext, SourceLookup, credential_candidates,
    credential_is_executable_supply,
};
use awaken_config_service::PublicationResolutionError;
use awaken_credential_vault::{
    CredentialBinding, CredentialHolderAdmission, CredentialPool, CredentialSource,
    DeferredCredentialHolderSelection, ExactCredentialAccessRequest, SelectionPolicy,
    compile_exact_credential_access,
};
use awaken_model_catalog::{ApiDialect, Offering, ProviderCatalog};
use awaken_runtime_contract::resolved::AcpExecutionProfile;
use awaken_runtime_contract::resolved::{Backend, ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::{
    CredentialMaterialBinding, CredentialUsage, InferenceEndpoint, ProviderExecutionProfile,
    UnspecifiedReasoning,
};
use awaken_tenancy::ScopeId;

use super::CatalogModelPublicationResolver;

pub(super) enum PublicationAccess<'a> {
    Direct(Option<&'a CredentialSource>),
    Brokered,
}

pub(super) struct PublicationCredentialLookup<'a> {
    pub(super) sources: &'a [CredentialSource],
    pub(super) pool: Option<&'a CredentialPool>,
}

/// Compile catalog identity into provider behavior once, before the immutable
/// runtime snapshot crosses the control/runtime boundary. Runtime adapters must
/// never recover this policy from mutable URLs or vendor-shaped JSON.
fn unspecified_reasoning(provider_slug: &str, dialect: ApiDialect) -> UnspecifiedReasoning {
    if provider_slug == "deepseek" && dialect == ApiDialect::OpenAiChat {
        UnspecifiedReasoning::Disabled
    } else {
        UnspecifiedReasoning::ProviderDefault
    }
}

impl SourceLookup for PublicationCredentialLookup<'_> {
    fn get(&self, id: &str) -> Option<&CredentialSource> {
        self.sources.iter().find(|source| source.id.0 == id)
    }

    fn get_pool(&self, id: &str) -> Option<&CredentialPool> {
        self.pool.filter(|pool| pool.id.0 == id)
    }
}

impl CatalogModelPublicationResolver {
    pub(super) fn publication_access<'a>(
        &self,
        lookup: &'a PublicationCredentialLookup<'a>,
        workspace: &ScopeId,
        offering: &Offering,
        credential_binding: &CredentialBinding,
        binding: &ModelBinding,
    ) -> Result<PublicationAccess<'a>, String> {
        match credential_binding {
            CredentialBinding::None => {
                if offering.source == awaken_model_catalog::OfferingSource::Brokered {
                    return Err(
                        "brokered offering requires an explicit brokered access binding".into(),
                    );
                }
            }
            CredentialBinding::Brokered => {
                if offering.source != awaken_model_catalog::OfferingSource::Brokered {
                    return Err(
                        "brokered access binding requires a brokered catalog offering".into(),
                    );
                }
            }
            CredentialBinding::Exact { .. } | CredentialBinding::OneOfCredentialPool { .. } => {
                if offering.source == awaken_model_catalog::OfferingSource::Brokered {
                    return Err("brokered offering cannot consume a local credential".into());
                }
            }
        }
        let sequence = lookup
            .pool
            .filter(|pool| matches!(pool.policy, SelectionPolicy::RotateSpread))
            .map_or(0, |pool| {
                let mut sequences = self
                    .credential_selection_sequences
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let sequence = sequences.entry(pool.id.0.clone()).or_default();
                let current = *sequence;
                *sequence = sequence.wrapping_add(1);
                current
            });
        let candidates = credential_candidates(
            credential_binding,
            lookup,
            CredentialSelectionContext {
                offering_provider: Some(offering.provider_id.as_str()),
                offering_endpoint: Some(offering.protocol_endpoint_id.as_str()),
                backend_ref: Some(&binding.backend_ref),
                availability: None,
                expected_workspace: Some(workspace.as_str()),
                selection_sequence: sequence,
            },
        )
        .map_err(|error| match credential_binding {
            CredentialBinding::Exact {
                credential_source_id,
            } => format!(
                "exact credential {} is absent, inactive, or incompatible with provider {}: {error}",
                credential_source_id.0, offering.provider_id
            ),
            CredentialBinding::OneOfCredentialPool { credential_pool_id } => format!(
                "credential pool {} has no active compatible member: {error}",
                credential_pool_id.0
            ),
            CredentialBinding::None | CredentialBinding::Brokered => error.to_string(),
        })?;
        match candidates {
            CredentialCandidateSet::None => Ok(PublicationAccess::Direct(None)),
            CredentialCandidateSet::Brokered => Ok(PublicationAccess::Brokered),
            direct @ CredentialCandidateSet::Direct { .. } => direct
                .first_eligible(|source| {
                    credential_is_executable_supply(
                        offering.provider_id.as_str(),
                        Some(offering.protocol_endpoint_id.as_str()),
                        &binding.backend_ref,
                        source,
                    ) && self.credential_usage(binding, source).is_ok()
                })
                .map(|source| PublicationAccess::Direct(Some(source)))
                .ok_or_else(|| match credential_binding {
                    CredentialBinding::Exact {
                        credential_source_id,
                    } => format!(
                        "exact credential {} is absent, inactive, or incompatible with provider {}",
                        credential_source_id.0, offering.provider_id
                    ),
                    CredentialBinding::OneOfCredentialPool { credential_pool_id } => format!(
                        "credential pool {} has no active compatible member",
                        credential_pool_id.0
                    ),
                    CredentialBinding::None | CredentialBinding::Brokered => {
                        "credential binding has no active compatible material source".into()
                    }
                }),
        }
    }

    fn credential_usage(
        &self,
        binding: &ModelBinding,
        source: &CredentialSource,
    ) -> Result<CredentialUsage, String> {
        let Backend::Acp(backend) = Backend::from_ref(&binding.backend_ref) else {
            if source.is_claude_code_setup_token() {
                return Err("Claude Code setup tokens require backend acp:claude".into());
            }
            return Ok(CredentialUsage::ProviderAdapter);
        };
        let cli = backend.cli();
        let capability = self
            .acp_capabilities
            .iter()
            .find(|capability| capability.backend_ref == binding.backend_ref)
            .ok_or_else(|| format!("ACP backend {cli} is not in the executable catalog"))?;
        capability
            .credential_usage(source.process_secret_environment_hint())
            .map_err(|error| format!("ACP backend {cli}: {error}"))
    }

    pub(super) fn provider_candidate(
        &self,
        catalog: &ProviderCatalog,
        workspace: &ScopeId,
        binding: ModelBinding,
        offering: &Offering,
        access: PublicationAccess<'_>,
        acp: Option<AcpExecutionProfile>,
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        let unavailable = |reason| PublicationResolutionError::CandidateUnavailable {
            binding: binding.clone(),
            reason,
        };
        let provider = catalog
            .providers
            .get(offering.provider_id.as_str())
            .ok_or_else(|| unavailable(format!("provider {} is missing", offering.provider_id)))?;
        let endpoint = catalog
            .endpoints
            .get(offering.protocol_endpoint_id.as_str())
            .ok_or_else(|| {
                unavailable(format!(
                    "endpoint {} is missing",
                    offering.protocol_endpoint_id
                ))
            })?;
        let base_url = endpoint
            .base_url
            .clone()
            .ok_or_else(|| unavailable(format!("endpoint {} has no base URL", endpoint.id.0)))?;
        let route_ref = if offering.source == awaken_model_catalog::OfferingSource::Brokered {
            format!(
                "brokered:{}@{}",
                offering.protocol_endpoint_id.0, endpoint.version
            )
        } else {
            format!("{}@{}", offering.protocol_endpoint_id.0, endpoint.version)
        };
        let reasoning = unspecified_reasoning(provider.slug.as_str(), endpoint.dialect);
        let endpoint = InferenceEndpoint {
            adapter_kind: endpoint.dialect.adapter_kind().to_string(),
            api_dialect: endpoint.dialect.as_str().to_string(),
            base_url,
            upstream_model: offering
                .upstream_model
                .clone()
                .unwrap_or_else(|| offering.model_id.clone()),
            processing_placement: None,
        };
        let provider_ref = format!("{}@{}", offering.provider_id.0, provider.version);
        let brokered = matches!(access, PublicationAccess::Brokered);
        let credential = match access {
            PublicationAccess::Direct(credential) => credential
                .map(|credential| {
                    let usage = self
                        .credential_usage(&binding, credential)
                        .map_err(unavailable)?;
                    let material_binding = CredentialMaterialBinding::for_target(
                        workspace.as_str(),
                        &(&provider_ref, &endpoint),
                        &usage,
                    );
                    let holder_admission = match &self.direct_holder {
                        Some(holder) => CredentialHolderAdmission::Selected(holder),
                        None => CredentialHolderAdmission::Deferred(
                            DeferredCredentialHolderSelection::ProviderPublication,
                        ),
                    };
                    compile_exact_credential_access(
                        credential,
                        ExactCredentialAccessRequest {
                            workspace_id: Some(workspace.as_str()),
                            target: None,
                            usage,
                            policy: self.direct_credential_policy.clone(),
                            holder_admission,
                            binding: &material_binding,
                            now_unix_ms: super::wall_clock_ms(),
                        },
                    )
                    .map_err(|error| unavailable(error.to_string()))
                })
                .transpose()?,
            PublicationAccess::Brokered => None,
        };
        let error_binding = binding.clone();
        match (brokered, acp) {
            (true, Some(acp)) => ResolvedModelCandidate::try_brokered_provider_with_profile(
                binding,
                provider_ref,
                route_ref,
                workspace.clone(),
                endpoint,
                ProviderExecutionProfile {
                    unspecified_reasoning: reasoning,
                    acp: Some(acp),
                },
            ),
            (true, None) => ResolvedModelCandidate::try_brokered_provider_with_reasoning(
                binding,
                provider_ref,
                route_ref,
                workspace.clone(),
                endpoint,
                reasoning,
            ),
            (false, Some(acp)) => ResolvedModelCandidate::try_provider_with_profile(
                binding,
                provider_ref,
                route_ref,
                workspace.clone(),
                credential,
                endpoint,
                ProviderExecutionProfile {
                    unspecified_reasoning: reasoning,
                    acp: Some(acp),
                },
            ),
            (false, None) => ResolvedModelCandidate::try_provider_with_reasoning(
                binding,
                provider_ref,
                route_ref,
                workspace.clone(),
                credential,
                endpoint,
                reasoning,
            ),
        }
        .map_err(|error| PublicationResolutionError::CandidateUnavailable {
            binding: error_binding,
            reason: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::unspecified_reasoning;
    use awaken_model_catalog::ApiDialect;
    use awaken_runtime_contract::UnspecifiedReasoning;

    #[test]
    fn catalog_identity_compiles_to_an_explicit_reasoning_policy() {
        // Cause/effect graph (catalog facts -> immutable runtime policy):
        // C1 DeepSeek + OpenAI Chat -> disable implicit thinking.
        // C2 DeepSeek + Anthropic compatibility -> retain provider default.
        // C3 DeepSeek + Gemini compatibility -> retain provider default.
        // C4 another vendor + OpenAI Chat -> retain provider default.
        // These edges prevent URL aliases and arbitrary request JSON from
        // changing policy after publication.
        assert_eq!(
            unspecified_reasoning("deepseek", ApiDialect::OpenAiChat),
            UnspecifiedReasoning::Disabled,
            "C1",
        );
        assert_eq!(
            unspecified_reasoning("deepseek", ApiDialect::AnthropicMessages),
            UnspecifiedReasoning::ProviderDefault,
            "C2",
        );
        assert_eq!(
            unspecified_reasoning("deepseek", ApiDialect::Gemini),
            UnspecifiedReasoning::ProviderDefault,
            "C3",
        );
        assert_eq!(
            unspecified_reasoning("openrouter", ApiDialect::OpenAiChat),
            UnspecifiedReasoning::ProviderDefault,
            "C4",
        );
    }
}
