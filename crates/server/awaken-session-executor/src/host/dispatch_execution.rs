// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Composition-only adapter from dispatch-owned routing to existing Run assembly and Session projections.
//! Composes this Host's roles into dispatch's claimed-Run resolver, without
//! routing policy: the topology chooses the resolver's construction once.
use super::*;
use awaken_dispatch_execution::session_execution::{
    ClaimedRunPreparation, ClaimedRunWorkers, ClaimedSessionInstallation, CommitBoundaryWorkers,
    ObservedProjection, SessionInstallation, ThreadCommitBoundary, execution_error,
};
use awaken_dispatch_execution::session_worker_resolver::{
    ClaimedExecution, CommittedTerminalScan, ProjectionAuthority, claim_outcome,
    map_admission_error,
};
use awaken_dispatch_execution::{AdmissionError, DispatchWorker, Error};
use awaken_dispatch_contract::{Claimed, RunClaim, RunDispatch};
use awaken_runtime_contract::{
    ExecutableAgentSnapshot, RuntimeRunContext, StaticPublishedAgentSnapshots,
};
use awaken_session_realization::realization_error::RealizationError;
use awaken_session_realization::worker_projection::SessionProjectionExecution;
use awaken_session_realization_contract::{
    FrozenSessionProjection, ObservedSessionRealizationLease,
};

/// The one construction of this Host's claimed-run resolver. The topology
/// selects the construction: a process running the local pool resolves claimed
/// execution; a process owning the Coordinator's committed truth scans
/// committed terminals; a co-located process does both.
pub(crate) fn session_worker_resolver(
    host: &Arc<SharedHost>,
) -> Result<SessionWorkerResolver, HostError> {
    let queue = host.dispatch_store()?;
    let clock = host.realizer.clock.clone();
    let assembly = Arc::new(ClaimedRunAssembly::new(host));
    let terminal_state = host.dispatch.terminal_reconciliation.clone();
    // Only a Coordinator-owned durable authority holds committed truth; it
    // always carries its local dispatch owner.
    let terminals = host
        .authority
        .scans_committed_terminals()
        .then(|| host.optional_dispatch_store())
        .flatten()
        .map(|dispatch| CommittedTerminalScan {
            dispatch,
            lease_owner: host.dispatch_owner().to_owned(),
        });
    if !host.runs_local_dispatch_pool() {
        return Ok(SessionWorkerResolver::coordinator(
            queue,
            clock,
            assembly,
            terminal_state,
            terminals,
        ));
    }
    let projection = host.worker.session_control.clone().map_or(
        ProjectionAuthority::CoLocated,
        ProjectionAuthority::RemoteControl,
    );
    let execution = ClaimedExecution {
        installation: assembly.clone(),
        preparation: assembly.clone(),
        runs: assembly.clone(),
        mcp: Arc::new(host.dispatch.run_mcp_effects.clone()),
        projection,
        publication_closure: crate::agent_catalog::RUN_PUBLICATIONS,
    };
    Ok(SessionWorkerResolver::worker(
        queue,
        clock,
        assembly,
        terminal_state,
        execution,
        terminals,
    ))
}

/// A realization failure crosses into dispatch as its one Run projection,
/// which carries the class its disposition implies.
pub(crate) fn admission_error(error: RealizationError) -> AdmissionError {
    AdmissionError::Run(awaken_session_types::RunError::from(error))
}

/// Adopt the Sandbox a claim carries through the realization executor. The
/// claim outcome is the one its realization disposition names.
pub(crate) async fn adopt_bound_sandbox(
    host: &SharedHost,
    encoded: Option<&str>,
    expected_sandbox_id: &str,
    run_id: &RunId,
    provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    recovery: awaken_dispatch_contract::WorkerRecoveryMode,
) -> Result<crate::session_environment::SessionEnvironmentAdoptionDisposition, Error> {
    let provider = host
        .realizer
        .session_environment_provider_for_thread(expected_sandbox_id, provisioning)
        .map_err(|error| map_admission_error(admission_error(error)))?;
    host.realizer
        .adopt_bound_session_environment(
            expected_sandbox_id,
            encoded,
            provider,
            None,
            recovery == awaken_dispatch_contract::WorkerRecoveryMode::RebuildFromCommittedTruth,
        )
        .await
        .map_err(|error| {
            claim_outcome(
                error.disposition().contract_class(),
                format!("run {}: {error}", run_id.0),
            )
        })
}

