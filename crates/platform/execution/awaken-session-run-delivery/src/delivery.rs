// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: The Coordinator's Session execution for Sessions its registered Workers execute: it lowers
// @formal reason: each admitted command over the installed frozen projection into durable dispatch rows and
// @formal reason: never executes, stages or realizes anything itself. Classification, not a formal-proof
// @formal reason: claim.
//! [`WorkerPlacedSessions`]: the Coordinator's Session execution for the
//! Sessions its registered Workers execute.
//!
//! The Session application admits each command; this lowers it over the
//! Session's installed frozen projection ([`SessionProjections`]) into the
//! durable dispatch rows a registered Worker claims. It verifies the live
//! Resource bindings the projection freezes and reads the pinned Skill bundles
//! slash commands expand from, each against its digest; it stages, mounts and
//! realizes nothing.

mod dispatched_runs;
mod durable_operations;
mod inputs;
mod memory;
mod outcomes;
pub use outcomes::{OutcomeRunExecutor, read_run_settlement};
mod roles;
mod thread_runs;

use std::collections::HashMap;
use std::sync::Arc;

use awaken_sync::InvariantLock;

use awaken_agent_contract::agent::thread::Id as ThreadId;

use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::DispatchPublication;
use awaken_resource_contract::{LiveResourceBindingVerifier, RepositoryBindingVerifier};
use awaken_runtime_contract::runs::{RunError, StepOutcome};

use awaken_session_configuration_contract::SkillBundleSource;
use awaken_session_configuration_contract::resource::binding_checks::verify_manifest_bindings;
use awaken_session_configuration_contract::skill_delivery::{
    load_pinned_skills, skill_store_run_error,
};
use awaken_session_execution_contract::ResourceOperationFence;
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunDelivery,
    SessionRunReservation,
};

use crate::session_run_activation::{activate_session_run_reservation, read_settled_phase};
use crate::session_run_reservation::{
    SessionRunInputs, reserve_session_run, select_session_run_origin, validate_session_run_command,
};
use crate::{
    CommittedReads, CompletionRegistry, DeliveryLock, SessionProjections, StepCommitExpectation,
    ThreadEventHub, activate_and_await_session_run, authoritative_step_snapshot,
    managed_skill_registry, message_prefix_before_exact_inputs, project_committed_step,
    session_activation_snapshot, settled_step,
};
use crate::{CoordinatedThreads, CoordinationAuthorities, SessionBudgets, SessionLowering};

/// The live authorities a Session's frozen Resource bindings are verified
/// against before a Run is admitted.
#[derive(Clone, Default)]
pub struct ResourceBindingVerifiers {
    pub memory: Option<Arc<dyn LiveResourceBindingVerifier>>,
    pub repositories: Option<Arc<dyn RepositoryBindingVerifier<ResourceOperationFence>>>,
}

/// Where [`WorkerPlacedSessions`] admits Runs, delivers them and observes them settle.
pub struct WorkerPlacement {
    /// The installed frozen projection of each Session.
    pub projections: Arc<SessionProjections>,
    /// The construction-selected publication, retaining no process service.
    pub publication: DispatchPublication,
    /// The durable dispatch owner rows are reserved and activated in.
    pub owner: Arc<AnyCoordinatorDispatch>,
    /// The Sessions' committed truth.
    pub reads: Arc<CommittedReads>,
    /// The live observation point of every Thread.
    pub hub: Arc<ThreadEventHub>,
    /// The foreground completion relay Workers settle through.
    pub completion: Arc<CompletionRegistry>,
    /// The content-addressed store of pinned Skill bundles.
    pub skill_bundles: Option<Arc<dyn SkillBundleSource<ResourceOperationFence>>>,
    pub resource_bindings: ResourceBindingVerifiers,
    /// Frozen Memory bindings over the process's existing Resource authority.
    pub memory_bindings: crate::MemoryBindingCompiler,
    /// The process's existing durable Memory extraction controller factory.
    pub memory_extractions: Arc<crate::MemoryExtractions>,
    /// The Workspaces' Skill catalog a Session's Skill selections pin from.
    pub skill_catalog:
        Option<Arc<dyn awaken_session_configuration_contract::SkillCatalogApplication>>,
    /// The cross-node dispatch wake, a latency hint only.
    pub wake: Option<Arc<dyn awaken_dispatch_contract::WakeSignal>>,
    pub clock: Arc<dyn awaken_time::WallClock>,
    /// The model a Thread selecting none runs on.
    pub default_model: String,
}

/// The Coordinator's Session execution for Worker-placed Sessions. Consumers:
/// the Session application, through its Worker-placement roles.
pub struct WorkerPlacedSessions {
    ports: WorkerPlacement,
    threads: Arc<CoordinatedThreads>,
    budgets: Arc<SessionBudgets>,
    /// One step projection at a time per Session, so concurrent observers
    /// publish each committed suffix once and in order.
    step_projections: InvariantLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// One Outcome command at a time per Session.
    outcome_commands: InvariantLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkerPlacedSessions {
    #[must_use]
    pub fn new(ports: WorkerPlacement) -> Self {
        let threads = Arc::new(CoordinatedThreads::new(CoordinationAuthorities {
            owner: Some(ports.owner.clone()),
            publication: ports.publication.clone(),
            reads: ports.reads.clone(),
            lowering: ports.projections.clone(),
            publications: ports.projections.publication_source(),
            wake: ports.wake.clone(),
            clock: ports.clock.clone(),
        }));
        let budgets = Arc::new(SessionBudgets::new(threads.clone(), ports.reads.clone()));
        Self {
            ports,
            threads,
            budgets,
            step_projections: InvariantLock::default(),
            outcome_commands: InvariantLock::default(),
        }
    }

    /// The frozen projections Runs are admitted over.
    #[must_use]
    pub fn projections(&self) -> &Arc<SessionProjections> {
        &self.ports.projections
    }

    /// The existing Memory controller's Worker-placement driver. The driver
    /// borrows these same delivery owners and executes no local model.
    #[must_use]
    pub fn memory_extraction_driver(
        &self,
        binding: Arc<crate::MemoryBinding>,
    ) -> Arc<dyn awaken_ext_memory::MemoryExtractionDriver> {
        Arc::new(memory::DispatchedMemory::new(
            binding,
            self.ports.reads.clone(),
            self.ports.projections.clone(),
            self.ports.owner.clone(),
            self.ports.completion.clone(),
        ))
    }

    /// Coordinator-side Memory observation for the original Worker settlement
    /// boundary. The observer holds only the selected delivery collaborators.
    #[must_use]
    pub fn worker_memory_settlement_observer(
        &self,
    ) -> Arc<dyn awaken_dispatch_contract::DispatchSettlementObserver> {
        memory::settlement_observer(&self.ports)
    }

    /// The Session application's role set for the Sessions registered Workers
    /// realize. Committed reads, coordination reads and budgets select their components.
    ///
    /// Budget ownership is selected through this role set, rather than a second
    /// implementation on the placement aggregate.
    /// ```compile_fail,E0277
    /// use std::sync::Arc;
    /// use awaken_session_execution_contract::BudgetAccounting;
    /// use awaken_session_run_delivery::WorkerPlacedSessions;
    /// fn duplicate_budget_role(sessions: Arc<WorkerPlacedSessions>) -> Arc<dyn BudgetAccounting> {
    ///     sessions
    /// }
    /// ```
    /// Coordination reads also select the original Thread component directly.
    /// ```compile_fail,E0277
    /// use std::sync::Arc;
    /// use awaken_session_execution_contract::SessionCoordinationReads;
    /// use awaken_session_run_delivery::WorkerPlacedSessions;
    /// fn duplicate_read_role(sessions: Arc<WorkerPlacedSessions>) -> Arc<dyn SessionCoordinationReads> {
    ///     sessions
    /// }
    /// ```
    #[must_use]
    pub fn session_execution_roles(
        self: &Arc<Self>,
    ) -> awaken_session_execution_contract::SessionExecutionRoleSelection {
        awaken_session_execution_contract::SessionExecutionRoleSelection {
            default_model: self.ports.default_model.clone(),
            placement: awaken_session_execution_contract::SessionPlacementRoles::RegisteredWorkers(
                self.clone(),
            ),
            coordination: self.clone(),
            coordination_reads: self.threads.clone(),
            budget: self.budgets.clone(),
            streaming_runs: self.clone(),
            thread_reads: self.ports.reads.clone(),
            outcomes: self.clone(),
            live_inbox: self.clone(),
            capability: self.clone(),
        }
    }

    /// The Agent coordination of the Sessions' Threads.
    #[must_use]
    pub fn threads(&self) -> &CoordinatedThreads {
        &self.threads
    }

    fn outcome_command(&self, session: &str) -> Result<Arc<tokio::sync::Mutex<()>>, RunError> {
        Ok(self
            .outcome_commands
            .lock()
            .map_err(|_| DeliveryLock::OutcomeCommands)?
            .entry(session.to_owned())
            .or_default()
            .clone())
    }

    fn step_projection(&self, session: &str) -> Result<Arc<tokio::sync::Mutex<()>>, RunError> {
        Ok(self
            .step_projections
            .lock()
            .map_err(|_| DeliveryLock::StepProjections)?
            .entry(session.to_owned())
            .or_default()
            .clone())
    }

    /// Reserve the Session application's admitted Run over the Session's
    /// installed projection.
    ///
    /// Decision table (causes → effect; the lowering's own rules are
    /// [`reserve_session_run`]'s):
    /// | Rule | cause | effect |
    /// |---|---|---|
    /// | C1 | no projection is installed | retryable refusal; nothing is read or reserved |
    /// | C2 | a frozen MemoryStore or Repository binding changed | refused before the Skill store is read |
    /// | C3 | the projection pins no Agent publication (a legacy unpinned Session: an inline Agent's Session freezes one at creation) | refused as a bad request, which Event-batch recovery settles terminally |
    /// | C4 | a pinned Skill bundle is missing or mismatched | refused; nothing is reserved |
    /// | C5 | every input holds | one unclaimable row placed on a registered Worker, carrying the desired manifest and the Session's frozen model |
    pub async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
        validate_session_run_command(&command)?;
        let origin = select_session_run_origin(&command, self.ports.owner.as_ref()).await?;
        let projection = self
            .ports
            .projections
            .installed(command.session_id.as_str())?;
        verify_manifest_bindings(
            projection.manifest(),
            self.ports.resource_bindings.memory.as_deref(),
            self.ports.resource_bindings.repositories.as_deref(),
        )
        .await?;
        if let Some(source) = origin {
            return reserve_session_run(
                command,
                SessionRunInputs::Task(source),
                |activation| SessionLowering::prepare(self.ports.projections.as_ref(), activation),
                self.ports.owner.as_ref(),
            )
            .await;
        }
        let publication = projection.publication().cloned().ok_or_else(|| {
            RunError::bad_request("a Worker-placed Session requires a pinned Agent publication")
        })?;
        let versions = load_pinned_skills(
            self.ports.skill_bundles.as_deref(),
            projection.workspace(),
            projection.manifest().resources.skills(),
            |_| Ok(None),
        )
        .await
        .map_err(skill_store_run_error)?;
        let skills =
            managed_skill_registry(versions, Some(&publication), Some(projection.tools()))?;
        let snapshot = session_activation_snapshot(publication, Some(projection.tools()), true)?;
        reserve_session_run(
            command,
            SessionRunInputs::Session {
                snapshot: &snapshot,
                skills: skills.as_deref(),
                model_ref_override: Some(projection.baseline().execution_model_ref.clone()),
            },
            |activation| SessionLowering::prepare(self.ports.projections.as_ref(), activation),
            self.ports.owner.as_ref(),
        )
        .await
    }

