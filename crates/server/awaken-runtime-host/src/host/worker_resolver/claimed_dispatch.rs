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
    ) -> Result<Arc<awaken_run_ingress::DispatchWorker<AnyDispatchStore>>, awaken_run_ingress::Error>
    {
        let parent = host
            .ctx_for_snapshot_with_sandbox(&session_thread_id.0, None, None, adopted)
            .await
            .map_err(|error| Self::execution_error(error.to_string()))?;
        let environment = parent.env.clone().ok_or_else(|| {
            Self::execution_error("a delegated child recovery has no parent Session environment")
        })?;
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
        // The parent Session context owns the one hydrated commit/read boundary.
        // Reopening the same durable file here creates a parallel projection:
        // the child can commit through it, but the in-flight parent waiter cannot
        // observe that terminal state until another process restart.
        let commit = parent.commit.clone();
        if let Some(delegation) = host
            .run_delegation(
                &session_thread_id.0,
                environment.clone(),
                commit.clone(),
                Some(snapshot),
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
            web_search: Some(host.web_search_plugin(&session_thread_id.0)),
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
        worker = worker
            .with_context(parent.attempt_context.clone())
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
        let claimed_agent_publications = claimed.request.agent_publications.clone();
        let claimed_source = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new(
            claimed_agent_publications.clone(),
        )
        .map_err(|error| {
            Self::execution_error(format!(
                "run {} has invalid Agent publications: {error}",
                claimed.lease.run_id.0
            ))
        })?;
        let expected_publications = awaken_runtime_contract::freeze_delegation_publications(
            &claimed.request.activation.snapshot,
            Some(&claimed_source),
            publication_workspace,
        )
        .map_err(|error| {
            Self::execution_error(format!(
                "run {} has an incomplete Agent publication closure: {error}",
                claimed.lease.run_id.0
            ))
        })?;
        let fingerprints = |snapshots: &[awaken_runtime_contract::ExecutableAgentSnapshot]| {
            let mut values = snapshots
                .iter()
                .map(|snapshot| snapshot.fingerprint.0.clone())
                .collect::<Vec<_>>();
            values.sort_unstable();
            values
        };
        if fingerprints(&expected_publications) != fingerprints(&claimed_agent_publications) {
            return Err(Self::execution_error(format!(
                "run {} has an inexact Agent publication closure",
                claimed.lease.run_id.0
            )));
        }
        host.session_slots.update(&thread_id.0, |slot| {
            slot.agent_publications = claimed_agent_publications
        });
        let dispatched_resources = if let Some(envelope) = &claimed.request.session_resources {
            let scope = claimed
                .request
                .execution_scope
                .as_ref()
                .map(|scope| scope.0.0.as_str());
            if envelope.workspace_id.trim().is_empty()
                || scope != Some(envelope.workspace_id.as_str())
            {
                return Err(Self::execution_error(format!(
                    "run {} has a resource manifest outside its execution scope",
                    claimed.lease.run_id.0
                )));
            }
            Some(envelope.decode_manifest().map_err(|error| {
                Self::execution_error(format!(
                    "run {} has an invalid Session resource manifest: {error}",
                    claimed.lease.run_id.0
                ))
            })?)
        } else {
            None
        };
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
            &claimed
                .request
                .activation
                .snapshot
                .resolved_spec
                .model_binding
                .provisioning,
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
            // claimed Run's immutable snapshot is already the exact execution
            // authority, so open the Session with it before the generic MCP
            // realizer falls back to ctx_for() without a publication argument.
            // Publication invalidates this temporary Runtime projection below;
            // the final resolve rebuilds it with the now-active MCP generation.
            self.resolve(
                &host,
                thread_id,
                agent_id,
                Some(claimed.request.activation.snapshot.clone()),
                adopted.take(),
            )
            .await?;
        }
        if let Some(stages) = dispatched_mcp_stages {
            Self::reconcile_dispatched_mcp(&host, &thread_id.0, stages).await?;
        }
        if run_thread_id != thread_id {
            return self
                .recovered_child_worker(&host, claimed, thread_id, adopted.take())
                .await;
        }
        host.session_slots.update(&thread_id.0, |slot| {
            slot.dispatch_claim = Some(awaken_run_ingress::RunClaim::from(&claimed.lease));
        });
        let worker = self
            .resolve(
                &host,
                thread_id,
                agent_id,
                Some(claimed.request.activation.snapshot.clone()),
                adopted.take(),
            )
            .await?;
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
            .fail_claimed_before_execution(claimed, "dispatch_resolution_failed", error.to_string())
            .await
    }

    async fn reconcile_committed_terminals(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, awaken_run_ingress::Error> {
        crate::host::terminal_reconciliation::reconcile_committed_terminals(self, now_ms, limit)
            .await
    }
}
