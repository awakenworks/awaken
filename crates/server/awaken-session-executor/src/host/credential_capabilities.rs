//! Process-local credential realization capabilities.

use super::*;

impl SharedHost {
    /// Exact independent credential-adapter profiles installed in this process.
    /// Both the process dispatch pool and per-Session durable ingress consume this
    /// one declaration; neither may infer custody from only the Native adapter.
    pub(crate) fn local_credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let mut profiles = vec![self.inference_routing.credential_realization_capabilities()];
        if let Some(materializer) = &self.credential_materializer {
            // Managed MCP executes through the same exact materializer installed
            // on the Host. Claim admission must therefore see its WorkerRelay
            // profile; inference/ACP profiles alone reject an otherwise
            // realizable frozen MCP credential and poison the local dispatch row.
            profiles.push(materializer.worker_relay_capabilities());
        }
        if let (Some(acp), Some(profile)) = (&self.acp, &self.deployment.acp) {
            profiles.extend(profile.cli_ids().filter_map(|cli| {
                let backend =
                    awaken_runtime_contract::resolved::Backend::from_ref(&format!("acp:{cli}"));
                match acp.credential_realization_capabilities(&backend) {
                    Ok(capabilities) => Some(capabilities),
                    Err(error) => {
                        tracing::error!(backend = %format!("acp:{cli}"), %error,
                            "configured ACP credential capability is unavailable");
                        None
                    }
                }
            }));
        }
        awaken_runtime_contract::CredentialRealizationCapabilities::alternatives(profiles)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use awaken_runtime_contract::{
        CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
        CredentialMaterialSource, CredentialRealizationKind, PlaintextBoundary, PlaintextHolder,
        ResolvedCredentialMaterial,
    };

    use super::*;

    struct ControlReferenceResolver;

    #[async_trait::async_trait]
    impl CredentialMaterialResolver for ControlReferenceResolver {
        fn supported_material_sources(&self) -> BTreeSet<CredentialMaterialSource> {
            BTreeSet::from([CredentialMaterialSource::ControlPlaneReference])
        }

        async fn resolve_exact(
            &self,
            _request: CredentialMaterialRequest<'_>,
        ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
            Err(CredentialMaterialError::Unavailable)
        }
    }

    fn has_worker_relay(
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
    ) -> bool {
        std::iter::once(capabilities)
            .chain(capabilities.alternatives.iter())
            .any(|profile| {
                profile.holders.contains(&PlaintextHolder::new(
                    PlaintextBoundary::Worker,
                    awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                )) && profile
                    .material_sources
                    .contains(&CredentialMaterialSource::ControlPlaneReference)
                    && profile
                        .realization_kinds
                        .contains(&CredentialRealizationKind::WorkerRelay)
            })
    }

    #[test]
    fn local_capabilities_publish_worker_relay_only_when_materializer_is_installed() {
        let without = SharedHost::new(Arc::new(crate::host::tests::MemoryHostModel), "test-model");
        assert!(
            !has_worker_relay(&without.local_credential_realization_capabilities()),
            "a host without a materializer must fail closed"
        );

        let materializer =
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(Arc::new(
                ControlReferenceResolver,
            ));
        let with = without.with_credential_materializer(materializer);
        assert!(
            has_worker_relay(&with.local_credential_realization_capabilities()),
            "the installed exact materializer must be visible to claim admission"
        );
    }
}
