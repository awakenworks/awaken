//! T16-C01 credential-bearing branches: Native Worker-held and ACP
//! WorkerRelay. The barrier is inside the exact credential resolution
//! (template: `ExactTargetResolver`, mcp_credentials.rs), which precedes
//! the MCP connect (Native) or the lazy Worker relay start (ACP).
//!
//! | branch             | barrier      | event (x3)              | expected                                   |
//! | Native Worker-held | resolve_exact| close/reinstall/expiry  | Staging owned at gate; one resolve; Err;   |
//! |                    |              |                         | zero initialize, nothing published         |
//! | ACP WorkerRelay    | resolve_exact| close/reinstall/expiry  | same; Worker relay never started           |

use super::*;
use awaken_session_realization::peers::EnvironmentPeer as _;

/// Exact-target credential resolver gated inside `resolve_exact`.
struct GatedResolver {
    holder: awaken_agent_contract::credential::PlaintextHolder,
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialMaterialResolver for GatedResolver {
    fn material_capabilities(
        &self,
    ) -> std::collections::BTreeSet<awaken_credential_contract::CredentialMaterialCapability> {
        [false, true]
            .into_iter()
            .map(
                |envelope| awaken_credential_contract::CredentialMaterialCapability {
                    holder: self.holder.clone(),
                    source:
                        awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
                    envelope,
                },
            )
            .collect()
    }

    async fn resolve_exact(
        &self,
        request: awaken_credential_contract::CredentialMaterialRequest<'_>,
    ) -> Result<
        awaken_credential_contract::ResolvedCredentialMaterial,
        awaken_credential_contract::CredentialMaterialError,
    > {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.entered.notify_one();
        self.release.acquire().await.expect("gate open").forget();
        Ok(awaken_credential_contract::ResolvedCredentialMaterial {
            credential: request.access.credential.clone(),
            holder: self.holder.clone(),
            material: awaken_credential_contract::CredentialMaterial::secret(
                awaken_agent_contract::RedactedString::new("mcp-bearer"),
            ),
        })
    }
}

fn worker_holder() -> awaken_agent_contract::credential::PlaintextHolder {
    awaken_agent_contract::credential::PlaintextHolder::new(
        awaken_agent_contract::credential::PlaintextBoundary::Worker,
        "awaken.worker",
    )
}

fn gated_resolver() -> Arc<GatedResolver> {
    Arc::new(GatedResolver {
        holder: worker_holder(),
        calls: std::sync::atomic::AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    })
}

fn materializer(
    resolver: &Arc<GatedResolver>,
) -> awaken_credential_materializer::PinnedCredentialMaterializer {
    awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
        resolver.clone(),
        awaken_time::system_clock(),
    )
}

