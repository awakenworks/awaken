//! Host tests, split by capability topic. Shared fixtures live here; the
//! crate-wide ones live in [`fixtures`].
use super::*;
use crate::session_environment::SandboxEnvironmentAccess;
use awaken_time::system_clock;

#[tokio::test]
async fn native_free_input_reuses_session_continuation_and_committed_reply_receipt() {
    // Causes: C1 exact primary Awaiting/manual-pause ticket; C2 current version;
    // C3 fresh/exact/changed operation; C4 a permission ticket instead of input;
    // C5 ticket was consumed with ResumeApplied. Effects: E1 one canonical Inbox
    // input and activity rotation; E2 exact retries are inert before/after C5;
    // E3 changed text, stale version or wrong kind produces zero new input.
    // R1=C1+C2+fresh=>E1; R2=exact=>E2; R3=changed|!C2|C4=>E3;
    // R4=C5+exact=>E2. Real SQLite commit/dispatch owners are used, no receipt map.
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PauseReason};
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_dispatch::{DispatchOutcome, Outbox as _};
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
    let dispatch = Arc::new(
        awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
            .await
            .unwrap(),
    );
    let (host, _managed) = dispatch_test_host(
        (crate::HostDependencies::ephemeral(Arc::new(MemoryHostModel), "stub")
            .await
            .with_ephemeral_dispatch(dispatch.clone()))
        .build()
        .expect("validated Host dependencies"),
    );
    let thread = ThreadId("native-input-session".into());
    let run = RunId("native-input-run".into());
    let ctx = host.ctx_for(&thread.0, None).await.unwrap();
    let commit = host.commit_for_read(&thread.0).await.unwrap();
    commit
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::running(run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let ticket = ResumeTicket::new(
        "native-input-correlation",
        run.clone(),
        thread.clone(),
        "snapshot",
        "catalog",
        AwaitTarget::Pause(PauseReason::Manual),
    );
    commit
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();
    let version = commit
        .recovery_snapshot(&thread, &run)
        .await
        .unwrap()
        .thread_version;
    dispatch
        .enqueue(
            RunDispatch::new(RunActivation::new(
                run.clone(),
                thread.clone(),
                ctx.config.clone(),
                Vec::new(),
            ))
            .for_session(thread.clone())
            .with_session_activity_epoch(10),
        )
        .await
        .unwrap();
    let claimed = dispatch
        .claim_run(
            &run,
            "native-input-owner",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    dispatch
        .settle(&run, claimed.lease.epoch(), DispatchOutcome::Awaiting, &[])
        .await
        .unwrap();
    let command = awaken_session_execution_contract::SessionThreadToolReplyCommand {
        session_id: SessionId::from(&thread.0),
        tool_request_event_id: Some("native-response-1".into()),
        expected_thread_version: Some(version),
        target: awaken_session_execution_contract::SessionThreadTarget::Primary,
        expected_run_id: run.clone(),
        expected_correlation_id: ticket.correlation_id.clone(),
        tool_use_id: String::new(),
        reply: awaken_session_execution_contract::SessionThreadToolReply::Input("answer".into()),
        accompanying_system: None,
    };
    let mut stale = command.clone();
    stale.expected_thread_version = Some(version + 1);
    assert!(
        host.session_thread_tool_reply_fence(&stale).await.is_err(),
        "R3 stale version"
    );
    let mut wrong_kind = command.clone();
    wrong_kind.reply = awaken_session_execution_contract::SessionThreadToolReply::Confirm(
        PermissionDecision::Allow { note: None },
    );
    assert!(
        host.session_thread_tool_reply_fence(&wrong_kind)
            .await
            .is_err(),
        "R3 manual pause is not permission"
    );
    let fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .unwrap();
    let delivery = awaken_session_execution_contract::SessionThreadToolReplyDelivery {
        command: command.clone(),
        fence: fence.clone(),
        session_activity_epoch: Some(11),
    };
    host.reply_session_thread_tool(delivery.clone())
        .await
        .unwrap();
    host.reply_session_thread_tool(delivery).await.unwrap();
    let pending = dispatch.list(&thread).await.unwrap();
    assert_eq!(pending.len(), 1, "R1/R2");
    assert_eq!(
        pending[0].input.result,
        ResumeResult::Input("answer".into())
    );
    assert_eq!(pending[0].input.message_id, command.delivery_operation_id());
    let resumed = dispatch
        .claim_run(
            &run,
            "native-input-owner",
            DEFAULT_LEASE_MS,
            &Default::default(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resumed.request.session_activity_epoch,
        Some(11),
        "R1 atomic rotation"
    );
    commit
        .commit(ThreadCommit::assemble(
            thread.clone(),
            RunDisposition::ended(run.clone(), EndCause::NaturalEnd),
            true,
            Vec::new(),
            Vec::new(),
            vec![
                awaken_agent_contract::audit::run_event::RunEvent::ResumeApplied {
                    correlation_id: ticket.correlation_id.clone(),
                    operation_id: command.delivery_operation_id(),
                }
                .into(),
            ],
        ))
        .await
        .unwrap();
    let replay = host
        .session_thread_tool_reply_fence(&command)
        .await
        .unwrap();
    assert!(replay.already_applied, "R4");
    host.reply_session_thread_tool(
        awaken_session_execution_contract::SessionThreadToolReplyDelivery {
            command: command.clone(),
            fence: replay,
            session_activity_epoch: None,
        },
    )
    .await
    .unwrap();
    let mut changed = command;
    changed.reply =
        awaken_session_execution_contract::SessionThreadToolReply::Input("changed answer".into());
    assert!(
        host.session_thread_tool_reply_fence(&changed)
            .await
            .is_err(),
        "R3 changed payload cannot reuse the consumed correlation"
    );
    assert_eq!(dispatch.relay().await.unwrap(), 0, "R4 no second delivery");
    assert_eq!(
        dispatch.list(&thread).await.unwrap().len(),
        1,
        "R4 receipt leaves Inbox unchanged"
    );
}
use awaken_dispatch_contract::ControlDispatchQueue;
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_session_execution_contract::{EnvironmentRuntime, TerminalCleanupRuntime};
use awaken_session_types::SessionId;
mod claimed_dispatch;
mod cold_terminal_cleanup;
mod compaction_memory;
mod context_construction;
mod coordinated_thread_close;
mod environment_binding;
mod environment_capacity;
mod environment_projection;
pub(crate) mod fixtures;
mod managed_interrupt;
mod mcp_credentials;
mod mcp_generation;
mod mcp_relay_lease;
mod outcome_query;
mod pinned_skill_read;
mod projection_convergence;
mod realization_renewal;
mod repository_credentials;
mod resource_activation;
mod resource_manifest;
mod resource_staging_lease;
mod resume_and_replies;
use fixtures::*;
mod run_boundary;
mod session_projection;
mod session_realizations;
mod skill_delivery;
mod terminal_disposal;
mod terminal_memory;
mod worker_projection;
use crate::config::block_text;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
use awaken_session_execution_contract::SessionRuntime;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

fn test_model_binding() -> awaken_runtime_contract::resolved::ModelBinding {
    awaken_runtime_contract::resolved::ModelBinding::new("test", "model", "native")
}

/// Approval-state tests name their precondition explicitly. Managed Agent
/// members default to always-allow; this exact test override asks only for
/// `write` while leaving unrelated follow-up effects unchanged.
fn write_confirmation_gate() -> Arc<awaken_runtime::PermissionGate> {
    let toolsets = [crate::config::test_agent_toolset_permission(
        "write",
        awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
    )];
    let policy = crate::config::effective_policy_with_toolsets(
        None,
        &[],
        &toolsets,
        crate::config::platform_auto_evaluator(),
    );
    Arc::new(awaken_runtime::PermissionGate::new(Arc::new(policy)))
}

/// Exercise Host behavior whose stated precondition is an already-admitted
/// Session. The fixture reuses the sole delivery implementation as an ordinary
/// thread extension; it deliberately owns no reservation, activity, or Worker
/// lifecycle. Those effects belong to SessionApplication tests and full E2Es.
/// Causes: C1 prepared frozen inputs and C2 one post-admission input batch.
/// Effects: E1 execute against C1; E2 commit/project one Step. Decision rule
/// T1=C1+C2=>E1+E2. The production Host-command fence remains active elsewhere.
async fn run_prepared_session_messages(
    managed: &crate::ManagedHost,
    agent: &str,
    thread: &str,
    messages: Vec<Message>,
) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError> {
    managed.validate_thread_resource_bindings(thread).await?;
    let result = managed
        .host
        .run_thread_extension_after_admission(Some(agent), thread, messages)
        .await;
    crate::step_projection::finish_managed_step(result)
}

async fn run_prepared_session(
    managed: &crate::ManagedHost,
    agent: &str,
    thread: &str,
    content: Vec<ContentBlock>,
) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError> {
    run_prepared_session_messages(managed, agent, thread, vec![crate::user_message(content)]).await
}

fn native_credential_profile() -> awaken_credential_contract::CredentialRealizationProfile {
    awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
}

fn managed_test_container_capabilities() -> awaken_provisioning_contract::SandboxCapabilities {
    awaken_provisioning_contract::SandboxCapabilities {
        process_tree_supervision: false,
        isolation: awaken_provisioning_contract::IsolationClass::Container,
        tool_transparent: true,
        path_fidelity: true,
        enforced_readonly: true,
        network_isolation: true,
        enforced_network_allowlist: true,
        secret_egress_substitution: true,
        resource_limits: true,
        custom_rootfs: true,
        package_provisioning: false,
        control_services: Default::default(),
    }
}

fn session_environment(
    network: awaken_session_configuration_contract::SessionNetworkPolicy,
    sandbox: serde_json::Value,
) -> awaken_session_configuration_contract::EnvironmentSnapshot {
    let sandbox: awaken_provisioning_contract::SandboxOverride =
        serde_json::from_value(sandbox).expect("valid SandboxOverride test fixture");
    let config_fingerprint = awaken_session_configuration_contract::EnvironmentFingerprint(
        awaken_agent_contract::stable_fingerprint(&(network.clone(), sandbox.clone())),
    );
    awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "test-environment".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint,
        sandbox,
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network,
        credential_realization: native_credential_profile(),
    }
}

fn on_tool_use_environment() -> awaken_session_configuration_contract::EnvironmentSnapshot {
    let mut environment = session_environment(
        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        serde_json::json!({}),
    );
    environment.sandbox_provisioning =
        awaken_session_configuration_contract::SandboxProvisioning::OnToolUse;
    environment
}

fn resource_transition(
    workspace: &str,
    previous_revision: u64,
    previous: awaken_session_resource_contract::ResolvedSessionResources,
    desired_revision: u64,
    desired: awaken_session_resource_contract::ResolvedSessionResources,
) -> awaken_session_resource_contract::SessionResourceTransition {
    awaken_session_resource_contract::SessionResourceTransition::new(
        awaken_session_resource_contract::SessionResourceManifest::at_revision(
            workspace,
            previous_revision,
            previous,
        ),
        awaken_session_resource_contract::SessionResourceManifest::at_revision(
            workspace,
            desired_revision,
            desired,
        ),
    )
    .expect("test Resource transition belongs to one Workspace")
}

/// One test fixture for the production SkillVersion authority. Callers vary
/// only identity/body/supporting files, so Managed tests cannot accidentally
/// reintroduce host-static SkillSpec setup as a parallel source.
pub(crate) fn frozen_skill_version(
    id: &str,
    name: &str,
    description: &str,
    body: &str,
    supporting_files: &[(&str, &str)],
) -> awaken_skill_store::SkillVersion {
    let mut files = vec![awaken_skill_store::SkillBundleFile {
        path: "SKILL.md".into(),
        content: format!("---\nname: {id}\ndescription: {description}\n---\n{body}").into_bytes(),
        executable: false,
    }];
    files.extend(supporting_files.iter().map(|(path, content)| {
        awaken_skill_store::SkillBundleFile {
            path: (*path).into(),
            content: content.as_bytes().to_vec(),
            executable: false,
        }
    }));
    awaken_skill_store::SkillVersion {
        id: format!("skver-{id}-1").into(),
        skill_id: id.into(),
        version: 1,
        name: name.into(),
        description: description.into(),
        directory: format!("/skills/{id}"),
        bundle_sha256: awaken_skill_store::bundle_sha256(&files),
        files,
        created_unix_nanos: 0,
    }
}

/// One Managed Skill test setup through the Resource pin and immutable version
/// owner. A test may omit the bytes to exercise the same fail-closed path; it
/// never authors a process-local slot as a second selection source.
async fn install_pinned_managed_skill_session(
    host: &Arc<SharedHost>,
    managed: &crate::ManagedHost,
    thread: &str,
    agent: &str,
    environment: awaken_session_configuration_contract::EnvironmentSnapshot,
    version: &awaken_skill_store::SkillVersion,
    seed_version: bool,
) -> Result<(), awaken_session_types::RunError> {
    install_pinned_managed_skill_session_with_tools(
        host,
        managed,
        thread,
        agent,
        environment,
        version,
        seed_version,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn install_pinned_managed_skill_session_with_tools(
    host: &Arc<SharedHost>,
    managed: &crate::ManagedHost,
    thread: &str,
    agent: &str,
    environment: awaken_session_configuration_contract::EnvironmentSnapshot,
    version: &awaken_skill_store::SkillVersion,
    seed_version: bool,
    tools: Option<awaken_session_configuration_contract::SessionToolConfiguration>,
) -> Result<(), awaken_session_types::RunError> {
    let workspace = host.local_workspace().to_string();
    if seed_version {
        host.tools
            .skills
            .create(
                awaken_skill_store::SkillDefinition {
                    id: version.skill_id.clone(),
                    workspace_id: workspace.clone(),
                    display_title: None,
                    latest_version: version.version,
                    last_version: version.version,
                    timestamps: Default::default(),
                },
                version.clone(),
            )
            .await
            .expect("test SkillStore")
            .expect("create immutable selected Skill version");
    }
    let resources = pinned_skill_resources(version);
    let mut init = bare_session(agent, &workspace);
    init.environment = environment;
    init.resources = resources.clone();
    init.tools = tools;
    install_complete_test_session_for_realization(managed, thread, init).await?;
    managed
        .apply_session_inputs(
            thread,
            &resource_transition(&workspace, 0, Default::default(), 0, resources),
        )
        .await
}

fn pinned_skill_resources(
    version: &awaken_skill_store::SkillVersion,
) -> awaken_session_resource_contract::ResolvedSessionResources {
    awaken_session_resource_contract::ResolvedSessionResources::default()
        .with_skills(vec![
            awaken_session_resource_contract::ResolvedSkillBinding {
                kind: awaken_agent_contract::AgentSkillKind::Custom,
                skill_id: version.skill_id.to_string(),
                version: version.version,
                bundle_sha256: version.bundle_sha256.clone(),
            },
        ])
        .expect("exact Skill Resource binding")
}

/// Test-only proof that a Managed Session was composed with its required
/// SessionApplication port. Individual coordination adapters have their own
/// behavioral fakes; this one admits the ordinary unlimited model-request path
/// and rejects every coordination command, so an environment test cannot
/// accidentally become a second coordination implementation.
struct RejectingSessionAgentCoordination;

fn reject_environment_test_coordination<T>() -> Result<T, awaken_session_types::RunError> {
    Err(awaken_session_types::RunError::internal(
        "environment test must not invoke Session coordination",
    ))
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionAgentCoordination
    for RejectingSessionAgentCoordination
{
    async fn admit_session_run_activity(
        &self,
        _session_id: &SessionId,
        _agent_id: &str,
        _run_id: &awaken_agent_contract::agent::run::Id,
        _mode: awaken_session_execution_contract::SessionRunActivityAdmissionMode,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivityAdmission,
        awaken_session_types::RunError,
    > {
        reject_environment_test_coordination()
    }

    async fn admit_session_model_request(
        &self,
        _session_id: &SessionId,
        _thread_id: &awaken_agent_contract::agent::thread::Id,
        _run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, awaken_session_types::RunError> {
        Ok(true)
    }

    async fn list_session_agents(
        &self,
        _session_id: &SessionId,
    ) -> Result<
        Vec<awaken_session_execution_contract::SessionAgentRosterEntry>,
        awaken_session_types::RunError,
    > {
        reject_environment_test_coordination()
    }

    async fn send_session_agent_message(
        &self,
        _command: awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_types::RunError,
    > {
        reject_environment_test_coordination()
    }

    async fn settle_session_agent_boundary(
        &self,
        _command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        reject_environment_test_coordination()
    }

    async fn finalize_session_agent_boundary(
        &self,
        _command: awaken_session_execution_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        reject_environment_test_coordination()
    }

    async fn interrupt_session_thread(
        &self,
        _session_id: &SessionId,
        _child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_types::RunError> {
        reject_environment_test_coordination()
    }

    async fn reply_session_thread_tool(
        &self,
        _command: awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), awaken_session_types::RunError> {
        reject_environment_test_coordination()
    }
}

/// Install the required Session application edge for tests whose subject is
/// unrelated to coordination or budget admission. A process-static strong
/// owner keeps the production-shaped weak edge alive without adding a second
/// behavioral fake to each fixture.
fn install_test_session_application(host: &Arc<SharedHost>) {
    static APPLICATION: std::sync::OnceLock<
        Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    > = std::sync::OnceLock::new();
    let application = APPLICATION.get_or_init(|| Arc::new(RejectingSessionAgentCoordination));
    host.session_application_bindings()
        .bind_agent_coordination(application.clone())
        .expect("install one test Session application authority");
}

/// Install the existing recording coordination authority for tests that cross
/// a real Session activity settlement boundary. Environment-only fixtures use
/// the stricter rejecting authority above so an accidental coordination effect
/// still fails closed.
fn install_recording_session_application(host: &Arc<SharedHost>) {
    static APPLICATION: std::sync::OnceLock<
        Arc<dyn awaken_session_execution_contract::SessionAgentCoordination>,
    > = std::sync::OnceLock::new();
    let application = APPLICATION.get_or_init(|| {
        Arc::new(crate::coordination::RecordingSessionAgentCoordination::default())
    });
    host.session_application_bindings()
        .bind_agent_coordination(application.clone())
        .expect("install recording Session application authority");
}

async fn resource_registry() -> Arc<awaken_resource_application::RegistryApplication> {
    let storage = awaken_resource_store::SqliteResourceStore::in_memory()
        .await
        .expect("open ephemeral Resource Registry");
    let clock = system_clock();
    awaken_resource_application::RegistryApplication::new(Arc::new(storage), clock).into()
}

#[derive(Default)]
pub(super) struct TestResourceLifecycle {
    intents: Mutex<BTreeMap<String, awaken_resource_contract::ResourcePurgeIntent>>,
    references: Mutex<BTreeSet<awaken_resource_contract::ResourceReferenceRecord>>,
    fences: Mutex<BTreeMap<(awaken_resource_contract::ResourceKind, String), String>>,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourcePurgeRepository for TestResourceLifecycle {
    async fn put(
        &self,
        intent: awaken_resource_contract::ResourcePurgeIntent,
    ) -> Result<
        awaken_resource_contract::PutResourcePurgeOutcome,
        awaken_resource_contract::ResourcePurgeError,
    > {
        intent.validate()?;
        let mut intents = self.intents.lock().unwrap();
        if let Some(existing) = intents.get(&intent.intent_id) {
            return if existing.same_request(&intent) {
                Ok(awaken_resource_contract::PutResourcePurgeOutcome::Existing)
            } else {
                Err(
                    awaken_resource_contract::ResourcePurgeError::IdempotencyConflict(
                        intent.idempotency_key,
                    ),
                )
            };
        }
        intents.insert(intent.intent_id.clone(), intent);
        Ok(awaken_resource_contract::PutResourcePurgeOutcome::Inserted)
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<
        Option<awaken_resource_contract::ResourcePurgeIntent>,
        awaken_resource_contract::ResourcePurgeError,
    > {
        Ok(self.intents.lock().unwrap().get(intent_id).cloned())
    }

    async fn recoverable(
        &self,
        _now_unix_ms: u64,
        _limit: usize,
    ) -> Result<
        Vec<awaken_resource_contract::ResourcePurgeIntent>,
        awaken_resource_contract::ResourcePurgeError,
    > {
        Ok(Vec::new())
    }

    async fn save(
        &self,
        _expected_revision: u64,
        intent: awaken_resource_contract::ResourcePurgeIntent,
    ) -> Result<(), awaken_resource_contract::ResourcePurgeError> {
        self.intents
            .lock()
            .unwrap()
            .insert(intent.intent_id.clone(), intent);
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourceReferenceIndex for TestResourceLifecycle {
    async fn add_reference(
        &self,
        record: awaken_resource_contract::ResourceReferenceRecord,
    ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
        Ok(self.references.lock().unwrap().insert(record))
    }

    async fn remove_reference(
        &self,
        record: &awaken_resource_contract::ResourceReferenceRecord,
    ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
        Ok(self.references.lock().unwrap().remove(record))
    }

    async fn replace_references(
        &self,
        kind: awaken_resource_contract::ResourceReferenceKind,
        reference_id: &str,
        records: Vec<awaken_resource_contract::ResourceReferenceRecord>,
    ) -> Result<(), awaken_resource_contract::ResourcePurgeError> {
        let mut references = self.references.lock().unwrap();
        references.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        references.extend(records);
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
            .references
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
            .references
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourceReclamationFence for TestResourceLifecycle {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &awaken_resource_contract::ResourceTarget,
    ) -> Result<
        awaken_resource_contract::AcquireResourceReclamationOutcome,
        awaken_resource_contract::ResourcePurgeError,
    > {
        let key = (target.kind, target.resource_id.clone());
        let mut fences = self.fences.lock().unwrap();
        if let Some(owner) = fences.get(&key) {
            return Ok(if owner == intent_id {
                awaken_resource_contract::AcquireResourceReclamationOutcome::AlreadyOwned
            } else {
                awaken_resource_contract::AcquireResourceReclamationOutcome::Contended
            });
        }
        fences.insert(key, intent_id.into());
        Ok(awaken_resource_contract::AcquireResourceReclamationOutcome::Acquired)
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &awaken_resource_contract::ResourceTarget,
    ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
        let key = (target.kind, target.resource_id.clone());
        let mut fences = self.fences.lock().unwrap();
        if fences.get(&key).is_some_and(|owner| owner == intent_id) {
            fences.remove(&key);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub(super) fn test_resource_reclamation()
-> Arc<dyn awaken_resource_contract::ResourceReclamationRepository> {
    Arc::new(TestResourceLifecycle::default())
}

/// Authoring shorthand used only by tests. Production crosses the runtime port
/// exclusively as `ResolvedSessionResources`.
#[derive(Clone)]
struct TestInput {
    kind: String,
    id: String,
    mount_path: String,
    access: awaken_resource_contract::ResourceAccess,
    instructions: Option<String>,
    initial_branch: Option<String>,
    initial_commit: Option<String>,
}

use awaken_resource_contract::ResourceAccess;

pub(crate) fn bind_test_memory(host: &SharedHost, thread: &str, store_id: &str, writable: bool) {
    let config = awaken_resource_contract::MemoryStoreConfigVersion {
        memory_store_id: store_id.to_string().into(),
        version: awaken_resource_contract::ConfigVersion::INITIAL,
        retention_policy: Default::default(),
    };
    let handle = host.platform_memory_handle(store_id.to_string(), writable);
    host.register_thread_memory(
        thread,
        Some(Arc::new(crate::memory_binding::MemoryBinding::new(
            thread,
            "default",
            handle,
            Some(test_resource_validator()),
            &config,
            writable,
        ))),
    );
}

fn managed_with_resource_source(host: Arc<SharedHost>) -> crate::ManagedHost {
    managed_with_resource_source_using(host, install_test_session_application)
}

fn managed_with_resource_source_using(
    host: Arc<SharedHost>,
    install_coordination: fn(&Arc<SharedHost>),
) -> crate::ManagedHost {
    install_coordination(&host);
    crate::ManagedHost::new(host)
}

fn http_basic_material(username: &str, password: &str) -> awaken_agent_contract::RedactedString {
    let material = awaken_credential_vault::StructuredCredentialMaterial {
        type_id: awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE.into(),
        fields: std::collections::BTreeMap::from([
            (
                "username".into(),
                awaken_agent_contract::RedactedString::new(username),
            ),
            (
                "password".into(),
                awaken_agent_contract::RedactedString::new(password),
            ),
        ]),
    };
    awaken_credential_vault::encode_structured_material(material)
        .expect("encode HTTP Basic test material")
}

fn effective_resources(
    resources: Vec<TestInput>,
) -> awaken_session_resource_contract::ResolvedSessionResources {
    use awaken_resource_contract::{
        BindingId, FileId, MemoryStoreId, RepositoryId, ResourceAccess,
    };
    use awaken_session_resource_contract::{ResolvedInput, ResolvedInputSource};

    awaken_session_resource_contract::ResolvedSessionResources::try_new(
        resources
            .into_iter()
            .enumerate()
            .map(|(index, resource)| {
                let source = match resource.kind.as_str() {
                    "file" => ResolvedInputSource::File {
                        file_id: FileId::from(resource.id.clone()),
                    },
                    "memory_store" => ResolvedInputSource::MemoryStore {
                        memory_store_id: MemoryStoreId::from(resource.id.clone()),
                        config: awaken_resource_contract::MemoryStoreConfigVersion {
                            memory_store_id: resource.id.clone().into(),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            retention_policy: Default::default(),
                        },
                    },
                    "github_repository" => ResolvedInputSource::Repository {
                        repository_id: RepositoryId::new(format!("test-repo-{index}")),
                        config: awaken_resource_contract::RepositoryConfigVersion {
                            repository_id: format!("test-repo-{index}").into(),
                            version: awaken_resource_contract::ConfigVersion::INITIAL,
                            remote_url: resource.id,
                            credential_binding: None,
                            initial_branch: resource.initial_branch,
                            initial_commit: resource.initial_commit,
                            clone_policy: Default::default(),
                        },
                        credential: None,
                    },
                    kind => panic!("unsupported test input kind {kind}"),
                };
                ResolvedInput {
                    created_at_unix_ms: None,
                    updated_at_unix_ms: None,
                    binding_id: BindingId::new(format!("test-input-{index}")),
                    source,
                    mount_path: resource.mount_path,
                    access: match (resource.kind.as_str(), resource.access) {
                        ("file", _) | (_, ResourceAccess::ReadOnly) => ResourceAccess::ReadOnly,
                        (_, ResourceAccess::ReadWrite) => ResourceAccess::ReadWrite,
                    },
                    instructions: resource.instructions,
                }
            })
            .collect(),
        Vec::new(),
    )
    .unwrap()
}

fn effective_repository(
    id: &str,
    url: &str,
    mount_path: &str,
    credential_binding: Option<String>,
) -> awaken_session_resource_contract::ResolvedSessionResources {
    let holder = awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
        .resource_holder;
    let credential = credential_binding.as_ref().map(|binding| {
        Box::new(
            awaken_session_resource_contract::ResolvedRepositoryCredential {
                access: awaken_runtime_contract::CredentialAccess::new(
                    awaken_runtime_contract::CredentialRef {
                        id: binding.clone(),
                        revision: 1,
                    },
                    awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                    awaken_session_resource_contract::repository_transport_credential_usage(),
                    awaken_runtime_contract::CredentialExecutionPolicy::exact(
                        holder.clone(),
                        awaken_runtime_contract::ModelExposurePolicy::Forbidden,
                    ),
                )
                .with_target(
                    awaken_session_resource_contract::repository_transport_credential_target(url)
                        .expect("HTTPS Repository target"),
                ),
                selected_plaintext_holder: holder,
            },
        )
    });
    awaken_session_resource_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_resource_contract::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: awaken_resource_contract::BindingId::from("test-repository"),
            source: awaken_session_resource_contract::ResolvedInputSource::Repository {
                repository_id: awaken_resource_contract::RepositoryId::from(id),
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: id.into(),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    remote_url: url.into(),
                    credential_binding,
                    initial_branch: None,
                    initial_commit: None,
                    clone_policy: Default::default(),
                },
                credential,
            },
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        }],
        Vec::new(),
    )
    .unwrap()
}

struct FixedRepositoryTransport(awaken_resource_contract::RepositoryTransport);

#[async_trait::async_trait]
impl crate::RepositoryBindingVerifier<crate::ResourceOperationFence> for FixedRepositoryTransport {
    async fn verify(
        &self,
        _workspace_id: &str,
        _repository_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _authority: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        Ok(self.0.clone())
    }
}

struct SequencedRepositoryTransport(AtomicUsize);

#[async_trait::async_trait]
impl crate::RepositoryBindingVerifier<crate::ResourceOperationFence>
    for SequencedRepositoryTransport
{
    async fn verify(
        &self,
        _workspace_id: &str,
        _repository_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _authority: Option<&crate::ResourceOperationFence>,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        let sequence = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(
            awaken_resource_contract::RepositoryTransport::GatewayMediated {
                remote_url: "https://gateway.internal/git/repo-platform".into(),
                capability: awaken_resource_contract::RepositoryGatewayCapability::new(format!(
                    "repository-capability-{sequence}"
                ))?,
                expires_at_unix_ms: None,
            },
        )
    }
}

fn memory_mount_store_id(mount: &awaken_provisioning_contract::MountRequirement) -> &str {
    let awaken_provisioning_contract::MountSource::MemoryStore { store_id, .. } = &mount.source
    else {
        panic!("expected a governed memory-store source")
    };
    store_id
}

fn test_memory_store_id() -> String {
    format!(
        "test-memory-store-{}",
        BASE_SEQ.fetch_add(1, Ordering::SeqCst)
    )
}

/// A model that blocks on its second inference (the first revision round) until
/// a gate is released, so a concurrent `interrupt` can land while the outcome
/// loop is mid-run. Its reply never contains the rubric, so the guard steers.
struct GatedModel {
    gate: Arc<tokio::sync::Notify>,
    reached: Arc<tokio::sync::Notify>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for GatedModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 2 {
            self.reached.notify_one();
            self.gate.notified().await;
        }
        Ok(ChatResponse {
            output: AssistantOutput::text(if call == 1 {
                r#"{"result":"needs_revision","explanation":"finish the deliverable"}"#
            } else {
                "a rough draft"
            }),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Install the one production-owned Dispatch composition required before a
/// direct or durable Run can apply its canonical empty Resource transition.
/// Interrupt fixtures retain the returned adapter so no test-only runtime or
/// partial Session projection competes with that composition.
pub(crate) fn install_test_dispatch_runtime(host: &Arc<SharedHost>) -> crate::ManagedHost {
    crate::ManagedHost::new(host.clone())
}

/// Wrap a fully configured ordinary Host and install the same canonical
/// Dispatch composition used by the application. Callers configure the Host
/// first, so this fixture cannot publish a partially configured adapter.
fn dispatch_test_host(host: Arc<SharedHost>) -> (Arc<SharedHost>, crate::ManagedHost) {
    let managed = install_test_dispatch_runtime(&host);
    (host, managed)
}

/// Install the complete projection corresponding to legacy `SessionInit` test
/// data. C1 empty/nonempty initial Resources and C2 absent/present model
/// coordinates produce E1 one baseline plus E2 the exact Empty->desired
/// transition through `SessionRuntime`; no caller reconstructs slot fields.
async fn install_complete_test_session(
    managed: &crate::ManagedHost,
    thread: &str,
    init: awaken_session_configuration_contract::SessionInit,
) -> Result<(), awaken_session_types::RunError> {
    let awaken_session_configuration_contract::SessionInit {
        workspace_id,
        agent_id,
        delegate_ids,
        tools,
        resource_revision,
        resources,
        model,
        runtime,
        environment,
    } = init;
    let model = model.unwrap_or_else(|| "stub".into());
    let runtime = runtime.unwrap_or_else(|| "default".into());
    let tools = tools.unwrap_or_else(|| {
        awaken_session_configuration_contract::SessionToolConfiguration::from_capabilities(
            &awaken_session_execution_contract::CapabilityRuntime::capabilities(managed),
        )
    });
    let publication = managed
        .host
        .agent
        .agent_publications
        .as_ref()
        .and_then(|source| {
            source.current(
                &workspace_id,
                &awaken_runtime_contract::snapshot::AgentId(agent_id.clone()),
            )
        });
    let primary = publication.as_ref().map_or_else(
        || {
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new(
                    "test",
                    model.clone(),
                    runtime.clone(),
                ),
            )
        },
        |publication| publication.resolved_spec.model_binding.clone(),
    );
    let published_model = primary.binding().identity().1.to_owned();
    let published_runtime = primary.binding().identity().2.to_owned();
    let model_override = Some(
        awaken_session_configuration_contract::SessionModelOverride {
            publication: Some(Box::new(
                awaken_session_configuration_contract::SessionModelPublication {
                    primary,
                    candidates: publication
                        .as_ref()
                        .map(|publication| publication.resolved_spec.model_candidates.clone())
                        .unwrap_or_default(),
                },
            )),
            inference: Default::default(),
        },
    );
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment,
            runtime_placement:
                awaken_session_configuration_contract::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id,
            agent_revision: None,
            model: published_model,
            model_override,
            runtime: Some(published_runtime),
            delegate_ids,
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    let previous = awaken_session_resource_contract::SessionResourceManifest::at_revision(
        workspace_id.clone(),
        0,
        awaken_session_resource_contract::ResolvedSessionResources::default(),
    );
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        managed,
        thread,
        awaken_session_realization_contract::FrozenSessionProjection {
            workspace_id,
            revision: awaken_session_types::SessionRevision(1),
            baseline,
            agent_publication: publication,
            environment: Default::default(),
            resource_revision,
            resources,
            previous_resource_manifest: Some(previous),
            tools,
            mcp: Vec::new(),
            request_context: Vec::new(),
        },
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
}

#[async_trait::async_trait]
trait CompleteTestSessionFixture {
    async fn install_complete_test_session(
        &self,
        thread: &str,
        init: awaken_session_configuration_contract::SessionInit,
    ) -> Result<(), awaken_session_types::RunError>;
}

#[async_trait::async_trait]
impl CompleteTestSessionFixture for crate::ManagedHost {
    async fn install_complete_test_session(
        &self,
        thread: &str,
        init: awaken_session_configuration_contract::SessionInit,
    ) -> Result<(), awaken_session_types::RunError> {
        install_complete_test_session(self, thread, init).await
    }
}

/// Add the ordinary Session realization authority used by fixtures that must
/// create a physical Environment and later mutate it. C1 a complete Dispatch
/// projection plus C2 one live lease and binding sink produce E1 a fenced V2
/// handle with exact owned-path evidence; tests never synthesize that handle.
async fn install_complete_test_session_for_realization(
    managed: &crate::ManagedHost,
    thread: &str,
    init: awaken_session_configuration_contract::SessionInit,
) -> Result<(), awaken_session_types::RunError> {
    managed.install_test_environment_binding_sink(Arc::new(BindingOrderSink {
        host: Arc::downgrade(&managed.host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    }));
    install_complete_test_session(managed, thread, init).await?;
    managed
        .host
        .install_session_realization_lease(
            thread,
            awaken_session_resource_contract::SessionRealizationLease {
                owner: "runtime-host-test".into(),
                runtime_incarnation: format!("runtime-host-test-{thread}"),
                epoch: 1,
                expires_at_unix_ms: u64::MAX,
            },
        )
        .unwrap();
    Ok(())
}

/// A trivial model for resource-lifecycle Runs.
struct OkModel;

#[async_trait::async_trait]
impl LlmExecutor for OkModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("ok"),
            usage: None,
            stop_reason: None,
        })
    }
}

struct BindingOrderSink {
    host: std::sync::Weak<SharedHost>,
    authorize_calls: AtomicUsize,
    calls: AtomicUsize,
    observed_before_publish: std::sync::atomic::AtomicBool,
    fail: bool,
    require_realization: bool,
    owned_session_id: Option<String>,
    committed_environment:
        Option<Arc<Mutex<Option<awaken_session_state_contract::SessionEnvironmentState>>>>,
}

fn test_committed_environment(
    receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
) -> awaken_session_state_contract::SessionEnvironmentState {
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &receipt.session_id,
        1,
        u64::MAX,
        "test-environment",
        "test-image",
    );
    awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: receipt.binding,
        effect_id: Some(receipt.effect_id),
        generation: Some(generation),
        idle_since_unix_ms: None,
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionEnvironmentBindingSink for BindingOrderSink {
    async fn authorize(
        &self,
        intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_types::RunError,
    > {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .owned_session_id
            .as_deref()
            .is_some_and(|owned| owned != intent.session_id())
        {
            Ok(awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned)
        } else if self.require_realization && intent.realization().is_none() {
            Err(awaken_session_types::RunError::unavailable_classified(
                "session_realization_stale",
                "replacement realization is not installed",
            ))
        } else {
            Ok(awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized)
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_state_contract::SessionEnvironmentReceipt,
    ) -> Result<
        awaken_session_state_contract::SessionEnvironmentState,
        awaken_session_types::RunError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let host = self.host.upgrade().expect("host remains live");
        self.observed_before_publish.store(
            host.realizer
                .session_environment(receipt.session_id.as_str())
                .await
                .is_none(),
            Ordering::SeqCst,
        );
        if self.fail {
            Err(awaken_session_types::RunError::unavailable(
                "binding store unavailable",
            ))
        } else {
            let committed = test_committed_environment(receipt);
            if let Some(recorded) = &self.committed_environment {
                *recorded.lock().unwrap() = Some(committed.clone());
            }
            Ok(committed)
        }
    }
}

// ---------------------------------------------------------------------------
// Resource-plane seam coverage: Runtime consumes one effective Session manifest.
//   G1 consistency  — prompt path/access == realized mount path/access
//   G4 fail-closed  — an effective resource with missing backing aborts preparation
//   G5 distribution — a worker activates carried inputs without an authoring DB
// ---------------------------------------------------------------------------

/// A bare session for `agent` with no wire resources — the common "just run the agent"
/// path where only its bound resources apply.
#[cfg(test)]
fn bare_session(
    agent: &str,
    workspace: &str,
) -> awaken_session_configuration_contract::SessionInit {
    awaken_session_configuration_contract::SessionInit {
        workspace_id: workspace.into(),
        agent_id: agent.into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    }
}

/// Calls the client-executed tool `lookup` until it sees a tool result, then ends
/// by echoing what the result carried — so a test can prove the delivered client
/// result actually reached the model's next inference.
struct ClientLookupModel;

#[async_trait::async_trait]
impl LlmExecutor for ClientLookupModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let tool_text = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(block_text(content)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(",");
        let output = if tool_text.is_empty() {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".into(),
                tool_id: "lookup".into(),
                arguments: serde_json::json!({ "q": "weather" }),
            }])
        } else {
            AssistantOutput::text(format!("result was {tool_text}"))
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[derive(Clone, Copy)]
enum TerminalRecoveryScenario {
    OrphanClaim,
    TotalAbsence,
    UnavailableAuxiliaryMemory,
    Disposing,
    LiveMemory,
    Restoring,
}

struct TerminalRecoveryProvider {
    restored_disposals: Mutex<Vec<pc::SandboxDisposalAuthorization>>,
    events: Arc<Mutex<Vec<&'static str>>>,
    scenario: TerminalRecoveryScenario,
    fence_refresh_probe: Option<Arc<TerminalFenceRefreshProbe>>,
}

#[derive(Default)]
struct TerminalFenceRefreshProbe {
    restored_release: tokio::sync::Notify,
    artifact_entered: tokio::sync::Notify,
    artifact_release: tokio::sync::Notify,
    memory_entered: tokio::sync::Notify,
    memory_release: tokio::sync::Notify,
    checkpoint_entered: tokio::sync::Notify,
    checkpoint_release: tokio::sync::Notify,
    fences: Mutex<Vec<(&'static str, pc::SandboxEffectFence)>>,
}

impl TerminalFenceRefreshProbe {
    fn record(&self, boundary: &'static str, fence: &pc::SandboxEffectFence) {
        self.fences.lock().unwrap().push((boundary, fence.clone()));
    }
}

#[async_trait::async_trait]
impl awaken_container_contract::ContainerEnvironmentProvider for TerminalRecoveryProvider {
    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_container_contract::ContainerAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn physical_incarnation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<String, awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::container_physical_incarnation(handle)
    }

    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        managed_test_container_capabilities()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn create_unattached_environment(
        &self,
        _spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn awaken_container_contract::ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "absent-auxiliary fixture never creates a container",
        ))
    }

    async fn create_environment_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn pc::CurrentExecutionAuthority,
        _intent: awaken_container_contract::EnvironmentRealizationIntent,
    ) -> Result<Arc<dyn awaken_container_contract::ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "absent-auxiliary fixture never creates a container",
        ))
    }

    async fn observe_environment(
        &self,
        adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        if matches!(basis, pc::SandboxObservationBasis::Unattached) {
            return Err(pc::SandboxError::unsupported(
                "terminal recovery fixture observes only under an effect fence",
            ));
        }
        let physical_incarnation = adoption
            .handle
            .container_physical_incarnation()?
            .to_string();
        match self.scenario {
            TerminalRecoveryScenario::OrphanClaim => {
                self.events.lock().unwrap().push("observe-orphan-claim");
                Ok(pc::SandboxObservation::Incompatible {
                    reason: "live continuation claim has no source Pod cleanup gate".into(),
                })
            }
            TerminalRecoveryScenario::TotalAbsence => {
                self.events.lock().unwrap().push("observe-total-absence");
                Ok(pc::SandboxObservation::DefinitivelyUnavailable {
                    physical_incarnation: Some(physical_incarnation),
                })
            }
            TerminalRecoveryScenario::UnavailableAuxiliaryMemory => {
                self.events
                    .lock()
                    .unwrap()
                    .push("observe-unavailable-memory");
                Ok(pc::SandboxObservation::DefinitivelyUnavailable {
                    physical_incarnation: Some(physical_incarnation),
                })
            }
            TerminalRecoveryScenario::Disposing => {
                self.events.lock().unwrap().push("observe-disposing");
                Ok(pc::SandboxObservation::Disposing {
                    physical_incarnation,
                })
            }
            TerminalRecoveryScenario::LiveMemory => {
                self.events.lock().unwrap().push("observe-live-memory");
                Ok(pc::SandboxObservation::Ready)
            }
            TerminalRecoveryScenario::Restoring => Err(pc::SandboxError::unsupported(
                "Restoring terminal cleanup must not observe an ordinary Environment",
            )),
        }
    }

    async fn adopt_unattached_environment(
        &self,
        _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn awaken_container_contract::ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "absent-auxiliary fixture never adopts a live container",
        ))
    }

    async fn adopt_environment_for_effect(
        &self,
        adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Arc<dyn awaken_container_contract::ContainerEnvironment>, pc::SandboxError> {
        if !matches!(self.scenario, TerminalRecoveryScenario::LiveMemory) {
            return Err(pc::SandboxError::unsupported(
                "non-live terminal recovery fixture cannot be ordinarily adopted",
            ));
        }
        pc::with_current_sandbox_effect(effect_fence, authority, |_| {
            self.events.lock().unwrap().push("adopt-live-memory");
            Ok(())
        })?;
        Ok(Arc::new(TerminalRecoveryEnvironment {
            handle: adoption.handle.clone(),
            events: self.events.clone(),
            mode: TerminalRecoveryEnvironmentMode::LiveMemory,
            fence_refresh_probe: self.fence_refresh_probe.clone(),
        }))
    }

    async fn prepare_terminal_environment_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        _expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<Arc<dyn awaken_container_contract::ContainerEnvironment>>, pc::SandboxError>
    {
        pc::with_current_sandbox_effect::<
            Option<Arc<dyn awaken_container_contract::ContainerEnvironment>>,
        >(terminal_effect_fence, authority, |_| match self.scenario {
            TerminalRecoveryScenario::OrphanClaim => Err(pc::SandboxError::unsupported(
                "orphan-claim observation must fail before terminal preparation",
            )),
            TerminalRecoveryScenario::TotalAbsence => {
                self.events.lock().unwrap().push("prepare-total-absence");
                Ok(None)
            }
            TerminalRecoveryScenario::UnavailableAuxiliaryMemory => {
                self.events
                    .lock()
                    .unwrap()
                    .push("reconstruct-unavailable-memory");
                Ok(Some(Arc::new(TerminalRecoveryEnvironment {
                    handle: handle
                        .ok_or_else(|| {
                            pc::SandboxError::unsupported(
                                "unavailable auxiliary Memory fixture requires its durable handle",
                            )
                        })?
                        .clone(),
                    events: self.events.clone(),
                    mode: TerminalRecoveryEnvironmentMode::UnavailableAuxiliaryMemory,
                    fence_refresh_probe: self.fence_refresh_probe.clone(),
                })))
            }
            TerminalRecoveryScenario::Disposing => {
                self.events.lock().unwrap().push("prepare-disposing");
                Ok(Some(Arc::new(TerminalRecoveryEnvironment {
                    handle: handle
                        .ok_or_else(|| {
                            pc::SandboxError::unsupported(
                                "disposing fixture requires an exact durable handle",
                            )
                        })?
                        .clone(),
                    events: self.events.clone(),
                    mode: TerminalRecoveryEnvironmentMode::Disposing,
                    fence_refresh_probe: self.fence_refresh_probe.clone(),
                })))
            }
            TerminalRecoveryScenario::LiveMemory => {
                self.events.lock().unwrap().push("prepare-live-memory");
                Ok(Some(Arc::new(TerminalRecoveryEnvironment {
                    handle: handle
                        .ok_or_else(|| {
                            pc::SandboxError::unsupported(
                                "live Memory fixture requires an exact durable handle",
                            )
                        })?
                        .clone(),
                    events: self.events.clone(),
                    mode: TerminalRecoveryEnvironmentMode::LiveMemory,
                    fence_refresh_probe: self.fence_refresh_probe.clone(),
                })))
            }
            TerminalRecoveryScenario::Restoring => Err(pc::SandboxError::unsupported(
                "Restoring terminal cleanup must use exact target disposal",
            )),
        })
    }

    fn restore_targets(
        &self,
    ) -> Option<
        &dyn pc::RestoreTargetProvider<Arc<dyn awaken_container_contract::ContainerEnvironment>>,
    > {
        Some(self)
    }
}

#[async_trait::async_trait]
impl pc::RestoreTargetProvider<Arc<dyn awaken_container_contract::ContainerEnvironment>>
    for TerminalRecoveryProvider
{
    async fn acquire_restore(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
    ) -> Result<
        pc::SandboxRestoreTarget<Arc<dyn awaken_container_contract::ContainerEnvironment>>,
        pc::SandboxError,
    > {
        Err(pc::SandboxError::unsupported(
            "terminal recovery fixture never acquires a restore target",
        ))
    }

    async fn dispose_restored(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        if !matches!(self.scenario, TerminalRecoveryScenario::Restoring) {
            return Err(pc::SandboxError::unsupported(
                "non-Restoring fixture cannot dispose a restore target",
            ));
        }
        authorization.validate()?;
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))?;
        self.restored_disposals
            .lock()
            .unwrap()
            .push(authorization.clone());
        self.events.lock().unwrap().push("dispose-restored");
        if let Some(probe) = &self.fence_refresh_probe {
            probe.restored_release.notified().await;
        }
        // Intentionally return the admitted backend result even after proof
        // loss. TR4 must exercise the Host's own post-response guard rather
        // than pass because this fixture preemptively rejects the response.
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TerminalRecoveryEnvironmentMode {
    Disposing,
    UnavailableAuxiliaryMemory,
    LiveMemory,
}

struct TerminalRecoveryEnvironment {
    handle: pc::SandboxHandle,
    events: Arc<Mutex<Vec<&'static str>>>,
    mode: TerminalRecoveryEnvironmentMode,
    fence_refresh_probe: Option<Arc<TerminalFenceRefreshProbe>>,
}

#[async_trait::async_trait]
impl pc::Sandbox for TerminalRecoveryEnvironment {
    async fn dispose_unattached(&self) -> Result<(), pc::SandboxError> {
        self.dispose().await
    }

    fn id(&self) -> &str {
        &self.handle.sandbox_id
    }

    fn handle(&self) -> pc::SandboxHandle {
        self.handle.clone()
    }

    fn checkpointing(&self) -> Option<&dyn pc::CheckpointingSandbox> {
        Some(self)
    }

    async fn spawn(
        &self,
        _command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        self.events.lock().unwrap().push("spawn-live-io");
        Err(pc::SandboxError::unsupported(
            "terminal recovery fixture must not spawn a process",
        ))
    }

    async fn attach(
        &self,
        _requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "terminal recovery fixture must not attach a mount",
        ))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        self.events.lock().unwrap().push("artifacts-live-io");
        Err(pc::SandboxError::unsupported(
            "container Artifact capture must use the batch file seam",
        ))
    }

    async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.events.lock().unwrap().push("artifact-read-live-io");
        Err(pc::SandboxError::unsupported(
            "container Artifact capture must not use per-file reads",
        ))
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &[]
    }

    async fn process(
        &self,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        Err(pc::SandboxError::not_found(
            "terminal recovery fixture has no reconnectable process",
        ))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        Ok(pc::SandboxStatus::Terminated)
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        if self.mode == TerminalRecoveryEnvironmentMode::LiveMemory {
            self.events.lock().unwrap().push("renew-live-memory");
            Ok(())
        } else {
            Err(pc::SandboxError::unsupported(
                "cleanup-only terminal recovery fixture must not renew a lease",
            ))
        }
    }

    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        let expected = self.handle.memory_materializations()?.unwrap_or_default();
        if let Some(probe) = &self.fence_refresh_probe {
            probe.record("memory", effect_fence);
            probe.memory_entered.notify_one();
            probe.memory_release.notified().await;
        }
        pc::MemoryReconciliationAck::default().acknowledge(
            effect_fence,
            authority,
            expected,
            complete_materializations,
            |_| {
                if matches!(
                    self.mode,
                    TerminalRecoveryEnvironmentMode::LiveMemory
                        | TerminalRecoveryEnvironmentMode::UnavailableAuxiliaryMemory
                ) {
                    self.events.lock().unwrap().push("ack-memory");
                }
                Ok(())
            },
        )
    }

    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        // The fixture consumes the actual captured Host authority too; a stale
        // slot must not record successful provider preparation.
        pc::MemoryReconciliationAck::default().require_for_disposal(
            &[],
            effect_fence,
            authority,
            |_| Ok(()),
        )?;
        self.events.lock().unwrap().push("prepare-source");
        if let Some(probe) = &self.fence_refresh_probe {
            probe.record("provider", effect_fence);
        }
        Ok(effect_fence.clone())
    }

    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| {
            self.events.lock().unwrap().push("dispose-physical");
            Ok(())
        })
    }
}

