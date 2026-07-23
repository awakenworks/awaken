//! Secret-free credential delivery facts embedded in a published execution
//! snapshot. These values are runtime input, not authoring state, IAM decisions,
//! or secret material.

use serde::{Deserialize, Serialize};

/// A stable reference to one credential source revision.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CredentialRef {
    pub id: String,
    pub revision: u64,
}

/// How credential material crosses into the execution boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialInjectionKind {
    Reference,
    /// The secret stays on an eligible worker. The control plane publishes only
    /// the exact source revision; placement must match a live worker observation
    /// before that worker may claim the run.
    WorkerReference,
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