    /// Publish the reservation the Session committed the activity of.
    pub async fn activate_session_run(
        &self,
        delivery: SessionRunDelivery,
    ) -> Result<SessionRunActivation, RunError> {
        activate_session_run_reservation(&self.ports.owner, &self.ports.publication, delivery).await
    }

    /// Activate an admitted Run and observe it settle: the observer registers
    /// before activation, so the Worker cannot settle the Run unobserved, and
    /// the settled step is the committed suffix after the Run's exact inputs.
    ///
    /// Decision table (causes → effect):
    /// | Rule | admission | committed truth | effect |
    /// |---|---|---|---|
    /// | O1 | reserved / replayed / activated | the Run settles | activated, then the committed step |
    /// | O2 | recovery claimed | the Run settles | nothing activated; the committed step |
    /// | O3 | completed | the Run already settled | nothing activated; the committed step, read at once |
    /// | O4 | any | the step does not follow the inputs | refused; nothing is published |
    pub async fn activate_and_observe_session_run(
        &self,
        admission: AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let session = admission.session_id().to_string();
        let run_id = admission.run_id().clone();
        let thread = ThreadId(session.clone());
        let commit = self.ports.reads.commit_for_read(&session).await?;
        let state = activate_and_await_session_run(
            &self.ports.completion,
            &run_id,
            sink,
            SETTLED_RECONCILIATION_INTERVAL,
            || async move {
                match admission {
                    AdmittedSessionRun::Reserved(delivery)
                    | AdmittedSessionRun::AlreadyReserved(delivery)
                    | AdmittedSessionRun::AlreadyActivated(delivery) => {
                        self.activate_session_run(delivery).await
                    }
                    AdmittedSessionRun::RecoveryClaimed { .. } => {
                        Ok(SessionRunActivation::RecoveryClaimed)
                    }
                    AdmittedSessionRun::Completed { .. } => Ok(SessionRunActivation::Completed),
                }
            },
            || read_settled_phase(&commit, &thread, &run_id, None),
        )
        .await?;
        let committed = authoritative_step_snapshot(&commit, &thread, &run_id).await?;
        let messages_before = message_prefix_before_exact_inputs(&committed, &input_message_ids)?;
        let step = self.step_projection(&session)?;
        let _projection = step.lock().await;
        settled_step(
            project_committed_step(
                &commit,
                &self.ports.hub,
                &thread,
                run_id,
                state,
                StepCommitExpectation {
                    messages_before,
                    input_ids: &input_message_ids,
                },
                &session,
            )
            .await?,
        )
    }
}