#[async_trait::async_trait]
impl pc::CheckpointingSandbox for TerminalRecoveryEnvironment {
    async fn checkpoint_for_effect(
        &self,
        _request: &pc::SandboxCheckpointCaptureRequest,
        _store: &dyn pc::SandboxCheckpointStore,
        _effect_fence: &pc::SandboxEffectFence,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "terminal recovery fixture never takes a new checkpoint",
        ))
    }

    async fn cleanup_checkpoint_for_terminal(
        &self,
        _request: &pc::SandboxCheckpointCaptureRequest,
        _store: &dyn pc::SandboxCheckpointStore,
        _expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        pc::with_current_sandbox_effect(terminal_effect_fence, authority, |_| {
            self.events.lock().unwrap().push("checkpoint-live-io");
            Ok(())
        })?;
        if let Some(probe) = &self.fence_refresh_probe {
            probe.record("checkpoint", terminal_effect_fence);
            probe.checkpoint_entered.notify_one();
            probe.checkpoint_release.notified().await;
            return pc::with_current_sandbox_effect(terminal_effect_fence, authority, |_| Ok(()));
        }
        Err(pc::SandboxError::not_found(
            "terminal recovery fixture has no pending checkpoint upload",
        ))
    }
}

impl TerminalRecoveryEnvironment {
    // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
    // are no longer a trait obligation or a reachable escape hatch. They stay
    // as this type's own implementation detail, used by its fenced edges.
    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::fence_rejected(
            "terminal recovery fixture requires the exact effect fence",
        ))
    }
}

