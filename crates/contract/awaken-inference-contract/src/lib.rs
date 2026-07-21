//! Secret-free inference and credential-injection vocabulary shared across the
//! configuration and execution boundary.
//!
//! These values are publication output, not IAM decisions and not secret
//! material. Configuration selects one exact endpoint/credential reference;
//! execution may only materialize that pinned choice.

use serde::{Deserialize, Serialize};

/// A stable reference to one credential source revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialRef {
    pub id: String,
    pub revision: u64,
}

/// How credential material crosses into the execution boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialInjectionKind {
    Reference,
    SealedEnvelope,
    Direct,
}

/// How the resolved endpoint consumes injected material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialUsage {
    ProviderAdapter,
    HttpHeader {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
    },
    QueryParameter {
        name: String,
    },
    ClientCertificate,
    EnvironmentVariable {
        name: String,
    },
    File {
        path: String,
    },
}

/// Secret-free credential instructions pinned into an executable snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialAccess {
    pub credential: CredentialRef,
    pub injection: CredentialInjectionKind,
    pub usage: CredentialUsage,
}

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
                injection: CredentialInjectionKind::Reference,
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
        self.candidates
            .iter()
            .find(|candidate| candidate.model_ref == model_ref)
            .map(|candidate| candidate.access.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_access_is_exact_and_secret_free() {
        let access = InferenceAccess::host_executor("model-a");
        assert!(access.is_host_executor_for("model-a"));
        assert!(!access.is_host_executor_for("model-b"));
        let wire = serde_json::to_string(&access).unwrap();
        assert!(!wire.contains("api_key"));
        assert!(!wire.contains("secret"));
    }
}