fn worker_held_request(session: &str, url: &str) -> crate::session_mcp::StageMcpAttachment {
    use awaken_agent_contract::{
        credential::CredentialAccess, credential::CredentialExecutionPolicy,
        credential::CredentialMaterialSource, credential::CredentialRef,
        credential::CredentialUsage, credential::ModelExposurePolicy,
    };
    let holder = worker_holder();
    let target = awaken_agent_contract::McpTarget::parse_http(url).unwrap();
    let mut request = native_request(session, url);
    request.credential = Some(
        CredentialAccess::new(
            CredentialRef {
                id: "flow-mcp".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
        )
        .with_target(awaken_agent_contract::credential::CredentialTarget::new(
            awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
            awaken_agent_contract::McpTarget::identity(target.http_url().unwrap())
                .unwrap()
                .canonical_url(),
        )),
    );
    request.selected_plaintext_holder = Some(holder);
    request
}

/// Drive one credential-bearing branch through its gated resolution and
/// the event, asserting the shared T16-C01 oracles.
async fn credential_row(
    host: &ExecutorComponents,
    resolver: &GatedResolver,
    session: &str,
    url: &str,
    event: Event,
    row: &str,
) {
    host.run_assembly
        .install_session_realization_lease(session, lease())
        .unwrap();
    let request = worker_held_request(session, url);
    let generation = request.generation.clone();
    let assembly = host.run_assembly.clone();
    let pending = tokio::spawn(async move { assembly.stage_mcp_attachment(request).await });
    resolver.entered.notified().await;
    check_staged_before_io(host.run_assembly.mcp_projection(&generation).as_ref())
        .unwrap_or_else(|violation| panic!("{row}: {violation}"));
    fire(host, session, event);
    resolver.release.add_permits(1);
    let error = pending
        .await
        .unwrap()
        .expect_err("a resolution that outlived its window must not stage");
    assert_eq!(
        resolver.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "{row}: exactly one credential resolution"
    );
    assert_eq!(error.code, event.expected_code(), "{row}: {error:?}");
    assert_nothing_published(host, session, &generation, row);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t16_c01_native_worker_held_resolution_is_fenced_before_connect() {
    for event in EVENTS {
        let row = format!("native-worker-held/{event:?}");
        let session = format!("mcp-window-worker-{event:?}").to_lowercase();
        let (url, seen) = crate::test_mcp::start(Some("Bearer mcp-bearer")).await;
        let resolver = gated_resolver();
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_credential_materializer(materializer(&resolver)))
        .build()
        .expect("validated Host dependencies");
        credential_row(&host, &resolver, &session, &url, event, &row).await;
        assert_eq!(
            initialize_count(&seen),
            0,
            "{row}: the post-wait fence denies the MCP connect"
        );
    }
}

/// Container provider advertising no-bypass secret egress; it admits only
/// pre-environment MCP staging (ported from mcp_credentials.rs H20).
struct SecureExternalProvider;

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxReadiness for SecureExternalProvider {
    fn capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
        managed_test_container_capabilities()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxProvider for SecureExternalProvider {
    type Environment = dyn awaken_provisioning_contract::SessionSandbox;

    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_provisioning_contract::SandboxAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    async fn prewarm_images(
        &self,
        _specs: &[awaken_provisioning_contract::SandboxSpec],
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    fn validate_closed_observation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
        observation: &awaken_provisioning_contract::SandboxObservation,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::validate_container_closed_observation(handle, observation)
    }

    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a awaken_provisioning_contract::SandboxSpec,
        effect: awaken_provisioning_contract::SandboxEffect<'a>,
        intent: awaken_provisioning_contract::SandboxRealizationIntent,
    ) -> awaken_provisioning_contract::ClaimedEffect<
        'a,
        Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        let claim = awaken_provisioning_contract::EffectClaim::in_process(
            awaken_provisioning_contract::test_support::retain_abandoned_effect(spec.scope.clone()),
        );
        awaken_provisioning_contract::ClaimedEffect::new(claim, async move {
            intent.admit_under(effect)?;
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "T16-C01 exercises pre-environment MCP staging only",
            ))
        })
    }

    async fn observe(
        &self,
        _adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not observe Environments",
        ))
    }

    async fn adopt_for_effect(
        &self,
        _adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        _effect: awaken_provisioning_contract::SandboxEffect<'_>,
    ) -> Result<
        Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "T16-C01 exercises pre-environment MCP staging only",
        ))
    }

    async fn prepare_terminal_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<Arc<dyn awaken_provisioning_contract::SessionSandbox>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    fn restoration(
        &self,
    ) -> awaken_provisioning_contract::SandboxRestoration<
        '_,
        Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        awaken_provisioning_contract::SandboxRestoration::Unsupported
    }
}

impl awaken_provisioning_contract::SandboxInstallation for SecureExternalProvider {
    fn install_memory_mounter(
        &self,
        _mounter: Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) {
    }

    fn install_secret_broker(&self, _broker: Arc<dyn awaken_provisioning_contract::SecretBroker>) {}
}

struct UnusedHandFactory;

impl awaken_session_environment_contract::HandExecutorFactory for UnusedHandFactory {
    fn bind(
        &self,
        _channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
    ) -> awaken_session_environment_contract::HandExecutorBinding {
        panic!("T16-C01 does not launch an Agent process")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t16_c01_acp_worker_relay_resolution_is_fenced_before_relay_start() {
    for event in EVENTS {
        let row = format!("acp-worker-relay/{event:?}");
        let session = format!("mcp-window-acp-{event:?}").to_lowercase();
        let (url, seen) = crate::test_mcp::start(Some("Bearer mcp-bearer")).await;
        let resolver = gated_resolver();
        let launch = awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]);
        let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(Arc::new(
            awaken_run_executor_acp::SubprocessChannelSource::new(launch),
        )));
        let host = (crate::SessionExecutorBuilder::ephemeral(Arc::new(OkModel), "stub")
            .await
            .with_acp(executor)
            .with_session_container_provider(
                Arc::new(SecureExternalProvider),
                Arc::new(UnusedHandFactory),
            )
            .with_credential_materializer(materializer(&resolver)))
        .build()
        .expect("validated Host dependencies");
        let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
            .resolved_model(
                awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    awaken_runtime_contract::resolved::ModelBinding::parse(
                        "provider", "model", "acp:test",
                    )
                    .expect("valid binding"),
                ),
            )
            .build();
        host.run_assembly
            .capabilities
            .register_thread_backend_projection(&session, "acp:test");
        host.run_assembly
            .capabilities
            .register_thread_agent_projection(&session, "assistant")
            .expect("installed Sessions");
        host.realization_executor
            .environments()
            .publication()
            .retain_session_publication(&session, Some(&snapshot))
            .expect("exact HostExecutor publication");
        assert!(
            host.run_assembly.mcp.relay().is_none(),
            "{row}: relay is lazy"
        );
        credential_row(&host, &resolver, &session, &url, event, &row).await;
        assert!(
            host.run_assembly.mcp.relay().is_none(),
            "{row}: the post-wait fence denies starting the Worker relay"
        );
        assert_eq!(initialize_count(&seen), 0, "{row}: no upstream contact");
    }
}