#[async_trait::async_trait]
impl awaken_sandbox_control::SandboxControlServicePublisher for TerminalRecoveryEnvironment {
    async fn publish_sandbox_control_service(
        &self,
        _kind: awaken_sandbox_control::SandboxControlServiceKind,
        _service: Arc<dyn awaken_sandbox_control::SandboxControlService>,
    ) -> Result<
        Box<dyn awaken_sandbox_control::PublishedSandboxControlService>,
        awaken_sandbox_control::SandboxControlPublishError,
    > {
        // This recovery fixture declares no control-service topology. It must
        // fail closed instead of manufacturing a provider publication lease.
        Err(awaken_sandbox_control::SandboxControlPublishError)
    }
}

#[async_trait::async_trait]
impl awaken_container_contract::ContainerEnvironment for TerminalRecoveryEnvironment {
    async fn owned_process_binding(
        &self,
    ) -> Result<Option<String>, awaken_provisioning_contract::SandboxError> {
        // This fixture retains no process containment.
        Ok(None)
    }

    async fn checkpoint_upload(
        &self,
        _access: awaken_container_contract::ContainerCheckpointAccess<'_>,
        _mutation: awaken_container_contract::ContainerCheckpointMutation<'_>,
    ) -> Result<
        Option<awaken_provisioning_contract::SandboxCheckpointUpload>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not implement a fenced checkpoint participant",
        ))
    }

    async fn remove_live_input_path(
        &self,
        _path: &str,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "late mount removal is unsupported on this test container",
        ))
    }

    fn record_owned_path(
        &self,
        _path: &str,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not support durable owned-path evidence",
        ))
    }

    async fn open_agent_channel(
        &self,
    ) -> Result<
        Box<dyn awaken_agent_channel::AgentChannel>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not expose a resident agent channel",
        ))
    }

    async fn spawn_agent_process(
        &self,
        _command: pc::Command,
    ) -> Result<awaken_container_contract::RuntimeAgentProcess, pc::SandboxError> {
        self.events.lock().unwrap().push("agent-live-io");
        Err(pc::SandboxError::unsupported(
            "terminal recovery fixture must not launch an Agent process",
        ))
    }

    async fn read_files(
        &self,
        root: &str,
    ) -> Result<Vec<awaken_container_contract::EnvironmentFile>, pc::SandboxError> {
        match self.mode {
            TerminalRecoveryEnvironmentMode::Disposing => {
                self.events.lock().unwrap().push("read-files-live-io");
                Err(pc::SandboxError::unsupported(
                    "Disposing cleanup must not read the container filesystem",
                ))
            }
            TerminalRecoveryEnvironmentMode::UnavailableAuxiliaryMemory => {
                self.events.lock().unwrap().push("read-files-live-io");
                Err(pc::SandboxError::unsupported(
                    "unprepared unavailable Memory must fail before live file reads",
                ))
            }
            TerminalRecoveryEnvironmentMode::LiveMemory if root == self.outputs_path() => {
                self.events.lock().unwrap().push("capture-artifacts");
                if let Some(probe) = &self.fence_refresh_probe {
                    probe.artifact_entered.notify_one();
                    probe.artifact_release.notified().await;
                }
                Ok(Vec::new())
            }
            TerminalRecoveryEnvironmentMode::LiveMemory
                if self
                    .handle
                    .memory_materializations()?
                    .unwrap_or_default()
                    .iter()
                    .any(|materialization| materialization.mount_path == root) =>
            {
                self.events.lock().unwrap().push("read-terminal-memory");
                Ok(vec![awaken_container_contract::EnvironmentFile {
                    path: "changed.txt".into(),
                    bytes: b"changed-under-terminal-v2".to_vec(),
                }])
            }
            TerminalRecoveryEnvironmentMode::LiveMemory => Err(pc::SandboxError::invalid(
                "live terminal fixture was asked to read an unfrozen path",
            )),
        }
    }
}