/// The executor's part of claimed-Run resolution over one Host: the installed
/// Session view, Run preparation, Run and commit-boundary Worker construction.
///
/// `host` is `Weak` because the Host owns the dispatch pool that holds the
/// resolver; a strong edge would keep the Host alive through its own pool.
/// The executor split into components (plan v4 D5) gives each role its owner.
const HOST_DROPPED: &str = "host dropped; pool idling";

pub(crate) struct ClaimedRunAssembly {
    host: std::sync::Weak<SharedHost>,
    sessions: crate::session_realizations::SessionRealizations,
    local_workspace: String,
}

impl ClaimedRunAssembly {
    fn new(host: &Arc<SharedHost>) -> Self {
        Self {
            host: Arc::downgrade(host),
            sessions: host.realizer.environments.sessions.clone(),
            local_workspace: host.realizer.publication.local_workspace().to_owned(),
        }
    }

    fn host(&self) -> Result<Arc<SharedHost>, Error> {
        self.host
            .upgrade()
            .ok_or_else(|| execution_error(HOST_DROPPED))
    }

    /// A dropped Host leaves the pool idling: a retryable admission failure.
    fn admitting_host(&self) -> Result<Arc<SharedHost>, AdmissionError> {
        self.host.upgrade().ok_or_else(|| {
            AdmissionError::Run(awaken_session_types::RunError::unavailable(HOST_DROPPED))
        })
    }
}

#[async_trait::async_trait]
impl ClaimedSessionInstallation for ClaimedRunAssembly {
    fn local_workspace(&self) -> &str {
        &self.local_workspace
    }

    fn bind_execution_workspace(
        &self,
        session: &str,
        workspace: &str,
    ) -> Result<bool, AdmissionError> {
        self.sessions
            .bind_execution_workspace(session, workspace)
            .map_err(admission_error)
    }

    async fn acquire_claimed_realization(
        &self,
        session: &str,
        claim: &RunClaim,
    ) -> (
        awaken_session_realization_contract::SessionRealizationPermit,
        Option<crate::session_resource::SessionRealizationLease>,
    ) {
        self.sessions
            .acquire_claimed_realization(session, claim)
            .await
    }

