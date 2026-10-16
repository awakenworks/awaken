// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Composition-only adapter from dispatch-owned routing to existing Run assembly and Session projections.
//! Injects the existing Run assembly into dispatch execution, without routing policy.
use super::*;
use awaken_dispatch_execution::session_execution::{
    RunWorkerFactory, SessionDispatch, TerminalRecovery, execution_error,
};
use awaken_dispatch_execution::session_worker_resolver::map_host_resolution_error;
use awaken_dispatch_execution::{DispatchBinding, DispatchWorker, Error};
use awaken_dispatch_contract::{Claimed, RunClaim, RunDispatch};
use awaken_runtime_contract::{
    ExecutableAgentSnapshot, RuntimeRunContext, StaticPublishedAgentSnapshots,
};
use awaken_session_realization::worker_projection::SessionProjectionExecution;
use awaken_session_realization_contract::{
    FrozenSessionProjection, ObservedSessionRealizationLease,
};

#[async_trait::async_trait]
impl SessionDispatch for SharedHost {
    async fn drive_claimed_projection(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        session: &awaken_session_types::SessionId,
        directive: awaken_session_realization_contract::SessionRealizationDirective,
        claimed: &Claimed,
        started: awaken_lease_contract::authority::AuthorityProofStarted,
        permit: &awaken_session_realization_contract::SessionRealizationPermit,
    ) -> Result<(), Error> {
        crate::application::worker_projection::drive_session_realization(
            self,
            control,
            session,
            directive,
            Some(claimed),
            started,
            permit,
        )
        .await
        .map_err(
            awaken_dispatch_execution::session_worker_resolver::map_session_realization_drive_error,
        )
    }
    fn install_runtime_projection(
        &self,
        thread: &str,
        projection: awaken_session_configuration_contract::SessionRuntimeProjection,
    ) -> Result<(), HostError> {
        self.install_environment_projection(thread, &projection.environment)?;
        self.installed_sessions
            .replace_frozen_with(thread, |next| next.tools = projection.tools);
        Ok(())
    }