pub(super) fn terminal_fixture_container_handle(
    thread: &str,
    spec: &pc::SandboxSpec,
) -> pc::SandboxHandle {
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
    pc::SandboxHandle::container_v2(
        thread,
        awaken_sandbox_container::ContainerSandboxHandleV2 {
            previous: awaken_sandbox_container::ContainerSandboxHandleV1 {
                container_id: "awaken-terminal-fixture".into(),
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection: false,
                continuation_excluded_paths: Vec::new(),
                runtime_handle: Some(awaken_sandbox_container::ContainerContinuationHandle::KubernetesContinuationV2 {
                    pod_uid: "pod-a".into(),
                    claim_uid: Some("claim-p".into()),
                }),
                sandbox_control_incarnation: None,
                control_services: spec.control_services.clone(),
            },
            adoption_fingerprint: fingerprint.clone(),
            realization_fingerprint: fingerprint,
            owned_paths: Vec::new(),
        },
    )
}

pub(super) struct RecordingTerminalCheckpointStore {
    pub(super) events: Arc<Mutex<Vec<&'static str>>>,
    pub(super) expected_id: String,
    pub(super) delete_gate: Option<Arc<tokio::sync::Semaphore>>,
}

#[async_trait::async_trait]
impl pc::SandboxCheckpointStore for RecordingTerminalCheckpointStore {
    async fn put(
        &self,
        _metadata: &pc::CheckpointObjectMetadata,
        _bytes: Vec<u8>,
    ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "terminal committed-checkpoint cleanup must not upload bytes",
        ))
    }

    async fn get(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "terminal committed-checkpoint cleanup must not read bytes",
        ))
    }

    async fn delete(&self, id: &str) -> Result<(), pc::SandboxError> {
        if id != self.expected_id {
            return Err(pc::SandboxError::conflict(
                "terminal cleanup targeted another checkpoint object",
            ));
        }
        self.events.lock().unwrap().push("delete-checkpoint");
        if let Some(gate) = &self.delete_gate {
            let _permit = gate.acquire().await.unwrap();
        }
        Ok(())
    }
}

