//! Process-local credential realization capabilities.

use super::*;

const ACP_MCP_CLIENT_INJECTION_MATERIAL_TYPE: &str =
    "awaken.credential.mcp-process-protocol-field/v1";

/// Compose the one installed-evidence profile shared by Session MCP staging,
/// local dispatch claims, and registered-Worker capability publication.
/// Adapter declaration, material source, holder, and last-mile mechanism stay
/// in one alternative so claim admission cannot synthesize them across peers.
pub(crate) fn acp_mcp_client_injection_capabilities(
    materializer: &awaken_credential_materializer::PinnedCredentialMaterializer,
    cli: &str,
    http_transport: bool,
) -> Option<awaken_runtime_contract::CredentialRealizationCapabilities> {
    use awaken_runtime_contract::{
        CredentialRealizationCapabilities, CredentialRealizationKind, PlaintextBoundary,
    };

    let adapter = awaken_run_executor_acp::acp_cli(cli)?;
    let delivery = awaken_credential_contract::select_mcp_credential_delivery(
        PlaintextBoundary::Workload,
        CredentialRealizationKind::ProcessProtocolField,
    );
    if !adapter.admits_mcp_client_credential(delivery, http_transport) {
        return None;
    }
    let (material_sources, recipient_bound_envelopes) = materializer.material_source_capabilities();
    let holder =
        awaken_runtime_contract::CredentialRealizationProfile::self_hosted_acp().mcp_holder;
    Some(CredentialRealizationCapabilities {
        holders: [holder].into_iter().collect(),
        material_sources,
        realization_kinds: [CredentialRealizationKind::ProcessProtocolField]
            .into_iter()
            .collect(),
        recipient_bound_envelopes,
        extension_consumers: [(
            format!(
                "{}acp:{cli}",
                awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX
            ),
            [ACP_MCP_CLIENT_INJECTION_MATERIAL_TYPE.to_string()]
                .into_iter()
                .collect(),
        )]
        .into_iter()
        .collect(),
        alternatives: Vec::new(),
    })
}

impl SharedHost {
    /// Exact independent credential-adapter profiles installed in this process.
    /// Both the process dispatch pool and per-Session durable ingress consume this
    /// one declaration; neither may infer custody from only the Native adapter.
    pub(crate) fn local_credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let mut profiles = vec![self.inference_routing.credential_realization_capabilities()];
        // Remote attempts are an independent adapter just like Native and ACP.
        // Preserve their profile as an alternative so claim admission can use
        // the installed A2A relay without synthesizing cross-adapter evidence.
        profiles.push(self.remote_credential_realization.clone());
        if let Some(materializer) = &self.credential_materializer {
            // Managed MCP executes through the same exact materializer installed
            // on the Host. Claim admission must therefore see its WorkerRelay
            // profile; inference/ACP profiles alone reject an otherwise
            // realizable frozen MCP credential and poison the local dispatch row.
            profiles.push(materializer.worker_relay_capabilities());
            if self.acp.is_some() {
                let configured = self
                    .deployment
                    .acp
                    .as_ref()
                    .map(|profile| profile.cli_ids().map(str::to_string).collect::<Vec<_>>())
                    .unwrap_or_else(|| {
                        awaken_run_executor_acp::known_acp_clis()
                            .iter()
                            .map(|cli| cli.id.to_string())
                            .collect()
                    });
                profiles.extend(configured.into_iter().filter_map(|cli| {
                    acp_mcp_client_injection_capabilities(materializer, &cli, true)
                }));
            }
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

    fn has_acp_mcp_client_injection(
        capabilities: &awaken_runtime_contract::CredentialRealizationCapabilities,
        backend_ref: &str,
    ) -> bool {
        let consumer = format!(
            "{}{backend_ref}",
            awaken_runtime_contract::credential::ACP_CREDENTIAL_CONSUMER_PREFIX
        );
        std::iter::once(capabilities)
            .chain(capabilities.alternatives.iter())
            .any(|profile| {
                profile.holders.contains(
                    &awaken_runtime_contract::CredentialRealizationProfile::self_hosted_acp()
                        .mcp_holder,
                ) && profile
                    .material_sources
                    .contains(&CredentialMaterialSource::ControlPlaneReference)
                    && profile
                        .realization_kinds
                        .contains(&CredentialRealizationKind::ProcessProtocolField)
                    && profile.extension_consumers.contains_key(&consumer)
            })
    }

    #[test]
    fn local_capabilities_publish_only_installed_session_mcp_mechanisms() {
        // Causes: C1 materializer absent/present; C2 ACP executor absent/present;
        // C3 exact catalog adapter declares HTTP ClientInjection; C4 an
        // independent Remote adapter declares WorkerRelay.
        // Effects: E1 WorkerRelay requires C1 or C4; E2 ProcessProtocolField
        // requires C1+C2+C3 and is qualified by the exact ACP backend key.
        // Constraints/invariants: every adapter is one alternative; holder,
        // source, mechanism, and declaration are never flattened or inferred
        // from the Session request. Decision rules: R1=!C1=>neither; R2=C1&&!C2
        // =>WorkerRelay only; R3=C1+C2+C3=>both; R4=C4=>WorkerRelay only;
        // R5=C1+C2+undeclared backend=>no client-injection profile for it.
        let without = SharedHost::new(Arc::new(crate::host::tests::MemoryHostModel), "test-model");
        let launch = awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]);
        let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
            launch,
        ));
        let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
        let acp_without_materializer =
            SharedHost::new(Arc::new(crate::host::tests::MemoryHostModel), "test-model")
                .with_acp(executor.clone());
        assert!(
            !has_worker_relay(&without.local_credential_realization_capabilities()),
            "R1: a host without a relay-capable adapter must fail closed"
        );
        assert!(
            !has_acp_mcp_client_injection(
                &acp_without_materializer.local_credential_realization_capabilities(),
                "acp:claude",
            ),
            "R1: an ACP executor without material access cannot advertise client injection"
        );

        let materializer =
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(Arc::new(
                ControlReferenceResolver,
            ));
        let remote_profile = materializer.worker_relay_capabilities();
        let mut with_remote =
            SharedHost::new(Arc::new(crate::host::tests::MemoryHostModel), "test-model");
        with_remote.remote_credential_realization = remote_profile;
        assert!(
            has_worker_relay(&with_remote.local_credential_realization_capabilities()),
            "R3: the installed Remote adapter must be visible to claim admission"
        );

        let with = without.with_credential_materializer(materializer.clone());
        assert!(
            has_worker_relay(&with.local_credential_realization_capabilities()),
            "R2: the installed exact materializer must be visible to claim admission"
        );
        assert!(
            !has_acp_mcp_client_injection(
                &with.local_credential_realization_capabilities(),
                "acp:claude",
            ),
            "R2: material access alone does not declare an ACP client"
        );

        let with_acp = SharedHost::new(Arc::new(crate::host::tests::MemoryHostModel), "test-model")
            .with_credential_materializer(materializer)
            .with_acp(executor);
        let capabilities = with_acp.local_credential_realization_capabilities();
        assert!(
            has_worker_relay(&capabilities),
            "R3: legacy relay remains installed"
        );
        assert!(
            has_acp_mcp_client_injection(&capabilities, "acp:claude"),
            "R3: the declared ACP client publishes its exact process-protocol profile"
        );
        assert!(
            !has_acp_mcp_client_injection(&capabilities, "acp:codex"),
            "R5: an undeclared client-injection adapter is not synthesized"
        );
    }
}