    fn dispatch_binding(&self) -> Option<DispatchBinding> {
        self.authority.dispatch_binding()
    }
    fn runs_local_dispatch_pool(&self) -> bool {
        SharedHost::runs_local_dispatch_pool(self)
    }
    fn credential_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        self.local_credential_realization_capabilities()
    }
    fn session_control(
        &self,
    ) -> Option<&dyn awaken_session_realization_contract::ClaimedSessionRealizationControl> {
        self.worker.session_control.as_deref()
    }
    fn terminal_reconciliation(
        &self,
    ) -> &awaken_dispatch_execution::session_reconciliation::TerminalReconciliationState {
        &self.dispatch.terminal_reconciliation
    }
    fn active_mcp_stages(
        &self,
        session: &str,
    ) -> Vec<awaken_session_configuration_contract::mcp::StageMcpAttachment> {
        self.active_mcp_projections(session)
            .into_iter()
            .map(|projection| projection.request)
            .collect()
    }
    fn dispatch_owner(&self) -> &str {
        SharedHost::dispatch_owner(self)
    }
}
#[async_trait::async_trait]
impl RunWorkerFactory for SharedHost {
    type Commit = RunRecoveryCommit;
    fn terminal_view<'a>(
        &self,
        commit: &'a Self::Commit,
    ) -> &'a dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView
    {
        commit.0.as_ref()
    }
    async fn open_commit(&self, thread: &ThreadId) -> Result<Arc<Self::Commit>, Error> {
        self.authority
            .open_commit(&thread.0)
            .await
            .map(|commit| Arc::new(RunRecoveryCommit(Arc::new(commit))))
            .map_err(|error| execution_error(error.to_string()))
    }
    async fn root_worker(
        &self,
        thread: &ThreadId,
        agent: Option<&str>,
        publication: Option<ExecutableAgentSnapshot>,
        dispatch: Option<&RunDispatch>,
        publications: Option<Arc<StaticPublishedAgentSnapshots>>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        let context = if let Some(dispatch) = dispatch {
            let effective_model_ref = dispatch.activation.effective_model_ref().to_owned();
            let input = ClaimedRuntimeInput {
                identity: RuntimePublicationIdentity::from_publications(
                    &dispatch.activation.snapshot,
                    &dispatch.agent_publications,
                    &effective_model_ref,
                ),
                publications: publications.ok_or_else(|| {
                    execution_error("claimed Run publication source is unavailable")
                })?,
                effective_model_ref,
            };
            self.ctx_for_claimed_snapshot(
                &thread.0,
                agent,
                publication
                    .ok_or_else(|| execution_error("claimed Run publication is unavailable"))?,
                input,
            )
            .await
        } else {
            self.ctx_for_snapshot(&thread.0, agent, publication).await
        };
        context
            .map(|ctx| ctx.claimed_worker.clone())
            .map_err(map_host_resolution_error)
    }
    async fn child_worker(
        &self,
        claimed: &Claimed,
        session_thread: &ThreadId,
        publications: Arc<StaticPublishedAgentSnapshots>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        super::claimed_run::construct_claimed_child(self, claimed, session_thread, publications)
            .await
    }
    async fn boundary_worker(
        &self,
        claimed: &Claimed,
        commit: Arc<Self::Commit>,
        context: Option<RuntimeRunContext>,
        install_cancellation_executor: bool,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error> {
        super::boundary_run::construct_boundary_worker(
            self,
            claimed,
            commit.0.clone(),
            context,
            install_cancellation_executor,
        )
        .await
    }
    fn ensure_deferred_executor(&self, thread: &str) {
        let needed = self.installed_sessions.read(thread, |slot| slot.run.deferred_executor.is_none() && slot.inputs.environment_projection.as_ref().is_some_and(|env| env.provisioning == awaken_session_configuration_contract::SandboxProvisioning::OnToolUse)).unwrap_or(false);
        if needed {
            let executor: Arc<dyn awaken_tool_contract::tool::ToolExecutor> =
                Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
                    self.dispatch.session_effects.host.clone(),
                    thread,
                ));
            self.installed_sessions
                .update_run(thread, |run| run.deferred_executor = Some(executor));
        }
    }
}
#[async_trait::async_trait]
impl SessionProjectionExecution for SharedHost {
    fn sessions(&self) -> crate::session_realizations::SessionRealizations {
        self.installed_sessions.realizations()
    }
    fn realizer(&self) -> &crate::realization_executor::RealizationExecutor {
        &self.realizer
    }
    async fn ensure_session_substrate(
        &self,
        thread: &str,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> Result<(), HostError> {
        self.session_child_execution_substrate(thread, publication)
            .await
            .map(|_| ())
    }

    async fn install_frozen_projection(
        &self,
        session: &str,
        projection: FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
        lease: Option<ObservedSessionRealizationLease>,
    ) -> Result<(), HostError> {
        self.install_frozen_session_projection(
            session,
            projection,
            claim,
            synchronize_resources,
            lease,
        )
        .await
    }
    fn invalidate_realization_lease(
        &self,
        session: &str,
        lease: &crate::session_resource::SessionRealizationLease,
    ) {
        self.invalidate_session_realization_lease(session, lease);
    }
    fn mcp_effects(&self) -> &dyn awaken_session_execution_contract::McpAttachmentRealizer {
        &self.dispatch.session_effects
    }
}
impl TerminalRecovery for SharedHost {
    fn scans_committed_terminals(&self) -> bool {
        self.authority.scans_committed_terminals()
    }
    fn coordinator_dispatch(&self) -> Option<Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>> {
        self.optional_dispatch_store()
    }
    fn worker_recovery_source(
        self: &Arc<Self>,
    ) -> Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource> {
        SharedHost::worker_recovery_source(self)
    }
}

/// Opaque reference to the original Thread commit boundary, used only during
/// one terminal candidate. It contains no cache, mutation, or new authority.
pub struct RunRecoveryCommit(Arc<crate::store::HostCommit>);
#[async_trait::async_trait]
impl awaken_agent_contract::thread::read::recovery::RunRecoverySource for RunRecoveryCommit {
    async fn recovery_snapshot(
        &self,
        thread: &ThreadId,
        run: &RunId,
    ) -> Result<
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        awaken_agent_contract::thread::read::recovery::RecoveryError,
    > {
        awaken_agent_contract::thread::read::recovery::RunRecoverySource::recovery_snapshot(
            self.0.as_ref(),
            thread,
            run,
        )
        .await
    }
}

impl awaken_session_realization_contract::SessionPreparationPlacement for SharedHost {
    fn requirements(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> Result<
        awaken_dispatch_contract::PlacementRequirements,
        awaken_session_realization_contract::SessionRealizationControlFailure,
    > {
        awaken_dispatch_execution::placement::session_preparation_requirements(session)
    }
}
