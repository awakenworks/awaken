//! Test model-publication adapter for the scenario host's live model catalog.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::{
    CredentialAccess, CredentialEnvelope, CredentialExecutionPolicy, CredentialMaterialSource,
    CredentialRef, CredentialUsage, SealedCredentialEnvelopeRef, TrustDomainRef,
};

const DISTRIBUTED_PROVIDER_REF: &str = "adr71-provider@1";
const DISTRIBUTED_ROUTE_REF: &str = "adr71-anthropic@1";
const DISTRIBUTED_CREDENTIAL_ID: &str = "adr71-provider-credential";
const DISTRIBUTED_ENVELOPE_ID: &str = "adr71-envelope";
const DISTRIBUTED_PAYLOAD_FINGERPRINT: &str = "sha256:adr71-provider-payload";
const DISTRIBUTED_PROVIDER_BASE_URL: &str = "http://provider:3000/v1/";

pub(crate) async fn scenario_model_catalog(
    model_ref: &str,
) -> Arc<dyn awaken_model_catalog::repo::CatalogRepo> {
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};

    let catalog: Arc<dyn CatalogRepo> = Arc::new(InMemoryCatalogRepo::new());
    catalog
        .put_provider(awaken_model_catalog::Provider {
            id: awaken_model_catalog::ProviderId::new("default"),
            slug: "default".into(),
            display_name: "Default".into(),
            version: 1,
        })
        .await
        .expect("put scenario provider");
    catalog
        .put_endpoint(awaken_model_catalog::ProtocolEndpoint {
            id: awaken_model_catalog::ProtocolEndpointId::new("ep"),
            provider_id: awaken_model_catalog::ProviderId::new("default"),
            dialect: awaken_model_catalog::ApiDialect::AnthropicMessages,
            base_url: None,
            timeout_secs: 30,
            display_name: "ep".into(),
            version: 1,
        })
        .await
        .expect("put scenario endpoint");
    catalog
        .put_offering(awaken_model_catalog::Offering {
            model_id: model_ref.into(),
            provider_id: awaken_model_catalog::ProviderId::new("default"),
            protocol_endpoint_id: awaken_model_catalog::ProtocolEndpointId::new("ep"),
            dialect: awaken_model_catalog::ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: Default::default(),
            last_seen_at_unix_ms: None,
        })
        .await
        .expect("put scenario offering");
    catalog
}

pub(crate) struct ScenarioHostModelResolver {
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
}

impl ScenarioHostModelResolver {
    pub(crate) fn new(catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>) -> Self {
        Self { catalog }
    }
}

#[async_trait::async_trait]
impl awaken_config_service::ModelPublicationResolver for ScenarioHostModelResolver {
    async fn resolve_models(
        &self,
        _workspace: &awaken_tenancy::ScopeId,
        selection: &awaken_agent_config::ModelSelection,
        fallbacks: &[ModelBinding],
    ) -> Result<
        awaken_config_service::ResolvedPublicationModels,
        awaken_config_service::PublicationResolutionError,
    > {
        let catalog = self.catalog.snapshot().await.map_err(|error| {
            awaken_config_service::PublicationResolutionError::CatalogUnavailable(error.to_string())
        })?;
        let (primary, fallbacks) = if let Some(primary) = selection.resolved() {
            (primary.clone(), fallbacks.to_vec())
        } else {
            let mut offerings = catalog.offerings.iter();
            let primary = offerings
                .next()
                .ok_or(awaken_config_service::PublicationResolutionError::MissingPrimary)?;
            let binding = |offering: &awaken_model_catalog::Offering| {
                ModelBinding::new(&offering.provider_id.0, &offering.model_id, "genai")
            };
            (binding(primary), offerings.map(binding).collect())
        };
        let context_window = catalog.context_window(&primary.model_ref);
        let max_output_tokens = catalog.max_output_tokens(&primary.model_ref);
        Ok(awaken_config_service::ResolvedPublicationModels::host(
            primary,
            fallbacks,
            context_window,
            max_output_tokens,
        ))
    }
}

/// Deployment-bound projection adapter for the distributed Worker proof.
/// Catalog selection remains authoritative in `ScenarioHostModelResolver`; this
/// adapter changes only how the selected bindings are executed.
pub(crate) struct DistributedProviderPublicationResolver {
    catalog: ScenarioHostModelResolver,
}

impl DistributedProviderPublicationResolver {
    pub(crate) fn new(catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>) -> Self {
        Self {
            catalog: ScenarioHostModelResolver::new(catalog),
        }
    }
}

