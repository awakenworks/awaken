use awaken_runtime_contract::{
    CredentialRealizationCapabilities, CredentialRealizationKind, PlaintextBoundary,
    PlaintextHolder,
};

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

#[cfg(all(test, feature = "authority"))]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource,
        CredentialRealizationKind, CredentialRef, CredentialUsage, HttpEffectPlacement,
        ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };

    use crate::PinnedCredentialMaterializer;

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
        let source = enter_credential(
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
        )
        .await
        .unwrap();
        let materializer = PinnedCredentialMaterializer::new(credentials, secrets);
        let holder = PlaintextHolder::new(PlaintextBoundary::Platform, "gateway.beta");
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version).unwrap(),
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpEffect {
                fields: BTreeMap::from([(
                    "token".into(),
                    BTreeSet::from([HttpEffectPlacement::Header {
                        name: "authorization".into(),
                    }]),
                )]),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(awaken_credential_contract::CredentialTarget::new(
            awaken_credential_contract::CredentialPurpose::HttpEffect,
            "https://api.example.test",
        ));

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
