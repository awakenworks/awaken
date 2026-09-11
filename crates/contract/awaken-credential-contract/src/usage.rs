use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};

use crate::HttpEffectPlacement;

/// Core delivery carrier that places material into a process environment.
///
/// The exact variable name is adapter-owned and travels in `public_config`;
/// the credential domain never learns it.
pub const PROCESS_ENV_CONSUMER: &str = "awaken.delivery.process-env";

/// Core delivery carrier that places material into a process-private file.
///
/// The exact path is adapter-owned and travels in `public_config`; the
/// credential domain never learns it.
pub const PRIVATE_FILE_CONSUMER: &str = "awaken.delivery.private-file";

/// Declared material type of the core process-environment carrier.
pub const PROCESS_ENV_MATERIAL_TYPE: &str = "awaken.credential.process-env/v1";

/// Declared material type of the core private-file carrier.
pub const PRIVATE_FILE_MATERIAL_TYPE: &str = "awaken.credential.private-file/v1";

/// Core delivery carriers transport bytes; they do not interpret them.
///
/// An external extension consumer pins one exact material type because it
/// parses that material. A core carrier places whatever material the selected
/// credential resolves to, so it is deliberately shape-agnostic. Keeping the
/// distinction in one predicate stops every call site from re-deciding it.
#[must_use]
pub fn is_core_delivery_consumer(consumer_id: &str) -> bool {
    matches!(consumer_id, PROCESS_ENV_CONSUMER | PRIVATE_FILE_CONSUMER)
}

/// How the resolved endpoint consumes injected material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
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
    ///
    /// This is also the one mechanism for core ENV and private-file delivery:
    /// see [`PROCESS_ENV_CONSUMER`] and [`PRIVATE_FILE_CONSUMER`]. Physical
    /// locations stay inside `public_config` so no deployment detail — an ENV
    /// name, a host path — enters the credential domain model.
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
}

impl CredentialUsage {
    /// Deliver through the core process-environment carrier.
    #[must_use]
    pub fn process_env(name: impl Into<String>) -> Self {
        Self::Extension {
            consumer_id: PROCESS_ENV_CONSUMER.to_string(),
            material_type: PROCESS_ENV_MATERIAL_TYPE.to_string(),
            public_config: serde_json::json!({ "name": name.into() }),
        }
    }

    /// Deliver through the core private-file carrier.
    #[must_use]
    pub fn private_file(path: impl Into<String>) -> Self {
        Self::Extension {
            consumer_id: PRIVATE_FILE_CONSUMER.to_string(),
            material_type: PRIVATE_FILE_MATERIAL_TYPE.to_string(),
            public_config: serde_json::json!({ "path": path.into() }),
        }
    }

    /// Adapter-owned environment variable name, when this is a core
    /// process-environment delivery.
    #[must_use]
    pub fn process_env_name(&self) -> Option<&str> {
        self.core_delivery_location(PROCESS_ENV_CONSUMER, "name")
    }

    /// Adapter-owned file path, when this is a core private-file delivery.
    #[must_use]
    pub fn private_file_path(&self) -> Option<&str> {
        self.core_delivery_location(PRIVATE_FILE_CONSUMER, "path")
    }

    /// Scalar material a process consumes directly: either the provider
    /// adapter reads it itself, or the core process-environment carrier places
    /// it. Structured, OAuth and effect-shaped usages are deliberately absent.
    ///
    /// Backend binding, published-candidate admission and recipient-envelope
    /// delivery all ask this one question; keeping it here stops the three
    /// from drifting apart.
    #[must_use]
    pub fn is_scalar_process_delivery(&self) -> bool {
        matches!(self, Self::ProviderAdapter) || self.process_env_name().is_some()
    }

    /// True for either core delivery carrier, whatever its location.
    #[must_use]
    pub fn is_core_delivery(&self) -> bool {
        matches!(
            self,
            Self::Extension { consumer_id, .. } if is_core_delivery_consumer(consumer_id)
        )
    }

    fn core_delivery_location(&self, expected_consumer: &str, field: &str) -> Option<&str> {
        match self {
            Self::Extension {
                consumer_id,
                public_config,
                ..
            } if consumer_id == expected_consumer => public_config.get(field)?.as_str(),
            _ => None,
        }
    }
}

/// Decoding shape that still accepts the retired `environment_variable` and
/// `file` encodings. Retained rows decode into the one `Extension` mechanism;
/// nothing re-serializes the retired tags.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CredentialUsageWire {
    ProviderAdapter,
    HttpHeader {
        name: String,
        #[serde(default)]
        scheme: Option<String>,
    },
    HttpBasicAuth,
    Extension {
        consumer_id: String,
        material_type: String,
        #[serde(default)]
        public_config: serde_json::Value,
    },
    QueryParameter {
        name: String,
    },
    HttpEffect {
        fields: BTreeMap<String, BTreeSet<HttpEffectPlacement>>,
    },
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

