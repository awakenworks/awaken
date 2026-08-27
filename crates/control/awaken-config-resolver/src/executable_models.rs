use awaken_credential_vault::CredentialSource;
use awaken_model_catalog::ProviderCatalog;
use awaken_runtime_contract::resolved::ProviderAccessKind;

use crate::credential_is_executable_supply;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ExecutableModelReadiness {
    Ready,
    OfferingUnavailable,
    CredentialUnavailable,
    RuntimeUnavailable,
    DialectUnavailable,
}

#[must_use]
const fn executable_model_readiness(
    offering_active: bool,
    executor_available: bool,
    dialect_supported: bool,
    brokered: bool,
    brokered_access_enabled: bool,
    matching_direct_credential: bool,
) -> ExecutableModelReadiness {
    if !offering_active {
        ExecutableModelReadiness::OfferingUnavailable
    } else if !executor_available {
        ExecutableModelReadiness::RuntimeUnavailable
    } else if !dialect_supported {
        ExecutableModelReadiness::DialectUnavailable
    } else if brokered {
        if brokered_access_enabled {
            ExecutableModelReadiness::Ready
        } else {
            ExecutableModelReadiness::RuntimeUnavailable
        }
    } else if matching_direct_credential {
        ExecutableModelReadiness::Ready
    } else {
        ExecutableModelReadiness::CredentialUnavailable
    }
}

#[cfg(kani)]
#[kani::proof]
fn brokered_and_direct_model_readiness_require_their_exact_access_evidence() {
    let offering_active = kani::any();
    let executor_available = kani::any();
    let dialect_supported = kani::any();
    let brokered = kani::any();
    let brokered_access_enabled = kani::any();
    let matching_direct_credential = kani::any();
    let readiness = executable_model_readiness(
        offering_active,
        executor_available,
        dialect_supported,
        brokered,
        brokered_access_enabled,
        matching_direct_credential,
    );

    if !offering_active {
        assert_eq!(readiness, ExecutableModelReadiness::OfferingUnavailable);
    } else if !executor_available {
        assert_eq!(readiness, ExecutableModelReadiness::RuntimeUnavailable);
    } else if !dialect_supported {
        assert_eq!(readiness, ExecutableModelReadiness::DialectUnavailable);
    } else if brokered {
        assert_eq!(
            readiness == ExecutableModelReadiness::Ready,
            brokered_access_enabled
        );
    } else {
        assert_eq!(
            readiness == ExecutableModelReadiness::Ready,
            matching_direct_credential
        );
    }
}

/// One installed executor's provider-model consumption contract.
///
/// This is a secret-free planning input projected from the executor's own
/// authoritative catalog. Empty `model_api_dialects` means every catalog
/// dialect supported by the native provider adapter; external executors must
/// enumerate their stable dialect tokens explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorModelCapability {
    pub backend_ref: String,
    pub model_api_dialects: Vec<String>,
    pub available: bool,
}

impl ExecutorModelCapability {
    #[must_use]
    pub fn native() -> Self {
        Self {
            backend_ref: "genai".into(),
            model_api_dialects: Vec::new(),
            available: true,
        }
    }

