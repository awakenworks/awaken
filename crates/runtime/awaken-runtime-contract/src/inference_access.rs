//! Secret-free inference access pinned at the configuration/execution boundary.

use serde::{Deserialize, Serialize};

use crate::{
    CredentialAccess, CredentialInjectionKind, CredentialInjectionPolicy, CredentialRef,
    CredentialUsage,
};

/// Provider-facing endpoint facts frozen by configuration publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceEndpoint {
    pub adapter_kind: String,
    pub base_url: String,
    pub upstream_model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceAccessCandidate {
    pub model_ref: String,
    pub access: InferenceAccess,
}

/// Immutable, non-secret instructions for obtaining inference credentials.
/// Endpoint/provider identities are pins, not topology modes; runtime and durable
/// dispatch transport this value without selecting another route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceAccess {
    pub scheme: String,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_access: Option<CredentialAccess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<InferenceEndpoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<InferenceAccessCandidate>,
}

impl InferenceAccess {
    #[must_use]
    pub fn new(scheme: impl Into<String>, reference: impl Into<String>) -> Self {
        Self {
            scheme: scheme.into(),
            reference: reference.into(),
            provider_ref: None,
            route_ref: None,
            scope_id: None,
            credential_access: None,
            endpoint: None,
            candidates: Vec::new(),
        }
    }

    #[must_use]
    pub fn exact_credential(
        credential_ref: impl Into<String>,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
    ) -> Self {
        let credential_ref = credential_ref.into();
        Self {
            scheme: "credential-source/v1".to_string(),
            reference: credential_ref.clone(),
            provider_ref: Some(provider_ref.into()),
            route_ref: Some(route_ref.into()),
            scope_id: None,
            credential_access: Some(CredentialAccess {
                credential: CredentialRef {
                    id: credential_ref,
                    revision: 0,
                },
                injection: CredentialInjectionPolicy::new(CredentialInjectionKind::Reference, [])
                    .expect("a single injection mechanism is valid"),
                usage: CredentialUsage::ProviderAdapter,
            }),
            endpoint: None,
            candidates: Vec::new(),
        }
    }

    /// Pin every non-secret input required to construct one provider client. The
    /// execution plane may validate these pins and inject the referenced material,
    /// but must not select another catalog route or workspace credential.
    #[must_use]
    pub fn resolved_credential(
        credential_ref: impl Into<String>,
        credential_version: u64,
        scope_id: impl Into<String>,
        provider_ref: impl Into<String>,
        route_ref: impl Into<String>,
        endpoint: InferenceEndpoint,
    ) -> Self {
        let credential_ref = credential_ref.into();
        Self {
            scheme: "credential-source/v1".to_string(),
            reference: credential_ref.clone(),
            provider_ref: Some(provider_ref.into()),
            route_ref: Some(route_ref.into()),
            scope_id: Some(scope_id.into()),
            credential_access: Some(CredentialAccess {
                credential: CredentialRef {
                    id: credential_ref,
                    revision: credential_version,
                },
                injection: CredentialInjectionPolicy::new(CredentialInjectionKind::Reference, [])
                    .expect("a single injection mechanism is valid"),
                usage: CredentialUsage::ProviderAdapter,
            }),
            endpoint: Some(endpoint),
            candidates: Vec::new(),
        }
    }

    #[must_use]
    pub fn host_executor(model_ref: impl Into<String>) -> Self {
        Self {
            scheme: "host-executor/v1".to_string(),
            reference: model_ref.into(),
            provider_ref: None,
            route_ref: None,
            scope_id: None,
            credential_access: None,
            endpoint: None,
            candidates: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_host_executor_for(&self, model_ref: &str) -> bool {
        self.scheme == "host-executor/v1"
            && self.reference == model_ref
            && self.provider_ref.is_none()
            && self.route_ref.is_none()
            && self.scope_id.is_none()
            && self.credential_access.is_none()
            && self.endpoint.is_none()
            && self.candidates.is_empty()
    }

    #[must_use]
    pub fn candidate_set(
        candidates: impl IntoIterator<Item = (String, InferenceAccess)>,
    ) -> Option<Self> {
        let candidates = candidates
            .into_iter()
            .map(|(model_ref, mut access)| {
                access.candidates.clear();
                InferenceAccessCandidate { model_ref, access }
            })
            .collect::<Vec<_>>();
        let first = candidates.first()?;
        Some(Self {
            scheme: first.access.scheme.clone(),
            reference: first.access.reference.clone(),
            provider_ref: first.access.provider_ref.clone(),
            route_ref: first.access.route_ref.clone(),
            scope_id: first.access.scope_id.clone(),
            credential_access: first.access.credential_access.clone(),
            endpoint: first.access.endpoint.clone(),
            candidates,
        })
    }

    #[must_use]
    pub fn for_model(&self, model_ref: &str) -> Option<Self> {
        if self.candidates.is_empty() {
            return Some(self.clone());
        }
        let candidate = self
            .candidates
            .iter()
            .find(|candidate| candidate.model_ref == model_ref)?;
        Some(candidate.access.clone())
    }
}
