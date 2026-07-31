//! A2A Agent Card discovery and remote-counterparty publication.

use awaken_config_resolver::{
    CredentialCandidateSet, CredentialSelectionContext, credential_candidates, derive_vendor_pool,
};
use awaken_config_service::PublicationResolutionError;
use awaken_credential_vault::{
    CredentialBinding, CredentialKind, CredentialSource, CredentialStatus,
};
use awaken_runtime_contract::resolved::{Backend, ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
};
use awaken_tenancy::ScopeId;

use super::{CatalogModelPublicationResolver, PublicationCredentialLookup};

#[async_trait::async_trait]
pub trait A2aCardDiscovery: Send + Sync {
    async fn discover(&self, endpoint: &str) -> Result<awaken_protocol_a2a::AgentCard, String>;
}

pub(super) struct HttpA2aCardDiscovery;

#[async_trait::async_trait]
impl A2aCardDiscovery for HttpA2aCardDiscovery {
    async fn discover(&self, endpoint: &str) -> Result<awaken_protocol_a2a::AgentCard, String> {
        let transport = awaken_protocol_a2a::HttpTransport::new(endpoint);
        awaken_protocol_a2a::client::agent_card(&transport)
            .await
            .map_err(|error| error.to_string())
    }
}

impl CatalogModelPublicationResolver {
    pub(super) fn remote_origin(endpoint: &str) -> Result<String, String> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|error| format!("invalid A2A endpoint URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("A2A endpoint must be an absolute HTTP(S) URL".into());
        }
        Ok(url.origin().ascii_serialization())
    }

    pub(super) async fn remote_candidate(
        &self,
        workspace: &ScopeId,
        binding: ModelBinding,
        sources: &[CredentialSource],
    ) -> Result<ResolvedModelCandidate, PublicationResolutionError> {
        let Backend::Remote { endpoint } = Backend::from_ref(&binding.backend_ref) else {
            unreachable!("caller matched Remote");
        };
        let unavailable = |reason| PublicationResolutionError::CandidateUnavailable {
            binding: binding.clone(),
            reason,
        };
        let origin = Self::remote_origin(&endpoint).map_err(unavailable)?;
        let card = self
            .a2a_cards
            .discover(&endpoint)
            .await
            .map_err(|error| unavailable(format!("discover A2A Agent Card: {error}")))?;
        let card_origin = Self::remote_origin(&card.url)
            .map_err(|error| unavailable(format!("invalid Agent Card URL: {error}")))?;
        if card_origin != origin {
            return Err(unavailable(
                "A2A Agent Card URL belongs to a different origin".into(),
            ));
        }
        let security =
            crate::a2a_security::project_agent_card_security(&card).map_err(unavailable)?;
        let credential = if security.anonymous {
            None
        } else {
            let usage =
                security.accepted_headers.first().cloned().ok_or_else(|| {
                    unavailable("A2A card has no supported authentication".into())
                })?;
            let pool = derive_vendor_pool(
                workspace.as_str(),
                &origin,
                None,
                &binding.backend_ref,
                sources,
            );
            let derived = CredentialBinding::OneOfCredentialPool {
                credential_pool_id: pool.id.clone(),
            };
            let lookup = PublicationCredentialLookup {
                sources,
                pool: Some(&pool),
            };
            let candidates = credential_candidates(
                &derived,
                &lookup,
                CredentialSelectionContext {
                    offering_provider: Some(&origin),
                    offering_endpoint: None,
                    backend_ref: Some(&binding.backend_ref),
                    availability: None,
                    expected_workspace: Some(workspace.as_str()),
                    selection_sequence: 0,
                },
            )
            .map_err(|error| unavailable(error.to_string()))?;
            let source = match candidates {
                CredentialCandidateSet::Direct { sources, .. } => sources
                    .into_iter()
                    .find(|source| {
                        source.status == CredentialStatus::Active
                            && matches!(
                                source.kind,
                                CredentialKind::Vault
                                    | CredentialKind::Oauth
                            )
                    })
                    .ok_or_else(|| {
                        unavailable(format!(
                            "A2A Agent Card requires authentication, but Workspace {workspace} has no active credential for {origin}"
                        ))
                    })?,
                CredentialCandidateSet::None
                | CredentialCandidateSet::Brokered => {
                    return Err(unavailable(
                        "A2A Agent Card requires a locally materializable credential"
                            .into(),
                    ));
                }
            };
            let revision = u64::try_from(source.version)
                .ok()
                .filter(|revision| *revision > 0)
                .ok_or_else(|| unavailable("A2A credential has an invalid revision".into()))?;
            Some(CredentialAccess::new(
                CredentialRef {
                    id: source.id.0.clone(),
                    revision,
                },
                CredentialMaterialSource::ControlPlaneReference,
                usage,
                CredentialExecutionPolicy::self_hosted_provider(),
            ))
        };
        Ok(ResolvedModelCandidate::remote(
            binding,
            workspace.clone(),
            credential,
            security.fingerprint,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_config_service::ModelPublicationResolver;
    use awaken_config_store::ModelSelection;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, InMemorySecretStore};
    use awaken_model_catalog::ProviderCatalog;
    use awaken_runtime_contract::CredentialUsage;
    use awaken_runtime_contract::resolved::ModelProvisioning;
    use std::sync::Arc;

    struct FixedA2aCard(awaken_protocol_a2a::AgentCard);

    #[async_trait::async_trait]
    impl A2aCardDiscovery for FixedA2aCard {
        async fn discover(
            &self,
            _endpoint: &str,
        ) -> Result<awaken_protocol_a2a::AgentCard, String> {
            Ok(self.0.clone())
        }
    }

    fn remote_selection() -> ModelSelection {
        ModelSelection::Pinned(awaken_runtime_contract::resolved::ModelBinding::new(
            "",
            "",
            "a2a:https://agent.example/service",
        ))
    }

    #[tokio::test]
    async fn publication_freezes_card_security_and_exact_counterparty_credential() {
        // Cause graph: C1 the Agent Card permits anonymous access; C2 it
        // instead requires one supported HTTP header; C3 the Workspace has an
        // active origin-tagged credential. Effects: E1 publish Remote without
        // material; E2 publish Remote with an exact revision/usage; E3 required
        // auth with no credential fails closed.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | A1   | Y  | N  | -  | E1     |
        // | A2   | N  | Y  | Y  | E2     |
        // | A3   | N  | Y  | N  | E3     |
        let anonymous_repo = Arc::new(InMemoryCredentialRepo::new());
        let mut anonymous_card = awaken_protocol_a2a::agent_card("remote");
        anonymous_card.url = "https://agent.example/a2a".into();
        let anonymous = CatalogModelPublicationResolver::new(
            ProviderCatalog::default(),
            anonymous_repo.clone(),
        )
        .with_a2a_card_discovery(Arc::new(FixedA2aCard(anonymous_card)))
        .resolve_models(&ScopeId::from("workspace-a"), &remote_selection(), &[])
        .await
        .expect("A1");
        assert!(matches!(
            anonymous.primary.provisioning,
            ModelProvisioning::Remote {
                credential: None,
                ..
            }
        ));

        let mut required_card = awaken_protocol_a2a::agent_card("remote");
        required_card.url = "https://agent.example/a2a".into();
        required_card.security_schemes = serde_json::from_value(serde_json::json!({
            "bearer": {"type": "http", "scheme": "Bearer"}
        }))
        .unwrap();
        required_card.security =
            serde_json::from_value(serde_json::json!([{"bearer": []}])).unwrap();
        let without_credential =
            CatalogModelPublicationResolver::new(ProviderCatalog::default(), anonymous_repo)
                .with_a2a_card_discovery(Arc::new(FixedA2aCard(required_card.clone())))
                .resolve_models(&ScopeId::from("workspace-a"), &remote_selection(), &[])
                .await
                .expect_err("A3");
        assert!(
            without_credential
                .to_string()
                .contains("requires authentication")
        );

        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let entered = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("https://agent.example".into()),
                env_key: None,
                secret: Some(RedactedString::new("remote-secret")),
                oauth_command: None,
            },
            &InMemorySecretStore::new(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let authenticated =
            CatalogModelPublicationResolver::new(ProviderCatalog::default(), credentials)
                .with_a2a_card_discovery(Arc::new(FixedA2aCard(required_card)))
                .resolve_models(&ScopeId::from("workspace-a"), &remote_selection(), &[])
                .await
                .expect("A2");
        let ModelProvisioning::Remote {
            credential: Some(access),
            security_fingerprint,
            ..
        } = authenticated.primary.provisioning
        else {
            panic!("A2 must freeze Remote credential authority");
        };
        assert_eq!(access.credential.id, entered.id.0);
        assert_eq!(
            access.usage,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            }
        );
        assert!(security_fingerprint.starts_with("sha256:"));
    }
}
