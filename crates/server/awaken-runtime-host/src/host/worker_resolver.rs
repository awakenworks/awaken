//! [`HostWorkerResolver`]: routes a claimed dispatch to the worker that owns
//! its thread, opening (or reusing) the session through the host.

use super::*;

struct WorkerProjectionSynchronizer<'a> {
    host: &'a SharedHost,
    claim: Option<&'a awaken_run_ingress::RunClaim>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionProjectionSynchronizer for WorkerProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_session_contract::FrozenSessionProjection,
        lease: &awaken_session_contract::SessionRealizationLease,
        _prepare_session: bool,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.host
            .install_frozen_session_projection(session_id, projection.clone(), self.claim)
            .await
            .map_err(|error| awaken_session_contract::RunError::internal(error.to_string()))?;
        self.host
            .install_session_realization_lease(session_id, lease.clone());
        Ok(())
    }
}

struct WorkerMcpEffects<'a>(&'a SharedHost);

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for WorkerMcpEffects<'_> {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
    {
        self.0.stage_dispatched_mcp(request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.publish_dispatched_mcp(generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.0.drain_dispatched_mcp(generation).await
    }
}

async fn adopt_bound_sandbox(
    host: &SharedHost,
    encoded: Option<&str>,
    expected_sandbox_id: &str,
    run_id: &RunId,
    recovery: awaken_run_ingress::WorkerRecoveryMode,
) -> Result<(Option<crate::session_environment::SessionEnvironment>, bool), awaken_run_ingress::Error>
{
    host.adopt_bound_session_environment(
        expected_sandbox_id,
        encoded,
        recovery == awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
    )
    .await
    .map_err(|error| HostWorkerResolver::execution_error(format!("run {}: {error}", run_id.0)))
}

/// Routes a claimed run to the worker that owns its thread, opening (or reusing)
/// the session through the host. Holds a `Weak` back-reference so the pool's tasks
/// never keep the host alive; if the host is dropped, `worker_for` fails and the
/// pool's drains idle out.
pub(crate) struct HostWorkerResolver {
    pub(crate) host: std::sync::Weak<SharedHost>,
}

impl HostWorkerResolver {
    fn execution_error(message: impl Into<String>) -> awaken_run_ingress::Error {
        awaken_run_ingress::Error::Execution(awaken_runtime_contract::execution::Error::Execution(
            message.into(),
        ))
    }

    fn host(&self) -> Result<Arc<SharedHost>, awaken_run_ingress::Error> {
        self.host
            .upgrade()
            .ok_or_else(|| Self::execution_error("host dropped; pool idling"))
    }

    pub(crate) async fn realize_application_session(
        host: &SharedHost,
        control: &Arc<dyn crate::ApplicationSessionControlClient>,
        session_id: &str,
        directive: awaken_session_contract::SessionRealizationDirective,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), awaken_run_ingress::Error> {
        awaken_session_contract::drive_session_realization(
            session_id,
            control.as_ref(),
            &WorkerProjectionSynchronizer { host, claim },
            &WorkerMcpEffects(host),
            directive,
        )
        .await
        .map_err(|error| Self::execution_error(error.to_string()))
    }

    async fn resolve(
        &self,
        host: &SharedHost,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
        sandbox: Option<crate::session_environment::SessionEnvironment>,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let agent = agent_id.filter(|a| !a.is_empty());
        let ctx = host
            .ctx_for_snapshot_with_sandbox(&thread_id.0, agent, published_snapshot, sandbox)
            .await
            .map_err(|e| Self::execution_error(e.to_string()))?;
        ctx.durable_ingress
            .as_ref()
            .map(|ingress| ingress.worker_handle())
            .ok_or_else(|| {
                Self::execution_error(format!("thread {} has no durable ingress", thread_id.0))
            })
    }

    /// Resolve the terminal-control path without creating, adopting, or probing a
    /// Session environment. Cancellation only needs the dispatch fence and the
    /// thread commit boundary; making it depend on the run's model, credentials,
    /// placement capabilities, or sandbox would let the failed dependency prevent
    /// its own termination.
    async fn cancellation_worker(
        &self,
        host: &SharedHost,
        claimed: &awaken_run_ingress::Claimed,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let thread_id = claimed.request.session_thread_id();
        let commit = Arc::new(
            host.build_commit(&thread_id.0)
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?,
        );
        let recovery_projection = commit.recovery_projection();
        let store = host
            .dispatch_store()
            .map_err(|error| Self::execution_error(error.to_string()))?;
        let mut worker = awaken_run_ingress::DispatchWorker::new(
            Arc::new(awaken_runtime::Runtime::new()),
            store,
            commit.clone(),
            claimed.lease.owner.clone(),
        );
        if host.upstream.is_none()
            && let Some(observer) = host
                .memory_terminal_observer(
                    &thread_id.0,
                    &claimed.request.activation.snapshot,
                    commit,
                )
                .await
        {
            worker = worker.with_context(
                awaken_runtime_contract::RuntimeRunContext::new().with_terminal_observer(observer),
            );
        }
        if matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(
                &claimed
                    .request
                    .activation
                    .snapshot
                    .resolved_spec
                    .model_binding
                    .backend_ref
            ),
            awaken_runtime_contract::resolved::Backend::Remote { .. }
        ) {
            let executor = host.remote_attempt_executor.clone().ok_or_else(|| {
                Self::execution_error(
                    "remote cancellation requires a configured remote attempt executor",
                )
            })?;
            worker.install_attempt_executor(executor);
        }
        if let Some(upstream) = &host.upstream {
            let claimed_commit = crate::commit_ingest::remote_claimed_commit(upstream)
                .map_err(|error| Self::execution_error(error.to_string()))?;
            worker = worker.with_claimed_commit(claimed_commit);
        }
        if let Some(projection) = recovery_projection {
            worker = worker.with_recovery_projection(projection);
        }
        Ok(Arc::new(worker))
    }
}

impl SharedHost {
    /// Exact independent credential-adapter profiles installed in this process.
    /// Both the process dispatch pool and per-Session durable ingress consume this
    /// one declaration; neither may infer custody from only the Native adapter.
    pub(crate) fn local_credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let mut profiles = vec![self.inference_routing.credential_realization_capabilities()];
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

#[async_trait::async_trait]
impl WorkerResolver<AnyDispatchStore> for HostWorkerResolver {
    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        self.host
            .upgrade()
            .map(|host| host.local_credential_realization_capabilities())
            .unwrap_or_default()
    }

