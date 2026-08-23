//! Claim-bound dispatch realization and worker selection.

use super::session_realization::WorkerMcpEffects;
use super::*;

pub(super) async fn adopt_bound_sandbox(
    host: &SharedHost,
    encoded: Option<&str>,
    expected_sandbox_id: &str,
    run_id: &RunId,
    provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    recovery: awaken_run_ingress::WorkerRecoveryMode,
) -> Result<(Option<crate::session_environment::SessionEnvironment>, bool), awaken_run_ingress::Error>
{
    host.adopt_bound_session_environment(
        expected_sandbox_id,
        encoded,
        provisioning,
        recovery == awaken_run_ingress::WorkerRecoveryMode::RebuildFromCommittedTruth,
    )
    .await
    .map_err(|error| HostWorkerResolver::execution_error(format!("run {}: {error}", run_id.0)))
}

impl HostWorkerResolver {
    async fn recovered_child_worker(
        &self,
        host: &SharedHost,
        claimed: &awaken_run_ingress::Claimed,
        session_thread_id: &awaken_agent_contract::agent::thread::Id,
        adopted: Option<crate::session_environment::SessionEnvironment>,
        publication_source: Arc<awaken_runtime_contract::StaticPublishedAgentSnapshots>,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let substrate = host
            .session_child_execution_substrate(&session_thread_id.0, adopted, None)
            .await
            .map_err(|error| Self::execution_error(error.to_string()))?;
        let environment = substrate.environment;
        let snapshot = &claimed.request.activation.snapshot;
        let authorization = crate::config::effective_tool_authorization(
            &snapshot.resolved_spec.plugin_config,
            &[],
            &snapshot.resolved_spec.plugin_config.agent.toolsets,
        );
        let mut runtime = crate::config::build_runtime_with_authorization(
            host.llm.clone(),
            environment.as_ref(),
            &authorization,
        );
        let commit = substrate.commit;
        if let Some(delegation) = host
            .run_delegation(
                &session_thread_id.0,
                environment.clone(),
                commit.clone(),
                Some(snapshot),
                publication_source,
            )
            .map_err(|error| Self::execution_error(error.to_string()))?
        {
            runtime = runtime.with_run_delegation(delegation);
        }
        let acp = host.acp.clone().map(|acp| {
            let environment = environment.clone();
            Arc::new(move |backend, permission| {
                Ok(
                    acp.executor_for(environment.clone(), permission, backend, Vec::new())
                        as Arc<dyn awaken_runtime_contract::execution::RunAttemptExecutor>,
                )
            }) as crate::agent_runner::ChildAcpExecutorFactory
        });
        let adapters = crate::agent_runner::ChildExecutionAdapters {
            acp,
            remote: host.remote_attempt_executor.clone(),
            remote_credentials: host.remote_credential_realization.clone(),
            web_search: Some(host.web_search_plugin(&session_thread_id.0, None)),
        };
        let runtime = Arc::new(runtime);
        let attempt = crate::agent_runner::child_attempt_executor(
            runtime,
            snapshot,
            &adapters,
            authorization.policy,
        )
        .map_err(|error| Self::execution_error(error.to_string()))?;
        let worker = self.boundary_worker(host, claimed, commit, false).await?;
        let mut worker =
            Arc::into_inner(worker).expect("a new child recovery boundary worker is not shared");
        // A cold/replacement child must resolve the same publication-pinned model
        // as the original attempt. The Runtime's host fallback is intentionally
        // unconfigured on a database-independent Worker; omitting this common
        // materializer silently turns a recovered child into a different Agent.
        if let Some(materializer) = host.worker_inference_materializer() {
            worker = worker.with_inference_materializer(materializer);
        }
        worker.install_attempt_executor(attempt);
        let workspace_id = claimed.request.execution_scope.as_ref().map_or_else(
            || host.local_workspace().to_owned(),
            |scope| scope.0.0.clone(),
        );
        let child_context = substrate
            .attempt_context
            .with_model_content_materializer(Arc::new(
                crate::model_content_materializer::ResourceModelContentMaterializer::new(
                    host.file_content_source.clone(),
                    workspace_id,
                    claimed.request.thread_id().0.clone(),
                    Some(awaken_run_ingress::RunClaim::from(&claimed.lease)),
                ),
            ));
        worker = worker
            .with_context(child_context)
            .with_local_credential_capabilities(adapters.remote_credentials);
        Ok(Arc::new(worker))
    }

