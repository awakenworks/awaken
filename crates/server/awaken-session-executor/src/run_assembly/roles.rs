// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the Session application's realization, Environment and terminal-cleanup roles
// @formal reason: served by RunAssembly. Each method installs projection facts or composes the realization
// @formal reason: executor's effect with the context, Memory, MCP and delegated-Run facts it reads; the
// @formal reason: realization decisions stay in the realization executor and the Session aggregate.
//! The realization, Environment and terminal-cleanup roles of the Session
//! application, served by [`RunAssembly`]: the realization executor performs
//! the effect, this component supplies the Session context facts around it.

use awaken_session_types::RunError;

use super::RunAssembly;
use crate::session_execution_error::to_run_error;
use awaken_session_configuration_contract::skill_delivery::skill_store_run_error;

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RunAssembly {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        awaken_authority_clock::authority_proof_started()
            .map_err(|error| RunError::unavailable(format!("authority clock: {error}")))
    }

    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, RunError> {
        self.renew_session_realization_projections(
            Some(control),
            Some((owner, runtime_incarnation)),
            timing,
        )
        .await
        .map_err(to_run_error)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        let realizations = self.installed_sessions.realizations();
        // Installing the frozen projection is the install path. Every
        // Environment-owner projection, including lease-only recovery, crosses
        // the same lifecycle guard as physical publication/retirement.
        self.installed_sessions
            .install(thread)
            .map_err(crate::installed_session::registry_run_error)?;
        let _lifecycle = realizations.lock_lifecycle(thread).await?;
        if mode.prepares_session() {
            // Cause/effect rule P2: an unprotected active Runtime rejects before
            // any frozen coordinate is published. The same lifecycle mutex also
            // prevents a peer from observing installed facts before the Managed
            // execution marker and deferred executor are complete.
            let preparation_needed = self.session_preparation_needed(thread)?;
            self.install_projection_facts(thread, &projection, &mode)
                .await?;
            if preparation_needed {
                self.complete_session_preparation(thread)?;
            }
        } else {
            self.install_projection_facts(thread, &projection, &mode)
                .await?;
        }
        drop(_lifecycle);
        if mode.adopts_resident_environment()
            && let Some(binding) = projection.environment.binding()
        {
            adopt_environment_for_realization(self, &projection.baseline.agent_id, thread, binding)
                .await?;
        }
        Ok(())
    }

    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        let realizations = self.installed_sessions.realizations();
        let missing = || {
            RunError::classified(
                "session_realization_lease_missing",
                "cannot renew a Session realization lease that is not installed",
            )
        };
        self.realization_executor.authority()
            .authorize_worker_effect(|| {
                Ok::<_, crate::realization_error::RealizationError>(realizations
            .modify_realization(thread, |r| {
                let current = r.execution().authority.lease().cloned().ok_or_else(missing)?;
                if !r.renew_lease(&current, lease) {
                    return Err(RunError::classified(
                        "session_realization_lease_conflict",
                        "Session realization renewal is not a monotonic successor of the installed fence",
                    ));
                }
                Ok(())
            })?
            .unwrap_or_else(|| Err(missing())))
            })
            .map_err(|error: crate::realization_error::RealizationError| RunError::from(error))?
    }

    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), RunError> {
        self.invalidate_session_realization_lease(thread, lease)
            .map_err(RunError::from)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for RunAssembly {
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), RunError> {
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.tools = Some(tools))
            .map_err(crate::installed_session::registry_run_error)?;
        self.realization_executor
            .environments()
            .evict_session_for_rebuild(thread)
            .await?;
        Ok(())
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        adopt_environment_for_realization(self, agent, thread, binding)
            .await
            .map_err(RunError::from)
    }

    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
        expected_mcp_generations: &[crate::session_mcp::McpGenerationRef],
    ) -> Result<awaken_session_state_contract::QuiescenceReceipt, RunError> {
        self.quiesce_environment_continuation(
            thread,
            operation,
            source_effect_id,
            source_binding,
            generation,
            expected_mcp_generations,
        )
        .await
    }

    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<crate::session_resource::ResolvedSkillBinding>, RunError> {
        if skills.is_empty() {
            return Ok(Vec::new());
        }
        self.capabilities
            .tools
            .skills
            .resolve(workspace_id, skills)
            .await
            .map_err(skill_store_run_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for RunAssembly {
    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        self.realization_executor
            .effects()
            .prepare_terminal_cleanup_effect(
                self.realization_executor.peers(),
                effect,
                authorization,
                |thread, lifecycle| {
                    let delegated = self.commit_source.delegated_runs_for_cleanup(
                        &self.thread_reads,
                        thread,
                        lifecycle,
                    );
                    async move {
                        delegated
                            .await
                            .map_err(crate::realization_error::RealizationError::from)
                    }
                },
                |thread, generation| {
                    let background = self.resources.memory.background();
                    async move {
                        background
                            .quiesce_shared_environment(
                                &thread,
                                &generation,
                                std::time::Duration::from_secs(30),
                            )
                            .await
                    }
                },
                (&self.mcp, self.installed_sessions.as_ref()),
            )
            .await
            .map_err(RunError::from)
    }

    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        self.realization_executor
            .effects()
            .acknowledge_terminal_cleanup_preparation(&self.mcp, effect)
            .await;
    }

    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        self.realization_executor
            .effects()
            .acknowledge_terminal_cleanup_disposal(&self.mcp, effect)
            .await;
    }

    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &crate::session_resource::SessionRealizationLease,
    ) {
        // Forgetting a settled local projection is best effort: a denied
        // retirement leaves it for the next completed-cleanup readback.
        if let Err(error) = self
            .realization_executor
            .effects()
            .retire_completed_terminal_cleanup_projection(&self.mcp, session_id.as_str(), lease)
            .await
        {
            tracing::warn!(%error, "completed terminal cleanup projection retained");
        }
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        self.install_terminal_cleanup_projection(assignment, proof)
            .await
            .map_err(to_run_error)
    }
}

// Both existing role entrances use this one adoption/context body. The
// realization entrance keeps the provider hint; the Environment role's
// existing neutral result is its explicit non-scheduling projection.
async fn adopt_environment_for_realization(
    assembly: &RunAssembly,
    agent: &str,
    thread: &str,
    binding: &str,
) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
    // Managed Session restoration is a continuity contract: the durable
    // binding must decode, belong to this Session/provider, and still name a
    // ready physical environment. Run-dispatch recovery has its own explicit
    // RebuildFromCommittedTruth policy; applying that fallback here would
    // turn corrupt or deleted Session authority into a replacement sandbox.
    let provider = assembly
        .realization_executor
        .environments()
        .projected_session_environment_provider(thread, Some(agent))?;
    let disposition = assembly
        .realization_executor
        .adopt_bound_session_environment(thread, Some(binding), provider, None, false)
        .await?;
    if disposition != crate::session_environment::SessionEnvironmentAdoptionDisposition::Ready {
        return Err(RunError::internal(
            "durable Session Environment adoption did not publish a ready binding",
        )
        .into());
    }
    assembly
        .ctx_for(thread, Some(agent))
        .await
        .map_err(to_run_error)?;
    Ok(())
}
