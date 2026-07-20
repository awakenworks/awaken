//! Credential access across the config-to-execution boundary.
//!
//! A published snapshot carries [`CredentialAccess`], which is durable and
//! secret-free. The configuration plane selects one injection mechanism; execution
//! only materializes that pinned choice immediately before an outbound call.

use serde::{Deserialize, Serialize};

/// A stable reference to one credential source revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialRef {
    pub id: String,
    pub revision: u64,
}

/// How credential material crosses into the execution boundary.
///
/// These are capability descriptions, not deployment modes: no variant names a
/// local, cloud, or gateway topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialInjectionKind {
    /// Exchange an opaque reference for request-scoped material.
    Reference,
    /// Deliver material encrypted to the selected workload identity.
    SealedEnvelope,
    /// Hand material across an already-trusted in-process boundary.
    Direct,
}

/// How the resolved endpoint consumes injected material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialUsage {
    /// The selected inference adapter owns the protocol-specific placement
    /// (for example `Authorization: Bearer` or `x-api-key`).
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
    /// The single mechanism selected during configuration publication.
    pub injection: CredentialInjectionKind,
    pub usage: CredentialUsage,
}