#[async_trait::async_trait]
impl awaken_config_service::ModelPublicationResolver for DistributedProviderPublicationResolver {
    async fn resolve_models(
        &self,
        workspace: &awaken_tenancy::ScopeId,
        selection: &awaken_agent_config::ModelSelection,
        fallbacks: &[ModelBinding],
    ) -> Result<
        awaken_config_service::ResolvedPublicationModels,
        awaken_config_service::PublicationResolutionError,
    > {
        let selected = self
            .catalog
            .resolve_models(workspace, selection, fallbacks)
            .await?;
        let candidate = |binding: ModelBinding| {
            let upstream_model = binding.model_ref.clone();
            awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
                binding,
                DISTRIBUTED_PROVIDER_REF,
                DISTRIBUTED_ROUTE_REF,
                workspace.clone(),
                Some(
                    CredentialAccess::new(
                        CredentialRef {
                            id: DISTRIBUTED_CREDENTIAL_ID.into(),
                            revision: 1,
                        },
                        CredentialMaterialSource::ControlPlaneReference,
                        CredentialUsage::ProviderAdapter,
                        CredentialExecutionPolicy::self_hosted_provider(),
                    )
                    .with_envelope(CredentialEnvelope::SealedForWorker {
                        envelope_ref: SealedCredentialEnvelopeRef {
                            id: DISTRIBUTED_ENVELOPE_ID.into(),
                            payload_fingerprint: DISTRIBUTED_PAYLOAD_FINGERPRINT.into(),
                        },
                        recipient: TrustDomainRef("awaken.worker".into()),
                        expires_at_unix_ms: u64::MAX,
                    }),
                ),
                awaken_runtime_contract::InferenceEndpoint {
                    adapter_kind: "anthropic".into(),
                    api_dialect: "anthropic_messages".into(),
                    base_url: DISTRIBUTED_PROVIDER_BASE_URL.into(),
                    upstream_model,
                    processing_placement: None,
                },
            )
        };
        Ok(awaken_config_service::ResolvedPublicationModels {
            primary: candidate(selected.primary.binding),
            candidates: selected
                .candidates
                .into_iter()
                .map(|candidate_model| candidate(candidate_model.binding))
                .collect(),
            context_window: selected.context_window,
            max_output_tokens: selected.max_output_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use awaken_config_service::ModelPublicationResolver as _;

    use super::*;

    #[tokio::test]
    async fn one_catalog_drives_explicit_and_auto_scenario_publication() {
        // Cause/effect decision table: M1 explicit model selection -> preserve
        // the authored binding; M2 Auto with one offering -> select that exact
        // offering; M3 Auto with an empty catalog -> fail closed as missing
        // primary. Both adapters reuse this one catalog selection implementation.
        let workspace = awaken_tenancy::ScopeId::from("workspace-a");
        let resolver = ScenarioHostModelResolver::new(scenario_model_catalog("echo").await);
        let explicit = ModelBinding::new("default", "echo", "default");
        let resolved = resolver
            .resolve_models(
                &workspace,
                &awaken_agent_config::ModelSelection::Pinned(explicit.clone()),
                &[],
            )
            .await
            .expect("M1 explicit model");
        assert_eq!(resolved.primary.binding, explicit, "M1");
        let resolved = resolver
            .resolve_models(&workspace, &awaken_agent_config::ModelSelection::Auto, &[])
            .await
            .expect("M2 Auto model");
        assert_eq!(resolved.primary.binding.model_ref, "echo", "M2");

        let empty = ScenarioHostModelResolver::new(Arc::new(
            awaken_model_catalog::repo::InMemoryCatalogRepo::new(),
        ));
        assert!(
            empty
                .resolve_models(&workspace, &awaken_agent_config::ModelSelection::Auto, &[])
                .await
                .is_err(),
            "M3"
        );
    }

    #[tokio::test]
    async fn distributed_projection_is_provider_and_recipient_bound() {
        // Cause/effect decision table: P1 selected catalog binding -> exact
        // Provider route; P2 ControlPlaneReference credential -> sealed Worker
        // envelope with no plaintext; P3 unavailable Auto selection -> the
        // canonical catalog resolver fails closed rather than a Host fallback.
        let workspace = awaken_tenancy::ScopeId::from("workspace-a");
        let resolver =
            DistributedProviderPublicationResolver::new(scenario_model_catalog("echo").await);
        let resolved = resolver
            .resolve_models(&workspace, &awaken_agent_config::ModelSelection::Auto, &[])
            .await
            .expect("P1 selected Provider model");
        let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(access),
            endpoint,
            ..
        } = &resolved.primary.provisioning
        else {
            panic!("P1 must publish one Provider candidate");
        };
        assert_eq!(access.credential.id, DISTRIBUTED_CREDENTIAL_ID, "P1");
        assert!(matches!(
            access.envelope,
            Some(CredentialEnvelope::SealedForWorker { .. })
        ));
        assert_eq!(endpoint.base_url, DISTRIBUTED_PROVIDER_BASE_URL, "P2");

        let empty = DistributedProviderPublicationResolver::new(Arc::new(
            awaken_model_catalog::repo::InMemoryCatalogRepo::new(),
        ));
        assert!(
            empty
                .resolve_models(&workspace, &awaken_agent_config::ModelSelection::Auto, &[])
                .await
                .is_err(),
            "P3"
        );
    }
}
