//! Secret-free credential selection and Provider publication projection.

use awaken_config_resolver::{CredentialCandidateSet, SourceLookup, credential_candidates};
use awaken_credential_vault::{
    CredentialBinding, CredentialKind, CredentialPool, CredentialSource, CredentialStatus,
    SelectionPolicy,
};
use awaken_model_catalog::{Offering, ProviderCatalog};
use awaken_runtime_contract::resolved::{Backend, ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage, InferenceEndpoint,
};
use awaken_runtime_host::PublicationResolutionError;
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
            Some(offering.provider_id.as_str()),
            Some(&binding.backend_ref),
            None,
            Some(workspace.as_str()),
            sequence,
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
                    source.status == CredentialStatus::Active
                        && source.kind != CredentialKind::Env
                        && Self::credential_usage(binding, source).is_ok()
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
        binding: &ModelBinding,
        source: &CredentialSource,
    ) -> Result<CredentialUsage, String> {
        let Backend::Acp { cli } = Backend::from_ref(&binding.backend_ref) else {
            if source.env_key.as_deref()
                == Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV)
            {
                return Err("Claude Code setup tokens require backend acp:claude".into());
            }
            return Ok(CredentialUsage::ProviderAdapter);
        };
        let profile = awaken_run_executor_acp::acp_cli(&cli)
            .ok_or_else(|| format!("ACP backend {cli} is not in the executable catalog"))?;
        let Some(delivery) = profile.model_delivery else {
            return Ok(CredentialUsage::ProviderAdapter);
        };
        let Some(name) = source.env_key.as_deref() else {
            return Ok(CredentialUsage::ProviderAdapter);
        };
        if !delivery.supports_credential_env(name) {
            return Err(format!(
                "ACP backend {cli} does not accept credential environment {name}"
            ));
        }
        Ok(CredentialUsage::EnvironmentVariable {
            name: name.to_string(),
        })
    }

    pub(super) fn provider_candidate(
        catalog: &ProviderCatalog,
        workspace: &ScopeId,
        binding: ModelBinding,
        offering: &Offering,
        access: PublicationAccess<'_>,
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
        let credential = match access {
            PublicationAccess::Direct(credential) => credential
                .map(|credential| {
                    let revision = u64::try_from(credential.version).map_err(|_| {
                        unavailable(format!(
                            "credential {} has a negative version",
                            credential.id.0
                        ))
                    })?;
                    let usage =
                        Self::credential_usage(&binding, credential).map_err(unavailable)?;
                    Ok(CredentialAccess::new(
                        CredentialRef {
                            id: credential.id.0.clone(),
                            revision,
                        },
                        match credential.kind {
                            CredentialKind::WorkerLocal => {
                                CredentialMaterialSource::WorkerReference
                            }
                            CredentialKind::Vault | CredentialKind::Oauth => {
                                CredentialMaterialSource::ControlPlaneReference
                            }
                            CredentialKind::Env => {
                                return Err(unavailable(
                                    "environment credentials cannot be frozen into a publication"
                                        .into(),
                                ));
                            }
                        },
                        usage,
                        CredentialExecutionPolicy::self_hosted_provider(),
                    ))
                })
                .transpose()?,
            PublicationAccess::Brokered => None,
        };
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
        Ok(ResolvedModelCandidate::provider(
            binding,
            format!("{}@{}", offering.provider_id.0, provider.version),
            route_ref,
            workspace.clone(),
            credential,
            InferenceEndpoint {
                adapter_kind: endpoint.dialect.adapter_kind().to_string(),
                api_dialect: endpoint.dialect.as_str().to_string(),
                base_url,
                upstream_model: offering
                    .upstream_model
                    .clone()
                    .unwrap_or_else(|| offering.model_id.clone()),
            },
        ))
    }
}
