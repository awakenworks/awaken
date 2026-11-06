// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: This source implements the repository-controlled credential product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Process-local credential realization capabilities.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_credential_contract::{
    ACP_CREDENTIAL_CONSUMER_PREFIX, CredentialRealizationCapabilities, CredentialRealizationProfile,
};

const ACP_MCP_CLIENT_INJECTION_MATERIAL_TYPE: &str =
    "awaken.credential.mcp-process-protocol-field/v1";

/// Compose the one installed-evidence profile shared by Session MCP staging,
/// local dispatch claims, and registered-Worker capability publication.
/// Adapter declaration, material source, holder, and last-mile mechanism stay
/// in one alternative so claim admission cannot synthesize them across peers.
pub(crate) fn acp_mcp_client_injection_capabilities(
    materializer: &awaken_credential_materializer::PinnedCredentialMaterializer,
    policy: &dyn awaken_acp_contract::AcpAdapterPolicy<
        awaken_runtime_contract::resolved::ProviderServerTool,
    >,
    cli: &str,
    http_transport: bool,
) -> Option<CredentialRealizationCapabilities> {
    use awaken_agent_contract::credential::PlaintextBoundary;
    use awaken_credential_contract::CredentialRealizationKind;

    let delivery = awaken_credential_contract::select_mcp_credential_delivery(
        PlaintextBoundary::Workload,
        CredentialRealizationKind::ProcessProtocolField,
    );
    if !policy.admits_mcp_client_credential(cli, delivery, http_transport) {
        return None;
    }
    let holder = CredentialRealizationProfile::self_hosted_acp().mcp_holder;
    let mut capabilities = materializer
        .realization_capabilities(holder, CredentialRealizationKind::ProcessProtocolField);
    if capabilities.is_empty() {
        return None;
    }
    capabilities.extension_consumers.insert(
        format!("{}acp:{cli}", ACP_CREDENTIAL_CONSUMER_PREFIX),
        [ACP_MCP_CLIENT_INJECTION_MATERIAL_TYPE.to_string()]
            .into_iter()
            .collect(),
    );
    Some(capabilities)
}

/// Read the actual resident's process defaults through its existing lifecycle
/// and adoption owners. This application-side projection retains no state and
/// never treats an absent baseline or a different resident as empty evidence.
pub(crate) fn resident_process_environment(
    realization: &crate::realization_executor::RealizationExecutor,
    thread: &str,
    environment: &Arc<crate::session_environment::SessionEnvironment>,
) -> Result<Vec<awaken_provisioning_contract::EnvVar>, crate::realization_error::RealizationError> {
    let holding = realization.environments();
    holding.resident_environment_activity_identity(thread, environment)?;
    let provider = holding.projected_session_environment_provider(thread, None)?;
    let spec =
        holding.session_environment_adoption_spec(realization.peers(), thread, provider, None)?;
    let layout = provider
        .adoption()
        .effective_adoption_layout(&spec, &environment.lifecycle().handle())
        .map_err(crate::realization_error::RealizationError::from)?;
    Ok(layout.spec.env)
}

