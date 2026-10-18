// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
//! Claim-bound dispatch realization and worker selection.

use super::{
    AnyDispatchStore, Arc, ClaimedExecution, DispatchWorker, Error, RunId, RunMcpAttachments,
    RunState, SessionWorkerResolver, WorkerResolver, execution_error, map_admission_error,
    terminal_resolution_error,
};

use awaken_session_configuration_contract::SessionRuntimeProjection;

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

/// The frozen projection a claimed dispatch carries. Bytes that do not decode
/// cannot decode on a retry either, so this is an absorbing resolution failure.
fn decode_dispatched_projection(
    envelope: &awaken_dispatch_contract::SessionRuntimeEnvelope,
    run_id: &awaken_agent_contract::agent::run::Id,
) -> Result<SessionRuntimeProjection, Error> {
    envelope
        .decode_with(SessionRuntimeProjection::decode)
        .map_err(|error| {
            terminal_resolution_error(format!(
                "run {} has an invalid Session runtime projection: {error}",
                run_id.0
            ))
        })
}

/// Stage and publish a compatibility dispatch's MCP generations, draining the
/// ones it no longer carries; a failed stage drains what this call staged.
pub async fn reconcile_dispatched_mcp(
    effects: &dyn RunMcpAttachments,
    session_id: &str,
    stages: Vec<awaken_session_configuration_contract::mcp::StageMcpAttachment>,
) -> Result<(), Error> {
    {
        let existing = effects
            .active_stages(session_id)
            .map_err(|error| execution_error(format!("dispatched MCP lookup failed: {error}")))?;
        let existing_generations = existing
            .iter()
            .map(|projection| awaken_agent_contract::stable_fingerprint(&projection.generation))
            .collect::<std::collections::BTreeSet<_>>();
        let mut desired_generations = std::collections::BTreeSet::new();
        let mut desired_names = std::collections::BTreeSet::new();
        for stage in &stages {
            if stage.generation.session_id() != session_id
                || !desired_generations
                    .insert(awaken_agent_contract::stable_fingerprint(&stage.generation))
                || !desired_names.insert(stage.name.clone())
            {
                return Err(execution_error(
                    "dispatched MCP projection has a foreign or duplicate generation/name",
                ));
            }
        }

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
                        return Err(execution_error(format!(
                            "dispatched MCP publication failed: {error}"
                        )));
                    }
                }
                Err(error) => {
                    for generation in newly_staged {
                        let _ = effects.drain_mcp_generation(generation).await;
                    }
                    return Err(execution_error(format!(
                        "dispatched MCP staging failed: {error}"
                    )));
                }
            }
        }
        for projection in existing {
            if !desired_generations.contains(&awaken_agent_contract::stable_fingerprint(
                &projection.generation,
            )) {
                effects
                    .drain_mcp_generation(projection.generation)
                    .await
                    .map_err(|error| {
                        execution_error(format!("obsolete dispatched MCP drain failed: {error}"))
                    })?;
            }
        }
        Ok(())
    }
}