pub(super) fn terminal_recovery_effect(
    thread: &str,
    owner: &str,
) -> (
    awaken_session_resource_contract::SessionRealizationLease,
    awaken_session_state_contract::SessionTerminalCleanupEffect,
) {
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: owner.into(),
        runtime_incarnation: format!("{owner}:incarnation"),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    let mut cleanup = awaken_session_state_contract::SessionCleanupOperation::default();
    assert!(cleanup.request(&awaken_session_types::SessionId::from(thread)));
    cleanup
        .freeze_targets(&awaken_session_types::SessionId::from(thread), [], 0, 0)
        .unwrap();
    let command = cleanup
        .command_for(&awaken_session_types::SessionId::from(thread), thread)
        .unwrap();
    let effect =
        awaken_session_state_contract::SessionTerminalCleanupEffect::new(command, lease.clone());
    (lease, effect)
}

pub(super) fn disposing_terminal_environment(
    thread: &str,
    lease: &awaken_session_resource_contract::SessionRealizationLease,
    handle: &pc::SandboxHandle,
    checkpoint_id: &str,
) -> (
    awaken_session_state_contract::SessionEnvironmentState,
    awaken_session_state_contract::SourceReleasePreparationEffect,
    awaken_session_state_contract::SourceReleasePreparedReceipt,
) {
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(thread),
        1,
        u64::MAX,
        "environment-disposing",
        "image-disposing",
    );
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        &awaken_session_types::SessionId::from(thread),
        "suspend",
        &generation,
        7,
        Some(lease.clone()),
        None,
    );
    let preparation: awaken_session_state_contract::SourceReleasePreparationEffect =
        serde_json::from_value(serde_json::json!({
            "operation": operation.clone(),
            "lease": lease.clone(),
        }))
        .expect("decode the aggregate-owned continuation preparation");
    let source_binding = serde_json::to_string(handle).expect("encode disposing source binding");
    let receipt = awaken_session_state_contract::SourceReleasePreparedReceipt::try_new(
        preparation.clone(),
        preparation.sandbox_effect_fence().unwrap(),
        &generation,
        &source_binding,
    )
    .unwrap();
    let state = awaken_session_state_contract::SessionEnvironmentState::Suspending {
        operation: operation.clone(),
        source_effect_id: Box::new("terminal-source-effect".into()),
        source_binding,
        generation,
        suspend_phase: awaken_session_state_contract::SuspendPhase::Disposing,
        checkpoint: Some(awaken_session_state_contract::SandboxCheckpointRef {
            id: checkpoint_id.into(),
            format: "awaken-fs-v1".into(),
            digest: "checkpoint-digest".into(),
            size_bytes: 1,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: "environment-disposing".into(),
            base_image_fingerprint: "image-disposing".into(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: operation.effect_id,
        }),
        source_release_preparation: Some(Box::new(receipt.clone())),
    };
    (state, preparation, receipt)
}