impl crate::RunCapabilities {
    /// Resolve the one exact inference plaintext holder used by both direct and
    /// dispatched attempts from the immutable candidate publication.
    pub(crate) fn inference_plaintext_holder(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<awaken_agent_contract::credential::PlaintextHolder>, SessionExecutionError>
    {
        awaken_dispatch_execution::session_dispatch::inference_plaintext_holder(activation, || {
            self.installed_sessions
                .realizations()
                .credential_realization(&activation.thread_id.0)
                .map_err(|error| {
                    awaken_dispatch_execution::AdmissionError::from(
                        awaken_session_types::RunError::from(error),
                    )
                })
        })
        .map_err(SessionExecutionError::from)
    }

    /// Root delegation and cold child recovery carry the same installed adapter
    /// set. Custody evidence is composed here, never narrowed by transport or
    /// inferred from the child's requested backend.
    pub(crate) fn child_execution_adapters(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        background: Arc<awaken_session_run_delivery::BackgroundRuns>,
        hosts_application: bool,
        environment_variables: impl FnOnce() -> Result<
            Option<Vec<awaken_provisioning_contract::EnvVar>>,
            crate::realization_error::RealizationError,
        >,
    ) -> Result<
        crate::agent_runner::ChildExecutionAdapters,
        crate::realization_error::RealizationError,
    > {
        let acp = match &self.exec.acp {
            crate::host::capability::AcpBackend::Installed(acp) => {
                Some((acp.clone(), environment_variables()?))
            }
            crate::host::capability::AcpBackend::Absent
            | crate::host::capability::AcpBackend::Projection(_) => None,
        }
        .map(|(acp, environment_variables)| {
            let process = environment.agent_process().clone();
            Arc::new(move |backend, permission| {
                acp.create(awaken_session_environment_contract::AcpSessionExecution {
                    environment: process.clone(),
                    environment_variables: environment_variables.clone(),
                    backend,
                    permission,
                    mcp_servers: Vec::new(),
                    attached_tools: None,
                    grant_observer: None,
                    continuation: None,
                })
            }) as crate::agent_runner::ChildAcpExecutorFactory
        });
        Ok(crate::agent_runner::ChildExecutionAdapters {
            session_thread_id: Some(awaken_agent_contract::ThreadId(thread.to_owned())),
            installed_sessions: Some(Arc::downgrade(&self.installed_sessions)),
            acp,
            remote: self.agent.remote_attempt_executor.clone(),
            credential_capabilities: self.local_credential_realization_capabilities(),
            web_search: Some(self.web_search_plugin(thread, None)?),
            web_fetch: Some(self.web_fetch_plugin(thread, None)?),
            clock: Some(self.clock.clone()),
            admin_tools: self.tools.admin_tools.clone(),
            background_supervisor: Some(self.background_supervisor.clone()),
            background_runs: Some(background),
            background_commit: None,
            background_attention: crate::background_task::TaskAttentionRoute::select(
                hosts_application,
                &self.session_application,
                self.worker.task_control.as_ref(),
            ),
            task_control: self.worker.task_control.clone(),
            background_generation: self
                .installed_sessions
                .realizations()
                .read_realization(thread, |realization| {
                    realization
                        .environment()
                        .owner
                        .resident_activity_identity(&environment)
                })?
                .flatten()
                .map(|(generation, _)| generation),
        })
    }

    /// Exact independent credential-adapter profiles installed in this process.
    /// Both the process dispatch pool and per-Session durable ingress consume this
    /// one declaration; neither may infer custody from only the Native adapter.
    pub(crate) fn local_credential_realization_capabilities(
        &self,
    ) -> CredentialRealizationCapabilities {
        let mut profiles = vec![
            self.agent
                .inference_routing
                .credential_realization_capabilities(),
        ];
        // Remote attempts are an independent adapter just like Native and ACP.
        // Preserve their profile as an alternative so claim admission can use
        // the installed A2A relay without synthesizing cross-adapter evidence.
        profiles.push(self.agent.remote_credential_realization.clone());
        if let Some(materializer) = &self.exec.credential_materializer {
            // Managed MCP executes through the same exact materializer installed
            // on the Host. Claim admission must therefore see its WorkerRelay
            // profile; inference/ACP profiles alone reject an otherwise
            // realizable frozen MCP credential and poison the local dispatch row.
            profiles.push(materializer.worker_relay_capabilities());
            // Repository credentials are consumed only by the Sandbox Git
            // helper protocol. This is a separate installed adapter profile,
            // not a widening of WorkerRelay custody.
            profiles.push(materializer.realization_capabilities(
                CredentialRealizationProfile::self_hosted_native().resource_holder,
                awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField,
            ));
            if let crate::host::capability::AcpBackend::Installed(acp) = &self.exec.acp {
                let policy = acp.adapter_policy();
                let configured = self
                    .exec
                    .acp_profile
                    .as_ref()
                    .map(|profile| profile.cli_ids().map(str::to_string).collect::<Vec<_>>())
                    .unwrap_or_else(|| {
                        policy.known_ids().into_iter().map(str::to_string).collect()
                    });
                profiles.extend(configured.into_iter().filter_map(|cli| {
                    acp_mcp_client_injection_capabilities(materializer, policy, &cli, true)
                }));
            }
        }
        if let (crate::host::capability::AcpBackend::Installed(acp), Some(profile)) =
            (&self.exec.acp, &self.exec.acp_profile)
        {
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
        CredentialRealizationCapabilities::alternatives(profiles)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_agent_contract::credential::{
        CredentialMaterialSource, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_credential_contract::{
        CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
        CredentialRealizationKind, ResolvedCredentialMaterial,
    };

    struct ControlReferenceResolver;

    #[async_trait::async_trait]
    impl CredentialMaterialResolver for ControlReferenceResolver {
        fn material_capabilities(
            &self,
        ) -> std::collections::BTreeSet<awaken_credential_contract::CredentialMaterialCapability>
        {
            let mut profiles = std::collections::BTreeSet::new();
            for holder in [
                awaken_agent_contract::credential::PlaintextHolder::new(
                    awaken_agent_contract::credential::PlaintextBoundary::Worker,
                    "awaken.worker",
                ),
                awaken_agent_contract::credential::PlaintextHolder::new(
                    awaken_agent_contract::credential::PlaintextBoundary::Workload,
                    "awaken.workload.acp",
                ),
            ] {
                profiles.insert(awaken_credential_contract::CredentialMaterialCapability {
                    holder,
                    source: CredentialMaterialSource::ControlPlaneReference,
                    envelope: false,
                });
            }
            profiles
        }

        async fn resolve_exact(
            &self,
            _request: CredentialMaterialRequest<'_>,
        ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
            Err(CredentialMaterialError::Unavailable)
        }
    }

    fn has_worker_relay(
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
    ) -> bool {
        std::iter::once(capabilities)
            .chain(capabilities.alternatives.iter())
            .any(|profile| {
                profile.holders.contains(&PlaintextHolder::new(
                    PlaintextBoundary::Worker,
                    awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                )) && profile
                    .material_sources
                    .contains(&CredentialMaterialSource::ControlPlaneReference)
                    && profile
                        .realization_kinds
                        .contains(&CredentialRealizationKind::WorkerRelay)
            })
    }

    fn has_acp_mcp_client_injection(
        capabilities: &awaken_credential_contract::CredentialRealizationCapabilities,
        backend_ref: &str,
    ) -> bool {
        let consumer = format!(
            "{}{backend_ref}",
            awaken_credential_contract::ACP_CREDENTIAL_CONSUMER_PREFIX
        );
        std::iter::once(capabilities)
            .chain(capabilities.alternatives.iter())
            .any(|profile| {
                profile.holders.contains(
                    &awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp()
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

    #[tokio::test]
    async fn child_adapters_preserve_installed_profiles_across_transport_topologies() {
        // Causes: C1 store-owning/database-less Worker role; C2 materializer installed;
        // C3 ACP role absent/projection/installed; C4 environment read succeeds/fails.
        // Effects: E1 exact Host profile preserved in
        // both fresh delegation and cold recovery (one constructor); E2 only
        // installed WorkerRelay / ACP process-field profiles are admitted.
        // Table: C1 is independent; !C2 -> neither; C2&&!C3 -> relay only;
        // C2&&C3=installed -> both. A remote-only subset loses C2/C3 and must fail.
        // R1: C3=absent/projection -> zero environment reads, independent of C4.
        // R2: C3=installed -> exactly one read; C4=success preserves the factory,
        // C4=failure returns the original typed error before a factory is built.
        // Root Worker execution is covered by credential_materialization_worker
        // P1/P6 and the Native/Claude real-service journeys, not a fake getter.
        use awaken_sandbox_local::LocalProvider;
        let temporary = tempfile::tempdir().unwrap();
        let sandbox = LocalProvider::new(temporary.path().join("child"))
            .create_sandbox(&crate::sandbox_layout::agent_run_sandbox_spec("profiles"))
            .await
            .unwrap();
        let environment = Arc::new(
            awaken_session_environment::SandboxEnvironment::workdir(sandbox)
                .into_session_environment(),
        );
        for database_less_worker in [false, true] {
            for material_access in [false, true] {
                for acp_role in ["absent", "projection", "installed"] {
                    for environment_read_succeeds in [false, true] {
                        let mut host = crate::SessionExecutorBuilder::ephemeral(
                            Arc::new(crate::host::MemoryHostModel),
                            "test-model",
                        )
                        .await;
                        if database_less_worker {
                            host = host.with_authority(crate::UpstreamAuthority::default());
                        }
                        if material_access {
                            host = host.with_credential_materializer(
                            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                                Arc::new(ControlReferenceResolver),
                             awaken_time::system_clock(),),
                        );
                        }
                        if acp_role == "projection" {
                            host = host.with_acp_projection_policy(
                                awaken_run_executor_acp::acp_adapter_policy(),
                            );
                        }
                        let acp_installed = acp_role == "installed";
                        if acp_installed {
                            let source =
                                Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
                                    awaken_run_executor_acp::AcpLaunch::custom(
                                        vec!["true".into()],
                                        vec![],
                                    ),
                                ));
                            host = host.with_acp(Arc::new(
                                awaken_run_executor_acp::AcpRunExecutor::new(source),
                            ));
                        }
                        let host = (host).build().expect("validated Host dependencies");
                        let environment_reads = std::cell::Cell::new(0);
                        let adapters = host.run_assembly.capabilities.child_execution_adapters(
                            "profiles",
                            environment.clone(),
                            host.resources.memory.background(),
                            host.run_assembly.authority.hosts_session_application(),
                            || {
                                environment_reads.set(environment_reads.get() + 1);
                                if environment_read_succeeds {
                                    Ok(None)
                                } else {
                                    Err(
                                        crate::realization_error::RealizationError::invalid_request(
                                            "environment read refused",
                                        ),
                                    )
                                }
                            },
                        );
                        assert_eq!(environment_reads.get(), usize::from(acp_installed));
                        if acp_installed && !environment_read_succeeds {
                            assert_eq!(
                                adapters.err().expect("R2 typed read failure").to_string(),
                                "environment read refused"
                            );
                            continue;
                        }
                        let adapters = adapters.expect("R1/R2 original adapters");
                        assert_eq!(
                            adapters.credential_capabilities,
                            host.run_assembly
                                .capabilities
                                .local_credential_realization_capabilities()
                        );
                        assert_eq!(
                            has_worker_relay(&adapters.credential_capabilities),
                            material_access
                        );
                        assert_eq!(
                            has_acp_mcp_client_injection(
                                &adapters.credential_capabilities,
                                "acp:claude"
                            ),
                            material_access && acp_installed
                        );
                        assert_eq!(adapters.acp.is_some(), acp_installed);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn local_capabilities_publish_only_installed_session_mcp_mechanisms() {
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
        let without = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::host::MemoryHostModel),
            "test-model",
        )
        .await)
            .build()
            .expect("validated Host dependencies");
        let launch = awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]);
        let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
            launch,
        ));
        let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
        let acp_without_materializer = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::host::MemoryHostModel),
            "test-model",
        )
        .await
        .with_acp(executor.clone()))
        .build()
        .expect("validated Host dependencies");
        assert!(
            !has_worker_relay(
                &without
                    .run_assembly
                    .capabilities
                    .local_credential_realization_capabilities()
            ),
            "R1: a host without a relay-capable adapter must fail closed"
        );
        assert!(
            !has_acp_mcp_client_injection(
                &acp_without_materializer
                    .run_assembly
                    .capabilities
                    .local_credential_realization_capabilities(),
                "acp:claude",
            ),
            "R1: an ACP executor without material access cannot advertise client injection"
        );

        let materializer =
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                Arc::new(ControlReferenceResolver),
                awaken_time::system_clock(),
            );
        let remote_profile = materializer.worker_relay_capabilities();
        let mut with_remote = crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::host::MemoryHostModel),
            "test-model",
        )
        .await;
        with_remote.agent.remote_credential_realization = remote_profile;
        let with_remote = (with_remote).build().expect("validated Host dependencies");
        assert!(
            has_worker_relay(
                &with_remote
                    .run_assembly
                    .capabilities
                    .local_credential_realization_capabilities()
            ),
            "R3: the installed Remote adapter must be visible to claim admission"
        );

        let with = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::host::MemoryHostModel),
            "test-model",
        )
        .await
        .with_credential_materializer(materializer.clone()))
        .build()
        .expect("validated Host dependencies");
        assert!(
            has_worker_relay(
                &with
                    .run_assembly
                    .capabilities
                    .local_credential_realization_capabilities()
            ),
            "R2: the installed exact materializer must be visible to claim admission"
        );
        assert!(
            !has_acp_mcp_client_injection(
                &with
                    .run_assembly
                    .capabilities
                    .local_credential_realization_capabilities(),
                "acp:claude",
            ),
            "R2: material access alone does not declare an ACP client"
        );

        let with_acp = (crate::SessionExecutorBuilder::ephemeral(
            Arc::new(crate::host::MemoryHostModel),
            "test-model",
        )
        .await
        .with_credential_materializer(materializer)
        .with_acp(executor))
        .build()
        .expect("validated Host dependencies");
        let capabilities = with_acp
            .run_assembly
            .capabilities
            .local_credential_realization_capabilities();
        assert!(
            has_worker_relay(&capabilities),
            "R3: legacy relay remains installed"
        );
        for backend in ["acp:claude", "acp:codex"] {
            assert!(
                has_acp_mcp_client_injection(&capabilities, backend),
                "R3: each witnessed declared client publishes its exact profile: {backend}"
            );
        }
        assert!(
            !has_acp_mcp_client_injection(&capabilities, "acp:opencode"),
            "R5: an undeclared client-injection adapter is not synthesized"
        );
    }
}
