//! [`HostWorkerResolver`]: routes a claimed dispatch to the worker that owns
//! its thread, opening (or reusing) the session through the host.

use super::*;

struct WorkerProjectionSynchronizer<'a>(&'a SharedHost);

#[async_trait::async_trait]
impl awaken_protocol_managed::SessionProjectionSynchronizer for WorkerProjectionSynchronizer<'_> {
    async fn synchronize_session_projection(
        &self,
        session_id: &str,
        projection: &awaken_protocol_managed::FrozenSessionProjection,
        lease: &awaken_protocol_managed::SessionRealizationLease,
        _prepare_session: bool,
    ) -> Result<(), awaken_protocol_managed::RunError> {
        self.0
            .install_frozen_session_projection(session_id, projection.clone())
            .await
            .map_err(|error| awaken_protocol_managed::RunError::internal(error.to_string()))?;
        self.0
            .install_session_realization_lease(session_id, lease.clone());
        Ok(())
    }
}

struct WorkerMcpEffects<'a>(&'a SharedHost);

#[async_trait::async_trait]
impl awaken_protocol_managed::McpAttachmentRealizer for WorkerMcpEffects<'_> {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_protocol_managed::StageMcpAttachment,
    ) -> Result<awaken_protocol_managed::McpRealizationReceipt, awaken_protocol_managed::RunError>
    {
        self.0.stage_dispatched_mcp(request).await
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), awaken_protocol_managed::RunError> {
        self.0.publish_dispatched_mcp(generation).await
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_protocol_managed::McpGenerationRef,
    ) -> Result<(), awaken_protocol_managed::RunError> {
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
        directive: awaken_protocol_managed::SessionRealizationDirective,
    ) -> Result<(), awaken_run_ingress::Error> {
        awaken_protocol_managed::drive_session_realization(
            session_id,
            control.as_ref(),
            &WorkerProjectionSynchronizer(host),
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
        if let Some(observer) = host
            .memory_terminal_observer(&thread_id.0, &claimed.request.activation.snapshot, commit)
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
            let plan = provisioner
                .prepare(&claimed.request.activation, ownership.clone())
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
                .contribute(&thread_id.0, &claim, plan)
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
            Self::realize_application_session(&host, control, &thread_id.0, receipt.realization)
                .await?;
        } else if let Some(manifest) = &dispatched_resources {
            host.install_dispatched_resources(&thread_id.0, manifest)
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?;
        }

        let (adopted, rebuild_binding) = adopt_bound_sandbox(
            &host,
            claimed.sandbox.as_deref(),
            &thread_id.0,
            &claimed.lease.run_id,
            claimed.request.placement.recovery,
        )
        .await?;

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
            let environment = host
                .session_environment(&thread_id.0)
                .await
                .ok_or_else(|| Self::execution_error("resolved session disappeared"))?;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct CountingProvisioner {
        calls: Arc<AtomicUsize>,
    }

    struct RecordingContributor {
        calls: Arc<AtomicUsize>,
        phases: Arc<std::sync::Mutex<Vec<&'static str>>>,
        projection: Arc<std::sync::Mutex<Option<awaken_protocol_managed::FrozenSessionProjection>>>,
        mcp_stage: Option<awaken_protocol_managed::StageMcpAttachment>,
    }

    #[derive(Default)]
    struct RecordingMcpRealizer {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_stage: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl awaken_protocol_managed::McpAttachmentRealizer for RecordingMcpRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: awaken_protocol_managed::StageMcpAttachment,
        ) -> Result<awaken_protocol_managed::McpRealizationReceipt, awaken_protocol_managed::RunError>
        {
            self.calls.lock().unwrap().push("stage");
            if self.fail_stage.load(Ordering::SeqCst) {
                return Err(awaken_protocol_managed::RunError::classified(
                    "test_mcp_stage_failed",
                    "test MCP stage failed",
                ));
            }
            Ok(awaken_protocol_managed::McpRealizationReceipt {
                receipt_fingerprint: request.fingerprint(),
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: awaken_protocol_managed::McpGenerationRef,
        ) -> Result<(), awaken_protocol_managed::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: awaken_protocol_managed::McpGenerationRef,
        ) -> Result<(), awaken_protocol_managed::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::ApplicationSessionControlClient for RecordingContributor {
        async fn contribute(
            &self,
            _session_id: &str,
            _claim: &awaken_run_ingress::RunClaim,
            plan: crate::ApplicationSessionPlan,
        ) -> Result<crate::ApplicationSessionControlReceipt, crate::ApplicationSessionError>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.phases.lock().unwrap().push("contribute");
            let holder = awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                "test.worker",
            );
            let input = awaken_protocol_managed::ApplicationSessionInput {
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: plan.prompts,
                mcp_inputs: plan.mcp_inputs,
                network_restriction: plan.network_restriction,
            };
            let baseline = awaken_protocol_managed::SessionBaseline::compile(
                awaken_protocol_managed::SessionBaselineInputs {
                    environment: awaken_protocol_managed::EnvironmentSnapshot {
                        environment_id: "env".into(),
                        revision: awaken_protocol_managed::EnvironmentRevision(1),
                        config_fingerprint: awaken_protocol_managed::EnvironmentFingerprint(
                            "env-fingerprint".into(),
                        ),
                        sandbox: serde_json::json!({}),
                        packages: Default::default(),
                        network: awaken_protocol_managed::SessionNetworkPolicy::Unrestricted,
                        credential_realization:
                            awaken_runtime_contract::CredentialRealizationProfile {
                                inference_holder: holder.clone(),
                                mcp_holder: holder.clone(),
                                resource_holder: holder,
                            },
                    },
                    mcp_authoring: Default::default(),
                    agent_id: "agent".into(),
                    model: "model".into(),
                    runtime: None,
                    application: Some(
                        awaken_protocol_managed::ApplicationContributionReceipt::from_input(
                            plan.fingerprint,
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
            let projection = awaken_protocol_managed::FrozenSessionProjection {
                workspace_id: "workspace".into(),
                revision: awaken_protocol_managed::SessionRevision(2),
                baseline,
                resources: Default::default(),
                mcp,
                toolsets: Vec::new(),
            };
            *self.projection.lock().unwrap() = Some(projection.clone());
            let lease = awaken_protocol_managed::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a".into(),
                epoch: 1,
                expires_at_unix_ms: self
                    .mcp_stage
                    .as_ref()
                    .map_or(u64::MAX, |stage| stage.generation.lease_expires_at_unix_ms),
            };
            Ok(crate::ApplicationSessionControlReceipt {
                contribution: awaken_protocol_managed::ApplicationSessionContributionReceipt {
                    outcome: awaken_protocol_managed::ApplicationContributionOutcome::Committed,
                    projection: projection.clone(),
                },
                realization: awaken_protocol_managed::SessionRealizationDirective {
                    projection,
                    lease,
                    action: awaken_protocol_managed::SessionRealizationAction::Stage {
                        prepare_session: true,
                        mcp_stages: self.mcp_stage.clone().into_iter().collect(),
                    },
                },
            })
        }
    }

    #[async_trait::async_trait]
    impl awaken_protocol_managed::SessionRealizationControl for RecordingContributor {
        async fn begin_session_realization(
            &self,
            command: awaken_protocol_managed::BeginSessionRealization,
        ) -> Result<
            awaken_protocol_managed::SessionRealizationDirective,
            awaken_protocol_managed::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("begin");
            let projection = self
                .projection
                .lock()
                .unwrap()
                .clone()
                .ok_or(awaken_protocol_managed::SessionRealizationControlFailure::NotReady)?;
            let mut stage = self
                .mcp_stage
                .clone()
                .ok_or(awaken_protocol_managed::SessionRealizationControlFailure::NotReady)?;
            stage.generation.lease_expires_at_unix_ms = command.target.lease_expires_at_unix_ms;
            stage.stage_idempotency_key =
                format!("renew:{}", command.target.lease_expires_at_unix_ms);
            Ok(awaken_protocol_managed::SessionRealizationDirective {
                projection,
                lease: awaken_protocol_managed::SessionRealizationLease {
                    owner: command.target.owner,
                    runtime_incarnation: command.target.runtime_incarnation,
                    epoch: 1,
                    expires_at_unix_ms: command.target.lease_expires_at_unix_ms,
                },
                action: awaken_protocol_managed::SessionRealizationAction::Stage {
                    prepare_session: false,
                    mcp_stages: vec![stage],
                },
            })
        }

        async fn activate_session_realization(
            &self,
            command: awaken_protocol_managed::ActivateSessionRealization,
        ) -> Result<
            awaken_protocol_managed::SessionRealizationDirective,
            awaken_protocol_managed::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("activate");
            Ok(awaken_protocol_managed::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("contribution projection"),
                lease: command.lease,
                action: awaken_protocol_managed::SessionRealizationAction::Publish {
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
            command: awaken_protocol_managed::AcknowledgeSessionRealization,
        ) -> Result<
            awaken_protocol_managed::SessionRealizationDirective,
            awaken_protocol_managed::SessionRealizationControlFailure,
        > {
            self.phases.lock().unwrap().push("acknowledge");
            Ok(awaken_protocol_managed::SessionRealizationDirective {
                projection: self
                    .projection
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("contribution projection"),
                lease: command.lease,
                action: awaken_protocol_managed::SessionRealizationAction::Complete,
            })
        }

        async fn fail_session_realization(
            &self,
            _command: awaken_protocol_managed::FailSessionRealization,
        ) -> Result<(), awaken_protocol_managed::SessionRealizationControlFailure> {
            self.phases.lock().unwrap().push("fail");
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::ApplicationSessionProvisioner for CountingProvisioner {
        async fn prepare(
            &self,
            activation: &RunActivation,
            ownership: Arc<dyn awaken_runtime_contract::runtime_context::AttemptOwnershipVerifier>,
        ) -> Result<crate::ApplicationSessionPlan, crate::ApplicationSessionError> {
            ownership
                .verify_current()
                .await
                .map_err(|error| crate::ApplicationSessionError::new(error.to_string()))?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::ApplicationSessionPlan::empty(format!(
                "application:{}",
                activation.snapshot.fingerprint.0
            )))
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
        // | W1 | T | success | installed | receipt then environment |
        // | W2 | T | success | missing | reject before environment |
        // | W3 | F | - | any | reject before provisioner |
        // | W4 | T | success + initial MCP | installed | stage/activate/publish/ack |
        // | W5 | T | initial MCP stage fails | installed | fail; no publish/environment |
        // | W6 | T | active MCP lease due | installed | same canonical driver renews generation |
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
                let generation = awaken_protocol_managed::McpGenerationRef {
                    session_id: format!("thread-application-{rule}"),
                    attachment_id: awaken_protocol_managed::McpAttachmentId("mcp-docs".into()),
                    generation: awaken_protocol_managed::McpGeneration(1),
                    runtime_incarnation: "worker-a".into(),
                    lease_epoch: 1,
                    lease_expires_at_unix_ms: initial_mcp_expiry,
                };
                awaken_protocol_managed::StageMcpAttachment {
                    workspace_id: "workspace".into(),
                    generation,
                    realization_id: "realize-docs-1".into(),
                    stage_idempotency_key: "stage-docs-1".into(),
                    name: "docs".into(),
                    target: awaken_protocol_managed::McpTarget::parse_http(
                        "https://mcp.example.test/sse",
                    )
                    .unwrap(),
                    credential: None,
                    selected_plaintext_holder: None,
                }
            });
            let host = if install_contributor {
                host.with_application_session_control(Arc::new(RecordingContributor {
                    calls: contribution_calls.clone(),
                    phases: phases.clone(),
                    projection,
                    mcp_stage,
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
                succeeds,
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
        let host = Arc::new(
            SharedHost::new(Arc::new(AdoptionModel), "stub").with_store_dir(storage.path()),
        );
        let _managed = crate::ManagedHost::new(host.clone());
        let bytes = b"frozen worker input".to_vec();
        let file_id = host.file_store().put(&bytes).await.expect("store file");
        host.register_file_ownership("workspace-a", &file_id)
            .await
            .expect("own file");
        let manifest = awaken_protocol_managed::SessionResourceManifest::new(
            "workspace-a",
            awaken_protocol_managed::ResolvedSessionResources {
                inputs: vec![awaken_protocol_managed::ResolvedInput {
                    binding_id: awaken_protocol_managed::resource_plane::BindingId::new(
                        "file-binding",
                    ),
                    source: awaken_protocol_managed::ResolvedInputSource::File {
                        file_id: awaken_protocol_managed::resource_plane::FileId::from(
                            file_id.as_str(),
                        ),
                    },
                    mount_path: "/uploads/input.bin".to_string(),
                    access: awaken_protocol_managed::resource_plane::ResourceAccess::ReadOnly,
                    instructions: None,
                }],
                skills: Some(Vec::new()),
            },
        );

        host.install_dispatched_resources("thread-cold-resource", &manifest)
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

        let environment = host
            .session_environment("thread-cold-resource")
            .await
            .expect("environment");
        let files = environment.list_files(".mnt").await.expect("list mounts");
        assert!(
            files
                .iter()
                .any(|(path, contents)| path.ends_with("uploads/input.bin") && contents == &bytes),
            "the first environment contains the exact immutable File bytes"
        );
        assert_eq!(
            host.thread_resource_manifest("thread-cold-resource"),
            Some(manifest)
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
        let handle = first_ctx.env.handle();
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
        let handle = ctx.env.handle();
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
        let handle = ctx.env.handle();
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
        let resident = ctx.env.handle();
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
