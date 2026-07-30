use awaken_credential_vault::CredentialSource;
use awaken_model_catalog::ProviderCatalog;

use crate::credential_can_supply;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ExecutableModelReadiness {
    Ready,
    OfferingUnavailable,
    CredentialUnavailable,
    RuntimeUnavailable,
    DialectUnavailable,
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExecutableModelOption {
    pub backend_ref: String,
    pub provider_id: String,
    pub model_id: String,
    pub endpoint_id: String,
    pub dialect: String,
    pub readiness: ExecutableModelReadiness,
}

/// Pure Catalog × Credential × Executor evaluator shared by config reads,
/// model-directory projection, and publication admission. Publication performs
/// only the stateful credential choice and revision fence after this
/// side-effect-free planner has accepted the same route.
#[must_use]
pub fn project_executable_models(
    catalog: &ProviderCatalog,
    credentials: &[CredentialSource],
    executors: &[ExecutorModelCapability],
) -> Vec<ExecutableModelOption> {
    let mut options = catalog
        .offerings
        .iter()
        .flat_map(|offering| {
            executors.iter().map(move |executor| {
                let readiness = if offering.status != awaken_model_catalog::OfferingStatus::Active {
                    ExecutableModelReadiness::OfferingUnavailable
                } else if !executor.available {
                    ExecutableModelReadiness::RuntimeUnavailable
                } else if !executor.supports(offering.dialect.as_str()) {
                    ExecutableModelReadiness::DialectUnavailable
                } else if credentials.iter().any(|credential| {
                    credential.status == awaken_credential_vault::CredentialStatus::Active
                        && credential.is_executable_origin()
                        && credential_can_supply(
                            offering.provider_id.as_str(),
                            Some(offering.protocol_endpoint_id.as_str()),
                            &executor.backend_ref,
                            credential,
                        )
                }) {
                    ExecutableModelReadiness::Ready
                } else {
                    ExecutableModelReadiness::CredentialUnavailable
                };
                ExecutableModelOption {
                    backend_ref: executor.backend_ref.clone(),
                    provider_id: offering.provider_id.0.clone(),
                    model_id: offering.model_id.clone(),
                    endpoint_id: offering.protocol_endpoint_id.0.clone(),
                    dialect: offering.dialect.as_str().to_string(),
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
    fn executable_planner_joins_offering_credential_runtime_and_dialect_once() {
        // Causes: C1 active/inactive Offering; C2 provider+endpoint compatible
        // credential; C3 runtime available/unavailable; C4 dialect
        // supported/unsupported.
        // Effects: E1 Ready; E2 OfferingUnavailable; E3
        // CredentialUnavailable; E4 RuntimeUnavailable; E5
        // DialectUnavailable. Each rule changes one cause while retaining the
        // same catalog/credential join used by publication admission.
        let catalog = ProviderCatalog {
            offerings: vec![offering("provider", "provider.open_ai_chat")],
            ..ProviderCatalog::default()
        };
        let credential = CredentialSource {
            id: awaken_credential_vault::CredentialSourceId("credential".into()),
            workspace_id: "workspace".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
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
        let readiness = |capability: ExecutorModelCapability, credentials: &[CredentialSource]| {
            project_executable_models(&catalog, credentials, &[capability])[0].readiness
        };
        assert_eq!(
            readiness(capability(true, &["open_ai_chat"]), &[credential.clone()]),
            ExecutableModelReadiness::Ready,
            "E1"
        );
        assert_eq!(
            readiness(capability(true, &["open_ai_chat"]), &[]),
            ExecutableModelReadiness::CredentialUnavailable,
            "E3"
        );
        let mut other_endpoint = credential.clone();
        other_endpoint.protocol_endpoint_id = Some("provider.open_ai_chat.backup".into());
        assert_eq!(
            readiness(capability(true, &["open_ai_chat"]), &[other_endpoint]),
            ExecutableModelReadiness::CredentialUnavailable,
            "E3: a credential for another endpoint cannot make this route ready"
        );
        assert_eq!(
            readiness(capability(false, &["open_ai_chat"]), &[credential.clone()]),
            ExecutableModelReadiness::RuntimeUnavailable,
            "E4"
        );
        assert_eq!(
            readiness(capability(true, &["anthropic_messages"]), &[credential]),
            ExecutableModelReadiness::DialectUnavailable,
            "E5"
        );
        assert_eq!(
            readiness(capability(true, &[]), &[]),
            ExecutableModelReadiness::DialectUnavailable,
            "an external executor with no dialect claims nothing"
        );
    }
}
