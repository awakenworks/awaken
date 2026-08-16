//! Installed credential realization evidence and its Worker manifest codec.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{CredentialMaterialSource, PlaintextBoundary, PlaintextHolder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRealizationKind {
    ProcessSecretEnvironment,
    PrivateSecretFile,
    /// Material crosses only an already-confined process protocol channel and
    /// is consumed as a typed field; it is neither an OS environment variable
    /// nor a durable/config-file projection.
    ProcessProtocolField,
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

/// How an authenticated MCP client receives access to its upstream server.
///
/// This is a derived, secret-free execution fact. It deliberately does not add
/// a field to the persisted Session or Worker-manifest wire: retained rows keep
/// their existing holder and realization-kind encoding, while new admission
/// code can classify that exact pair without an implicit migration default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum McpCredentialDelivery {
    /// The selected trusted MCP client boundary receives credential material
    /// and injects it into the upstream protocol request.
    ClientInjection,
    /// A Platform-held egress mediator injects credential material; Runtime and
    /// workload receive only the mediated route.
    GatewayMediation,
}

/// Derive the sole MCP delivery mode represented by an exact holder/mechanism
/// pair. Every cross-boundary or nonsensical pairing fails closed.
///
/// `WorkerRelay` remains readable on the existing wire, but is intentionally
/// not reclassified as client injection. A future migration may replace that
/// legacy mechanism only through an explicit Session generation change.
#[must_use]
pub const fn select_mcp_credential_delivery(
    boundary: PlaintextBoundary,
    realization: CredentialRealizationKind,
) -> Option<McpCredentialDelivery> {
    match (boundary, realization) {
        (PlaintextBoundary::Workload, CredentialRealizationKind::ProcessProtocolField) => {
            Some(McpCredentialDelivery::ClientInjection)
        }
        (PlaintextBoundary::Platform, CredentialRealizationKind::PlatformRelay) => {
            Some(McpCredentialDelivery::GatewayMediation)
        }
        _ => None,
    }
}

const fn delivery_is(
    selected: Option<McpCredentialDelivery>,
    expected: McpCredentialDelivery,
) -> bool {
    matches!(
        (selected, expected),
        (
            Some(McpCredentialDelivery::ClientInjection),
            McpCredentialDelivery::ClientInjection
        ) | (
            Some(McpCredentialDelivery::GatewayMediation),
            McpCredentialDelivery::GatewayMediation
        )
    )
}

const fn same_boundary(left: PlaintextBoundary, right: PlaintextBoundary) -> bool {
    matches!(
        (left, right),
        (PlaintextBoundary::Workload, PlaintextBoundary::Workload)
            | (PlaintextBoundary::Worker, PlaintextBoundary::Worker)
            | (PlaintextBoundary::Platform, PlaintextBoundary::Platform)
    )
}

const fn same_realization(
    left: CredentialRealizationKind,
    right: CredentialRealizationKind,
) -> bool {
    matches!(
        (left, right),
        (
            CredentialRealizationKind::ProcessSecretEnvironment,
            CredentialRealizationKind::ProcessSecretEnvironment
        ) | (
            CredentialRealizationKind::PrivateSecretFile,
            CredentialRealizationKind::PrivateSecretFile
        ) | (
            CredentialRealizationKind::ProcessProtocolField,
            CredentialRealizationKind::ProcessProtocolField
        ) | (
            CredentialRealizationKind::WorkerProviderAdapter,
            CredentialRealizationKind::WorkerProviderAdapter
        ) | (
            CredentialRealizationKind::WorkerRelay,
            CredentialRealizationKind::WorkerRelay
        ) | (
            CredentialRealizationKind::PlatformProviderAdapter,
            CredentialRealizationKind::PlatformProviderAdapter
        ) | (
            CredentialRealizationKind::PlatformRelay,
            CredentialRealizationKind::PlatformRelay
        )
    )
}