    pub(super) async fn reconcile_dispatched_mcp(
        host: &SharedHost,
        session_id: &str,
        stages: Vec<awaken_session_contract::StageMcpAttachment>,
    ) -> Result<(), awaken_run_ingress::Error> {
        use awaken_session_contract::McpAttachmentRealizer as _;

        let existing = host.active_mcp_projections(session_id);
        let existing_generations = existing
            .iter()
            .map(|projection| {
                awaken_session_contract::stable_fingerprint(&projection.request.generation)
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut desired_generations = std::collections::BTreeSet::new();
        let mut desired_names = std::collections::BTreeSet::new();
        for stage in &stages {
            if stage.generation.session_id != session_id
                || !desired_generations.insert(awaken_session_contract::stable_fingerprint(
                    &stage.generation,
                ))
                || !desired_names.insert(stage.name.clone())
            {
                return Err(Self::execution_error(
                    "dispatched MCP projection has a foreign or duplicate generation/name",
                ));
            }
        }

        let effects = WorkerMcpEffects(host);
        let mut newly_staged = Vec::new();
        for stage in stages {
            match effects.stage_mcp_attachment(stage.clone()).await {
                Ok(receipt) => {
                    if !existing_generations.contains(&awaken_session_contract::stable_fingerprint(
                        &receipt.generation,
                    )) {
                        newly_staged.push(receipt.generation.clone());
                    }
                    if let Err(error) = effects
                        .publish_mcp_generation(receipt.generation.clone())
                        .await
                    {
                        for generation in newly_staged {
                            let _ = effects.drain_mcp_generation(generation).await;
                        }
                        return Err(Self::execution_error(format!(
                            "dispatched MCP publication failed: {error}"
                        )));
                    }
                }
                Err(error) => {
                    for generation in newly_staged {
                        let _ = effects.drain_mcp_generation(generation).await;
                    }
                    return Err(Self::execution_error(format!(
                        "dispatched MCP staging failed: {error}"
                    )));
                }
            }
        }
        for projection in existing {
            if !desired_generations.contains(&awaken_session_contract::stable_fingerprint(
                &projection.request.generation,
            )) {
                effects
                    .drain_mcp_generation(projection.request.generation)
                    .await
                    .map_err(|error| {
                        Self::execution_error(format!(
                            "obsolete dispatched MCP drain failed: {error}"
                        ))
                    })?;
            }
        }
        Ok(())
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
        Self::require_local_execution(&host)?;
        self.resolve(
            &host,
            thread_id,
            agent_id.filter(|id| !id.is_empty()),
            None,
            None,
        )
        .await
    }

    async fn worker_for_claimed(
        &self,
        claimed: &awaken_run_ingress::Claimed,
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let host = self.host()?;
        Self::require_local_execution(&host)?;
        // Reservation repair owns no executable Run yet. It needs only the
        // parent commit boundary, queue fence, and Session admission observer;
        // resolving publications, credentials, Sandbox, MCP, or Environment
        // here can both block repair and create effects before admission.
        if claimed.session_activity_admission_required {
            let thread_id = claimed.request.session_thread_id();
            let commit = Arc::new(
                host.build_commit(&thread_id.0)
                    .await
                    .map_err(|error| Self::execution_error(error.to_string()))?,
            );
            return self.boundary_worker(&host, claimed, commit, false).await;
        }
        if claimed.cancellation_requested {
            return self.cancellation_worker(&host, claimed).await;
        }
        let thread_id = claimed.request.session_thread_id();
        let run_thread_id = claimed.request.thread_id();
        let agent_id = claimed.request.activation.snapshot.root_agent_id.0.as_str();
        let agent_id = (!agent_id.is_empty()).then_some(agent_id);
        let publication_workspace = claimed
            .request
            .execution_scope
            .as_ref()
            .map_or_else(|| host.local_workspace(), |scope| scope.0.0.as_str());
        let claimed_source = crate::agent_catalog::exact_run_publication_source(
            &claimed.request.activation.snapshot,
            &claimed.request.agent_publications,
            publication_workspace,
        )
        .map_err(|error| {
            Self::execution_error(format!(
                "run {} has an invalid Agent publication closure: {error}",
                claimed.lease.run_id.0
            ))
        })?;
        let effective_model_ref = claimed.request.activation.effective_model_ref().to_string();
        let claimed_runtime_input = ClaimedRuntimeInput {
            identity: RuntimePublicationIdentity::from_publications(
                &claimed.request.activation.snapshot,
                &claimed.request.agent_publications,
                &effective_model_ref,
            ),
            publications: claimed_source.clone(),
            effective_model_ref,
        };
        host.session_slots.update(&thread_id.0, |slot| {
            slot.dispatch_claim = Some(awaken_run_ingress::RunClaim::from(&claimed.lease));
        });
        let dispatched_resources =
            crate::dispatch_session_runtime::decode_dispatched_resource_manifest(&claimed.request)
                .map_err(Self::execution_error)?;
        let dispatched_mcp_stages = if let Some(envelope) = &claimed.request.session_runtime {
            let projection = envelope.decode_projection().map_err(|error| {
                Self::execution_error(format!(
                    "run {} has an invalid Session runtime projection: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
            host.install_environment_projection(&thread_id.0, &projection.environment)
                .map_err(|error| Self::execution_error(error.to_string()))?;
            host.session_slots
                .update(&thread_id.0, |slot| slot.tools = projection.tools);
            projection.mcp_stages
        } else {
            None
        };
        // Claimed Sessions realize MCP exclusively through Control's frozen
        // directive below. The dispatch envelope remains a compatibility carrier
        // for ordinary Sessions; replaying its MCP stages as well would advance a
        // second process-local lease fence outside the canonical phase protocol.
        let dispatched_mcp_stages =
            if host.session_control.is_some() && claimed.request.session_thread_id.is_some() {
                None
            } else {
                dispatched_mcp_stages
            };
        install_claimed_session_projection(
            &host,
            claimed,
            thread_id,
            dispatched_resources.as_ref(),
        )
        .await?;
        if let Some(stages) = dispatched_mcp_stages.as_ref() {
            host.register_thread_agent_projection(&thread_id.0, agent_id.unwrap_or("assistant"));
            host.register_thread_backend_projection(
                &thread_id.0,
                &claimed
                    .request
                    .activation
                    .snapshot
                    .resolved_spec
                    .model_binding
                    .backend_ref,
            );
            if let Some(workspace) = stages.first().map(|stage| stage.workspace_id.as_str()) {
                host.register_thread_workspace(&thread_id.0, workspace);
            }
        }
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
            claimed
                .request
                .activation
                .snapshot
                .resolved_spec
                .model_binding
                .provisioning(),
            claimed.request.placement.recovery,
        )
        .await?;
        let mut adopted = adopted;
        let requires_sandbox_stdio_environment =
            dispatched_mcp_stages.as_ref().is_some_and(|stages| {
                stages
                    .iter()
                    .any(|stage| stage.target.sandbox_stdio_target().is_some())
            });
        if requires_sandbox_stdio_environment {
            // A sandbox-stdio stage may need to realize a cold Environment. The
            // physical Session substrate must exist before staging, but parent
            // Agent plugins remain attempt-scoped and are built only by the
            // final root/child resolver below.
            let frozen_parent =
                (run_thread_id == thread_id).then_some(&claimed.request.activation.snapshot);
            host.session_child_execution_substrate(&thread_id.0, adopted.take(), frozen_parent)
                .await
                .map_err(|error| {
                    Self::execution_error(format!(
                        "run {} sandbox-stdio physical realization failed: {error}",
                        claimed.lease.run_id.0
                    ))
                })?;
        }
        if let Some(stages) = dispatched_mcp_stages {
            Self::reconcile_dispatched_mcp(&host, &thread_id.0, stages).await?;
        }
        if run_thread_id != thread_id {
            return self
                .recovered_child_worker(&host, claimed, thread_id, adopted.take(), claimed_source)
                .await
                .map_err(|error| {
                    Self::execution_error(format!(
                        "run {} coordinated child resolution failed: {error}",
                        claimed.lease.run_id.0
                    ))
                });
        }
        let worker = self
            .resolve_claimed(
                &host,
                thread_id,
                agent_id,
                claimed.request.activation.snapshot.clone(),
                adopted.take(),
                claimed_runtime_input,
            )
            .await
            .map_err(|error| {
                Self::execution_error(format!(
                    "run {} root Session resolution failed: {error}",
                    claimed.lease.run_id.0
                ))
            })?;
        if claimed.sandbox.is_none() || rebuild_binding {
            let Some(environment) = host.session_environment(&thread_id.0).await else {
                return Ok(worker);
            };
            let encoded = serde_json::to_string(&environment.handle())
                .map_err(|error| Self::execution_error(error.to_string()))?;
            let outcome = host
                .dispatch_store()
                .map_err(|error| Self::execution_error(error.to_string()))?
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

    async fn settle_claimed_resolution_failure(
        &self,
        claimed: &awaken_run_ingress::Claimed,
        error: awaken_run_ingress::Error,
        clock: Arc<dyn awaken_run_ingress::Clock>,
    ) -> Result<Option<(RunId, RunState)>, awaken_run_ingress::Error> {
        let host = self.host()?;
        let thread_id = claimed.request.session_thread_id();
        let commit = Arc::new(
            host.build_commit(&thread_id.0)
                .await
                .map_err(|commit_error| Self::execution_error(commit_error.to_string()))?,
        );
        let worker = self.boundary_worker(&host, claimed, commit, false).await?;
        worker
            .fail_claimed_before_execution(
                claimed,
                "dispatch_resolution_failed",
                error.to_string(),
                clock,
            )
            .await
    }

    async fn terminalize_retry_exhausted(
        &self,
        claimed: &awaken_run_ingress::Claimed,
        clock: Arc<dyn awaken_run_ingress::Clock>,
    ) -> Result<Option<(RunId, RunState)>, awaken_run_ingress::Error> {
        let host = self.host()?;
        let thread_id = claimed.request.session_thread_id();
        let commit = Arc::new(
            host.build_commit(&thread_id.0)
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?,
        );
        let worker = self.boundary_worker(&host, claimed, commit, false).await?;
        worker.terminalize_retry_exhausted(claimed, clock).await
    }

    async fn reconcile_committed_terminals(
        &self,
        clock: Arc<dyn awaken_run_ingress::Clock>,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, awaken_run_ingress::Error> {
        crate::host::terminal_reconciliation::reconcile_committed_terminals(self, clock, limit)
            .await
    }
}
