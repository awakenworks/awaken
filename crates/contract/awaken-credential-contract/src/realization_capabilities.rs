//! Installed credential realization evidence and its Worker manifest codec.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{CredentialMaterialSource, PlaintextHolder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRealizationKind {
    ProcessSecretEnvironment,
    PrivateSecretFile,
    WorkerProviderAdapter,
    WorkerRelay,
    /// A trusted downstream platform adapter consumes the exact published
    /// credential reference without exposing provider plaintext to the Worker
    /// or workload.
    PlatformProviderAdapter,
    /// A trusted platform egress adapter consumes exact material only while
    /// executing a target-bound effect. The caller receives the upstream result
    /// and secret-free receipt, never the credential material.
    PlatformRelay,
}

/// Installed last-mile capabilities.  This is evidence, not preference policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRealizationCapabilities {
    pub holders: BTreeSet<PlaintextHolder>,
    pub material_sources: BTreeSet<CredentialMaterialSource>,
    pub realization_kinds: BTreeSet<CredentialRealizationKind>,
    #[serde(default)]
    pub recipient_bound_envelopes: bool,
    /// Installed external consumers and the namespaced material types each one
    /// accepts. This is execution evidence, not an extension preference.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extension_consumers: BTreeMap<String, BTreeSet<String>>,
    /// Independent adapter profiles installed in one process/Worker.
    ///
    /// Keeping profiles separate is security-significant: flattening a Native
    /// Worker-holder profile and an ACP Workload-holder profile into three unions
    /// would synthesize holder/source/realization combinations that no adapter
    /// actually implements. Legacy single-profile declarations leave this empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<CredentialRealizationCapabilities>,
}

/// WorkerManifest capability namespace for one canonical credential realization
/// evidence payload. The Worker contract remains credential-domain agnostic; the
/// publishing and consuming contexts own this codec.
pub const CREDENTIAL_REALIZATION_CAPABILITY_PREFIX: &str = "credential-realization.awaken.dev/v1:";
/// Namespaced extension-consumer key for the exact ACP backend that consumes a
/// credential realization profile. This keeps independently installed ACP
/// adapters correlated with their last-mile mechanism after Worker profiles
/// are composed.
pub const ACP_CREDENTIAL_CONSUMER_PREFIX: &str = "awaken.acp.credential-delivery/";
pub const PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE: &str =
    "awaken.credential.process-secret-environment/v1";
pub const PRIVATE_SECRET_FILE_MATERIAL_TYPE: &str = "awaken.credential.private-secret-file/v1";

impl CredentialRealizationCapabilities {
    /// Merge evidence from independently installed adapters on the same Worker.
    /// This describes the Worker's complete claim surface; the selected backend
    /// must still perform its own exact route and binding validation before it
    /// opens material.
    pub fn merge(&mut self, other: &Self) {
        self.holders.extend(other.holders.iter().cloned());
        self.material_sources
            .extend(other.material_sources.iter().copied());
        self.realization_kinds
            .extend(other.realization_kinds.iter().copied());
        self.recipient_bound_envelopes |= other.recipient_bound_envelopes;
        for (consumer, material_types) in &other.extension_consumers {
            self.extension_consumers
                .entry(consumer.clone())
                .or_default()
                .extend(material_types.iter().cloned());
        }
    }

    /// Whether this adapter/provider advertises no credential realization at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.holders.is_empty()
            && self.material_sources.is_empty()
            && self.realization_kinds.is_empty()
            && !self.recipient_bound_envelopes
            && self.extension_consumers.is_empty()
            && self.alternatives.iter().all(Self::is_empty)
    }

    /// Compose independent adapter evidence without creating a Cartesian-product
    /// capability. Nested compositions are flattened and empty profiles discarded.
    #[must_use]
    pub fn alternatives(
        profiles: impl IntoIterator<Item = CredentialRealizationCapabilities>,
    ) -> Self {
        let mut alternatives = Vec::new();
        for mut profile in profiles {
            alternatives.append(&mut profile.alternatives);
            if !profile.is_empty() {
                alternatives.push(profile);
            }
        }
        Self {
            alternatives,
            ..Self::default()
        }
    }

    pub(super) fn supports(
        &self,
        holder: &PlaintextHolder,
        source: CredentialMaterialSource,
        realization: CredentialRealizationKind,
        envelope: bool,
    ) -> bool {
        (self.holders.contains(holder)
            && self.material_sources.contains(&source)
            && self.realization_kinds.contains(&realization)
            && (!envelope || self.recipient_bound_envelopes))
            || self
                .alternatives
                .iter()
                .any(|profile| profile.supports(holder, source, realization, envelope))
    }

    pub(super) fn supports_extension(&self, consumer_id: &str, material_type: &str) -> bool {
        self.extension_consumers
            .get(consumer_id)
            .is_some_and(|types| types.contains(material_type))
            || self
                .alternatives
                .iter()
                .any(|profile| profile.supports_extension(consumer_id, material_type))
    }

    /// Resolve the one last-mile credential mechanism advertised for an exact
    /// ACP backend. Missing evidence preserves compatibility with legacy Worker
    /// manifests; contradictory evidence fails closed.
    pub fn acp_backend_realization_kind(
        &self,
        backend_ref: &str,
    ) -> Result<Option<CredentialRealizationKind>, String> {
        let consumer_id = format!("{ACP_CREDENTIAL_CONSUMER_PREFIX}{backend_ref}");
        let mut kinds = BTreeSet::new();
        for (material_type, kind) in [
            (
                PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE,
                CredentialRealizationKind::ProcessSecretEnvironment,
            ),
            (
                PRIVATE_SECRET_FILE_MATERIAL_TYPE,
                CredentialRealizationKind::PrivateSecretFile,
            ),
        ] {
            if self.supports_extension(&consumer_id, material_type) {
                kinds.insert(kind);
            }
        }
        match kinds.len() {
            0 => Ok(None),
            1 => Ok(kinds.into_iter().next()),
            _ => Err(format!(
                "ACP backend `{backend_ref}` advertises contradictory credential delivery mechanisms"
            )),
        }
    }

    /// Encode this evidence as one canonical Worker capability. Empty evidence
    /// emits no capability.
    pub fn manifest_capability(&self) -> Result<Option<String>, serde_json::Error> {
        if self.is_empty() {
            return Ok(None);
        }
        serde_json::to_string(self).map(|payload| {
            Some(format!(
                "{CREDENTIAL_REALIZATION_CAPABILITY_PREFIX}{payload}"
            ))
        })
    }

    /// Decode the unique credential realization capability from an otherwise
    /// domain-neutral Worker capability set. Multiple declarations fail closed.
    pub fn from_manifest_capabilities(capabilities: &BTreeSet<String>) -> Result<Self, String> {
        let mut encoded = capabilities.iter().filter_map(|capability| {
            capability.strip_prefix(CREDENTIAL_REALIZATION_CAPABILITY_PREFIX)
        });
        let Some(payload) = encoded.next() else {
            return Ok(Self::default());
        };
        if encoded.next().is_some() {
            return Err(
                "Worker manifest declares multiple credential realization capabilities".to_string(),
            );
        }
        serde_json::from_str(payload)
            .map_err(|error| format!("invalid credential realization capability: {error}"))
    }
}