impl SessionWorkerResolver {
    /// Install and publish only the admitted claim's canonical projection.
    async fn install_claimed_dispatch(
        &self,
        execution: &ClaimedExecution,
        claimed: &awaken_dispatch_contract::Claimed,
    ) -> Result<
        (
            bool,
            Option<Vec<awaken_session_configuration_contract::mcp::StageMcpAttachment>>,
        ),
        Error,
    > {
        let installation = execution.installation.as_ref();
        let thread_id = claimed.request.session_thread_id();
        let run_thread_id = claimed.request.thread_id();
        let has_authority = super::claimed_session::has_session_projection_authority(claimed);
        let dispatched_resources =
            crate::session_envelope::decode_dispatched_resource_manifest(&claimed.request)
                .map_err(execution_error)?;
        let dispatched_mcp_stages = if let Some(envelope) = &claimed.request.session_runtime {
            let projection = decode_dispatched_projection(envelope, claimed.lease.run_id())?;
            let stages = projection.mcp_stages.clone();
            execution
                .preparation
                .install_runtime_projection(&thread_id.0, projection)
                .map_err(map_admission_error)?;
            stages
        } else {
            None
        };
        self.install_claimed_session_projection(
            execution,
            claimed,
            thread_id,
            dispatched_resources.as_ref(),
        )
        .await?;
        let Some(observed) = installation
            .observe_projection(&thread_id.0)
            .map_err(map_admission_error)?
        else {
            return Err(Error::ResolutionNotReady(
                format!(
                    "run {} lost its installed Session before claim binding",
                    claimed.lease.run_id().0
                )
                .into(),
            ));
        };
        let canonical_session_realization = canonical_session_projection_is_installed(
            has_authority,
            observed.session_dispatch,
            observed.has_frozen_baseline,
        );
        if has_authority && !canonical_session_realization {
            return Err(Error::ResolutionNotReady(
                format!(
                    "run {} lost its canonical Session projection before MCP effects",
                    claimed.lease.run_id().0
                )
                .into(),
            ));
        }
        let claim = awaken_dispatch_contract::RunClaim::from(&claimed.lease);
        // A failed/NotReady resolve must not publish an attempt claim into the
        // shared Session projection: lifecycle recovery is independently owned
        // by its realization lease. Once the complete projection is installed,
        // the self-affine root synchronizes its aggregate-owned durable Sandbox
        // into the queue's existing delivery cache under this exact claim.
        if canonical_session_realization
            && run_thread_id == thread_id
            && let Some(binding) = installation
                .durable_environment_binding(&thread_id.0)
                .map_err(map_admission_error)?
        {
            let outcome = self.queue.bind_sandbox(&claim, &binding).await?;
            if !outcome.applied() {
                return Err(execution_error(format!(
                    "run {} lost ownership before Session Sandbox cache synchronization",
                    claimed.lease.run_id().0
                )));
            }
        }
        if !installation
            .bind_claim_if_projection_unchanged(
                &thread_id.0,
                &observed.installation,
                has_authority,
                claim,
            )
            .map_err(map_admission_error)?
        {
            return Err(Error::ResolutionNotReady(
                format!(
                    "run {} lost its original Session projection before claim binding",
                    claimed.lease.run_id().0
                )
                .into(),
            ));
        }
        Ok((canonical_session_realization, dispatched_mcp_stages))
    }

    /// Keep physical admission before MCP effects and before root/child plugins.
    async fn prepare_claimed_environment(
        execution: &ClaimedExecution,
        claimed: &awaken_dispatch_contract::Claimed,
        canonical_session_realization: bool,
        dispatched_mcp_stages: Option<
            &[awaken_session_configuration_contract::mcp::StageMcpAttachment],
        >,
    ) -> Result<(), Error> {
        let thread_id = claimed.request.session_thread_id();
        let run_thread_id = claimed.request.thread_id();
        execution
            .preparation
            .bind_deferred_executor(&thread_id.0)
            .map_err(map_admission_error)?;
        // The Control synchronizer merged projection + claim binding before
        // applying the Resource transition. Re-adopting the stale claim handle
        // here would create a second owner and rejects a legitimate
        // RebuildFromCommittedTruth replacement in the same resolve.
        if !canonical_session_realization {
            execution
                .installation
                .adopt_bound_environment(
                    &thread_id.0,
                    claimed.sandbox.as_deref(),
                    claimed.lease.run_id(),
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
        }
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
            execution
                .preparation
                .ensure_session_substrate(&thread_id.0, frozen_parent)
                .await
                .map_err(map_admission_error)?;
        }
        Ok(())
    }

    /// An Environment-free Worker at the claim's commit boundary.
    async fn boundary_worker(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
        install_cancellation_executor: bool,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error> {
        self.commits
            .open_commit(claimed.request.session_thread_id())
            .await?
            .boundary_worker(claimed, None, install_cancellation_executor)
            .await
    }
}

#[async_trait::async_trait]
impl WorkerResolver<AnyDispatchStore> for SessionWorkerResolver {
    fn dispatch_store(&self) -> Result<Arc<AnyDispatchStore>, Error> {
        Ok(self.queue.clone())
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        self.execution
            .as_ref()
            .map(|execution| execution.runs.credential_capabilities())
            .unwrap_or_default()
    }

