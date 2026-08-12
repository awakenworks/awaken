use std::collections::BTreeSet;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use awaken_environment_realization_contract::EnvironmentImageBuildError;
use awaken_session_contract::{
    ManagedSessionRepository, McpAttachmentRealizer, PersistedSession, RunError,
    SandboxProvisioning, SessionEnvironmentBindingSink, SessionRuntime, SessionRuntimePlacement,
};

use super::*;

struct NoopRuntime;
struct SuccessfulRuntime;
struct DiscardProgress;

#[async_trait::async_trait]
impl awaken_agent_contract::stream::sink::Sink for DiscardProgress {
    async fn send(
        &self,
        _event: awaken_agent_contract::stream::event::Event,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingCleanupRuntime {
    fail_quiesce_once: AtomicBool,
    fail_before_effect_once: AtomicBool,
    fail_once: AtomicBool,
    intents: Mutex<Vec<awaken_session_contract::SessionTerminalCleanupIntent>>,
    effective_ids: Mutex<BTreeSet<String>>,
    block_quiesce: AtomicBool,
    quiesce_entered: tokio::sync::Notify,
    quiesce_release: tokio::sync::Notify,
    delegated_snapshot: Mutex<awaken_session_contract::DelegatedRunSnapshot>,
}

#[derive(Default)]
struct RecordingReferenceIndex {
    fail_replace: AtomicBool,
    records: Mutex<BTreeSet<awaken_resource_contract::ResourceReferenceRecord>>,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourceReferenceIndex for RecordingReferenceIndex {
    async fn add_reference(
        &self,
        record: awaken_resource_contract::ResourceReferenceRecord,
    ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
        Ok(self.records.lock().unwrap().insert(record))
    }

    async fn remove_reference(
        &self,
        record: &awaken_resource_contract::ResourceReferenceRecord,
    ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
        Ok(self.records.lock().unwrap().remove(record))
    }

    async fn replace_references(
        &self,
        kind: awaken_resource_contract::ResourceReferenceKind,
        reference_id: &str,
        records: Vec<awaken_resource_contract::ResourceReferenceRecord>,
    ) -> Result<(), awaken_resource_contract::ResourcePurgeError> {
        if self.fail_replace.load(Ordering::SeqCst) {
            return Err(awaken_resource_contract::ResourcePurgeError::Storage(
                "injected reference projection failure".into(),
            ));
        }
        let mut current = self.records.lock().unwrap();
        current.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        current.extend(records);
        Ok(())
    }

    async fn references(
        &self,
        target: &awaken_resource_contract::ResourceTarget,
    ) -> Result<
        Vec<awaken_resource_contract::ResourceReference>,
        awaken_resource_contract::ResourcePurgeError,
    > {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| &record.target == target)
            .map(|record| record.reference.clone())
            .collect())
    }

    async fn references_for_resource(
        &self,
        kind: awaken_resource_contract::ResourceKind,
        resource_id: &str,
    ) -> Result<
        Vec<awaken_resource_contract::ResourceReferenceRecord>,
        awaken_resource_contract::ResourcePurgeError,
    > {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}

struct UnusedFileCatalog;

#[async_trait::async_trait]
impl awaken_resource_contract::FileCatalog for UnusedFileCatalog {
    async fn create_file(
        &self,
        _record: awaken_resource_contract::FileRecord,
    ) -> Result<
        awaken_resource_contract::CreateFileRecordOutcome,
        awaken_resource_contract::FileCatalogError,
    > {
        unreachable!("Skill-only projection never creates Files")
    }

    async fn get_file(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _include_deleted: bool,
    ) -> Result<
        Option<awaken_resource_contract::FileRecord>,
        awaken_resource_contract::FileCatalogError,
    > {
        unreachable!("Skill-only projection never resolves Files")
    }

    async fn list_files(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<awaken_resource_contract::FileRecord>, awaken_resource_contract::FileCatalogError>
    {
        Ok(Vec::new())
    }

    async fn mark_file_deleted(
        &self,
        _workspace_id: &str,
        _file_id: &str,
    ) -> Result<
        Option<awaken_resource_contract::FileRecord>,
        awaken_resource_contract::FileCatalogError,
    > {
        Ok(None)
    }

    async fn active_size_bytes(
        &self,
        _workspace_id: &str,
    ) -> Result<u64, awaken_resource_contract::FileCatalogError> {
        Ok(0)
    }
}

#[async_trait::async_trait]
impl SessionRuntime for NoopRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeReport, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    fn model(&self) -> String {
        "unused".into()
    }
}

#[async_trait::async_trait]
impl SessionRuntime for SuccessfulRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Ok(awaken_session_contract::StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            false,
            false,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("message test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        unreachable!("message test never resumes")
    }

    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        unreachable!("message test never adds a system message")
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeReport, RunError> {
        unreachable!("message test never defines an outcome")
    }

    fn model(&self) -> String {
        "successful-test-model".into()
    }
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingCleanupRuntime {
    async fn quiesce_terminal_delegations(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, RunError> {
        if self.fail_quiesce_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected pre-quiescence crash window"));
        }
        if self.block_quiesce.load(Ordering::SeqCst) {
            self.quiesce_entered.notify_one();
            self.quiesce_release.notified().await;
        }
        Ok(self.delegated_snapshot.lock().unwrap().clone())
    }

    async fn execute_terminal_cleanup(
        &self,
        intent: awaken_session_contract::SessionTerminalCleanupIntent,
    ) -> Result<awaken_session_contract::SessionTerminalCleanupReceipt, RunError> {
        if self.fail_before_effect_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected pre-effect crash window"));
        }
        self.intents.lock().unwrap().push(intent.clone());
        self.effective_ids
            .lock()
            .unwrap()
            .insert(intent.effect_id.clone());
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected cleanup crash window"));
        }
        Ok(awaken_session_contract::SessionTerminalCleanupReceipt::new(
            &intent,
            Vec::new(),
            true,
            true,
            true,
        ))
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeReport, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    fn model(&self) -> String {
        "unused".into()
    }
}

struct FaultingSessionRepository {
    inner: Arc<dyn ManagedSessionRepository>,
    fail_operation_once: Mutex<Option<String>>,
}

impl FaultingSessionRepository {
    fn new(inner: Arc<dyn ManagedSessionRepository>) -> Self {
        Self {
            inner,
            fail_operation_once: Mutex::new(None),
        }
    }

    fn fail_once(&self, operation: &str) {
        *self.fail_operation_once.lock().unwrap() = Some(operation.to_string());
    }
}

#[async_trait::async_trait]
impl ManagedSessionRepository for FaultingSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: awaken_session_contract::IdempotencyRecord,
        lifecycle_facts: Vec<awaken_session_contract::ManagedLifecycleFact>,
    ) -> Result<
        awaken_session_contract::SessionRevision,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner
            .create(owner_scope, session, idempotency, lifecycle_facts)
            .await
    }

    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: awaken_session_contract::SessionMutation,
    ) -> Result<
        awaken_session_contract::SessionMutationResult,
        awaken_session_contract::SessionRepositoryError,
    > {
        let should_fail = self
            .fail_operation_once
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|operation| mutation.idempotency.key.contains(operation));
        if should_fail {
            self.fail_operation_once.lock().unwrap().take();
            return Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
                    "injected root-CAS outage".into(),
                ),
            );
        }
        self.inner.commit_mutation(owner_scope, mutation).await
    }

    async fn append_lifecycle(
        &self,
        fact: awaken_session_contract::ManagedLifecycleFact,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.inner.append_lifecycle(fact).await
    }

    async fn pending_lifecycle(
        &self,
    ) -> Result<
        Vec<awaken_session_contract::ManagedLifecycleFact>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.pending_lifecycle().await
    }

    async fn complete_lifecycle(
        &self,
        fact_id: &str,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.inner.complete_lifecycle(fact_id).await
    }

    async fn get(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, awaken_session_contract::SessionRepositoryError> {
        self.inner.get(session_id).await
    }

    async fn reconcilable_sessions(
        &self,
    ) -> Result<
        awaken_session_contract::SessionRecoveryScan,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.reconcilable_sessions().await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionIdempotencyReceipt>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn owner(
        &self,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::SessionRepositoryError> {
        self.inner.owner(session_id).await
    }
}

struct NoopMcpRealizer;

#[async_trait::async_trait]
impl McpAttachmentRealizer for NoopMcpRealizer {}

#[derive(Default)]
struct RecordingEnvironmentSource {
    dispatched: Mutex<BTreeSet<String>>,
    awakened: Mutex<BTreeSet<String>>,
    retired: Mutex<BTreeSet<String>>,
    failures: Mutex<BTreeSet<String>>,
}

impl RecordingEnvironmentSource {
    fn fail_for(&self, session_id: &str) {
        self.failures.lock().unwrap().insert(session_id.into());
    }

    fn recover_for(&self, session_id: &str) {
        self.failures.lock().unwrap().remove(session_id);
    }
}

#[async_trait::async_trait]
impl SessionEnvironmentSource for RecordingEnvironmentSource {
    async fn get(
        &self,
        _environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        unreachable!("work dispatch does not reopen the Environment catalog")
    }

    async fn resolve_current_for_session(
        &self,
        _environment_id: &str,
        _runtime: Option<&str>,
        _mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        unreachable!("work dispatch consumes the frozen Session baseline")
    }

    async fn resolve_exact_for_session(
        &self,
        _environment_id: &str,
        _revision: u64,
        _runtime: Option<&str>,
        _mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        unreachable!("work dispatch consumes the frozen Session baseline")
    }

    async fn enqueue_session_work(
        &self,
        _environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
        if self.failures.lock().unwrap().contains(session_id) {
            return Err(
                awaken_session_contract::work_queue::WorkQueueError::Storage(format!(
                    "injected failure for {session_id}"
                )),
            );
        }
        self.dispatched
            .lock()
            .unwrap()
            .insert(session_id.to_string());
        Ok(format!("work:{session_id}"))
    }

    async fn wake_session_work(
        &self,
        _environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
        self.awakened.lock().unwrap().insert(session_id.to_string());
        Ok(format!("work:{session_id}"))
    }

    async fn retire_session_work(
        &self,
        _environment_id: &str,
        session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::work_queue::WorkItem>,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        self.retired.lock().unwrap().insert(session_id.to_string());
        Ok(None)
    }

    async fn acquire_session_work(
        &self,
        _environment_id: &str,
        _session_id: &str,
        _worker_owner: &str,
        _now_ms: u64,
    ) -> Result<
        Option<awaken_session_contract::work_queue::SessionWorkLease>,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        Ok(None)
    }
}

