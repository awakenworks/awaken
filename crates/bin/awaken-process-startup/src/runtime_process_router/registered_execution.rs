// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: The production composition selects existing Worker delivery, commit, observation
// @formal reason: and process-lifecycle owners. No execution, queue or receipt authority is added.
//! Coordinator composition for Sessions executed by registered Workers.

use std::sync::Arc;

use awaken_dispatch::{DispatchSettlementError, DispatchSettlementObserver, RunClaim, RunDispatch};
use awaken_dispatch_erasure::{AnyCoordinatorDispatch, DispatchBinding, RuntimeAuthority};
use awaken_dispatch_execution::session_worker_resolver::{
    CommittedTerminalScan, SessionWorkerResolver,
};
use awaken_dispatch_execution::{DispatchHint, ProcessDispatch};
use awaken_session_application::{SessionApplication, SessionRunApplication};
use awaken_session_run_delivery::{
    AuthorityCommits, CommittedReads, CompletionRegistry, MemoryBindingCompiler, MemoryExtractions,
    ResourceBindingVerifiers, SessionBoundaryEffects, SessionCommitWorkers,
    SessionDispatchLifecycle, SessionDispatchObserver, SessionProjections, SettlementWakeup,
    ThreadEventHub, WorkerPlacedSessions, WorkerPlacement,
};

use super::{ControlSurface, DeploymentAndResources, ExecutionStores, ProcessBase};
use crate::coordinator_execution::{ExecutionCompositionError, SessionCommitOwner};

/// A construction product consumed after the Session application exists.
/// The application retains delivery roles; service tasks retain the same commit
/// and observer owners, whose delivery collaborators retain no process services.
pub(super) struct RegisteredExecution {
    pub(super) sessions: Arc<WorkerPlacedSessions>,
    pub(super) dispatch: Arc<ProcessDispatch>,
    pub(super) owner: Arc<AnyCoordinatorDispatch>,
    pub(super) capture_policy: awaken_data_governance_contract::CapturePolicy,
    commits: Arc<AuthorityCommits>,
    reads: Arc<CommittedReads>,
    completion: Arc<CompletionRegistry>,
    extractions: Arc<MemoryExtractions>,
    clock: Arc<dyn awaken_time::WallClock>,
    files: Arc<
        dyn awaken_resource_contract::FileContentSource<
                awaken_session_realization_contract::ResourceOperationFence,
            >,
    >,
    storage_dir: Option<std::path::PathBuf>,
}

impl RegisteredExecution {
    pub(super) fn prepare(
        base: &ProcessBase,
        resources: &DeploymentAndResources,
        control: &ControlSurface,
        execution: &ExecutionStores,
        deployment: &awaken_deployment_config::DeploymentConfig,
        authority: Arc<dyn RuntimeAuthority>,
        default_model: String,
    ) -> Result<Self, String> {
        let clock = base.stores.clock.clone();
        let owner = authority.dispatch_store();
        let wake = authority.dispatch_wake();
        let dispatch = Arc::new(ProcessDispatch::new(
            deployment.execution_topology()?,
            clock.clone(),
            DispatchHint::new(
                wake.clone()
                    .unwrap_or_else(|| Arc::new(awaken_dispatch_execution::LocalWakeSignal::new())),
            ),
        ));
        let projections = Arc::new(SessionProjections::new(
            base.stores.platform_workspace.clone(),
            Some(base.agents.catalog.clone()),
        ));
        let commits = Arc::new(AuthorityCommits::new(authority));
        let reads = Arc::new(CommittedReads::new(commits.clone(), projections.clone()));
        let hub = Arc::new(ThreadEventHub::new());
        let completion = Arc::new(CompletionRegistry::new(hub.clone()));
        let extractions = Arc::new(MemoryExtractions::new(
            execution.memory_extractions.clone(),
            Arc::new(awaken_session_run_delivery::BackgroundRuns::new()),
            clock.clone(),
        ));
        let files = resources.resource_application.file_content_source();
        let sessions = Arc::new(WorkerPlacedSessions::new(WorkerPlacement {
            projections,
            publication: dispatch.publication(),
            owner: owner.clone(),
            reads: reads.clone(),
            hub,
            completion: completion.clone(),
            skill_bundles: Some(resources.resource_application.skill_bundle_source()),
            resource_bindings: ResourceBindingVerifiers {
                memory: Some(resources.resource_registry.clone()),
                repositories: Some(Arc::new(
                    awaken_resource_application::RegistryRepositoryBindingVerifier::new(
                        resources.resource_registry.clone(),
                    ),
                )),
            },
            memory_bindings: MemoryBindingCompiler::new(
                resources.resource_authorities.memory_execution_content(),
                Some(resources.resource_registry.clone()),
            ),
            memory_extractions: extractions.clone(),
            skill_catalog: Some(resources.resource_application.skill_catalog_application()),
            wake,
            clock: clock.clone(),
            default_model,
        }));
        Ok(Self {
            sessions,
            dispatch,
            owner,
            capture_policy: awaken_data_governance_contract::CapturePolicy::new(
                deployment.content_capture.level,
                control.capture_policy.clone(),
            ),
            commits,
            reads,
            completion,
            extractions,
            clock,
            files,
            storage_dir: deployment.storage_dir.clone(),
        })
    }