    async fn install_dispatched_resources(
        &self,
        session: &str,
        manifest: &crate::session_resource::SessionResourceManifest,
        claim: &RunClaim,
    ) -> Result<(), awaken_session_types::RunError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| awaken_session_types::RunError::unavailable(HOST_DROPPED))?;
        host.realizer
            .staging
            .install_dispatched_resources(&host.realizer, session, manifest, Some(claim))
            .await
    }

    fn observe_projection(
        &self,
        session: &str,
    ) -> Result<Option<ObservedProjection>, AdmissionError> {
        Ok(self
            .sessions
            .installed_projection(session)
            .map_err(admission_error)?
            .map(|installed| ObservedProjection {
                session_dispatch: installed.session_dispatch,
                has_frozen_baseline: installed.has_frozen_baseline,
                installation: SessionInstallation::new(installed.installation),
            }))
    }

    fn bind_claim_if_projection_unchanged(
        &self,
        session: &str,
        observed: &SessionInstallation,
        require_installed_projection: bool,
        claim: RunClaim,
    ) -> Result<bool, AdmissionError> {
        // An installation this realization did not mint is never the installed one.
        let Ok(observed) = observed.identity().downcast::<tokio::sync::Mutex<()>>() else {
            return Ok(false);
        };
        self.sessions
            .bind_claim_if_projection_unchanged(
                session,
                &observed,
                require_installed_projection,
                claim,
            )
            .map_err(admission_error)
    }

    fn durable_environment_binding(&self, session: &str) -> Result<Option<String>, AdmissionError> {
        self.admitting_host()?
            .realizer
            .durable_environment_binding(session)
            .map_err(admission_error)
    }

    fn pin_dispatched_identity(
        &self,
        session: &str,
        agent_id: &str,
        backend_ref: &str,
        workspace: Option<&str>,
    ) -> Result<(), AdmissionError> {
        self.sessions
            .pin_dispatched_identity(session, agent_id, backend_ref, workspace)
            .map_err(admission_error)
    }

    async fn adopt_bound_environment(
        &self,
        session: &str,
        encoded: Option<&str>,
        run_id: &RunId,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
        recovery: awaken_dispatch_contract::WorkerRecoveryMode,
    ) -> Result<(), Error> {
        adopt_bound_sandbox(
            self.host()?.as_ref(),
            encoded,
            session,
            run_id,
            provisioning,
            recovery,
        )
        .await
        .map(|_| ())
    }

    fn published_snapshot(
        &self,
        session: &str,
    ) -> Result<Option<ExecutableAgentSnapshot>, AdmissionError> {
        self.sessions
            .published_snapshot(session)
            .map_err(admission_error)
    }
}

#[async_trait::async_trait]
impl ClaimedRunPreparation for ClaimedRunAssembly {
    fn install_runtime_projection(
        &self,
        session: &str,
        projection: awaken_session_configuration_contract::SessionRuntimeProjection,
    ) -> Result<(), AdmissionError> {
        let host = self.admitting_host()?;
        host.install_environment_projection(session, &projection.environment)?;
        host.installed_sessions
            .replace_frozen_with(session, |next| next.tools = projection.tools);
        Ok(())
    }

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
            self.host()?.as_ref(),
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

    async fn ensure_session_substrate(
        &self,
        session: &str,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> Result<(), AdmissionError> {
        self.admitting_host()?
            .session_child_execution_substrate(session, publication)
            .await
            .map(|_| ())
            .map_err(AdmissionError::from)
    }

    fn bind_deferred_executor(&self, session: &str) {
        if let Ok(host) = self.host() {
            crate::lazy_sandbox::bind_deferred_executor(&host, session);
        }
    }
}

#[async_trait::async_trait]
impl ClaimedRunWorkers for ClaimedRunAssembly {
    fn credential_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        self.host
            .upgrade()
            .map(|host| host.local_credential_realization_capabilities())
            .unwrap_or_default()
    }

    async fn root_worker(
        &self,
        thread: &ThreadId,
        agent: Option<&str>,
        publication: Option<ExecutableAgentSnapshot>,
        dispatch: Option<&RunDispatch>,
        publications: Option<Arc<StaticPublishedAgentSnapshots>>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        let host = self.host()?;
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
            host.ctx_for_claimed_snapshot(
                &thread.0,
                agent,
                publication
                    .ok_or_else(|| execution_error("claimed Run publication is unavailable"))?,
                input,
            )
            .await
        } else {
            host.ctx_for_snapshot(&thread.0, agent, publication).await
        };
        context
            .map(|ctx| ctx.claimed_worker.clone())
            .map_err(map_admission_error)
    }

    async fn child_worker(
        &self,
        claimed: &Claimed,
        session_thread: &ThreadId,
        publications: Arc<StaticPublishedAgentSnapshots>,
    ) -> Result<Arc<DispatchWorker<AnyDispatchStore>>, Error> {
        super::claimed_run::construct_claimed_child(
            self.host()?.as_ref(),
            claimed,
            session_thread,
            publications,
        )
        .await
    }
}