fn persisted(id: &str, self_hosted: bool, status: &str) -> PersistedSession {
    let environment = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "env-worker".into(),
        revision: awaken_environment_contract::EnvironmentRevision(7),
        self_hosted,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-7".into()),
        sandbox: serde_json::json!({}),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    PersistedSession {
        session_id: id.into(),
        revision: Default::default(),
        baseline: awaken_session_contract::SessionBaselineState::Frozen(
            awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
                    environment,
                    runtime_placement: if self_hosted {
                        SessionRuntimePlacement::Worker
                    } else {
                        SessionRuntimePlacement::Local
                    },
                    mcp_authoring: Default::default(),
                    agent_id: "agent".into(),
                    model: "model".into(),
                    runtime: None,
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                },
            ),
        ),
        title: None,
        metadata: Default::default(),
        tools: Default::default(),
        activity_epoch: 0,
        running_interval: None,
        runtime_active_millis: 0,
        budget: Default::default(),
        environment: Default::default(),
        mcp: Default::default(),
        resources: Default::default(),
        realization: None,
        realization_progress: Default::default(),
        execution: status.parse().expect("fixture execution state"),
        disposition: Default::default(),
        terminal_cleanup: Default::default(),
    }
}

async fn create(repo: &dyn ManagedSessionRepository, session: PersistedSession) {
    let id = session.session_id.clone();
    repo.create(
        "workspace",
        session,
        awaken_session_contract::IdempotencyRecord {
            key: format!("create:{id}"),
            payload_hash: format!("payload:{id}"),
        },
        Vec::new(),
    )
    .await
    .expect("persist fixture");
}

