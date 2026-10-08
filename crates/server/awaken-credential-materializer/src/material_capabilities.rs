// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: One projection of installed exact holder/source/envelope tuples into Native, ACP, MCP
// @formal reason: and platform profiles. Concrete resolver and codec tests cover projection; the separate
// @formal reason: Boolean admission proof does not prove arbitrary resolver declarations, material
// @formal reason: availability, or this complete adapter.
//! One projection from installed material tuples to last-mile execution evidence.
use std::collections::BTreeSet;

use awaken_credential_contract::{
    CredentialMaterialCapability, CredentialRealizationCapabilities, CredentialRealizationKind,
};
use awaken_runtime_contract::{CredentialMaterialSource, PlaintextBoundary, PlaintextHolder};

use super::PinnedCredentialMaterializer;

impl PinnedCredentialMaterializer {
    pub(super) fn installed_material_capabilities(&self) -> BTreeSet<CredentialMaterialCapability> {
        let mut installed = self
            .external_material_resolver
            .as_ref()
            .map(|resolver| resolver.material_capabilities())
            .unwrap_or_default();
        if self.has_local_authority() {
            for holder in [
                PlaintextHolder::new(
                    PlaintextBoundary::Worker,
                    awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                ),
                PlaintextHolder::new(
                    PlaintextBoundary::Workload,
                    awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
                ),
            ] {
                installed.insert(CredentialMaterialCapability {
                    holder,
                    source: CredentialMaterialSource::ControlPlaneReference,
                    envelope: false,
                });
            }
        }
        installed
    }

    /// Exact installed evidence for the caller's already-selected holder and
    /// mechanism. Local Vault custody can serve that policy-selected boundary;
    /// external custody must declare the complete tuple before admission.
    #[must_use]
    pub fn realization_capabilities(
        &self,
        holder: PlaintextHolder,
        realization: CredentialRealizationKind,
    ) -> CredentialRealizationCapabilities {
        let mut installed = self.installed_material_capabilities();
        if self.has_local_authority() {
            installed.insert(CredentialMaterialCapability {
                holder: holder.clone(),
                source: CredentialMaterialSource::ControlPlaneReference,
                envelope: false,
            });
        }
        let profiles = installed
            .into_iter()
            .filter(|profile| profile.holder == holder)
            .map(|profile| CredentialRealizationCapabilities {
                holders: [profile.holder].into_iter().collect(),
                material_sources: [profile.source].into_iter().collect(),
                realization_kinds: [realization].into_iter().collect(),
                recipient_bound_envelopes: profile.envelope,
                envelope_required: profile.envelope,
                ..Default::default()
            });
        CredentialRealizationCapabilities::alternatives(profiles)
    }

    fn boundary_capabilities(
        &self,
        boundary: PlaintextBoundary,
        realization: CredentialRealizationKind,
    ) -> CredentialRealizationCapabilities {
        let holders: BTreeSet<_> = self
            .installed_material_capabilities()
            .into_iter()
            .filter(|profile| profile.holder.boundary == boundary)
            .map(|profile| profile.holder)
            .collect();
        CredentialRealizationCapabilities::alternatives(
            holders
                .into_iter()
                .map(|holder| self.realization_capabilities(holder, realization)),
        )
    }

    /// Exact evidence for the Worker-side Native provider adapter.
    #[must_use]
    pub fn provider_adapter_capabilities(&self) -> CredentialRealizationCapabilities {
        self.boundary_capabilities(
            PlaintextBoundary::Worker,
            CredentialRealizationKind::WorkerProviderAdapter,
        )
    }

    /// Exact evidence for Worker-held mediated transport/material adapters.
    #[must_use]
    pub fn worker_relay_capabilities(&self) -> CredentialRealizationCapabilities {
        let mut profile = self.boundary_capabilities(
            PlaintextBoundary::Worker,
            CredentialRealizationKind::WorkerRelay,
        );
        profile.extension_consumers = self.extension_capabilities();
        profile
    }

    /// Exact evidence for a Workload process-secret adapter.
    #[must_use]
    pub fn process_secret_capabilities(&self) -> CredentialRealizationCapabilities {
        self.boundary_capabilities(
            PlaintextBoundary::Workload,
            CredentialRealizationKind::ProcessSecretEnvironment,
        )
    }

    /// The same backend-specific projection is consumed by Host and Worker
    /// manifest assembly. Neither caller rewrites only an aggregate profile.
    #[must_use]
    pub fn acp_model_capabilities(
        &self,
        cli_id: &str,
        delivery: awaken_acp_contract::managed_credential::ManagedCredentialDelivery,
    ) -> CredentialRealizationCapabilities {
        let (kind, material_type) = match delivery {
            awaken_acp_contract::managed_credential::ManagedCredentialDelivery::ProcessSecret => (
                CredentialRealizationKind::ProcessSecretEnvironment,
                awaken_credential_contract::PROCESS_SECRET_ENVIRONMENT_MATERIAL_TYPE,
            ),
            awaken_acp_contract::managed_credential::ManagedCredentialDelivery::Artifact(_) => (
                CredentialRealizationKind::PrivateSecretFile,
                awaken_credential_contract::PRIVATE_SECRET_FILE_MATERIAL_TYPE,
            ),
        };
        let mut profile = self.boundary_capabilities(PlaintextBoundary::Workload, kind);
        if !profile.is_empty() {
            profile.extension_consumers.insert(
                format!(
                    "{}acp:{}",
                    awaken_credential_contract::ACP_CREDENTIAL_CONSUMER_PREFIX,
                    cli_id
                ),
                [material_type.to_string()].into_iter().collect(),
            );
        }
        profile
    }
}