/// How often an observer rereads committed truth when no Worker settlement
/// reached this Coordinator (a peer Coordinator relayed it).
const SETTLED_RECONCILIATION_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{projection, publication};
    use awaken_agent_contract::AgentSkillKind;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_dispatch_contract::{
        DispatchControlReads as _, DispatchReads as _, SessionRunAdmissionObservation,
        SessionRunAdmissionReads as _,
    };
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, ResourceRegistryError, RetentionPolicy,
        SkillBundleFile, SkillVersion, skill_bundle_sha256,
    };
    use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation};
    use awaken_session_configuration_contract::SkillBundleSourceError;
    use awaken_session_configuration_contract::resource::{
        ResolvedInput, ResolvedInputSource, ResolvedSessionResources, ResolvedSkillBinding,
    };
    use awaken_session_execution_contract::{
        ChildRunCoordination as _, SessionRunExecutionRequirements,
    };
    use awaken_time::WallClockExt as _;

    fn skill() -> SkillVersion {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content:
                b"---\nname: review\ndescription: review code\n---\nReview $ARGUMENTS carefully."
                    .to_vec(),
            executable: false,
        }];
        SkillVersion {
            id: "review-1".into(),
            skill_id: "review".into(),
            version: 1,
            name: "review".into(),
            description: "review code".into(),
            directory: "/skills/review".into(),
            bundle_sha256: skill_bundle_sha256(&files),
            files,
            created_unix_nanos: 0,
        }
    }

    fn pin(version: &SkillVersion) -> ResolvedSkillBinding {
        ResolvedSkillBinding {
            kind: AgentSkillKind::Custom,
            skill_id: version.skill_id.to_string(),
            version: version.version,
            bundle_sha256: version.bundle_sha256.clone(),
        }
    }

    /// The content-addressed store: one stored bundle.
    struct Store(SkillVersion);

    #[async_trait::async_trait]
    impl SkillBundleSource<ResourceOperationFence> for Store {
        async fn load(
            &self,
            _workspace_id: &str,
            binding: &ResolvedSkillBinding,
            _fence: Option<&ResourceOperationFence>,
        ) -> Result<Option<SkillVersion>, SkillBundleSourceError> {
            Ok((binding.skill_id == self.0.skill_id.as_str()).then(|| self.0.clone()))
        }
    }

    /// A live MemoryStore binding at version 4.
    pub(super) struct Memory;

    #[async_trait::async_trait]
    impl LiveResourceBindingVerifier for Memory {
        async fn verify_memory_binding(
            &self,
            _workspace_id: &str,
            id: &str,
            version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            (version == ConfigVersion(4))
                .then_some(())
                .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))
        }

        async fn verify_repository_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            unreachable!("no Repository is bound")
        }
    }

    pub(super) fn memory_input(version: u64) -> ResolvedInput {
        ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: "memory".into(),
            source: ResolvedInputSource::MemoryStore {
                memory_store_id: "memory-1".into(),
                config: MemoryStoreConfigVersion {
                    memory_store_id: "memory-1".into(),
                    version: ConfigVersion(version),
                    retention_policy: RetentionPolicy::default(),
                },
            },
            mount_path: "/mnt/memory".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    /// Every Thread's committed truth, in memory.
    #[derive(Default)]
    struct MemoryCommits {
        commits: std::sync::Mutex<HashMap<String, Arc<dyn awaken_dispatch_erasure::LocalCommit>>>,
        read_error: Option<fn() -> RunError>,
        reads: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::SessionCommitSource for MemoryCommits {
        async fn open_for_read(
            &self,
            session: &str,
        ) -> Result<Arc<crate::SessionCommit>, RunError> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(error) = &self.read_error {
                return Err(error());
            }
            let commit = self
                .commits
                .lock()
                .expect("commit map")
                .entry(session.to_owned())
                .or_insert_with(|| {
                    Arc::new(awaken_dispatch_erasure::LocalCommitAdapter::projected(
                        awaken_store_inmem::MemoryCommitCoordinator::new(
                            awaken_time::system_clock(),
                        ),
                    ))
                })
                .clone();
            Ok(Arc::new(crate::SessionCommit::Local(commit)))
        }

        async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RunError> {
            Ok(self
                .commits
                .lock()
                .expect("commit map")
                .contains_key(thread))
        }
    }

    pub(super) async fn delivery(stored: Option<SkillVersion>) -> WorkerPlacedSessions {
        let clock = awaken_time::system_clock();
        let owner = Arc::new(AnyCoordinatorDispatch::from_durable_store(
            awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
            None,
        ));
        delivery_with_process(stored, clock, owner).await.0
    }

    async fn delivery_with_process(
        stored: Option<SkillVersion>,
        clock: Arc<dyn awaken_time::WallClock>,
        owner: Arc<AnyCoordinatorDispatch>,
    ) -> (
        WorkerPlacedSessions,
        Arc<awaken_dispatch_execution::ProcessDispatch>,
    ) {
        let memory_content = awaken_resource_contract::MemoryExecutionContent::new(Arc::new(
            awaken_memory_store::VolatileMemoryRepository::new(clock.clone()),
        ));
        let extractions = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("private Memory extraction authority"),
        );
        let memory_extractions = Arc::new(crate::MemoryExtractions::new(
            extractions,
            Arc::new(crate::BackgroundRuns::new()),
            clock.clone(),
        ));
        let projections = Arc::new(SessionProjections::new("local", None));
        let hub = Arc::new(ThreadEventHub::new());
        let completion = Arc::new(CompletionRegistry::new(hub.clone()));
        let process = Arc::new(awaken_dispatch_execution::ProcessDispatch::new(
            awaken_deployment_config::ExecutionTopology {
                foreground_delivery: awaken_deployment_config::ForegroundRunDelivery::Durable,
                claim_pool: awaken_deployment_config::ClaimPool::RemoteWorkersOnly,
                lease_owner: awaken_deployment_config::LeaseOwnerId::new("coordinator")
                    .expect("lease owner"),
                lost_wake_recovery_bound: std::time::Duration::from_millis(5),
                queue_depth: awaken_deployment_config::QueueDepthSource::External,
            },
            clock.clone(),
            awaken_dispatch_execution::wake::DispatchHint::new(Arc::new(
                awaken_dispatch_execution::LocalWakeSignal::new(),
            )),
        ));
        let sessions = WorkerPlacedSessions::new(WorkerPlacement {
            projections: projections.clone(),
            publication: process.publication(),
            owner,
            reads: Arc::new(CommittedReads::new(
                Arc::new(MemoryCommits::default()),
                projections,
            )),
            hub,
            completion,
            skill_bundles: stored.map(|version| {
                Arc::new(Store(version)) as Arc<dyn SkillBundleSource<ResourceOperationFence>>
            }),
            resource_bindings: ResourceBindingVerifiers {
                memory: Some(Arc::new(Memory)),
                repositories: None,
            },
            memory_bindings: crate::MemoryBindingCompiler::new(
                memory_content,
                Some(Arc::new(Memory)),
            ),
            memory_extractions,
            skill_catalog: None,
            wake: None,
            clock,
            default_model: "provider/default".into(),
        });
        (sessions, process)
    }

    #[tokio::test]
    async fn live_subscription_uses_committed_membership_and_the_original_hub() {
        // Causes: C1 target=root, committed advisor child, unknown child, or
        // the same child under a different physical Session; C2 observation
        // enters the shared reader or the Worker-placement role. R1 root and
        // exact child => both observers receive the same original live event;
        // R2 unknown/cross-Session child => InvalidRequest and no observer.
        // The child relationship is committed through the real Memory store,
        // not supplied as a match Boolean or a dispatch row. This extraction
        // adds no state transition; commit atomicity and hub delivery remain
        // their original owners' guarantees, covered separately.
        use awaken_agent_contract::agent::content::ContentBlock;
        use awaken_agent_contract::agent::delegation::DelegationId;
        use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
        let sessions = delivery(None).await;
        let session = awaken_session_types::SessionId::from("live-session");
        let run = RunId("live-root-run".into());
        let child = DelegationId::for_parent_call(&run, "advisor").child_run_id();
        let commit = sessions
            .ports
            .reads
            .commit_for_read(session.as_str())
            .await
            .unwrap();
        commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                ThreadId(session.to_string()),
                awaken_agent_contract::thread::commit::RunDisposition::running(run.clone()),
                true,
                vec![Message::new(
                    MessageId::assistant(&run, 0),
                    Role::Assistant,
                    vec![ContentBlock::tool_use(
                        "advisor",
                        awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                        serde_json::json!({}),
                    )],
                )],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        for (physical, target, allowed) in [
            (session.clone(), session.to_string(), true),
            (session.clone(), child.0.clone(), true),
            (session.clone(), "unknown-child".into(), false),
            (
                awaken_session_types::SessionId::from("other-session"),
                child.0.clone(),
                false,
            ),
        ] {
            let direct = sessions
                .ports
                .reads
                .subscribe_session_thread_live(&sessions.ports.hub, &physical, &target)
                .await;
            let adapter = sessions
                .subscribe_session_thread_live(&physical, &target)
                .await;
            if !allowed {
                for result in [direct, adapter] {
                    let error = result.err().expect("R2 refuses an observer");
                    assert_eq!(
                        error.kind,
                        awaken_runtime_contract::runs::RunErrorKind::BadRequest
                    );
                    assert_eq!(error.code, "invalid_request");
                    assert_eq!(
                        error.message,
                        "live observer target is not a coordinated Session Thread"
                    );
                }
                continue;
            }
            let observation = awaken_agent_contract::stream::event::Observation::from(
                awaken_agent_contract::stream::event::Event {
                    run_id: run.clone(),
                    kind: awaken_agent_contract::event::AgentEvent::Delta(
                        awaken_agent_contract::event::Delta::TextDelta {
                            delta: target.clone(),
                        },
                    ),
                },
            );
            sessions
                .ports
                .hub
                .publish(&target, crate::ThreadEvent::StepEnded { awaiting: false });
            sessions
                .ports
                .hub
                .publish(&target, crate::ThreadEvent::Live(observation.clone()));
            for result in [direct, adapter] {
                let mut observer = result.unwrap().expect("R1 original hub observer");
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(30), observer.recv())
                        .await
                        .unwrap()
                        .unwrap(),
                    Some(observation.clone()),
                    "R1 preserves live filtering and fan-out on the exact channel",
                );
            }
        }
    }

    #[tokio::test]
    async fn live_subscription_preserves_source_refusal_but_root_never_reads_it() {
        // Causes: C1 root/child; C2 original source refuses as Unavailable or
        // classified Internal; C3 direct reader/Worker-placement adapter.
        // R3 root => live subscription and zero source opens for both C2/C3;
        // R4 child => exact kind/code/message and one original source open.
        // Root equality, not a fallback after a failed read, selects R3.
        let failures: [fn() -> RunError; 2] = [
            || RunError::unavailable("original source is unavailable"),
            || RunError::classified("original_source_corrupt", "original source is corrupt"),
        ];
        for failure in failures {
            let mut sessions = delivery(None).await;
            let source = Arc::new(MemoryCommits {
                read_error: Some(failure),
                ..Default::default()
            });
            sessions.ports.reads = Arc::new(CommittedReads::new(
                source.clone(),
                sessions.ports.projections.clone(),
            ));
            for adapter in [false, true] {
                for root in [true, false] {
                    let session = awaken_session_types::SessionId::from("live-source");
                    let target = if root { "live-source" } else { "child" };
                    let before = source.reads.load(std::sync::atomic::Ordering::SeqCst);
                    let result = if adapter {
                        sessions
                            .subscribe_session_thread_live(&session, target)
                            .await
                    } else {
                        sessions
                            .ports
                            .reads
                            .subscribe_session_thread_live(&sessions.ports.hub, &session, target)
                            .await
                    };
                    if root {
                        assert!(result.unwrap().is_some(), "R3 root is read-free");
                    } else {
                        let actual = result.err().expect("R4 original read failure");
                        let expected = failure();
                        assert_eq!(
                            (actual.kind, actual.code, actual.message),
                            (expected.kind, expected.code, expected.message)
                        );
                    }
                    assert_eq!(
                        source.reads.load(std::sync::atomic::Ordering::SeqCst) - before,
                        usize::from(!root)
                    );
                }
            }
        }
    }

    fn command(session: &str, text: &str) -> AdmitSessionRun {
        AdmitSessionRun {
            session_id: session.into(),
            agent_id: "agent".into(),
            operation_id: "operation-1".into(),
            run_id: RunId(format!("{session}-run")),
            messages: vec![Message::text(
                MessageId("message-1".into()),
                Role::User,
                text,
            )],
            data_subject_id: None,
            traceparent: None,
            execution_requirements: SessionRunExecutionRequirements {
                tool_capability_narrowing:
                    awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured,
                required_worker_capabilities: Default::default(),
            },
            replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
            private_task_input: None,
        }
    }

    /// An Agent that selects the `review` Skill and can read files.
    fn reviewing_agent() -> awaken_runtime_contract::ExecutableAgentSnapshot {
        let mut agent = publication("agent");
        agent.resolved_spec.plugin_config.agent.skills =
            vec![awaken_agent_contract::AgentSkillBinding::custom("review")];
        agent.resolved_spec.tool_descriptors =
            vec![awaken_runtime_contract::resolved::ToolDescriptor::pinned(
                "test",
                "read",
                "read",
                serde_json::json!({"type": "object"}),
            )];
        agent
            .recompute_fingerprint()
            .expect("fingerprint the publication");
        agent
    }

    fn install(
        delivery: &WorkerPlacedSessions,
        session: &str,
        inputs: Vec<ResolvedInput>,
        skills: Vec<ResolvedSkillBinding>,
    ) {
        let mut frozen = projection(1, "provider/model", Some(reviewing_agent()));
        frozen.resources = ResolvedSessionResources::try_new(inputs, skills).expect("resources");
        delivery
            .projections()
            .install(session, frozen)
            .expect("install the projection");
    }

    async fn reserved(
        delivery: &WorkerPlacedSessions,
        session: &str,
    ) -> Option<Box<awaken_dispatch_contract::RunDispatch>> {
        match delivery
            .ports
            .owner
            .observe_session_run_admission(&RunId(format!("{session}-run")))
            .await
            .expect("observe the reservation")
        {
            SessionRunAdmissionObservation::Live(request) => Some(request),
            _ => None,
        }
    }

    async fn settle_task_origin(
        delivery: &WorkerPlacedSessions,
        session: &str,
        run: &str,
        mut activation: RunActivation,
        publications: Vec<ExecutableAgentSnapshot>,
        physical_session: Option<&str>,
    ) {
        use awaken_dispatch_contract::{
            DispatchClaims as _, DispatchEnqueue as _, DispatchOutcome, DispatchSettlement as _,
        };
        // This existing queue owns the source fact. Real Done settles the exact
        // selected value; the fixture never seeds a separate publication ledger.
        activation.run_id = RunId(run.into());
        activation.thread_id = ThreadId(session.into());
        if !activation
            .snapshot
            .resolved_spec
            .plugin_ids
            .iter()
            .any(|id| id == "background_task")
        {
            activation
                .snapshot
                .resolved_spec
                .plugin_ids
                .push("background_task".into());
            activation.snapshot.recompute_fingerprint().unwrap();
        }
        let mut request = awaken_dispatch_contract::RunDispatch::new(activation)
            .with_agent_publications(publications);
        if let Some(parent) = physical_session {
            request = request
                .for_session(ThreadId(parent.into()))
                .with_session_activity_epoch(7);
        }
        // C0 Task inputs and optional physical Session/epoch are now final.
        // Publish intrinsic demand once before the same owner enqueues/settles.
        request = request.with_worker_codec_requirements();
        delivery.ports.owner.enqueue(request).await.unwrap();
        let claimed = delivery
            .ports
            .owner
            .claim_run(
                &RunId(run.into()),
                "task-origin-owner",
                30_000,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
        delivery
            .ports
            .owner
            .settle(
                &RunId(run.into()),
                claimed.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn coordinator_reservation_follows_its_decision_table() {
        let stored = skill();

        // C1: no installed projection is a retryable refusal.
        let delivery = delivery(Some(stored.clone())).await;
        let refused = delivery
            .reserve_session_run(command("cold", "hello"))
            .await
            .expect_err("C1");
        assert_eq!(refused.code, "session_projection_not_installed", "C1");
        assert!(
            reserved(&delivery, "cold").await.is_none(),
            "C1 nothing reserved"
        );

        // C2: a MemoryStore binding that changed since the Session froze it.
        install(&delivery, "stale", vec![memory_input(3)], Vec::new());
        delivery
            .reserve_session_run(command("stale", "hello"))
            .await
            .expect_err("C2");
        assert!(
            reserved(&delivery, "stale").await.is_none(),
            "C2 nothing reserved"
        );

        // C4: a pinned bundle the store does not hold, then one whose digest
        // is not the pin.
        let missing = delivery_without_store().await;
        install(&missing, "missing", Vec::new(), vec![pin(&stored)]);
        missing
            .reserve_session_run(command("missing", "/review the patch"))
            .await
            .expect_err("C4 missing");
        assert!(
            reserved(&missing, "missing").await.is_none(),
            "C4 nothing reserved"
        );
        let mut repinned = pin(&stored);
        repinned.bundle_sha256 = "sha256:other".into();
        install(&delivery, "repinned", Vec::new(), vec![repinned]);
        delivery
            .reserve_session_run(command("repinned", "/review the patch"))
            .await
            .expect_err("C4 mismatched");
        assert!(
            reserved(&delivery, "repinned").await.is_none(),
            "C4 nothing reserved"
        );

        // C5: every input holds. The row is unclaimable until activation,
        // placed on a registered Worker, carries the desired manifest whose
        // Skill binding pins the bundle digest, the frozen model, and the
        // slash command expanded from the verified bundle.
        install(
            &delivery,
            "ready",
            vec![memory_input(4)],
            vec![pin(&stored)],
        );
        assert_eq!(
            delivery
                .reserve_session_run(command("ready", "/review the patch"))
                .await
                .expect("C5"),
            SessionRunReservation::Reserved,
            "C5"
        );
        let row = reserved(&delivery, "ready").await.expect("C5 reserved row");
        assert_eq!(
            row.activation.model_ref_override.as_deref(),
            Some("provider/model"),
            "C5 frozen model"
        );
        let input = row.activation.input[0].text_content();
        assert!(
            input.contains("Review the patch carefully."),
            "C5 expansion: {input}"
        );
        let resources = row
            .session_resources
            .as_ref()
            .expect("C5 Resource envelope");
        assert!(
            resources
                .resolved_resources_json
                .contains(stored.bundle_sha256.as_str()),
            "C5 the envelope pins the bundle digest"
        );
        assert_eq!(row.private_task_input, None, "C5 no Task input");

        // R7 (remote Task control): a Task attention's private input rides on
        // the row unchanged.
        install(&delivery, "attention", vec![memory_input(4)], Vec::new());
        let input: awaken_session_application_contract::PrivateTaskInput = serde_json::from_value(serde_json::json!({
            "version":1, "observations":[{"version":1,"task_id":"task"}],
            "tenure":{"thread":"attention","partition":{"kind":"thread"},"run":"attention-origin","task":"task","holder":"holder","epoch":3}
        })).unwrap();
        settle_task_origin(
            &delivery,
            "attention",
            "attention-origin",
            RunActivation::new(
                RunId("attention-origin".into()),
                ThreadId("attention".into()),
                reviewing_agent(),
                Vec::new(),
            ),
            Vec::new(),
            None,
        )
        .await;
        let mut attention = command("attention", "Background task has news.");
        attention.private_task_input = Some(input.clone());
        assert_eq!(
            delivery.reserve_session_run(attention).await.expect("R7"),
            SessionRunReservation::Reserved,
            "R7"
        );
        let row = reserved(&delivery, "attention")
            .await
            .expect("R7 reserved row");
        assert_eq!(row.private_task_input, Some(input), "R7 carried unchanged");
    }

    async fn delivery_without_store() -> WorkerPlacedSessions {
        delivery(None).await
    }

    #[tokio::test]
    async fn task_receipt_reads_the_original_live_and_completed_dispatch_identity() {
        // R1 missing -> no receipt; R2 Reserved -> Unavailable, never an ACK;
        // R3 activated exact input -> original activity epoch; R4 each Task
        // subject axis changes -> conflict; R5 Completed -> original immutable
        // receipt without any current Task-owner query or new queue write.
        // This is the real queue/read-role boundary; Session application tests
        // independently require its matching original activity receipt.
        use awaken_dispatch_contract::{
            DispatchClaims as _, DispatchOutcome, DispatchSettlement as _,
        };
        use awaken_session_execution_contract::SessionCoordinationReads as _;
        let delivery = delivery(Some(skill())).await;
        install(&delivery, "task-receipt", Vec::new(), Vec::new());
        let mut input = command("task-receipt", "background attention");
        input.private_task_input = Some(
            serde_json::from_value(serde_json::json!({"version":1, "observations":[],
            "tenure":{"thread":"task-receipt", "partition":{"kind":"thread"}, "run":"origin",
                "task":"task", "holder":"holder", "epoch":3}}))
            .unwrap(),
        );
        settle_task_origin(
            &delivery,
            "task-receipt",
            "origin",
            RunActivation::new(
                RunId("origin".into()),
                ThreadId("task-receipt".into()),
                reviewing_agent(),
                Vec::new(),
            ),
            Vec::new(),
            None,
        )
        .await;
        assert_eq!(
            delivery
                .threads
                .session_run_command_receipt(&input)
                .await
                .unwrap(),
            None,
            "R1"
        );
        delivery.reserve_session_run(input.clone()).await.unwrap();
        assert_eq!(
            delivery
                .threads
                .session_run_command_receipt(&input)
                .await
                .unwrap_err()
                .kind,
            awaken_runtime_contract::runs::RunErrorKind::Unavailable,
            "R2"
        );
        delivery
            .activate_session_run(SessionRunDelivery {
                session_id: input.session_id.clone(),
                run_id: input.run_id.clone(),
                session_activity_epoch: 7,
            })
            .await
            .unwrap();
        assert_eq!(
            delivery
                .threads
                .session_run_command_receipt(&input)
                .await
                .unwrap(),
            Some(SessionRunReservation::AlreadyActivated {
                session_activity_epoch: 7
            }),
            "R3"
        );
        for completed in [false, true] {
            if completed {
                // R0 the actual reservation is RemoteOnly: the old local
                // claimant must get None. Declare the compatible candidate in
                // the shared data fixture and check the original strict kernel;
                // Memory's independent exact-claim regression cannot certify it.
                assert!(
                    delivery
                        .ports
                        .owner
                        .claim_run(
                            &input.run_id,
                            "task-receipt-owner",
                            30_000,
                            &Default::default(),
                        )
                        .await
                        .unwrap()
                        .is_none(),
                    "R0 local claim cannot consume a Worker-only callback",
                );
                let worker = crate::test_support::task_callback_worker("task-receipt-owner");
                let SessionRunAdmissionObservation::Live(request) = delivery
                    .ports
                    .owner
                    .observe_session_run_admission(&input.run_id)
                    .await
                    .unwrap()
                else {
                    panic!("R0 original activated callback");
                };
                awaken_worker_contract::can_assign(
                    &worker,
                    &request.placement,
                    None,
                    false,
                    delivery.ports.clock.now_ms(),
                )
                .unwrap_or_else(|failure| {
                    panic!(
                        "R0 declared candidate satisfies the same SQLite placement law: {failure:?}; static_eligibility={:?}; placement={:?}; worker={:?}",
                        awaken_worker_contract::can_claim(&worker.manifest, &request.placement),
                        request.placement,
                        worker,
                    )
                });
                let claim = delivery
                    .ports
                    .owner
                    .claim_run_compatible(&input.run_id, &worker, 30_000)
                    .await
                    .unwrap()
                    .unwrap();
                delivery
                    .ports
                    .owner
                    .settle(
                        &input.run_id,
                        claim.lease.epoch(),
                        DispatchOutcome::Done,
                        &[],
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    delivery
                        .threads
                        .session_run_command_receipt(&input)
                        .await
                        .unwrap(),
                    Some(SessionRunReservation::Completed),
                    "R5"
                );
            }
            let original = delivery
                .ports
                .owner
                .observe_session_run_admission(&input.run_id)
                .await
                .unwrap();
            for axis in ["task", "run", "holder", "epoch", "thread", "partition"] {
                let mut changed = input.clone();
                let mut wire =
                    serde_json::to_value(changed.private_task_input.as_ref().unwrap()).unwrap();
                if axis == "epoch" {
                    wire["tenure"][axis] = serde_json::json!(4);
                } else if axis == "partition" {
                    wire["tenure"][axis] =
                        serde_json::json!({"kind":"session", "session_thread":"other"});
                } else {
                    wire["tenure"][axis] = serde_json::json!(format!("other-{axis}"));
                }
                changed.private_task_input = Some(serde_json::from_value(wire).unwrap());
                assert_eq!(
                    delivery
                        .threads
                        .session_run_command_receipt(&changed)
                        .await
                        .unwrap_err()
                        .kind,
                    awaken_runtime_contract::runs::RunErrorKind::Conflict,
                    "R4 axis={axis} completed={completed}"
                );
                assert_eq!(
                    delivery
                        .ports
                        .owner
                        .observe_session_run_admission(&input.run_id)
                        .await
                        .unwrap(),
                    original,
                    "read never mutates original"
                );
            }
        }
    }

    #[tokio::test]
    async fn coordinator_activation_publishes_only_a_committed_reservation() {
        // A1: a zero activity epoch is refused before the store is touched.
        let delivery = delivery(Some(skill())).await;
        install(&delivery, "session", Vec::new(), Vec::new());
        assert_eq!(
            delivery
                .reserve_session_run(command("session", "hello"))
                .await
                .expect("reserve"),
            SessionRunReservation::Reserved
        );
        let run_id = RunId("session-run".into());
        let zero = delivery
            .activate_session_run(SessionRunDelivery {
                session_id: "session".into(),
                run_id: run_id.clone(),
                session_activity_epoch: 0,
            })
            .await
            .expect_err("A1");
        assert_eq!(zero.code, "invalid_request", "A1");

        // A2: the committed activity activates the row once; a replay is the
        // same activation.
        let activate = || SessionRunDelivery {
            session_id: "session".into(),
            run_id: run_id.clone(),
            session_activity_epoch: 3,
        };
        assert_eq!(
            delivery.activate_session_run(activate()).await.expect("A2"),
            SessionRunActivation::Activated,
            "A2"
        );
        assert_eq!(
            delivery
                .activate_session_run(activate())
                .await
                .expect("A2 replay"),
            SessionRunActivation::AlreadyActivated {
                session_activity_epoch: 3
            },
            "A2 replay"
        );

        // A4: an unknown reservation is refused.
        let unknown = delivery
            .activate_session_run(SessionRunDelivery {
                session_id: "session".into(),
                run_id: RunId("unknown".into()),
                session_activity_epoch: 3,
            })
            .await
            .expect_err("A4");
        assert_eq!(unknown.code, "invalid_request", "A4");
    }

    fn layout(
        placement: awaken_session_types::SessionRuntimePlacement,
        resources: Vec<awaken_resource_contract::InputBinding>,
    ) -> awaken_session_configuration_contract::SessionSandboxLayout {
        awaken_session_configuration_contract::SessionSandboxLayout {
            workspace_id: "workspace".into(),
            agent_id: "agent".into(),
            agent_revision: None,
            runtime_placement: placement,
            model_override: None,
            runtime: None,
            mounts: Vec::new(),
            env: Vec::new(),
            environment: crate::test_support::environment(),
            resources,
        }
    }

    fn repository(mount_path: &str) -> awaken_resource_contract::InputBinding {
        awaken_resource_contract::InputBinding {
            binding_id: awaken_resource_contract::BindingId::from(mount_path),
            target: awaken_resource_contract::InputResourceId::Repository(
                awaken_resource_contract::RepositoryId::from("repo"),
            ),
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    #[tokio::test]
    async fn coordinator_input_admission_follows_its_decision_table() {
        use awaken_session_execution_contract::SessionInputAdmission as _;
        use awaken_session_types::SessionRuntimePlacement::{Local, Worker};
        let delivery = delivery(None).await;

        // V2: a local placement needs a co-located Worker.
        let local = delivery
            .validate_session_sandbox_layout("session", &layout(Local, Vec::new()))
            .expect_err("V2");
        assert_eq!(
            local.message,
            awaken_session_execution_contract::NO_COLOCATED_WORKER,
            "V2"
        );
        // V3: a Repository nested in another Repository breaks a structural rule.
        assert!(
            delivery
                .validate_session_sandbox_layout(
                    "session",
                    &layout(
                        Worker,
                        vec![
                            repository("/workspace/repo"),
                            repository("/workspace/repo/child")
                        ],
                    ),
                )
                .is_err(),
            "V3"
        );
        // V4: a structurally valid Worker placement is admitted.
        delivery
            .validate_session_sandbox_layout(
                "session",
                &layout(Worker, vec![repository("/workspace/repo")]),
            )
            .expect("V4");

        // Skills: an empty selection pins nothing; a custom one needs a catalog.
        assert!(
            delivery
                .resolve_session_skills("workspace", &[])
                .await
                .expect("no Skill")
                .is_empty()
        );
        assert!(
            delivery
                .resolve_session_skills(
                    "workspace",
                    &[awaken_agent_contract::AgentSkillBinding::custom("review")]
                )
                .await
                .is_err(),
            "no catalog"
        );

        // Tools: a replacement reaches the held projection.
        install(&delivery, "session", Vec::new(), Vec::new());
        let mut tools = awaken_session_configuration_contract::SessionToolConfiguration::default();
        tools.toolsets.push(awaken_agent_contract::ToolsetPolicy {
            source: awaken_agent_contract::ToolsetSource::Agent,
            default: awaken_agent_contract::ToolExecutionPolicy::default(),
            overrides: Vec::new(),
        });
        delivery
            .replace_session_tools("session", tools.clone())
            .await
            .expect("replace tools");
        assert_eq!(
            delivery
                .projections()
                .installed("session")
                .expect("held")
                .tools(),
            &tools
        );
    }

    #[tokio::test]
    async fn coordinator_capabilities_and_live_inbox_follow_their_decision_tables() {
        use awaken_session_execution_contract::{CapabilityRuntime as _, LiveInboxAccess as _};
        let delivery = delivery(None).await;
        // C2: a Thread with no projection advertises the Workspace default
        // roster (no publication: none) and the built-in Hand tools.
        let flat = delivery.capabilities_for("flat").expect("C2");
        assert!(flat.delegates.is_empty(), "C2");
        assert_eq!(
            flat.builtin_tools.len(),
            crate::builtin_hand_tools().len(),
            "C2 Hand tools"
        );
        // C1: an installed Session advertises its own frozen roster.
        install(&delivery, "session", Vec::new(), Vec::new());
        let session = delivery.capabilities_for("session").expect("C1");
        assert_eq!(
            session.delegates,
            delivery
                .projections()
                .installed("session")
                .expect("held")
                .baseline()
                .delegate_ids,
            "C1"
        );
        // The Coordinator owns no attempt: every live inbox is inactive.
        assert!(
            !delivery
                .live_inbox_snapshot("session")
                .await
                .expect("snapshot")
                .active
        );
        assert!(matches!(
            delivery.live_inbox_queue("session", Vec::new()).await,
            Err(awaken_session_application_contract::LiveInboxError::Inactive)
        ));
    }

    #[tokio::test]
    async fn coordinator_thread_runs_follow_their_decision_table() {
        let delivery = delivery(None).await;
        // T1: a Managed Session root admits its own Runs.
        install(&delivery, "session", Vec::new(), Vec::new());
        let root = delivery
            .run_thread(super::thread_runs::ThreadRun {
                agent: None,
                thread: "session",
                input: Vec::new(),
                sink: None,
                data_subject_id: None,
            })
            .await
            .expect_err("T1");
        assert_eq!(
            root.message, "Managed Session roots must use Session-owned Run ingress",
            "T1"
        );
        // T3: an Agent with no publication does not run on a Coordinator.
        let unpublished = delivery
            .run_thread(super::thread_runs::ThreadRun {
                agent: Some("ghost"),
                thread: "flat",
                input: Vec::new(),
                sink: None,
                data_subject_id: None,
            })
            .await
            .expect_err("T3");
        assert!(
            unpublished.message.contains("has no publication"),
            "T3: {}",
            unpublished.message
        );
        // A Thread with nothing awaiting has nothing to resume, and an idle
        // Thread has nothing to interrupt.
        let resume = delivery
            .resume_thread(
                "flat",
                "call",
                awaken_runtime_contract::runs::RunResume::Permission(
                    awaken_agent_contract::agent::awaiting::PermissionDecision::Allow {
                        note: None,
                    },
                ),
                None,
            )
            .await
            .expect_err("no awaiting Run");
        assert_eq!(resume.message, "no awaiting run to resume");
        delivery
            .interrupt_thread("flat")
            .await
            .expect("idle interrupt");
    }

    #[tokio::test]
    async fn coordinator_outcomes_drive_only_installed_sessions() {
        // Change-impact design for sharing DispatchedRuns: O1 cold => refuse;
        // O2 installed+idle => no work; O3 preparation => one aggregate/no Run;
        // O4 grader Workspace replay is exact. The adapter borrows the same
        // queue/observation owners. DispatchPreparation alone binds the closed
        // execution format from its correlated frozen-input read; completion
        // registration still precedes enqueue. Its admission rules and the
        // completion/activation tables retain their original owners.
        use awaken_session_execution_contract::Outcomes as _;
        let delivery = delivery(None).await;
        // An Outcome needs the Session's installed projection.
        let cold = delivery
            .prepare_outcome("session", "outcome-1", "goal", "rubric", 2)
            .await
            .expect_err("cold");
        assert_eq!(cold.code, "session_projection_not_installed");
        install(&delivery, "session", Vec::new(), Vec::new());
        // No active Outcome: continuing drives nothing and dispatches no Run.
        struct IdleAdmission;
        #[async_trait::async_trait]
        impl awaken_session_execution_contract::PreparedSessionRunAdmission for IdleAdmission {
            async fn admit(
                &self,
                _: awaken_runtime_contract::RunActivation,
            ) -> awaken_runtime_contract::diagnostic::Result<
                awaken_agent_contract::agent::run::RunState,
            > {
                panic!("O2 idle continuation cannot execute a Run")
            }
        }
        assert!(
            delivery
                .continue_outcome("session", &IdleAdmission)
                .await
                .expect("idle continuation")
                .is_none()
        );
        // Preparation persists only the aggregate; no Worker Run is dispatched.
        delivery
            .prepare_outcome("session", "outcome-1", "goal", "rubric", 2)
            .await
            .expect("prepare");
        assert!(
            delivery
                .ports
                .owner
                .list_dispatches()
                .await
                .expect("dispatch rows")
                .is_empty(),
            "no Run before Session activity"
        );
        // A grader Thread is bound to one Workspace.
        let projections = delivery.projections();
        projections
            .register_thread_workspace("outcome/outcome-1/grader/1", "workspace")
            .expect("bind");
        assert!(
            projections
                .register_thread_workspace("outcome/outcome-1/grader/1", "other")
                .is_err()
        );
    }

    #[tokio::test]
    async fn ordinary_worker_runs_share_lowering_and_preserve_admission_refusals() {
        // Cause/effect decision table, crossed with fresh/extension mode:
        // D1 foreign Thread => refuse before queue effects; D2 matching Thread
        // with no configured model => canonical actionable refusal and no row;
        // D3 configured model but an existing different request at the same Run
        // id => refuse and preserve that exact original row. These use the real
        // queue and lowering, without a model, provider, worker or second store.
        use awaken_dispatch_contract::DispatchEnqueue as _;
        use awaken_runtime_contract::execution::RunExecutor as _;
        use awaken_runtime_contract::{RunActivation, RuntimeRunContext};

        for extension in [false, true] {
            for cause in ["foreign-thread", "unconfigured", "conflicting-request"] {
                let delivery = delivery(None).await;
                let thread = ThreadId("auxiliary".into());
                let run = RunId(format!("{cause}-{extension}"));
                let model = if cause == "unconfigured" {
                    awaken_dispatch_execution::session_dispatch::UNCONFIGURED_MODEL_REF
                } else {
                    "model"
                };
                let snapshot =
                    awaken_runtime_contract::ExecutableAgentSnapshot::builder("auxiliary")
                        .model(
                            awaken_runtime_contract::resolved::ModelBinding::parse(
                                "provider", model, "native",
                            )
                            .expect("literal test model binding"),
                        )
                        .build();
                let activation = RunActivation::new(
                    run.clone(),
                    if cause == "foreign-thread" {
                        ThreadId("foreign".into())
                    } else {
                        thread.clone()
                    },
                    snapshot,
                    vec![Message::text(
                        MessageId("input".into()),
                        Role::User,
                        "new input",
                    )],
                );
                let original = if cause == "conflicting-request" {
                    let mut old = activation.clone();
                    old.input[0] =
                        Message::text(MessageId("original".into()), Role::User, "original input");
                    let request = awaken_dispatch_contract::RunDispatch::new(old);
                    delivery
                        .ports
                        .owner
                        .enqueue(request.clone())
                        .await
                        .expect("D3 original row");
                    Some(request)
                } else {
                    None
                };
                let commit = delivery
                    .ports
                    .reads
                    .commit_for_read(&thread.0)
                    .await
                    .unwrap();
                let runs = super::dispatched_runs::DispatchedRuns::new(
                    delivery.ports.projections.as_ref(),
                    delivery.ports.owner.as_ref(),
                    &delivery.ports.completion,
                    thread,
                    commit,
                    extension,
                );
                let error = runs
                    .execute(activation, RuntimeRunContext::new())
                    .await
                    .expect_err("D1-D3 refuse");
                if cause == "unconfigured" {
                    assert!(
                        error.to_string().contains("No model is configured."),
                        "D2: {error}"
                    );
                }
                let observed = delivery
                    .ports
                    .owner
                    .observe_session_run_admission(&run)
                    .await
                    .unwrap();
                match (original, observed) {
                    (None, SessionRunAdmissionObservation::Missing) => {}
                    (Some(original), SessionRunAdmissionObservation::Live(stored)) => {
                        assert_eq!(*stored, original, "D3 original authority retained")
                    }
                    other => panic!("unexpected admission side effect: {other:?}"),
                }
            }
        }
    }

    #[tokio::test]
    async fn coordination_read_role_retains_the_original_threads_without_placement() {
        // Cause/effect graph: C1 retain only the published coordination reads;
        // C2 release all other roles and the placement; C3 drop the read role.
        // R1=C1+C2 -> placement and unrelated Memory recovery are released,
        // while the original Threads and committed reads answer the query.
        // R2=R1+C3 -> those same collaborators are released. All eight read
        // operations keep the existing CoordinatedThreads implementation;
        // this table detects an unnecessary facade lifetime, not new policy.
        let sessions = Arc::new(delivery(None).await);
        let parent = Arc::downgrade(&sessions);
        let memory = Arc::downgrade(&sessions.ports.memory_extractions);
        let threads = Arc::downgrade(&sessions.threads);
        let reads = Arc::downgrade(&sessions.ports.reads);
        let roles = sessions.session_execution_roles();
        let selected = roles.coordination_reads.clone();
        drop(roles);
        drop(sessions);
        assert!(parent.upgrade().is_none(), "R1 placement released");
        assert!(memory.upgrade().is_none(), "R1 unrelated Memory released");
        assert!(threads.upgrade().is_some(), "R1 original Threads retained");
        assert!(
            reads.upgrade().is_some(),
            "R1 original committed reads retained"
        );
        assert!(
            selected
                .delegated_runs("coordination-read-role")
                .await
                .expect("R1 query")
                .is_empty()
        );
        drop(selected);
        assert!(threads.upgrade().is_none(), "R2 Threads released");
        assert!(reads.upgrade().is_none(), "R2 committed reads released");
    }

    #[tokio::test]
    async fn delivery_roles_keep_publication_without_retaining_process_services() {
        // C1 full Worker placement plus its budget/read/coordination roles;
        // C2 the root releases the actual ProcessDispatch; C3 retained roles
        // still read original committed truth and publish through the owner.
        // L1=C1+C2 -> service owner released despite all application roles;
        // L2=L1+C3 -> same successful reads and outbox, no replacement service.
        // This checks the complete placement, not merely a narrow trait cast.
        let clock = awaken_time::system_clock();
        let owner = Arc::new(AnyCoordinatorDispatch::from_durable_store(
            awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
            None,
        ));
        let (sessions, process) = delivery_with_process(None, clock, owner).await;
        let sessions = Arc::new(sessions);
        let weak = Arc::downgrade(&process);
        let publication = process.publication();
        let roles = sessions.session_execution_roles();
        drop(process);
        assert!(
            weak.upgrade().is_none(),
            "L1 no service retained by delivery/Threads/budget"
        );
        roles
            .budget
            .session_usage("publication-session")
            .await
            .expect("L2 budget reads");
        assert!(
            roles
                .coordination_reads
                .delegated_runs("publication-session")
                .await
                .expect("L2 original committed reads")
                .is_empty()
        );
        publication
            .publish_staged(&sessions.ports.owner)
            .await
            .expect("L2 original outbox");
        sessions
            .threads()
            .durable_run_control()
            .expect("L2 original control owner");
        assert!(
            weak.upgrade().is_none(),
            "L2 no replacement service constructed"
        );
    }

    #[tokio::test]
    async fn budget_role_retains_only_its_own_delivery_collaborators() {
        // Cause/effect graph: C1 retain only the published BudgetAccounting;
        // C2 drop the placement aggregate and every other selected role;
        // C3 drop the budget role itself. E1 C1+C2 releases the aggregate and
        // unrelated Memory recovery while retaining the original committed
        // reads; E2 a budget query still uses those live collaborators;
        // E3 C3 releases those reads. This detects a forwarding facade that
        // accidentally gives a narrow caller the entire placement lifetime.
        // | Rule | placement | budget role | Effect |
        // | B1 | dropped | retained | E1+E2 |
        // | B2 | dropped | dropped | E3 |
        let sessions = Arc::new(delivery(None).await);
        let parent = Arc::downgrade(&sessions);
        let extractions = Arc::downgrade(&sessions.ports.memory_extractions);
        let reads = Arc::downgrade(&sessions.ports.reads);
        let roles = sessions.session_execution_roles();
        let budget = roles.budget.clone();
        drop(roles);
        drop(sessions);
        assert!(parent.upgrade().is_none(), "B1/E1 aggregate released");
        assert!(
            extractions.upgrade().is_none(),
            "B1/E1 Memory recovery released"
        );
        assert!(reads.upgrade().is_some(), "B1/E1 required reads retained");
        budget
            .session_usage("budget-role-thread")
            .await
            .expect("B1/E2");
        drop(budget);
        assert!(reads.upgrade().is_none(), "B2/E3 reads released");
    }

    #[tokio::test]
    async fn the_worker_placement_role_set_realizes_no_session() {
        // A configured durable Memory authority is an input collaborator;
        // constructing Worker-placement roles still acquires no local
        // realization capability and starts no extraction or Environment.
        let delivery = Arc::new(delivery(None).await);
        let roles = delivery.session_execution_roles();
        assert!(roles.placement.colocated().is_none());
        assert!(roles.placement.require_colocated().is_err());
    }

    #[tokio::test]
    async fn coordinator_durable_operations_follow_their_decision_table() {
        use awaken_session_execution_contract::DurableRunOperations as _;
        let delivery = delivery(Some(skill())).await;
        install(&delivery, "session", Vec::new(), Vec::new());
        delivery
            .reserve_session_run(command("session", "hello"))
            .await
            .expect("reserve");
        // O1: a live row's cancellation is its durable intent.
        delivery.cancel("session", "session-run").await.expect("O1");
        // O2: an unknown Run reaches no owner.
        assert!(delivery.cancel("session", "unknown").await.is_err(), "O2");
        // O3: the Coordinator owns no attempt.
        assert!(delivery.pause("session", None).await.is_err(), "O3 pause");
        assert!(
            delivery.wake("session", "session-run").await.is_err(),
            "O3 wake"
        );
        // O5: nothing awaits on the Thread.
        assert!(
            delivery.deliver("session", true).await.is_err(),
            "O5 deliver"
        );
        assert!(
            delivery.resume("session", "go on".into()).await.is_err(),
            "O5 resume"
        );
        // O6: reconciliation is the Coordinator loop's.
        assert!(delivery.reconcile("session").await.is_err(), "O6");
        // Operational reads list the Thread's rows.
        let rows = delivery.dispatches("session").await.expect("rows");
        assert_eq!(rows.len(), 1, "one reserved row");
        assert!(
            delivery
                .dead_letters("session")
                .await
                .expect("dead letters")
                .is_empty()
        );
        assert!(
            !delivery
                .requeue_dead_letter("session", "session-run")
                .await
                .expect("requeue")
        );
    }

    #[tokio::test]
    async fn task_callback_membership_reads_native_descendants_from_one_physical_session() {
        use awaken_agent_contract::agent::delegation::{
            DelegationId, DelegationLimits, DelegationRegistry, RequestDelegation,
        };
        use awaken_agent_contract::agent::state::StateKey as _;
        use awaken_agent_contract::thread::commit::RunDisposition;
        use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
        use awaken_runtime_contract::RunDelegations;
        use awaken_session_execution_contract::SessionCoordinationReads as _;
        // M1 root -> direct child -> grandchild are original committed Native
        // registry edges, exact canonical child Run/Thread identities. M2 root
        // advances and its old registry is logically cleared => historical
        // membership remains for ACKs; no new relationship is granted. M3 the
        // same child is reattributed in a later otherwise-valid registry =>
        // uncertainty, never latest-Agent wins. M4 another partition/unknown
        // Thread is not a callback target. The public observed-root projection
        // retains its original nonrecursive semantics and fence. Actual Memory
        // commits and existing Session-scoped reader are used, not scripted links.
        let sessions = delivery(None).await;
        let parent = awaken_session_types::SessionId::from("native-callback-parent");
        let root_thread = ThreadId(parent.to_string());
        // M0 the common projection fixture freezes Agent `agent`; this case
        // deliberately selects `parent`. Compile the matching baseline from
        // the same selected inputs before installation, including its original
        // fingerprint law. Do not overwrite a fingerprint or relax identity.
        let mut root_projection = projection(1, "provider/model", Some(publication("parent")));
        let baseline = &root_projection.baseline;
        root_projection.baseline = awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment: baseline.environment.clone(),
                runtime_placement: baseline.runtime_placement,
                mcp_authoring: baseline.mcp_authoring.clone(),
                agent_id: "parent".into(),
                agent_revision: baseline.agent_revision,
                model: baseline.model.clone(),
                model_override: baseline.model_override.clone(),
                runtime: baseline.runtime.clone(),
                delegate_ids: baseline.delegate_ids.clone(),
                toolsets: baseline.toolsets.clone(),
                mounts: baseline.mounts.clone(),
                env: baseline.env.clone(),
                prompts: baseline.prompts.clone(),
                transcript_prefix: baseline.transcript_prefix.clone(),
            },
        );
        sessions
            .projections()
            .install(parent.as_str(), root_projection)
            .unwrap();
        let root_run = RunId("native-parent-run".into());
        let registry = |run: &RunId,
                        agent: &str,
                        call: &str,
                        target: &str,
                        parent_origin: Option<
            &awaken_agent_contract::agent::delegation::DelegationOrigin,
        >| {
            let id = DelegationId::for_parent_call(run, call);
            let mut registry = DelegationRegistry::new(
                run.clone(),
                agent,
                parent_origin
                    .map(|origin| origin.agent_lineage.clone())
                    .unwrap_or_default(),
                parent_origin.map_or(0, |origin| origin.depth),
                DelegationLimits::default(),
            );
            registry
                .request(RequestDelegation {
                    id: id.clone(),
                    parent_call_id: call.into(),
                    target_agent_id: target.into(),
                    recursive_self: false,
                    child_run_id: id.child_run_id(),
                })
                .unwrap();
            (registry, id.child_run_id())
        };
        let (mut first, child_run) = registry(&root_run, "parent", "first", "child-agent", None);
        let child = ThreadId(child_run.0.clone());
        let child_origin = first
            .child_origin(&DelegationId::for_parent_call(&root_run, "first"))
            .unwrap();
        let (nested, grand_run) = registry(
            &child_run,
            "child-agent",
            "nested",
            "grand-agent",
            Some(&child_origin),
        );
        let grand = ThreadId(grand_run.0.clone());
        let commit = sessions
            .ports
            .reads
            .commit_for_read(parent.as_str())
            .await
            .unwrap();
        for (thread, run, registry) in [
            (root_thread.clone(), root_run.clone(), Some(first.clone())),
            (child.clone(), child_run.clone(), Some(nested)),
            (grand.clone(), grand_run.clone(), None),
        ] {
            commit
                .commit(awaken_agent_contract::ThreadCommit::assemble(
                    thread.clone(),
                    // M0 the root remains active until its one M2 terminal
                    // commit. Native descendants are already terminal and
                    // remain legitimate callback members of this Session.
                    if thread == root_thread {
                        RunDisposition::running(run)
                    } else {
                        RunDisposition::ended(
                            run,
                            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                        )
                    },
                    true,
                    Vec::new(),
                    registry
                        .map(|registry| RunDelegations::write(&Some(registry)))
                        .into_iter()
                        .collect(),
                    Vec::new(),
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&parent, &child, "parent")
                .await
                .unwrap()
                .map(|(agent, _)| agent),
            Some("child-agent".into()),
            "M1 direct"
        );
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&parent, &grand, "parent")
                .await
                .unwrap()
                .map(|(agent, _)| agent),
            Some("grand-agent".into()),
            "M1 nested"
        );
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&parent, &ThreadId("not-a-child".into()), "parent")
                .await
                .unwrap(),
            None,
            "M4 unknown"
        );
        let root_prefix = sessions
            .ports
            .reads
            .session_thread_recovery_snapshot(parent.as_str(), &root_thread)
            .await
            .unwrap()
            .unwrap();
        assert!(
            sessions
                .ports
                .reads
                .coordinated_threads(parent.as_str(), Some(&root_prefix))
                .await
                .unwrap()
                .is_empty(),
            "M1 public fenced query unchanged"
        );
        // M5 a retained accepted SEND batch survives transcript compaction.
        // Its original coordinated creation origin remains independent from
        // the subsequent Native child's actual lineage/depth.
        // The original grandchild Run has ended. Its next legal FollowUp
        // Run owns this SEND; do not append to a terminal Run or replace the
        // Thread's immutable Native creation origin.
        let sending_run = awaken_session_execution_contract::session_run_id(
            &grand.0.as_str().into(),
            "grandchild-send-follow-up",
        );
        let call = "send-reset";
        let operation =
            awaken_tool_contract::ToolBatch::operation_id_for_step(&sending_run, 0, call);
        let sent_thread = awaken_session_execution_contract::coordinated_thread_id(
            &parent,
            &sending_run,
            &operation,
        );
        let sent_run = awaken_session_execution_contract::coordinated_run_id(
            &parent,
            &sent_thread,
            &sending_run,
            &operation,
        );
        let send_origin = awaken_dispatch_execution::coordinated_run_origin(
            parent.as_str(),
            &operation,
            sending_run.clone(),
            call.into(),
            "parent".into(),
        );
        let mut batch = awaken_tool_contract::ToolBatch::for_step(
            sending_run.clone(),
            0,
            [(
                awaken_tool_contract::tool::ToolCall {
                    call_id: call.into(),
                    tool_id: awaken_ext_builtin_tools::SEND_MESSAGE.into(),
                    arguments: serde_json::json!({"agent_id":"roster-agent", "message":"spawn"}),
                },
                awaken_tool_contract::tool::ToolRecoveryPolicy::durable_request(),
            )],
        )
        .unwrap();
        batch.mark_executing(call).unwrap();
        batch
            .complete(awaken_tool_contract::tool::ToolOutput::ok(call, "accepted"))
            .unwrap();
        batch
            .set_result_messages(
                call,
                vec![Message::new(
                    MessageId::tool_result(call),
                    Role::Tool,
                    vec![
                        awaken_agent_contract::agent::content::ContentBlock::tool_result_with_error(
                            call,
                            vec![awaken_agent_contract::agent::content::ContentBlock::text(
                                serde_json::to_string(
                                    &awaken_ext_builtin_tools::AgentMessageReceipt {
                                        session_thread_id: sent_thread.0.clone(),
                                        accepted: true,
                                    },
                                )
                                .unwrap(),
                            )],
                            false,
                        ),
                    ],
                )],
            )
            .unwrap();
        commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                grand.clone(),
                RunDisposition::ended(
                    sending_run.clone(),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                vec![
                    awaken_tool_contract::ActiveToolBatch::write(&Some(batch)),
                    awaken_tool_contract::ActiveToolBatch::write(&None),
                ],
                Vec::new(),
            ))
            .await
            .unwrap();
        let (after_send, leaf_run) = registry(
            &sent_run,
            "roster-agent",
            "leaf",
            "leaf-agent",
            Some(&send_origin),
        );
        let leaf = ThreadId(leaf_run.0.clone());
        let leaf_origin = after_send
            .child_origin(&DelegationId::for_parent_call(&sent_run, "leaf"))
            .unwrap();
        for (thread, run, state) in [
            (
                sent_thread.clone(),
                sent_run,
                vec![RunDelegations::write(&Some(after_send))],
            ),
            (leaf.clone(), leaf_run, Vec::new()),
        ] {
            commit
                .commit(awaken_agent_contract::ThreadCommit::assemble(
                    thread,
                    RunDisposition::ended(
                        run,
                        awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                    ),
                    true,
                    Vec::new(),
                    state,
                    Vec::new(),
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&parent, &sent_thread, "parent")
                .await
                .unwrap(),
            Some(("roster-agent".into(), send_origin)),
            "M5 original SEND creation metadata"
        );
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&parent, &leaf, "parent")
                .await
                .unwrap(),
            Some(("leaf-agent".into(), leaf_origin)),
            "M5 original Native lineage after coordinated creation"
        );
        // M6 direct/recursive/after-SEND callbacks choose the original queued
        // snapshot, including a None-version descendant frozen at v1. The
        // process has no live publication source: any accidental current lookup
        // fails. Actual graph membership and the original child compiler/queue
        // are used; the new Run keeps this Thread's original creation origin.
        for logical in [&child, &grand, &sent_thread, &leaf] {
            let (agent, creation) = sessions
                .threads
                .task_callback_source(&parent, logical, "parent")
                .await
                .unwrap()
                .unwrap();
            let original_run = sessions
                .ports
                .reads
                .session_thread_recovery_snapshot(parent.as_str(), logical)
                .await
                .unwrap()
                .unwrap()
                .latest_run_id
                .unwrap();
            let mut snapshot = publication(&agent);
            snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
            snapshot.metadata.source.revision = 1;
            snapshot.resolved_spec.plugin_config.agent.delegates.push(
                awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                    agent_id: awaken_runtime_contract::snapshot::AgentId("none-child".into()),
                    source_revision: None,
                    recursive_self: false,
                },
            );
            snapshot
                .resolved_spec
                .plugin_ids
                .push("background_task".into());
            snapshot.recompute_fingerprint().unwrap();
            let mut selected = publication("none-child");
            selected.metadata.source.agent_id = selected.root_agent_id.clone();
            selected.metadata.source.revision = 1;
            selected.recompute_fingerprint().unwrap();
            let inherited =
                awaken_agent_contract::toolset::InheritedToolsets::with_caller(Vec::new(), None);
            let activation = RunActivation::new(
                original_run.clone(),
                logical.clone(),
                snapshot.clone(),
                Vec::new(),
            )
            .with_delegation_origin(creation.clone())
            .with_inherited_toolsets(Some(inherited.clone()))
            .with_data_subject(Some(awaken_runtime_contract::DataSubjectId(
                "child-content-owner".into(),
            )));
            settle_task_origin(
                &sessions,
                &logical.0,
                &original_run.0,
                activation.clone(),
                vec![selected.clone()],
                Some(parent.as_str()),
            )
            .await;
            let source = sessions
                .threads
                .task_origin_execution(&parent, logical, &original_run)
                .await
                .unwrap();
            assert_eq!(source.0, activation, "M6 original queued execution");
            let operation = format!("callback:{}", logical.0);
            let new_run = awaken_session_execution_contract::session_run_id(
                &logical.0.as_str().into(),
                &operation,
            );
            let task = AdmitSessionRun {
                session_id: parent.clone(),
                agent_id: agent.clone(),
                operation_id: operation.clone(),
                run_id: new_run.clone(),
                messages: vec![Message::text(MessageId(format!("input:{}",logical.0)), Role::System, "Task needs input")],
                data_subject_id: None,
                traceparent: None,
                execution_requirements: Default::default(),
                replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
                private_task_input: Some(serde_json::from_value(serde_json::json!({
                    "version":1,"observations":[],"tenure": {
                        "thread":logical.0,"partition":{"kind":"session","session_thread":parent},
                        "run":original_run.0,"task":"task","holder":"holder","epoch":1,
                    }
                })).unwrap()),
            };
            let fingerprint = task.command_fingerprint();
            sessions
                .threads
                .admit_coordinated_run(awaken_session_execution_contract::CoordinatedRunCommand {
                    intent: awaken_session_execution_contract::CoordinatedRunIntent::FollowUp,
                    session_id: parent.clone(),
                    thread_id: logical.clone(),
                    run_id: new_run.clone(),
                    parent_run_id: original_run,
                    parent_call_id: "task".into(),
                    operation_id: operation,
                    snapshot: snapshot.clone(),
                    message: String::new(),
                    session_activity_epoch: Some(9),
                    max_unarchived_threads: 24,
                    task_attention: Some(task),
                    task_attention_execution: Some(source),
                })
                .await
                .unwrap();
            let SessionRunAdmissionObservation::Live(row) = sessions
                .ports
                .owner
                .observe_session_run_admission(&new_run)
                .await
                .unwrap()
            else {
                panic!("M6 callback admitted");
            };
            assert_eq!(
                row.activation.snapshot, snapshot,
                "M6 correct original Agent/revision"
            );
            assert_eq!(
                row.agent_publications,
                vec![selected],
                "M6 ordinary None selection remains v1"
            );
            assert_eq!(
                row.activation.delegation_origin,
                Some(creation),
                "M6 callback does not reset lineage/depth"
            );
            assert_eq!(
                row.activation.inherited_toolsets(),
                Some(&inherited),
                "M6 inherited constraints"
            );
            assert_eq!(
                row.activation.data_subject_id, activation.data_subject_id,
                "M6 content owner"
            );
            assert_eq!(
                row.session_command_fingerprint,
                Some(fingerprint),
                "M6 original Task input fingerprint"
            );
            assert_eq!(
                row.session_thread_id().0,
                parent.as_str(),
                "M6 same physical partition"
            );
        }
        first.end_parent();
        commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                root_thread.clone(),
                RunDisposition::ended(
                    root_run.clone(),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                vec![
                    RunDelegations::write(&Some(first)),
                    RunDelegations::write(&None),
                ],
                Vec::new(),
            ))
            .await
            .unwrap();
        commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                root_thread.clone(),
                RunDisposition::ended(
                    RunId("later-parent-run".into()),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&parent, &grand, "parent")
                .await
                .unwrap()
                .map(|(agent, _)| agent),
            Some("grand-agent".into()),
            "M2 retained ancestry after end/clear/new Run"
        );
        let other = awaken_session_types::SessionId::from("another-callback-parent");
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&other, &grand, "parent")
                .await
                .unwrap(),
            None,
            "M4 no cross-partition inference"
        );
        // M3 the earlier parent has already ended. A fresh active Run may
        // record state, and its same canonical child cannot be reattributed.
        // The positive observation below proves this is a valid additional
        // Native edge before the mutation, not a forged post-terminal write.
        let mutation_run = awaken_session_execution_contract::session_run_id(
            &parent,
            "membership-mutation-parent",
        );
        let (original_membership, child_run) =
            registry(&mutation_run, "parent", "first", "child-agent", None);
        let mutation_child = ThreadId(child_run.0.clone());
        commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                root_thread.clone(),
                RunDisposition::running(mutation_run.clone()),
                true,
                Vec::new(),
                vec![RunDelegations::write(&Some(original_membership))],
                Vec::new(),
            ))
            .await
            .unwrap();
        commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                mutation_child.clone(),
                RunDisposition::ended(
                    child_run.clone(),
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ))
            .await
            .unwrap();
        assert_eq!(
            sessions
                .threads
                .task_callback_source(&parent, &mutation_child, "parent")
                .await
                .unwrap()
                .map(|(agent, _)| agent),
            Some("child-agent".into()),
            "M3 original active registry is readable before reattribution",
        );
        let (reattributed, same_child) =
            registry(&mutation_run, "parent", "first", "different-agent", None);
        assert_eq!(same_child, child_run, "M3 same canonical identity");
        commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                root_thread,
                RunDisposition::ended(
                    mutation_run,
                    awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                ),
                true,
                Vec::new(),
                vec![RunDelegations::write(&Some(reattributed))],
                Vec::new(),
            ))
            .await
            .unwrap();
        assert!(
            sessions
                .threads
                .task_callback_source(&parent, &grand, "parent")
                .await
                .is_err(),
            "M3 history cannot reassign a child"
        );
    }

    #[tokio::test]
    async fn exact_task_receipts_survive_null_or_corrupt_sqlite_source_while_fresh_input_is_unavailable()
     {
        use awaken_dispatch_contract::{
            DispatchClaims as _, DispatchOutcome, DispatchSettlement as _,
        };
        use awaken_session_execution_contract::SessionCoordinationReads as _;
        // A1 origin and target callback both reach real SQLite Done; A2 the
        // target's retained payload later corrupts => same exact lightweight
        // receipt, not another reservation. A3 origin source is NULL/corrupt =>
        // accepted target still has its receipt; a fresh operation is retryable
        // Unavailable with zero new dispatch/feed. No current source backfill.
        // This exercises Core receipt + frozen-source roles on one exact Store.
        // App's real-SQLite-activity test owns the separate matching activity
        // requirement; this is not claimed as a complete HTTP/product journey.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("task-source-receipts.sqlite");
        let clock = awaken_time::system_clock();
        let store =
            awaken_dispatch::SqliteDispatchStore::open(path.to_str().unwrap(), clock.clone())
                .await
                .unwrap();
        let owner = Arc::new(AnyCoordinatorDispatch::from_durable_store(store, None));
        let (delivery, _) = delivery_with_process(None, clock, owner).await;
        let session = "sqlite-source-ack";
        install(&delivery, session, Vec::new(), Vec::new());
        let origin = "sqlite-source-origin";
        settle_task_origin(
            &delivery,
            session,
            origin,
            RunActivation::new(
                RunId(origin.into()),
                ThreadId(session.into()),
                reviewing_agent(),
                Vec::new(),
            ),
            Vec::new(),
            None,
        )
        .await;
        let mut task = command(session, "Task requires input");
        task.private_task_input = Some(serde_json::from_value(serde_json::json!({"version":1,"observations":[],"tenure":{"thread":session,"partition":{"kind":"thread"},"run":origin,"task":"task","holder":"holder","epoch":1}})).unwrap());
        delivery.reserve_session_run(task.clone()).await.unwrap();
        delivery
            .activate_session_run(SessionRunDelivery {
                session_id: session.into(),
                run_id: task.run_id.clone(),
                session_activity_epoch: 7,
            })
            .await
            .unwrap();
        // A0 the same real SQLite callback is RemoteOnly, so a local
        // claimer is ineligible. The shared candidate advertises these fixed
        // formats, not physical Task execution; retain all A1-A3 source/ACK
        // assertions and use the existing strict compatible claim transition.
        assert!(
            delivery
                .ports
                .owner
                .claim_run(
                    &task.run_id,
                    "source-ack-worker",
                    30_000,
                    &Default::default(),
                )
                .await
                .unwrap()
                .is_none(),
            "A0 local claim cannot consume a Worker-only callback",
        );
        let worker = crate::test_support::task_callback_worker("source-ack-worker");
        let SessionRunAdmissionObservation::Live(request) = delivery
            .ports
            .owner
            .observe_session_run_admission(&task.run_id)
            .await
            .unwrap()
        else {
            panic!("A0 original activated callback");
        };
        awaken_worker_contract::can_assign(
            &worker,
            &request.placement,
            None,
            false,
            delivery.ports.clock.now_ms(),
        )
        .unwrap_or_else(|failure| {
                    panic!(
                        "A0 declared candidate satisfies the original SQLite placement law: {failure:?}; static_eligibility={:?}; placement={:?}; worker={:?}",
                        awaken_worker_contract::can_claim(&worker.manifest, &request.placement),
                        request.placement,
                        worker,
                    )
                });
        let claimed = delivery
            .ports
            .owner
            .claim_run_compatible(&task.run_id, &worker, 30_000)
            .await
            .unwrap()
            .unwrap();
        delivery
            .ports
            .owner
            .settle(
                &task.run_id,
                claimed.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap();
        let sql = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let before = delivery
            .ports
            .owner
            .completion_events_after(0, usize::MAX)
            .await
            .unwrap();
        sqlx::query("UPDATE runtime_dispatch_completion SET retained_request='{}' WHERE run_id=?1")
            .bind(&task.run_id.0)
            .execute(&sql)
            .await
            .unwrap();
        assert_eq!(
            delivery
                .threads
                .session_run_command_receipt(&task)
                .await
                .unwrap(),
            Some(SessionRunReservation::Completed),
            "A2 target source corruption does not invalidate receipt"
        );
        assert_eq!(
            delivery
                .threads
                .task_origin_execution(&session.into(), &ThreadId(session.into()), &task.run_id)
                .await
                .unwrap_err()
                .kind,
            awaken_runtime_contract::runs::RunErrorKind::Unavailable,
            "A2 fresh source demand is Unknown"
        );
        for payload in [None, Some("{}"), Some("broken-json")] {
            sqlx::query(
                "UPDATE runtime_dispatch_completion SET retained_request=?1 WHERE run_id=?2",
            )
            .bind(payload)
            .bind(origin)
            .execute(&sql)
            .await
            .unwrap();
            assert_eq!(
                delivery
                    .threads
                    .session_run_command_receipt(&task)
                    .await
                    .unwrap(),
                Some(SessionRunReservation::Completed),
                "A3 original receipt independent of origin source"
            );
            let mut fresh = task.clone();
            fresh.operation_id.push_str("-fresh");
            fresh.run_id.0.push_str("-fresh");
            fresh.messages[0].id.0.push_str("-fresh");
            assert_eq!(
                delivery
                    .reserve_session_run(fresh.clone())
                    .await
                    .unwrap_err()
                    .kind,
                awaken_runtime_contract::runs::RunErrorKind::Unavailable,
                "A3 no current fallback"
            );
            assert!(
                delivery
                    .ports
                    .owner
                    .dispatch_summary(&fresh.run_id)
                    .await
                    .unwrap()
                    .is_none(),
                "A3 zero new work"
            );
            assert_eq!(
                delivery
                    .ports
                    .owner
                    .completion_events_after(0, usize::MAX)
                    .await
                    .unwrap(),
                before,
                "A2/A3 same public completion feed"
            );
        }
        sql.close().await;
    }
}
