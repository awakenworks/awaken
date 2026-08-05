//! Claim-bound dispatch realization and worker selection.

use super::*;

pub(super) struct WorkerProjectionSynchronizer<'a> {
    pub(super) host: &'a SharedHost,
    pub(super) claim: Option<&'a awaken_run_ingress::RunClaim>,
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

pub(super) struct WorkerMcpEffects<'a>(pub(super) &'a SharedHost);

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
        let agent_id = claimed.request.activation.snapshot.root_agent_id.0.as_str();
        let agent_id = (!agent_id.is_empty()).then_some(agent_id);
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
            let (environment, toolsets, mcp_stages) =
                crate::provisioning::decode_session_runtime_envelope(envelope).map_err(
                    |error| {
                        Self::execution_error(format!(
                            "run {} has an invalid Session runtime projection: {error}",
                            claimed.lease.run_id.0
                        ))
                    },
                )?;
            host.install_environment_projection(&thread_id.0, &environment)
                .map_err(|error| Self::execution_error(error.to_string()))?;
            host.session_slots
                .update(&thread_id.0, |slot| slot.toolsets = toolsets);
            mcp_stages
        } else {
            None
        };
        install_application_projection(&host, claimed, thread_id, dispatched_resources.as_ref())
            .await?;
        if let Some(stages) = dispatched_mcp_stages {
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
            Self::reconcile_dispatched_mcp(&host, &thread_id.0, stages).await?;
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

    async fn reconcile_committed_terminals(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, awaken_run_ingress::Error> {
        crate::host::terminal_reconciliation::reconcile_committed_terminals(self, now_ms, limit)
            .await
    }
}