fn terminal_test_session(
    session_id: &str,
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
    lease: awaken_session_resource_contract::SessionRealizationLease,
    children: impl IntoIterator<Item = String>,
    publication: Option<awaken_session_state_contract::SessionRepositoryPublicationIntent>,
) -> awaken_session_state_contract::PersistedSession {
    let mut session = awaken_session_state_contract::PersistedSession::frozen_with_budget(
        session_id,
        projection.baseline.clone(),
        awaken_session_state_contract::SessionResourceState::from_active(
            projection.resources.clone(),
        ),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    session.environment = projection.environment.clone();
    session.realization = Some(lease);
    match publication {
        Some(intent) => {
            session
                .terminal_cleanup
                .request_with_publication(
                    &awaken_session_types::SessionId::from(session_id),
                    intent,
                )
                .expect("request the aggregate-owned terminal publication");
        }
        None => assert!(session.ensure_terminal_cleanup_fence()),
    }
    session
        .freeze_terminal_cleanup_targets(children, 0, 0)
        .expect("freeze the aggregate-owned terminal target set");
    session
}

fn terminal_preparation_authorization(
    effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
) -> awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization {
    let session_id = &effect.command.session_id;
    let thread_id = &effect.command.thread_id;
    let session = terminal_test_session(
        session_id.as_str(),
        projection,
        effect.lease.clone(),
        (thread_id != session_id).then_some(thread_id.clone()),
        None,
    );
    let inherited_provider_disposal = session
        .authorize_terminal_cleanup_effect(effect)
        .expect("derive preparation authorization from the complete Session aggregate");
    awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
        effect.clone(),
        projection.workspace_id.clone(),
        inherited_provider_disposal,
    )
    .expect("close the aggregate-derived terminal preparation authorization")
}

