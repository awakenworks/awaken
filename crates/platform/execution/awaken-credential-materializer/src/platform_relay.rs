// @formal entrypoint credential.http_effect_usage_binds_exact_fields_and_placements
// @formal entrypoint credential.platform_relay_requires_exact_holder_workspace_usage_and_target
use awaken_agent_contract::credential::{PlaintextBoundary, PlaintextHolder};
use awaken_credential_contract::{CredentialRealizationCapabilities, CredentialRealizationKind};

use crate::PinnedCredentialMaterializer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlatformRelayProjection<T> {
    boundary: PlaintextBoundary,
    trust_domain: T,
    realization: CredentialRealizationKind,
}

/// Local admission kernel for the built-in platform effect boundary. Boundary
/// and mechanism are deliberately exact categories, not ordered strengths, so
/// every other pairing fails closed.
#[must_use]
const fn platform_relay_local_gate(
    boundary: PlaintextBoundary,
    realization: CredentialRealizationKind,
) -> bool {
    matches!(boundary, PlaintextBoundary::Platform)
        && matches!(realization, CredentialRealizationKind::PlatformRelay)
}

#[must_use]
const fn platform_relay_projection<T>(trust_domain: T) -> PlatformRelayProjection<T> {
    PlatformRelayProjection {
        boundary: PlaintextBoundary::Platform,
        trust_domain,
        realization: CredentialRealizationKind::PlatformRelay,
    }
}

impl PinnedCredentialMaterializer {
    /// Exact evidence for a platform-held, secretless-caller egress adapter.
    ///
    /// The returned profile authorizes no holder by itself: publication policy
    /// must independently allow this exact opaque trust domain. A Gateway uses
    /// this only in the process that owns its last-mile effect implementation;
    /// it is not evidence for a plaintext-returning RPC.
    #[must_use]
    pub fn platform_relay_capabilities(
        &self,
        trust_domain: impl Into<String>,
    ) -> CredentialRealizationCapabilities {
        let projection = platform_relay_projection(trust_domain.into());
        debug_assert!(platform_relay_local_gate(
            projection.boundary,
            projection.realization,
        ));
        self.realization_capabilities(
            PlaintextHolder::new(projection.boundary, projection.trust_domain),
            projection.realization,
        )
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn arbitrary_boundary(tag: u8) -> PlaintextBoundary {
        match tag % 3 {
            0 => PlaintextBoundary::Workload,
            1 => PlaintextBoundary::Worker,
            _ => PlaintextBoundary::Platform,
        }
    }

    fn arbitrary_realization(tag: u8) -> CredentialRealizationKind {
        match tag % 6 {
            0 => CredentialRealizationKind::ProcessSecretEnvironment,
            1 => CredentialRealizationKind::PrivateSecretFile,
            2 => CredentialRealizationKind::WorkerProviderAdapter,
            3 => CredentialRealizationKind::WorkerRelay,
            4 => CredentialRealizationKind::PlatformProviderAdapter,
            _ => CredentialRealizationKind::PlatformRelay,
        }
    }

    /// Proves the local gate's complete product space has exactly one admitted
    /// boundary/mechanism pair, while construction preserves the opaque domain.
    /// @obligation credential.platform_relay_requires_exact_holder_workspace_usage_and_target
    #[kani::proof]
    fn platform_relay_local_gate_admits_only_exact_platform_relay() {
        let boundary = arbitrary_boundary(kani::any());
        let realization = arbitrary_realization(kani::any());
        let admitted = platform_relay_local_gate(boundary, realization);
        assert_eq!(
            admitted,
            matches!(boundary, PlaintextBoundary::Platform)
                && matches!(realization, CredentialRealizationKind::PlatformRelay)
        );

        let trust_domain_identity = kani::any::<u64>();
        let projected = platform_relay_projection(trust_domain_identity);
        assert!(platform_relay_local_gate(
            projected.boundary,
            projected.realization,
        ));
        assert_eq!(projected.trust_domain, trust_domain_identity);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::credential::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, HttpEffectPlacement, ModelExposurePolicy, PlaintextBoundary,
        PlaintextHolder,
    };
    use awaken_agent_contract::{credential::CredentialPurpose, credential::CredentialTarget};
    use awaken_credential_contract::CredentialRealizationKind;
    use awaken_credential_contract::{
        CredentialDescriptor, CredentialMaterialDescriptor, CredentialTargetContract,
        OPAQUE_SECRET_MATERIAL_TYPE,
    };
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};