#[async_trait::async_trait]
impl CommitBoundaryWorkers for ClaimedRunAssembly {
    async fn open_commit(&self, thread: &ThreadId) -> Result<Box<dyn ThreadCommitBoundary>, Error> {
        let host = self.host()?;
        let commit = host
            .authority
            .open_commit(&thread.0)
            .await
            .map_err(|error| execution_error(error.to_string()))?;
        Ok(Box::new(OpenedThreadCommit {
            host,
            commit: Arc::new(commit),
        }))
    }

    fn recovery_source(
        &self,
    ) -> Result<Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>, Error>
    {
        Ok(SharedHost::worker_recovery_source(&self.host()?))
    }
}

/// One opened Thread commit boundary, held only for one resolve or terminal
/// candidate. It contains no cache, mutation, or new authority.
struct OpenedThreadCommit {
    host: Arc<SharedHost>,
    commit: Arc<crate::store::HostCommit>,
}

#[async_trait::async_trait]
impl ThreadCommitBoundary for OpenedThreadCommit {
    fn committed(
        &self,
    ) -> &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView {
        self.commit.as_ref()
    }

    async fn boundary_worker(
        &self,
        claimed: &Claimed,
        context: Option<RuntimeRunContext>,
        install_cancellation_executor: bool,
    ) -> Result<DispatchWorker<AnyDispatchStore>, Error> {
        super::boundary_run::construct_boundary_worker(
            self.host.as_ref(),
            claimed,
            self.commit.clone(),
            context,
            install_cancellation_executor,
        )
        .await
    }
}

#[async_trait::async_trait]
impl SessionProjectionExecution for SharedHost {
    fn sessions(&self) -> crate::session_realizations::SessionRealizations {
        // Projection execution consumes the constructor-bound view of the same
        // original slots as realization, publication, staging, and terminal cleanup.
        self.realizer.environments.sessions.clone()
    }
    fn realizer(&self) -> &crate::realization_executor::RealizationExecutor {
        &self.realizer
    }
    async fn ensure_session_substrate(
        &self,
        thread: &str,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> Result<(), RealizationError> {
        self.session_child_execution_substrate(thread, publication)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }

    async fn install_frozen_projection(
        &self,
        session: &str,
        projection: FrozenSessionProjection,
        claim: Option<&RunClaim>,
        synchronize_resources: bool,
        lease: Option<ObservedSessionRealizationLease>,
    ) -> Result<(), RealizationError> {
        self.install_frozen_session_projection(
            session,
            projection,
            claim,
            synchronize_resources,
            lease,
        )
        .await
        .map_err(Into::into)
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

/// Dispatch admission reads the installed Session through the realization's
/// named operations on the same slots.
pub(crate) struct InstalledSessionReads(
    pub(crate) crate::session_realizations::SessionRealizations,
);

impl awaken_dispatch_execution::session_dispatch::DispatchAdmissionReads for InstalledSessionReads {
    fn execution_workspace(&self, thread: &str, fallback: &str) -> Result<String, AdmissionError> {
        self.0.workspace(thread, fallback).map_err(admission_error)
    }

    fn registered_workspace(&self, thread: &str) -> Result<Option<String>, AdmissionError> {
        self.0.registered_workspace(thread).map_err(admission_error)
    }

    fn credential_realization(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_credential_contract::CredentialRealizationProfile>, AdmissionError>
    {
        self.0
            .credential_realization(thread)
            .map_err(admission_error)
    }

    fn installed_dispatch_inputs(
        &self,
        thread: &str,
    ) -> Result<awaken_dispatch_execution::session_dispatch::InstalledDispatchInputs, AdmissionError>
    {
        let installed = self
            .0
            .installed_dispatch_inputs(thread)
            .map_err(admission_error)?;
        Ok(
            awaken_dispatch_execution::session_dispatch::InstalledDispatchInputs {
                resources: installed.resources,
                runtime_projection: installed.runtime_projection,
                session_dispatch: installed.session_dispatch,
            },
        )
    }
}