fn terminal_disposal_effect(
    preparation: awaken_session_state_contract::SessionCleanupPreparation,
    current_lease: awaken_session_resource_contract::SessionRealizationLease,
    projection: &awaken_session_realization_contract::FrozenSessionProjection,
) -> awaken_session_state_contract::SessionTerminalCleanupDisposalEffect {
    let session_id = preparation.effect.command.session_id.clone();
    let thread_id = preparation.effect.command.thread_id.clone();
    let mut session = terminal_test_session(
        session_id.as_str(),
        projection,
        current_lease.clone(),
        (thread_id != session_id).then_some(thread_id.clone()),
        None,
    );
    let mut expected_command = session
        .terminal_cleanup
        .command_for(&session_id, &thread_id)
        .expect("the aggregate retains the exact preparation command");
    if thread_id == session_id
        && let Some(request) = projection
            .environment
            .restoring_request(&projection.workspace_id, &session_id)
    {
        expected_command = expected_command
            .with_restore_target(request)
            .expect("the root command owns its exact restore target");
    }
    assert_eq!(
        expected_command, preparation.effect.command,
        "the Host receipt must bind the aggregate-derived command",
    );
    let repository_preparation =
        awaken_session_state_contract::SessionCleanupRepositoryPreparation::new(
            &session_id,
            &projection.workspace_id,
            &session.resources,
        )
        .expect("prepare the aggregate-owned Repository retirement plan");
    session
        .record_terminal_cleanup_preparation(
            &projection.workspace_id,
            &current_lease,
            preparation,
            Some(repository_preparation),
        )
        .expect("admit the exact preparation before projecting disposal");
    let command = match session
        .terminal_cleanup_work_action()
        .expect("project the canonical terminal action")
        .expect("the complete preparation set enters Disposing")
    {
        awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { command } => command,
        action => panic!("expected aggregate disposal action, got {action:?}"),
    };
    awaken_session_state_contract::SessionTerminalCleanupDisposalEffect::new(command, current_lease)
}

