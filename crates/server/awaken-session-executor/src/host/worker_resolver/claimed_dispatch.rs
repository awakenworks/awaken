// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
//! Claim-bound dispatch realization and worker selection.

use super::session_realization::WorkerMcpEffects;
use super::*;

/// Project the already-installed frozen Session facts into claimed-dispatch
/// ownership. The presence of a remote Control client is a transport detail:
/// all-in-one execution installs the same complete projection locally.
const fn canonical_session_projection_is_installed(
    has_session_projection_authority: bool,
    session_dispatch: bool,
    has_frozen_baseline: bool,
) -> bool {
    has_session_projection_authority && session_dispatch && has_frozen_baseline
}

pub(super) async fn adopt_bound_sandbox(
    host: &SharedHost,
    encoded: Option<&str>,
    expected_sandbox_id: &str,
    run_id: &RunId,
    provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    recovery: awaken_dispatch_contract::WorkerRecoveryMode,
) -> Result<
    crate::host::session::SessionEnvironmentAdoptionDisposition,
    awaken_dispatch_execution::Error,
> {
    let provider = host
        .session_environment_provider_for_thread(expected_sandbox_id, provisioning)
        .map_err(HostWorkerResolver::map_host_resolution_error)?;
    host.adopt_bound_session_environment(
        expected_sandbox_id,
        encoded,
        provider,
        None,
        recovery == awaken_dispatch_contract::WorkerRecoveryMode::RebuildFromCommittedTruth,
    )
    .await
    .map_err(|mut error| {
        error.message = format!("run {}: {}", run_id.0, error.message);
        HostWorkerResolver::map_host_resolution_error(error)
    })
}

impl HostWorkerResolver {
    async fn recovered_child_worker(
        &self,
        host: &SharedHost,
        claimed: &awaken_dispatch_contract::Claimed,
        session_thread_id: &awaken_agent_contract::agent::thread::Id,
        publication_source: Arc<awaken_runtime_contract::StaticPublishedAgentSnapshots>,
    ) -> Result<
        Arc<awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>>,
        awaken_dispatch_execution::Error,
    > {
        crate::agent_runner::validate_child_tool_configuration(
            &claimed.request.activation.snapshot,
            claimed.request.activation.inherited_toolsets(),
        )
        .map_err(|error| Self::execution_error(error.to_string()))?;
        let substrate = host
            .session_child_execution_substrate(&session_thread_id.0, None)
            .await
            .map_err(Self::map_host_resolution_error)?;
        let environment = substrate.environment;
        let snapshot = &claimed.request.activation.snapshot;
        let authorization = crate::config::effective_tool_authorization(
            &snapshot.resolved_spec.plugin_config,
            &[],
            &snapshot.resolved_spec.plugin_config.agent.toolsets,
        );
        let runtime = crate::config::build_runtime_with_authorization(
            host.agent.llm.clone(),
            environment.as_ref(),
            &authorization,
            Some(host.background_supervisor.clone()),
        );
        let commit = substrate.commit;
        let adapters = host.child_execution_adapters(&session_thread_id.0, environment.clone());
        let mut runtime = crate::agent_runner::configure_child_native_runtime(
            runtime,
            snapshot,
            claimed.request.activation.inherited_toolsets(),
            &adapters,
        )
        .map_err(|error| Self::execution_error(error.to_string()))?;
        if let Some(delegation) = host
            .run_delegation(
                &session_thread_id.0,
                environment.clone(),
                commit.clone(),
                Some(snapshot),
                publication_source,
                crate::delegate::CallerToolConfiguration {
                    toolsets: snapshot.resolved_spec.plugin_config.agent.toolsets.clone(),
                    inherited: claimed.request.activation.inherited_toolsets().cloned(),
                },
            )
            .map_err(Self::map_host_resolution_error)?
        {
            runtime = runtime.with_run_delegation(delegation);
        }
        let runtime = Arc::new(runtime);
        let attempt = crate::agent_runner::child_attempt_executor(
            runtime,
            snapshot,
            &adapters,
            authorization.policy,
        )
        .map_err(|error| Self::execution_error(error.to_string()))?;
        crate::agent_runner::ChildWorkerFactory::claimed_recovery(
            self,
            host,
            claimed,
            commit,
            session_thread_id,
            crate::agent_runner::ClaimedChildAttempt {
                executor: attempt,
                context: substrate.attempt_context,
                credentials: adapters.credential_capabilities,
            },
        )
        .await
    }