fn application(
    repo: Arc<dyn ManagedSessionRepository>,
    environments: Arc<dyn SessionEnvironmentSource>,
) -> SessionApplication {
    application_with_configuration(
        repo,
        environments,
        SessionApplicationConfiguration::default(),
    )
}

fn application_with_runtime(
    runtime: Arc<dyn SessionRuntime>,
    repo: Arc<dyn ManagedSessionRepository>,
    environments: Arc<dyn SessionEnvironmentSource>,
) -> SessionApplication {
    SessionApplication::new(runtime, Arc::new(NoopMcpRealizer), repo, environments)
}

fn application_with_configuration(
    repo: Arc<dyn ManagedSessionRepository>,
    environments: Arc<dyn SessionEnvironmentSource>,
    configuration: SessionApplicationConfiguration,
) -> SessionApplication {
    SessionApplication::new_with_configuration(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
        repo,
        environments,
        configuration,
    )
}

fn skill_resources(id: &str) -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources {
        inputs: Vec::new(),
        skills: Some(vec![awaken_session_contract::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: id.into(),
            version: 1,
            bundle_sha256: format!("sha-{id}"),
        }]),
    }
}

fn file_resources(id: &str) -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources {
        inputs: vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("input"),
            source: awaken_session_contract::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from(id),
            },
            mount_path: "/workspace/input".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        skills: Some(Vec::new()),
    }
}

/// Local-restart ownership FMECA and cause/effect decision table. C1 the
/// configured logical owner is unchanged or different; C2 the Runtime
/// incarnation is the original process or a replacement. Effects are E1 an
/// exact replay, E2 immediate epoch-fenced takeover, E3 stale-owner
/// rejection, or E4 claim-authorized topology reassignment. C3 renewal and
/// reassignment are mutually exclusive so one command cannot obscure its
/// authority class.
///
/// | Rule | Logical owner | Incarnation | Reassign | Renew | Effect |
/// |---|---|---|---|---|---|
/// | L1 | same | same | false | false | E1 replay |
/// | L2 | same | replacement | false | false | E2 epoch advances immediately |
/// | L3 | different | replacement | false | false | E3 stale ownership |
/// | L4 | different | replacement | true | false | E4 epoch advances immediately |
/// | L5 | any | any | true | true | invalid, no mutation |
mod authority;
mod continuation;
mod creation;
mod realization;
mod run_admission;