    async fn worker_for(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        agent_id: Option<&str>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        self.execution()?
            .runs
            .root_worker(
                thread_id,
                agent_id.filter(|id| !id.is_empty()),
                None,
                None,
                None,
            )
            .await
    }

    async fn worker_for_claimed(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        let execution = self.execution()?;
        // Reservation repair owns no executable Run yet. It needs only the
        // parent commit boundary, queue fence, and Session admission observer;
        // resolving publications, credentials, Sandbox, MCP, or Environment
        // here can both block repair and create effects before admission.
        if claimed.session_activity_admission_required {
            return Ok(Arc::new(self.boundary_worker(claimed, false).await?));
        }
        if claimed.cancellation_requested {
            return Ok(Arc::new(self.boundary_worker(claimed, true).await?));
        }
        let installation = execution.installation.as_ref();
        let thread_id = claimed.request.session_thread_id();
        let run_thread_id = claimed.request.thread_id();
        let agent_id = claimed.request.activation.snapshot.root_agent_id.0.as_str();
        let agent_id = (!agent_id.is_empty()).then_some(agent_id);
        let publication_workspace = claimed.request.execution_scope.as_ref().map_or_else(
            || installation.local_workspace(),
            |scope| scope.0.0.as_str(),
        );
        if let Some(scope) = &claimed.request.execution_scope
            && !installation
                .bind_execution_workspace(&thread_id.0, scope.0.as_str())
                .map_err(map_admission_error)?
        {
            return Err(execution_error(
                "claimed Thread Workspace conflicts with its dispatch",
            ));
        }
        let claimed_source = execution
            .publication_closure
            .exact_source(
                &claimed.request.activation.snapshot,
                &claimed.request.agent_publications,
                publication_workspace,
            )
            .map_err(|error| {
                execution_error(format!(
                    "run {} has an invalid Agent publication closure: {error}",
                    claimed.lease.run_id().0
                ))
            })?;
        let (canonical_session_realization, dispatched_mcp_stages) =
            self.install_claimed_dispatch(execution, claimed).await?;
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
            installation
                .pin_dispatched_identity(
                    &thread_id.0,
                    agent_id.unwrap_or("assistant"),
                    claimed
                        .request
                        .activation
                        .snapshot
                        .resolved_spec
                        .model_binding
                        .identity()
                        .2,
                    stages.first().map(|stage| stage.workspace_id.as_str()),
                )
                .map_err(map_admission_error)?;
        }
        Self::prepare_claimed_environment(
            execution,
            claimed,
            canonical_session_realization,
            dispatched_mcp_stages.as_deref(),
        )
        .await?;
        if let Some(stages) = dispatched_mcp_stages {
            reconcile_dispatched_mcp(execution.mcp.as_ref(), &thread_id.0, stages).await?;
        }
        if run_thread_id != thread_id {
            return execution
                .runs
                .child_worker(claimed, thread_id, claimed_source)
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
            installation
                .published_snapshot(&thread_id.0)
                .map_err(map_admission_error)?
                .unwrap_or_else(|| claimed.request.activation.snapshot.clone())
        } else {
            claimed.request.activation.snapshot.clone()
        };
        execution
            .runs
            .root_worker(
                thread_id,
                agent_id,
                Some(immutable_publication),
                Some(&claimed.request),
                Some(claimed_source),
            )
            .await
    }

    async fn settle_claimed_resolution_failure(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
        error: Error,
    ) -> Result<Option<(RunId, RunState)>, Error> {
        self.boundary_worker(claimed, false)
            .await?
            .fail_claimed_before_execution(claimed, "dispatch_resolution_failed", error.to_string())
            .await
    }

    async fn worker_for_retry_exhausted(
        &self,
        claimed: &awaken_dispatch_contract::Claimed,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        Ok(Arc::new(self.boundary_worker(claimed, false).await?))
    }

    async fn reconcile_committed_terminals(
        &self,
        limit: usize,
    ) -> Result<Vec<(RunId, RunState)>, Error> {
        self.reconcile_committed_terminal_runs(limit).await
    }

    async fn reconcile_worker_evidence(&self) -> Result<(), Error> {
        self.release_closed_worker_evidence().await
    }
}

use awaken_dispatch_contract::DispatchExecution as _;

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
