// @formal surface obligation=config.inference_access_resolved_once_at_publication
// @formal surface obligation=credential_reference.exact_revision_never_resolves_another_revision
// @formal reason: Binds exact credential evidence into immutable model publication.
//! Secret-free credential selection and Provider publication projection.

use awaken_config_resolver::{
    CredentialCandidateSet, CredentialSelectionContext, SourceLookup, credential_candidates,
    credential_is_executable_supply,
};
use awaken_config_service::PublicationResolutionError;
use awaken_credential_contract::{CredentialPurpose, CredentialTarget};
use awaken_credential_vault::{
    CredentialBinding, CredentialPool, CredentialSource, ExactCredentialAccessRequest,
    SelectionPolicy, publish_exact_credential_access,
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
                backend_ref: Some(binding.identity().2),
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
                        binding.identity().2,
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
        let Backend::Acp(backend) = Backend::from_ref(binding.identity().2) else {
            if source.is_claude_code_setup_token() {
                return Err("Claude Code setup tokens require backend acp:claude".into());
            }
            return Ok(CredentialUsage::ProviderAdapter);
        };
        let cli = backend.cli();
        let capability = self
            .acp_capabilities
            .iter()
            .find(|capability| capability.backend_ref == binding.identity().2)
            .ok_or_else(|| format!("ACP backend {cli} is not in the executable catalog"))?;
        capability
            .credential_usage(source.process_secret_environment_hint())
            .map_err(|error| format!("ACP backend {cli}: {error}"))
    }

    pub(super) async fn provider_candidate(
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
        let route_ref = format!("{}@{}", offering.protocol_endpoint_id.0, endpoint.version);
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
            PublicationAccess::Direct(Some(credential)) => {
                let usage = self
                    .credential_usage(&binding, credential)
                    .map_err(unavailable)?;
                let material_binding = CredentialMaterialBinding::for_target(
                    workspace.as_str(),
                    &(&provider_ref, &endpoint),
                    &usage,
                );
                let selected_holder = self.direct_holders.as_ref().map(|holders| holders.for_backend(binding.identity().2)).ok_or_else(|| {
                        unavailable(
                            "direct provider credential publication requires an exact deployment-selected plaintext holder"
                                .into(),
                        )
                    })?;
                Some(
                    publish_exact_credential_access(
                        credential,
                        ExactCredentialAccessRequest {
                            workspace_id: Some(workspace.as_str()),
                            target: Some(CredentialTarget::new(
                                CredentialPurpose::ProviderAdapter,
                                offering.provider_id.as_str(),
                            )),
                            usage,
                            policy: self.direct_credential_policy.clone(),
                            selected_holder,
                            binding: &material_binding,
                            now_unix_ms: super::wall_clock_ms(),
                        },
                        self.credential_material_delivery
                            .as_ref()
                            .map(|(secrets, delivery)| (secrets.as_ref(), delivery)),
                    )
                    .await
                    .map_err(|error| unavailable(error.to_string()))?,
                )
            }
            PublicationAccess::Direct(None) => None,
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
    use super::super::tests::{
        catalog_with_dialect, direct_resolver, test_acp_capabilities, verified_acp_worker,
    };
    use super::unspecified_reasoning;
    use super::*;
    use awaken_agent_config::ModelSelection;
    use awaken_agent_contract::RedactedString;
    use awaken_config_service::ModelPublicationResolver;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialKind, CredentialStatus, InMemorySecretStore, SecretStore,
    };
    use awaken_model_catalog::ApiDialect;
    use awaken_runtime_contract::UnspecifiedReasoning;
    use awaken_runtime_contract::resolved::ModelProvisioning;
    use std::sync::{Arc, Mutex};

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
    struct ProviderDeliveryIssuer {
        fault: u8,
        calls: Mutex<Vec<awaken_credential_contract::CredentialEnvelopeIssuance>>,
    }

    #[async_trait::async_trait]
    impl awaken_credential_contract::CredentialEnvelopeIssuer for ProviderDeliveryIssuer {
        async fn issue(
            &self,
            request: awaken_credential_contract::CredentialEnvelopeIssuance,
        ) -> Result<awaken_credential_contract::CredentialEnvelope, String> {
            use awaken_credential_contract::{
                CredentialEnvelope, SealedCredentialEnvelopeRef,
                credential_envelope_payload_fingerprint,
            };
            let fingerprint = credential_envelope_payload_fingerprint(
                &request.access,
                &request.selected_holder,
                &request.binding,
            );
            let recipient = request.selected_holder.trust_domain.clone();
            let workload = request.selected_holder.boundary
                == awaken_credential_contract::PlaintextBoundary::Workload;
            self.calls.lock().unwrap().push(request);
            if self.fault == 1 {
                return Err("injected delivery unavailable".into());
            }
            let envelope = CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: if self.fault == 5 {
                        ""
                    } else {
                        "provider-envelope"
                    }
                    .into(),
                    payload_fingerprint: if self.fault == 2 {
                        "wrong-payload".into()
                    } else {
                        fingerprint
                    },
                },
                recipient: if self.fault == 3 {
                    awaken_credential_contract::TrustDomainRef("wrong-worker".into())
                } else {
                    recipient
                },
                expires_at_unix_ms: if self.fault == 4 { 0 } else { u64::MAX },
            };
            Ok(match envelope {
                CredentialEnvelope::SealedForWorker {
                    envelope_ref,
                    recipient,
                    expires_at_unix_ms,
                } if workload => CredentialEnvelope::SealedForWorkload {
                    envelope_ref,
                    recipient,
                    expires_at_unix_ms,
                },
                envelope => envelope,
            })
        }
    }

    #[tokio::test]
    async fn native_and_claude_provider_publications_deliver_exact_material_or_fail_closed() {
        // Causes: C1 Native/Claude select their canonical distinct usage AND
        // exact Worker/Workload holder; using Native envelope for ACP must fail; C2
        // installed issuer returns exact/failure/payload drift/recipient drift/
        // expiry/empty identity; C3 source active/disabled; C4 material present/
        // absent. Effects: D1 exact delivery -> one envelope and one secret read
        // for the selected source; D2 any issuer fault -> no candidate, no bare
        // reference fallback; D3 disabled/missing material -> no issuer call.
        // Each D rule crosses both backends. Existing no-delivery selection
        // tests preserve AllInOne direct-reference behavior. This is publication
        // integration through real Vault I/O, not a claim of vendor execution.
        for backend in ["genai", "acp:claude"] {
            for fault in 0..8 {
                let credentials = Arc::new(InMemoryCredentialRepo::new());
                let secrets = Arc::new(InMemorySecretStore::new());
                let source = enter_credential(
                    CredentialCreateParams {
                        workspace_id: "workspace-a".into(),
                        kind: CredentialKind::Vault,
                        provider_id: Some("openai".into()),
                        env_key: Some("ANTHROPIC_API_KEY".into()),
                        secret: Some(RedactedString::new("publication-test-secret")),
                        oauth_command: None,
                    },
                    secrets.as_ref(),
                    credentials.as_ref(),
                )
                .await
                .unwrap();
                if fault == 6 {
                    let mut disabled = source.clone();
                    disabled.status = CredentialStatus::Disabled;
                    credentials.put(disabled).await.unwrap();
                }
                if fault == 7 {
                    secrets
                        .delete(source.material_ref.as_ref().unwrap())
                        .await
                        .unwrap();
                }
                let issuer = Arc::new(ProviderDeliveryIssuer {
                    fault,
                    calls: Mutex::new(Vec::new()),
                });
                let resolver = direct_resolver(
                    catalog_with_dialect(&["test-model"], ApiDialect::AnthropicMessages),
                    credentials,
                )
                .with_acp_capabilities(test_acp_capabilities())
                .with_worker_observations(
                    verified_acp_worker("provider-managed", "acp:claude", "sha256:delivery").await,
                )
                .with_credential_material_delivery(
                    secrets,
                    Some(
                        awaken_credential_contract::CredentialMaterialDelivery::RecipientEnvelope(
                            issuer.clone(),
                        ),
                    ),
                );
                let result = resolver
                    .resolve_models(
                        &ScopeId::from("workspace-a"),
                        &ModelSelection::Target {
                            target: awaken_config_resolver::ModelTarget {
                                model_id: "test-model".into(),
                                provider_id: Some("openai".into()),
                                api_dialect: None,
                                protocol_endpoint_id: None,
                                endpoint_name: None,
                            },
                            backend_ref: backend.into(),
                            configuration: Default::default(),
                        },
                        &[],
                    )
                    .await;
                let calls = issuer.calls.lock().unwrap();
                assert_eq!(
                    calls.len(),
                    usize::from(fault < 6),
                    "{backend}/D{fault}: {:?}",
                    result.as_ref().err()
                );
                if fault == 0 {
                    let resolved = result.expect("D1 exact delivery");
                    let ModelProvisioning::Provider {
                        credential: Some(access),
                        provider_ref,
                        endpoint,
                        ..
                    } = resolved.primary.provisioning()
                    else {
                        panic!("D1 provider");
                    };
                    assert!(
                        access.envelope.is_some(),
                        "D1 regression: formerly bare reference"
                    );
                    let issued = &calls[0];
                    let holder = if backend == "genai" {
                        awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
                            .inference_holder
                    } else {
                        awaken_runtime_contract::CredentialRealizationProfile::self_hosted_acp()
                            .inference_holder
                    };
                    assert_eq!(issued.selected_holder, holder, "D1 backend recipient");
                    access
                        .envelope
                        .as_ref()
                        .unwrap()
                        .validate_for_holder(&holder, super::super::wall_clock_ms())
                        .unwrap();
                    assert_eq!(issued.access.credential.id, source.id.0);
                    assert_eq!(issued.access.credential.revision, source.version as u64);
                    assert_eq!(issued.material.expose_secret(), "publication-test-secret");
                    assert_eq!(
                        issued.binding,
                        awaken_credential_contract::CredentialMaterialBinding::for_target(
                            "workspace-a",
                            &(provider_ref, endpoint),
                            &access.usage
                        )
                    );
                    assert_eq!(
                        access.usage,
                        if backend == "genai" {
                            CredentialUsage::ProviderAdapter
                        } else {
                            CredentialUsage::process_env("ANTHROPIC_API_KEY")
                        }
                    );
                } else {
                    assert!(result.is_err(), "{backend}/D{fault} must fail closed");
                }
            }
        }
    }
}