    #[must_use]
    pub fn supports(&self, dialect: &str) -> bool {
        (self.backend_ref == "genai" && self.model_api_dialects.is_empty())
            || self
                .model_api_dialects
                .iter()
                .any(|candidate| candidate == dialect)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExecutableModelOption {
    pub backend_ref: String,
    pub provider_id: String,
    pub model_id: String,
    pub endpoint_id: String,
    pub dialect: String,
    pub access_kind: ProviderAccessKind,
    pub readiness: ExecutableModelReadiness,
}

/// Pure Catalog × access posture × Executor evaluator shared by config reads,
/// model-directory projection, and publication admission. Direct/BYOK access
/// is proven by Workspace Credentials; Brokered access is proven only by the
/// composition's existing brokered runtime capability. Publication performs
/// the stateful credential/route choice and revision fence after this
/// side-effect-free planner has accepted the same route.
#[must_use]
pub fn project_executable_models(
    catalog: &ProviderCatalog,
    credentials: &[CredentialSource],
    executors: &[ExecutorModelCapability],
    brokered_access_enabled: bool,
) -> Vec<ExecutableModelOption> {
    let mut options = catalog
        .offerings
        .iter()
        .flat_map(|offering| {
            executors.iter().map(move |executor| {
                let brokered = offering.source == awaken_model_catalog::OfferingSource::Brokered;
                let matching_direct_credential = if brokered {
                    false
                } else {
                    credentials.iter().any(|credential| {
                        credential_is_executable_supply(
                            offering.provider_id.as_str(),
                            Some(offering.protocol_endpoint_id.as_str()),
                            &executor.backend_ref,
                            credential,
                        )
                    })
                };
                let readiness = executable_model_readiness(
                    offering.status == awaken_model_catalog::OfferingStatus::Active,
                    executor.available,
                    executor.supports(offering.dialect.as_str()),
                    brokered,
                    brokered_access_enabled,
                    matching_direct_credential,
                );
                ExecutableModelOption {
                    backend_ref: executor.backend_ref.clone(),
                    provider_id: offering.provider_id.0.clone(),
                    model_id: offering.model_id.clone(),
                    endpoint_id: offering.protocol_endpoint_id.0.clone(),
                    dialect: offering.dialect.as_str().to_string(),
                    access_kind: if brokered {
                        ProviderAccessKind::Brokered
                    } else {
                        ProviderAccessKind::Direct
                    },
                    readiness,
                }
            })
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| {
        (
            &left.model_id,
            &left.provider_id,
            &left.endpoint_id,
            &left.backend_ref,
        )
            .cmp(&(
                &right.model_id,
                &right.provider_id,
                &right.endpoint_id,
                &right.backend_ref,
            ))
    });
    options
}

/// Validate one selected Offering against the same executor capability facts
/// used by discovery. This is the publication half of the one planner.
pub fn validate_executor_offering(
    executors: &[ExecutorModelCapability],
    backend_ref: &str,
    dialect: &str,
) -> Result<(), ExecutableModelReadiness> {
    let Some(executor) = executors
        .iter()
        .find(|executor| executor.backend_ref == backend_ref)
    else {
        return Err(ExecutableModelReadiness::RuntimeUnavailable);
    };
    if !executor.available {
        return Err(ExecutableModelReadiness::RuntimeUnavailable);
    }
    if !executor.supports(dialect) {
        return Err(ExecutableModelReadiness::DialectUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_model_catalog::{
        ApiDialect, Offering, OfferingSource, OfferingStatus, ProtocolEndpointId, ProviderId,
    };

    fn offering(provider: &str, endpoint: &str) -> Offering {
        Offering {
            model_id: "shared/model".into(),
            provider_id: ProviderId::new(provider),
            protocol_endpoint_id: ProtocolEndpointId::new(endpoint),
            dialect: ApiDialect::OpenAiChat,
            upstream_model: None,
            source: OfferingSource::Manual,
            status: OfferingStatus::Active,
            last_seen_at_unix_ms: None,
        }
    }

    #[test]
    fn executable_model_wire_deserializes_into_the_canonical_read_model() {
        // Causes: C1 the response has every required ExecutableModelOption
        // field; C2 readiness and access kind are canonical snake_case tokens;
        // C3 either token is unknown or absent. Effects: E1 decode directly
        // into the canonical DTO and nested enums; E2 reject an unknown token;
        // E3 reject an incomplete option. Decision rules: C1+C2 -> E1;
        // C1+C3(unknown) -> E2; !C1+C3(absent) -> E3. No client-side
        // compatibility DTO participates.
        let cases = [
            ("ready", ExecutableModelReadiness::Ready),
            (
                "offering_unavailable",
                ExecutableModelReadiness::OfferingUnavailable,
            ),
            (
                "credential_unavailable",
                ExecutableModelReadiness::CredentialUnavailable,
            ),
            (
                "runtime_unavailable",
                ExecutableModelReadiness::RuntimeUnavailable,
            ),
            (
                "dialect_unavailable",
                ExecutableModelReadiness::DialectUnavailable,
            ),
        ];
        for (readiness, expected) in cases {
            let options: Vec<ExecutableModelOption> = serde_json::from_value(serde_json::json!([{
                "backend_ref": "genai",
                "provider_id": "provider",
                "model_id": "model",
                "endpoint_id": "provider.open_ai_chat",
                "dialect": "open_ai_chat",
                "access_kind": "direct",
                "readiness": readiness,
            }]))
            .expect("canonical executable-model response deserializes");
            assert_eq!(options[0].readiness, expected, "E1: {readiness}");
        }

        let unknown = serde_json::json!([{
            "backend_ref": "genai",
            "provider_id": "provider",
            "model_id": "model",
            "endpoint_id": "provider.open_ai_chat",
            "dialect": "open_ai_chat",
            "access_kind": "direct",
            "readiness": "future_state",
        }]);
        assert!(
            serde_json::from_value::<Vec<ExecutableModelOption>>(unknown).is_err(),
            "E2"
        );
        let incomplete = serde_json::json!([{
            "backend_ref": "genai",
            "provider_id": "provider",
            "model_id": "model",
            "endpoint_id": "provider.open_ai_chat",
            "dialect": "open_ai_chat",
            "access_kind": "direct",
        }]);
        assert!(
            serde_json::from_value::<Vec<ExecutableModelOption>>(incomplete).is_err(),
            "E3"
        );
    }

    #[test]
    fn executable_planner_joins_offering_credential_runtime_and_dialect_once() {
        // Causes: C1 active/inactive Offering; C2 direct/BYOK or Brokered
        // source; C3 provider+endpoint compatible local credential; C4 runtime
        // available/unavailable; C5 dialect supported/unsupported; C6 brokered
        // access capability enabled/disabled.
        // Effects: E1 Ready with Direct access; E2 OfferingUnavailable; E3
        // CredentialUnavailable; E4 RuntimeUnavailable; E5
        // DialectUnavailable; E6 Ready with Brokered access. Decision rules:
        // direct/BYOK+C3+C4+C5 -> E1;
        // direct/BYOK+!C3 -> E3; Brokered+C4+C5+C6 -> E1 without a local
        // Credential; Brokered+!C6 -> E4; !C1 -> E2. Each rule retains the same
        // catalog/access join used by publication admission.
        let catalog = ProviderCatalog {
            offerings: vec![offering("provider", "provider.open_ai_chat")],
            ..ProviderCatalog::default()
        };
        let credential = CredentialSource {
            id: awaken_credential_contract::CredentialSourceId("credential".into()),
            replacement_of: None,
            workspace_id: "workspace".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            descriptor: None,
            provider_id: Some("provider".into()),
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: Some(awaken_credential_vault::SecretRef("secret".into())),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: awaken_credential_vault::CredentialStatus::Active,
            version: 1,
        };
        let capability = |available, dialects: &[&str]| ExecutorModelCapability {
            backend_ref: "acp:test".into(),
            model_api_dialects: dialects.iter().map(|value| (*value).into()).collect(),
            available,
        };
        let option = |capability: ExecutorModelCapability, credentials: &[CredentialSource]| {
            project_executable_models(&catalog, credentials, &[capability], false).remove(0)
        };
        assert_eq!(
            option(
                capability(true, &["open_ai_chat"]),
                std::slice::from_ref(&credential),
            )
            .readiness,
            ExecutableModelReadiness::Ready,
            "E1"
        );
        assert_eq!(
            option(
                capability(true, &["open_ai_chat"]),
                std::slice::from_ref(&credential),
            )
            .access_kind,
            ProviderAccessKind::Direct,
            "E1"
        );
        assert_eq!(
            option(capability(true, &["open_ai_chat"]), &[]).readiness,
            ExecutableModelReadiness::CredentialUnavailable,
            "E3"
        );
        let mut other_endpoint = credential.clone();
        other_endpoint.protocol_endpoint_id = Some("provider.open_ai_chat.backup".into());
        assert_eq!(
            option(capability(true, &["open_ai_chat"]), &[other_endpoint]).readiness,
            ExecutableModelReadiness::CredentialUnavailable,
            "E3: a credential for another endpoint cannot make this route ready"
        );
        assert_eq!(
            option(
                capability(false, &["open_ai_chat"]),
                std::slice::from_ref(&credential),
            )
            .readiness,
            ExecutableModelReadiness::RuntimeUnavailable,
            "E4"
        );
        assert_eq!(
            option(
                capability(true, &["anthropic_messages"]),
                std::slice::from_ref(&credential),
            )
            .readiness,
            ExecutableModelReadiness::DialectUnavailable,
            "E5"
        );
        assert_eq!(
            option(capability(true, &[]), &[]).readiness,
            ExecutableModelReadiness::DialectUnavailable,
            "an external executor with no dialect claims nothing"
        );

        let mut brokered = catalog.clone();
        brokered.offerings[0].source = OfferingSource::Brokered;
        assert_eq!(
            project_executable_models(&brokered, &[], &[ExecutorModelCapability::native()], true)
                [0]
            .readiness,
            ExecutableModelReadiness::Ready,
            "E1: brokered access uses the existing runtime capability, not a local key"
        );
        assert_eq!(
            project_executable_models(&brokered, &[], &[ExecutorModelCapability::native()], true)
                [0]
            .access_kind,
            ProviderAccessKind::Brokered,
            "E6: access kind is explicit and independent from credential presence"
        );
        assert_eq!(
            project_executable_models(
                &brokered,
                std::slice::from_ref(&credential),
                &[ExecutorModelCapability::native()],
                false,
            )[0]
            .readiness,
            ExecutableModelReadiness::RuntimeUnavailable,
            "E4: a local credential cannot substitute for a disabled brokered path"
        );
    }

    #[test]
    fn acp_executor_dialect_matrix_fails_closed() {
        // Cause/effect graph: selected ACP executor (C1) + executor availability
        // (C2) + selected Offering API dialect (C3) -> publication admission
        // (E1), RuntimeUnavailable (E2), or DialectUnavailable (E3).
        //
        // Decision table:
        // | rule | executor   | available | dialect              | effect |
        // | R1   | claude     | yes       | anthropic_messages   | E1     |
        // | R2   | claude     | yes       | open_ai_responses    | E3     |
        // | R3   | codex      | yes       | open_ai_responses    | E1     |
        // | R4   | codex      | yes       | anthropic_messages   | E3     |
        // | R5   | unknown    | -         | any                  | E2     |
        // | R6   | claude     | no        | anthropic_messages   | E2     |
        let executors = [
            ExecutorModelCapability {
                backend_ref: "acp:claude".into(),
                model_api_dialects: vec!["anthropic_messages".into()],
                available: true,
            },
            ExecutorModelCapability {
                backend_ref: "acp:codex".into(),
                model_api_dialects: vec!["open_ai_responses".into()],
                available: true,
            },
            ExecutorModelCapability {
                backend_ref: "acp:claude-offline".into(),
                model_api_dialects: vec!["anthropic_messages".into()],
                available: false,
            },
        ];
        assert_eq!(
            validate_executor_offering(&executors, "acp:claude", "anthropic_messages"),
            Ok(()),
            "R1"
        );
        assert_eq!(
            validate_executor_offering(&executors, "acp:claude", "open_ai_responses"),
            Err(ExecutableModelReadiness::DialectUnavailable),
            "R2: Claude Code must not receive an OpenAI Responses route"
        );
        assert_eq!(
            validate_executor_offering(&executors, "acp:codex", "open_ai_responses"),
            Ok(()),
            "R3"
        );
        assert_eq!(
            validate_executor_offering(&executors, "acp:codex", "anthropic_messages"),
            Err(ExecutableModelReadiness::DialectUnavailable),
            "R4"
        );
        assert_eq!(
            validate_executor_offering(&executors, "acp:unknown", "anthropic_messages"),
            Err(ExecutableModelReadiness::RuntimeUnavailable),
            "R5"
        );
        assert_eq!(
            validate_executor_offering(&executors, "acp:claude-offline", "anthropic_messages"),
            Err(ExecutableModelReadiness::RuntimeUnavailable),
            "R6"
        );
    }
}
