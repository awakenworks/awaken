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
use awaken_session_application_contract::StepOutcome;
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
use awaken_session_types::RunError;

use crate::session_run_activation::{activate_session_run_reservation, read_settled_phase};
use crate::session_run_reservation::{
    SessionRunInputs, reserve_session_run, validate_session_run_command,
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
    ) -> awaken_session_execution_contract::SessionExecutionRoles {
        awaken_session_execution_contract::SessionExecutionRoles {
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
    /// | C3 | the projection pins no Agent publication | refused |
    /// | C4 | a pinned Skill bundle is missing or mismatched | refused; nothing is reserved |
    /// | C5 | every input holds | one unclaimable row placed on a registered Worker, carrying the desired manifest and the Session's frozen model |
    pub async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
        validate_session_run_command(&command)?;
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
            SessionRunInputs {
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
        DispatchReads as _, SessionRunAdmissionObservation, SessionRunAdmissionReads as _,
    };
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, ResourceRegistryError, RetentionPolicy,
        SkillBundleFile, SkillVersion, skill_bundle_sha256,
    };
    use awaken_session_configuration_contract::SkillBundleSourceError;
    use awaken_session_configuration_contract::resource::{
        ResolvedInput, ResolvedInputSource, ResolvedSessionResources, ResolvedSkillBinding,
    };
    use awaken_session_execution_contract::{
        ChildRunCoordination as _, SessionRunExecutionRequirements,
    };

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
        delivery_with_process(stored).await.0
    }

    async fn delivery_with_process(
        stored: Option<SkillVersion>,
    ) -> (
        WorkerPlacedSessions,
        Arc<awaken_dispatch_execution::ProcessDispatch>,
    ) {
        let clock = awaken_time::system_clock();
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
                local: awaken_deployment_config::LocalExecution::RemoteWorkersOnly,
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
            owner: Arc::new(AnyCoordinatorDispatch::from_durable_store(
                awaken_dispatch::MemoryDispatchStore::new(clock),
                None,
            )),
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
            clock: awaken_time::system_clock(),
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
                    assert_eq!(error.kind, awaken_session_types::RunErrorKind::BadRequest);
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
        let input = awaken_session_application_contract::PrivateTaskInput::new(vec![
            serde_json::json!({"version": 1, "task_id": "task"}),
        ])
        .expect("R7 input");
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
                awaken_session_application_contract::RunResume::Permission(
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
        let (sessions, process) = delivery_with_process(None).await;
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
}
