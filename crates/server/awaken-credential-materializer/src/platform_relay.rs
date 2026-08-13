use awaken_runtime_contract::{
    CredentialRealizationCapabilities, CredentialRealizationKind, PlaintextBoundary,
    PlaintextHolder,
};

use crate::PinnedCredentialMaterializer;

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
        self.realization_capabilities(
            PlaintextHolder::new(PlaintextBoundary::Platform, trust_domain),
            CredentialRealizationKind::PlatformRelay,
        )
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
        );

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