    use crate::pinned_vault;

    /// Platform-relay cause/effect graph: C1 canonical local authority stores
    /// are installed; C2 publication allows the exact Platform trust domain;
    /// C3 source revision and Workspace match; C4 caller selects PlatformRelay;
    /// C5 usage is the built-in HTTP effect. C1+C2+C3+C4+C5 resolves material
    /// only inside that holder process and advertises no Extension consumer. A
    /// wrong Workspace or trust domain fails before the caller may execute an
    /// effect.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    /// |---|---|---|---|---|---|---|
    /// | P1 | T | T | T | T | T | exact material at Platform holder; no Extension |
    /// | P2 | T | T | F | T | T | fail closed |
    /// | P3 | T | F | T | T | T | fail closed |
    #[tokio::test]
    async fn platform_relay_resolves_only_an_exact_holder_and_workspace() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let usage = CredentialUsage::HttpEffect {
            fields: BTreeMap::from([(
                "token".into(),
                BTreeSet::from([HttpEffectPlacement::Header {
                    name: "authorization".into(),
                }]),
            )]),
        };
        let target =
            CredentialTarget::new(CredentialPurpose::HttpEffect, "https://api.example.test");
        let mut source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("domain-pack/provider".into()),
                env_key: None,
                secret: Some(RedactedString::new("platform-only")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        source.provider_id = None;
        source.descriptor = Some(CredentialDescriptor::new(
            "domain-pack/provider",
            CredentialMaterialDescriptor::secret(OPAQUE_SECRET_MATERIAL_TYPE),
            [CredentialTargetContract::new(target.clone(), usage.clone())],
        ));
        credentials.put(source.clone()).await.unwrap();
        let materializer = pinned_vault(credentials, secrets, awaken_time::system_clock());
        let holder = PlaintextHolder::new(PlaintextBoundary::Platform, "gateway.beta");
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version).unwrap(),
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(target);

        let capabilities = materializer.platform_relay_capabilities("gateway.beta");
        assert!(capabilities.holders.contains(&holder));
        assert!(
            capabilities
                .realization_kinds
                .contains(&CredentialRealizationKind::PlatformRelay)
        );
        assert!(
            capabilities.extension_consumers.is_empty(),
            "built-in platform HTTP effects never enter Extension consumers"
        );
        let resolved = materializer
            .resolve_for_workspace(
                &access,
                &holder,
                CredentialRealizationKind::PlatformRelay,
                "workspace-a",
                &("route-1", "POST", "/issues"),
            )
            .await
            .unwrap();
        assert_eq!(
            resolved.material.into_secret().unwrap().expose_secret(),
            "platform-only"
        );
        assert!(
            materializer
                .resolve_for_workspace(
                    &access,
                    &holder,
                    CredentialRealizationKind::PlatformRelay,
                    "workspace-b",
                    &("route-1", "POST", "/issues"),
                )
                .await
                .is_err()
        );
        let wrong_holder = PlaintextHolder::new(PlaintextBoundary::Platform, "another-gateway");
        assert!(
            materializer
                .resolve_for_workspace(
                    &access,
                    &wrong_holder,
                    CredentialRealizationKind::PlatformRelay,
                    "workspace-a",
                    &("route-1", "POST", "/issues"),
                )
                .await
                .is_err()
        );
    }
}
