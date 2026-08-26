//! Reserved Assistant model selection at the startup boundary.
//!
//! This policy consumes existing catalog, credential, and local ACP observations.
//! It never authors another default, credential, or provider record.

use awaken_acp_application::AcpHostObservation;
use awaken_agent_config::ModelSelection;
use awaken_credential_vault::{CredentialKind, CredentialSource, CredentialStatus};
use awaken_model_catalog::{OfferingStatus, ProviderCatalog};

pub(crate) fn select(
    catalog: &ProviderCatalog,
    credentials: &[CredentialSource],
    observations: &[AcpHostObservation],
) -> Option<ModelSelection> {
    let detected = |cli_id: &str| {
        observations
            .iter()
            .any(|observation| observation.cli_id == cli_id && observation.detected())
    };
    let active = |source: &&CredentialSource| source.status == CredentialStatus::Active;

    if detected("claude")
        && let Some(offering) = catalog.offerings.iter().find(|offering| {
            offering.status == OfferingStatus::Active
                && offering.provider_id.as_str() == "anthropic"
        })
        && credentials.iter().any(|source| {
            source.is_claude_code_setup_token()
                && awaken_config_resolver::credential_is_executable_supply(
                    offering.provider_id.as_str(),
                    Some(offering.protocol_endpoint_id.as_str()),
                    "acp:claude",
                    source,
                )
        })
    {
        return Some(ModelSelection::pinned(
            "anthropic",
            &offering.model_id,
            "acp:claude",
        ));
    }

    if catalog.offerings.iter().any(|offering| {
        offering.status == OfferingStatus::Active
            && credentials.iter().any(|source| {
                source.kind != CredentialKind::WorkerLocal
                    && awaken_config_resolver::credential_is_executable_supply(
                        offering.provider_id.as_str(),
                        Some(offering.protocol_endpoint_id.as_str()),
                        "genai",
                        source,
                    )
            })
    }) {
        return Some(ModelSelection::Auto);
    }

    let available = awaken_run_executor_acp::known_acp_clis()
        .iter()
        .filter(|cli| {
            observations.iter().any(|observation| {
                observation.cli_id == cli.id
                    && observation.credential_state
                        == Some(awaken_runtime_contract::CredentialObservationState::Available)
                    && observation.capability_state
                        == Some(awaken_acp_application::AcpCapabilityState::Verified)
            }) && credentials.iter().filter(active).any(|source| {
                source.kind == CredentialKind::WorkerLocal
                    && source
                        .worker_local_binding
                        .as_ref()
                        .is_some_and(|binding| binding.driver_id == format!("acp:{}", cli.id))
            })
        })
        .collect::<Vec<_>>();
    if let [cli] = available.as_slice() {
        let awaken_runtime_contract::resolved::Backend::Acp(backend_ref) =
            awaken_runtime_contract::resolved::Backend::from_ref(&format!("acp:{}", cli.id))
        else {
            return None;
        };
        return Some(ModelSelection::BackendDefault {
            backend_ref,
            configuration: Default::default(),
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_contract::{
        CredentialDescriptor, CredentialMaterialDescriptor, CredentialPurpose, CredentialSourceId,
        CredentialTarget, CredentialTargetContract, CredentialUsage, HttpEffectPlacement,
        OPAQUE_SECRET_MATERIAL_TYPE,
    };
    use awaken_credential_vault::WorkerLocalBinding;
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
            descriptor: None,
            provider_id: provider.map(str::to_string),
            protocol_endpoint_id: None,
            env_key: env_key.map(str::to_string),
            material_ref: None,
            auxiliary_material_refs: Default::default(),
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
            capability_state: Some(awaken_acp_application::AcpCapabilityState::Verified),
            capability_fingerprint: Some("fixture".into()),
            capability_reason_code: None,
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
        // Cause/effect graph: C1 canonical provider API credential exists; C2
        // legacy setup token plus detected Claude exists; C3 Worker-local ACP is
        // uniquely ready; C4 a described non-Provider credential spoofs the
        // legacy setup-token env key. Effects: E1 Auto; E2 pinned acp:claude;
        // E3 backend default; E4 no selection from the spoofed source.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|---|
        // | S1 | Y | N | - | N | E1 |
        // | S2 | - | Y | - | N | E2 |
        // | S3 | N | N | Y | N | E3 |
        // | S4 | N | env-key only | N | Y | E4 |
        let catalog = anthropic_catalog();
        let api_key = credential("api", CredentialKind::Vault, Some("anthropic"), None, None);
        assert_eq!(
            select(&catalog, &[api_key], &[observation("codex")]),
            Some(ModelSelection::Auto)
        );

        let setup_token = credential(
            "setup",
            CredentialKind::Vault,
            Some("anthropic"),
            Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV),
            None,
        );
        assert_eq!(
            select(&catalog, &[setup_token], &[observation("claude")]),
            Some(ModelSelection::pinned(
                "anthropic",
                "claude-sonnet",
                "acp:claude"
            ))
        );

        let mut described_spoof = credential(
            "described-spoof",
            CredentialKind::Vault,
            None,
            Some(awaken_credential_vault::CLAUDE_CODE_SETUP_TOKEN_ENV),
            None,
        );
        described_spoof.descriptor = Some(CredentialDescriptor::new(
            "anthropic",
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
            [CredentialTargetContract::new(
                CredentialTarget::new(
                    CredentialPurpose::HttpEffect,
                    "https://connector.example.test/invoke",
                ),
                CredentialUsage::HttpEffect {
                    fields: std::collections::BTreeMap::from([(
                        "token".into(),
                        std::collections::BTreeSet::from([HttpEffectPlacement::Header {
                            name: "authorization".into(),
                        }]),
                    )]),
                },
            )],
        ));
        assert_eq!(
            select(&catalog, &[described_spoof], &[observation("claude")]),
            None,
            "S4/E4"
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
            Some(
                ModelSelection::try_backend_default("acp:codex", Default::default())
                    .expect("exact ACP backend"),
            )
        );
    }

    #[test]
    fn local_auto_selection_requires_exactly_one_available_verified_backend() {
        // Cause graph:
        // C1 detected only -> E1 no automatic selection.
        // C2 login Available + capability Verified + binding -> E2 candidate.
        // C3 candidate cardinality -> E3 exactly one selects; zero/many do not.
        //
        // Decision table:
        // S1 zero usable candidates -> None
        // S2 one usable candidate   -> BackendDefault
        // S3 two usable candidates  -> None (explicit persisted choice required)
        let codex = credential(
            "codex-worker",
            CredentialKind::WorkerLocal,
            None,
            None,
            Some("acp:codex"),
        );
        let claude = credential(
            "claude-worker",
            CredentialKind::WorkerLocal,
            None,
            None,
            Some("acp:claude"),
        );

        let mut unavailable = observation("codex");
        unavailable.credential_state =
            Some(awaken_runtime_contract::CredentialObservationState::LoginRequired);
        assert_eq!(
            select(
                &ProviderCatalog::default(),
                std::slice::from_ref(&codex),
                &[unavailable]
            ),
            None,
            "S1"
        );
        assert_eq!(
            select(
                &ProviderCatalog::default(),
                std::slice::from_ref(&codex),
                &[observation("codex")]
            ),
            Some(
                ModelSelection::try_backend_default("acp:codex", Default::default())
                    .expect("exact ACP backend"),
            ),
            "S2"
        );
        assert_eq!(
            select(
                &ProviderCatalog::default(),
                &[codex, claude],
                &[observation("codex"), observation("claude")]
            ),
            None,
            "S3"
        );
    }
}