    pub(super) async fn reconcile_dispatched_mcp(
        host: &SharedHost,
        session_id: &str,
        stages: Vec<awaken_session_mcp_contract::StageMcpAttachment>,
    ) -> Result<(), awaken_dispatch_execution::Error> {
        use awaken_session_execution_contract::McpAttachmentRealizer as _;

        let existing = host.active_mcp_projections(session_id);
        let existing_generations = existing
            .iter()
            .map(|projection| {
                awaken_agent_contract::stable_fingerprint(&projection.request.generation)
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut desired_generations = std::collections::BTreeSet::new();
        let mut desired_names = std::collections::BTreeSet::new();
        for stage in &stages {
            if stage.generation.session_id() != session_id
                || !desired_generations
                    .insert(awaken_agent_contract::stable_fingerprint(&stage.generation))
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
                    if !existing_generations.contains(&awaken_agent_contract::stable_fingerprint(
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
            if !desired_generations.contains(&awaken_agent_contract::stable_fingerprint(
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
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        self.host
            .upgrade()
            .map(|host| host.local_credential_realization_capabilities())
            .unwrap_or_default()
    }

    async fn worker_for(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
    ) -> Result<
        Arc<awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>>,
        awaken_dispatch_execution::Error,
    > {
        let host = self.host()?;
        Self::require_local_execution(&host)?;
        self.resolve(&host, thread_id, agent_id.filter(|id| !id.is_empty()), None)
            .await
    }

    async fn worker_for_claimed(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
    ) -> Result<
        Arc<awaken_dispatch_execution::DispatchWorker<AnyDispatchStore>>,
        awaken_dispatch_execution::Error,
    > {
        let host = self.host()?;
        Self::require_local_execution(&host)?;
        // Reservation repair owns no executable Run yet. It needs only the
        // parent commit boundary, queue fence, and Session admission observer;
        // resolving publications, credentials, Sandbox, MCP, or Environment
        // here can both block repair and create effects before admission.
        if claimed.session_activity_admission_required {
            let thread_id = claimed.request.session_thread_id();
            let commit = Arc::new(
                host.authority
                    .open_commit(&thread_id.0)
                    .await
                    .map_err(|error| Self::execution_error(error.to_string()))?,
            );
            return Ok(Arc::new(
                self.boundary_worker(&host, claimed, commit, None, false)
                    .await?,
            ));
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
        if let Some(scope) = &claimed.request.execution_scope {
            if host
                .registered_thread_workspace(&thread_id.0)
                .is_some_and(|existing| existing != scope.0.as_str())
            {
                return Err(Self::execution_error(
                    "claimed Thread Workspace conflicts with its dispatch",
                ));
            }
            host.register_thread_workspace(&thread_id.0, scope.0.as_str());
        }
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
                .map_err(Self::map_host_resolution_error)?;
            host.session_slots
                .replace_frozen_with(&thread_id.0, |next| next.tools = projection.tools);
            projection.mcp_stages
        } else {
            None
        };
        install_claimed_session_projection(
            &host,
            claimed,
            thread_id,
            dispatched_resources.as_ref(),
        )
        .await?;
        let canonical_session_realization = host
            .session_slots
            .read(&thread_id.0, |slot| {
                canonical_session_projection_is_installed(
                    super::claimed_session::has_session_projection_authority(claimed),
                    slot.run.session_dispatch,
                    slot.frozen.baseline.is_some(),
                )
            })
            .unwrap_or(false);
        let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
        // A failed/NotReady resolve must not publish an attempt claim into the
        // shared Session projection: lifecycle recovery is independently owned
        // by its realization lease. Once the complete projection is installed,
        // the self-affine root synchronizes its aggregate-owned durable Sandbox
        // into the queue's existing delivery cache under this exact claim.
        if canonical_session_realization
            && run_thread_id == thread_id
            && let Some(binding) = host.durable_session_environment_binding(&thread_id.0)
        {
            let outcome = host
                .dispatch_store()
                .map_err(|error| Self::execution_error(error.to_string()))?
                .bind_sandbox(&claim, &binding)
                .await?;
            if !outcome.applied() {
                return Err(Self::execution_error(format!(
                    "run {} lost ownership before Session Sandbox cache synchronization",
                    claimed.lease.run_id.0
                )));
            }
        }
        host.session_slots.update_run(&thread_id.0, |run| {
            run.dispatch_claim = Some(claim);
        });
        // A complete frozen Session projection owns MCP realization regardless
        // of whether it arrived through remote Control or was installed by the
        // co-located Runtime. The dispatch envelope remains a compatibility
        // carrier only for a partial/ordinary slot; replaying it for a canonical
        // slot would advance a second process-local generation path.
        let dispatched_mcp_stages = if canonical_session_realization {
            None
        } else {
            dispatched_mcp_stages
        };
        if let Some(stages) = dispatched_mcp_stages.as_ref() {
            host.register_thread_agent_projection(&thread_id.0, agent_id.unwrap_or("assistant"));
            host.register_thread_backend_projection(
                &thread_id.0,
                claimed
                    .request
                    .activation
                    .snapshot
                    .resolved_spec
                    .model_binding
                    .identity()
                    .2,
            );
            if let Some(workspace) = stages.first().map(|stage| stage.workspace_id.as_str()) {
                host.register_thread_workspace(&thread_id.0, workspace);
            }
        }
        let needs_deferred_executor = host
            .session_slots
            .read(&thread_id.0, |slot| {
                let environment = slot.frozen.environment_projection.as_ref();
                slot.run.deferred_executor.is_none()
                    && environment.is_some_and(|environment| {
                        environment.provisioning
                            == awaken_session_configuration_contract::SandboxProvisioning::OnToolUse
                    })
            })
            .unwrap_or(false);
        if needs_deferred_executor {
            let executor: Arc<dyn awaken_tool_contract::tool::ToolExecutor> =
                Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
                    Arc::downgrade(&host),
                    &thread_id.0,
                ));
            host.session_slots
                .update_run(&thread_id.0, |run| run.deferred_executor = Some(executor));
        }
        let _adoption = if canonical_session_realization {
            // The Control synchronizer merged projection + claim binding before
            // applying the Resource transition. Re-adopting the stale claim
            // handle here would create a second owner and rejects a legitimate
            // RebuildFromCommittedTruth replacement in the same resolve.
            crate::host::session::SessionEnvironmentAdoptionDisposition::NoBinding
        } else {
            adopt_bound_sandbox(
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
            .await?
        };
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
            host.session_child_execution_substrate(&thread_id.0, frozen_parent)
                .await
                .map_err(Self::map_host_resolution_error)?;
        }
        if let Some(stages) = dispatched_mcp_stages {
            Self::reconcile_dispatched_mcp(&host, &thread_id.0, stages).await?;
        }
        if run_thread_id != thread_id {
            return self
                .recovered_child_worker(&host, claimed, thread_id, claimed_source)
                .await;
        }
        // Complete Session projection installation has already validated the
        // delivered Run coordinates and retained the immutable publication in
        // the root slot. That projection may be installed by a co-located
        // Runtime or through remote Control; transport presence is not an
        // authority fact. The Run snapshot is a derived execution clone:
        // Managed coordination and Session tool projection may legitimately
        // change its descriptors and fingerprint. Reuse the retained
        // publication for immutable selection while `claimed_runtime_input`
        // continues to bind Runtime cache/plugins to the exact delivered
        // execution closure. Compatibility dispatch without a complete frozen
        // projection still uses the delivered root as its sole authority.
        let immutable_publication = if canonical_session_realization {
            host.session_slots
                .published_snapshot(&thread_id.0)
                .unwrap_or_else(|| claimed.request.activation.snapshot.clone())
        } else {
            claimed.request.activation.snapshot.clone()
        };
        let worker = self
            .resolve_claimed(
                &host,
                thread_id,
                agent_id,
                immutable_publication,
                claimed_runtime_input,
            )
            .await?;
        Ok(worker)
    }

    async fn settle_claimed_resolution_failure(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
        error: awaken_dispatch_execution::Error,
    ) -> Result<Option<(RunId, RunState)>, awaken_dispatch_execution::Error> {
        let host = self.host()?;
        let thread_id = claimed.request.session_thread_id();
        let commit = Arc::new(
            host.authority
                .open_commit(&thread_id.0)
                .await
                .map_err(|commit_error| Self::execution_error(commit_error.to_string()))?,
        );
        let worker = self
            .boundary_worker(&host, claimed, commit, None, false)
            .await?;
        worker
            .fail_claimed_before_execution(claimed, "dispatch_resolution_failed", error.to_string())
            .await
    }

    async fn terminalize_retry_exhausted(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
    ) -> Result<Option<(RunId, RunState)>, awaken_dispatch_execution::Error> {
        let host = self.host()?;
        let thread_id = claimed.request.session_thread_id();
        let commit = Arc::new(
            host.authority
                .open_commit(&thread_id.0)
                .await
                .map_err(|error| Self::execution_error(error.to_string()))?,
        );
        let worker = self
            .boundary_worker(&host, claimed, commit, None, false)
            .await?;
        worker.terminalize_retry_exhausted(claimed).await
    }

    async fn reconcile_committed_terminals(
        &self,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, awaken_dispatch_execution::Error> {
        crate::host::terminal_reconciliation::reconcile_committed_terminals(self, limit).await
    }

    async fn reconcile_worker_evidence(&self) -> Result<(), awaken_dispatch_execution::Error> {
        self.host()?.reconcile_worker_evidence().await
    }
}

#[cfg(test)]
mod canonical_projection_tests {
    use super::canonical_session_projection_is_installed;

    #[test]
    fn claimed_dispatch_uses_only_a_complete_session_projection() {
        // Cause/effect graph: C1 the claim has explicit Session affinity or a Coordinator Work receipt;
        // C2 the canonical Dispatch projection was installed; C3 its frozen
        // baseline is present. E1 the retained immutable publication selects
        // the Session, while E2 the compatibility envelope remains the sole
        // input. The three facts are conjunctive so a partial or ordinary slot
        // can never acquire canonical ownership by transport inference.
        //
        // | Rule | C1 authority | C2 dispatch | C3 baseline | Effect |
        // | P1 | yes | yes | yes | E1 canonical projection |
        // | P2 | no  | yes | yes | E2 ordinary dispatch |
        // | P3 | yes | no  | yes | E2 partial projection |
        // | P4 | yes | yes | no  | E2 partial projection |
        for (rule, affinity, dispatch, baseline, expected) in [
            ("P1", true, true, true, true),
            ("P2", false, true, true, false),
            ("P3", true, false, true, false),
            ("P4", true, true, false, false),
        ] {
            assert_eq!(
                canonical_session_projection_is_installed(affinity, dispatch, baseline),
                expected,
                "{rule}"
            );
        }
    }
}