    pub(super) fn finish(
        self,
        application: &Arc<SessionApplication>,
    ) -> Result<awaken_coordinator::ExecutionPorts, ExecutionCompositionError> {
        let feed: Arc<dyn awaken_dispatch::DispatchOperationalFeed> = self.owner.clone();
        crate::coordinator_execution::verify_composition(&self.dispatch, &feed, application)?;
        let projections = self.sessions.projections().clone();
        let projected_agents = projections.clone();
        let runs = Arc::new(SessionRunApplication::new(
            self.sessions.clone(),
            application.run_admissions(),
            application.coordination(),
            move |thread| {
                use awaken_dispatch_execution::session_dispatch::DispatchAdmissionReads as _;
                projections
                    .execution_workspace(thread, projections.local_workspace())
                    .map_err(Into::into)
            },
            move |thread| {
                Ok(projected_agents
                    .projection(thread)?
                    .map(|projection| projection.baseline().agent_id.clone()))
            },
        ));
        let completion = Arc::new(SettlementWakeup::new(
            self.completion.clone(),
            application.lifecycle().reconciliation_wakeup(),
        ));
        let memory = self.sessions.worker_memory_settlement_observer();
        let effects = Arc::new(CoordinatorBoundaryEffects {
            sessions: Arc::new(SessionDispatchObserver::new(application.coordination())),
            memory: memory.clone(),
        });
        let binding = DispatchBinding::local(self.owner.clone());
        let workers = Arc::new(SessionCommitWorkers::new(
            self.commits,
            Some(binding.clone()),
            self.clock.clone(),
            self.reads.clone(),
            effects,
        ));
        let owner = self.owner.clone();
        let clock = self.clock;
        let lifecycle = SessionDispatchLifecycle::new(
            self.dispatch.clone(),
            Some(binding),
            completion.clone(),
            Some(owner.clone()),
            self.extractions.background(),
            move |process| {
                Ok(Arc::new(SessionWorkerResolver::coordinator(
                    owner.worker_view(),
                    clock.clone(),
                    workers.clone(),
                    process.terminal_reconciliation().clone(),
                    Some(CommittedTerminalScan {
                        dispatch: owner.clone(),
                        lease_owner: process.topology().lease_owner.as_str().to_owned(),
                    }),
                )))
            },
        );
        Ok(awaken_coordinator::ExecutionPorts {
            session_runs: runs,
            durable_runs: self.sessions,
            dispatch: self.dispatch,
            dispatch_lifecycle: Arc::new(lifecycle),
            coordinator_dispatch: self.owner,
            commit_owners: Arc::new(SessionCommitOwner(self.reads.clone())),
            worker_recovery: self.reads,
            worker_completion: completion,
            worker_settlement: memory,
            worker_stream: self.completion,
            worker_file_content: self.files,
            storage_dir: self.storage_dir,
        })
    }
}

/// Only composition of existing callbacks: Memory intent registration precedes
/// Session settlement; Dispatch owns the CAS between before/after callbacks.
pub(super) struct CoordinatorBoundaryEffects {
    pub(super) sessions: Arc<SessionDispatchObserver>,
    pub(super) memory: Arc<dyn DispatchSettlementObserver>,
}

#[async_trait::async_trait]
impl SessionBoundaryEffects for CoordinatorBoundaryEffects {
    async fn configure(
        self: Arc<Self>,
        worker: awaken_dispatch_execution::DispatchWorker<
            awaken_dispatch_erasure::AnyDispatchStore,
        >,
        claimed: &awaken_dispatch::Claimed,
        commit: Arc<awaken_session_run_delivery::SessionCommit>,
        context: Option<awaken_runtime_contract::RuntimeRunContext>,
    ) -> Result<
        awaken_dispatch_execution::DispatchWorker<awaken_dispatch_erasure::AnyDispatchStore>,
        awaken_dispatch_execution::Error,
    > {
        let worker = self
            .sessions
            .clone()
            .configure(worker, claimed, commit, context)
            .await?;
        Ok(worker.with_settlement_observer(self))
    }
}

#[async_trait::async_trait]
impl DispatchSettlementObserver for CoordinatorBoundaryEffects {
    async fn before_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &awaken_agent_contract::agent::run::RunState,
        cancelled: bool,
    ) -> Result<(), DispatchSettlementError> {
        self.memory
            .before_settle(dispatch, claim, state, cancelled)
            .await?;
        self.sessions
            .before_settle(dispatch, claim, state, cancelled)
            .await
    }

    async fn after_settle(
        &self,
        dispatch: &RunDispatch,
        claim: &RunClaim,
        state: &awaken_agent_contract::agent::run::RunState,
        cancelled: bool,
    ) -> Result<(), DispatchSettlementError> {
        self.memory
            .after_settle(dispatch, claim, state, cancelled)
            .await?;
        self.sessions
            .after_settle(dispatch, claim, state, cancelled)
            .await
    }
}
