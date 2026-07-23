//! Secret-free inference and credential-injection vocabulary shared across the
//! configuration and execution boundary.
//!
//! These values are embedded in a complete published model candidate; they are
//! not IAM decisions or secret material. Execution may only materialize the
//! exact endpoint/credential reference frozen by publication.

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