impl<'de> Deserialize<'de> for CredentialUsage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match CredentialUsageWire::deserialize(deserializer)? {
            CredentialUsageWire::ProviderAdapter => Self::ProviderAdapter,
            CredentialUsageWire::HttpHeader { name, scheme } => Self::HttpHeader { name, scheme },
            CredentialUsageWire::HttpBasicAuth => Self::HttpBasicAuth,
            CredentialUsageWire::Extension {
                consumer_id,
                material_type,
                public_config,
            } => Self::Extension {
                consumer_id,
                material_type,
                public_config,
            },
            CredentialUsageWire::QueryParameter { name } => Self::QueryParameter { name },
            CredentialUsageWire::HttpEffect { fields } => Self::HttpEffect { fields },
            CredentialUsageWire::SignatureVerification { fields } => {
                Self::SignatureVerification { fields }
            }
            CredentialUsageWire::ClientCertificate => Self::ClientCertificate,
            CredentialUsageWire::EnvironmentVariable { name } => Self::process_env(name),
            CredentialUsageWire::File { path } => Self::private_file(path),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialPurpose, CredentialTarget};

    /// Retained rows encoded with the retired `environment_variable` and
    /// `file` tags decode into the one core carrier mechanism. Nothing
    /// re-serializes the retired tags, so this compatibility is one-way.
    #[test]
    fn retired_location_encodings_decode_into_core_carriers() {
        let env: CredentialUsage = serde_json::from_value(
            serde_json::json!({"type": "environment_variable", "name": "K"}),
        )
        .expect("legacy env row decodes");
        assert_eq!(env, CredentialUsage::process_env("K"));
        assert_eq!(env.process_env_name(), Some("K"));

        let file: CredentialUsage =
            serde_json::from_value(serde_json::json!({"type": "file", "path": "/run/c"}))
                .expect("legacy file row decodes");
        assert_eq!(file, CredentialUsage::private_file("/run/c"));
        assert_eq!(file.private_file_path(), Some("/run/c"));

        // The retired tags are gone from the written wire; a physical location
        // survives only inside adapter-owned `public_config`.
        let written = serde_json::to_value(&env).expect("serialize");
        assert_eq!(written["type"], "extension");
        assert_eq!(written["consumer_id"], PROCESS_ENV_CONSUMER);
        assert_eq!(written["public_config"]["name"], "K");
    }

    /// The point of the fold: Repository, MCP, remote-agent and provider
    /// credentials reuse one delivery mechanism. Purpose constrains which
    /// upstream may consume material, never how the bytes travel.
    #[test]
    fn core_carriers_are_purpose_neutral() {
        for purpose in [
            CredentialPurpose::ProviderAdapter,
            CredentialPurpose::RepositoryTransport,
            CredentialPurpose::McpAuthorization,
            CredentialPurpose::RemoteAgentAuthorization,
            CredentialPurpose::WebProviderAuthorization,
        ] {
            for usage in [
                CredentialUsage::process_env("K"),
                CredentialUsage::private_file("/run/c"),
            ] {
                let target = CredentialTarget::new(purpose, "upstream");
                assert!(
                    target.validate_usage(&usage).is_ok(),
                    "{purpose:?} must admit the core carrier {usage:?}"
                );
            }
        }
    }

    /// An external extension is still held to its declared material type; only
    /// the two runtime-owned carriers are shape-agnostic.
    #[test]
    fn only_the_two_runtime_owned_carriers_are_core() {
        assert!(is_core_delivery_consumer(PROCESS_ENV_CONSUMER));
        assert!(is_core_delivery_consumer(PRIVATE_FILE_CONSUMER));
        assert!(!is_core_delivery_consumer("vendor.example.consumer"));
        assert!(
            !CredentialUsage::Extension {
                consumer_id: "vendor.example.consumer".into(),
                material_type: "vendor/v1".into(),
                public_config: serde_json::Value::Null,
            }
            .is_core_delivery()
        );
    }

    /// Backend binding, published-candidate admission and recipient-envelope
    /// delivery share this one predicate; it must stay narrow.
    #[test]
    fn scalar_process_delivery_covers_adapter_and_env_only() {
        assert!(CredentialUsage::ProviderAdapter.is_scalar_process_delivery());
        assert!(CredentialUsage::process_env("K").is_scalar_process_delivery());
        assert!(!CredentialUsage::private_file("/run/c").is_scalar_process_delivery());
        assert!(!CredentialUsage::HttpBasicAuth.is_scalar_process_delivery());
    }
}
