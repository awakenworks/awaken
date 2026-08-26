use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::HttpEffectPlacement;

/// How the resolved endpoint consumes injected material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialUsage {
    ProviderAdapter,
    HttpHeader {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
    },
    /// RFC 7617 HTTP Basic authentication. This consumes the built-in typed
    /// material id [`crate::HTTP_BASIC_MATERIAL_TYPE`].
    HttpBasicAuth,
    /// An externally owned consumer. Core transports never interpret
    /// `public_config`; the exact target adapter named by `consumer_id`
    /// validates it and consumes only material whose type matches
    /// `material_type`.
    Extension {
        consumer_id: String,
        material_type: String,
        #[serde(default)]
        public_config: serde_json::Value,
    },
    QueryParameter {
        name: String,
    },
    /// A platform-held HTTP effect whose complete material-field destinations
    /// are frozen before materialization. The Gateway must compare the actual
    /// effect references with this exact field/placement map before I/O.
    HttpEffect {
        fields: BTreeMap<String, BTreeSet<HttpEffectPlacement>>,
    },
    /// Host-side verification of signed evidence. The exact target identifies
    /// the consumer; this map freezes each material field and every admitted
    /// provider-neutral verification algorithm without exposing the key.
    SignatureVerification {
        fields: BTreeMap<String, BTreeSet<SignatureVerificationAlgorithm>>,
    },
    ClientCertificate,
    EnvironmentVariable {
        name: String,
    },
    File {
        path: String,
    },
}

/// Closed provider-neutral algorithms implemented by the host verification
/// boundary. Provider-specific message construction remains in the caller.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SignatureVerificationAlgorithm {
    HmacSha256,
    Sha256Parts,
    ConstantTime,
}