async fn install_terminal_recovery_projection(
    host: &Arc<SharedHost>,
    managed: &crate::ManagedHost,
    thread: &str,
    lease: awaken_session_resource_contract::SessionRealizationLease,
    environment: awaken_session_state_contract::SessionEnvironmentState,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    let mut projection = remote_terminal_cleanup_projection();
    awaken_session_execution_contract::RealizationRuntime::install_session_projection(
        managed,
        thread,
        projection.clone(),
        awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("install frozen terminal recovery projection");
    // A changed aggregate Environment is a new root-CAS snapshot.
    projection.revision.0 += 1;
    projection.environment = environment;
    host.install_test_terminal_cleanup_projection(
        &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            authority: crate::installed_session::test_lease_receipt(lease),
        },
    )
    .await
    .expect("install exact terminal recovery assignment");
    projection
}

fn remote_terminal_cleanup_projection()
-> awaken_session_realization_contract::FrozenSessionProjection {
    remote_terminal_cleanup_projection_with_idle_retention(Default::default())
}

fn remote_terminal_cleanup_projection_with_idle_retention(
    idle_retention: awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    let mut environment = session_environment(
        awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        serde_json::json!({}),
    );
    environment.idle_retention = idle_retention;
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment,
            runtime_placement:
                awaken_session_configuration_contract::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "terminal-agent".into(),
            agent_revision: None,
            model_override: None,
            model: "terminal-model".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    awaken_session_realization_contract::FrozenSessionProjection {
        workspace_id: "terminal-workspace".into(),
        revision: awaken_session_types::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: Some(
            awaken_session_resource_contract::SessionResourceManifest::new(
                "terminal-workspace",
                awaken_session_resource_contract::ResolvedSessionResources::default(),
            ),
        ),
        mcp: Vec::new(),
        tools: Default::default(),
        request_context: Vec::new(),
    }
}

fn install_test_renewal_authority(
    host: &SharedHost,
    control: &RemoteTerminalCleanupControl,
    session_id: &str,
    lease: awaken_session_resource_contract::SessionRealizationLease,
) {
    host.install_session_realization_lease(session_id, lease.clone())
        .unwrap();
    control
        .renewals
        .lock()
        .unwrap()
        .insert(session_id.into(), lease);
}