    async fn worker_for(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let host = self.host()?;
        // Open the session bound to the claimed run's OWN agent, so `ctx_for` resolves
        // that agent's published config from the worker's config service — its own
        // catalog and model binding. A cold worker thus runs the configured model
        // against a matching fingerprint, with no session-level model registry. An
        // already-resident session is returned from the cache; a cold thread rebuilds
        // from committed truth. `None`/empty opens the built-in default agent.
        self.resolve(&host, thread_id, agent_id, None, None).await
    }

    async fn worker_for_claimed(
        &self,
        claimed: &awaken_run_ingress::Claimed,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let host = self.host()?;
        if claimed.cancellation_requested {
            return self.cancellation_worker(&host, claimed).await;
        }
        let thread_id = claimed.request.session_thread_id();
        let agent_id = claimed.request.activation.snapshot.root_agent_id.0.as_str();
        let agent_id = (!agent_id.is_empty()).then_some(agent_id);

        let dispatched_resources = if let Some(envelope) = &claimed.request.session_resources {
            let dispatched_scope = claimed
                .request
                .execution_scope
                .as_ref()
                .map(|scope| scope.0.0.as_str());
            if envelope.workspace_id.trim().is_empty()
                || dispatched_scope != Some(envelope.workspace_id.as_str())
            {
                return Err(Self::execution_error(format!(
                    "run {} has a resource manifest outside its execution scope",
                    claimed.lease.run_id.0
                )));
            }
            let manifest = crate::provisioning::decode_session_resource_envelope(envelope)
                .map_err(|error| {
                    Self::execution_error(format!(
                        "run {} has an invalid Session resource manifest: {error}",
                        claimed.lease.run_id.0
                    ))
                })?;
            Some(manifest)
        } else {
            None
        };

        // Install the immutable Session runtime projection before resource
        // staging or context construction. Without this envelope a cold Worker
        // cannot distinguish eager from on-tool-use provisioning and would
        // eagerly allocate an unbound Sandbox for a Brain-only run.
        if let Some(envelope) = &claimed.request.session_runtime {
            let (environment, toolsets) = crate::provisioning::decode_session_runtime_envelope(
                envelope,
            )
            .map_err(|error| {
                Self::execution_error(format!(
                    "run {} has an invalid Session runtime projection: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
            host.install_environment_projection(&thread_id.0, &environment)
                .map_err(|error| Self::execution_error(error.to_string()))?;
            host.session_slots
                .update(&thread_id.0, |slot| slot.toolsets = toolsets);
        }

        if let Some(provisioner) = &host.application_session_provisioner {
            let dispatch: Arc<dyn awaken_run_ingress::DispatchQueue> = host
                .dispatch_store()
                .map_err(|error| Self::execution_error(error.to_string()))?;
            let ownership = awaken_run_ingress::claim_bound_ownership_verifier(
                dispatch,
                awaken_run_ingress::RunClaim::from(&claimed.lease),
                Arc::new(awaken_run_ingress::SystemClock),
            );
            ownership.verify_current().await.map_err(|error| {
                Self::execution_error(format!(
                    "run {} lost ownership before application provisioning: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
            let contribution = provisioner
                .prepare(&claimed.request.activation, &thread_id.0, ownership.clone())
                .await
                .map_err(|error| {
                    Self::execution_error(format!(
                        "run {} application provisioning failed: {error}",
                        claimed.lease.run_id.0
                    ))
                })?;
            ownership.verify_current().await.map_err(|error| {
                Self::execution_error(format!(
                    "run {} lost ownership during application provisioning: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
            let control = host.application_session_control.as_ref().ok_or_else(|| {
                Self::execution_error(
                    "application Session provisioner has no Control contribution client",
                )
            })?;
            let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
            let receipt = control
                .contribute(&claim, contribution)
                .await
                .map_err(|error| {
                    Self::execution_error(format!(
                        "run {} application contribution failed: {error}",
                        claimed.lease.run_id.0
                    ))
                })?;
            ownership.verify_current().await.map_err(|error| {
                Self::execution_error(format!(
                    "run {} lost ownership during application contribution: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
            if let Some(dispatched) = &dispatched_resources
                && (dispatched.workspace_id != receipt.contribution.projection.workspace_id
                    || dispatched.resources != receipt.contribution.projection.resources)
            {
                return Err(Self::execution_error(
                    "Control contribution projection conflicts with the claimed resource snapshot",
                ));
            }
            Self::realize_application_session(
                &host,
                control,
                &thread_id.0,
                receipt.realization,
                Some(&claim),
            )
            .await?;
        } else if let Some(manifest) = &dispatched_resources {
            let claim = awaken_run_ingress::RunClaim::from(&claimed.lease);
            host.install_dispatched_resources(&thread_id.0, manifest, Some(&claim))
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?;
        }

        // A cold durable Worker installs the frozen projection directly rather
        // than crossing ManagedHost::prepare_session. Reconstruct the lazy Hand
        // executor from that immutable projection before SessionCtx is built.
        let needs_deferred_executor = host
            .session_slots
            .read(&thread_id.0, |slot| {
                slot.deferred_executor.is_none()
                    && slot
                        .environment_projection
                        .as_ref()
                        .is_some_and(|environment| {
                            environment.provisioning
                                == awaken_session_contract::SandboxProvisioning::OnToolUse
                        })
            })
            .unwrap_or(false);
        if needs_deferred_executor {
            let executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor> =
                Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
                    Arc::downgrade(&host),
                    &thread_id.0,
                ));
            host.session_slots
                .update(&thread_id.0, |slot| slot.deferred_executor = Some(executor));
        }

        let (adopted, rebuild_binding) = adopt_bound_sandbox(
            &host,
            claimed.sandbox.as_deref(),
            &thread_id.0,
            &claimed.lease.run_id,
            claimed.request.placement.recovery,
        )
        .await?;

        // A deferred Native Session carries the current lease fence into its
        // first Sandbox-target tool. Brain-only runs never create or bind one.
        host.session_slots.update(&thread_id.0, |slot| {
            if slot.deferred_executor.is_some() {
                slot.deferred_claim = Some(awaken_run_ingress::RunClaim::from(&claimed.lease));
            }
        });

        let worker = self
            .resolve(
                &host,
                thread_id,
                agent_id,
                Some(claimed.request.activation.snapshot.clone()),
                adopted,
            )
            .await?;

        // Persist the first placement before executing the claimed run. If the
        // process dies after this write, the next owner sees the handle and adopts
        // the same environment; a failed write leaves the run unexecuted/retryable.
        if claimed.sandbox.is_none() || rebuild_binding {
            let Some(environment) = host.session_environment(&thread_id.0).await else {
                // `on_tool_use`: the deferred executor will bind this exact
                // claim before publishing its first Sandbox. Returning the
                // worker here lets Brain tools execute without a Sandbox.
                return Ok(worker);
            };
            let encoded = serde_json::to_string(&environment.handle())
                .map_err(|e| Self::execution_error(e.to_string()))?;
            let outcome = host
                .dispatch_store()
                .map_err(|e| Self::execution_error(e.to_string()))?
                .bind_sandbox(
                    &awaken_run_ingress::RunClaim::from(&claimed.lease),
                    &encoded,
                )
                .await
                .map_err(awaken_run_ingress::Error::from)?;
            if !outcome.applied() {
                return Err(Self::execution_error(
                    "sandbox binding was fenced by a replacement claim",
                ));
            }
        }
        Ok(worker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::{
        AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct AdoptionModel;

    #[async_trait::async_trait]
    impl LlmExecutor for AdoptionModel {
        async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    fn test_activation(thread: &str, run: &str) -> RunActivation {
        let fingerprint = CatalogFingerprint(format!("catalog-{run}"));
        RunActivation::new(
            RunId(run.to_string()),
            ThreadId(thread.to_string()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId(format!("snapshot-{run}")),
                metadata: Default::default(),
                root_agent_id: AgentId("agent-a".to_string()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: fingerprint.clone(),
                    instructions: "test".to_string(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("provider", "model", "backend"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint,
            },
            Vec::new(),
        )
    }

    fn deferred_environment() -> awaken_session_contract::EnvironmentSnapshot {
        awaken_session_contract::EnvironmentSnapshot {
            environment_id: "lazy-env".into(),
            revision: awaken_session_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                "lazy-env-v1".into(),
            ),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: awaken_session_contract::SandboxProvisioning::OnToolUse,
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    async fn prepare_deferred_session(host: Arc<SharedHost>, thread: &str) -> crate::ManagedHost {
        use awaken_session_contract::SessionRuntime;
        let managed = crate::ManagedHost::new(host.clone());
        managed
            .prepare_session(
                thread,
                awaken_session_contract::SessionInit {
                    workspace_id: host.local_workspace().into(),
                    agent_id: "agent-a".into(),
                    delegate_ids: Vec::new(),
                    toolsets: None,
                    resource_revision: 0,
                    resources: Default::default(),
                    model: None,
                    runtime: None,
                    environment: deferred_environment(),
                },
            )
            .await
            .expect("prepare deferred Session");
        managed
    }

    struct ToggleBindingSink {
        fail: std::sync::atomic::AtomicBool,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionEnvironmentBindingSink for ToggleBindingSink {
        async fn persist(
            &self,
            _session_id: &str,
            _binding: &str,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err(awaken_session_contract::RunError::internal(
                    "injected Session binding failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    async fn claim(
        store: &awaken_run_ingress::AnyDispatchStore,
        thread: &str,
        run: &str,
        owner: &str,
        now: u64,
    ) -> awaken_run_ingress::Claimed {
        use awaken_run_ingress::DispatchQueue;
        store
            .enqueue(awaken_run_ingress::RunDispatch::new(test_activation(
                thread, run,
            )))
            .await
            .expect("enqueue deferred run");
        store
            .claim(owner, 1_000, now, &Default::default())
            .await
            .expect("claim deferred run")
            .expect("deferred run available")
    }

    /// C1-C3: a cold Worker must derive eager-vs-deferred provisioning only from
    /// the immutable dispatch envelope. Legacy absence remains eager, an exact
    /// on-tool-use projection stays sandbox-free during Brain resolution, and a
    /// malformed projection fails before any Sandbox can be created.
    #[tokio::test]
    async fn cold_worker_runtime_projection_decision_table() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let _managed = crate::ManagedHost::new(host.clone());
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let legacy = claim(&store, "cold-legacy", "run-legacy", "worker-a", now).await;
        resolver
            .worker_for_claimed(&legacy)
            .await
            .expect("C1 legacy projection remains eager");
        assert!(
            host.session_environment("cold-legacy").await.is_some(),
            "C1"
        );

        let runtime = crate::provisioning::encode_session_runtime_envelope(
            deferred_environment(),
            Some(Vec::new()),
        )
        .expect("encode runtime projection");
        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "cold-deferred",
                    "run-deferred",
                ))
                .with_session_runtime(runtime),
            )
            .await
            .expect("enqueue deferred projection");
        let deferred = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim deferred projection")
            .expect("deferred projection available");
        resolver
            .worker_for_claimed(&deferred)
            .await
            .expect("C2 cold Brain resolution stays deferred");
        assert!(
            host.session_environment("cold-deferred").await.is_none(),
            "C2"
        );
        assert!(
            host.session_slots
                .read("cold-deferred", |slot| slot.deferred_executor.is_some()
                    && slot.toolsets.as_ref().is_some_and(Vec::is_empty))
                .unwrap_or(false),
            "C2"
        );

        store
            .enqueue(
                awaken_run_ingress::RunDispatch::new(test_activation(
                    "cold-invalid",
                    "run-invalid",
                ))
                .with_session_runtime(awaken_run_ingress::SessionRuntimeEnvelope::new("{")),
            )
            .await
            .expect("enqueue invalid projection");
        let invalid = store
            .claim("worker-a", 1_000, now, &Default::default())
            .await
            .expect("claim invalid projection")
            .expect("invalid projection available");
        let error = match resolver.worker_for_claimed(&invalid).await {
            Ok(_) => panic!("C3 malformed runtime projection must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid Session runtime projection")
        );
        assert!(
            host.session_environment("cold-invalid").await.is_none(),
            "C3"
        );
    }

    /// D1-D5: durable lazy placement is fenced by the current dispatch claim.
    /// Brain resolution stays sandbox-free; a replacement claim rejects stale
    /// publication; and a crash gap after dispatch binding is repaired by adoption.
    #[tokio::test]
    async fn durable_deferred_sandbox_publication_decision_table() {
        use awaken_run_ingress::{Clock, DispatchQueue};
        use awaken_session_contract::SessionRuntime;

        let now = awaken_run_ingress::SystemClock.now_ms();
        let store = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
        );
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_dispatch_store(store.clone()),
        );
        let sink = Arc::new(ToggleBindingSink {
            fail: std::sync::atomic::AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        });
        let managed = prepare_deferred_session(host.clone(), "durable-lazy").await;
        managed.install_environment_binding_sink(sink.clone());
        let claimed = claim(&store, "durable-lazy", "run-lazy", "worker-a", now).await;
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };
        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("D1 resolves Brain worker without Sandbox");
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D1"
        );

        let replacement = store
            .claim("worker-b", 1_000, now + 2_000, &Default::default())
            .await
            .expect("replacement claim")
            .expect("expired claim is recoverable");
        let deferred = host
            .session_slots
            .read("durable-lazy", |slot| slot.deferred_executor.clone())
            .flatten()
            .expect("deferred executor");
        let error = deferred
            .invoke(&awaken_runtime_contract::tool::ToolCall {
                call_id: "stale-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing"}),
            })
            .await
            .expect_err("D3 stale claim is fenced");
        assert!(
            error.to_string().contains("replacement claim"),
            "D3: {error}"
        );
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D3"
        );

        resolver
            .worker_for_claimed(&replacement)
            .await
            .expect("install replacement claim");
        sink.fail.store(true, Ordering::SeqCst);
        let deferred = host
            .session_slots
            .read("durable-lazy", |slot| slot.deferred_executor.clone())
            .flatten()
            .expect("replacement deferred executor");
        let error = deferred
            .invoke(&awaken_runtime_contract::tool::ToolCall {
                call_id: "crash-gap-read".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing"}),
            })
            .await
            .expect_err("D4 Session binding failure");
        assert!(
            error
                .to_string()
                .contains("injected Session binding failure")
        );
        assert!(
            host.session_environment("durable-lazy").await.is_none(),
            "D4"
        );

        let adopted = store
            .claim("worker-c", 1_000, now + 4_000, &Default::default())
            .await
            .expect("adoption claim")
            .expect("dispatch-bound run is recoverable");
        assert!(adopted.sandbox.is_some(), "D4 dispatch binding survived");
        sink.fail.store(false, Ordering::SeqCst);
        resolver
            .worker_for_claimed(&adopted)
            .await
            .expect("D5 adopts and repairs Session binding");
        assert!(
            host.session_environment("durable-lazy").await.is_some(),
            "D5"
        );
        assert_eq!(
            sink.calls.load(Ordering::SeqCst),
            2,
            "D4 failure + D5 repair"
        );
    }

    struct CountingProvisioner {
        calls: Arc<AtomicUsize>,
    }

    struct RecordingContributor {
        calls: Arc<AtomicUsize>,
        phases: Arc<std::sync::Mutex<Vec<&'static str>>>,
        projection: Arc<std::sync::Mutex<Option<awaken_session_contract::FrozenSessionProjection>>>,
        mcp_stage: Option<awaken_session_contract::StageMcpAttachment>,
        fail_begin: Arc<AtomicBool>,
    }

    #[derive(Default)]
    struct RecordingMcpRealizer {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_stage: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::McpAttachmentRealizer for RecordingMcpRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_session_contract::StageMcpAttachment,
        ) -> Result<awaken_session_contract::McpRealizationReceipt, awaken_session_contract::RunError>
        {
            self.calls.lock().unwrap().push("stage");
            if self.fail_stage.load(Ordering::SeqCst) {
                return Err(awaken_session_contract::RunError::classified(
                    "test_mcp_stage_failed",
                    "test MCP stage failed",
                ));
            }
            Ok(awaken_session_contract::McpRealizationReceipt {
                receipt_fingerprint: request.fingerprint(),
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: awaken_session_contract::McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::ApplicationSessionControlClient for RecordingContributor {
        async fn contribute(
            &self,
            _claim: &awaken_run_ingress::RunClaim,
            contribution: awaken_session_contract::ApplicationSessionContribution,
        ) -> Result<crate::ApplicationSessionControlReceipt, crate::ApplicationSessionError>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.phases.lock().unwrap().push("contribute");
            let holder = awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                "test.worker",
            );
            let input = contribution.input;
            let baseline = awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment: awaken_session_contract::EnvironmentSnapshot {
                        environment_id: "env".into(),
                        revision: awaken_session_contract::EnvironmentRevision(1),
                        self_hosted: false,
                        config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                            "env-fingerprint".into(),
                        ),
                        sandbox: serde_json::json!({}),
                        sandbox_provisioning: Default::default(),
                        packages: Default::default(),
                        prepared_image: None,
                        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                        credential_realization:
                            awaken_runtime_contract::CredentialRealizationProfile {
                                inference_holder: holder.clone(),
                                mcp_holder: holder.clone(),
                                resource_holder: holder,
                            },
                    },
                    mcp_authoring: Default::default(),
                    // The Control projection and the claimed publication name
                    // the same immutable Agent. A different id is a projection
                    // conflict and is covered by the Session projection tests.
                    agent_id: "agent-a".into(),
                    model: "model".into(),
                    runtime: None,
                    application: Some(
                        awaken_session_contract::ApplicationContributionReceipt::from_input(
                            contribution.application_fingerprint,
                            &input,
                        ),
                    ),
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: input.mounts,
                    env: input.env,
                    prompts: input.prompts,
                },
            );
            let mcp = self.mcp_stage.as_ref().map_or_else(Vec::new, |stage| {
                vec![
                    serde_json::from_value(serde_json::json!({
                        "attachment_id": stage.generation.attachment_id,
                        "name": stage.name,
                        "generation": stage.generation.generation,
                        "target": stage.target,
                        "origin": "application",
                        "credential": stage.credential,
                        "selected_plaintext_holder": stage.selected_plaintext_holder,
                        "state": "realizing",
                        "publication_acknowledged": false,
                        "realization": null,
                        "attempts": 1,
                        "last_error": null
                    }))
                    .expect("test MCP projection"),
                ]
            });
            let projection = awaken_session_contract::FrozenSessionProjection {
                workspace_id: "workspace".into(),
                revision: awaken_session_contract::SessionRevision(2),
                baseline,
                resource_revision: 0,
                resources: Default::default(),
                mcp,
                toolsets: Vec::new(),
            };
            *self.projection.lock().unwrap() = Some(projection.clone());
            let lease = awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a".into(),
                epoch: 1,
                expires_at_unix_ms: self
                    .mcp_stage
                    .as_ref()
                    .map_or(u64::MAX, |stage| stage.generation.lease_expires_at_unix_ms),
            };
            Ok(crate::ApplicationSessionControlReceipt {
                contribution: awaken_session_contract::ApplicationSessionContributionReceipt {
                    outcome: awaken_session_contract::ApplicationContributionOutcome::Committed,
                    projection: projection.clone(),
                },
                realization: awaken_session_contract::SessionRealizationDirective {
                    projection,
                    lease,
                    action: awaken_session_contract::SessionRealizationAction::Stage {
                        prepare_session: true,
                        mcp_stages: self.mcp_stage.clone().into_iter().collect(),
                    },
                },
            })
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::SessionRealizationControl for RecordingContributor {
        async fn begin_session_realization(
            &self,
            command: awaken_session_contract::BeginSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("begin");
            if self.fail_begin.load(Ordering::SeqCst) {
                return Err(awaken_session_contract::SessionRealizationControlFailure::NotReady);
            }
            let projection = self
                .projection
                .lock()
                .unwrap()
                .clone()
                .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
            let mut stage = self
                .mcp_stage
                .clone()
                .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
            stage.generation.lease_expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
            stage.stage_idempotency_key =
                format!("renew:{}", command.target.lease_expires_at_unix_ms);
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection,
                lease: awaken_session_contract::SessionRealizationLease {
                    owner: command.target.owner,
                    runtime_incarnation: command.target.runtime_incarnation,
                    epoch: 1,
                    expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                },
                action: awaken_session_contract::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            })
        }

        async fn activate_session_realization(
            &self,
            command: awaken_session_contract::ActivateSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("activate");
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("contribution projection"),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Publish {
                    publish: command
                        .mcp_receipts
                        .into_iter()
                        .map(|receipt| receipt.generation)
                        .collect(),
                    drain: Vec::new(),
                },
            })
        }

        async fn acknowledge_session_realization(
            &self,
            command: awaken_session_contract::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_session_contract::SessionRealizationDirective,
            awaken_session_contract::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("acknowledge");
            Ok(awaken_session_contract::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("contribution projection"),
                lease: command.lease,
                action: awaken_session_contract::SessionRealizationAction::Complete,
            })
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_session_contract::FailSessionRealization,
        ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
            self.phases.lock().unwrap().push("fail");
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::ApplicationSessionProvisioner for CountingProvisioner {
        async fn prepare(
            &self,
            activation: &RunActivation,
            session_id: &str,
            ownership: Arc<dyn awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier>,
        ) -> Result<
            awaken_session_contract::ApplicationSessionContribution,
            crate::ApplicationSessionError,
        > {
            ownership
                .verify_current()
                .await
                .map_err(|error| crate::ApplicationSessionError::new(error.to_string()))?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(awaken_session_contract::ApplicationSessionContribution {
                session_id: session_id.to_owned(),
                application_fingerprint: format!(
                    "application:{}",
                    activation.snapshot.fingerprint.0
                ),
                input: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn claimed_application_contribution_is_acknowledged_before_session_realization() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        // Cause graph: live exact claim -> provisioner succeeds -> Control client
        // exists -> contribution receipt carries a frozen projection -> install it
        // before Session environment realization. A missing client must fail closed;
        // the stale-claim table row is generated by the adjacent test.
        //
        // | Rule | Claim live | Provisioner | Contributor | Effect |
        // |---|---|---|---|---|
        // | W1 | T | success | installed, exact Agent | receipt then environment |
        // | W2 | T | success | missing | reject before environment |
        // | W3 | F | - | any | reject before provisioner |
        // | W4 | T | success + initial MCP | installed | stage/activate/publish/ack |
        // | W5 | T | initial MCP stage fails | installed | fail; no publish/environment |
        // | W6 | T | active MCP lease due | installed | same canonical driver renews generation |
        // | W7 | T | renewal loses Session authority | installed | revoke only that Session; Worker remains healthy |
        for (rule, install_contributor, with_initial_mcp, fail_mcp_stage) in [
            ("W1", true, false, false),
            ("W2", false, false, false),
            ("W4", true, true, false),
            ("W5", true, true, true),
        ] {
            let storage = tempfile::tempdir().expect("storage");
            let dispatch = Arc::new(
                awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                    .expect("in-memory dispatch"),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let contribution_calls = Arc::new(AtomicUsize::new(0));
            let phases = Arc::new(std::sync::Mutex::new(Vec::new()));
            let projection = Arc::new(std::sync::Mutex::new(None));
            let fail_begin = Arc::new(AtomicBool::new(false));
            let host = SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_store_dir(storage.path())
                .with_dispatch_store(dispatch.clone())
                .with_application_session_provisioner(Arc::new(CountingProvisioner {
                    calls: calls.clone(),
                }));
            let mcp_realizer = Arc::new(RecordingMcpRealizer::default());
            mcp_realizer
                .fail_stage
                .store(fail_mcp_stage, Ordering::SeqCst);
            let initial_mcp_expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
                + 1_000;
            let mcp_stage = with_initial_mcp.then(|| {
                let generation = awaken_session_contract::McpGenerationRef {
                    session_id: format!("thread-application-{rule}"),
                    attachment_id: awaken_session_contract::McpAttachmentId("mcp-docs".into()),
                    generation: awaken_session_contract::McpGeneration(1),
                    runtime_incarnation: "worker-a".into(),
                    lease_epoch: 1,
                    lease_expires_at_unix_ms: initial_mcp_expiry,
                };
                awaken_session_contract::StageMcpAttachment {
                    workspace_id: "workspace".into(),
                    generation,
                    realization_id: "realize-docs-1".into(),
                    stage_idempotency_key: "stage-docs-1".into(),
                    name: "docs".into(),
                    target: awaken_session_contract::McpTarget::parse_http(
                        "https://mcp.example.test/sse",
                    )
                    .unwrap(),
                    credential: None,
                    prompts_as_skills: false,
                    selected_plaintext_holder: None,
                }
            });
            let host = if install_contributor {
                host.with_application_session_control(Arc::new(RecordingContributor {
                    calls: contribution_calls.clone(),
                    phases: phases.clone(),
                    projection,
                    mcp_stage,
                    fail_begin: fail_begin.clone(),
                }))
            } else {
                host
            };
            let host = Arc::new(host);
            let managed = crate::ManagedHost::new(host.clone())
                .with_mcp_attachment_realizer(mcp_realizer.clone());
            drop(managed);
            let thread = format!("thread-application-{rule}");
            let run = format!("run-application-{rule}");
            dispatch
                .enqueue(awaken_run_ingress::RunDispatch::new(test_activation(
                    &thread, &run,
                )))
                .await
                .expect("enqueue");
            let now = awaken_run_ingress::SystemClock.now_ms();
            let claimed = dispatch
                .claim("worker-a", 30_000, now, &Default::default())
                .await
                .expect("claim")
                .expect("claimed run");
            let resolver = HostWorkerResolver {
                host: Arc::downgrade(&host),
            };
            let result = resolver.worker_for_claimed(&claimed).await;

            if rule == "W4" {
                assert_eq!(
                    host.renew_due_session_realizations(
                        initial_mcp_expiry,
                        initial_mcp_expiry + 1_000,
                    )
                    .await
                    .expect("W6"),
                    1,
                    "W6"
                );
                fail_begin.store(true, Ordering::SeqCst);
                assert_eq!(
                    host.renew_due_session_realizations(
                        initial_mcp_expiry + 1_000,
                        initial_mcp_expiry + 2_000,
                    )
                    .await
                    .expect("W7 isolates the rejected Session renewal"),
                    0,
                    "W7"
                );
                assert!(!host.session_slots.contains(&thread), "W7");
            }

            assert_eq!(calls.load(Ordering::SeqCst), 1, "{rule}");
            assert_eq!(
                contribution_calls.load(Ordering::SeqCst),
                usize::from(install_contributor),
                "{rule}"
            );
            let succeeds = install_contributor && !fail_mcp_stage;
            assert_eq!(result.is_ok(), succeeds, "{rule}");
            let expected_phases: &[&str] = if fail_mcp_stage {
                &["contribute", "fail"]
            } else if rule == "W4" {
                &[
                    "contribute",
                    "activate",
                    "acknowledge",
                    "begin",
                    "activate",
                    "acknowledge",
                    "begin",
                ]
            } else if install_contributor {
                &["contribute", "activate", "acknowledge"]
            } else {
                &[]
            };
            assert_eq!(phases.lock().unwrap().as_slice(), expected_phases, "{rule}");
            let expected_mcp: &[&str] = if fail_mcp_stage {
                &["stage"]
            } else if rule == "W4" {
                &["stage", "publish", "stage", "publish"]
            } else if with_initial_mcp {
                &["stage", "publish"]
            } else {
                &[]
            };
            assert_eq!(
                mcp_realizer.calls.lock().unwrap().as_slice(),
                expected_mcp,
                "{rule}"
            );
            assert_eq!(
                host.session_environment(&thread).await.is_some(),
                succeeds && rule != "W4",
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn stale_claim_is_rejected_before_application_provisioning() {
        use awaken_run_ingress::{Clock, DispatchQueue};

        let storage = tempfile::tempdir().expect("storage");
        let dispatch = Arc::new(
            awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
                .expect("in-memory dispatch"),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_store_dir(storage.path())
                .with_dispatch_store(dispatch.clone())
                .with_application_session_provisioner(Arc::new(CountingProvisioner {
                    calls: calls.clone(),
                })),
        );
        dispatch
            .enqueue(awaken_run_ingress::RunDispatch::new(test_activation(
                "thread-stale-application",
                "run-stale-application",
            )))
            .await
            .expect("enqueue");
        let now = awaken_run_ingress::SystemClock.now_ms();
        let mut claimed = dispatch
            .claim("worker-a", 30_000, now, &Default::default())
            .await
            .expect("claim")
            .expect("claimed run");
        claimed.lease.epoch += 1;
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        assert!(resolver.worker_for_claimed(&claimed).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            host.session_environment("thread-stale-application")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn resource_manifest_must_match_the_durable_execution_scope() {
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let run = RunId("run-scope-mismatch".to_string());
        let request =
            awaken_run_ingress::RunDispatch::new(test_activation("thread-scope-mismatch", &run.0))
                .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-b"),
                ))
                .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
                    "workspace-a",
                    r#"{"inputs":[],"skills":[]}"#,
                ));
        let claimed = awaken_run_ingress::Claimed {
            request,
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "worker-a".to_string(),
                expires_ms: 100,
                epoch: 1,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: false,
            pending: Vec::new(),
            recovered: false,
            sandbox: None,
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let error = match resolver.worker_for_claimed(&claimed).await {
            Ok(_) => panic!("scope mismatch must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("outside its execution scope"));
        assert!(
            host.session_environment("thread-scope-mismatch")
                .await
                .is_none(),
            "scope rejection happens before sandbox creation"
        );
    }

    #[tokio::test]
    async fn malformed_resource_manifest_is_rejected_before_sandbox_creation() {
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let run = RunId("run-malformed-resources".to_string());
        let request = awaken_run_ingress::RunDispatch::new(test_activation(
            "thread-malformed-resources",
            &run.0,
        ))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-a"),
        ))
        .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
            "workspace-a",
            "not-json",
        ));
        let claimed = awaken_run_ingress::Claimed {
            request,
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "worker-a".to_string(),
                expires_ms: 100,
                epoch: 1,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: false,
            pending: Vec::new(),
            recovered: false,
            sandbox: None,
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        let error = match resolver.worker_for_claimed(&claimed).await {
            Ok(_) => panic!("malformed manifest must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid Session resource manifest")
        );
        assert!(
            host.session_environment("thread-malformed-resources")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn cold_worker_installs_frozen_file_manifest_before_opening_environment() {
        let storage = tempfile::tempdir().expect("storage");
        let mut raw_host =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        // Cause: immutable Managed File input. Effect: the selected provider must
        // guarantee both `/mnt/session/uploads/...` path fidelity and OS-enforced
        // read-only semantics; Workdir correctly fails this compatibility rule.
        raw_host.session_provider =
            crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
                storage.path().join("sandboxes"),
                false,
            );
        let host = Arc::new(raw_host);
        let _managed = crate::ManagedHost::new(host.clone());
        let bytes = b"frozen worker input".to_vec();
        let file_id = host
            .file_application()
            .expect("test composition installs File application")
            .create_uploaded_file(
                "workspace-a",
                "input.bin".into(),
                "application/octet-stream".into(),
                &bytes,
            )
            .await
            .expect("create file")
            .id;
        let manifest = awaken_session_contract::SessionResourceManifest::new(
            "workspace-a",
            awaken_session_contract::ResolvedSessionResources {
                inputs: vec![awaken_session_contract::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::new("file-binding"),
                    source: awaken_session_contract::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from(file_id.as_str()),
                    },
                    mount_path: "/uploads/input.bin".to_string(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            },
        );

        host.install_dispatched_resources("thread-cold-resource", &manifest, None)
            .await
            .expect("install frozen manifest");
        let activation = test_activation("thread-cold-resource", "run-cold-resource");
        host.ctx_for_snapshot_with_sandbox(
            "thread-cold-resource",
            Some("agent-a"),
            Some(activation.snapshot),
            None,
        )
        .await
        .expect("open environment after resource install");

        let mount = host
            .sandbox_spec("thread-cold-resource")
            .mounts
            .into_iter()
            .find(|mount| mount.mount_id == file_id)
            .expect("frozen File mount");
        assert_eq!(mount.mount_path, "/mnt/session/uploads/uploads/input.bin");
        assert_eq!(
            mount.access,
            awaken_provisioning_contract::MountAccess::ReadOnly
        );
        let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = mount.source
        else {
            panic!("frozen File uses the binary-safe carried source")
        };
        assert_eq!(contents, bytes);
        assert_eq!(
            host.thread_resource_manifest("thread-cold-resource"),
            Some(manifest)
        );
    }

    /// Claimed Session Resource generation cause/effect decision table.
    /// Causes: C1 prior process-local generation exists; C2 incoming generation
    /// is exact, newer, older, or same-generation/different-value; C3 Workspace
    /// partition is unchanged. Effects: E1 revalidate exact bytes without a
    /// logical replacement; E2 advance to the newer generation; E3 reject stale,
    /// corrupt, or cross-Workspace replacement and retain the prior manifest.
    /// Rules: R1 exact+C3=>E1; R2 newer+C3=>E2; R3 older+C3=>E3;
    /// R4 same-revision/different-value+C3=>E3; R5 !C3=>E3.
    #[tokio::test]
    async fn claimed_worker_advances_only_to_a_newer_resource_generation() {
        let host = Arc::new(SharedHost::new(Arc::new(AdoptionModel), "stub"));
        let _managed = crate::ManagedHost::new(host.clone());
        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-generation-fence".into()),
            owner: "worker-generation-fence".into(),
            epoch: 1,
        };
        let revision_four = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            4,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        host.install_dispatched_resources("thread-generation-fence", &revision_four, None)
            .await
            .expect("install active generation");

        host.install_dispatched_resources("thread-generation-fence", &revision_four, Some(&claim))
            .await
            .expect("R1 exact replay");

        for rejected in [
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                3,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                4,
                awaken_session_contract::ResolvedSessionResources {
                    inputs: Vec::new(),
                    skills: Some(Vec::new()),
                },
            ),
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-b",
                5,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
        ] {
            let result = host
                .install_dispatched_resources("thread-generation-fence", &rejected, Some(&claim))
                .await;
            assert!(
                result.is_err(),
                "R3/R4/R5 reject non-authoritative replacement: {rejected:?}"
            );
            assert_eq!(
                host.thread_resource_manifest("thread-generation-fence"),
                Some(revision_four.clone())
            );
        }

        let revision_five = awaken_session_contract::SessionResourceManifest::at_revision(
            "workspace-a",
            5,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        host.install_dispatched_resources("thread-generation-fence", &revision_five, Some(&claim))
            .await
            .expect("R2 newer generation");
        assert_eq!(
            host.thread_resource_manifest("thread-generation-fence"),
            Some(revision_five)
        );
    }

    /// Cause/effect decision table for Worker-side File staging:
    /// | Rule | Worker File source | exact claim | local File DB entry | Effect |
    /// |---|---|---|---|---|
    /// | W1 | remote adapter | present | absent | stage returned digest/bytes; later runtime validation does not reopen a local File database |
    /// | W2 | remote adapter | absent | absent | fail closed; no mount |
    #[tokio::test]
    async fn cold_worker_file_staging_uses_only_the_claim_bound_source() {
        struct ClaimFileSource;

        #[async_trait::async_trait]
        impl crate::FileContentSource for ClaimFileSource {
            async fn read(
                &self,
                workspace_id: &str,
                file_id: &str,
                claim: Option<&awaken_run_ingress::RunClaim>,
            ) -> Result<Option<(String, Vec<u8>)>, crate::FileContentSourceError> {
                let Some(claim) = claim else {
                    return Ok(None);
                };
                if workspace_id != "workspace-remote"
                    || file_id != "file-remote"
                    || claim.run_id.0 != "run-remote"
                    || claim.owner != "worker-remote"
                    || claim.epoch != 7
                {
                    return Ok(None);
                }
                let bytes = b"remote immutable File".to_vec();
                Ok(Some((awaken_file_store::content_id(&bytes), bytes)))
            }
        }

        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub")
                .with_file_content_source(Arc::new(ClaimFileSource)),
        );
        let managed = crate::ManagedHost::new(host.clone());
        let manifest = awaken_session_contract::SessionResourceManifest::new(
            "workspace-remote",
            awaken_session_contract::ResolvedSessionResources {
                inputs: vec![awaken_session_contract::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::new("remote-file-binding"),
                    source: awaken_session_contract::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-remote"),
                    },
                    mount_path: "/input.txt".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            },
        );
        let claim = awaken_run_ingress::RunClaim {
            run_id: RunId("run-remote".into()),
            owner: "worker-remote".into(),
            epoch: 7,
        };

        host.install_dispatched_resources("remote-file-ok", &manifest, Some(&claim))
            .await
            .expect("W1");
        let mount = host
            .sandbox_spec("remote-file-ok")
            .mounts
            .into_iter()
            .find(|mount| mount.mount_id == "file-remote")
            .expect("W1 exact mount");
        assert!(
            matches!(
                mount.source,
                awaken_provisioning_contract::MountSource::InlineBytes { ref contents, .. }
                    if contents == b"remote immutable File"
            ),
            "W1"
        );
        managed
            .validate_thread_resource_bindings("remote-file-ok")
            .await
            .expect("W1 has no redundant local File catalog check");

        let error = host
            .install_dispatched_resources("remote-file-no-claim", &manifest, None)
            .await
            .expect_err("W2");
        assert!(error.to_string().contains("not found"), "W2: {error}");
        assert!(
            host.sandbox_spec("remote-file-no-claim").mounts.is_empty(),
            "W2"
        );
    }

    #[tokio::test]
    async fn sandbox_binding_is_validated_before_provider_adoption() {
        let storage = tempfile::tempdir().expect("storage");
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        assert!(
            host.adopt_bound_session_environment("thread-a", Some("not-json"), false)
                .await
                .is_err()
        );

        let wrong = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            "local", "thread-b",
        ))
        .unwrap();
        assert!(
            host.adopt_bound_session_environment("thread-a", Some(&wrong), false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn recovery_mode_controls_the_production_adoption_seam() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-adoption";
        let first = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let first_ctx = first.ctx_for(thread, None).await.expect("first session");
        let handle = first_ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        drop(first_ctx);
        drop(first);

        let replacement =
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let run_id = RunId("run-adoption".into());
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&encoded),
            thread,
            &run_id,
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("continuity mode adopts the durable handle");
        assert!(!rebuild);
        assert_eq!(adopted.unwrap().handle(), handle);

        let missing_thread = "thread-missing";
        let missing = serde_json::to_string(&awaken_provisioning_contract::SandboxHandle::new(
            handle.provider_kind,
            missing_thread,
        ))
        .unwrap();
        let (adopted, rebuild) = adopt_bound_sandbox(
            &replacement,
            Some(&missing),
            missing_thread,
            &run_id,
            awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .expect("rebuild mode may replace a missing sandbox from committed truth");
        assert!(adopted.is_none());
        assert!(rebuild);
        assert!(
            adopt_bound_sandbox(
                &replacement,
                Some(&missing),
                missing_thread,
                &run_id,
                awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
            )
            .await
            .is_err(),
            "continuity mode fails closed when the bound sandbox is gone"
        );
    }

    #[tokio::test]
    async fn a_resident_environment_is_reused_without_a_second_adoption() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-resident-adoption";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let handle = ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();

        let (adopted, rebuild) = adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &RunId("resident-run".into()),
            awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
        )
        .await
        .expect("resident handle is already adopted");

        assert!(adopted.is_none(), "no duplicate environment wrapper");
        assert!(!rebuild);
        assert_eq!(host.session_environment_handle(thread).await, Some(handle));
    }

    #[tokio::test]
    async fn a_dead_resident_environment_fails_continuity_and_is_fenced_before_rebuild() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-dead-resident";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let handle = ctx.env.as_ref().expect("eager environment").handle();
        let encoded = serde_json::to_string(&handle).unwrap();
        std::fs::remove_dir_all(storage.path().join("sandboxes").join(thread))
            .expect("terminate local sandbox out of band");

        let run = RunId("dead-resident-run".into());
        assert!(
            adopt_bound_sandbox(
                &host,
                Some(&encoded),
                thread,
                &run,
                awaken_run_ingress::WorkerRecoveryMode::RequireSandboxContinuity,
            )
            .await
            .is_err(),
            "continuity never silently replaces a dead resident sandbox"
        );
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(handle.clone()),
            "a failed continuity check does not mutate the owner registry"
        );

        let (adopted, rebuild) = adopt_bound_sandbox(
            &host,
            Some(&encoded),
            thread,
            &run,
            awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .expect("explicit rebuild may discard the dead resident environment");
        assert!(adopted.is_none());
        assert!(rebuild);
        assert!(host.session_environment(thread).await.is_none());
        assert!(
            !host
                .session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn a_stale_binding_cannot_evict_a_different_resident_environment() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-binding-fence";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        let ctx = host.ctx_for(thread, None).await.expect("resident session");
        let resident = ctx.env.as_ref().expect("eager environment").handle();
        let mut stale = resident.clone();
        stale.extra = Some(serde_json::json!({"generation": "stale"}));
        let encoded = serde_json::to_string(&stale).unwrap();

        assert!(
            adopt_bound_sandbox(
                &host,
                Some(&encoded),
                thread,
                &RunId("stale-binding-run".into()),
                awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
            )
            .await
            .is_err(),
            "rebuild mode cannot override the full-handle fence"
        );
        assert_eq!(
            host.session_environment_handle(thread).await,
            Some(resident)
        );
        assert!(
            host.session_slots
                .read(thread, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }

    #[tokio::test]
    async fn an_aba_replacement_with_the_same_handle_survives_stale_discard() {
        let storage = tempfile::tempdir().expect("storage");
        let thread = "thread-environment-aba";
        let host = SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path());
        host.ctx_for(thread, None).await.expect("resident session");
        let observed = host
            .session_environment(thread)
            .await
            .expect("observed owner");
        let replacement = Arc::new(
            host.session_provider
                .adopt(&observed.handle())
                .await
                .expect("same-handle replacement"),
        );
        assert_eq!(observed.handle(), replacement.handle());
        assert!(!Arc::ptr_eq(&observed, &replacement));

        host.session_slots.update(thread, |slot| {
            slot.runtime = None;
            slot.environment = Some(replacement.clone());
        });

        assert!(
            !host.discard_session_environment(thread, &observed).await,
            "object identity fences a stale observer even when the handle is reused"
        );
        let current = host
            .session_environment(thread)
            .await
            .expect("replacement kept");
        assert!(Arc::ptr_eq(&current, &replacement));
    }

    #[tokio::test]
    async fn cancellation_resolution_does_not_touch_an_invalid_sandbox_binding() {
        let storage = tempfile::tempdir().expect("storage");
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let thread = ThreadId("thread-control-only".to_string());
        let run = RunId("run-control-only".to_string());
        let fingerprint = CatalogFingerprint("control-only-catalog".to_string());
        let activation = RunActivation::new(
            run.clone(),
            thread.clone(),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("control-only-snapshot".to_string()),
                metadata: Default::default(),
                root_agent_id: AgentId("control-only-agent".to_string()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: fingerprint.clone(),
                    instructions: "test".to_string(),
                    max_steps: 1,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("provider", "model", "backend"),
                    ),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint,
            },
            Vec::new(),
        );
        let claimed = awaken_run_ingress::Claimed {
            request: awaken_run_ingress::RunDispatch::new(activation),
            lease: awaken_run_ingress::Lease {
                run_id: run,
                owner: "control-owner".to_string(),
                expires_ms: 100,
                epoch: 2,
            },
            credential_bindings: Vec::new(),
            cancellation_requested: true,
            pending: Vec::new(),
            recovered: false,
            sandbox: Some("this is deliberately not a sandbox handle".to_string()),
            assignment: None,
        };
        let resolver = HostWorkerResolver {
            host: Arc::downgrade(&host),
        };

        resolver
            .worker_for_claimed(&claimed)
            .await
            .expect("terminal control bypasses sandbox decoding/adoption");
        assert!(host.session_environment(&thread.0).await.is_none());
        assert!(
            !host
                .session_slots
                .read(&thread.0, |slot| slot.runtime.is_some())
                .unwrap_or(false)
        );
    }
}
