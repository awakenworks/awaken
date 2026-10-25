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

mod inputs;
mod roles;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::thread::Id as ThreadId;

use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::ProcessDispatch;
use awaken_resource_contract::{LiveResourceBindingVerifier, RepositoryBindingVerifier};
use awaken_session_application_contract::StepOutcome;
use awaken_session_configuration_contract::SkillBundleSource;
use awaken_session_configuration_contract::resource::binding_checks::verify_manifest_bindings;
use awaken_session_configuration_contract::skill_delivery::{
    load_pinned_skills, skill_store_run_error,
};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunDelivery,
    SessionRunReservation,
};
use awaken_session_realization_contract::ResourceOperationFence;
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
    /// The process's dispatch, publishing activated rows to claiming Workers.
    pub dispatch: Arc<ProcessDispatch>,
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
    budgets: SessionBudgets,
    /// One step projection at a time per Session, so concurrent observers
    /// publish each committed suffix once and in order.
    step_projections: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl WorkerPlacedSessions {
    #[must_use]
    pub fn new(ports: WorkerPlacement) -> Self {
        let threads = Arc::new(CoordinatedThreads::new(CoordinationAuthorities {
            owner: Some(ports.owner.clone()),
            dispatch: ports.dispatch.clone(),
            reads: ports.reads.clone(),
            lowering: ports.projections.clone(),
            publications: ports.projections.publication_source(),
            wake: ports.wake.clone(),
            clock: ports.clock.clone(),
        }));
        let budgets =
            SessionBudgets::new(threads.clone(), ports.dispatch.clone(), ports.reads.clone());
        Self {
            ports,
            threads,
            budgets,
            step_projections: Mutex::default(),
        }
    }

    /// The frozen projections Runs are admitted over.
    #[must_use]
    pub fn projections(&self) -> &Arc<SessionProjections> {
        &self.ports.projections
    }

    /// The Agent coordination of the Sessions' Threads.
    #[must_use]
    pub fn threads(&self) -> &CoordinatedThreads {
        &self.threads
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
        activate_session_run_reservation(&self.ports.owner, &self.ports.dispatch, delivery).await
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
    use awaken_dispatch_contract::{SessionRunAdmissionObservation, SessionRunAdmissionReads as _};
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, ResourceRegistryError, RetentionPolicy,
        SkillBundleFile, SkillVersion, skill_bundle_sha256,
    };
    use awaken_session_configuration_contract::SkillBundleSourceError;
    use awaken_session_configuration_contract::resource::{
        ResolvedInput, ResolvedInputSource, ResolvedSessionResources, ResolvedSkillBinding,
    };
    use awaken_session_execution_contract::SessionRunExecutionRequirements;

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
    struct Memory;

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

    fn memory_input(version: u64) -> ResolvedInput {
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

    /// Committed truth the reservation and activation rules never read.
    struct NoCommits;

    #[async_trait::async_trait]
    impl crate::SessionCommitSource for NoCommits {
        async fn open_for_read(
            &self,
            _session: &str,
        ) -> Result<Arc<crate::SessionCommit>, RunError> {
            Err(RunError::unavailable("no committed truth under test"))
        }

        async fn durable_thread_exists(&self, _thread: &str) -> Result<bool, RunError> {
            Ok(false)
        }
    }

    fn delivery(stored: Option<SkillVersion>) -> WorkerPlacedSessions {
        let clock = awaken_time::system_clock();
        let projections = Arc::new(SessionProjections::new("local", None));
        let hub = Arc::new(ThreadEventHub::new());
        let completion = Arc::new(CompletionRegistry::new(hub.clone()));
        WorkerPlacedSessions::new(WorkerPlacement {
            projections: projections.clone(),
            dispatch: Arc::new(ProcessDispatch::new(
                awaken_deployment_config::ExecutionTopology {
                    ingress: awaken_deployment_config::RunIngress::Durable,
                    local: awaken_deployment_config::LocalExecution::RemoteWorkersOnly,
                    lease_owner: awaken_deployment_config::LeaseOwnerId::new("coordinator")
                        .expect("lease owner"),
                    lost_wake_recovery_bound: std::time::Duration::from_millis(5),
                    queue_depth: awaken_deployment_config::QueueDepthSource::External,
                },
                clock.clone(),
                completion.clone(),
                awaken_dispatch_execution::wake::DispatchHint::new(Arc::new(
                    awaken_dispatch_execution::LocalWakeSignal::new(),
                )),
            )),
            owner: Arc::new(AnyCoordinatorDispatch::from_durable_store(
                awaken_dispatch::MemoryDispatchStore::new(clock),
                None,
            )),
            reads: Arc::new(CommittedReads::new(Arc::new(NoCommits), projections)),
            hub,
            completion,
            skill_bundles: stored.map(|version| {
                Arc::new(Store(version)) as Arc<dyn SkillBundleSource<ResourceOperationFence>>
            }),
            resource_bindings: ResourceBindingVerifiers {
                memory: Some(Arc::new(Memory)),
                repositories: None,
            },
            skill_catalog: None,
            wake: None,
            clock: awaken_time::system_clock(),
            default_model: "provider/default".into(),
        })
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
        let delivery = delivery(Some(stored.clone()));
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
        let missing = delivery_without_store();
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
    }

    fn delivery_without_store() -> WorkerPlacedSessions {
        delivery(None)
    }

    #[tokio::test]
    async fn coordinator_activation_publishes_only_a_committed_reservation() {
        // A1: a zero activity epoch is refused before the store is touched.
        let delivery = delivery(Some(skill()));
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
        let delivery = delivery(None);

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
        use awaken_session_execution_contract::{CapabilityRuntime as _, LiveInboxRuntime as _};
        let delivery = delivery(None);
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
            Err(awaken_session_execution_contract::LiveInboxError::Inactive)
        ));
    }
}