/// Representation-free kernel for delivery-aware MCP realization receipts.
///
/// Complete Session request/receipt identity is supplied as `exact_binding` by
/// the Session contract. This kernel additionally requires the exact holder,
/// exact realization mechanism, and their one valid delivery classification;
/// matching only a broader delivery category can never authorize a receipt.
#[must_use]
pub const fn mcp_credential_delivery_receipt_matches(
    expected_delivery: McpCredentialDelivery,
    expected_boundary: PlaintextBoundary,
    expected_realization: CredentialRealizationKind,
    actual_boundary: PlaintextBoundary,
    actual_realization: CredentialRealizationKind,
    exact_binding: bool,
) -> bool {
    exact_binding
        && same_boundary(expected_boundary, actual_boundary)
        && same_realization(expected_realization, actual_realization)
        && delivery_is(
            select_mcp_credential_delivery(expected_boundary, expected_realization),
            expected_delivery,
        )
        && delivery_is(
            select_mcp_credential_delivery(actual_boundary, actual_realization),
            expected_delivery,
        )
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

#[cfg(kani)]
mod verification {
    use super::*;

    fn boundary(tag: u8) -> PlaintextBoundary {
        match tag % 3 {
            0 => PlaintextBoundary::Workload,
            1 => PlaintextBoundary::Worker,
            _ => PlaintextBoundary::Platform,
        }
    }

    fn realization(tag: u8) -> CredentialRealizationKind {
        match tag % 7 {
            0 => CredentialRealizationKind::ProcessSecretEnvironment,
            1 => CredentialRealizationKind::PrivateSecretFile,
            2 => CredentialRealizationKind::ProcessProtocolField,
            3 => CredentialRealizationKind::WorkerProviderAdapter,
            4 => CredentialRealizationKind::WorkerRelay,
            5 => CredentialRealizationKind::PlatformProviderAdapter,
            _ => CredentialRealizationKind::PlatformRelay,
        }
    }

    fn delivery(tag: bool) -> McpCredentialDelivery {
        if tag {
            McpCredentialDelivery::GatewayMediation
        } else {
            McpCredentialDelivery::ClientInjection
        }
    }

    #[kani::proof]
    fn mcp_delivery_selection_has_only_the_explicit_pair_classes() {
        let boundary = boundary(kani::any());
        let realization = realization(kani::any());
        let selected = select_mcp_credential_delivery(boundary, realization);
        assert_eq!(
            selected,
            match (boundary, realization) {
                (PlaintextBoundary::Workload, CredentialRealizationKind::ProcessProtocolField) => {
                    Some(McpCredentialDelivery::ClientInjection)
                }
                (PlaintextBoundary::Platform, CredentialRealizationKind::PlatformRelay) => {
                    Some(McpCredentialDelivery::GatewayMediation)
                }
                _ => None,
            }
        );
    }

    #[kani::proof]
    fn mcp_delivery_receipt_requires_exact_binding_holder_and_mechanism() {
        let expected_delivery = delivery(kani::any());
        let expected_boundary = boundary(kani::any());
        let expected_realization = realization(kani::any());
        let actual_boundary = boundary(kani::any());
        let actual_realization = realization(kani::any());
        let exact_binding = kani::any();
        let matches = mcp_credential_delivery_receipt_matches(
            expected_delivery,
            expected_boundary,
            expected_realization,
            actual_boundary,
            actual_realization,
            exact_binding,
        );
        if matches {
            assert!(exact_binding);
            assert_eq!(expected_boundary, actual_boundary);
            assert_eq!(expected_realization, actual_realization);
            assert_eq!(
                select_mcp_credential_delivery(actual_boundary, actual_realization),
                Some(expected_delivery)
            );
        }
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    #[test]
    fn process_protocol_field_wire_is_explicit_and_round_trips() {
        assert_eq!(
            serde_json::to_string(&CredentialRealizationKind::ProcessProtocolField).unwrap(),
            "\"process_protocol_field\""
        );
        assert_eq!(
            serde_json::from_str::<CredentialRealizationKind>("\"process_protocol_field\"")
                .unwrap(),
            CredentialRealizationKind::ProcessProtocolField
        );
    }

    #[test]
    fn delivery_selection_is_a_closed_holder_mechanism_matrix() {
        let boundaries = [
            PlaintextBoundary::Workload,
            PlaintextBoundary::Worker,
            PlaintextBoundary::Platform,
        ];
        let realizations = [
            CredentialRealizationKind::ProcessSecretEnvironment,
            CredentialRealizationKind::PrivateSecretFile,
            CredentialRealizationKind::ProcessProtocolField,
            CredentialRealizationKind::WorkerProviderAdapter,
            CredentialRealizationKind::WorkerRelay,
            CredentialRealizationKind::PlatformProviderAdapter,
            CredentialRealizationKind::PlatformRelay,
        ];
        for boundary in boundaries {
            for realization in realizations {
                let expected = match (boundary, realization) {
                    (
                        PlaintextBoundary::Workload,
                        CredentialRealizationKind::ProcessProtocolField,
                    ) => Some(McpCredentialDelivery::ClientInjection),
                    (PlaintextBoundary::Platform, CredentialRealizationKind::PlatformRelay) => {
                        Some(McpCredentialDelivery::GatewayMediation)
                    }
                    _ => None,
                };
                assert_eq!(
                    select_mcp_credential_delivery(boundary, realization),
                    expected,
                    "boundary={boundary:?}, realization={realization:?}"
                );
            }
        }
    }

    #[test]
    fn receipt_matching_rejects_partial_reclassified_and_legacy_relay_matches() {
        assert!(mcp_credential_delivery_receipt_matches(
            McpCredentialDelivery::GatewayMediation,
            PlaintextBoundary::Platform,
            CredentialRealizationKind::PlatformRelay,
            PlaintextBoundary::Platform,
            CredentialRealizationKind::PlatformRelay,
            true,
        ));
        assert!(!mcp_credential_delivery_receipt_matches(
            McpCredentialDelivery::ClientInjection,
            PlaintextBoundary::Platform,
            CredentialRealizationKind::PlatformRelay,
            PlaintextBoundary::Platform,
            CredentialRealizationKind::PlatformRelay,
            true,
        ));
        assert!(!mcp_credential_delivery_receipt_matches(
            McpCredentialDelivery::GatewayMediation,
            PlaintextBoundary::Platform,
            CredentialRealizationKind::PlatformRelay,
            PlaintextBoundary::Worker,
            CredentialRealizationKind::PlatformRelay,
            true,
        ));
        assert!(!mcp_credential_delivery_receipt_matches(
            McpCredentialDelivery::GatewayMediation,
            PlaintextBoundary::Platform,
            CredentialRealizationKind::PlatformRelay,
            PlaintextBoundary::Platform,
            CredentialRealizationKind::PlatformRelay,
            false,
        ));
        assert_eq!(
            select_mcp_credential_delivery(
                PlaintextBoundary::Worker,
                CredentialRealizationKind::WorkerRelay,
            ),
            None,
            "retained WorkerRelay wire is never silently reclassified"
        );
    }
}
