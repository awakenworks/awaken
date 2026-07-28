//! Reserved Assistant model selection at the composition boundary.
//!
//! This policy consumes existing catalog, credential, and local ACP observations.
//! It never authors another default, credential, or provider record.

use awaken_acp_application::AcpHostObservation;
use awaken_config_store::ModelSelection;
use awaken_credential_vault::{
    CLAUDE_CODE_SETUP_TOKEN_ENV, CredentialKind, CredentialSource, CredentialStatus,
};
use awaken_model_catalog::{OfferingStatus, ProviderCatalog};

pub(crate) fn select(
    catalog: &ProviderCatalog,
    credentials: &[CredentialSource],
    observations: &[AcpHostObservation],
) -> ModelSelection {
    let detected = |cli_id: &str| {
        observations
            .iter()
            .any(|observation| observation.cli_id == cli_id && observation.detected())
    };
    let active = |source: &&CredentialSource| source.status == CredentialStatus::Active;

    if detected("claude")
        && credentials.iter().filter(active).any(|source| {
            source.env_key.as_deref() == Some(CLAUDE_CODE_SETUP_TOKEN_ENV)
                && source.provider_id.as_deref() == Some("anthropic")
        })
        && let Some(offering) = catalog.offerings.iter().find(|offering| {
            offering.status == OfferingStatus::Active
                && offering.provider_id.as_str() == "anthropic"
        })
    {
        return ModelSelection::pinned("anthropic", &offering.model_id, "acp:claude");
    }

    if catalog.offerings.iter().any(|offering| {
        offering.status == OfferingStatus::Active
            && credentials.iter().filter(active).any(|source| {
                source.env_key.as_deref() != Some(CLAUDE_CODE_SETUP_TOKEN_ENV)
                    && source.kind != CredentialKind::WorkerLocal
                    && awaken_config_resolver::can_consume(offering.provider_id.as_str(), source)
            })
    }) {
        return ModelSelection::Auto;
    }

    if let Some(cli) = awaken_run_executor_acp::known_acp_clis()
        .iter()
        .find(|cli| {
            detected(cli.id)
                && credentials.iter().filter(active).any(|source| {
                    source.kind == CredentialKind::WorkerLocal
                        && source
                            .worker_local_binding
                            .as_ref()
                            .is_some_and(|binding| binding.driver_id == format!("acp:{}", cli.id))
                })
        })
    {
        return ModelSelection::BackendDefault {
            backend_ref: format!("acp:{}", cli.id),
        };
    }

    ModelSelection::Auto
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::{CredentialSourceId, WorkerLocalBinding};
    use awaken_model_catalog::{ApiDialect, Offering, ProtocolEndpointId, ProviderId};

    fn credential(
        id: &str,
        kind: CredentialKind,
        provider: Option<&str>,
        env_key: Option<&str>,
        worker_driver: Option<&str>,
    ) -> CredentialSource {
        CredentialSource {
            id: CredentialSourceId(id.into()),
            workspace_id: "workspace".into(),
            kind,
            provider_id: provider.map(str::to_string),
            env_key: env_key.map(str::to_string),
            material_ref: None,
            oauth_command: None,
            worker_local_binding: worker_driver
                .map(|driver| WorkerLocalBinding::new(driver, "local-user")),
            status: CredentialStatus::Active,
            version: 1,
        }
    }

    fn observation(cli_id: &str) -> AcpHostObservation {
        AcpHostObservation {
            cli_id: cli_id.into(),
            display_name: cli_id.into(),
            detection: awaken_acp_application::AcpDetectionState::Detected,
            version: Some("1".into()),
            credential_state: Some(awaken_runtime_contract::CredentialObservationState::Available),
            reason_code: None,
        }
    }

    fn anthropic_catalog() -> ProviderCatalog {
        let mut catalog = ProviderCatalog::default();
        catalog.offerings.push(Offering {
            model_id: "claude-sonnet".into(),
            provider_id: ProviderId::new("anthropic"),
            protocol_endpoint_id: ProtocolEndpointId::new("anthropic-main"),
            dialect: ApiDialect::AnthropicMessages,
            upstream_model: None,
            source: Default::default(),
            status: OfferingStatus::Active,
            last_seen_at_unix_ms: None,
        });
        catalog
    }

    #[test]
    fn one_existing_executable_path_is_selected() {
        // | Provider API | setup token + Claude | Worker-local ACP | Choice |
        // | yes | no | any | Auto provider |
        // | any | yes | any | pinned acp:claude |
        // | no | no | yes | ACP backend default |
        let catalog = anthropic_catalog();
        let api_key = credential("api", CredentialKind::Vault, Some("anthropic"), None, None);
        assert_eq!(
            select(&catalog, &[api_key], &[observation("codex")]),
            ModelSelection::Auto
        );

        let setup_token = credential(
            "setup",
            CredentialKind::Vault,
            Some("anthropic"),
            Some(CLAUDE_CODE_SETUP_TOKEN_ENV),
            None,
        );
        assert_eq!(
            select(&catalog, &[setup_token], &[observation("claude")]),
            ModelSelection::pinned("anthropic", "claude-sonnet", "acp:claude")
        );

        let worker = credential(
            "worker",
            CredentialKind::WorkerLocal,
            None,
            None,
            Some("acp:codex"),
        );
        assert_eq!(
            select(
                &ProviderCatalog::default(),
                &[worker],
                &[observation("codex")],
            ),
            ModelSelection::BackendDefault {
                backend_ref: "acp:codex".into()
            }
        );
    }
}
