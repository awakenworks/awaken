use super::*;
use crate::config::block_text;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
use awaken_session_contract::SessionRuntime;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

fn test_model_binding() -> awaken_runtime_contract::resolved::ModelBinding {
    awaken_runtime_contract::resolved::ModelBinding::new("test", "model", "native")
}

fn awaiting_tool_batch_state(
    run_id: &RunId,
    ticket: &ResumeTicket,
    wait_kind: awaken_runtime_contract::ToolWaitKind,
) -> awaken_agent_contract::agent::state::Command {
    let awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall { call_id, tool, .. } =
        ticket.target()
    else {
        panic!("awaiting tool-batch fixture requires a tool ticket")
    };
    let mut batch = awaken_runtime_contract::ToolBatch::for_step(
        run_id.clone(),
        0,
        [(
            awaken_runtime_contract::llm::ToolCall {
                call_id: call_id.clone(),
                tool_id: tool.tool_id.clone(),
                arguments: tool.arguments.clone(),
            },
            awaken_runtime_contract::ToolRecoveryPolicy::default(),
        )],
    )
    .expect("valid awaiting tool-batch fixture");
    batch
        .mark_awaiting(call_id, wait_kind, ticket.correlation_id.clone())
        .expect("ticket and tool batch enter one exact wait");
    awaken_runtime_contract::ActiveToolBatch::write(&Some(batch))
}

/// Approval-state tests name their precondition explicitly. Managed Agent
/// members default to always-allow; this exact test override asks only for
/// `write` while leaving unrelated follow-up effects unchanged.
fn write_confirmation_gate() -> Arc<awaken_runtime::PermissionGate> {
    let toolsets = [crate::config::test_agent_toolset_permission(
        "write",
        awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
    )];
    let policy = awaken_ext_permission::RuleBasedToolPermissionPolicy::new(
        crate::config::effective_ruleset_with_toolsets(None, &[], &toolsets),
    );
    Arc::new(awaken_runtime::PermissionGate::new(Arc::new(policy)))
}

fn host_requiring_write_confirmation(model: Arc<dyn LlmExecutor>) -> Arc<SharedHost> {
    let host =
        Arc::new(SharedHost::new(model, "stub").with_gate_override(write_confirmation_gate()));
    let _managed = install_test_dispatch_runtime(&host);
    host
}

fn completed_outcome(progress: HostOutcomeDrive) -> HostOutcomeReport {
    match progress {
        HostOutcomeDrive::Completed(report) => report,
        HostOutcomeDrive::Awaiting => panic!("test Outcome unexpectedly awaited input"),
    }
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
) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
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
) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
    run_prepared_session_messages(managed, agent, thread, vec![crate::user_message(content)]).await
}

fn native_credential_profile() -> awaken_runtime_contract::CredentialRealizationProfile {
    awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
}

fn managed_test_container_capabilities() -> awaken_provisioning_contract::SandboxCapabilities {
    awaken_provisioning_contract::SandboxCapabilities {
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
    network: awaken_session_contract::SessionNetworkPolicy,
    sandbox: serde_json::Value,
) -> awaken_session_contract::EnvironmentSnapshot {
    let sandbox: awaken_provisioning_contract::SandboxOverride =
        serde_json::from_value(sandbox).expect("valid SandboxOverride test fixture");
    let config_fingerprint = awaken_session_contract::EnvironmentFingerprint(
        awaken_agent_contract::stable_fingerprint(&(network.clone(), sandbox.clone())),
    );
    awaken_session_contract::EnvironmentSnapshot {
        environment_id: "test-environment".into(),
        revision: awaken_session_contract::EnvironmentRevision(1),
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

fn on_tool_use_environment() -> awaken_session_contract::EnvironmentSnapshot {
    let mut environment = session_environment(
        awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        serde_json::json!({}),
    );
    environment.sandbox_provisioning = awaken_session_contract::SandboxProvisioning::OnToolUse;
    environment
}

fn resource_transition(
    workspace: &str,
    previous_revision: u64,
    previous: awaken_session_contract::ResolvedSessionResources,
    desired_revision: u64,
    desired: awaken_session_contract::ResolvedSessionResources,
) -> awaken_session_contract::SessionResourceTransition {
    awaken_session_contract::SessionResourceTransition::new(
        awaken_session_contract::SessionResourceManifest::at_revision(
            workspace,
            previous_revision,
            previous,
        ),
        awaken_session_contract::SessionResourceManifest::at_revision(
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

/// Test-only proof that a Managed Session was composed with its required
/// SessionApplication port. Individual coordination adapters have their own
/// behavioral fakes; this one admits the ordinary unlimited model-request path
/// and rejects every coordination command, so an environment test cannot
/// accidentally become a second coordination implementation.
struct RejectingSessionAgentCoordination;

fn reject_environment_test_coordination<T>() -> Result<T, awaken_session_contract::RunError> {
    Err(awaken_session_contract::RunError::internal(
        "environment test must not invoke Session coordination",
    ))
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionAgentCoordination for RejectingSessionAgentCoordination {
    async fn admit_session_model_request(
        &self,
        _session_id: &str,
        _thread_id: &awaken_agent_contract::agent::thread::Id,
        _run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<bool, awaken_session_contract::RunError> {
        Ok(true)
    }

    async fn list_session_agents(
        &self,
        _session_id: &str,
    ) -> Result<
        Vec<awaken_session_contract::SessionAgentRosterEntry>,
        awaken_session_contract::RunError,
    > {
        reject_environment_test_coordination()
    }

    async fn send_session_agent_message(
        &self,
        _command: awaken_session_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_contract::SessionAgentMessageReceipt,
        awaken_session_contract::RunError,
    > {
        reject_environment_test_coordination()
    }

    async fn settle_session_agent_boundary(
        &self,
        _command: awaken_session_contract::SessionAgentBoundaryCommand,
    ) -> Result<(), awaken_session_contract::RunError> {
        reject_environment_test_coordination()
    }

    async fn interrupt_session_thread(
        &self,
        _session_id: &str,
        _child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_contract::RunError> {
        reject_environment_test_coordination()
    }

    async fn reply_session_thread_tool(
        &self,
        _command: awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(), awaken_session_contract::RunError> {
        reject_environment_test_coordination()
    }
}

/// Install the required Session application edge for tests whose subject is
/// unrelated to coordination or budget admission. A process-static strong
/// owner keeps the production-shaped weak edge alive without adding a second
/// behavioral fake to each fixture.
fn install_test_session_application(host: &Arc<SharedHost>) {
    static APPLICATION: std::sync::OnceLock<
        Arc<dyn awaken_session_contract::SessionAgentCoordination>,
    > = std::sync::OnceLock::new();
    let application = APPLICATION.get_or_init(|| Arc::new(RejectingSessionAgentCoordination));
    crate::ManagedHost::new(host.clone())
        .install_agent_coordination_application(Arc::downgrade(application))
        .expect("install one test Session application authority");
}

/// Install the existing recording coordination authority for tests that cross
/// a real Session activity settlement boundary. Environment-only fixtures use
/// the stricter rejecting authority above so an accidental coordination effect
/// still fails closed.
fn install_recording_session_application(host: &Arc<SharedHost>) {
    static APPLICATION: std::sync::OnceLock<
        Arc<dyn awaken_session_contract::SessionAgentCoordination>,
    > = std::sync::OnceLock::new();
    let application = APPLICATION.get_or_init(|| {
        Arc::new(crate::coordination::RecordingSessionAgentCoordination::default())
    });
    crate::ManagedHost::new(host.clone())
        .install_agent_coordination_application(Arc::downgrade(application))
        .expect("install recording Session application authority");
}

fn resource_registry() -> Arc<awaken_resource_application::RegistryApplication> {
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Registry"),
    );
    Arc::new(awaken_resource_application::RegistryApplication::new(
        storage,
    ))
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
        Some(Arc::new(host.memory.bind(
            thread,
            "default",
            handle,
            Some(test_resource_validator()),
            &config,
            writable,
        ))),
    );
}

struct TestLiveResourceBindingVerifier;

pub(crate) fn test_resource_validator()
-> Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier> {
    Arc::new(TestLiveResourceBindingVerifier)
}

impl awaken_resource_contract::LiveResourceBindingVerifier for TestLiveResourceBindingVerifier {
    fn verify_memory_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        Ok(())
    }

    fn verify_repository_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        Ok(())
    }
}

fn managed_with_resource_source(host: Arc<SharedHost>) -> crate::ManagedHost {
    let validator = test_resource_validator();
    install_test_session_application(&host);
    let repository_bindings = Arc::new(
        awaken_resource_application::RegistryRepositoryBindingVerifier::new(validator.clone()),
    );
    crate::ManagedHost::new(host)
        .with_resource_validator(validator.clone())
        .with_repository_binding_verifier(repository_bindings)
        .install_dispatch_session_runtime()
}

fn http_basic_material(username: &str, password: &str) -> awaken_agent_contract::RedactedString {
    let material = awaken_credential_vault::StructuredCredentialMaterial {
        type_id: awaken_runtime_contract::credential::HTTP_BASIC_MATERIAL_TYPE.into(),
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

fn repository_credential_descriptor(url: &str) -> awaken_credential_contract::CredentialDescriptor {
    awaken_credential_contract::CredentialDescriptor::new(
        "git",
        awaken_credential_contract::CredentialMaterialDescriptor::structured(
            awaken_runtime_contract::credential::HTTP_BASIC_MATERIAL_TYPE,
            ["password", "username"],
        ),
        [awaken_credential_contract::CredentialTargetContract::new(
            awaken_session_contract::repository_transport_credential_target(url)
                .expect("HTTPS Repository target"),
            awaken_session_contract::repository_transport_credential_usage(),
        )],
    )
}

fn effective_resources(
    resources: Vec<TestInput>,
) -> awaken_session_contract::ResolvedSessionResources {
    use awaken_resource_contract::{
        BindingId, FileId, MemoryStoreId, RepositoryId, ResourceAccess,
    };
    use awaken_session_contract::{ResolvedInput, ResolvedInputSource};

    awaken_session_contract::ResolvedSessionResources::try_new(
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

#[tokio::test]
async fn missing_projection_admits_only_the_direct_empty_revision_zero_noop() {
    struct Rule {
        id: &'static str,
        previous_revision: u64,
        desired_revision: u64,
        previous_nonempty: bool,
        desired_nonempty: bool,
        claimed: bool,
        accepted: bool,
    }

    // Missing-projection cause/effect decision table:
    // C1 no dispatch claim; C2 no Managed dispatch marker; C3 no frozen
    // baseline; C4 previous == desired; C5 desired revision is zero; C6 both
    // endpoint Resource sets are empty. E1 admits the direct no-op into the
    // canonical apply path; E2 rejects before File/Skill/Repository compilation.
    // C2+C3 are fixed true for every row because an installed Managed marker or
    // baseline already uses the ordinary projection branch. Explicit fixture
    // branches materialize each C6 endpoint without obscuring its empty/nonempty
    // cause behind boolean combinators.
    //
    // | Rule | C1 | C4 | C5 | C6 | Effect |
    // | D1 | T | T | T | T | E1 |
    // | D2 | F | T | T | T | E2 |
    // | D3 | T | F | F | T | E2 |
    // | D4 | T | T | F | T | E2 |
    // | D5 | T | T | T | F | E2 |
    // | D6 | T | F | T | F | E2 |
    let rules = [
        Rule {
            id: "D1-direct-empty-rev0-noop",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: false,
            accepted: true,
        },
        Rule {
            id: "D2-claimed",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: true,
            accepted: false,
        },
        Rule {
            id: "D3-advancing-empty",
            previous_revision: 0,
            desired_revision: 1,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: false,
            accepted: false,
        },
        Rule {
            id: "D4-nonzero-noop",
            previous_revision: 1,
            desired_revision: 1,
            previous_nonempty: false,
            desired_nonempty: false,
            claimed: false,
            accepted: false,
        },
        Rule {
            id: "D5-nonempty-noop",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: true,
            desired_nonempty: true,
            claimed: false,
            accepted: false,
        },
        Rule {
            id: "D6-nonempty-replacement",
            previous_revision: 0,
            desired_revision: 0,
            previous_nonempty: false,
            desired_nonempty: true,
            claimed: false,
            accepted: false,
        },
    ];

    let nonempty = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: "guard-must-not-read-this-file".into(),
        mount_path: "/guarded.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    for rule in rules {
        let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
        let _runtime = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
        let thread = format!("direct-resource-guard-{}", rule.id);
        let previous = if rule.previous_nonempty {
            nonempty.clone()
        } else {
            Default::default()
        };
        let desired = if rule.desired_nonempty {
            nonempty.clone()
        } else {
            Default::default()
        };
        let transition = resource_transition(
            host.local_workspace(),
            rule.previous_revision,
            previous,
            rule.desired_revision,
            desired,
        );
        let claim = rule.claimed.then(|| awaken_run_ingress::RunClaim {
            run_id: awaken_agent_contract::agent::run::Id(format!("run-{}", rule.id)),
            owner: "claimed-worker".into(),
            epoch: 1,
        });
        assert_eq!(
            host.session_slots
                .read(&thread, |slot| (
                    slot.baseline.is_some(),
                    slot.session_dispatch
                ))
                .unwrap_or((false, false)),
            (false, false),
            "{} keeps C2+C3 fixed",
            rule.id
        );

        let result = host
            .apply_dispatched_resource_transition(&thread, &transition, claim.as_ref())
            .await;
        if rule.accepted {
            result.unwrap_or_else(|error| panic!("{} must admit E1: {error}", rule.id));
        } else {
            let error = result.expect_err("all non-direct rows must fail closed");
            assert_eq!(
                error.code, "session_resource_projection_not_staged",
                "{} must reject at E2 before compiling the synthetic File",
                rule.id
            );
        }
    }
}

fn effective_repository(
    id: &str,
    url: &str,
    mount_path: &str,
    credential_binding: Option<String>,
) -> awaken_session_contract::ResolvedSessionResources {
    let holder =
        awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native().resource_holder;
    let credential = credential_binding.as_ref().map(|binding| {
        Box::new(awaken_session_contract::ResolvedRepositoryCredential {
            access: awaken_runtime_contract::CredentialAccess::new(
                awaken_runtime_contract::CredentialRef {
                    id: binding.clone(),
                    revision: 1,
                },
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_session_contract::repository_transport_credential_usage(),
                awaken_runtime_contract::CredentialExecutionPolicy::exact(
                    holder.clone(),
                    awaken_runtime_contract::ModelExposurePolicy::Forbidden,
                ),
            )
            .with_target(
                awaken_session_contract::repository_transport_credential_target(url)
                    .expect("HTTPS Repository target"),
            ),
            selected_plaintext_holder: holder,
        })
    });
    awaken_session_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("test-repository"),
            source: awaken_session_contract::ResolvedInputSource::Repository {
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
impl crate::RepositoryBindingVerifier<awaken_run_ingress::RunClaim> for FixedRepositoryTransport {
    async fn verify(
        &self,
        _workspace_id: &str,
        _repository_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        Ok(self.0.clone())
    }
}

struct SequencedRepositoryTransport(AtomicUsize);

#[async_trait::async_trait]
impl crate::RepositoryBindingVerifier<awaken_run_ingress::RunClaim>
    for SequencedRepositoryTransport
{
    async fn verify(
        &self,
        _workspace_id: &str,
        _repository_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _claim: Option<&awaken_run_ingress::RunClaim>,
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

struct GatewayThenDirectRepositoryTransport(AtomicUsize);

#[async_trait::async_trait]
impl crate::RepositoryBindingVerifier<awaken_run_ingress::RunClaim>
    for GatewayThenDirectRepositoryTransport
{
    async fn verify(
        &self,
        _workspace_id: &str,
        _repository_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(
                awaken_resource_contract::RepositoryTransport::GatewayMediated {
                    remote_url: "https://gateway.internal/git/repo-platform".into(),
                    capability: awaken_resource_contract::RepositoryGatewayCapability::new(
                        "repository-capability-initial",
                    )?,
                    expires_at_unix_ms: None,
                },
            )
        } else {
            Ok(awaken_resource_contract::RepositoryTransport::Direct)
        }
    }
}

#[derive(Default)]
struct RecordingRepositoryRealizer(Mutex<Vec<String>>);

#[async_trait::async_trait]
impl awaken_provisioning_contract::RepositoryRealizer for RecordingRepositoryRealizer {
    async fn realize_repository(
        &self,
        _plan: &awaken_provisioning_contract::RepositoryRealizationPlan,
        credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        self.0.lock().unwrap().push(
            credential
                .map(|credential| credential.expose_password().to_owned())
                .unwrap_or_default(),
        );
        Ok(())
    }

    async fn publish_repository(
        &self,
        plan: &awaken_provisioning_contract::RepositoryRealizationPlan,
        expectation: &awaken_provisioning_contract::RepositoryPublicationExpectation,
        _credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
    ) -> Result<
        awaken_provisioning_contract::RepositoryPublicationReceipt,
        awaken_provisioning_contract::RepositoryPublicationError,
    > {
        Ok(awaken_provisioning_contract::RepositoryPublicationReceipt::new(plan, expectation))
    }
}

fn carried_mount_bytes(mount: &awaken_provisioning_contract::MountRequirement) -> Vec<u8> {
    let awaken_provisioning_contract::MountSource::InlineBytes { contents, .. } = &mount.source
    else {
        panic!("expected a carried resource source")
    };
    contents.clone()
}

fn memory_mount_store_id(mount: &awaken_provisioning_contract::MountRequirement) -> &str {
    let awaken_provisioning_contract::MountSource::MemoryStore { store_id, .. } = &mount.source
    else {
        panic!("expected a governed memory-store source")
    };
    store_id
}

/// Test startup adapter for runtime-host's dependency-inverted MemoryMounter
/// port. Production installs `awaken-sandbox-memoryd` from awaken-coordinator.
struct TestMemoryMounter {
    fs: Arc<dyn awaken_memory_store::MemoryRepository>,
}

struct TestMemoryMount {
    heads: Vec<awaken_provisioning_contract::MemoryMaterializationHead>,
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::MemoryMount for TestMemoryMount {
    fn realization(&self) -> awaken_provisioning_contract::Realization {
        awaken_provisioning_contract::Realization::Copy
    }

    fn materialization_heads(
        &self,
    ) -> Option<Vec<awaken_provisioning_contract::MemoryMaterializationHead>> {
        Some(self.heads.clone())
    }

    async fn teardown(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::MemoryMounter for TestMemoryMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &std::path::Path,
        _access: awaken_provisioning_contract::MountAccess,
    ) -> Result<
        Box<dyn awaken_provisioning_contract::MemoryMount>,
        awaken_provisioning_contract::SandboxError,
    > {
        std::fs::create_dir_all(host_path)
            .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))?;
        let mut heads = Vec::new();
        for entry in
            self.fs.list(store_id, "/").await.map_err(|error| {
                awaken_provisioning_contract::SandboxError::new(error.to_string())
            })?
        {
            let Some(memory) =
                self.fs
                    .get_by_path(store_id, &entry.path)
                    .await
                    .map_err(|error| {
                        awaken_provisioning_contract::SandboxError::new(error.to_string())
                    })?
            else {
                continue;
            };
            heads.push(awaken_provisioning_contract::MemoryMaterializationHead {
                path: memory.path.clone(),
                id: memory.id.clone(),
                content_sha256: memory.content_sha256.clone(),
            });
            let path = host_path.join(memory.path.trim_start_matches('/'));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    awaken_provisioning_contract::SandboxError::new(error.to_string())
                })?;
            }
            std::fs::write(path, memory.content.unwrap_or_default()).map_err(|error| {
                awaken_provisioning_contract::SandboxError::new(error.to_string())
            })?;
        }
        heads.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Box::new(TestMemoryMount { heads }))
    }
}

pub(crate) fn install_test_memory_mounter(host: &SharedHost) {
    host.install_memory_mounter(Arc::new(TestMemoryMounter {
        fs: host.memory_repository(),
    }));
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
    crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime()
}

/// Wrap a fully configured ordinary Host and install the same canonical
/// Dispatch composition used by the application. Callers configure the Host
/// first, so this fixture cannot publish a partially configured adapter.
fn dispatch_test_host(host: SharedHost) -> (Arc<SharedHost>, crate::ManagedHost) {
    let host = Arc::new(host);
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
    init: awaken_session_contract::SessionInit,
) -> Result<(), awaken_session_contract::RunError> {
    let awaken_session_contract::SessionInit {
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
        awaken_session_contract::SessionToolConfiguration::from_capabilities(
            &awaken_session_contract::SessionRuntime::capabilities(managed),
        )
    });
    let publication = managed.host.agent_publications.as_ref().and_then(|source| {
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
    let published_model = primary.binding().model_ref.clone();
    let published_runtime = primary.binding().backend_ref.clone();
    let model_override = Some(awaken_session_contract::SessionModelOverride {
        publication: Some(Box::new(awaken_session_contract::SessionModelPublication {
            primary,
            candidates: publication
                .as_ref()
                .map(|publication| publication.resolved_spec.model_candidates.clone())
                .unwrap_or_default(),
        })),
        inference: Default::default(),
    });
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment,
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
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
    let previous = awaken_session_contract::SessionResourceManifest::at_revision(
        workspace_id.clone(),
        0,
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    awaken_session_contract::SessionRuntime::install_session_projection(
        managed,
        thread,
        awaken_session_contract::FrozenSessionProjection {
            workspace_id,
            revision: awaken_session_contract::SessionRevision(1),
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
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
}

#[async_trait::async_trait]
trait CompleteTestSessionFixture {
    async fn install_complete_test_session(
        &self,
        thread: &str,
        init: awaken_session_contract::SessionInit,
    ) -> Result<(), awaken_session_contract::RunError>;
}

#[async_trait::async_trait]
impl CompleteTestSessionFixture for crate::ManagedHost {
    async fn install_complete_test_session(
        &self,
        thread: &str,
        init: awaken_session_contract::SessionInit,
    ) -> Result<(), awaken_session_contract::RunError> {
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
    init: awaken_session_contract::SessionInit,
) -> Result<(), awaken_session_contract::RunError> {
    managed.install_environment_binding_sink(Arc::new(BindingOrderSink {
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
    managed.host.install_session_realization_lease(
        thread,
        awaken_session_contract::SessionRealizationLease {
            owner: "runtime-host-test".into(),
            runtime_incarnation: format!("runtime-host-test-{thread}"),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        },
    );
    Ok(())
}

fn memory_test_snapshot(agent: &str) -> awaken_runtime_contract::ExecutableAgentSnapshot {
    crate::config::server_config(
        agent,
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            serde_json::json!({"binding_id": "test-input-0"}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    )
}

fn memory_test_host(model: Arc<dyn LlmExecutor>, agent: &str) -> Arc<SharedHost> {
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([memory_test_snapshot(
            agent,
        )])
        .expect("valid Memory Agent publication");
    Arc::new(SharedHost::new(model, "stub").with_agent_publications(Arc::new(publications)))
}

/// Install one complete Memory-enabled Session through the immutable Agent and
/// exact Resource-transition authorities. C1 the Host publication source owns
/// the Agent selecting binding `test-input-0`; C2 the Session pins that one
/// writable store. C1+C2 produces one automatic Memory binding used by recall
/// and terminal extraction, without a second publication or slot-write path.
async fn install_complete_memory_test_session(
    managed: &crate::ManagedHost,
    thread: &str,
    agent: &str,
    store: &str,
) {
    let resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store.into(),
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let mut init = bare_session(agent, managed.host.local_workspace());
    init.resources = resources.clone();
    install_complete_test_session(managed, thread, init)
        .await
        .expect("install complete Memory Session projection");
    awaken_session_contract::SessionRuntime::apply_session_inputs(
        managed,
        thread,
        &resource_transition(
            managed.host.local_workspace(),
            0,
            Default::default(),
            0,
            resources,
        ),
    )
    .await
    .expect("compile exact Memory Resource transition");
}

/// Wait for the Provider-side gate or fail immediately if setup terminates the
/// Run first. This makes fixture drift observable instead of turning a missing
/// precondition into an unbounded `Notify` wait.
async fn await_interrupt_inference_gate<T>(
    reached: &tokio::sync::Notify,
    task: &mut tokio::task::JoinHandle<Result<T, HostError>>,
) {
    tokio::select! {
        () = reached.notified() => {}
        result = task => match result {
            Ok(Ok(_)) => panic!("Run completed before reaching the inference gate"),
            Ok(Err(error)) => panic!("Run failed before reaching the inference gate: {error}"),
            Err(error) => panic!("Run task failed before reaching the inference gate: {error}"),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_cancels_the_run_and_reports_interrupted() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = Arc::new(SharedHost::new(model, "scripted"));
    let _managed = install_test_dispatch_runtime(&host);

    // Cause/effect rule: C0 the canonical Dispatch runtime is installed; C1 an
    // Outcome receives needs_revision; C2 its next Worker Run reaches blocked
    // inference; C3 interrupt is accepted. Effects: E0 fixture setup cannot
    // masquerade as a gate hang; E1 the prior Grade remains needs_revision; E2
    // the active cycle ends interrupted. R1=C0+C1+C2+C3=>E0+E1+E2.
    let driver = host.clone();
    let mut task =
        tokio::spawn(async move { driver.define_outcome("t1", "finish", "FINAL", 5).await });

    // Once the loop is blocked mid-run, interrupt it, then release the gate.
    await_interrupt_inference_gate(&reached, &mut task).await;
    host.interrupt("t1").await.expect("interrupt");
    gate.notify_one();

    let report = completed_outcome(task.await.expect("join").expect("define_outcome"));
    // Round 1 graded needs_revision; the interrupt ended the run before the
    // second round could conclude, so the outcome reports interrupted.
    assert_eq!(report.iterations[0].result, "needs_revision");
    assert_eq!(
        report.iterations.last().expect("a round").result,
        "interrupted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_acknowledgment_replaces_the_budget_terminal() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C0 the canonical Dispatch runtime is installed; C1
    // max_iterations=1 and a needs_revision Grade enter the stable
    // acknowledgment Run; C2 that Run owns the active cancellation slot; C3
    // user.interrupt lands while its model request is blocked. Effects: E0
    // fixture setup cannot masquerade as a gate hang; E1 the existing Host
    // cancellation path ends the Outcome as interrupted; E2 the one graded
    // cycle remains iteration 0; E3 its public terminal is interrupted, with
    // neither max_iterations_reached nor an invented cycle 1.
    //
    // | Rule | Runtime | At cap | Ack active | Interrupt | Effects |
    // | R1 | installed | yes | yes | yes | E0 + iteration 0 interrupted |
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = Arc::new(SharedHost::new(model, "scripted"));
    let _managed = install_test_dispatch_runtime(&host);

    let driver = host.clone();
    let mut task = tokio::spawn(async move {
        driver
            .define_outcome("ack-interrupt", "finish", "FINAL", 1)
            .await
    });

    await_interrupt_inference_gate(&reached, &mut task).await;
    host.interrupt("ack-interrupt").await.expect("interrupt");
    gate.notify_one();

    let report = completed_outcome(task.await.expect("join").expect("define_outcome"));
    assert_eq!(report.iterations.len(), 1, "R1/E2-E3");
    assert_eq!(report.iterations[0].iteration, 0, "R1/E2");
    assert_eq!(report.iterations[0].result, "interrupted", "R1/E1+E3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_authority_loss_interrupts_active_session_before_revocation() {
    // Cause/effect rule: C0 the canonical Dispatch runtime is installed; C1 an
    // Outcome owns active Worker and Judge Session projections; C2 its next
    // Worker inference is blocked; C3 worker authority is lost. Effects: E0
    // setup errors fail before the gate wait; E1 both active projections are
    // interrupted before E2 both realizations are revoked.
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = Arc::new(SharedHost::new(model, "scripted"));
    let _managed = install_test_dispatch_runtime(&host);

    let driver = host.clone();
    let mut task = tokio::spawn(async move {
        driver
            .define_outcome("authority-loss-active", "finish", "FINAL", 5)
            .await
    });

    await_interrupt_inference_gate(&reached, &mut task).await;
    // The outcome owns both its Worker Session and its Judge Session; authority
    // loss must fence every process-local projection, not only the caller thread.
    assert_eq!(host.interrupt_all_session_runs().await, 2);
    gate.notify_one();

    let report = completed_outcome(task.await.expect("join").expect("define_outcome"));
    assert_eq!(
        report.iterations.last().expect("a round").result,
        "interrupted"
    );
    assert_eq!(host.revoke_all_session_realizations().await.unwrap(), 2);
}

#[tokio::test]
async fn interrupt_is_a_noop_when_nothing_runs() {
    let host = SharedHost::new(
        Arc::new(GatedModel {
            gate: Arc::new(tokio::sync::Notify::new()),
            reached: Arc::new(tokio::sync::Notify::new()),
            calls: AtomicUsize::new(0),
        }),
        "scripted",
    );
    // No run in flight → interrupt succeeds and does nothing.
    host.interrupt("idle-thread")
        .await
        .expect("interrupt is a no-op");
}

#[tokio::test]
async fn runtime_constructor_installs_the_exact_file_content_source() {
    struct ConstructorFileSource;

    #[async_trait::async_trait]
    impl crate::FileContentSource<awaken_run_ingress::RunClaim> for ConstructorFileSource {
        async fn read(
            &self,
            workspace_id: &str,
            file_id: &str,
            _purpose: &awaken_resource_contract::FileReadPurpose,
            _claim: Option<&awaken_run_ingress::RunClaim>,
        ) -> Result<
            Option<awaken_resource_contract::ResolvedFileContent>,
            awaken_resource_contract::FileContentSourceError,
        > {
            let bytes = format!("{workspace_id}/{file_id}").into_bytes();
            Ok(Some(awaken_resource_contract::ResolvedFileContent {
                file_id: file_id.into(),
                content_id: awaken_resource_contract::content_id(&bytes),
                filename: "input.txt".into(),
                media_type: "text/plain".into(),
                bytes,
            }))
        }
    }

    // Cause/effect decision table: R1 production Runtime constructor + exact
    // File content port -> that same port serves immutable bytes; R2 no
    // post-construction override -> the default-feature CLI remains compilable
    // without exposing the volatile fixture mutator. Unavailable-source failure
    // semantics are owned by awaken-resource-contract and tested there.
    let host = SharedHost::new_with_runtime_resources_and_deployment(
        Arc::new(MemoryHostModel),
        "stub",
        Arc::new(ConstructorFileSource),
        Arc::new(
            awaken_memory_store::SqliteMemoryRepository::open(":memory:")
                .expect("open test Memory repository"),
        ),
        SharedHost::test_memory_extraction_repository(None),
        crate::DeploymentConfig::ephemeral(),
    );
    let resolved = host
        .worker_file_content_source()
        .read(
            "workspace-runtime",
            "file-runtime",
            &awaken_resource_contract::FileReadPurpose::SessionResource,
            None,
        )
        .await
        .expect("read through exact constructor port")
        .expect("constructor source returns one File");

    assert_eq!(resolved.bytes, b"workspace-runtime/file-runtime");
}

#[tokio::test]
async fn attributed_run_meets_deployment_capture_with_control_consent() {
    struct SubjectConsent;
    #[derive(Default)]
    struct CountingCaptureSink(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_runtime_contract::DataSubjectConsentSource for SubjectConsent {
        async fn consent_ceiling(
            &self,
            subject: &awaken_runtime_contract::DataSubjectId,
            _purpose: awaken_runtime_contract::Purpose,
        ) -> awaken_runtime_contract::ContentCapture {
            if subject.as_str() == "granted" {
                awaken_runtime_contract::ContentCapture::Full
            } else {
                awaken_runtime_contract::ContentCapture::Structured
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_runtime_contract::CaptureSink for CountingCaptureSink {
        async fn record(
            &self,
            _subject: &awaken_runtime_contract::DataSubjectId,
            _purpose: awaken_runtime_contract::Purpose,
            _kind: awaken_runtime_contract::ContentKind,
            _content: &str,
        ) -> Result<(), awaken_runtime_contract::CaptureError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // Cause/effect decision table: deployment Full + granted subject -> prompt
    // and completion are persisted; deployment Full + absent/withdrawn subject
    // -> no content row. The single attempt-executor decorator owns this meet for
    // both direct and durable delivery; this unit partition proves the direct
    // route and management_capture_erasure_loop_e2e proves durable delivery.
    let mut deployment = crate::DeploymentConfig::ephemeral();
    deployment.content_capture.level = awaken_runtime_contract::ContentCapture::Full;
    let captured = Arc::new(CountingCaptureSink::default());
    let (host, _managed) = dispatch_test_host(
        SharedHost::new_with_deployment(Arc::new(MemoryHostModel), "stub", deployment)
            .with_capture_sink(captured.clone())
            .with_data_subject_consent_source(Arc::new(SubjectConsent)),
    );

    host.run_attributed(
        None,
        "consent-granted",
        user("hello granted"),
        Some(awaken_runtime_contract::DataSubjectId("granted".into())),
    )
    .await
    .expect("R1 attributed run");
    let after_granted = captured.0.load(Ordering::SeqCst);
    assert!(after_granted >= 2, "R1 prompt and completion captured");

    host.run_attributed(
        None,
        "consent-unknown",
        user("hello unknown"),
        Some(awaken_runtime_contract::DataSubjectId("unknown".into())),
    )
    .await
    .expect("R2 attributed run");
    assert_eq!(
        captured.0.load(Ordering::SeqCst),
        after_granted,
        "R2 consent clamps capture"
    );
}

#[tokio::test]
async fn bound_executor_is_the_ordinary_run_execution_boundary() {
    use crate::run_exec::BoundRunExecutor;
    use awaken_runtime_contract::execution::RunExecutor;

    let (host, _managed) = dispatch_test_host(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
    let ctx = host.ctx_for("snapshot-run", None).await.expect("context");
    let before = ctx.commit.committed_messages(&ctx.thread_id).len();
    let activation = RunActivation::new(
        RunId("snapshot-run-1".into()),
        ctx.thread_id.clone(),
        ctx.config.clone(),
        user("hello"),
    );
    let state = BoundRunExecutor::new(&host, ctx.clone())
        .execute(activation, RuntimeRunContext::new())
        .await
        .expect("snapshot run");

    assert!(matches!(state, RunState::Ended(EndCause::NaturalEnd)));
    let messages = ctx.commit.committed_messages(&ctx.thread_id);
    assert_eq!(before, 0);
    assert_eq!(messages.len(), 2, "user + assistant delta");
    assert_eq!(messages.last().unwrap().text_content(), "ok");
}

#[tokio::test]
async fn host_application_decorator_wraps_the_complete_session_boundary() {
    use crate::run_exec::BoundRunExecutor;
    use awaken_runtime_contract::execution::{
        Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
    };
    use awaken_runtime_contract::resume::ResumeCommand;
    use std::sync::atomic::Ordering;

    struct ObservingAttemptExecutor {
        calls: Arc<AtomicUsize>,
        inner: Arc<dyn RunAttemptExecutor>,
    }

    #[async_trait::async_trait]
    impl RunExecutor for ObservingAttemptExecutor {
        async fn execute(
            &self,
            activation: RunActivation,
            context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.execute(activation, context).await
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for ObservingAttemptExecutor {
        async fn resume(
            &self,
            activation: RunActivation,
            command: ResumeCommand,
            context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.resume(activation, command, context).await
        }

        async fn cancel(
            &self,
            activation: RunActivation,
            context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.inner.cancel(activation, context).await
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let decorator_calls = calls.clone();
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_attempt_decorator(Arc::new(
            move |inner| {
                Arc::new(ObservingAttemptExecutor {
                    calls: decorator_calls.clone(),
                    inner,
                })
            },
        )),
    );
    let ctx = host.ctx_for("injected-attempt", None).await.unwrap();
    let activation = RunActivation::new(
        RunId("injected-attempt-run".into()),
        ctx.thread_id.clone(),
        ctx.config.clone(),
        user("go"),
    );
    let state = BoundRunExecutor::new(&host, ctx)
        .execute(activation, RuntimeRunContext::new())
        .await
        .unwrap();

    assert_eq!(state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn control_frozen_baseline_is_the_only_worker_runtime_projection() {
    use awaken_provisioning_contract::{
        EnvValue, EnvVar, EnvVisibility, MountAccess, MountLifetime, MountRequirement, MountSource,
    };

    fn projection(
        prompt: &str,
        with_environment_inputs: bool,
    ) -> awaken_session_contract::FrozenSessionProjection {
        let mount = MountRequirement {
            mount_id: "flow-workspace".into(),
            source: MountSource::Inline {
                contents: "project".into(),
            },
            mount_path: "/workspace/project.txt".into(),
            access: MountAccess::ReadOnly,
            lifetime: MountLifetime::PerRun,
            required: true,
        };
        let env = EnvVar {
            name: "FLOW_PROJECT".into(),
            value: EnvValue::Inline {
                value: "project-a".into(),
            },
            visibility: EnvVisibility::Process,
        };
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            "test.worker",
        );
        let mounts = with_environment_inputs
            .then_some(mount)
            .into_iter()
            .collect();
        let env = with_environment_inputs.then_some(env).into_iter().collect();
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: if with_environment_inputs {
                        awaken_session_contract::SessionNetworkPolicy::None
                    } else {
                        awaken_session_contract::SessionNetworkPolicy::Unrestricted
                    },
                    credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                        inference_holder: holder.clone(),
                        mcp_holder: holder.clone(),
                        resource_holder: holder,
                    },
                },
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                mcp_authoring: Default::default(),
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                model: "model".into(),
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts,
                env,
                prompts: vec![prompt.into()],
                transcript_prefix: None,
            },
        );
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_contract::SessionRevision(2),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 7,
            resources: awaken_session_contract::ResolvedSessionResources::default(),
            previous_resource_manifest: Some(
                awaken_session_contract::SessionResourceManifest::at_revision(
                    "workspace",
                    7,
                    awaken_session_contract::ResolvedSessionResources::default(),
                ),
            ),
            mcp: Vec::new(),
            tools: Default::default(),
            request_context: Vec::new(),
        }
    }

    #[derive(Clone, Default)]
    struct PromptRecorder(Arc<Mutex<Vec<ChatRequest>>>);

    #[derive(Default)]
    struct RepositoryClaimRecorder(Mutex<Vec<Option<awaken_run_ingress::RunClaim>>>);

    struct FailingRepositoryVerifier;

    #[async_trait::async_trait]
    impl crate::RepositoryBindingVerifier<awaken_run_ingress::RunClaim> for RepositoryClaimRecorder {
        async fn verify(
            &self,
            _workspace_id: &str,
            _repository_id: &str,
            _config_version: awaken_resource_contract::ConfigVersion,
            claim: Option<&awaken_run_ingress::RunClaim>,
        ) -> Result<
            awaken_resource_contract::RepositoryTransport,
            awaken_resource_contract::RepositoryBindingVerifierError,
        > {
            self.0.lock().unwrap().push(claim.cloned());
            Ok(awaken_resource_contract::RepositoryTransport::Direct)
        }
    }

    #[async_trait::async_trait]
    impl crate::RepositoryBindingVerifier<awaken_run_ingress::RunClaim> for FailingRepositoryVerifier {
        async fn verify(
            &self,
            _workspace_id: &str,
            _repository_id: &str,
            _config_version: awaken_resource_contract::ConfigVersion,
            _claim: Option<&awaken_run_ingress::RunClaim>,
        ) -> Result<
            awaken_resource_contract::RepositoryTransport,
            awaken_resource_contract::RepositoryBindingVerifierError,
        > {
            Err(
                awaken_resource_contract::RepositoryBindingVerifierError::new(
                    "injected binding failure",
                ),
            )
        }
    }

    #[async_trait::async_trait]
    impl LlmExecutor for PromptRecorder {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.0.lock().unwrap().push(request);
            Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    // Test-runtime stack cause/effect decision table. C0 the ordinary libtest
    // thread uses its default stack; C1 every independent projection scenario
    // lives in one outer async state machine; C2 the same scenarios execute in
    // their original order as bounded inner futures; C3 the primary Host,
    // Managed adapter, recorder, and claim recorder remain shared. Effects: E0
    // C0+C1 overflows before the behavioral assertions can finish; E1 C0+C2+C3
    // retains every assertion and state dependency while bounding live future
    // state to one scenario. The larger-stack run is diagnostic evidence only,
    // never a test or product runtime requirement.
    //
    // | Rule | Default stack | Segmented | Shared primary state | Effect |
    // | S1 | yes | no  | yes | E0 (red baseline) |
    // | S2 | yes | yes | yes | E1 (all rules complete) |
    async {
        // Co-located Native baseline installation has the same immutable binding
        // rules as the claimed Worker projection. Decision table:
        // B1 valid first install without an early admission marker -> accept
        // and classify from the Control-frozen baseline; B2 identical replay ->
        // idempotent; B3 empty fingerprint -> reject; B4 different fingerprint
        // -> reject; B5 Environment already realized -> reject rather than run
        // without the frozen mounts/env/prompts. Effects: E1 B1 excludes the
        // direct live-authored Skill source on a cold Worker; E2 B2 preserves
        // the same projection; E3 B3-B5 publish no conflicting projection.
        let baseline_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
        let _baseline_runtime =
            crate::ManagedHost::new(baseline_host.clone()).install_dispatch_session_runtime();
        let first_projection = projection("baseline-a", true);
        baseline_host
            .install_frozen_session_projection(
                "local-baseline",
                first_projection.clone(),
                None,
                true,
                None,
            )
            .await
            .expect("B1 valid baseline installs");
        assert!(
            baseline_host
                .session_slots
                .read("local-baseline", |slot| {
                    slot.baseline.is_some() && slot.session_dispatch
                })
                .unwrap_or(false),
            "B1/E1 frozen baseline and Managed execution marker publish atomically on a cold Worker"
        );
        assert_eq!(
            baseline_host.session_skill_source_roots(
                "local-baseline",
                None,
                crate::skills::MANAGED_SKILLS_SUBDIR,
            ),
            None,
            "B1/E1 no direct live-authored Skill source is registered"
        );
        baseline_host
            .install_frozen_session_projection(
                "local-baseline",
                first_projection.clone(),
                None,
                true,
                None,
            )
            .await
            .expect("B2 same baseline is idempotent");

        let mut empty = first_projection.clone();
        empty.baseline.fingerprint.0.clear();
        assert!(
            baseline_host
                .install_frozen_session_projection("empty-baseline", empty, None, true, None)
                .await
                .unwrap_err()
                .message
                .contains("fingerprint must not be empty"),
            "B3"
        );
        let conflicting = projection("baseline-b", true);
        assert!(
            baseline_host
                .install_frozen_session_projection("local-baseline", conflicting, None, true, None,)
                .await
                .unwrap_err()
                .message
                .contains("different frozen Session baseline"),
            "B4"
        );
        baseline_host
            .ctx_for("realized-before-baseline", None)
            .await
            .expect("realize the negative-case Environment");
        assert!(
            baseline_host
                .install_frozen_session_projection(
                    "realized-before-baseline",
                    first_projection,
                    None,
                    true,
                    None,
                )
                .await
                .unwrap_err()
                .message
                .contains("realized before its frozen Session baseline"),
            "B5"
        );
    }
    .await;

    async {
        // Fault-injection rule B6: any fallible Resource verification fails before
        // publishing the complete logical projection. No baseline, workspace,
        // Agent, manifest, prompt, or lease fragment may survive independently. An
        // empty process-local coordination slot is not Session truth and may remain
        // to serialize a concurrent retry.
        let failed_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
        let _failed_runtime = crate::ManagedHost::new(failed_host.clone())
            .with_repository_binding_verifier(Arc::new(FailingRepositoryVerifier))
            .install_dispatch_session_runtime();
        let mut rejected = projection("must not publish", false);
        rejected.resources = effective_repository(
            "rejected-repository",
            "https://example.invalid/rejected.git",
            "/workspace/rejected",
            None,
        );
        assert!(
            failed_host
                .install_frozen_session_projection("failed-projection", rejected, None, true, None)
                .await
                .unwrap_err()
                .message
                .contains("injected binding failure"),
            "B6 injected fault"
        );
        assert!(
            failed_host
                .session_slots
                .read("failed-projection", |slot| {
                    slot.baseline.is_none()
                        && slot.workspace.is_none()
                        && slot.agent_id.is_none()
                        && slot.manifest.is_none()
                        && slot.resources.mounts.is_empty()
                        && slot.resources.prompts.is_empty()
                        && slot.request_context.is_empty()
                        && slot.realization_lease.is_none()
                        && !slot.session_dispatch
                })
                .unwrap_or(true),
            "B6 failed preparation cannot publish partial Session truth"
        );
    }
    .await;

    let recorder = PromptRecorder::default();
    let observed = recorder.0.clone();
    let host = Arc::new(SharedHost::new(Arc::new(recorder), "stub"));
    install_test_session_application(&host);
    let repository_claims = Arc::new(RepositoryClaimRecorder::default());
    let managed = crate::ManagedHost::new(host.clone())
        .with_repository_binding_verifier(repository_claims.clone())
        .install_dispatch_session_runtime();

    async {
        // Complete-install and unattempted Resource-amendment cause/effect table.
        // C0 the application calls the sole complete-projection port in Dispatch
        // mode; C1 a complete frozen dispatch projection already names generation
        // 7; C2 the Session aggregate
        // amends its still-unattempted desired content at generation 7; C3 the
        // caller is the Coordinator Dispatch installer or a claimed Worker.
        // Effects: E0 the one call publishes baseline, the exact staged Resource
        // transition, and the Managed execution marker together (there is no
        // second SessionInit port), while active completion remains absent; E1 the
        // Coordinator replaces its disposable transition and preserves generation
        // 7; E2 Worker rejects the same content change and leaves the old staged
        // transition intact. A newer generation and exact replay remain covered by
        // the projection-owner decision table.
        //
        // | Rule | C0 | C1 | C2 | C3 | Effect |
        // | A0 | T | F | F | Coordinator Dispatch | E0 |
        // | A1 | T | T | T | Worker claim | E2 |
        // | A2 | T | T | T | Coordinator Dispatch | E1 |
        let amendment_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
        let amendment_managed = crate::ManagedHost::new(amendment_host.clone())
            .with_repository_binding_verifier(Arc::new(RepositoryClaimRecorder::default()))
            .install_dispatch_session_runtime();
        let initial_dispatch = projection("authority amendment", false);
        awaken_session_contract::SessionRuntime::install_session_projection(
            &amendment_managed,
            "authority-amendment",
            initial_dispatch.clone(),
            awaken_session_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .expect("A1 initial dispatch projection");
        assert!(
            amendment_host
                .session_slots
                .read("authority-amendment", |slot| {
                    slot.baseline.is_some()
                        && slot.session_dispatch
                        && slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired().revision == initial_dispatch.resource_revision
                                && transition.desired().resources == initial_dispatch.resources
                        })
                })
                .unwrap_or(false),
            "A0/E0 complete projection stages one transition without forging physical completion"
        );
        let mut amended_dispatch = initial_dispatch;
        amended_dispatch.resources = effective_repository(
            "authority-amendment-repository",
            "https://example.invalid/authority-amendment.git",
            "/workspace/authority-amendment",
            None,
        );
        let amendment_claim = awaken_run_ingress::RunClaim {
            run_id: RunId("authority-amendment-run".into()),
            owner: "claimed-worker".into(),
            epoch: 1,
        };
        assert!(
            amendment_host
                .install_frozen_session_projection(
                    "authority-amendment",
                    amended_dispatch.clone(),
                    Some(&amendment_claim),
                    true,
                    None,
                )
                .await
                .unwrap_err()
                .message
                .contains("cannot replace its current Session Resource projection"),
            "A1/E2 claimed Worker remains fenced"
        );
        assert!(
            amendment_host
                .session_slots
                .read("authority-amendment", |slot| {
                    slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired().resources
                                == awaken_session_contract::ResolvedSessionResources::default()
                        })
                })
                .unwrap_or(false),
            "A1/E2 rejection is side-effect free"
        );
        awaken_session_contract::SessionRuntime::install_session_projection(
            &amendment_managed,
            "authority-amendment",
            amended_dispatch.clone(),
            awaken_session_contract::SessionProjectionInstallMode::Dispatch,
        )
        .await
        .expect("A2 authority amendment");
        assert!(
            amendment_host
                .session_slots
                .read("authority-amendment", |slot| {
                    slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired()
                                == &awaken_session_contract::SessionResourceManifest::at_revision(
                                    amended_dispatch.workspace_id.clone(),
                                    amended_dispatch.resource_revision,
                                    amended_dispatch.resources.clone(),
                                )
                        })
                })
                .unwrap_or(false),
            "A2/E1 exact same-generation content replaces only the staged projection"
        );
    }
    .await;

    async {
        // Prospective-layout decision rule: F1 a cold projection's Repository is
        // nested below a mount in the same not-yet-resident frozen baseline. F1 must
        // fail before the baseline, Environment, Resource manifest, Skill loader, or
        // Repository verifier is touched. Reading only the current slot would miss
        // this first-install combination.
        let mut conflicting = projection("prospective conflict", true);
        conflicting.resources = effective_resources(vec![TestInput {
            kind: "github_repository".into(),
            id: "https://github.com/awaken/prospective-conflict.git".into(),
            mount_path: "/workspace/project.txt/repository".into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
            initial_branch: None,
            initial_commit: None,
        }]);
        let error = host
            .install_frozen_session_projection(
                "prospective-layout-conflict",
                conflicting,
                None,
                true,
                None,
            )
            .await
            .expect_err("F1 rejects the complete prospective layout");
        assert!(error.message.contains("overlaps"), "F1: {error:?}");
        assert!(
            host.thread_resource_manifest("prospective-layout-conflict")
                .is_none(),
            "F1 no Resource projection"
        );
        assert!(
            host.session_slots
                .read("prospective-layout-conflict", |slot| {
                    slot.baseline.is_none() && slot.environment_projection.is_none()
                })
                .unwrap_or(true),
            "F1 no partial frozen projection"
        );
        assert!(
            repository_claims.0.lock().unwrap().is_empty(),
            "F1 no Repository verifier effect"
        );
    }
    .await;

    async {
        let frozen = projection("Use the bound Flow project.", true);
        host.install_frozen_session_projection("flow-thread", frozen.clone(), None, true, None)
            .await
            .expect("first frozen projection installs");
        host.install_frozen_session_projection("flow-thread", frozen, None, true, None)
            .await
            .expect("same frozen fingerprint is idempotent");
    }
    .await;

    // Branch request-context cause/effect graph: C1 the frozen baseline is
    // already resident; C2 its immutable transcript prefix is materialized only
    // on the later claim-fenced projection; C3 an identical projection replays;
    // C4 the branch-only binding sink persists and returns one exact Store-read
    // Resident before any later projection. E1 C1+C2+C4 invalidates only the
    // stale SessionCtx and rebuilds with the exact request-only messages; E2
    // C1+C2+C3+C4 retains that rebuilt context; E3 C4 persists exactly once and
    // every later projection carries that returned Resident. FMECA: updating
    // the slot directly leaves a resident ACP/Native attempt context stale,
    // while replaying an Unmaterialized aggregate over its live owner violates
    // the durable binding fence.
    //
    // | Rule | C1 | C2 changed | C3 replay | Effect |
    // | B1   | T  | T          | F         | E1+E3  |
    // | B2   | T  | F          | T         | E2+E3  |
    let branch_host = Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
    install_test_session_application(&branch_host);
    let committed_environment = Arc::new(Mutex::new(None));
    let branch_sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&branch_host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: Some("branch-thread".into()),
        committed_environment: Some(committed_environment.clone()),
    });
    let branch_managed =
        crate::ManagedHost::new(branch_host.clone()).install_dispatch_session_runtime();
    branch_managed.install_environment_binding_sink(branch_sink.clone());
    let mut branch = projection("branch prompt", false);
    branch_host
        .install_frozen_session_projection("branch-thread", branch.clone(), None, true, None)
        .await
        .expect("B1 baseline without materialized prefix");
    let stale = branch_host
        .ctx_for("branch-thread", None)
        .await
        .expect("B1 resident context");
    branch.environment = committed_environment
        .lock()
        .unwrap()
        .clone()
        .expect("B1 Store-read Resident returned by the binding sink");
    assert_eq!(branch_sink.calls.load(Ordering::SeqCst), 1, "B1/E3");
    branch.request_context = vec![Message::text(
        MessageId("source-prefix".into()),
        Role::Assistant,
        "E2E_SOURCE_ONLY_exact",
    )];
    branch_host
        .install_frozen_session_projection("branch-thread", branch.clone(), None, true, None)
        .await
        .expect("B1 late materialized prefix");
    let rebuilt = branch_host
        .ctx_for("branch-thread", None)
        .await
        .expect("B1 rebuilt context");
    assert!(
        !Arc::ptr_eq(&stale, &rebuilt),
        "B1/E1 stale context retired"
    );
    assert_eq!(
        rebuilt.attempt_context.request_context, branch.request_context,
        "B1/E1 exact request-only prefix"
    );
    branch_host
        .install_frozen_session_projection("branch-thread", branch, None, true, None)
        .await
        .expect("B2 identical replay");
    let replayed = branch_host
        .ctx_for("branch-thread", None)
        .await
        .expect("B2 resident context");
    assert!(
        Arc::ptr_eq(&rebuilt, &replayed),
        "B2/E2 no redundant rebuild"
    );
    assert_eq!(branch_sink.calls.load(Ordering::SeqCst), 1, "B2/E3");

    {
        // Frozen-projection Resource-generation cause/effect decision table.
        // C1=projection has an explicit non-legacy Resource generation;
        // C2=resources are non-default and must be staged before Environment
        // creation. E1=the Runtime's exact transition preserves that generation;
        // E2=active completion stays absent until the physical transition; E3=the
        // staged sandbox requirements remain available. R1 C1+C2=>E1,E2,E3.
        assert!(
            host.session_slots
                .read("flow-thread", |slot| {
                    slot.manifest.is_none()
                        && slot.resource_transition.as_ref().is_some_and(|transition| {
                            transition.desired().revision == 7
                                && transition.desired().resources
                                    == awaken_session_contract::ResolvedSessionResources::default()
                        })
                })
                .unwrap_or(false),
            "R1 stages the exact SessionResourceState generation without forging completion"
        );

        let spec = host.sandbox_spec("flow-thread");
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.env.len(), 1);
        assert_eq!(
            spec.network,
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            "Workdir does not advertise strict network isolation"
        );
        assert!(
            spec.deny_tool_egress,
            "the Workdir tool wrapper retains the frozen deny intent"
        );
        assert_eq!(
            host.thread_session_prompts("flow-thread"),
            vec!["Use the bound Flow project."]
        );
    }

    async {
        // Cause graph: C1 frozen Session prompt; C2 current attempt context absent;
        // C3 activation already carries the exact explicit System message. Effects:
        // E1 project exactly one request-only context message; E2 leave Thread input
        // unchanged; E3 deduplicate equal explicit input without deleting it.
        //
        // | Rule | C1 | C2 | C3 | E |
        // | P1   | 0  | *  | *  | 0 |
        // | P2   | 1  | 1  | 0  | 1 |
        // | P3   | 1  | 1  | 1  | 0 (deduplicate) |
        // | P4   | 1  | later Run | 0 | E1 again, never from history |
        host.run(None, "no-baseline", user("P1")).await.expect("P1");
        host.install_frozen_session_projection(
            "prompt-thread",
            projection("Use the bound Flow project.", false),
            None,
            true,
            None,
        )
        .await
        .expect("P2/P4 projection");
        host.run_thread_extension_after_admission(None, "prompt-thread", user("P2"))
            .await
            .expect("P2");
        host.run_thread_extension_after_admission(None, "prompt-thread", user("P4"))
            .await
            .expect("P4");
        host.install_frozen_session_projection(
            "deduplicated",
            projection("exact prompt", false),
            None,
            true,
            None,
        )
        .await
        .expect("P3 projection");
        host.run_thread_extension_after_admission(
            None,
            "deduplicated",
            vec![
                Message::text(
                    MessageId("system-existing".into()),
                    Role::System,
                    "exact prompt",
                ),
                Message::text(MessageId("user-existing".into()), Role::User, "P3"),
            ],
        )
        .await
        .expect("P3");
        {
            let requests = observed.lock().unwrap();
            let prompt_count =
                |request: &ChatRequest, prompt: &str| {
                    request
                .messages
                .iter()
                .filter(|message| message.role == Role::System)
                .flat_map(|message| message.content.iter())
                .filter(|content| matches!(content, ContentBlock::Text { text } if text == prompt))
                .count()
                };
            assert_eq!(
                prompt_count(&requests[0], "Use the bound Flow project."),
                0,
                "P1"
            );
            assert_eq!(
                prompt_count(&requests[1], "Use the bound Flow project."),
                1,
                "P2"
            );
            assert_eq!(
                prompt_count(&requests[2], "Use the bound Flow project."),
                1,
                "P4 fresh request context is projected exactly once"
            );
            assert_eq!(prompt_count(&requests[3], "exact prompt"), 1, "P3");
        }
        assert!(
            managed
                .committed_messages("prompt-thread")
                .await
                .expect("prompt Thread truth")
                .iter()
                .all(|message| message.text_content() != "Use the bound Flow project."),
            "E2 derived Session context must never enter a Thread delta"
        );
    }
    .await;

    async {
        // Request-context decision rule C4: a frozen projection carries one
        // materialized source prefix. Effect E4: the model sees it before current
        // input while the target's committed Thread contains neither a copied source
        // message nor any second transcript authority.
        let mut contextual = projection("", false);
        contextual.request_context = vec![Message::text(
            MessageId("source-prefix".into()),
            Role::User,
            "prior branch context",
        )];
        host.install_frozen_session_projection("context-thread", contextual, None, true, None)
            .await
            .expect("C4 projection");
        host.run_thread_extension_after_admission(
            None,
            "context-thread",
            user("current branch input"),
        )
        .await
        .expect("C4 run");
        let request = observed
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("C4 request");
        let user_text = request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            user_text,
            ["prior branch context", "current branch input"],
            "C4/E4"
        );
        assert!(
            managed
                .committed_messages("context-thread")
                .await
                .expect("C4 committed truth")
                .iter()
                .all(|message| message.id.0 != "source-prefix"),
            "C4/E4 request context is not copied into target truth"
        );
    }
    .await;

    async {
        let replacement = projection("different", true);
        assert!(
            host.install_frozen_session_projection("flow-thread", replacement, None, true, None)
                .await
                .is_err(),
            "a bound Session cannot switch frozen baselines"
        );
    }
    .await;

    async {
        // Frozen-config / live-claim cause/effect decision table:
        // | Rule | Baseline/config | Claim epoch | Effect |
        // |---|---|---|---|
        // | C1 | first exact projection | 1 | install and verify with epoch 1 |
        // | C2 | exact projection replay | 2 | retain config, reverify with epoch 2 |
        // | C3 | different baseline | any | reject before replacing config |
        let mut repository_projection = projection("repository claim", false);
        repository_projection.resources = effective_repository(
            "repository-claim",
            "https://example.invalid/repository.git",
            "/workspace/repository",
            None,
        );
        let claim = |epoch| awaken_run_ingress::RunClaim {
            run_id: RunId("repository-claim-run".into()),
            owner: "repository-worker".into(),
            epoch,
        };
        host.install_frozen_session_projection(
            "repository-claim-thread",
            repository_projection.clone(),
            Some(&claim(1)),
            true,
            None,
        )
        .await
        .expect("C1 first claim");
        host.install_frozen_session_projection(
            "repository-claim-thread",
            repository_projection,
            Some(&claim(2)),
            true,
            None,
        )
        .await
        .expect("C2 replacement claim");
        assert_eq!(
            repository_claims
                .0
                .lock()
                .unwrap()
                .iter()
                .map(|claim| claim.as_ref().map(|claim| claim.epoch))
                .collect::<Vec<_>>(),
            vec![Some(1), Some(2)],
            "C2 must not retain the stale claim from C1"
        );
    }
    .await;
}

#[tokio::test]
async fn tool_bearing_snapshot_can_be_restricted_at_the_run_boundary() {
    use crate::run_exec::BoundRunExecutor;
    use awaken_runtime_contract::execution::RunExecutor;
    use awaken_runtime_contract::resolved::ToolDescriptor;

    let (host, _managed) = dispatch_test_host(SharedHost::new(Arc::new(MemoryHostModel), "stub"));
    let ctx = host.ctx_for("unsafe-grader", None).await.expect("context");
    let mut snapshot = ctx.config.clone();
    snapshot
        .resolved_spec
        .tool_descriptors
        .push(ToolDescriptor::pinned(
            "test",
            "write",
            "writes",
            serde_json::json!({"type": "object"}),
        ));

    let activation = RunActivation::new(
        RunId("unsafe-grader-1".into()),
        ctx.thread_id.clone(),
        snapshot,
        user("grade"),
    )
    .without_tools();
    // Cause graph: C1 the snapshot has a colliding model-facing tool catalog;
    // C2 this attempt narrows authority to DenyAll. E1 no tool or discovery
    // prompt is projected, E2 the model can answer normally, and E3 a recovered
    // or injected call remains blocked by the same closed narrowing enum.
    let state = BoundRunExecutor::new(&host, ctx)
        .execute(activation, RuntimeRunContext::new())
        .await
        .expect("declared tools do not bypass a per-Run deny-all restriction");

    assert!(
        matches!(state, RunState::Ended(EndCause::NaturalEnd)),
        "unexpected restricted Run state: {state:?}"
    );
}

/// A model that blocks on its first inference until released, so a concurrent
/// `interrupt` lands while a plain Run is in flight.
struct BlockOnceModel {
    reached: Arc<tokio::sync::Notify>,
    gate: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl LlmExecutor for BlockOnceModel {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.reached.notify_one();
        self.gate.notified().await;
        Ok(ChatResponse {
            output: AssistantOutput::text("too late"),
            usage: None,
            stop_reason: None,
        })
    }
}

/// The real-Run interrupt the conformance matrix flagged as unasserted: a plain
/// `run` (a Managed Session's normal Run), interrupted while its inference is in
/// flight, ends `Cancelled` promptly instead of running to completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_ends_an_in_flight_run_as_cancelled() {
    // Test design. Causes: C0 the canonical Dispatch runtime is installed; C1 a
    // Managed Run is blocked in inference; C2 interrupt is accepted before
    // inference returns. Effects: E0 setup errors fail before the gate wait; E1
    // C2 ends the exact Run as Cancelled rather than committing the late model
    // reply. Constraint/Invariant: interruption targets the active Run generation
    // only. Decision rule: C0, block, interrupt, release inference, require E0+E1.
    let reached = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(SharedHost::new(
        Arc::new(BlockOnceModel {
            reached: reached.clone(),
            gate: gate.clone(),
        }),
        "scripted",
    ));
    let _managed = install_test_dispatch_runtime(&host);

    let driver = host.clone();
    let mut task = tokio::spawn(async move { driver.run(None, "t-int", user("go")).await });

    // The Run is blocked mid-inference; interrupt it, then release the gate.
    await_interrupt_inference_gate(&reached, &mut task).await;
    host.interrupt("t-int").await.expect("interrupt");
    gate.notify_one();

    let result = task.await.expect("join").expect("run");
    assert!(
        matches!(result.state, RunState::Ended(EndCause::Cancelled)),
        "an interrupted in-flight Run ends Cancelled, not run to completion: {:?}",
        result.state
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_interrupt_returns_after_intent_before_the_blocked_attempt_finishes() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect: C0 is the canonical Dispatch runtime composition; C1 is a
    // durable Run blocked inside Provider inference; C2 is an interrupt accepted
    // by the Session control edge. E0 is that setup failure cannot masquerade as
    // a gate hang; E1 is that C2 returns while C1 remains blocked; E2 is the
    // existing pool drainer committing one Cancelled terminal fact under the new
    // claim epoch. Constraint: the caller may wake the pool but must never become
    // a synchronous dispatch driver.
    //
    // | Rule | Runtime | durable Run | Provider | interrupt | Effects |
    // |---|---|---|---|---|---|
    // | R1 | installed | active | blocked | accepted | E0 + E1 + eventual E2 |
    // | R2 | installed | absent | n/a | replay/no-op | immediate success (sibling test) |
    let reached = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    let host = Arc::new(
        SharedHost::new(
            Arc::new(BlockOnceModel {
                reached: reached.clone(),
                gate: gate.clone(),
            }),
            "scripted",
        )
        .with_dispatch_store(dispatch),
    );
    let _managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool();

    let driver = host.clone();
    let mut run =
        tokio::spawn(async move { driver.run(None, "durable-interrupt", user("go")).await });
    await_interrupt_inference_gate(&reached, &mut run).await;

    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        host.interrupt("durable-interrupt"),
    )
    .await
    .expect("R1/E1 interrupt admission must not await Provider completion")
    .expect("R1 interrupt admission");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .expect("R1/E2 cancellation drainer")
        .expect("R1 join")
        .expect("R1 durable Run");
    assert!(
        matches!(result.state, RunState::Ended(EndCause::Cancelled)),
        "R1/E2: {:?}",
        result.state
    );
    gate.notify_waiters();
}

#[tokio::test]
async fn live_only_durable_control_never_materializes_a_cold_session() {
    use awaken_run_ingress::DispatchQueue as _;

    // Cause/effect table for the Host boundary: C1 typed durable capability is
    // enabled; C2 a process-resident Session context exists; C3 the Runtime owns
    // the exact active attempt. Effects: E1 reject with the capability error;
    // E2 reject as NoSubscriber; E3 deliver through the existing live-control
    // service; E4 create no Session/Environment/dispatch fact.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | W0 | F | any | any | E1 + E4 |
    // | W1 | T | F | F | E2 + E4 |
    // | W2 | T | T | F | E2 (LiveRunControlService test) |
    // | W3 | T | T | T | E3 (Runtime active-attempt test) |
    //
    // This test owns W0/W1, where the regression previously called `ctx_for`
    // and surfaced an unrelated Environment failure. The cited lower-owner
    // tests retain W2/W3, so this Host test adds no parallel attempt registry.
    let cold_thread = "cold-live-only-control";
    let run_id = "cold-live-only-run";
    let direct = SharedHost::new(Arc::new(OkModel), "stub");
    let unsupported = direct
        .wake_durable(cold_thread, run_id)
        .await
        .expect_err("W0 direct ingress rejects durable control");
    assert_eq!(unsupported.kind, HostErrorKind::BadRequest, "W0/E1");
    assert!(
        unsupported.message.contains("durable ingress not enabled"),
        "W0/E1: {}",
        unsupported.message
    );
    assert!(
        direct.session_slots.read(cold_thread, |_| ()).is_none(),
        "W0/E4 direct control must not create a Session slot"
    );

    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("live-control dispatch store"),
    );
    let durable = SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(dispatch.clone());
    let wake = durable
        .wake_durable(cold_thread, run_id)
        .await
        .expect_err("W1 cold wake has no subscriber");
    assert_eq!(wake.kind, HostErrorKind::BadRequest, "W1/E2");
    assert_eq!(
        wake.message,
        format!("no live subscriber for run: {run_id}"),
        "W1/E2"
    );

    let explicit_pause = durable
        .pause_durable(cold_thread, Some(run_id))
        .await
        .expect_err("W1 cold explicit pause has no subscriber");
    assert_eq!(explicit_pause.kind, HostErrorKind::BadRequest, "W1/E2");
    assert_eq!(explicit_pause.message, wake.message, "W1/E2");
    let implicit_pause = durable
        .pause_durable(cold_thread, None)
        .await
        .expect_err("W1 cold implicit pause has no active attempt");
    assert_eq!(implicit_pause.kind, HostErrorKind::BadRequest, "W1/E2");
    assert_eq!(
        implicit_pause.message, "thread has no locally owned active run",
        "W1/E2"
    );
    assert!(
        durable.session_slots.read(cold_thread, |_| ()).is_none(),
        "W1/E4 live-only control must not materialize a Session or Environment"
    );
    assert!(
        dispatch
            .list_dispatches()
            .await
            .expect("W1 inspect dispatch authority")
            .is_empty(),
        "W1/E4 live-only control must not create a dispatch fact"
    );
}

#[tokio::test]
async fn managed_user_run_reservation_precedes_physical_environment_realization() {
    use awaken_run_ingress::DispatchQueue as _;

    // Cause/effect graph: C1 a cold Managed Session has no Runtime or
    // Environment; C2 its complete command is exact or operation/capabilities
    // change; C3 durable phase is Reserved, Activated, Recovery, or Completed;
    // C4 only trace context changes. Effects: E1 persist one unclaimable
    // reservation; E2 create no Environment; E3 evict the envelope-only Runtime
    // so the claimed Worker must rebuild from frozen dispatch truth; E4 freeze
    // application restrictions and current command fingerprint in that row; E5
    // exact/C4 retries report the phase; E6 command changes conflict against
    // both live and completed durable evidence as BadRequest.
    // Constraint: physical realization remains in the existing claimed Worker
    // path; reservation introduces no second executor or store.
    //
    // | Rule | Command | Phase | Effects |
    // |---|---|---|---|
    // | R1 | exact | Reserved/Activated/Recovery/Completed | E1-E5 |
    // | R2 | operation changed | Reserved/Completed | E6 |
    // | R3 | capabilities empty to present | Reserved/Completed | E6 |
    // | R4 | trace changed | Reserved | E5 |
    let thread = "cold-session-reservation";
    let run_id = RunId("cold-session-reservation-run".into());
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("reservation dispatch"),
    );
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(dispatch.clone()));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    let command = awaken_session_contract::AdmitSessionRun {
        session_id: thread.into(),
        agent_id: "assistant".into(),
        operation_id: "cold-session-reservation-op".into(),
        run_id: run_id.clone(),
        messages: vec![Message::new(
            MessageId::session_event_input(thread, "cold-session-reservation-op"),
            Role::User,
            vec![ContentBlock::text("reserve before realization")],
        )],
        data_subject_id: None,
        traceparent: None,
        execution_requirements: awaken_session_contract::SessionRunExecutionRequirements {
            tool_capability_narrowing:
                awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll,
            required_worker_capabilities: std::collections::BTreeSet::from([
                "application:test-session-envelope/v1".to_string(),
            ]),
        },
        replacement: awaken_session_contract::SessionRunReplacement::PreservePrior,
    };
    let expected_fingerprint =
        awaken_session_contract::SessionRunCommandFingerprint::current(&command);

    assert_eq!(
        managed
            .reserve_session_run(command.clone())
            .await
            .expect("R1 reservation"),
        awaken_session_contract::SessionRunReservation::Reserved,
        "R1/E1"
    );
    assert_eq!(
        managed
            .reserve_session_run(command.clone())
            .await
            .expect("R1 exact Reserved replay"),
        awaken_session_contract::SessionRunReservation::AlreadyReserved,
        "R1/E5 Reserved"
    );
    let mut retraced = command.clone();
    retraced.traceparent = Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into());
    assert_eq!(
        managed
            .reserve_session_run(retraced)
            .await
            .expect("R4 trace-only replay"),
        awaken_session_contract::SessionRunReservation::AlreadyReserved,
        "R4/E5"
    );
    let mut changed_operation = command.clone();
    changed_operation.operation_id = "cold-session-reservation-other-op".into();
    assert!(
        managed
            .reserve_session_run(changed_operation)
            .await
            .is_err(),
        "R2/E6 operation change conflicts"
    );
    assert_eq!(
        managed
            .session_run_state(thread, &RunId("cold-session-reservation-run".into()))
            .await
            .expect("R1 read pre-claim state"),
        None,
        "R1/E2 recovery read observes committed Thread truth without realization"
    );
    let rows = dispatch
        .list_dispatches()
        .await
        .expect("R1 inspect dispatch");
    assert_eq!(rows.len(), 1, "R1/E1 exact reservation");
    assert_eq!(
        rows[0].state,
        awaken_run_ingress::DispatchState::Reserved,
        "R1/E1 remains unclaimable before activity activation"
    );
    assert!(host.session_environment(thread).await.is_none(), "R1/E2");
    assert!(
        host.session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten()
            .is_none(),
        "R1/E3"
    );

    let mut empty_capabilities = command.clone();
    empty_capabilities.run_id = RunId("cold-session-empty-capabilities-run".into());
    empty_capabilities.operation_id = "cold-session-empty-capabilities-op".into();
    empty_capabilities.messages = vec![Message::new(
        MessageId::session_event_input(thread, "cold-session-empty-capabilities-op"),
        Role::User,
        vec![ContentBlock::text("empty capability command")],
    )];
    empty_capabilities
        .execution_requirements
        .required_worker_capabilities
        .clear();
    assert_eq!(
        managed
            .reserve_session_run(empty_capabilities.clone())
            .await
            .expect("R3 reserve empty capabilities"),
        awaken_session_contract::SessionRunReservation::Reserved,
        "R3 precondition"
    );
    let mut added_capability = empty_capabilities.clone();
    added_capability
        .execution_requirements
        .required_worker_capabilities
        .insert("application:added-after-reservation/v1".into());
    assert!(
        managed.reserve_session_run(added_capability).await.is_err(),
        "R3/E6 empty-to-present capability change conflicts"
    );
    let mut recovery_command = command.clone();
    recovery_command.run_id = RunId("cold-session-recovery-run".into());
    recovery_command.operation_id = "cold-session-recovery-op".into();
    recovery_command.messages = vec![Message::new(
        MessageId::session_event_input(thread, "cold-session-recovery-op"),
        Role::User,
        vec![ContentBlock::text("recover reserved command")],
    )];
    assert_eq!(
        managed
            .reserve_session_run(recovery_command.clone())
            .await
            .expect("R1 reserve recovery probe"),
        awaken_session_contract::SessionRunReservation::Reserved,
        "R1/E5 Recovery precondition"
    );
    let recovery_claim = dispatch
        .claim_run(
            &recovery_command.run_id,
            "reservation-recovery-owner",
            1_000,
            u64::MAX,
            &Default::default(),
        )
        .await
        .expect("R1 claim expired recovery probe")
        .expect("R1 expired reservation is repairable");
    assert!(
        recovery_claim.session_activity_admission_required,
        "R1 Recovery claim stays admission-only"
    );
    assert_eq!(
        managed
            .reserve_session_run(recovery_command)
            .await
            .expect("R1 exact Recovery replay"),
        awaken_session_contract::SessionRunReservation::RecoveryClaimed,
        "R1/E5 Recovery"
    );
    assert_eq!(
        dispatch
            .resolve_claimed_session_run_reservation(
                &awaken_run_ingress::RunClaim::from(&recovery_claim.lease),
                awaken_run_ingress::SessionRunReservationResolution::Rejected,
            )
            .await
            .expect("R1 remove recovery probe"),
        awaken_run_ingress::SettleOutcome::Applied,
        "R1 Recovery cleanup"
    );

    assert_eq!(
        dispatch
            .activate_session_run_reservation(
                &RunId("cold-session-reservation-run".into()),
                &ThreadId(thread.into()),
                1,
            )
            .await
            .expect("R1 activate exact reservation"),
        awaken_run_ingress::SessionRunReservationActivation::Activated,
        "R1/E4 inspection uses the ordinary post-activity transition"
    );
    assert_eq!(
        managed
            .reserve_session_run(command.clone())
            .await
            .expect("R1 exact Activated replay"),
        awaken_session_contract::SessionRunReservation::AlreadyActivated {
            session_activity_epoch: 1,
        },
        "R1/E5 Activated"
    );
    let mut manifest = awaken_run_ingress::WorkerManifest::default();
    manifest
        .capabilities
        .insert("application:test-session-envelope/v1".into());
    let capability_fingerprint = manifest
        .fingerprint()
        .expect("R1 deterministic Worker capability fingerprint");
    let worker = awaken_run_ingress::WorkerSnapshot {
        identity: awaken_run_ingress::WorkerIdentity::new("inspection-worker", "boot-1", 1),
        state: awaken_run_ingress::WorkerState::Ready,
        manifest,
        capability_fingerprint,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        expires_at_ms: 10_000,
    };
    let claimed = dispatch
        .claim_compatible(&worker, 1_000, 0)
        .await
        .expect("R1 inspect executable dispatch")
        .expect("R1 exact executable dispatch");
    assert_eq!(
        claimed.request.activation.tool_capability_narrowing,
        awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll,
        "R1/E4 application restrictions only narrow Session tool authority"
    );
    assert_eq!(
        claimed.request.placement.location,
        awaken_run_ingress::ExecutionLocation::RemoteRequired,
        "R1/E4 application protocol cannot fall back to a generic local executor"
    );
    assert!(
        claimed
            .request
            .placement
            .required_capabilities
            .contains("application:test-session-envelope/v1"),
        "R1/E4 capability is frozen in the one durable dispatch"
    );
    assert_eq!(
        claimed.request.session_command_fingerprint.as_ref(),
        Some(&expected_fingerprint),
        "R1/E4 fingerprint is computed from the raw command before projection"
    );
    assert_eq!(
        dispatch
            .settle(
                &run_id,
                claimed.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("R1 settle Managed reservation"),
        awaken_run_ingress::SettleOutcome::Applied,
        "R1 Completed precondition"
    );
    assert_eq!(
        managed
            .reserve_session_run(command)
            .await
            .expect("R1 exact Completed replay"),
        awaken_session_contract::SessionRunReservation::Completed,
        "R1/E5 Completed"
    );
    let completion = dispatch
        .completion_events_after(0, usize::MAX)
        .await
        .expect("R1 completion fingerprint query")
        .into_iter()
        .find(|completion| completion.run_id == run_id)
        .expect("R1 Managed completion exists");
    assert_eq!(
        completion.request_fingerprint.as_deref(),
        Some(expected_fingerprint.as_str()),
        "R1/E4 completion retains the current compact identity"
    );

    assert_eq!(
        dispatch
            .activate_session_run_reservation(
                &empty_capabilities.run_id,
                &ThreadId(thread.into()),
                2,
            )
            .await
            .expect("R3 activate empty-capability completion probe"),
        awaken_run_ingress::SessionRunReservationActivation::Activated,
        "R3 Completed precondition"
    );
    let empty_capabilities_claim = dispatch
        .claim_run(
            &empty_capabilities.run_id,
            "empty-capability-completion-worker",
            1_000,
            0,
            &Default::default(),
        )
        .await
        .expect("R3 claim empty-capability completion probe")
        .expect("R3 activated empty-capability probe is claimable");
    assert_eq!(
        dispatch
            .settle(
                &empty_capabilities.run_id,
                empty_capabilities_claim.lease.epoch,
                awaken_run_ingress::DispatchOutcome::Done,
                &[],
            )
            .await
            .expect("R3 settle empty-capability completion probe"),
        awaken_run_ingress::SettleOutcome::Applied,
        "R3 Completed precondition"
    );
    assert_eq!(
        managed
            .reserve_session_run(empty_capabilities.clone())
            .await
            .expect("R3 exact empty-capability Completed replay"),
        awaken_session_contract::SessionRunReservation::Completed,
        "R3/E5 Completed"
    );
    let mut completed_changed_operation = empty_capabilities.clone();
    completed_changed_operation.operation_id =
        "cold-session-empty-capabilities-completed-other-op".into();
    let completed_operation_error = managed
        .reserve_session_run(completed_changed_operation)
        .await
        .expect_err("R2 completed operation change conflicts");
    assert_eq!(
        completed_operation_error.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "R2/E6 Completed"
    );
    let mut completed_added_capability = empty_capabilities;
    completed_added_capability
        .execution_requirements
        .required_worker_capabilities
        .insert("application:added-after-completion/v1".into());
    let completed_capability_error = managed
        .reserve_session_run(completed_added_capability)
        .await
        .expect_err("R3 completed empty-to-present capability change conflicts");
    assert_eq!(
        completed_capability_error.kind,
        awaken_session_contract::RunErrorKind::BadRequest,
        "R3/E6 Completed"
    );
}

#[tokio::test]
async fn acp_execution_rebuilds_an_envelope_only_reservation_context() {
    // Test design: acp_execution_rebuilds_an_envelope_only_reservation_context
    // Cause/effect graph: C1 an ACP publication; C2 reservation preflight leaves
    // an envelope-only cached context; C3 execution wins the race before the
    // reservation caller evicts it; C4 the admitted Namespace provider preserves
    // one path for arbitrary ACP processes; C5 the direct live-authored Skill
    // source exists while initially empty and therefore requires the canonical
    // ACP tool-export adapter. Effects: E1 execution replaces that context; E2
    // the replacement owns a physical Environment; E3 Skill tools cross the
    // existing ACP MCP export boundary. Decision table:
    // ACP+C1-C5=>E1+E2+E3; ACP+C1-C3+split-Workdir=>fail before effects (owned by
    // `projected_acp_rejects_a_provider_with_split_tool_and_process_paths`);
    // Native on-tool-use/A2A contexts remain eligible for environment-free reuse.
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new("", "", "acp:opencode"),
            ),
        )
        .build();
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("one exact ACP publication");
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]),
    ));
    let sandbox_root = tempfile::tempdir().expect("Namespace fixture root");
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_agent_publications(Arc::new(publications))
        .with_acp(Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(
            source,
        )))
        .with_acp_tool_exporter(Arc::new(
            crate::acp_tool_export::RecordingAcpToolExporter::default(),
        ));
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            sandbox_root.path(),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let (host, _managed) = dispatch_test_host(raw_host);

    let reserved = host
        .ctx_for_session_reservation("acp-reservation-race", Some("assistant"))
        .await
        .expect("reservation preflight");
    assert!(reserved.env.is_none(), "C1+C2: reservation is effect-free");

    let executable = host
        .ctx_for("acp-reservation-race", Some("assistant"))
        .await
        .expect("C3 execution rebuild");
    assert!(
        !Arc::ptr_eq(&reserved, &executable),
        "E1: execution cannot reuse the envelope-only ACP context"
    );
    assert!(
        executable.env.is_some(),
        "E2: exact ACP routing is paired with its Session Environment"
    );
}

#[tokio::test]
async fn managed_interrupt_cancels_cold_dispatch_without_realizing_an_environment() {
    use awaken_run_ingress::{DispatchQueue as _, RunDispatch};

    // Cause/effect graph: C1 one Session-affined dispatch exhausted retries; C2
    // no Runtime context/Environment is resident; C3 Environment realization
    // may be unavailable; C4 user.interrupt is accepted. Effects: E1 select and
    // requeue the exact dead-letter row; E2 persist its cancellation so the
    // ordinary Worker cancellation claim can settle it; E3 do not construct a
    // Runtime context or touch Environment realization. Constraint: this test
    // observes the canonical Session slot and dispatch store only; it adds no
    // control registry or force-delete path.
    //
    // | Rule | dispatch | resident context | Environment | interrupt | Effects |
    // |---|---|---|---|---|---|
    // | C1 | unique dead letter | absent | unavailable/unknown | accepted | E1+E2+E3 |
    // | C2 | absent | absent | unavailable/unknown | accepted | no-op+E3 (idle sibling) |
    // | C3 | multiple | absent | any | accepted | reject (ambiguity sibling) |
    let thread = "cold-managed-interrupt";
    let run_id = RunId("cold-managed-interrupt-run".into());
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                thread, &run_id.0,
            ))
            .for_session(ThreadId(thread.into()))
            .with_session_activity_epoch(1),
        )
        .await
        .expect("C1 executable Session dispatch");
    let claimed = dispatch
        .claim("failed-worker", 1, 0, &Default::default())
        .await
        .expect("C1 claim")
        .expect("C1 exact claim");
    assert_eq!(
        dispatch
            .quarantine_retry_exhausted(0, claimed.lease.expires_ms.saturating_add(1))
            .await
            .expect("C1 quarantine retry exhaustion"),
        1,
        "C1 exact dead letter"
    );
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(dispatch.clone());
    assert!(
        host.session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten()
            .is_none(),
        "C2 precondition"
    );

    host.interrupt(thread).await.expect("C4 interrupt accepted");

    let rows = dispatch
        .list_dispatches()
        .await
        .expect("E1 inspect canonical dispatch");
    assert_eq!(rows.len(), 1, "E1 exact row");
    assert_eq!(
        rows[0].state,
        awaken_run_ingress::DispatchState::Pending,
        "E1 dead letter reuses the same runnable row"
    );
    assert!(rows[0].cancellation_requested, "E2 durable cancellation");
    assert!(
        host.session_slots
            .read(thread, |slot| slot.runtime.clone())
            .flatten()
            .is_none(),
        "E3 interrupt must not realize a cold Runtime/Environment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_interrupt_signals_the_registered_attempt_only_after_durable_intent() {
    // Test design summary; the full L1 matrix is below. Causes: C1 an exact
    // attempt is registered and C2 Managed interrupt selects its durable row.
    // Effects: E1 durable cancel intent precedes E2 live signal and E3 settlement.
    // Constraint/Invariant: live signaling accelerates but never replaces durable
    // authority. Decision rule: L1 requires E1 -> E2 -> E3 in that order.
    use awaken_run_ingress::{DispatchQueue as _, RunDispatch};
    use awaken_runtime_contract::execution::{
        Error as ExecutionError, Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
    };
    use awaken_runtime_contract::resume::ResumeCommand;

    struct BlockingManagedAttempt {
        dispatch: Arc<awaken_run_ingress::AnyDispatchStore>,
        entered: tokio::sync::Notify,
        live_cancel_observed: tokio::sync::Notify,
        intent_preceded_signal: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl RunExecutor for BlockingManagedAttempt {
        async fn execute(
            &self,
            activation: RunActivation,
            context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            let cancellation = context.cancellation.ok_or_else(|| {
                ExecutionError::Execution("Managed attempt has no cancellation token".into())
            })?;
            self.entered.notify_one();
            cancellation.cancelled().await;
            let persisted = self
                .dispatch
                .list_dispatches()
                .await
                .map_err(|error| ExecutionError::Execution(error.to_string()))?
                .into_iter()
                .find(|row| row.run_id == activation.run_id)
                .is_some_and(|row| row.cancellation_requested);
            self.intent_preceded_signal
                .store(persisted, Ordering::SeqCst);
            self.live_cancel_observed.notify_one();
            Ok(RunState::Ended(EndCause::Cancelled))
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for BlockingManagedAttempt {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            Err(ExecutionError::Execution(
                "fresh Managed interruption test must not resume".into(),
            ))
        }
    }

    // Cause/effect graph: C1 a Session-affined Run is actively blocked inside
    // an opaque attempt; C2 Worker RAII registered its exact cancellation token
    // in the Session Runtime; C3 the Managed Event path has direct foreground
    // delivery and no `active_run`/`cancel` hint, while its Run still executes
    // from the process dispatch authority; C4 user.interrupt selects that row.
    // Effects: E1 the dispatch authority records `cancel_requested` first; E2
    // only then the same Runtime registry cancels the blocked attempt promptly;
    // E3 the pool's ordinary replacement claim commits one Cancelled terminal
    // and settles the row. Constraints: Runtime delivery is only an accelerator,
    // the caller never drives a second claim, and no Host/ACP-private registry is
    // introduced. The neighboring remote-cancel and ambiguous/idle tests own the
    // no-local-registration, multiple-row, and absent-row rules.
    //
    // | Rule | active row | exact registration | foreground ingress/hint | interrupt | Effects |
    // |---|---|---|---|---|---|
    // | L1 | leased | yes | direct/absent | accepted | E1 + E2 + E3 |
    let thread = "managed-live-cancel";
    let run_id = RunId("managed-live-cancel-run".into());
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    let attempt = Arc::new(BlockingManagedAttempt {
        dispatch: dispatch.clone(),
        entered: tokio::sync::Notify::new(),
        live_cancel_observed: tokio::sync::Notify::new(),
        intent_preceded_signal: std::sync::atomic::AtomicBool::new(false),
    });
    let mut host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_dispatch_store(dispatch.clone())
        .with_remote_attempt_executor(RemoteAttemptInstallation {
            executor: attempt.clone(),
            credential_realization: Default::default(),
        });
    // Managed Session Events use the dispatch authority independently of the
    // ordinary foreground-ingress choice. Mirror the scenario-host topology:
    // direct foreground plus a co-located process pool.
    host.deployment.durable = false;
    let host = Arc::new(host);
    install_recording_session_application(&host);
    host.ensure_dispatch_pool();

    let mut activation =
        crate::host::worker_resolver::test_support::test_activation(thread, &run_id.0);
    activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "remote",
                "",
                "a2a:http://blocking.invalid",
            ),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "sha256:blocking-remote",
        )
        .expect("coherent blocking remote candidate");
    dispatch
        .enqueue(
            RunDispatch::new(activation)
                .for_session(ThreadId(thread.into()))
                .with_session_activity_epoch(1),
        )
        .await
        .expect("L1 Managed Run admission");
    host.dispatch_pool_or_err().expect("L1 pool").notify().await;

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        attempt.entered.notified(),
    )
    .await
    .expect("L1/C1+C2 exact attempt entered");
    let ctx = host
        .ctx_for(thread, None)
        .await
        .expect("L1 resident Session");
    assert!(
        !ctx.delivery.is_durable()
            && ctx.active_run.lock().unwrap().is_none()
            && ctx.cancel.lock().unwrap().is_none(),
        "L1/C3 Managed Event execution has no foreground control hint"
    );
    let active = dispatch
        .list_dispatches()
        .await
        .expect("L1 inspect active row");
    assert_eq!(active.len(), 1, "L1/C1 one exact dispatch");
    assert_eq!(
        active[0].state,
        awaken_run_ingress::DispatchState::Leased,
        "L1/C1 cancellation must land after the attempt is executing"
    );
    assert!(!active[0].cancellation_requested, "L1/C1 precondition");

    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        host.interrupt(thread),
    )
    .await
    .expect("L1 interrupt admission is non-blocking")
    .expect("L1 interrupt accepted");
    tokio::time::timeout(
        std::time::Duration::from_millis(250),
        attempt.live_cancel_observed.notified(),
    )
    .await
    .expect("L1/E2 registered attempt receives live cancellation");
    assert!(
        attempt.intent_preceded_signal.load(Ordering::SeqCst),
        "L1/E1 durable intent must precede the live signal"
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if dispatch
                .list_dispatches()
                .await
                .expect("L1 inspect settlement")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("L1/E3 cancellation settlement");
    assert_eq!(
        host.commit_for_read(thread)
            .await
            .expect("L1 committed truth")
            .run_state(&run_id),
        Some(RunState::Ended(EndCause::Cancelled)),
        "L1/E3"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn managed_primary_interrupt_recovers_the_active_dispatch_without_a_local_run_hint() {
    // Test design. Causes: C0 the canonical Dispatch runtime is installed; C1
    // the primary Session has no local run/cancel hint; C2 exactly one executable
    // durable dispatch is active; C3 a remote attempt owns it. Effects: E1
    // interrupt selects C2, persists cancellation, invokes remote cancel, and
    // settles Cancelled. Constraint/Invariant: selection uses durable dispatch
    // truth, never guessed process state. Decision rule: C0+unique C2+C3 yields
    // E1; ambiguity is owned by the fail-closed sibling test.
    use awaken_run_ingress::{Clock as _, DispatchQueue as _, RunDispatch};
    use awaken_runtime_contract::execution::{
        Error as ExecutionError, Result as ExecutionResult, RunAttemptExecutor, RunExecutor,
    };
    use awaken_runtime_contract::resume::ResumeCommand;

    struct RecordingRemoteAttempt {
        cancelled: Mutex<Vec<RunId>>,
        cancellation_seen: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl RunExecutor for RecordingRemoteAttempt {
        async fn execute(
            &self,
            _activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            Err(ExecutionError::Execution(
                "the replacement cancellation claim must not execute the remote Run".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl RunAttemptExecutor for RecordingRemoteAttempt {
        async fn resume(
            &self,
            _activation: RunActivation,
            _command: ResumeCommand,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<RunState> {
            Err(ExecutionError::Execution(
                "the replacement cancellation claim must not resume the remote Run".into(),
            ))
        }

        async fn cancel(
            &self,
            activation: RunActivation,
            _context: RuntimeRunContext,
        ) -> ExecutionResult<()> {
            self.cancelled.lock().unwrap().push(activation.run_id);
            self.cancellation_seen.notify_one();
            Ok(())
        }
    }

    // Cause/effect graph: C1 a Managed primary Run is already leased from the
    // one durable Dispatch store; C2 its immutable backend is remote; C3 the Run
    // bypassed BoundRunExecutor, so SessionCtx has no `active_run`; C4 the same
    // accepted user interrupt reaches SharedHost. Effects: E1 C4 resolves the
    // unique executable Session-affined row instead of succeeding as a no-op;
    // E2 the ordinary pool cancellation claim invokes the installed remote
    // executor exactly once with that Run id; E3 the claim commits Cancelled and
    // settles the row. Constraints: the interrupt caller records intent only;
    // the old lease is fenced and may not perform remote cancellation; no second
    // queue, live-control bus, Session cache, or synchronous driver is created.
    //
    // | Rule | leased row | backend | local hint | interrupt | Effects |
    // |---|---|---|---|---|---|
    // | M1 | unique + Session-affined | remote | absent | accepted | E1+E2+E3 |
    // | M2 | absent | any | absent | accepted | no-op (existing idle test) |
    // | M3 | multiple executable primary rows | any | absent | accepted | fail closed |
    // | M4 | exact local hint | any | present | accepted | existing exact-id path |
    let thread = "managed-active-remote-interrupt";
    let run_id = RunId("managed-active-remote-run".into());
    let mut activation =
        crate::host::worker_resolver::test_support::test_activation(thread, &run_id.0);
    activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "remote",
                "",
                "a2a:http://remote.invalid",
            ),
            awaken_tenancy::ScopeId::from("default"),
            None,
            "sha256:active-remote",
        )
        .expect("coherent active remote candidate");
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    dispatch
        .enqueue(
            RunDispatch::new(activation)
                .for_session(ThreadId(thread.into()))
                .with_session_activity_epoch(1),
        )
        .await
        .expect("M1 Managed Run admission");
    let previous = dispatch
        .claim(
            "previous-worker",
            DEFAULT_LEASE_MS,
            awaken_run_ingress::SystemClock.now_ms(),
            &Default::default(),
        )
        .await
        .expect("M1 old Worker claim")
        .expect("M1 active dispatch");
    assert_eq!(previous.lease.run_id, run_id, "M1/C1");

    let remote = Arc::new(RecordingRemoteAttempt {
        cancelled: Mutex::new(Vec::new()),
        cancellation_seen: tokio::sync::Notify::new(),
    });
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_dispatch_store(dispatch.clone())
            .with_remote_attempt_executor(RemoteAttemptInstallation {
                executor: remote.clone(),
                credential_realization: Default::default(),
            }),
    );
    install_recording_session_application(&host);
    let _managed = install_test_dispatch_runtime(&host);
    let ctx = host
        .ctx_for(thread, None)
        .await
        .expect("M1 resident Session");
    assert!(
        ctx.active_run.lock().unwrap().is_none(),
        "M1/C3 Managed admission has no foreground hint"
    );
    host.ensure_dispatch_pool();

    host.interrupt(thread)
        .await
        .expect("M1/E1 interrupt intent");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        remote.cancellation_seen.notified(),
    )
    .await
    .expect("M1/E2 canonical cancellation claim");
    assert_eq!(
        *remote.cancelled.lock().unwrap(),
        std::slice::from_ref(&run_id),
        "M1/E2"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if dispatch
                .list_dispatches()
                .await
                .expect("M1 inspect settlement")
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("M1/E3 cancellation settlement");
    assert_eq!(
        host.commit_for_read(thread)
            .await
            .expect("M1 committed truth")
            .run_state(&run_id),
        Some(RunState::Ended(EndCause::Cancelled)),
        "M1/E3"
    );
}

#[tokio::test]
async fn managed_primary_interrupt_fails_closed_on_ambiguous_dispatch_authority() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_run_ingress::{DispatchQueue as _, RunDispatch};

    // Cause/effect graph: C1 the process-local active Run hint is absent; C2 the
    // authoritative store exposes two executable rows with the same primary
    // Session affinity; C3 an interrupt is requested. Effects: E1 reject the
    // inconsistent authority instead of choosing or mass-cancelling; E2 neither
    // row receives cancellation intent. Constraint: a present exact local hint
    // retains the established exact-id path and is covered by the neighboring
    // durable interrupt test.
    //
    // | Rule | local hint | executable primary rows | interrupt | Effects |
    // |---|---|---|---|---|
    // | A1 | absent | two | requested | E1+E2 |
    let thread = "ambiguous-managed-primary-interrupt";
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("dispatch store"),
    );
    for run in ["ambiguous-primary-a", "ambiguous-primary-b"] {
        dispatch
            .enqueue(
                RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                    thread, run,
                ))
                .for_session(ThreadId(thread.into()))
                .with_session_activity_epoch(1),
            )
            .await
            .expect("A1 conflicting executable admission");
    }
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(dispatch.clone());

    let error = host
        .interrupt(thread)
        .await
        .expect_err("A1/E1 ambiguous primary authority must fail closed");
    assert!(
        error
            .message
            .contains("multiple executable primary dispatches"),
        "A1/E1: {error:?}"
    );
    let rows = dispatch
        .list_dispatches()
        .await
        .expect("A1 inspect unchanged rows");
    assert_eq!(rows.len(), 2, "A1/C2");
    assert!(rows.iter().all(|row| !row.cancellation_requested), "A1/E2");
}

/// The main assistant answers plainly; the memory extractor (identified by its
/// system instructions) saves one memory then reports done.
pub(crate) struct MemoryHostModel;

#[async_trait::async_trait]
impl LlmExecutor for MemoryHostModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let is_extractor = request.messages.iter().any(|m| {
            m.role == Role::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction Agent"),
                    _ => false,
                })
        });
        let output = if is_extractor {
            if request.messages.iter().any(|m| m.role == Role::Tool) {
                AssistantOutput::text("saved 1 memory")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({
                        "name": "user prefs",
                        "content": "user likes rust",
                    }),
                }])
            }
        } else {
            AssistantOutput::text("ok")
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// The compactor (identified by its instructions) replies with a fixed summary;
/// the main assistant reports whether it saw a delivered summary in its system
/// messages, proving the summary reached the next Run's model input.
struct CompactHostModel;

#[async_trait::async_trait]
impl LlmExecutor for CompactHostModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let system_text: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let reply = if system_text.contains("conversation-compaction Agent") {
            "COMPACTED".to_string()
        } else if system_text.contains("Summary of earlier conversation") {
            "seen-summary".to_string()
        } else {
            format!(
                "no-summary-users={}",
                request
                    .messages
                    .iter()
                    .filter(|message| message.role == Role::User)
                    .count()
            )
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn compaction_summary_reaches_the_same_long_run() {
    // Test design. Causes: C1 first turn is below compaction threshold; C2 the
    // second turn crosses it on the same Thread. Effects: E1 C1 has no summary;
    // E2 C2 injects the committed summary into that Run's inference. Constraint/
    // Invariant: compaction changes context projection, not Thread identity or
    // committed history. Decision rule: execute C1 then C2 and distinguish replies.
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction_tokens(1, 1),
    );
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, "hello")];

    // Run 1: only the single user message → below threshold, no summary injected.
    let r1 = host.run(None, "t-c", user("u1")).await.expect("Run 1");
    assert!(matches!(r1.state, RunState::Ended(_)));
    let reply1 = r1
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(reply1, "no-summary-users=1", "short Run is not compacted");

    // Run 2: the conversation now exceeds the threshold, so the compact plugin's
    // BeforeInference hook summarizes the older slice inline and the model sees it.
    let r2 = host.run(None, "t-c", user("u2")).await.expect("Run 2");
    let reply2 = r2
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(reply2, "seen-summary");
}

#[tokio::test]
async fn compaction_keeps_full_history_until_a_summary_activates_the_window() {
    // Test design. Causes: C1 multiple turns remain below the configured summary
    // threshold. Effects: E1 the model sees all user history and no KeepLast fold
    // activates. Constraint/Invariant: window truncation requires committed prefix
    // coverage from a summary. Decision rule: keep C1 false for summary activation
    // and require both user messages remain visible.
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction_tokens(100, 1),
    );
    let user = |id: &str| vec![Message::text(MessageId(id.into()), Role::User, id)];

    host.run(None, "t-before-fold", user("u1"))
        .await
        .expect("Run 1");
    let run = host
        .run(None, "t-before-fold", user("u2"))
        .await
        .expect("Run 2");
    let reply = run
        .new_messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| block_text(&message.content))
        .unwrap_or_default();
    assert_eq!(
        reply, "no-summary-users=2",
        "KeepLast must remain inactive until compaction supplies prefix coverage"
    );
}

/// Durable compaction dispatch cause/effect graph: C1 an embedded Agent enables
/// compaction with a non-default effective token window; C2 reservation generates the
/// immutable dispatch snapshot; C3 a claimed Worker rebuilds exclusively from
/// that snapshot. Effects: E1 C1+C2 freezes the exact plugin id/config; E2
/// C1+C2+C3 retains the same window and tail. Constraint: the generic
/// `plugin_ids`/`plugin_config` projection is the sole configuration authority;
/// no process-local compaction settings are consulted after publication.
///
/// | Rule | configured | phase | window/keep_last | effect |
/// |---|---|---|---|---|
/// | C7 | 2/1 | reservation | 2/1 | freeze exact config |
/// | C8 | 2/1 | claimed rebuild | 2/1 | preserve exact config |
#[tokio::test]
async fn generated_compaction_config_survives_claimed_rebuild() {
    let host =
        Arc::new(SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction_tokens(2, 1));
    let _managed = install_test_dispatch_runtime(&host);

    let provisional = host
        .ctx_for_session_reservation("durable-compact-config", Some("assistant"))
        .await
        .expect("C7 reservation context");
    let assert_exact_config = |snapshot: &ExecutableAgentSnapshot, rule: &str| {
        assert!(
            snapshot
                .resolved_spec
                .plugin_ids
                .iter()
                .any(|id| id == awaken_ext_compact::COMPACT_PLUGIN_ID),
            "{rule} selects compact"
        );
        let config: awaken_ext_compact::CompactConfig = serde_json::from_value(
            snapshot.resolved_spec.plugin_config[awaken_ext_compact::COMPACT_PLUGIN_ID].clone(),
        )
        .expect("valid frozen CompactConfig");
        assert_eq!(
            (config.max_tokens, config.keep_last),
            (Some(2), 1),
            "{rule}"
        );
    };
    assert_exact_config(&provisional.config, "C7/E1");

    host.evict_session_for_rebuild("durable-compact-config")
        .await;
    let executable = host
        .ctx_for_snapshot(
            "durable-compact-config",
            Some("assistant"),
            Some(provisional.config.clone()),
        )
        .await
        .expect("C8 claimed-style context");
    assert_exact_config(&executable.config, "C8/E2");
}

#[tokio::test]
async fn published_compact_plugin_is_installed_from_the_immutable_agent() {
    // Cause/effect graph: C1 an immutable publication selects `compact`; C2 the
    // process has no ambient compaction override. C1+C2 must install the exact
    // publication-selected plugin before inference. Otherwise authoring accepts
    // a capability that the product runtime can never execute.
    let snapshot = crate::config::server_config(
        "published-compact",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_compact::COMPACT_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_compact::COMPACT_PLUGIN_ID.to_string(),
            serde_json::json!({}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot.clone()])
            .expect("one immutable published Agent");
    let (host, managed) = dispatch_test_host(
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications)),
    );
    install_test_session_application(&host);
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        "published-compact-thread",
        host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &snapshot,
    )
    .await;

    let outcome = run_prepared_session_messages(
        &managed,
        "published-compact",
        "published-compact-thread",
        vec![Message::text(
            MessageId("published-compact-input".into()),
            Role::User,
            "prepare the handoff",
        )],
    )
    .await
    .expect("publication-selected compact plugin runs without ambient enablement");

    assert_eq!(outcome.state(), &RunState::Ended(EndCause::NaturalEnd));
}

/// The extractor saves "the user prefers tea"; the main agent answers "tea"
/// only when that memory is present in its system context (recalled).
struct MemLoopModel;

#[async_trait::async_trait]
impl LlmExecutor for MemLoopModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let system_text: String = request
            .messages
            .iter()
            .filter(|m| m.role == Role::System)
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if system_text.contains("memory extraction Agent") {
            let already = request.messages.iter().any(|m| {
                m.role == Role::Tool
                    && m.content.iter().any(|b| match b {
                        ContentBlock::ToolResult { content, .. } => {
                            block_text(content).contains("saved memory")
                        }
                        _ => false,
                    })
            });
            let output = if already {
                AssistantOutput::text("done")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "w".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({
                        "name": "beverage-preference",
                        "content": "the user prefers tea",
                    }),
                }])
            };
            return Ok(ChatResponse {
                output,
                usage: None,
                stop_reason: None,
            });
        }
        // Main agent: answer from recalled memory when present.
        let reply = if system_text.contains("the user prefers tea") {
            "tea"
        } else {
            "ok"
        };
        Ok(ChatResponse {
            output: AssistantOutput::text(reply),
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn memory_written_in_one_thread_is_recalled_and_used_in_another() {
    // Test design. Causes: C1 Thread A extracts a durable memory into a shared
    // store; C2 fresh Thread B binds that store and runs. Effects: E1 C1 persists
    // the preference; E2 C2 recalls and uses it. Constraint/Invariant: transfer is
    // only through the Memory store, never copied Thread transcript. Decision rule:
    // write on A, drain extraction, then require recall-derived output on B.
    let host = memory_test_host(Arc::new(MemLoopModel), "memory-agent");
    install_test_memory_mounter(&host);
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "thread-1", "memory-agent", &store).await;
    install_complete_memory_test_session(&managed, "thread-2", "memory-agent", &store).await;
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // Thread 1: the user states a preference; extraction saves it.
    run_prepared_session_messages(
        &managed,
        "memory-agent",
        "thread-1",
        user("I really enjoy tea in the morning"),
    )
    .await
    .expect("Thread 1 Run");
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    assert!(
        host.memory_stores
            .fs()
            .get_by_path(&store, "/beverage-preference.md")
            .await
            .unwrap()
            .is_some(),
        "the preference should be saved in the bound store"
    );

    // Thread 2 (a fresh conversation): the saved memory is recalled into context
    // and the agent uses it to answer.
    let r = run_prepared_session_messages(
        &managed,
        "memory-agent",
        "thread-2",
        user("What beverage do I prefer?"),
    )
    .await
    .expect("Thread 2 Run");
    let reply = r
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(
        reply, "tea",
        "the fresh thread should recall and use the saved memory"
    );
}

#[tokio::test]
async fn reopening_a_direct_terminal_thread_recovers_a_missing_extraction_outbox_intent() {
    use awaken_ext_memory::MemoryExtractionRepository as _;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-memory-outbox-{stamp}"));
    let thread = "memory-outbox-thread";
    let authority = Arc::new(crate::EphemeralRuntimeAuthority::new());

    // Cause/effect graph: C1 committed terminal truth exists; C2 its extraction
    // intent is absent; C3 the direct deployment materializes its canonical
    // DirectAttemptDriver; C4 the prior process left an unowned legacy sandbox
    // root. Effects: E1 the exact terminal observer creates and completes the
    // missing intent before any Environment effect; E2 ordinary Environment
    // recovery still rejects C4; E3 the completed extraction survives Host
    // replacement; E4 the original foreground delivery is the direct driver.
    // Decision table: D1=C1+C2+C3 => E1+E3+E4; D2=D1+C4 => E1+E2+E3+E4.
    // Constraint K1: neither rule adopts, deletes, or fabricates durable identity
    // for the legacy root. The test injects authority because runtime-host no
    // longer opens a commit Store.
    let (first, first_managed) = dispatch_test_host(
        SharedHost::new(Arc::new(MemoryHostModel), "stub")
            .with_store_dir(&dir)
            .with_runtime_authority(authority.clone()),
    );
    first
        .run(None, thread, user("remember rust"))
        .await
        .expect("terminal run");
    assert!(
        first
            .session_slots
            .read(thread, |slot| {
                slot.runtime
                    .as_ref()
                    .is_some_and(|context| context.delivery.direct().is_some())
            })
            .unwrap_or(false),
        "D1/E4 foreground delivery must retain DirectAttemptDriver"
    );
    drop(first_managed);
    drop(first);

    // Rebind the frozen resource and reopen the committed thread. Context recovery
    // derives the missing outbox identity from the latest terminal run and inserts
    // the same durable intent normal after-commit delivery would have produced.
    let (second, second_managed) = dispatch_test_host(
        SharedHost::new(Arc::new(MemoryHostModel), "stub")
            .with_store_dir(&dir)
            .with_runtime_authority(authority),
    );
    bind_test_memory(&second, thread, "outbox-store", true);
    let commit = second
        .commit_for_read(thread)
        .await
        .expect("committed Thread reader");
    let thread_id = ThreadId(thread.into());
    let run = commit.latest_run(&thread_id).expect("terminal run record");
    let error = match second.ctx_for(thread, None).await {
        Ok(_) => panic!("D2/E2 legacy root must remain fail-closed"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("legacy sandbox root") && error.message.contains("is not absent"),
        "D2/E2: {error:?}"
    );
    assert!(
        second
            .drain_runtime(std::time::Duration::from_secs(10))
            .await
    );

    let repository = awaken_session_store::SqliteManagedSessionRepository::open(
        &dir.join("sessions.db").to_string_lossy(),
    )
    .unwrap();
    let intent = repository
        .get_extraction(&format!("memory-extraction:{thread}:{}", run.id.0))
        .await
        .unwrap()
        .expect("recovered extraction intent");
    assert_eq!(
        intent.status,
        awaken_ext_memory::MemoryExtractionStatus::Completed
    );
    assert!(
        second
            .memory_stores
            .fs()
            .get_by_path("outbox-store", "/user-prefs.md")
            .await
            .unwrap()
            .is_some(),
        "recovered outbox drives the same governed Memory store"
    );

    drop(second_managed);
    drop(second);
    std::fs::remove_dir_all(dir).ok();
}

/// An explicitly configured Awaken Memory extension selects only its frozen
/// Session binding; that same handle serves extraction + recall, while an
/// ordinary unbound Managed Session sees no store.
#[tokio::test]
async fn managed_memory_is_per_store_and_an_unbound_session_cannot_see_host_memory() {
    use awaken_session_contract::SessionInit;

    let snapshot = crate::config::server_config(
        "agent",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            serde_json::json!({"binding_id": "test-input-0"}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let host = Arc::new(
        SharedHost::new(Arc::new(MemLoopModel), "stub")
            .with_agent_publications(Arc::new(publications)),
    );
    let unbound_store = test_memory_store_id();
    host.memory_stores
        .fs()
        .create(&unbound_store, "/must-not-leak.md", "the user prefers tea")
        .await
        .unwrap();
    install_test_memory_mounter(&host);
    let store_a = test_memory_store_id();
    let store_b = test_memory_store_id();
    let managed = managed_with_resource_source(host.clone());
    let init = |agent: &str, store: Option<&str>| {
        SessionInit {
            workspace_id: host.local_workspace().into(),
            agent_id: agent.into(),
            delegate_ids: Vec::new(),
            tools: None,
            resource_revision: 0,
            resources: effective_resources(
                store
                    .map(|id| TestInput {
                        kind: "memory_store".into(),
                        id: id.into(),
                        mount_path: "/memory".into(),
                        // Workdir cannot OS-enforce read-only mounts, so this integration
                        // path uses a writable mount. The handle-level read-only
                        // invariant is covered separately.
                        access: ResourceAccess::ReadWrite,
                        instructions: None,
                        initial_branch: None,
                        initial_commit: None,
                    })
                    .into_iter()
                    .collect(),
            ),
            model: None,
            runtime: None,
            environment: session_environment(
                awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
        }
    };

    managed
        .install_complete_test_session("managed-write-a", init("agent", Some(&store_a)))
        .await
        .unwrap();
    run_prepared_session(
        &managed,
        "agent",
        "managed-write-a",
        vec![ContentBlock::text("I enjoy tea")],
    )
    .await
    .unwrap();
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    assert!(
        host.memory_stores
            .fs()
            .get_by_path(&store_a, "/beverage-preference.md")
            .await
            .unwrap()
            .is_some(),
        "extraction writes the bound platform store"
    );
    assert!(
        host.memory_stores
            .fs()
            .list(&store_b, "/")
            .await
            .unwrap()
            .is_empty(),
        "a different store remains untouched"
    );

    let reply = |outcome: &awaken_session_contract::StepOutcome| {
        outcome
            .new_messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Assistant)
            .map(|message| block_text(&message.content))
            .unwrap_or_default()
    };
    managed
        .install_complete_test_session("managed-read-a", init("agent", Some(&store_a)))
        .await
        .unwrap();
    let same = run_prepared_session(
        &managed,
        "agent",
        "managed-read-a",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    assert_eq!(reply(&same), "tea", "recall reads the same bound store");

    managed
        .install_complete_test_session("managed-read-b", init("agent", Some(&store_b)))
        .await
        .unwrap();
    let other = run_prepared_session(
        &managed,
        "agent",
        "managed-read-b",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    assert_eq!(reply(&other), "ok", "store B cannot recall store A");

    managed
        .install_complete_test_session("managed-unbound", init("unmanaged-agent", None))
        .await
        .unwrap();
    let unbound = run_prepared_session(
        &managed,
        "unmanaged-agent",
        "managed-unbound",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    assert_eq!(
        reply(&unbound),
        "ok",
        "a Session with no binding cannot see another governed store"
    );
}

/// Cause/effect decision table for a live Memory binding:
///
/// | requested manifest | relation to installed pin | effect |
/// |---|---|---|
/// | prepare blocked by lifecycle | absent | expose neither Environment nor manifest |
/// | durable active generation | absent beside an adopted live Environment | install as cold recovery |
/// | exact replay | equal | idempotent success; retain the live environment |
/// | concurrent exact replays | equal | serialize on the existing Session lifecycle and converge |
/// | empty/different | mutation | fail closed because Memory is create-time only |
///
/// This is the active-active recovery case: a peer may ask the Runtime to replay
/// durable Session truth after the local projection has already been installed.
#[tokio::test]
async fn exact_live_memory_manifest_replay_is_idempotent_but_change_fails_closed() {
    use awaken_session_contract::SessionInit;

    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            std::env::temp_dir().join(format!(
                "awaken-memory-recovery-namespace-{}",
                std::process::id()
            )),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let host = Arc::new(raw_host);
    install_test_memory_mounter(&host);
    let store = test_memory_store_id();
    host.memory_stores
        .fs()
        .create(&store, "/seed.md", "seed")
        .await
        .unwrap();
    let resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store,
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let managed = managed_with_resource_source(host.clone());
    let init = SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: "agent".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: resources.clone(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    };
    let lifecycle = host
        .session_slots
        .update("memory-replay", |slot| slot.lifecycle.clone());
    let lifecycle_guard = lifecycle.lock().await;
    let preparing = tokio::spawn({
        let managed = managed.clone();
        async move {
            managed
                .install_complete_test_session("memory-replay", init)
                .await
        }
    });
    tokio::task::yield_now().await;
    assert!(
        host.session_environment("memory-replay").await.is_none(),
        "preparation cannot expose Environment before the lifecycle owner commits the manifest"
    );
    drop(lifecycle_guard);
    preparing
        .await
        .expect("preparation task")
        .expect("prepare Session");
    run_prepared_session(
        &managed,
        "agent",
        "memory-replay",
        vec![ContentBlock::text("open")],
    )
    .await
    .unwrap();
    let environment = host
        .session_environment("memory-replay")
        .await
        .expect("live environment");

    // Reproduce the active-active recovery state observed in the black-box
    // pressure test: the durable sandbox binding is resident, while this process
    // has not yet projected the authoritative Resource generation.
    host.session_slots.update("memory-replay", |slot| {
        slot.manifest = None;
        slot.resources = Default::default();
        slot.memory_bindings.clear();
        slot.memory = None;
    });
    let replay = resource_transition(
        host.local_workspace(),
        0,
        resources.clone(),
        1,
        resources.clone(),
    );
    managed
        .apply_session_inputs("memory-replay", &replay)
        .await
        .expect("cold durable generation installs beside the adopted Environment");

    let (left, right) = tokio::join!(
        managed.apply_session_inputs("memory-replay", &replay),
        managed.apply_session_inputs("memory-replay", &replay),
    );
    left.expect("first exact durable replay");
    right.expect("concurrent exact durable replay");
    assert!(Arc::ptr_eq(
        &environment,
        &host.session_environment("memory-replay").await.unwrap()
    ));

    let error = managed
        .apply_session_inputs(
            "memory-replay",
            &resource_transition(
                host.local_workspace(),
                1,
                resources.clone(),
                2,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
        )
        .await
        .expect_err("live Memory removal must remain forbidden");
    assert!(error.message.contains("create-time only"));
}

#[tokio::test]
async fn published_agent_memory_config_can_disable_recall_and_extraction() {
    // Cause/effect rule: C1 a bound MemoryStore makes content available; C2 the
    // published parent Agent selects the `memory` plugin with both behavior flags
    // false. C1+C2 -> neither recall context nor terminal extraction (one Agent
    // publication is the authority; the resource carries no parallel policy).

    let snapshot = crate::config::server_config(
        "agent",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()],
        &std::collections::BTreeMap::from([(
            awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
            serde_json::json!({
                "binding_id": "test-input-0",
                "recall_enabled": false,
                "extraction_enabled": false
            }),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let host = Arc::new(
        SharedHost::new(Arc::new(MemLoopModel), "stub")
            .with_agent_publications(Arc::new(publications)),
    );
    install_test_memory_mounter(&host);
    let store = test_memory_store_id();
    host.memory_stores
        .fs()
        .create(&store, "/existing.md", "the user prefers tea")
        .await
        .unwrap();
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("agent", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store.clone(),
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .install_complete_test_session("managed-policy", init)
        .await
        .unwrap();
    let outcome = run_prepared_session(
        &managed,
        "agent",
        "managed-policy",
        vec![ContentBlock::text("What do I prefer?")],
    )
    .await
    .unwrap();
    let reply = outcome
        .new_messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| block_text(&message.content))
        .unwrap_or_default();
    assert_eq!(reply, "ok", "disabled recall does not inject store content");
    assert!(host.drain_runtime(std::time::Duration::from_secs(1)).await);
    assert!(
        host.memory_stores
            .fs()
            .get_by_path(&store, "/beverage-preference.md")
            .await
            .unwrap()
            .is_none(),
        "disabled extraction does not mutate the store"
    );
}

/// The main agent awaits on a `write` (Ask-gated) then finishes on resume; the
/// extractor saves a memory. Proves resume-ended Runs trigger the aux agents.
struct ResumeMemModel;

#[async_trait::async_trait]
impl LlmExecutor for ResumeMemModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let saw_tool = request.messages.iter().any(|m| m.role == Role::Tool);
        // The extractor's own write_memory succeeded (its result text), distinct
        // from the main Run's `write` result that is also in its seeded context.
        let saved_memory = request.messages.iter().any(|m| {
            m.role == Role::Tool
                && m.content.iter().any(|b| match b {
                    ContentBlock::ToolResult { content, .. } => {
                        block_text(content).contains("saved memory")
                    }
                    _ => false,
                })
        });
        let is_extractor = request.messages.iter().any(|m| {
            m.role == Role::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction Agent"),
                    _ => false,
                })
        });
        let output = if is_extractor {
            if saved_memory {
                AssistantOutput::text("extracted")
            } else {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "mw".into(),
                    tool_id: "write_memory".into(),
                    arguments: serde_json::json!({ "name": "resumed", "content": "after-resume" }),
                }])
            }
        } else if saw_tool {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "note.txt", "content": "x" }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn resume_ended_run_triggers_memory_extraction() {
    // Decision rule: resume to Ended, drain background work, and require one
    // extraction over only the new committed messages.
    // Test design. Causes: C1 a resumed Run reaches Ended with new messages;
    // C2 memory extraction is enabled. Effects: E1 terminal resume schedules one
    // extraction over the new committed slice. Constraint/Invariant: extraction
    // follows committed terminal truth, not the caller's resume return. Decision
    // rule: resume to Ended, drain background work, and require one extraction.
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([memory_test_snapshot(
            "assistant",
        )])
        .expect("valid Memory Agent publication");
    let host = Arc::new(
        SharedHost::new(Arc::new(ResumeMemModel), "stub")
            .with_gate_override(write_confirmation_gate())
            .with_agent_publications(Arc::new(publications)),
    );
    install_test_memory_mounter(&host);
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "t-res", "assistant", &store).await;

    // Run 1 awaits on the Ask-gated `write`.
    let r1 = run_prepared_session_messages(
        &managed,
        "assistant",
        "t-res",
        vec![Message::text(MessageId("u1".into()), Role::User, "hi")],
    )
    .await
    .expect("Run 1");
    assert!(
        matches!(r1.state(), RunState::Awaiting),
        "Run should await on write"
    );
    let pending = r1.pending().cloned().expect("a pending tool");

    // RunResume approves the write; the Run now ends and extraction fires.
    let r2 = host
        .resume(
            "t-res",
            &pending.tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("resume");
    assert!(
        matches!(r2.state, RunState::Ended(_)),
        "resume should end the Run"
    );

    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    let saved = host
        .memory_stores
        .fs()
        .get_by_path(&store, "/resumed.md")
        .await
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    assert_eq!(saved, "after-resume");
}

/// The extractor writes a `seen.md` whose content is the non-prompt user texts
/// it was seeded with, so a test can check which messages each extraction saw.
struct CursorModel;

#[async_trait::async_trait]
impl LlmExecutor for CursorModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let is_extractor = request.messages.iter().any(|m| {
            m.role == Role::System
                && m.content.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains("memory extraction Agent"),
                    _ => false,
                })
        });
        if !is_extractor {
            return Ok(ChatResponse {
                output: AssistantOutput::text("ok"),
                usage: None,
                stop_reason: None,
            });
        }
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            return Ok(ChatResponse {
                output: AssistantOutput::text("extracted"),
                usage: None,
                stop_reason: None,
            });
        }
        // Join the user texts it was seeded with, excluding the extraction prompt.
        let seen: Vec<String> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| block_text(&m.content))
            .filter(|t| !t.contains("Extract durable memories"))
            .collect();
        Ok(ChatResponse {
            output: AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w".into(),
                tool_id: "write_memory".into(),
                arguments: serde_json::json!({ "name": "seen", "content": seen.join(",") }),
            }]),
            usage: None,
            stop_reason: None,
        })
    }
}

/// Cause/effect design: C1 Run 1 commits `alpha` and finishes memory extraction;
/// C2 Run 2 then commits `beta` on the same Thread. Effect E1: the advanced
/// extraction cursor supplies only `beta` to the second extraction, so `/seen.md`
/// excludes already-processed `alpha`. Decision rule X1=C1+C2=>E1.
#[tokio::test]
async fn extraction_cursor_only_processes_new_messages() {
    // Constraint/Invariant: the committed extraction cursor is the sole boundary;
    // previously processed messages must never re-enter a later seed. Decision rule:
    // advance the cursor once, append new messages, and require only the
    // suffix effect documented below.
    let host = memory_test_host(Arc::new(CursorModel), "memory-agent");
    install_test_memory_mounter(&host);
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "t-cur", "memory-agent", &store).await;
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    run_prepared_session_messages(&managed, "memory-agent", "t-cur", user("alpha"))
        .await
        .expect("Run 1");
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    run_prepared_session_messages(&managed, "memory-agent", "t-cur", user("beta"))
        .await
        .expect("Run 2");
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);

    // The second extraction saw only "beta" — Run 1's "alpha" was past the cursor.
    let seen = host
        .memory_stores
        .fs()
        .get_by_path(&store, "/seen.md")
        .await
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    assert_eq!(
        seen, "beta",
        "cursor should exclude already-extracted messages"
    );
}

#[tokio::test]
async fn run_end_fires_background_memory_extraction() {
    // Test design. Causes: C1 an ordinary Run reaches Ended; C2 memory extraction
    // is configured. Effects: E1 terminal completion schedules extraction without
    // blocking the Run response; E2 draining background work persists the result.
    // Constraint/Invariant: committed Run end triggers exactly one background job.
    // Decision rule: end one Run, observe prompt return, then drain and require E2.
    let host = memory_test_host(Arc::new(MemoryHostModel), "memory-agent");
    install_test_memory_mounter(&host);
    let managed = managed_with_resource_source(host.clone());
    let store = test_memory_store_id();
    install_complete_memory_test_session(&managed, "t-mem", "memory-agent", &store).await;

    let input = vec![Message::text(
        MessageId("u1".into()),
        Role::User,
        "I really like rust",
    )];
    let result = run_prepared_session_messages(&managed, "memory-agent", "t-mem", input)
        .await
        .expect("Run");
    assert!(
        matches!(result.state(), RunState::Ended(_)),
        "Run should end"
    );

    let drained = host.drain_runtime(std::time::Duration::from_secs(10)).await;
    assert!(drained, "memory extraction should drain");

    let saved = host
        .memory_stores
        .fs()
        .get_by_path(&store, "/user-prefs.md")
        .await
        .unwrap()
        .unwrap()
        .content
        .unwrap();
    assert_eq!(saved, "user likes rust");
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

/// Live mount decision table:
/// | resident tier | requested File access | effect |
/// |---|---|---|
/// | Namespace | read-only | materialize, replace projection, evict context |
/// | Workdir | read-only | reject without changing projection (next test) |
///
/// This rule exercises the admitted branch and proves detach reverses it in the
/// same Session-owned environment.
#[tokio::test]
async fn applying_changed_inputs_rebuilds_the_resource_projection_and_cached_sandbox() {
    // Causes: C1 an active Session's Resource inputs change to a new generation;
    // C2 a cached sandbox/projection exists; C3 the process slot retains a Run
    // claim from the preceding settled attempt; C4 the Resource reservation is
    // authorized by the Session root. Effects: E1 active generation and installed
    // sandbox projection advance as one pair; E2 the reservation persists through
    // the root authority without reading or rewriting the attempt's raw sandbox
    // cache; E3 Create/Adopt claim fencing remains outside this Resource rule.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | R1 | yes | yes | absent | yes | E1 |
    // | R2 | yes | yes | stale | yes | E1+E2 |
    // | R3 | create/adopt | any | stale | n/a | E3; fail closed |
    //
    // This test owns R2. Run-ingress sandbox-binding conformance owns R3.
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            std::env::temp_dir().join(format!(
                "awaken-hot-attach-namespace-{}",
                std::process::id()
            )),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // C0 freezes the ordinary empty generation before the first environment is
    // realized, so C1 below advances one exact durable Resource transition.
    install_complete_test_session_for_realization(
        &managed,
        "t-attach",
        bare_session("agent", host.local_workspace()),
    )
    .await
    .expect("install complete empty Session projection");

    // A blob to mount, and a first Run that builds + caches the Thread's sandbox.
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.local_workspace(),
            "data.txt".into(),
            "text/plain".into(),
            b"hello-attached",
        )
        .await
        .expect("create File")
        .id;
    run_prepared_session_messages(&managed, "agent", "t-attach", user("hi"))
        .await
        .expect("first Run");
    let environment_before = host
        .session_environment("t-attach")
        .await
        .expect("session environment");
    let handle_before = environment_before.handle();
    assert!(
        host.session_slots
            .read("t-attach", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "the first Run caches the Thread's sandbox ctx"
    );
    let before = host.sandbox_spec("t-attach").mounts.len();

    // Model the exact post-settlement residue observed by an idle durable
    // Session. No Dispatch store is installed, so any attempt to reinterpret
    // this cache as current Resource authority fails the test before effects.
    let stale_claim = awaken_run_ingress::RunClaim {
        run_id: RunId("settled-before-live-attach".into()),
        owner: "retired-worker".into(),
        epoch: 1,
    };
    host.session_slots.update("t-attach", |slot| {
        slot.dispatch_claim = Some(stale_claim.clone());
    });

    // Attach a file resource on the LIVE session.
    let res = TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/data.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    };
    let attached = effective_resources(vec![res.clone()]);
    managed
        .apply_session_inputs(
            "t-attach",
            &resource_transition(
                host.local_workspace(),
                0,
                Default::default(),
                1,
                attached.clone(),
            ),
        )
        .await
        .expect("attach");

    // The cached sandbox was evicted (so the next Run rebuilds) ...
    assert!(
        !host
            .session_slots
            .read("t-attach", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "attach evicts the cached ctx so the next Run rebuilds with the mount"
    );
    let handle_after_attach = host
        .session_environment_handle("t-attach")
        .await
        .expect("live attach retains one Session environment");
    assert_eq!(handle_after_attach.sandbox_id, handle_before.sandbox_id);
    assert_eq!(
        handle_after_attach.provider_kind(),
        handle_before.provider_kind()
    );
    assert!(
        handle_after_attach
            .owned_paths()
            .is_some_and(|paths| paths.iter().any(|path| path.ends_with("data.txt"))),
        "live attach is reserved in the durable V2 handle before projection"
    );
    assert_eq!(
        environment_before
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .unwrap(),
        vec![("data.txt".into(), b"hello-attached".to_vec())],
        "the live environment receives the file before attach returns"
    );
    // ... and the spec the next Run will build now carries the mount + its bytes.
    let spec = host.sandbox_spec("t-attach");
    assert_eq!(spec.mounts.len(), before + 1, "one more mount staged");
    let dump = serde_json::to_string(&spec.mounts).expect("mounts serialize");
    assert!(
        dump.contains("data.txt"),
        "mount realized at the resource path: {dump}"
    );
    assert_eq!(
        carried_mount_bytes(spec.mounts.last().unwrap()),
        b"hello-attached"
    );
    assert_eq!(
        host.session_slots
            .read("t-attach", |slot| slot.dispatch_claim.clone())
            .flatten(),
        Some(stale_claim),
        "R2 Resource reservation leaves the attempt delivery cache untouched"
    );
    host.session_slots
        .update("t-attach", |slot| slot.dispatch_claim = None);

    run_prepared_session_messages(&managed, "agent", "t-attach", user("after attach"))
        .await
        .expect("runtime rebuild over retained environment");
    let environment_after = host
        .session_environment("t-attach")
        .await
        .expect("retained environment");
    assert!(Arc::ptr_eq(&environment_before, &environment_after));

    // Detach removes exactly that mount again.
    managed
        .apply_session_inputs(
            "t-attach",
            &resource_transition(host.local_workspace(), 1, attached, 2, Default::default()),
        )
        .await
        .expect("detach");
    let spec = host.sandbox_spec("t-attach");
    assert_eq!(spec.mounts.len(), before, "the mount is dropped on detach");
    assert!(
        !serde_json::to_string(&spec.mounts)
            .unwrap()
            .contains("data.txt")
    );
    assert!(
        environment_after
            .list_workspace_files(".mnt")
            .await
            .unwrap()
            .is_empty(),
        "detach removes the projected file from the same live environment"
    );
    assert_eq!(
        host.session_environment_handle("t-attach").await,
        Some(handle_after_attach),
        "detachment retains conservative path evidence for crash recovery"
    );
}

/// Live invalid-layout decision rule: L1 an installed File generation and
/// resident Namespace environment exist; L2 the next durable generation carries
/// a historical `/repo` Repository. L1+L2 => reject before verifier, Skill/File
/// compilation, projection transaction, or Git; keep the old manifest, mounts,
/// resident bytes, environment handle, and cached Runtime atomically unchanged.
#[tokio::test]
async fn invalid_repository_live_replacement_preserves_the_installed_generation() {
    use awaken_session_contract::SessionRuntime;

    let storage = tempfile::tempdir().unwrap();
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            storage.path().join("sandboxes"),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let host = Arc::new(raw_host);
    let verifier = Arc::new(SequencedRepositoryTransport(AtomicUsize::new(0)));
    let managed = managed_with_resource_source(host.clone())
        .with_repository_binding_verifier(verifier.clone())
        .install_dispatch_session_runtime();
    let file_id = host
        .file_application()
        .expect("File application")
        .create_uploaded_file(
            host.local_workspace(),
            "retained.txt".into(),
            "text/plain".into(),
            b"retained-generation",
        )
        .await
        .unwrap()
        .id;
    let installed = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/retained.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let mut init = bare_session("agent", host.local_workspace());
    init.resources = installed;
    install_complete_test_session_for_realization(&managed, "invalid-live-repository", init)
        .await
        .unwrap();
    run_prepared_session_messages(
        &managed,
        "agent",
        "invalid-live-repository",
        vec![Message::text(
            MessageId("initial-generation".into()),
            Role::User,
            "open the environment",
        )],
    )
    .await
    .unwrap();
    let environment = host
        .session_environment("invalid-live-repository")
        .await
        .expect("resident environment");
    let before_handle = environment.handle();
    let before_manifest = host
        .thread_resource_manifest("invalid-live-repository")
        .expect("installed manifest");
    let before_spec = host.sandbox_spec("invalid-live-repository");
    let before_files = environment
        .list_frozen_mount_files("/mnt/session/uploads")
        .await
        .unwrap();

    let canonical = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: "https://github.com/awaken/invalid-live.git".into(),
        mount_path: "/workspace/repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    // L2 is historical durable input: new commands cannot construct `/repo`,
    // while the canonical replay decoder retains the exact legacy bytes so the
    // Runtime guard can reject them without silently relocating the path.
    let mut durable = serde_json::to_value(canonical).unwrap();
    durable["inputs"][0]["mount_path"] = serde_json::Value::String("/repo".into());
    let invalid = serde_json::from_value(durable).expect("decode historical Resource manifest");
    let error = managed
        .apply_session_inputs(
            "invalid-live-repository",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                invalid,
            ),
        )
        .await
        .expect_err("L2 fails before projection mutation");
    assert!(error.message.contains("/repo"), "L1/L2: {error:?}");
    assert_eq!(verifier.0.load(Ordering::SeqCst), 0, "L1/L2 verifier");
    assert_eq!(
        host.thread_resource_manifest("invalid-live-repository"),
        Some(before_manifest),
        "L1/L2 manifest"
    );
    assert_eq!(
        host.sandbox_spec("invalid-live-repository"),
        before_spec,
        "L1/L2 mounts"
    );
    assert_eq!(
        host.session_environment_handle("invalid-live-repository")
            .await,
        Some(before_handle),
        "L1/L2 environment"
    );
    assert_eq!(
        environment
            .list_frozen_mount_files("/mnt/session/uploads")
            .await
            .unwrap(),
        before_files,
        "L1/L2 physical files"
    );
    assert!(
        host.session_slots
            .read("invalid-live-repository", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "L1/L2 cached Runtime"
    );
    assert!(
        host.thread_repository_activations("invalid-live-repository")
            .is_empty(),
        "L1/L2 Git activation"
    );
}

/// Repository detach decision table on the resident Namespace environment:
/// C1 a create-time Repository is physically realized; C2 the next durable
/// generation omits it; C3 the Session environment remains resident. E1 removes
/// the exact working tree before the mutation returns and E2 preserves the same
/// environment handle. A stale readable checkout is forbidden.
///
/// | Rule | C1 | C2 | C3 | Effect |
/// | RD1  | T  | T  | T  | E1 removed + E2 retained environment |
#[tokio::test]
async fn applying_repository_detach_removes_the_resident_namespace_checkout() {
    let fixture = tempfile::tempdir().unwrap();
    let source = fixture.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&source)
            .status()
            .expect("run git fixture command");
        assert!(status.success(), "git fixture command failed: {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "runtime-host-test@awaken.invalid"]);
    git(&["config", "user.name", "Awaken Runtime Host Test"]);
    std::fs::write(source.join("README.md"), "resident repository").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-q", "-m", "seed"]);

    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            fixture.path().join("sandboxes"),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let repository_path_fidelity = raw_host.session_provider.capabilities().path_fidelity;
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());
    let desired = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: source.to_string_lossy().into_owned(),
        mount_path: "/workspace/live-repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    install_complete_test_session_for_realization(
        &managed,
        "t-repo-detach",
        awaken_session_contract::SessionInit {
            workspace_id: host.local_workspace().into(),
            agent_id: "agent".into(),
            delegate_ids: Vec::new(),
            tools: None,
            resource_revision: 0,
            resources: desired,
            model: None,
            runtime: None,
            environment: session_environment(
                awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
        },
    )
    .await
    .unwrap();
    let realization = run_prepared_session_messages(
        &managed,
        "agent",
        "t-repo-detach",
        vec![Message::text(
            MessageId("repo-before".into()),
            Role::User,
            "observe repository",
        )],
    )
    .await;
    if !repository_path_fidelity {
        let error = match realization {
            Err(error) => error,
            Ok(_) => panic!(
                "a Namespace provider without path fidelity must reject Repository realization"
            ),
        };
        assert!(
            error
                .message
                .contains("one sandbox-absolute workspace path"),
            "RD0 capability rejection must explain the shared-path invariant: {}",
            error.message
        );
        assert!(
            host.session_environment("t-repo-detach").await.is_none(),
            "RD0 rejection happens before a resident environment or checkout is exposed"
        );
        return;
    }
    realization.expect("RD1 path-fidelitous Namespace realizes the Repository");
    let environment = host
        .session_environment("t-repo-detach")
        .await
        .expect("resident environment");
    let before_manifest = host
        .thread_resource_manifest("t-repo-detach")
        .expect("installed manifest");
    let handle = environment.handle();
    let checkout = fixture
        .path()
        .join("sandboxes/t-repo-detach/workspace/live-repo");
    assert_eq!(
        std::fs::read(checkout.join("README.md")).unwrap(),
        b"resident repository",
        "RD1 create-time checkout is physically present"
    );

    managed
        .apply_session_inputs(
            "t-repo-detach",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
        )
        .await
        .expect("detach repository");
    assert!(
        !checkout.exists(),
        "RD1 detached checkout must not remain readable"
    );
    assert_eq!(
        host.session_environment_handle("t-repo-detach").await,
        Some(handle),
        "RD1 mutation retains the one Session environment"
    );
}

/// Causes: a live Workdir environment exists and the replacement manifest adds
/// a read-only File. Constraint: Workdir provides lexical containment but cannot
/// enforce mount immutability. Effect/rule W1: reject before changing the staged
/// manifest, resident files, or cached context.
#[tokio::test]
async fn applying_readonly_file_to_live_workdir_fails_closed_without_partial_projection() {
    // Decision rule: exercise the read-only collision branch and require both the
    // explicit failure and unchanged live projection effects documented below.

    let mut deployment = crate::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = crate::SandboxTier::Local;
    let host = Arc::new(SharedHost::new_with_deployment(
        Arc::new(OkModel),
        "stub",
        deployment,
    ));
    let managed = managed_with_resource_source(host.clone());
    host.run(
        None,
        "t-local-attach",
        vec![Message::text(MessageId("initial".into()), Role::User, "hi")],
    )
    .await
    .expect("first Run");
    let environment = host
        .session_environment("t-local-attach")
        .await
        .expect("live Workdir environment");
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.local_workspace(),
            "data.txt".into(),
            "text/plain".into(),
            b"must-remain-unmounted",
        )
        .await
        .expect("create File")
        .id;
    let before_manifest = host
        .thread_resource_manifest("t-local-attach")
        .unwrap_or_else(|| {
            awaken_session_contract::SessionResourceManifest::at_revision(
                host.local_workspace(),
                0,
                awaken_session_contract::ResolvedSessionResources::default(),
            )
        });
    let before = host.sandbox_spec("t-local-attach");
    let attached = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/data.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .apply_session_inputs(
            "t-local-attach",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                attached,
            ),
        )
        .await
        .expect_err("Workdir cannot admit an official read-only File copy");
    assert!(error.message.contains("does not enforce read-only"));
    assert_eq!(host.sandbox_spec("t-local-attach"), before);
    assert!(
        environment
            .list_workspace_files("mnt/session/uploads")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        host.session_slots
            .read("t-local-attach", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "rejected replacement leaves the cached runtime intact"
    );
}

/// Committed-query cause/effect graph after execution provisioning fails:
/// C1 a frozen File plus a CPU limit is staged; C2 Workdir cannot enforce limits;
/// C3 a side-effect-free reservation projection occurs; C4 no runtime/environment
/// becomes resident; C5 a committed-state GET follows. C1+C2+C3 cause E1 the
/// reservation to reject before any physical effect. C1+C2
/// cause E2 executable Run construction to fail. C4+C5 cause E3 the query to open only
/// committed truth, return the empty page, and leave the execution environment
/// absent instead of retrying the failing provisioning.
///
/// | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
/// |---|---|---|---|---|---|---|
/// | Q1 | T | T | T | T | F | E1 reservation denied/no env |
/// | Q2 | T | T | F | T | F | E2 direct Run denied/no env |
/// | Q3 | T | T | F | T | T | E3 query succeeds/no env |
#[tokio::test]
async fn committed_queries_do_not_provision_a_failed_session_environment() {
    let mut deployment = crate::DeploymentConfig::ephemeral();
    deployment.sandbox_tier = crate::SandboxTier::Local;
    let host = Arc::new(SharedHost::new_with_deployment(
        Arc::new(OkModel),
        "stub",
        deployment,
    ));
    let managed = managed_with_resource_source(host.clone());
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.local_workspace(),
            "query.txt".into(),
            "text/plain".into(),
            b"read-only",
        )
        .await
        .expect("create File")
        .id;
    managed
        .install_complete_test_session(
            "t-query-after-provisioning-denial",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: effective_resources(vec![TestInput {
                    kind: "file".into(),
                    id: file_id,
                    mount_path: "/query.txt".into(),
                    access: ResourceAccess::ReadOnly,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                }]),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({"limits": {"cpu_millis": 1}}),
                ),
            },
        )
        .await
        .expect("stage frozen Session");

    let reservation_error = match host
        .ctx_for_session_reservation("t-query-after-provisioning-denial", Some("assistant"))
        .await
    {
        Ok(_) => panic!("Q1 reservation must reject unsupported limits"),
        Err(error) => error,
    };
    assert!(
        reservation_error.message.contains("resource limits"),
        "Q1: {reservation_error}"
    );
    assert!(
        host.session_environment("t-query-after-provisioning-denial")
            .await
            .is_none(),
        "Q1 reservation preflight has no physical effect"
    );

    let error = match run_prepared_session_messages(
        &managed,
        "assistant",
        "t-query-after-provisioning-denial",
        vec![Message::text(
            MessageId("denied".into()),
            Role::User,
            "must not reach inference",
        )],
    )
    .await
    {
        Ok(_) => panic!("Workdir must reject the read-only File"),
        Err(error) => error,
    };
    assert!(error.message.contains("resource limits"), "Q2: {error}");
    assert!(
        host.session_environment("t-query-after-provisioning-denial")
            .await
            .is_none(),
        "Q2 failed provisioning must not publish an environment"
    );

    let feed = host
        .run_lifecycle_feed("t-query-after-provisioning-denial")
        .await
        .expect("committed query must not retry Sandbox provisioning");
    let page = awaken_agent_contract::RunLifecycleFeed::events_after(
        feed.as_ref(),
        awaken_agent_contract::RunLifecycleCursor(0),
        100,
    )
    .await
    .expect("read empty committed lifecycle page");
    assert!(page.events.is_empty(), "Q3 inference never committed a Run");
    assert!(
        host.session_environment("t-query-after-provisioning-denial")
            .await
            .is_none(),
        "Q3 committed query remains free of environment side effects"
    );
}

/// Cause-effect graph for the sole Environment projection:
///
/// C1 exact frozen network fact is Unrestricted / Allowlist / None (O constraint)
///  -> C2 install one Environment fingerprint
///  -> C3 ignore any retained `sandbox.network` field
///  -> E1 Native spec carries the exact frozen network
///  -> E2 Workdir convenience flag exists iff E1 is restricted.
///
/// | Rule | C1 | sandbox.network | E1 | E2 deny flag |
/// |---|---|---|---|---|
/// | N1 | Unrestricted | none | Unrestricted | F |
/// | N2 | Allowlist | conflicting None | exact Allowlist | T |
/// | N3 | None | conflicting Unrestricted | None | T |
#[tokio::test]
async fn frozen_environment_network_follows_the_decision_table() {
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = managed_with_resource_source(host.clone());
    let denies = |spec: awaken_provisioning_contract::SandboxSpec| spec.deny_tool_egress;
    let rules = [
        (
            "N1",
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            false,
        ),
        (
            "N2",
            awaken_session_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.example".into()],
            },
            serde_json::json!({"network": {"mode": "none"}}),
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            true,
        ),
        (
            "N3",
            awaken_session_contract::SessionNetworkPolicy::None,
            serde_json::json!({"network": {"mode": "unrestricted"}}),
            awaken_provisioning_contract::NetworkPolicy::Unrestricted,
            true,
        ),
    ];
    for (id, network, sandbox, expected_network, expected_denial) in rules {
        let thread = format!("environment-{id}");
        managed
            .install_test_session_init(
                &thread,
                SessionInit {
                    workspace_id: host.local_workspace().into(),
                    agent_id: "a".into(),
                    delegate_ids: Vec::new(),
                    tools: None,
                    resource_revision: 0,
                    resources: Default::default(),
                    model: None,
                    runtime: None,
                    environment: session_environment(network, sandbox),
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        let spec = host.sandbox_spec(&thread);
        assert_eq!(spec.network, expected_network, "{id}");
        assert_eq!(denies(spec), expected_denial, "{id}");
    }
}

/// The frozen Environment reaches the sandbox spec through one projection. Isolation
/// and limits come from the network-free sandbox blob, while the distinct network fact
/// remains authoritative even if a retained blob contains a conflicting legacy field.
#[tokio::test]
async fn install_test_session_init_overlays_the_environment_sandbox_onto_the_spec() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_provisioning_contract::{IsolationClass, NetworkPolicy};
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);

    let init = SessionInit {
        workspace_id: "ws".into(),
        agent_id: "a".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_contract::SessionNetworkPolicy::Allowlist {
                hosts: vec!["api.github.com".into()],
            },
            serde_json::json!({
            "isolation": "namespace",
            "network": { "mode": "unrestricted" },
            "limits": { "cpu_millis": 2000, "memory_bytes": 4294967296u64 }
            }),
        ),
    };
    managed
        .install_complete_test_session("t-sb", init)
        .await
        .unwrap();

    let spec = host.sandbox_spec("t-sb");
    assert_eq!(
        spec.isolation,
        IsolationClass::Namespace,
        "env isolation overlaid"
    );
    assert_eq!(
        spec.network,
        NetworkPolicy::Unrestricted,
        "Workdir does not claim strict allowlist enforcement"
    );
    assert!(
        spec.deny_tool_egress,
        "the Workdir tool wrapper retains the frozen restriction intent"
    );
    assert_eq!(spec.limits.cpu_millis, Some(2000));
    assert_eq!(spec.limits.memory_bytes, Some(4_294_967_296));

    // Workspace/resource cause-effect rules: W1 a nonempty Resource manifest
    // already carries its Workspace through staging; W2 an empty manifest must
    // still retain SessionInit.workspace_id. Both effects select the same
    // workspace-scoped immutable Agent publication; neither may fall back to
    // the process platform workspace.
    //
    // | Rule | resources | Session workspace | retained lookup scope |
    // | W1 | nonempty | ws | ws |
    // | W2 | empty | ws | ws |
    // A session with no override keeps the host default (Workdir, no limits).
    let bare = SessionInit {
        workspace_id: "ws".into(),
        agent_id: "a".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    };
    managed
        .install_complete_test_session("t-bare", bare)
        .await
        .unwrap();
    assert_eq!(
        host.registered_thread_workspace("t-bare").as_deref(),
        Some("ws"),
        "W2"
    );
    let context = host
        .ctx_for("t-bare", None)
        .await
        .expect("W2 runtime context")
        .context();
    assert_eq!(
        context
            .execution_scope
            .as_ref()
            .map(|scope| scope.0.as_str()),
        Some("ws"),
        "W2 Session ownership enters the one attempt context"
    );
    assert_eq!(
        host.sandbox_spec("t-bare").isolation,
        IsolationClass::Workdir
    );
    assert!(!host.sandbox_spec("t-bare").limits.is_set());
}

/// Cause/effect design: C1 eager Session preparation freezes configuration; C2 no
/// Run has requested a context; C3 the first Run requests one. Effects: E1 C1+C2
/// leaves the physical environment absent; E2 C1+C3 materializes it and waits for
/// readiness before execution. Decision table: L1=C1+C2=>E1; L2=C1+C3=>E2.
#[tokio::test]
async fn install_test_session_init_is_lazy_and_first_run_materializes_the_environment() {
    // Constraint/Invariant: Session preparation records intent but only the first
    // executable Run may create the Environment. Decision rule: observe zero live
    // effects after prepare, then exactly one materialization on first Run.
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    managed
        .install_complete_test_session(
            "lazy-environment",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();

    assert!(
        host.session_environment("lazy-environment").await.is_none(),
        "preparation must not create a Hand/Sandbox"
    );
    run_prepared_session_messages(
        &managed,
        "assistant",
        "lazy-environment",
        vec![Message::text(
            MessageId("lazy-user".into()),
            Role::User,
            "hello",
        )],
    )
    .await
    .expect("first Run waits for environment readiness");
    assert!(host.session_environment("lazy-environment").await.is_some());
}

/// Cause/effect graph: C1 the Host is Coordinator-only; C2 the frozen
/// Environment is eager; C3 context construction is needed to serialize a Run;
/// C4 the exact delivered Skill includes filesystem support. C1 dominates C2
/// and assigns C4 realization to the claimed Worker: E1 construct the dispatch
/// context and Skill registry, E2 retain the frozen Environment snapshot, E3
/// allocate and write no physical sandbox. The local-pool materialization row is
/// covered by `replacing_a_manifest_removes_the_old_delivered_skill_tree_immediately`.
///
/// | Rule | Coordinator-only | Provisioning | Filesystem Skill | Context/registry | Physical environment |
/// |---|---|---|---|---|---|
/// | D1 | yes | eager | yes | built | absent |
/// | D2 | no | eager | yes | built | resident + files |
/// | D3 | any | on_tool_use | no | inference only | absent |
#[tokio::test]
async fn coordinator_dispatch_context_never_materializes_an_eager_environment() {
    // Constraint/Invariant: Coordinator-side dispatch composition is effect-free;
    // only the claimed Worker may realize an Environment. Decision rule: build
    // the dispatch context and require the zero-materialization effects below.
    use awaken_session_contract::SessionInit;
    use awaken_skill_store::{SkillBundleFile, SkillVersion, bundle_sha256};
    let mut host = SharedHost::new(Arc::new(OkModel), "stub");
    host.deployment.disable_local_pool = true;
    let host = Arc::new(host);
    install_test_session_application(&host);
    install_test_dispatch_runtime(&host)
        .install_test_session_init(
            "coordinator-dispatch-only",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .expect("D1 installs frozen dispatch facts");
    let files = vec![
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: files\ndescription: inspect files\n---\nRead the bundled guide."
                .to_vec(),
            executable: false,
        },
        SkillBundleFile {
            path: "references/guide.md".into(),
            content: b"guide".to_vec(),
            executable: false,
        },
    ];
    let hash = bundle_sha256(&files);
    host.session_slots
        .update("coordinator-dispatch-only", |slot| {
            slot.skills = Some(vec![SkillVersion {
                id: "skver_files_1".into(),
                skill_id: "files".into(),
                version: 1,
                name: "files".into(),
                description: "inspect files".into(),
                directory: "/skills/files".into(),
                bundle_sha256: hash,
                files,
                created_unix_nanos: 0,
            }]);
        });

    let context = host
        .ctx_for("coordinator-dispatch-only", Some("assistant"))
        .await
        .expect("D1 builds a sandbox-free dispatch context");
    assert!(context.skill_registry.is_some(), "D1/E1");
    assert!(
        host.session_environment("coordinator-dispatch-only")
            .await
            .is_none(),
        "D1/E3"
    );
    assert!(
        host.session_slots
            .read("coordinator-dispatch-only", |slot| slot
                .environment_snapshot
                .is_some())
            .unwrap_or(false),
        "D1/E2"
    );
}

/// Cause/effect graph: C1 the Host is Coordinator-only; C2 immutable
/// provisioning is BackendOwned; C3 placement is Local or Worker; C4 the
/// frozen layout is valid or structurally conflicting; C5 the caller installs
/// Dispatch or physical Realization facts; C6 a trusted-host provider is
/// present. Effects: E1 a Worker admission/Dispatch performs structural
/// validation without requesting the Coordinator's provider; E2 a structural
/// conflict is rejected before frozen facts; E3 Local/physical validation
/// requires the exact provider; E4 a claimed Worker with that provider passes;
/// E5 no Coordinator environment is created. Create and persisted Resource
/// updates share the prospective port, so P1-P3 own both entry points.
///
/// | Rule | Placement/stage | Provider | Layout | Effect |
/// |---|---|---|---|---|
/// | P1 | Worker prospective | absent | valid | E1+E5 |
/// | P2 | Worker prospective | absent | conflict | E2+E5 |
/// | P3 | Local prospective | absent | valid | E3 |
/// | P4 | Worker Dispatch | absent | valid | E1+E5 |
/// | P5 | Worker Dispatch | absent | conflict | E2+E5 |
/// | P6 | Worker physical install | absent | valid | E3 |
/// | P7 | Worker physical install | present | valid | E4 |
/// | B1 | Coordinator context | absent | valid | E1+E5 |
/// | B2 | local context | absent | valid | E3 |
///
/// The contract-owned path kernel remains the sole structural authority; this
/// test owns only the placement-to-validation-scope decision.
#[tokio::test]
async fn coordinator_defers_backend_owned_environment_to_the_claimed_worker() {
    let mut host = SharedHost::new(Arc::new(OkModel), "stub");
    host.deployment.disable_local_pool = true;
    let host = Arc::new(host);
    let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
        awaken_runtime_contract::resolved::ModelBinding::new("local", "", "acp:codex"),
        awaken_runtime_contract::CredentialRef {
            id: "codex-login".into(),
            revision: 1,
        },
        awaken_runtime_contract::resolved::BackendModelSelection::Default,
        "codex-test",
        "sha256:codex-test",
        Default::default(),
    )
    .expect("coherent backend-owned candidate");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("local-codex")
        .resolved_model(candidate.clone())
        .build();

    let context = host
        .ctx_for_snapshot(
            "coordinator-backend-owned",
            Some("local-codex"),
            Some(snapshot.clone()),
        )
        .await
        .expect("B1 builds the dispatch context without a trusted-host provider");

    assert!(context.env.is_none(), "B1/E2");
    assert!(
        host.session_environment("coordinator-backend-owned")
            .await
            .is_none(),
        "B1/E2"
    );

    let model_override = || awaken_session_contract::SessionModelOverride {
        publication: Some(Box::new(awaken_session_contract::SessionModelPublication {
            primary: candidate.clone(),
            candidates: Vec::new(),
        })),
        inference: Default::default(),
    };
    let mount = || awaken_provisioning_contract::MountRequirement {
        mount_id: "project".into(),
        source: awaken_provisioning_contract::MountSource::Inline {
            contents: "project".into(),
        },
        mount_path: "/workspace/project".into(),
        access: awaken_provisioning_contract::MountAccess::ReadOnly,
        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
        required: true,
    };
    let repository_binding = awaken_resource_contract::InputBinding {
        binding_id: awaken_resource_contract::BindingId::from("repo"),
        target: awaken_resource_contract::InputResourceId::Repository(
            awaken_resource_contract::RepositoryId::from("repo"),
        ),
        mount_path: "/workspace/project/repository".into(),
        access: awaken_resource_contract::ResourceAccess::ReadWrite,
        instructions: None,
    };
    let layout =
        |runtime_placement, mounts, resources| awaken_session_contract::SessionSandboxLayout {
            workspace_id: host.local_workspace().into(),
            agent_id: "local-codex".into(),
            agent_revision: None,
            runtime_placement,
            model_override: Some(model_override()),
            runtime: Some("acp:codex".into()),
            mounts,
            env: Vec::new(),
            environment: on_tool_use_environment(),
            resources,
        };
    let managed = crate::ManagedHost::new(host.clone());
    managed
        .validate_session_sandbox_layout(
            "worker-prospective-valid",
            &layout(
                awaken_session_contract::SessionRuntimePlacement::Worker,
                Vec::new(),
                Vec::new(),
            ),
        )
        .expect("P1 Worker admission needs no Coordinator provider");
    let conflict = managed
        .validate_session_sandbox_layout(
            "worker-prospective-conflict",
            &layout(
                awaken_session_contract::SessionRuntimePlacement::Worker,
                vec![mount()],
                vec![repository_binding],
            ),
        )
        .expect_err("P2 structural conflict remains fail-closed");
    assert!(conflict.to_string().contains("overlaps"), "P2: {conflict}");
    let local_error = managed
        .validate_session_sandbox_layout(
            "local-prospective-valid",
            &layout(
                awaken_session_contract::SessionRuntimePlacement::Local,
                Vec::new(),
                Vec::new(),
            ),
        )
        .expect_err("P3 Local admission requires its exact provider");
    assert!(
        local_error
            .to_string()
            .contains("BackendOwned provisioning requires a trusted-host Session provider"),
        "P3: {local_error}"
    );

    let projection = |mounts, resources| {
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: on_tool_use_environment(),
                runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
                mcp_authoring: Default::default(),
                agent_id: "local-codex".into(),
                agent_revision: None,
                model: candidate.binding().model_ref.clone(),
                model_override: Some(model_override()),
                runtime: Some(candidate.binding().backend_ref.clone()),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts,
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        );
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: host.local_workspace().into(),
            revision: awaken_session_contract::SessionRevision(1),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 0,
            resources,
            previous_resource_manifest: Some(
                awaken_session_contract::SessionResourceManifest::new(
                    host.local_workspace(),
                    awaken_session_contract::ResolvedSessionResources::default(),
                ),
            ),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
        }
    };
    host.install_dispatch_frozen_session_projection(
        "worker-dispatch-valid",
        projection(
            Vec::new(),
            awaken_session_contract::ResolvedSessionResources::default(),
        ),
    )
    .await
    .expect("P4 Worker Dispatch needs no Coordinator provider");
    assert!(
        host.session_environment("worker-dispatch-valid")
            .await
            .is_none(),
        "P4/E5"
    );
    let dispatch_conflict = host
        .install_dispatch_frozen_session_projection(
            "worker-dispatch-conflict",
            projection(
                vec![mount()],
                effective_resources(vec![TestInput {
                    kind: "github_repository".into(),
                    id: "https://example.invalid/repo.git".into(),
                    mount_path: "/workspace/project/repository".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadWrite,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                }]),
            ),
        )
        .await
        .expect_err("P5 Dispatch rejects a structural conflict");
    assert!(
        dispatch_conflict.message.contains("overlaps"),
        "P5: {dispatch_conflict:?}"
    );
    assert!(
        host.session_slots
            .read("worker-dispatch-conflict", |slot| slot.baseline.is_none())
            .unwrap_or(true),
        "P5/E2"
    );

    let physical_without_provider = SharedHost::new(Arc::new(OkModel), "stub");
    let realization_error = physical_without_provider
        .install_frozen_session_projection(
            "worker-realization-missing-provider",
            projection(
                Vec::new(),
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
            None,
            true,
            None,
        )
        .await
        .expect_err("P6 physical install requires the exact provider");
    assert!(
        realization_error
            .message
            .contains("BackendOwned provisioning requires a trusted-host Session provider"),
        "P6: {realization_error:?}"
    );
    let mut physical = SharedHost::new(Arc::new(OkModel), "stub");
    physical.backend_owned_session_provider = Some(
        crate::session_environment::SessionEnvironmentProvider::workdir_with_agent_stderr(
            std::env::temp_dir().join("awaken-worker-layout-provider"),
            false,
        ),
    );
    physical
        .install_frozen_session_projection(
            "worker-realization-valid",
            projection(
                Vec::new(),
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
            None,
            true,
            None,
        )
        .await
        .expect("P7 claimed Worker validates its exact trusted provider");

    let local = SharedHost::new(Arc::new(OkModel), "stub");
    let error = match local
        .ctx_for_snapshot("local-backend-owned", Some("local-codex"), Some(snapshot))
        .await
    {
        Ok(_) => panic!("B2 rejects execution without a trusted-host provider"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("BackendOwned provisioning requires a trusted-host Session provider"),
        "B2: {error}"
    );
    assert!(
        local
            .session_environment("local-backend-owned")
            .await
            .is_none(),
        "B2 environment"
    );
}

/// L1: `on_tool_use` means inference alone must not allocate a Sandbox.
#[tokio::test]
async fn on_tool_use_text_only_run_keeps_the_environment_absent() {
    // Test design. Causes: C1 a text-only Run invokes no environment-requiring
    // tool. Effects: E1 no Environment is created or persisted. Constraint/
    // Invariant: tool demand, not Run existence, owns lazy materialization.
    // Decision rule: complete the text-only partition and require E1.
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    managed
        .install_test_session_init(
            "deferred-text",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    run_prepared_session_messages(
        &managed,
        "assistant",
        "deferred-text",
        vec![Message::text(MessageId("u1".into()), Role::User, "hello")],
    )
    .await
    .unwrap();
    assert!(host.session_environment("deferred-text").await.is_none());
}

/// Delegation/provisioning cause-effect graph: C1=`on_tool_use`; C2=the exact
/// publication contains a delegate; C3=this Host executes locally; C4 a Managed
/// Session has its required coordination authority. Effects:
/// E1=plain inference without C2 stays deferred; E2=C1+C2+C3 creates one
/// Session Environment before the delegation service is exposed; E3=a
/// Coordinator-only Host remains environment-free because the claimed Worker
/// owns E2; E4=!C4 fails closed before any environment mutation; E5 a second
/// local coordination binding is rejected instead of replacing the first.
/// Decision rows L1, L9, and D1 cover E1, E2, and E3 respectively. L9 first
/// exercises !C4=>E4, then C4=>E2, and C4+duplicate=>E5.
#[tokio::test]
async fn on_tool_use_published_delegate_forces_one_eager_environment() {
    // Constraint/Invariant: the published delegate requirement is frozen before
    // execution and may materialize exactly one Environment. Decision rule:
    // execute the delegate-demand partition and require one eager realization.
    use awaken_runtime_contract::StaticPublishedAgentSnapshots;
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use awaken_runtime_contract::snapshot::AgentId;
    use awaken_session_contract::SessionInit;

    let child = awaken_runtime_contract::ExecutableAgentSnapshot::builder("child")
        .model(test_model_binding())
        .build();
    let parent = awaken_runtime_contract::ExecutableAgentSnapshot::builder("parent")
        .model(test_model_binding())
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("child".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let publications = StaticPublishedAgentSnapshots::try_new([parent, child])
        .expect("one authoritative publication catalog");
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications)),
    );
    let managed = install_test_dispatch_runtime(&host);
    managed
        .install_complete_test_session(
            "deferred-delegate",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "parent".into(),
                delegate_ids: vec!["child".into()],
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("prepare exact publication");

    let missing = match host.ctx_for("deferred-delegate", Some("parent")).await {
        Err(error) => error,
        Ok(_) => panic!("L9/E4 missing coordination authority must fail closed"),
    };
    assert!(
        missing
            .to_string()
            .contains("no Session application authority"),
        "L9/E4: {missing}"
    );
    assert!(
        host.session_environment("deferred-delegate")
            .await
            .is_none(),
        "L9/E4 no environment side effect"
    );

    let coordination: Arc<dyn awaken_session_contract::SessionAgentCoordination> =
        Arc::new(RejectingSessionAgentCoordination);
    managed
        .install_agent_coordination_application(Arc::downgrade(&coordination))
        .expect("L9 one coordination application");
    assert_eq!(
        managed.install_agent_coordination_application(Arc::downgrade(&coordination)),
        Err(crate::AgentCoordinationInstallError::AlreadyInstalled),
        "L9/E5"
    );

    let context = host
        .ctx_for("deferred-delegate", Some("parent"))
        .await
        .expect("L9 delegate context");
    assert!(context.env.is_some(), "L9/E2 runtime environment");
    assert!(
        host.session_environment("deferred-delegate")
            .await
            .is_some(),
        "L9/E2 single Session owner"
    );
}

#[tokio::test]
async fn model_request_gate_follows_the_session_dispatch_decision_table() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_session_contract::SessionInit;

    // Cause/effect graph: C1 is the canonical `session_dispatch` projection;
    // C2 is an installed Session application endpoint. E1 installs the one
    // per-request gate; E2 omits it; E3 rejects construction before execution.
    // Direct protocol Threads never gain C1 merely because the process also
    // serves Managed Sessions. Managed primary and child Threads both carry C1.
    //
    // | Rule | C1 | C2 | Effect |
    // | G1   | no | no | E2     |
    // | G2   | no | yes| E2     |
    // | G3   | yes| yes| E1     |
    // | G4   | yes| no | E3     |
    // | G5   | inherited child of G3 | yes | E1 with the same authority |
    let direct = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let _direct_runtime = install_test_dispatch_runtime(&direct);
    let direct_context = direct.ctx_for("gate-direct-absent", None).await.unwrap();
    assert!(
        direct_context.attempt_context.model_request_gate.is_none(),
        "G1"
    );

    let coordination: Arc<dyn awaken_session_contract::SessionAgentCoordination> =
        Arc::new(RejectingSessionAgentCoordination);
    let direct_with_endpoint = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_dispatch_runtime(&direct_with_endpoint)
        .install_agent_coordination_application(Arc::downgrade(&coordination))
        .unwrap();
    let direct_context = direct_with_endpoint
        .ctx_for("gate-direct-present", None)
        .await
        .unwrap();
    assert!(
        direct_context.attempt_context.model_request_gate.is_none(),
        "G2"
    );

    let session_init = || SessionInit {
        workspace_id: "default".into(),
        agent_id: "assistant".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: on_tool_use_environment(),
    };
    let managed = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed_runtime = install_test_dispatch_runtime(&managed);
    managed_runtime
        .install_test_session_init("gate-managed-present", session_init())
        .await
        .unwrap();
    managed_runtime
        .install_agent_coordination_application(Arc::downgrade(&coordination))
        .unwrap();
    let managed_context = managed.ctx_for("gate-managed-present", None).await.unwrap();
    assert!(
        managed_context.attempt_context.model_request_gate.is_some(),
        "G3"
    );
    let managed_gate = managed_context
        .attempt_context
        .model_request_gate
        .as_ref()
        .expect("G5 root gate")
        .clone();
    let managed_child = managed_context.attempt_context.for_child_run();
    assert!(
        Arc::ptr_eq(
            managed_child
                .model_request_gate
                .as_ref()
                .expect("G5 child gate"),
            &managed_gate,
        ),
        "G5 a synchronous child inherits the exact Session request authority"
    );

    let missing = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_dispatch_runtime(&missing)
        .install_test_session_init("gate-managed-absent", session_init())
        .await
        .unwrap();
    let error = match missing.ctx_for("gate-managed-absent", None).await {
        Ok(_) => panic!("G4 must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("no Session application authority"),
        "G4: {error}"
    );
}

struct BrainSkillModel;

#[async_trait::async_trait]
impl LlmExecutor for BrainSkillModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                call_id: "skills-1".into(),
                tool_id: awaken_ext_skills::SKILL_LIST_TOOL_ID.into(),
                arguments: serde_json::json!({}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// Published-Skill Managed-filesystem cause/effect graph. C1 the immutable
/// Agent publication selects a Skill; C2 the Session slot contains its exact
/// verified bundle; C3 Runtime owns a Sandbox. Effects: E1 Skill metadata/path
/// is model-visible without its full body; E2 `SKILL.md` is materialized; E3 no
/// semantic Skill tools create a parallel activation path.
///
/// | Rule | C1 selected | C2 delivered | Effect |
/// |---|---|---|---|
/// | S1 | yes | yes | E1 + E2 + E3 |
/// | S2 | yes | no | fail closed before inference/materialization/read |
/// | S3 | no | yes | no unselected Skill projection/read |
#[tokio::test]
async fn published_agent_receives_managed_filesystem_skill_discovery() {
    use awaken_runtime_contract::StaticPublishedAgentSnapshots;
    use awaken_runtime_contract::agent_bindings::AgentBindings;
    use awaken_skill_store::{SkillBundleFile, SkillVersion, bundle_sha256};

    #[derive(Clone, Default)]
    struct ToolFaceRecorder(Arc<Mutex<Vec<ChatRequest>>>);

    #[async_trait::async_trait]
    impl LlmExecutor for ToolFaceRecorder {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.0.lock().unwrap().push(request.clone());
            let system = request
                .messages
                .iter()
                .filter(|message| message.role == Role::System)
                .map(|message| block_text(&message.content))
                .collect::<Vec<_>>()
                .join("\n");
            let output = if request
                .messages
                .last()
                .is_some_and(|message| message.role == Role::Tool)
            {
                AssistantOutput::text("used managed filesystem Skill")
            } else if system.contains(".skills/release-signal/SKILL.md") {
                AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                    call_id: "read-managed-skill".into(),
                    tool_id: "read".into(),
                    arguments: serde_json::json!({
                        "path": ".skills/release-signal/SKILL.md"
                    }),
                }])
            } else {
                AssistantOutput::text("no selected managed Skill")
            };
            Ok(ChatResponse {
                output,
                usage: None,
                stop_reason: None,
            })
        }
    }

    let selected = "release-signal";
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("published-skill")
        .model(test_model_binding())
        .tools(crate::config::advertised_tools(
            &HashSet::new(),
            &HashSet::new(),
            &[],
        ))
        .agent_bindings(AgentBindings {
            skills: vec![awaken_agent_contract::AgentSkillBinding::custom(selected)],
            toolsets: vec![awaken_agent_contract::ToolsetPolicy {
                source: awaken_agent_contract::ToolsetSource::Agent,
                default: awaken_agent_contract::ToolExecutionPolicy::default(),
                overrides: Vec::new(),
            }],
            ..Default::default()
        })
        .build();
    let publications = Arc::new(
        StaticPublishedAgentSnapshots::try_new([snapshot.clone()])
            .expect("one immutable published Agent"),
    );
    let recorder = ToolFaceRecorder::default();
    let observed = recorder.0.clone();
    let host = Arc::new(
        SharedHost::new(Arc::new(recorder), "stub").with_agent_publications(publications.clone()),
    );
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        "published-skill-thread",
        host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &snapshot,
    )
    .await;
    let files = vec![SkillBundleFile {
        path: "SKILL.md".into(),
        content: b"---\nname: release-signal\ndescription: release\n---\nSay READY.".to_vec(),
        executable: false,
    }];
    host.session_slots.update("published-skill-thread", |slot| {
        slot.skills = Some(vec![SkillVersion {
            id: "skver-release-signal-1".into(),
            skill_id: selected.into(),
            version: 1,
            name: selected.into(),
            description: "release".into(),
            directory: "/skills/release-signal".into(),
            bundle_sha256: bundle_sha256(&files),
            files,
            created_unix_nanos: 0,
        }]);
    });

    run_prepared_session_messages(
        &managed,
        "published-skill",
        "published-skill-thread",
        vec![Message::text(
            MessageId("published-skill-user".into()),
            Role::User,
            "Release signal",
        )],
    )
    .await
    .expect("published Skill run");

    {
        let requests = observed.lock().unwrap();
        let request = requests.first().expect("initial model request");
        let tools = &request.tools;
        assert!(
            !tools
                .iter()
                .any(|tool| tool.id == awaken_ext_skills::SKILL_LIST_TOOL_ID),
            "S1/E3 list_skills is absent"
        );
        assert!(
            !tools
                .iter()
                .any(|tool| tool.id == awaken_ext_skills::SKILL_TOOL_ID),
            "S1/E3 Skill is absent"
        );
        let system = request
            .messages
            .iter()
            .filter(|message| message.role == Role::System)
            .map(|message| block_text(&message.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(system.contains("release-signal"), "S1/E1 name");
        assert!(
            system.contains(".skills/release-signal/SKILL.md"),
            "S1/E1 path"
        );
        assert!(
            !system.contains("Say READY"),
            "S1/E1 body remains on demand"
        );
        assert!(
            requests
                .last()
                .expect("post-read model request")
                .messages
                .iter()
                .filter(|message| message.role == Role::Tool)
                .map(|message| block_text(&message.content))
                .any(|content| content.contains("Say READY")),
            "S1/E1 the advertised path is readable through the real Session executor"
        );
    }
    assert!(
        host.session_environment("published-skill-thread")
            .await
            .expect("S1 sandbox")
            .scan_skill_dir(crate::skills::DELIVERED_SKILLS_SUBDIR)
            .unwrap()
            .iter()
            .any(|file| file.id == selected),
        "S1/E2"
    );

    let missing_recorder = ToolFaceRecorder::default();
    let missing_observed = missing_recorder.0.clone();
    let missing_host = Arc::new(
        SharedHost::new(Arc::new(missing_recorder), "stub").with_agent_publications(publications),
    );
    install_test_session_application(&missing_host);
    let missing_managed = install_test_dispatch_runtime(&missing_host);
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &missing_managed,
        "selected-without-delivery",
        missing_host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &snapshot,
    )
    .await;
    let missing = run_prepared_session_messages(
        &missing_managed,
        "published-skill",
        "selected-without-delivery",
        vec![Message::text(
            MessageId("selected-without-delivery-user".into()),
            Role::User,
            "Release signal",
        )],
    )
    .await
    .expect_err("S2 selected Skill without frozen bytes fails closed");
    assert!(
        missing.message.contains("no frozen version bytes"),
        "S2: {missing}"
    );
    let missing_system = missing_observed
        .lock()
        .unwrap()
        .iter()
        .flat_map(|request| request.messages.iter())
        .filter(|message| message.role == Role::System)
        .map(|message| block_text(&message.content))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!missing_system.contains("release-signal"), "S2");
    assert!(
        missing_host
            .session_environment("selected-without-delivery")
            .await
            .is_none(),
        "S2 fails before exposing a partial Environment"
    );

    let unselected_snapshot =
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("unselected-skill")
            .model(test_model_binding())
            .agent_bindings(AgentBindings {
                toolsets: vec![awaken_agent_contract::ToolsetPolicy {
                    source: awaken_agent_contract::ToolsetSource::Agent,
                    default: awaken_agent_contract::ToolExecutionPolicy::default(),
                    overrides: Vec::new(),
                }],
                ..Default::default()
            })
            .build();
    let unselected_publications =
        StaticPublishedAgentSnapshots::try_new([unselected_snapshot.clone()])
            .expect("one immutable unselected Agent");
    let unselected_recorder = ToolFaceRecorder::default();
    let unselected_observed = unselected_recorder.0.clone();
    let unselected_host = Arc::new(
        SharedHost::new(Arc::new(unselected_recorder), "stub")
            .with_agent_publications(Arc::new(unselected_publications)),
    );
    install_test_session_application(&unselected_host);
    let unselected_managed = install_test_dispatch_runtime(&unselected_host);
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &unselected_managed,
        "unselected-delivery",
        unselected_host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &unselected_snapshot,
    )
    .await;
    unselected_host
        .session_slots
        .update("unselected-delivery", |slot| {
            slot.skills = host
                .session_slots
                .read("published-skill-thread", |slot| slot.skills.clone())
                .flatten();
        });
    run_prepared_session_messages(
        &unselected_managed,
        "unselected-skill",
        "unselected-delivery",
        vec![Message::text(
            MessageId("unselected-delivery-user".into()),
            Role::User,
            "Release signal",
        )],
    )
    .await
    .expect("S3 unselected delivery stays unavailable");
    let unselected_system = unselected_observed
        .lock()
        .unwrap()
        .iter()
        .flat_map(|request| request.messages.iter())
        .filter(|message| message.role == Role::System)
        .map(|message| block_text(&message.content))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!unselected_system.contains("release-signal"), "S3");
    assert!(
        unselected_host
            .session_environment("unselected-delivery")
            .await
            .expect("S3 sandbox")
            .scan_skill_dir(crate::skills::DELIVERED_SKILLS_SUBDIR)
            .unwrap()
            .is_empty(),
        "S3"
    );
}

struct HandReadModel;

#[async_trait::async_trait]
impl LlmExecutor for HandReadModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let output = if request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                call_id: "read-1".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "missing.txt"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// L2: the direct-session compatibility adapter executes an instruction-only
/// Skill in the Brain without awakening Hand.
#[tokio::test]
async fn direct_on_tool_use_brain_skill_adapter_keeps_the_environment_absent() {
    // Test design. Causes: C1 a direct (non-Managed) caller selects a Brain-owned
    // Skill; C2 it disables every filesystem tool; C3 Environment provisioning
    // is OnToolUse. Effects: E1 the compatibility `list_skills` adapter reaches
    // the canonical registry; E2 it returns the catalog; E3 Hand remains absent.
    // Rule L2: !Managed+C1+C2+C3 => E1+E2+E3. Constraint: the adapter consumes
    // the one registry; it neither creates a second catalog nor grants Hand.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    let host = Arc::new(
        SharedHost::new(Arc::new(BrainSkillModel), "stub").with_skills(vec![
            awaken_ext_skills::SkillSpec::new("think", "Think", "reason", "Think carefully."),
        ]),
    );
    let thread = "direct-deferred-brain";
    host.install_environment_projection(thread, &on_tool_use_environment())
        .unwrap();
    let deferred: Arc<dyn awaken_runtime_contract::tool::ToolExecutor> = Arc::new(
        crate::lazy_sandbox::DeferredSandboxExecutor::new(Arc::downgrade(&host), thread),
    );
    host.session_slots.update(thread, |slot| {
        slot.tools = Some(awaken_session_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
                overrides: vec![ToolPolicyOverride::new(
                    "web_fetch",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                )],
            }],
            client_tools: Vec::new(),
        });
        slot.deferred_executor = Some(deferred);
    });

    let result = host
        .run(
            None,
            thread,
            vec![Message::text(MessageId("u2".into()), Role::User, "skills")],
        )
        .await
        .unwrap();
    let tool_results = result
        .new_messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| block_text(&message.content))
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 1, "L2/E1 one semantic Skill result");
    assert!(
        tool_results[0].contains("\"skills\"") && !tool_results[0].contains("unknown tool"),
        "L2/E2 canonical Skill catalog: {tool_results:?}"
    );
    assert!(host.session_environment(thread).await.is_none(), "L2/E3");
}

#[tokio::test]
async fn managed_session_ignores_unbound_host_skill_sources() {
    // Managed repository Skill rule R7. C1 the caller is Managed; C2 its frozen
    // Resource manifest selects no Skill; C3 a filesystem-requiring host-static Skill
    // exists; C4 a durable catalog cache also contains an unbound Skill; C5 all
    // filesystem tools are disabled. Effect E1 no Skill descriptor/executor or
    // prompt is projected; E2 Hand stays absent.
    // R7=C1+C2+C3+C4+C5=>E1+E2.
    // Counter-rule A2=!C1+instruction-only static Skill+C4=>the direct semantic
    // adapter is covered by L2. This distinguishes a compatibility input from the Managed
    // Binding/version/bytes authority instead of synchronizing both catalogs.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
    };
    use awaken_session_contract::SessionInit;

    let host_static = awaken_ext_skills::SkillSpec {
        environment: awaken_ext_skills::SkillEnvironment::Filesystem,
        dir: Some("skills/think".into()),
        ..awaken_ext_skills::SkillSpec::new("think", "Think", "reason", "Think carefully.")
    };
    let storage = tempfile::tempdir().expect("Managed Skill authority test storage");
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_skills(vec![host_static])
            .with_skill_store(storage.path().join("skills")),
    );
    let durable = frozen_skill_version(
        "cached",
        "Cached",
        "unbound durable catalog entry",
        "Never project without a binding.",
        &[],
    );
    host.skills
        .create(
            awaken_skill_store::SkillDefinition {
                id: durable.skill_id.clone(),
                workspace_id: host.local_workspace().into(),
                display_title: None,
                latest_version: durable.version,
                last_version: durable.version,
                timestamps: Default::default(),
            },
            durable,
        )
        .await
        .expect("durable Skill store configured")
        .expect("cache unbound Skill version");
    assert_eq!(
        host.skills.managed_ids_in(host.local_workspace()),
        vec!["cached".to_string()],
        "A1/C4 proves the live catalog contains an otherwise advertisable Skill"
    );
    install_test_session_application(&host);
    install_test_dispatch_runtime(&host)
        .install_test_session_init(
            "managed-no-frozen-skill",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: Some(awaken_session_contract::SessionToolConfiguration {
                    toolsets: vec![ToolsetPolicy {
                        source: ToolsetSource::Agent,
                        default: ToolExecutionPolicy {
                            enabled: false,
                            permission: ToolPermissionRequirement::AlwaysAllow,
                        },
                        overrides: Vec::new(),
                    }],
                    client_tools: Vec::new(),
                }),
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    let context = host
        .ctx_for("managed-no-frozen-skill", Some("assistant"))
        .await
        .expect("A1 builds from the empty frozen Skill projection");
    assert!(
        context
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .all(|descriptor| {
                descriptor.id != awaken_ext_skills::SKILL_LIST_TOOL_ID
                    && descriptor.id != awaken_ext_skills::SKILL_TOOL_ID
            }),
        "A1/E1 no semantic Skill compatibility surface"
    );
    assert_eq!(
        host.session_slots
            .read("managed-no-frozen-skill", |slot| slot.skill_prompt.clone())
            .flatten(),
        None,
        "A1/E1 no unbound filesystem Skill prompt"
    );
    assert!(
        host.session_environment("managed-no-frozen-skill")
            .await
            .is_none(),
        "A1/E2"
    );
}

/// L3: the Runtime's per-tool target routing sends a Sandbox tool through the
/// deferred Hand; the invoking Run blocks until materialization completes.
#[tokio::test]
async fn on_tool_use_runtime_hand_call_materializes_before_tool_execution() {
    // Test design. Causes: C1 a Runtime Hand tool is selected on a lazy Session.
    // Effects: E1 Environment materialization completes before the tool effect;
    // E2 exactly one Environment is persisted. Constraint/Invariant: placement
    // precedes Hand execution. Decision rule: exercise C1 and assert E1 ordering/E2.
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(HandReadModel), "stub"));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    managed
        .install_complete_test_session(
            "deferred-runtime-hand",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    run_prepared_session_messages(
        &managed,
        "assistant",
        "deferred-runtime-hand",
        vec![Message::text(MessageId("u3".into()), Role::User, "read")],
    )
    .await
    .unwrap();
    assert!(
        host.session_environment("deferred-runtime-hand")
            .await
            .is_some()
    );
}

/// L7: a capability that needs filesystem state defeats `on_tool_use`; context
/// construction must eagerly provide the real Environment rather than expose a
/// broken `${SKILL_DIR}`.
#[tokio::test]
async fn on_tool_use_filesystem_skill_forces_an_eager_environment() {
    // Test design. Causes: C1 a Managed Resource projection contains an exact
    // frozen Skill version with a supporting file. Effects: E1 the Environment
    // is realized before Skill execution. Constraint/Invariant: filesystem
    // demand comes from frozen version bytes, never a host-static compatibility
    // spec, and cannot run against an absent or partial sandbox. Decision rule:
    // execute C1 and require one eager materialization.
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    install_complete_test_session_for_realization(
        &managed,
        "deferred-filesystem-skill",
        SessionInit {
            workspace_id: host.local_workspace().into(),
            agent_id: "assistant".into(),
            delegate_ids: Vec::new(),
            tools: None,
            resource_revision: 0,
            resources: Default::default(),
            model: None,
            runtime: None,
            environment: on_tool_use_environment(),
        },
    )
    .await
    .unwrap();
    host.session_slots
        .update("deferred-filesystem-skill", |slot| {
            slot.skills = Some(vec![frozen_skill_version(
                "files",
                "Files",
                "inspect files",
                "Read the guide.",
                &[("references/guide.md", "guide")],
            )]);
        });

    let ctx = host
        .ctx_for("deferred-filesystem-skill", Some("assistant"))
        .await
        .unwrap();
    assert!(ctx.env.is_some());
    assert!(
        host.session_environment("deferred-filesystem-skill")
            .await
            .is_some()
    );
}

/// Managed-filesystem reservation cause/effect graph: C1 the Session permits
/// `read`; C2 its Resource projection contains an exact frozen instruction-only
/// Skill version; C3 reservation must remain side-effect free; C4 the
/// claimed/local execution context is built afterward.
/// Effects: E1 C1+C2+C3 projects the stable `SKILL.md` path without creating an
/// Environment; E2 C1+C2+C4 eagerly creates the Environment and materializes
/// that exact path. Constraint: reservation and execution share one Skill
/// projection; semantic tools are never a fallback for a missing provisional
/// directory.
///
/// | Rule | read | selected Skill | phase | Environment | path/body |
/// |---|---|---|---|---|---|
/// | L9 | yes | yes | reservation | absent | path only |
/// | L10 | yes | yes | execution | present | same path + body |
///
/// M3 in `skill_delivery_profile_uses_one_managed_filesystem_projection` covers
/// the Managed read-disabled rejection. L2 is direct compatibility only.
#[tokio::test]
async fn managed_filesystem_skill_path_survives_deferred_reservation() {
    use awaken_session_contract::SessionInit;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let managed = install_test_dispatch_runtime(&host);
    install_complete_test_session_for_realization(
        &managed,
        "deferred-managed-filesystem-skill",
        SessionInit {
            workspace_id: host.local_workspace().into(),
            agent_id: "assistant".into(),
            delegate_ids: Vec::new(),
            tools: None,
            resource_revision: 0,
            resources: Default::default(),
            model: None,
            runtime: None,
            environment: on_tool_use_environment(),
        },
    )
    .await
    .expect("prepare lazy Session");
    host.session_slots
        .update("deferred-managed-filesystem-skill", |slot| {
            slot.skills = Some(vec![frozen_skill_version(
                "release-signal",
                "Release signal",
                "release safely",
                "RESERVATION-SKILL-BODY",
                &[],
            )]);
        });

    let provisional = host
        .ctx_for_session_reservation("deferred-managed-filesystem-skill", Some("assistant"))
        .await
        .expect("L9 provisional reservation context");
    assert!(
        provisional.env.is_none(),
        "L9/E1 no reservation side effect"
    );
    let provisional_prompt = host
        .session_slots
        .read("deferred-managed-filesystem-skill", |slot| {
            slot.skill_prompt.clone()
        })
        .flatten()
        .expect("L9/E1 projected Skill metadata");
    assert!(
        provisional_prompt.contains(".skills/release-signal/SKILL.md"),
        "L9/E1 stable path: {provisional_prompt}"
    );
    assert_eq!(
        provisional
            .config
            .resolved_spec
            .plugin_config
            .agent
            .skills
            .iter()
            .map(|skill| skill.skill_id.as_str())
            .collect::<Vec<_>>(),
        vec!["release-signal"],
        "L9/E1 generated dispatch snapshot freezes the selected catalog"
    );

    host.evict_session_for_rebuild("deferred-managed-filesystem-skill")
        .await;
    let executable = host
        .ctx_for_snapshot(
            "deferred-managed-filesystem-skill",
            Some("assistant"),
            Some(provisional.config.clone()),
        )
        .await
        .expect("L10 claimed-style executable context");
    let environment = executable.env.as_ref().expect("L10/E2 eager Environment");
    let files = environment
        .scan_skill_dir(crate::skills::DELIVERED_SKILLS_SUBDIR)
        .unwrap();
    let materialized = files
        .iter()
        .find(|file| file.id == "release-signal")
        .expect("L10/E2 exact Skill path materialized");
    assert!(materialized.content.contains("RESERVATION-SKILL-BODY"));
    assert_eq!(
        host.session_slots
            .read("deferred-managed-filesystem-skill", |slot| {
                slot.skill_prompt.clone()
            })
            .flatten()
            .as_deref(),
        Some(provisional_prompt.as_str()),
        "L10/E2 execution retains the reservation path"
    );
}

/// Legacy delivered-Skill deferral cause/effect decision table. Causes: C1 the
/// Environment is `on_tool_use`; C2 the legacy resource manifest has no frozen
/// Skill selection; C3 a cold-process durable catalog contains a Skill
/// with a support file. The cold catalog is not a frozen Session selection:
/// C1+C2+C3 therefore retains deferral, creates no Environment, and materializes
/// no ambient Skill bytes. L1/L2 above cover the selected rows.
#[tokio::test]
async fn on_tool_use_legacy_delivered_filesystem_skill_forces_an_eager_environment() {
    use awaken_skill_store::{SkillBundleFile, SkillDefinition, SkillVersion, bundle_sha256};

    let storage = tempfile::tempdir().expect("storage");
    let skill_store = storage.path().join("skills");
    let catalog_host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_skill_store(skill_store.clone()));
    let workspace = catalog_host.local_workspace().to_string();
    let files = vec![
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: files\ndescription: inspect files\n---\nRead the bundled guide."
                .to_vec(),
            executable: false,
        },
        SkillBundleFile {
            path: "references/guide.md".into(),
            content: b"guide".to_vec(),
            executable: false,
        },
    ];
    let hash = bundle_sha256(&files);
    catalog_host
        .skills
        .create(
            SkillDefinition {
                id: "files".into(),
                workspace_id: workspace.clone(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            SkillVersion {
                id: "skver_files_1".into(),
                skill_id: "files".into(),
                version: 1,
                name: "files".into(),
                description: "inspect files".into(),
                directory: "/skills/files".into(),
                bundle_sha256: hash,
                files,
                created_unix_nanos: 0,
            },
        )
        .await
        .expect("durable SkillStore")
        .expect("create Skill");
    drop(catalog_host);

    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_skill_store(skill_store)
            .with_store_dir(storage.path()),
    );
    assert!(
        host.skills.cache_snapshot_in(&workspace).is_empty(),
        "L8 starts from a cold process cache"
    );
    let managed = managed_with_resource_source(host.clone());
    managed
        .install_complete_test_session(
            "deferred-legacy-delivered-skill",
            awaken_session_contract::SessionInit {
                workspace_id: workspace,
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("L8 cold legacy projection");

    let context = host
        .ctx_for("deferred-legacy-delivered-skill", Some("assistant"))
        .await
        .expect("L8 context");
    assert!(
        context.env.is_none(),
        "L8 unselected catalog remains deferred"
    );
    assert!(
        host.session_environment("deferred-legacy-delivered-skill")
            .await
            .is_none(),
        "L8 no ambient Skill materialization"
    );
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
        Option<Arc<Mutex<Option<awaken_session_contract::SessionEnvironmentState>>>>,
}

struct MovingRealizationFenceSink {
    authorize_calls: AtomicUsize,
    calls: AtomicUsize,
    accepted_epoch: AtomicU64,
}

fn test_committed_environment(
    receipt: awaken_session_contract::SessionEnvironmentReceipt,
) -> awaken_session_contract::SessionEnvironmentState {
    let generation = awaken_session_contract::SandboxGeneration::new(
        &receipt.session_id,
        1,
        u64::MAX,
        "test-environment",
        "test-image",
    );
    awaken_session_contract::SessionEnvironmentState::Resident {
        binding: receipt.binding,
        effect_id: Some(receipt.effect_id),
        generation: Some(generation),
        idle_since_unix_ms: None,
    }
}

#[derive(Default)]
struct ResponseLossBindingSink {
    authorize_calls: AtomicUsize,
    persist_calls: AtomicUsize,
    committed: std::sync::Mutex<Option<(String, String)>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for ResponseLossBindingSink {
    async fn authorize(
        &self,
        intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_contract::RunError,
    > {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((effect_id, binding)) = self.committed.lock().unwrap().as_ref()
            && effect_id == intent.effect_id()
        {
            Ok(
                awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                    binding: binding.clone(),
                },
            )
        } else {
            Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized)
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        self.persist_calls.fetch_add(1, Ordering::SeqCst);
        let mut committed = self.committed.lock().unwrap();
        if committed.as_ref() == Some(&(receipt.effect_id.clone(), receipt.binding.clone())) {
            return Ok(test_committed_environment(receipt));
        }
        *committed = Some((receipt.effect_id, receipt.binding));
        Err(awaken_session_contract::RunError::unavailable(
            "root CAS response was lost",
        ))
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for MovingRealizationFenceSink {
    async fn authorize(
        &self,
        intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_contract::RunError,
    > {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        let asserted_epoch = intent.realization().map_or(0, |lease| lease.epoch);
        if asserted_epoch == self.accepted_epoch.load(Ordering::SeqCst) {
            Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized)
        } else {
            Err(awaken_session_contract::RunError::unavailable_classified(
                "session_realization_stale",
                "a newer exact realization fence owns the Session",
            ))
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let asserted_epoch = receipt.realization.as_ref().map_or(0, |lease| lease.epoch);
        if asserted_epoch == self.accepted_epoch.load(Ordering::SeqCst) {
            Ok(test_committed_environment(receipt))
        } else {
            Err(awaken_session_contract::RunError::unavailable_classified(
                "session_realization_stale",
                "a newer exact realization fence owns the Session",
            ))
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for BindingOrderSink {
    async fn authorize(
        &self,
        intent: &awaken_session_contract::SessionEnvironmentEffectIntent,
    ) -> Result<
        awaken_session_contract::SessionEnvironmentEffectAuthorization,
        awaken_session_contract::RunError,
    > {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .owned_session_id
            .as_deref()
            .is_some_and(|owned| owned != intent.session_id())
        {
            Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned)
        } else if self.require_realization && intent.realization().is_none() {
            Err(awaken_session_contract::RunError::unavailable_classified(
                "session_realization_stale",
                "replacement realization is not installed",
            ))
        } else {
            Ok(awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized)
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<awaken_session_contract::SessionEnvironmentState, awaken_session_contract::RunError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let host = self.host.upgrade().expect("host remains live");
        self.observed_before_publish.store(
            host.session_environment(&receipt.session_id)
                .await
                .is_none(),
            Ordering::SeqCst,
        );
        if self.fail {
            Err(awaken_session_contract::RunError::unavailable(
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

// Immediate binding decision table: resident -> reuse without a write; absent +
// concurrent callers -> one lifecycle owner; pre-effect rejection -> zero
// provider/receipt effects; successful CAS -> persist before publish; ambiguous
// persistence -> retain the exact hidden Candidate and retry the same Store
// port; definite failure remains retryable with that Candidate and publishes
// nothing. Repository tests own CAS/idempotence at the aggregate.
#[tokio::test]
async fn new_environment_binding_commits_once_before_concurrent_contexts_can_use_it() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    install_test_dispatch_runtime(&host).install_environment_binding_sink(sink.clone());

    let (left, right) = tokio::join!(
        host.ctx_for("binding-order", None),
        host.ctx_for("binding-order", None)
    );
    left.unwrap();
    right.unwrap();
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert!(sink.observed_before_publish.load(Ordering::SeqCst));
    assert!(host.session_environment("binding-order").await.is_some());
}

#[tokio::test]
async fn root_response_loss_is_read_back_without_recreating_the_environment() {
    // Cause/effect table: C1 provider creation succeeds; C2 root CAS commits;
    // C3 its response is lost; C4 exact authorize readback returns the committed
    // binding. R1 C1+C2+C3 retains the hidden Candidate and returns retryable;
    // R2=R1+C4 retries the same idempotent persist, publishes the Store-read
    // identity with no second provider effect, and later context lookup is pure
    // slot reuse.
    use awaken_session_contract::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(ResponseLossBindingSink::default());
    install_test_dispatch_runtime(&host).install_environment_binding_sink(sink.clone());

    let first_error = match host.ctx_for("binding-response-loss", None).await {
        Err(error) => error,
        Ok(_) => panic!("R1 lost response must remain retryable"),
    };
    assert_eq!(first_error.kind, HostErrorKind::Unavailable, "R1");
    assert!(
        host.session_environment("binding-response-loss")
            .await
            .is_none(),
        "R1 hidden Candidate is not published"
    );
    let candidate = host
        .prepared_session_environment("binding-response-loss")
        .expect("R1 exact hidden Candidate");
    let first = host
        .ctx_for("binding-response-loss", None)
        .await
        .expect("R2 exact retry reads back committed identity");
    let first_handle = first.env.as_ref().expect("R1 Environment").handle();
    assert_eq!(
        first_handle,
        candidate.environment.handle(),
        "R2 no recreate"
    );
    let second = host
        .ctx_for("binding-response-loss", None)
        .await
        .expect("R1 slot replay");
    assert!(Arc::ptr_eq(&first, &second), "R1 no context recreation");
    assert_eq!(
        second.env.as_ref().expect("R1 Environment").handle(),
        first_handle,
        "R1 immutable physical identity"
    );
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2, "R1+R2");
    assert_eq!(sink.persist_calls.load(Ordering::SeqCst), 2, "R1+R2");
}

#[tokio::test]
async fn already_applied_create_adopts_under_the_existing_lifecycle_lock() {
    // Cause/effect table: C1 root already committed this exact Create effect;
    // C2 the process-local slot is cold; C3 the physical handle is a Ready V2
    // realization created from the complete frozen projection and its exact
    // provider-effective spec; C4 the committed intent carries that projection's
    // immutable Environment fingerprint. R1 C1+C2+C3+C4 adopts and publishes
    // under the one lifecycle lock without a second create, receipt mutation,
    // or recursive lock acquisition; one idempotent Store read returns the
    // aggregate-generated Resident identity. The shared fixture owns
    // projection/spec/fence construction; this row varies only response loss.
    use awaken_session_contract::SessionRuntime;
    let storage = tempfile::tempdir().unwrap();
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path()));
    install_test_session_application(&host);
    let thread = "binding-already-applied";
    let activation =
        crate::host::worker_resolver::test_support::test_activation(thread, "binding-applied");
    let projection =
        crate::host::worker_resolver::test_support::empty_frozen_projection_for_snapshot(
            host.local_workspace(),
            crate::host::worker_resolver::test_support::eager_environment(),
            &activation.snapshot,
        );
    let binding = crate::host::worker_resolver::test_support::available_local_environment_binding(
        &host,
        thread,
        &projection,
    )
    .await;
    let intent = awaken_session_contract::SessionEnvironmentEffectIntent::new(
        thread,
        awaken_session_contract::SessionEnvironmentEffectKind::Create,
        None,
    )
    .for_environment(projection.baseline.environment.config_fingerprint.0.clone());
    let sink = Arc::new(ResponseLossBindingSink {
        committed: std::sync::Mutex::new(Some((intent.effect_id().to_string(), binding.clone()))),
        ..Default::default()
    });
    install_test_dispatch_runtime(&host).install_environment_binding_sink(sink.clone());

    let context = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        host.ctx_for(thread, None),
    )
    .await
    .expect("R1 lifecycle owner must not re-enter its mutex")
    .expect("R1 exact committed binding is adopted");
    assert_eq!(
        serde_json::to_string(&context.env.as_ref().expect("R1 Environment").handle()).unwrap(),
        binding,
        "R1 root identity"
    );
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1, "R1");
    assert_eq!(
        sink.persist_calls.load(Ordering::SeqCst),
        1,
        "R1 Store-read identity"
    );
}

#[tokio::test]
async fn dispatch_store_topology_keeps_one_physical_execution_owner() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the Runtime authority is ephemeral or durable; C2
    // this process owns local execution or is coordinator-only. Effects: E1 one
    // local pool drains accepted ordinary/background child Runs; E2 a
    // coordinator-only process never starts a competing claimer. Durability is a
    // persistence axis, not an execution-placement gate: otherwise an ephemeral
    // Managed Session can persist a coordinated child in its authority queue with
    // no Worker able to claim it.
    //
    // | Rule | Authority | Local owner | Effect |
    // |---|---|---|---|
    // | T1 | ephemeral | yes | E1 |
    // | T2 | durable injected | yes | E1 |
    // | T3 | durable injected | no | E2 |
    let ephemeral = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    assert!(!ephemeral.deployment.durable, "T1 precondition");
    ephemeral.ensure_dispatch_pool();
    assert!(ephemeral.dispatch_pool.get().is_some(), "T1/E1");

    let local_store =
        Arc::new(awaken_run_ingress::AnyDispatchStore::open_sqlite(":memory:").unwrap());
    let local =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(local_store));
    assert!(local.runs_local_dispatch_pool(), "local");
    local.ensure_dispatch_pool();
    assert!(local.dispatch_pool.get().is_some(), "T2/E1");

    let coordinator_store =
        Arc::new(awaken_run_ingress::AnyDispatchStore::open_sqlite(":memory:").unwrap());
    let coordinator = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_coordinator_dispatch_store(coordinator_store),
    );
    assert!(!coordinator.runs_local_dispatch_pool(), "coordinator");
    coordinator.ensure_dispatch_pool();
    assert!(coordinator.dispatch_pool.get().is_none(), "T3/E2");
}

/// Restart authorization cause/effect graph: C1 the replacement realization
/// lease is absent/present. R1 !C1 rejects before provider or receipt I/O with a
/// retryable stale code; R2 C1 authorizes, persists once, then publishes. The
/// second attempt reloads current root authority instead of relabeling an effect
/// created under the rejected lease.
#[tokio::test]
async fn recovered_environment_waits_for_replacement_realization_before_publish() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: true,
        owned_session_id: None,
        committed_environment: None,
    });
    install_test_dispatch_runtime(&host).install_environment_binding_sink(sink.clone());

    let error = match host.ctx_for("binding-restart", None).await {
        Ok(_) => panic!("R1 missing realization reached provider I/O"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Unavailable, "R1");
    assert_eq!(error.code, "session_realization_stale", "R1");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1, "R1");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 0, "R1");
    assert!(
        host.session_environment("binding-restart").await.is_none(),
        "R1"
    );
    host.install_session_realization_lease(
        "binding-restart",
        awaken_session_contract::SessionRealizationLease {
            owner: "local-worker".into(),
            runtime_incarnation: "replacement".into(),
            epoch: 2,
            expires_at_unix_ms: u64::MAX,
        },
    );
    host.ctx_for("binding-restart", None).await.expect("R2");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2, "R2");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1, "R2");
    assert!(host.session_environment("binding-restart").await.is_some());
}

/// Multi-renewal FMECA: C1 no lease, C2 stale epoch 2, C3 accepted epoch 3.
/// E1 each stale attempt stops at the one pre-effect authorization; E2 only C3
/// reaches provider/receipt publication. The three attempts prove retryability
/// without reusing a substrate produced under an older epoch.
#[tokio::test]
async fn environment_binding_catches_up_across_multiple_realization_fences() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(MovingRealizationFenceSink {
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        accepted_epoch: AtomicU64::new(3),
    });
    install_test_dispatch_runtime(&host).install_environment_binding_sink(sink.clone());

    let first = match host.ctx_for("binding-moving-fence", None).await {
        Ok(_) => panic!("C1 reached provider I/O"),
        Err(error) => error,
    };
    assert_eq!(first.kind, HostErrorKind::Unavailable, "E1");
    for epoch in [2, 3] {
        host.install_session_realization_lease(
            "binding-moving-fence",
            awaken_session_contract::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "worker/incarnation".into(),
                epoch,
                expires_at_unix_ms: u64::MAX,
            },
        );
        let result = host.ctx_for("binding-moving-fence", None).await;
        if epoch == 2 {
            let error = match result {
                Ok(_) => panic!("C2 reached provider I/O"),
                Err(error) => error,
            };
            assert_eq!(error.kind, HostErrorKind::Unavailable, "E1");
        } else {
            result.expect("C3 commits");
        }
    }

    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 3, "E1/E2");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1, "E2");
    assert!(
        host.session_environment("binding-moving-fence")
            .await
            .is_some()
    );
}

/// Durable-binding decision table: no binding + no resident Environment permits
/// first creation; exact binding + adopted/resident permits reuse (covered by the
/// recovery E2E); exact binding + neither rejects ordinary execution before
/// provider creation, while the Coordinator-only reservation boundary freezes an
/// environment-free dispatch context and leaves adoption to the claimed Worker.
///
/// | Rule | durable binding | local adoption | context purpose | Effect |
/// | B1 | present | absent | execute | reject, no substitute |
/// | B2 | present | absent | reserve dispatch | env=None, preserve binding |
#[tokio::test]
async fn missing_durable_environment_adoption_is_deferred_only_for_reservation() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    host.install_session_environment_owner_projection(
        "binding-corrupt",
        "workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: "opaque".into(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .expect("project durable expectation");
    let error = match host.ctx_for("binding-corrupt", None).await {
        Ok(_) => panic!("missing adoption must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("was not adopted"), "B1");
    assert!(
        host.session_environment("binding-corrupt").await.is_none(),
        "B1"
    );

    let reserved = host
        .ctx_for_session_reservation("binding-corrupt", None)
        .await
        .expect("B2 Coordinator-only projection does not adopt Worker substrate");
    assert!(reserved.env.is_none(), "B2");
    assert_eq!(
        host.durable_session_environment_binding("binding-corrupt")
            .as_deref(),
        Some("opaque"),
        "B2 durable identity is retained",
    );
    assert!(
        host.session_environment("binding-corrupt").await.is_none(),
        "B2 no substitute or local adoption",
    );
}

#[tokio::test]
async fn binding_commit_failure_is_retryable_and_never_publishes_the_environment() {
    // Definite-failure decision table: C1 authorize succeeds; C2 persistence
    // fails before durable commit; C3 the caller retries. R1=C1+C2 retains one
    // hidden Candidate and returns Unavailable; R2=R1+C3 reuses that exact
    // Candidate, repeats only authorize/persist, and still publishes nothing.
    use awaken_session_contract::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    install_test_dispatch_runtime(&host).install_environment_binding_sink(sink.clone());

    let error = match host.ctx_for("binding-failure", None).await {
        Ok(_) => panic!("binding failure must not publish a context"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("binding store unavailable"));
    assert_eq!(error.kind, HostErrorKind::Unavailable);
    let candidate = host
        .prepared_session_environment("binding-failure")
        .expect("R1 exact hidden Candidate");
    let retry = match host.ctx_for("binding-failure", None).await {
        Err(error) => error,
        Ok(_) => panic!("a definite failure must retry the retained Candidate"),
    };
    assert!(retry.to_string().contains("binding store unavailable"));
    let retried = host
        .prepared_session_environment("binding-failure")
        .expect("R2 retained Candidate");
    assert!(candidate.exact_matches(&retried), "R2 no provider recreate");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2);
    assert!(host.session_environment("binding-failure").await.is_none());
}

/// L3/L4: concurrent first Hand calls join one creator and only observe the
/// environment after its durable binding has succeeded.
#[tokio::test]
async fn on_tool_use_concurrent_hand_calls_create_and_persist_one_environment() {
    // Test design. Causes: C1 concurrent Hand calls race on one lazy Session.
    // Effects: E1 one creation wins; E2 all calls reuse the same persisted
    // Environment. Constraint/Invariant: Session realization has one CAS owner,
    // never one sandbox per caller. Decision rule: release concurrent C1 and
    // require creation cardinality one plus shared identity.
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    let managed = install_test_dispatch_runtime(&host);
    managed.install_environment_binding_sink(sink.clone());
    managed
        .install_complete_test_session(
            "deferred-hand",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();
    let ctx = host.ctx_for("deferred-hand", None).await.unwrap();
    assert!(ctx.env.is_none());
    let hand = ctx
        .attempt_context
        .tool_executor
        .as_ref()
        .expect("deferred hand")
        .clone();
    let left = awaken_runtime_contract::tool::ToolCall {
        call_id: "read-left".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"path": "missing-left"}),
    };
    let right = awaken_runtime_contract::tool::ToolCall {
        call_id: "read-right".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"path": "missing-right"}),
    };

    let (_left, _right) = tokio::join!(hand.invoke(&left), hand.invoke(&right));
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert!(sink.observed_before_publish.load(Ordering::SeqCst));
    assert!(host.session_environment("deferred-hand").await.is_some());
}

/// L5: persistence failure fails closed; no deferred environment becomes visible.
#[tokio::test]
async fn on_tool_use_binding_failure_never_publishes_the_environment() {
    // Test design. Causes: C1 Environment binding fails after realization starts;
    // C2 the next tool call retries. Effects: E1 tool execution fails; E2 no
    // Environment is published as active; E3 C2 reuses the exact hidden Candidate.
    // Constraint/Invariant: publication follows complete binding and cannot expose
    // a partial sandbox. Rules L5=C1=>E1+E2; L6=L5+C2=>E1+E2+E3.
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    let managed = install_test_dispatch_runtime(&host);
    managed.install_environment_binding_sink(sink.clone());
    managed
        .install_complete_test_session(
            "deferred-failure",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();
    let ctx = host.ctx_for("deferred-failure", None).await.unwrap();
    let hand = ctx
        .attempt_context
        .tool_executor
        .as_ref()
        .expect("deferred hand")
        .clone();
    let call = awaken_runtime_contract::tool::ToolCall {
        call_id: "read-failure".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({"path": "missing"}),
    };
    let error = hand.invoke(&call).await.expect_err("binding failure");
    assert!(error.to_string().contains("binding store unavailable"));
    let candidate = host
        .prepared_session_environment("deferred-failure")
        .expect("L5 hidden Candidate");
    let retry = hand
        .invoke(&call)
        .await
        .expect_err("the next tool attempt retries the retained Candidate");
    assert!(retry.to_string().contains("binding store unavailable"));
    let retried = host
        .prepared_session_environment("deferred-failure")
        .expect("L6 retained Candidate");
    assert!(candidate.exact_matches(&retried), "L6/E3");
    assert_eq!(sink.authorize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2);
    assert!(host.session_environment("deferred-failure").await.is_none());
}

/// Runtime stages the effective resources supplied by the Session control plane. It
/// does not need the Agent binding repository, which keeps remote workers stateless.
#[tokio::test]
async fn install_test_session_init_mounts_an_effective_memory_resource() {
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a memory store with known bytes. The control plane has already resolved
    // this resource into the SessionInit passed across the runtime boundary.
    let store_id = test_memory_store_id();
    host.memory_stores
        .fs()
        .create(&store_id, "/facts.md", "the secret code is BANANA-42")
        .await
        .expect("seed memory");
    let managed = managed_with_resource_source(host.clone());

    let bare = |agent: &str| SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: agent.into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: effective_resources(
            (agent == "a")
                .then(|| TestInput {
                    kind: "memory_store".into(),
                    id: store_id.clone(),
                    mount_path: "/mnt/memory".into(),
                    access: ResourceAccess::ReadWrite,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                })
                .into_iter()
                .collect(),
        ),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    };

    // An effective Session input mounts without an authoring repository on the host.
    managed
        .install_test_session_init("t-bound", bare("a"))
        .await
        .unwrap();
    let dump = serde_json::to_string(&host.sandbox_spec("t-bound").mounts).unwrap();
    assert!(
        dump.contains("mnt/memory"),
        "the bound memory store is mounted at its path: {dump}"
    );
    assert_eq!(
        memory_mount_store_id(&host.sandbox_spec("t-bound").mounts[0]),
        store_id
    );

    let mut read_only = bare("a");
    let binding_id = read_only.resources.inputs()[0].binding_id.clone();
    read_only.resources = read_only
        .resources
        .update_input(&binding_id, |input| {
            input.access = awaken_resource_contract::ResourceAccess::ReadOnly;
        })
        .unwrap();
    managed
        .install_test_session_init("t-read-only", read_only)
        .await
        .unwrap();
    assert_eq!(
        host.sandbox_spec("t-read-only").mounts[0].access,
        awaken_provisioning_contract::MountAccess::ReadOnly
    );

    // An empty effective input set mounts nothing.
    managed
        .install_test_session_init("t-unbound", bare("no-bindings"))
        .await
        .unwrap();
    let empty = serde_json::to_string(&host.sandbox_spec("t-unbound").mounts).unwrap();
    assert!(
        !empty.contains("BANANA-42"),
        "an unbound agent mounts nothing extra: {empty}"
    );
}

#[tokio::test]
async fn filesystem_free_session_projects_multiple_memories_as_semantic_tools() {
    // Cause/effect graph: C1 OnToolUse Environment; C2 the exact Agent toolset
    // disables every filesystem member but allows web ids without publishing a
    // web plugin; C3 two frozen
    // MemoryStore bindings with different access; C4 write has absent/current/
    // stale CAS hash. Effects: E1 no Sandbox or Memory mount; E2 prompt names
    // every binding and semantic-tool protocol; E3 tools route only to the
    // explicit binding; E4 create/current-CAS succeed, stale/read-only writes
    // fail without clobbering; E5 a policy-only WebFetch does not manufacture a
    // capability or materialize a Sandbox. Decision rules M1=C1+C2+C3 -> E1..E3;
    // M2=C4 absent -> create; M3=C4 current -> update; M4=C4 stale -> reject;
    // M5=read-only binding -> reject; M6 a cold Runtime rebuild retains the
    // physical Session's frozen delivery and tool projection; M7=C1+C2+
    // unpublished WebFetch -> E5. Constraint K1 a rebuild may replace only
    // process-local Runtime state, never the Session-owned delivery decision.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };
    use awaken_runtime_contract::tool::ToolCall;

    struct SemanticMemoryModel {
        fetch_url: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl LlmExecutor for SemanticMemoryModel {
        async fn infer(
            &self,
            request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            let last = request.messages.last().expect("semantic Memory message");
            let output = if last.role == Role::Tool {
                AssistantOutput::text(format!(
                    "runtime-semantic-tool:{}",
                    block_text(&last.content)
                ))
            } else if let Some(url) = self.fetch_url.lock().unwrap().clone() {
                AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                    call_id: "runtime-web-fetch".into(),
                    tool_id: "web_fetch".into(),
                    arguments: serde_json::json!({ "url": url }),
                }])
            } else {
                AssistantOutput::from_tool_calls(vec![awaken_runtime_contract::llm::ToolCall {
                    call_id: "runtime-read-memory".into(),
                    tool_id: "read_memory".into(),
                    arguments: serde_json::json!({
                        "binding": "test-input-1",
                        "path": "/preference.md"
                    }),
                }])
            };
            Ok(ChatResponse {
                output,
                usage: None,
                stop_reason: None,
            })
        }
    }

    let model = Arc::new(SemanticMemoryModel {
        fetch_url: Mutex::new(None),
    });
    let host = Arc::new(SharedHost::new(model.clone(), "stub"));
    let managed = managed_with_resource_source(host.clone());
    let store_a = "semantic-store-a";
    let store_b = "semantic-store-b";
    host.memory_repository()
        .create(store_b, "/preference.md", "from-b")
        .await
        .unwrap();

    let mut init = bare_session("assistant", host.local_workspace());
    init.environment = on_tool_use_environment();
    init.tools = Some(awaken_session_contract::SessionToolConfiguration {
        toolsets: vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: false,
                permission: ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: ["web_fetch", "web_search"]
                .into_iter()
                .map(|name| {
                    ToolPolicyOverride::new(
                        name,
                        ToolExecutionPolicy {
                            enabled: true,
                            permission: ToolPermissionRequirement::AlwaysAllow,
                        },
                    )
                })
                .collect(),
        }],
        client_tools: Vec::new(),
    });
    init.resources = effective_resources(vec![
        TestInput {
            kind: "memory_store".into(),
            id: store_a.into(),
            mount_path: "memory/a".into(),
            access: ResourceAccess::ReadWrite,
            instructions: Some("Use for durable project facts.".into()),
            initial_branch: None,
            initial_commit: None,
        },
        TestInput {
            kind: "memory_store".into(),
            id: store_b.into(),
            mount_path: "memory/b".into(),
            access: ResourceAccess::ReadOnly,
            instructions: Some("Use for user preferences.".into()),
            initial_branch: None,
            initial_commit: None,
        },
    ]);
    managed
        .install_test_session_init("semantic-memory", init)
        .await
        .unwrap();
    let context = host
        .ctx_for("semantic-memory", Some("assistant"))
        .await
        .unwrap();

    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M1/E1"
    );
    assert!(
        host.sandbox_spec("semantic-memory").mounts.is_empty(),
        "M1/E1"
    );
    let prompt = host.thread_session_prompts("semantic-memory").join("\n");
    assert!(
        prompt.contains("test-input-0") && prompt.contains("test-input-1"),
        "M1/E2"
    );
    assert!(prompt.contains("through the memory tools"), "M1/E2");
    assert!(!prompt.contains("is mounted"), "M1/E2");
    let misplaced_file_call = context
        .attempt_context
        .tool_executor
        .as_ref()
        .expect("filesystem-free executor")
        .invoke(&ToolCall {
            call_id: "misplaced-read".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "anything"}),
        })
        .await
        .unwrap_err();
    assert!(
        misplaced_file_call
            .to_string()
            .contains("filesystem-free Session"),
        "M1/E1 fail closed"
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M1/E1 rejected file call cannot awaken Sandbox"
    );
    assert!(
        context
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .any(|descriptor| descriptor.id == "read_memory"),
        "M1/E3 model surface"
    );
    let runtime_read = run_prepared_session_messages(
        &managed,
        "assistant",
        "semantic-memory",
        vec![Message::text(
            MessageId("semantic-memory-runtime-read".into()),
            Role::User,
            "Read the preference memory",
        )],
    )
    .await
    .expect("M1 final Runtime dispatches read_memory");
    assert!(
        runtime_read
            .new_messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| block_text(&message.content))
            .any(|text| text.contains("runtime-semantic-tool") && text.contains("from-b")),
        "M1/E3 model descriptor reaches the bound executor through host.run"
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M1/E1 semantic Runtime execution remains Sandbox-free"
    );

    *model.fetch_url.lock().unwrap() = Some("https://fixture.invalid/value".into());
    let runtime_fetch = run_prepared_session_messages(
        &managed,
        "assistant",
        "semantic-memory",
        vec![Message::text(
            MessageId("semantic-memory-runtime-fetch".into()),
            Role::User,
            "Fetch the configured URL",
        )],
    )
    .await
    .expect("M7 unpublished WebFetch fails within the Runtime tool result");
    assert!(
        runtime_fetch
            .new_messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .map(|message| block_text(&message.content))
            .any(|text| text.contains("runtime-semantic-tool:unknown tool: web_fetch")),
        "M7/E5 tool policy cannot manufacture WebFetch: {:?}",
        runtime_fetch.new_messages
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M7/E5 rejected WebFetch remains Sandbox-free"
    );

    let bindings = host
        .session_slots
        .read("semantic-memory", |slot| slot.memory_bindings.clone())
        .unwrap();
    let tools = crate::session_memory_tools::SessionMemoryTools::new(bindings).unwrap();
    let invoke = |tool_id: &str, arguments: serde_json::Value| {
        let tool = tools
            .executors
            .iter()
            .find(|tool| tool.id() == tool_id)
            .cloned()
            .expect("semantic Memory tool is wired");
        let tool_id = tool_id.to_string();
        async move {
            tool.invoke(ToolCall {
                call_id: format!("{tool_id}-call"),
                tool_id,
                arguments,
            })
            .await
        }
    };
    let read_b = invoke(
        "read_memory",
        serde_json::json!({"binding": "test-input-1", "path": "/preference.md"}),
    )
    .await
    .unwrap();
    assert!(
        !read_b.is_error && read_b.text().contains("from-b"),
        "M1/E3"
    );

    let created = invoke(
        "write_memory",
        serde_json::json!({"binding": "test-input-0", "path": "/fact.md", "content": "v1"}),
    )
    .await
    .unwrap();
    assert!(!created.is_error, "M2/E4");
    let created: awaken_resource_contract::Memory = serde_json::from_str(&created.text()).unwrap();
    let updated = invoke(
        "write_memory",
        serde_json::json!({
            "binding": "test-input-0",
            "path": "/fact.md",
            "content": "v2",
            "expected_sha256": created.content_sha256
        }),
    )
    .await
    .unwrap();
    assert!(!updated.is_error, "M3/E4");
    let stale = invoke(
        "write_memory",
        serde_json::json!({
            "binding": "test-input-0",
            "path": "/fact.md",
            "content": "clobber",
            "expected_sha256": "stale"
        }),
    )
    .await
    .unwrap_err();
    assert!(stale.to_string().contains("cas conflict"), "M4/E4: {stale}");
    let read_only = invoke(
        "write_memory",
        serde_json::json!({"binding": "test-input-1", "path": "/new.md", "content": "no"}),
    )
    .await
    .unwrap_err();
    assert!(
        read_only.to_string().contains("read-only"),
        "M5/E4: {read_only}"
    );
    assert_eq!(
        host.memory_repository()
            .get_by_path(store_a, "/fact.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "M4 never clobbers"
    );
    host.session_slots.update("semantic-memory", |slot| {
        slot.runtime = None;
    });
    let rebuilt = host
        .ctx_for("semantic-memory", Some("assistant"))
        .await
        .expect("M6 rebuilds from the frozen Session projection");
    assert_eq!(
        host.session_slots
            .read("semantic-memory", |slot| slot.content_delivery)
            .flatten(),
        Some(crate::session_slot::ManagedContentDelivery::SemanticTools),
        "M6 keeps one delivery path"
    );
    assert!(
        rebuilt
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .any(|descriptor| descriptor.id == "read_memory"),
        "M6 preserves the Session-owned semantic Memory tools"
    );
    assert!(
        host.session_environment("semantic-memory").await.is_none(),
        "M6 does not materialize a Sandbox while rebuilding"
    );
}

#[tokio::test]
async fn activation_applies_current_resource_state_as_a_deny_only_overlay() {
    use awaken_resource_contract::{
        ChangeMemoryStoreState, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
        RegisterMemoryStore, ResourceAdministration as _, ResourceState,
    };

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    let catalog = resource_registry();
    catalog
        .register_memory_store(RegisterMemoryStore {
            definition: MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: host.local_workspace().into(),
                name: "memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            initial_config: MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        })
        .expect("register test MemoryStore");
    let manifest = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    catalog
        .change_memory_store_state(ChangeMemoryStoreState {
            workspace_id: host.local_workspace().into(),
            id: store_id.clone().into(),
            state: ResourceState::Suspended,
        })
        .expect("suspend test MemoryStore");
    let managed = crate::ManagedHost::new(host.clone()).with_resource_validator(catalog.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = manifest;

    let error = managed
        .install_test_session_init("t-suspended", init)
        .await
        .unwrap_err();

    assert!(error.message.contains("not active"));
    assert!(host.sandbox_spec("t-suspended").mounts.is_empty());
}

#[tokio::test]
async fn memory_activation_enforces_catalog_workspace_without_iam_policy_logic() {
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RegisterMemoryStore,
        ResourceAdministration as _, ResourceState,
    };

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    let catalog = resource_registry();
    catalog
        .register_memory_store(RegisterMemoryStore {
            definition: MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: "workspace-a".into(),
                name: "private-memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            initial_config: MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        })
        .expect("register workspace-fenced MemoryStore");
    let managed = crate::ManagedHost::new(host.clone()).with_resource_validator(catalog);
    let mut init = bare_session("agent", "workspace-b");
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .install_test_session_init("wrong-workspace", init)
        .await
        .unwrap_err();
    assert!(error.message.contains("not found"));
    assert!(host.sandbox_spec("wrong-workspace").mounts.is_empty());
    assert!(host.memory_for_thread("wrong-workspace").is_none());
}

/// The same effective input contract realizes File and Repository resources without
/// exposing their authoring repository to Runtime.
#[tokio::test]
async fn install_test_session_init_mounts_effective_file_and_stages_effective_repo() {
    use awaken_provisioning_contract::{MountAccess, MountSource};
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a file blob and pass the already-resolved File and Repository inputs.
    let binary = vec![0, 0xff, b'R', 0x80, b'\n'];
    let record = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            host.local_workspace(),
            "notes.txt".into(),
            "application/octet-stream".into(),
            &binary,
        )
        .await
        .unwrap();
    let file_id = record.id;
    let managed = managed_with_resource_source(host.clone());

    managed
        .install_test_session_init(
            "t-multi",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: effective_resources(vec![
                    TestInput {
                        kind: "file".into(),
                        id: file_id.clone(),
                        mount_path: "/mnt/files/notes.txt".into(),
                        access: ResourceAccess::ReadOnly,
                        instructions: None,
                        initial_branch: None,
                        initial_commit: None,
                    },
                    TestInput {
                        kind: "github_repository".into(),
                        id: "https://github.com/awaken/example.git".into(),
                        mount_path: "/workspace/repo".into(),
                        access: ResourceAccess::ReadOnly,
                        instructions: None,
                        initial_branch: None,
                        initial_commit: None,
                    },
                ]),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();

    // The exact bytes and their content identity cross the neutral mount contract;
    // no UTF-8 conversion can corrupt binary input.
    let spec = host.sandbox_spec("t-multi");
    let mount = &spec.mounts[0];
    assert_eq!(mount.mount_path, "/mnt/session/uploads/mnt/files/notes.txt");
    assert_eq!(mount.access, MountAccess::ReadOnly);
    let MountSource::InlineBytes {
        contents,
        content_hash,
    } = &mount.source
    else {
        panic!("effective File input must use the binary-safe carried source")
    };
    assert_eq!(contents, &binary);
    assert_eq!(content_hash.as_deref(), Some(record.blob_id.as_str()));

    // The repo is staged for a host-side clone (not a byte mount).
    let repositories = host.thread_repository_activations("t-multi");
    assert_eq!(
        repositories.len(),
        1,
        "the bound repository has one realization plan"
    );
    assert_eq!(
        repositories[0].plan.source_remote_url,
        "https://github.com/awaken/example.git"
    );
    assert_eq!(
        repositories[0].plan.transport_url,
        "https://github.com/awaken/example.git"
    );
    assert_eq!(
        repositories[0].plan.mount_path, "/workspace/repo",
        "the realization plan carries the same Agent-visible path as the frozen input"
    );
}

/// Repository mount-path cause/effect rule: C1 a new command names the canonical
/// `/workspace/repo` child; C2 no Environment exists yet. C1+C2 stages exactly
/// one Repository activation without relocating the path, and keeps the active
/// manifest absent until the physical transition completes.
#[tokio::test]
async fn complete_projection_preserves_the_canonical_repository_path_without_relocation() {
    use awaken_session_contract::SessionInit;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let verifier = Arc::new(FixedRepositoryTransport(
        awaken_resource_contract::RepositoryTransport::Direct,
    ));
    let managed = managed_with_resource_source(host.clone())
        .with_repository_binding_verifier(verifier)
        .install_dispatch_session_runtime();
    let resources = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: "https://github.com/awaken/example.git".into(),
        mount_path: "/workspace/repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .install_complete_test_session(
            "canonical-repository-path",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: resources.clone(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .expect("canonical Repository path stages through the complete projection");
    managed
        .apply_session_inputs(
            "canonical-repository-path",
            &resource_transition(host.local_workspace(), 0, Default::default(), 0, resources),
        )
        .await
        .expect("compile the exact cold Repository transition");
    let activations = host.thread_repository_activations("canonical-repository-path");
    assert_eq!(activations.len(), 1, "one exact Repository activation");
    assert_eq!(activations[0].plan.mount_path, "/workspace/repo");
    assert!(
        host.thread_resource_manifest("canonical-repository-path")
            .is_none(),
        "staging is not physical completion"
    );
    assert!(
        host.sandbox_spec("canonical-repository-path")
            .mounts
            .is_empty(),
        "Repository stays on its sole activation path"
    );
}

/// Terminal publication cause/effect decision table:
///
/// | Rule | frozen input | local branch/HEAD | remote ref | Effect |
/// |---|---|---|---|---|
/// | P1 | one writable Repository | exact expected coordinate | absent | publish and return canonical Session receipt |
/// | P2 | same command replay | exact expected coordinate | same commit | no-op with byte-identical receipt |
/// | P3 | complete ordinary projection carries empty rev0 -> desired rev1 | exact | any | canonical Dispatch install owns baseline plus Resource staging before realization; publication performs no second lookup |
/// | P4 | replacement Host starts without root/child slots and claims one Resident aggregate binding | exact | stale without an authorized prior | cold install/adopt, child preparation, durable rejection, root preparation, then physical disposal |
///
/// Invalid coordinate and mismatched remote rules are exhaustively covered at
/// the Repository realizer boundary; this integration test proves the
/// ManagedHost uses that sole implementation and retains the Environment until
/// first execution, replay, and the ordered terminal reconciliation have
/// produced evidence.
#[tokio::test]
async fn managed_terminal_repository_publication_pushes_exact_commit_and_replays() {
    let temp = tempfile::tempdir().expect("publication fixture");
    let sandbox_root = temp.path().join("sandboxes");
    let namespace_probe = awaken_sandbox_local::NamespaceProvider::new(&sandbox_root);
    if awaken_provisioning_contract::SandboxProvider::probe_ready(&namespace_probe)
        .await
        .is_err()
    {
        eprintln!("skipping Namespace execution assertion: OS sandbox/user namespaces unavailable");
        return;
    }
    let remote = temp.path().join("remote.git");
    let seed = temp.path().join("seed");
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .expect("git fixture command");
        assert!(status.success(), "git {args:?}");
    };
    git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
    git(
        temp.path(),
        &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
    );
    git(&seed, &["config", "user.name", "seed"]);
    git(&seed, &["config", "user.email", "seed@example.invalid"]);
    std::fs::write(seed.join("README.md"), "base").expect("seed file");
    git(&seed, &["add", "README.md"]);
    git(&seed, &["commit", "-m", "base"]);
    git(&seed, &["push", "-u", "origin", "HEAD"]);

    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let mut raw_host =
        SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone());
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            sandbox_root.clone(),
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let repository_path_fidelity = raw_host.session_provider.capabilities().path_fidelity;
    let host = Arc::new(raw_host);
    let workspace_id = host.local_workspace().to_string();
    let managed = managed_with_resource_source(host.clone());
    let session_id = "terminal-publication-local";
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "terminal-publication-worker".into(),
        runtime_incarnation: "terminal-publication-worker:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
            .saturating_add(60_000),
    };
    // Reuse the Environment-binding test authority so the exact realization
    // lease produces a V2 provider fence and owned-path evidence. P1 therefore
    // exercises the canonical create/persist/publish path instead of a legacy
    // unfenced test Sandbox.
    let initial_binding_sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    managed.install_environment_binding_sink(initial_binding_sink.clone());
    let resources = effective_repository(
        "repository-publication",
        remote.to_str().unwrap(),
        "/workspace/repository",
        None,
    );
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment: session_environment(
                awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model_override: None,
            model: "stub".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    let ordinary_projection = awaken_session_contract::FrozenSessionProjection {
        workspace_id: workspace_id.clone(),
        revision: awaken_session_contract::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment: Default::default(),
        resource_revision: 1,
        resources: resources.clone(),
        previous_resource_manifest: Some(
            awaken_session_contract::SessionResourceManifest::at_revision(
                &workspace_id,
                0,
                Default::default(),
            ),
        ),
        tools: Default::default(),
        mcp: Vec::new(),
        request_context: Vec::new(),
    };
    // P1/P3 cause-effect rule: a cold Session with desired revision 1 requires
    // both its immutable baseline and exact empty-rev0 -> desired-rev1 Resource
    // transition before any Environment can exist. The complete Runtime port
    // owns both effects; no test-only preparation or direct slot write may
    // create a competing partial projection.
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        session_id,
        ordinary_projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("P1/P3 install complete ordinary projection before realization");
    host.install_session_realization_lease(session_id, lease.clone());
    let realization = run_prepared_session_messages(
        &managed,
        "agent",
        session_id,
        vec![Message::text(
            MessageId("terminal-publication-input".into()),
            Role::User,
            "prepare repository",
        )],
    )
    .await;
    if !repository_path_fidelity {
        let error = match realization {
            Err(error) => error,
            Ok(_) => panic!(
                "a Namespace provider without path fidelity must reject Repository publication"
            ),
        };
        assert!(
            error
                .message
                .contains("one sandbox-absolute workspace path"),
            "P0 capability rejection must explain the shared-path invariant: {}",
            error.message
        );
        assert!(
            host.session_environment(session_id).await.is_none(),
            "P0 rejection happens before a publishable environment is exposed"
        );
        let published = std::process::Command::new("git")
            .current_dir(&remote)
            .args(["show-ref", "--verify", "--quiet", "refs/heads/awf/work"])
            .status()
            .expect("inspect publication ref");
        assert!(
            !published.success(),
            "P0 rejected realization cannot publish a remote branch"
        );
        return;
    }
    realization.expect("P1 realize Session Environment");
    let environment = host
        .session_environment(session_id)
        .await
        .expect("P1 retained Environment");
    let status = environment
        .run_test_command(awaken_provisioning_contract::Command::new([
            "sh",
            "-c",
            concat!(
                "git -C repository config user.name agent && ",
                "git -C repository config user.email agent@example.invalid && ",
                "git -C repository checkout -b awf/work && ",
                "printf changed > repository/README.md && ",
                "git -C repository add README.md && ",
                "git -C repository commit -m changed && ",
                "mkdir -p .terminal-publication-proof && ",
                "git -C repository rev-parse HEAD > .terminal-publication-proof/commit.txt"
            ),
        ]))
        .await
        .expect("P1 author commit");
    assert_eq!(status.code, Some(0), "P1 exact local commit");
    let commit = environment
        // P1 observes only the dedicated proof directory. Traversing the
        // attached Repository would mix its provider-owned Git metadata into
        // this assertion and duplicate the Repository realizer's own checks.
        .list_workspace_files(".terminal-publication-proof")
        .await
        .expect("P1 list workspace")
        .into_iter()
        .find_map(|(path, bytes)| (path == "commit.txt").then_some(bytes))
        .map(String::from_utf8)
        .transpose()
        .expect("P1 UTF-8 commit")
        .expect("P1 commit file")
        .trim()
        .to_string();
    let intent = awaken_session_contract::SessionRepositoryPublicationIntent {
        input: resources.inputs()[0].clone(),
        expectation: awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: commit.clone(),
            expected_prior_commit: None,
        },
    };
    let child_id = "terminal-publication-child";
    let mut publication_ready = terminal_test_session(
        session_id,
        &ordinary_projection,
        lease.clone(),
        [child_id.to_string()],
        Some(intent.clone()),
    );
    let child_cleanup = publication_ready
        .terminal_cleanup
        .command_for(session_id, child_id)
        .expect("P4 child cleanup command");
    let child_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        child_cleanup.clone(),
        lease.clone(),
    );
    publication_ready
        .record_terminal_cleanup_preparation(
            &ordinary_projection.workspace_id,
            &lease,
            awaken_session_contract::SessionCleanupPreparation::try_new(
                &child_effect,
                child_effect.sandbox_effect_fence().unwrap(),
                Vec::new(),
            )
            .unwrap(),
            None,
        )
        .expect("P4 aggregate admits durable child preparation before publication");
    let command = publication_ready
        .terminal_cleanup
        .publication_command(session_id)
        .expect("P1 command projection")
        .expect("P1 command");
    let mut terminal_projection = ordinary_projection.clone();
    let retained = host
        .session_slots
        .read(session_id, |slot| {
            slot.environment_owner.terminal_bound_environment()
        })
        .flatten()
        .expect("P1 retained exact Environment identity");
    let crate::session_slot::BoundSessionEnvironmentIdentity::Durable {
        effect_id,
        generation,
    } = retained.identity
    else {
        panic!("P1 Environment must retain its generated Store-read authority");
    };
    terminal_projection.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: retained.binding,
        effect_id: Some(effect_id),
        generation: Some(generation),
        idle_since_unix_ms: None,
    };
    terminal_projection.previous_resource_manifest = Some(
        awaken_session_contract::SessionResourceManifest::at_revision(
            &workspace_id,
            1,
            resources.clone(),
        ),
    );
    awaken_session_contract::SessionRuntime::install_terminal_cleanup_assignment(
        &managed,
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection: terminal_projection.clone(),
            lease: lease.clone(),
        },
    )
    .await
    .expect("P1 install the aggregate-fenced publication assignment");
    let first = managed
        .execute_terminal_repository_publication_for_lease(command.clone(), &lease)
        .await
        .expect("P1 publish");
    let replay = managed
        .execute_terminal_repository_publication_for_lease(command.clone(), &lease)
        .await
        .expect("P2 replay");
    assert_eq!(first, replay, "P2 canonical first/replay receipt");
    let awaken_session_contract::SessionRepositoryPublicationEffect::Published(first) = first
    else {
        panic!("P1 expected a publication receipt");
    };
    assert_eq!(first.effect_receipt.commit, commit, "P1 exact receipt");
    let remote_commit = std::process::Command::new("git")
        .current_dir(temp.path())
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "rev-parse",
            "refs/heads/awf/work",
        ])
        .output()
        .expect("P1 inspect remote");
    assert!(remote_commit.status.success(), "P1 remote branch exists");
    assert_eq!(
        String::from_utf8(remote_commit.stdout)
            .expect("P1 remote commit UTF-8")
            .trim(),
        commit,
        "P1 remote exact commit"
    );
    let divergent = temp.path().join("divergent");
    git(
        temp.path(),
        &[
            "clone",
            "--branch",
            "awf/work",
            remote.to_str().unwrap(),
            divergent.to_str().unwrap(),
        ],
    );
    git(&divergent, &["config", "user.name", "remote-writer"]);
    git(
        &divergent,
        &["config", "user.email", "remote@example.invalid"],
    );
    std::fs::write(divergent.join("README.md"), "remote changed").unwrap();
    git(&divergent, &["commit", "-am", "remote changed"]);
    git(&divergent, &["push", "origin", "awf/work"]);
    assert!(
        host.session_environment(session_id).await.is_some(),
        "P1-P2 Environment survives until root cleanup"
    );

    let control_session = terminal_test_session(
        session_id,
        &terminal_projection,
        lease.clone(),
        [child_id.to_string()],
        Some(intent),
    );
    assert_eq!(
        control_session
            .terminal_cleanup
            .command_for(session_id, child_id),
        Some(child_cleanup),
        "P4 Control consumes the same aggregate-derived child command",
    );
    assert_eq!(
        control_session
            .terminal_cleanup
            .publication_command(session_id)
            .expect("P4 aggregate-owned Repository publication projection"),
        None,
        "P4 fresh aggregate keeps publication behind child preparation",
    );
    control.assignments.lock().unwrap().push_back(
        awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection: terminal_projection,
            lease: lease.clone(),
        },
    );
    *control.session.lock().unwrap() = Some(control_session);
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };

    // P4 cause/effect graph: C1 the old Worker durably published one exact
    // Resident binding and then lost all process-local state; C2 a replacement
    // Host has the same Namespace root, local Workspace, and Control authority
    // but no root/child slot; C3 claim-next returns the sole aggregate-frozen
    // assignment; C4 child preparation is still pending; C5 the frozen
    // create-only publication has no authorized prior while the remote advanced
    // after P1/P2. Effects: E1 only the canonical cold
    // claim/install/driver path may reconstruct the root; E2 live publication
    // adoption writes no ordinary Environment receipt and leaves the provider
    // marker Ready, so only the later root preparation enters terminal takeover
    // under T; E3 child preparation precedes the durable CAS rejection, root
    // preparation, and disposal; E4 all replacement slots and Environment
    // handles are retired. Missing, foreign,
    // Disposing, or otherwise non-live provider evidence remains owned by the
    // adjacent fail-closed recovery tests.
    //
    // | Rule | durable source | replacement slots | claim | Effect |
    // |---|---|---|---|---|
    // | P4a | Resident exact | root/child absent | exact | E1 + E2 + E3 + E4 |
    // | P4b | missing/foreign/Disposing | root/child absent | exact | fail closed; no receipt/delete |
    drop(environment);
    drop(managed);
    drop(host);

    let mut raw_replacement = SharedHost::new(Arc::new(OkModel), "stub")
        .with_session_control(control.clone())
        .with_local_workspace(workspace_id);
    raw_replacement.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            sandbox_root,
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let replacement_host = Arc::new(raw_replacement);
    let replacement_managed = managed_with_resource_source(replacement_host.clone());
    let replacement_binding_sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&replacement_host),
        authorize_calls: AtomicUsize::new(0),
        calls: AtomicUsize::new(0),
        observed_before_publish: AtomicBool::new(false),
        fail: false,
        require_realization: true,
        owned_session_id: None,
        committed_environment: None,
    });
    replacement_managed.install_environment_binding_sink(replacement_binding_sink.clone());

    assert!(
        !replacement_host.session_slots.contains(session_id)
            && !replacement_host.session_slots.contains(child_id),
        "P4 replacement starts without root or child process-local slots"
    );
    assert!(
        replacement_host
            .session_environment(session_id)
            .await
            .is_none(),
        "P4 replacement starts without a resident Environment wrapper"
    );

    assert_eq!(
        replacement_host
            .recover_terminal_cleanup_assignments(target.clone())
            .await
            .expect("P4 cold assignment recovery"),
        1,
        "P4 the replacement claims and drives exactly one assignment"
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target],
        "P4 one exact claim plus the terminating empty scan"
    );
    assert_eq!(
        control.events.lock().unwrap().as_slice(),
        [
            "cleanup:poll",
            "cleanup:poll",
            "cleanup:prepared:terminal-publication-child",
            "cleanup:poll",
            "publication:poll",
            "publication:rejection",
            "cleanup:poll",
            "cleanup:prepared:terminal-publication-local",
            "cleanup:poll",
            "cleanup:disposed",
        ],
        "P4 child preparation -> durable publication rejection -> root preparation -> physical disposal"
    );
    assert!(
        control.publication_receipts.lock().unwrap().is_empty(),
        "P4"
    );
    assert_eq!(
        control.publication_rejections.lock().unwrap().len(),
        1,
        "P4"
    );
    assert_eq!(control.preparations.lock().unwrap().len(), 2, "P4");
    assert_eq!(control.disposals.lock().unwrap().len(), 1, "P4");
    assert_eq!(
        replacement_binding_sink
            .authorize_calls
            .load(Ordering::SeqCst),
        0,
        "P4 terminal adoption never enters ordinary binding authorization"
    );
    assert_eq!(
        replacement_binding_sink.calls.load(Ordering::SeqCst),
        0,
        "P4 terminal adoption writes no ordinary binding receipt"
    );
    assert!(
        replacement_host
            .session_environment(session_id)
            .await
            .is_none(),
        "P4 physical disposal runs only after publication and both durable preparations"
    );
    assert!(
        !replacement_host.session_slots.contains(session_id)
            && !replacement_host.session_slots.contains(child_id),
        "P4 completion retires replacement root and child slots"
    );
}

#[tokio::test]
async fn file_activation_rejects_bytes_that_do_not_match_the_file_id() {
    // Cause/effect decision rule D1: the authoritative File application returns
    // bytes whose canonical digest differs from the logical record's blob id ->
    // staging fails closed before a Sandbox mount is published.
    use awaken_file_store::{FileStore, FileStoreError};

    struct CorruptFileStore;

    #[async_trait::async_trait]
    impl FileStore for CorruptFileStore {
        async fn put(&self, _bytes: &[u8]) -> Result<String, FileStoreError> {
            unreachable!("test only reads the corrupt entry")
        }

        async fn get(&self, _id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
            Ok(Some(b"different bytes".to_vec()))
        }

        async fn list(&self) -> Result<Vec<String>, FileStoreError> {
            Ok(Vec::new())
        }

        async fn delete(&self, _id: &str) -> Result<bool, FileStoreError> {
            Ok(false)
        }
    }

    let declared_blob_id = awaken_sandbox_local::content_fingerprint(b"declared bytes");
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    let corrupt_store = Arc::new(CorruptFileStore);
    raw_host.file_store = corrupt_store.clone();
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        corrupt_store,
        raw_host.file_catalog.clone(),
        raw_host
            .resource_reclamation()
            .expect("test lifecycle repository"),
    ));
    raw_host = raw_host.with_file_application(
        application.clone(),
        Arc::new(
            awaken_resource_application::ApplicationFileContentSource::new(application.clone()),
        ),
        Arc::new(awaken_resource_application::ApplicationArtifactPublisher::new(application)),
    );
    let host = Arc::new(raw_host);
    let public_id = "file_corrupt".to_string();
    host.file_catalog()
        .create_file(awaken_resource_contract::FileRecord {
            id: public_id.clone(),
            workspace_id: host.local_workspace().into(),
            blob_id: declared_blob_id,
            filename: "input.bin".into(),
            mime_type: "application/octet-stream".into(),
            size_bytes: 14,
            created_at: "2026-01-01T00:00:00Z".into(),
            expires_at: None,
            downloadable: false,
            scope_id: None,
            logical_path: None,
            harvest_key: None,
            artifact_idempotency_scope: None,
            deleted: false,
        })
        .await
        .unwrap();
    let managed = crate::ManagedHost::new(host.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: public_id,
        mount_path: "/mnt/input.bin".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .install_test_session_init("t-corrupt-file", init)
        .await;

    assert!(
        error
            .unwrap_err()
            .message
            .contains("content digest mismatch"),
        "corrupt content must fail before Agent execution"
    );
}

#[tokio::test]
async fn file_activation_enforces_workspace_ownership_without_iam_policy_logic() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let file_id = host
        .file_application()
        .expect("test startup installs File application")
        .create_uploaded_file(
            "workspace-a",
            "input.txt".into(),
            "text/plain".into(),
            b"workspace-a",
        )
        .await
        .unwrap()
        .id;
    let managed = crate::ManagedHost::new(host);
    let mut init = bare_session("a", "workspace-b");
    init.resources = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/mnt/input.txt".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let error = managed
        .install_test_session_init("t-cross-workspace", init)
        .await;

    assert!(
        error
            .unwrap_err()
            .message
            .contains("not found in this workspace"),
        "resource integrity rejects a foreign Workspace id without parsing IAM policy"
    );
}

#[tokio::test]
async fn published_mcp_credential_is_materialized_only_for_its_workspace_and_revision() {
    use awaken_ext_mcp::McpToolTransport;
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_contract::McpAttachmentRealizer;

    struct PinnedBearerRefresher;

    #[async_trait::async_trait]
    impl awaken_ext_mcp::CredentialRefresher for PinnedBearerRefresher {
        async fn refresh(
            &self,
            _challenge: &awaken_ext_mcp::AuthChallenge,
        ) -> Option<awaken_ext_mcp::Credential> {
            Some(awaken_ext_mcp::Credential::Bearer(
                "published-mcp-token".into(),
            ))
        }
    }

    struct ExactRefreshFactory;

    impl awaken_credential_materializer::CredentialRefreshFactory for ExactRefreshFactory {
        fn refresher(
            &self,
            _credential_id: awaken_credential_contract::CredentialSourceId,
            _access: awaken_runtime_contract::CredentialRefreshAccess,
        ) -> Arc<dyn awaken_ext_mcp::CredentialRefresher> {
            Arc::new(PinnedBearerRefresher)
        }

        fn bearer_reloader(
            &self,
            _credential_id: awaken_credential_contract::CredentialSourceId,
            _credential_revision: u64,
        ) -> Arc<dyn awaken_ext_mcp::CredentialRefresher> {
            Arc::new(PinnedBearerRefresher)
        }
    }

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let mut credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: "workspace-a".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(awaken_agent_contract::RedactedString::from(
                "published-mcp-token".to_string(),
            )),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .unwrap();
    let managed = crate::ManagedHost::new(host.clone())
        .with_credentials(credentials.clone(), secrets.clone())
        .with_credential_refresh_factory(Arc::new(ExactRefreshFactory));
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    let (mcp_url, seen) = crate::test_mcp::start(Some("Bearer published-mcp-token")).await;
    let mcp_audience = awaken_session_contract::McpTarget::identity(&mcp_url)
        .unwrap()
        .canonical_url();
    credential.descriptor = Some(awaken_credential_contract::CredentialDescriptor::new(
        mcp_audience.clone(),
        awaken_credential_contract::CredentialMaterialDescriptor::secret(
            awaken_credential_contract::OPAQUE_SECRET_MATERIAL_TYPE,
        ),
        [awaken_credential_contract::CredentialTargetContract::new(
            awaken_credential_contract::CredentialTarget::new(
                awaken_credential_contract::CredentialPurpose::McpAuthorization,
                mcp_audience.clone(),
            ),
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
        )],
    ));
    awaken_credential_vault::repo::CredentialRepo::put(credentials.as_ref(), credential.clone())
        .await
        .unwrap();
    let generation = |session: &str| awaken_session_contract::McpGenerationRef {
        session_id: session.into(),
        attachment_id: awaken_session_contract::McpAttachmentId("mcp-docs".into()),
        generation: awaken_session_contract::McpGeneration(1),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: u64::MAX - 1,
    };
    let request = |session: &str, workspace: &str, revision: u64| {
        awaken_session_contract::StageMcpAttachment {
            workspace_id: workspace.into(),
            generation: generation(session),
            realization_id: format!("realize-{session}"),
            stage_idempotency_key: format!("stage-{session}"),
            name: "docs".into(),
            target: awaken_session_contract::McpTarget::parse_http(&mcp_url).unwrap(),
            credential: Some(
                CredentialAccess::new(
                    CredentialRef {
                        id: credential.id.0.clone(),
                        revision,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::HttpHeader {
                        name: "authorization".into(),
                        scheme: Some("Bearer".into()),
                    },
                    CredentialExecutionPolicy::exact(
                        holder.clone(),
                        ModelExposurePolicy::VirtualOnly,
                    ),
                )
                .with_target(awaken_runtime_contract::CredentialTarget::new(
                    awaken_credential_contract::CredentialPurpose::McpAuthorization,
                    mcp_audience.clone(),
                )),
            ),
            prompts_as_skills: false,
            selected_plaintext_holder: Some(holder.clone()),
        }
    };

    // Cause graph: exact described MCP target + workspace + revision + allowed
    // Worker holder -> Native host material is staged but invisible; durable
    // publication command -> visible.
    // ACP authentication additionally requires installed no-bypass provider evidence
    // before material resolution; anonymous ACP has no secret and needs no custody proof.
    //
    // | Rule | workspace | revision | stage | publish | Effect |
    // |---|---|---|---|---|---|
    // | H1 | exact | exact | success | no | staged/invisible |
    // | H2 | exact | exact | replay | no | same receipt/no duplicate |
    // | H3 | exact | exact | success | yes | active/visible |
    // | H4 | exact | stale | fail | - | no projection |
    // | H5 | foreign | exact | fail | - | no projection |
    // | H6 | exact | exact | drain | - | hidden/material cleared |
    // | H7 | exact | exact | drain replay | - | idempotent |
    // | H8 | exact | exact | publish removed | - | reject |
    // | H9 | exact | exact | expired stage | - | reject/no projection |
    // | H10 | exact | exact | conflicting replay | - | reject/no duplicate |
    // | H11 | exact | exact | drain unknown | - | idempotent no-op |
    // | H12 | exact | exact | authenticated ACP/no no-bypass | - | reject before resolver |
    // | H13 | - | - | anonymous ACP | no | staged without bearer |
    // | H14 | exact immutable binding | later lease | renew | publish | same generation/material, extended fence |
    // | H15 | changed target | later lease | renew | - | reject/no mutation |
    // | H16 | exact binding | same lease/new key | renew | - | reject/no mutation |
    // | H17 | non-bearer usage | exact holder/revision | stage | - | reject before materialization |
    // | H18 | authenticated ACP/Forbidden exposure | exact | stage | - | reject before relay/no lookup |
    // | H19 | expired predecessor/current same-epoch renewal | exact | stage+publish | admitted effects complete under current local authority |
    // | H20 | authenticated ACP/complete provider evidence | exact | stage+publish+call | generation route injects; no inline secret |
    // | H21 | non-OAuth bearer/exact factory | exact | stage | - | one neutral challenge refresher; no Vault in Runtime |
    // | H22 | exact removed tombstone | exact request replay | stage | - | rebuild one staged projection/material |
    // | H24 | Managed prompts-as-skills | any backend | stage | - | reject before projection/materialization |
    // | H25 | exact Staging owner | exact replay | stage | - | retryable unavailable/no false staged receipt |
    // | H26 | default weak / frozen BackendOwned strong | exact | stage | - | admit from selected strong provider |
    // | H27 | default strong / frozen BackendOwned weak | exact | stage | - | reject from selected weak provider |
    let exact_request = request("mcp-exact", "workspace-a", 1);
    let receipt = managed
        .stage_mcp_attachment(exact_request.clone())
        .await
        .expect("H1");
    assert!(host.active_mcp_projections("mcp-exact").is_empty(), "H1");
    assert_eq!(
        managed
            .stage_mcp_attachment(exact_request)
            .await
            .expect("H2"),
        receipt,
        "H2"
    );
    assert_eq!(
        host.mcp_projection(&generation("mcp-exact"))
            .unwrap()
            .server
            .unwrap()
            .bearer()
            .map(|secret| secret.expose_secret()),
        Some("published-mcp-token")
    );
    let projection = host.mcp_projection(&generation("mcp-exact")).unwrap();
    assert!(
        matches!(
            projection.server.unwrap().transport,
            crate::mcp::McpTransportMaterialKind::Http {
                refresh: Some(_),
                ..
            }
        ),
        "H21"
    );
    managed
        .publish_mcp_generation(generation("mcp-exact"))
        .await
        .expect("H3");
    assert_eq!(host.active_mcp_projections("mcp-exact").len(), 1, "H3");

    let stale_error = managed
        .stage_mcp_attachment(request("mcp-stale", "workspace-a", 2))
        .await
        .unwrap_err();
    assert_eq!(stale_error.code, "mcp_credential_revision_mismatch", "H4");

    let foreign_error = managed
        .stage_mcp_attachment(request("mcp-foreign", "workspace-b", 1))
        .await
        .unwrap_err();
    assert_eq!(foreign_error.code, "mcp_credential_revision_mismatch", "H5");

    let mut unsupported_usage = request("mcp-usage", "workspace-a", 1);
    unsupported_usage.credential.as_mut().unwrap().usage = CredentialUsage::QueryParameter {
        name: "token".into(),
    };
    assert_eq!(
        managed
            .stage_mcp_attachment(unsupported_usage)
            .await
            .unwrap_err()
            .code,
        "mcp_credential_usage_unsupported",
        "H17"
    );
    assert!(
        host.mcp_projection(&generation("mcp-usage")).is_none(),
        "H17"
    );
    let mut prompt_skill = request("mcp-native-prompt-skill", "workspace-a", 1);
    prompt_skill.prompts_as_skills = true;
    assert_eq!(
        managed
            .stage_mcp_attachment(prompt_skill)
            .await
            .unwrap_err()
            .code,
        "mcp_prompt_skills_unsupported",
        "H24"
    );
    assert!(
        host.mcp_projection(&generation("mcp-native-prompt-skill"))
            .is_none(),
        "H24"
    );

    let mut conflicting_renewal = request("mcp-exact", "workspace-a", 1);
    conflicting_renewal.generation.lease_expires_at_unix_ms = u64::MAX;
    conflicting_renewal.stage_idempotency_key = "renew-conflicting".into();
    conflicting_renewal.target =
        awaken_session_contract::McpTarget::parse_http("https://other.example.test/mcp").unwrap();
    assert_eq!(
        managed
            .stage_mcp_attachment(conflicting_renewal)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_generation",
        "H15"
    );
    let mut non_increasing = request("mcp-exact", "workspace-a", 1);
    non_increasing.stage_idempotency_key = "renew-without-extension".into();
    assert_eq!(
        managed
            .stage_mcp_attachment(non_increasing)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_generation",
        "H16"
    );
    let mut renewal = request("mcp-exact", "workspace-a", 1);
    renewal.generation.lease_expires_at_unix_ms = u64::MAX;
    renewal.stage_idempotency_key = "renew-exact".into();
    let renewed_generation = renewal.generation.clone();
    let renewed = managed
        .stage_mcp_attachment(renewal.clone())
        .await
        .expect("H14 stage");
    assert_eq!(renewed.generation, renewed_generation, "H14");
    managed
        .publish_mcp_generation(renewed_generation.clone())
        .await
        .expect("H14 publish");
    let projection = host
        .mcp_projection(&renewed_generation)
        .expect("H14 projection");
    assert_eq!(
        projection
            .server
            .as_ref()
            .and_then(|server| server.bearer())
            .map(|secret| secret.expose_secret()),
        Some("published-mcp-token"),
        "H14 retained material"
    );
    managed
        .drain_mcp_generation(renewed_generation.clone())
        .await
        .expect("H6");
    assert!(host.active_mcp_projections("mcp-exact").is_empty(), "H6");
    let drained = host.mcp_projection(&renewed_generation).unwrap();
    assert!(
        drained.server.is_none() && drained.native_wiring.is_none(),
        "H6"
    );
    managed
        .drain_mcp_generation(renewed_generation.clone())
        .await
        .expect("H7");
    assert!(
        managed
            .publish_mcp_generation(renewed_generation.clone())
            .await
            .is_err(),
        "H8"
    );
    let recovered = managed
        .stage_mcp_attachment(renewal)
        .await
        .expect("H22 exact removed replay");
    assert_eq!(recovered.generation, renewed_generation, "H22");
    let recovered_projection = host
        .mcp_projection(&renewed_generation)
        .expect("H22 rebuilt projection");
    assert_eq!(
        recovered_projection.state,
        crate::session_slot::McpProjectionState::Staged,
        "H22"
    );
    assert_eq!(
        host.session_slots.read("mcp-exact", |slot| slot.mcp.len()),
        Some(1),
        "H22 no duplicate tombstone"
    );
    let staging_request = recovered_projection.request.clone();
    host.session_slots.modify("mcp-exact", |slot| {
        slot.mcp[0].state = crate::session_slot::McpProjectionState::Staging;
        slot.mcp[0].staging = Some(crate::session_slot::McpStagingActivity::default());
    });
    assert_eq!(
        managed
            .stage_mcp_attachment(staging_request)
            .await
            .unwrap_err()
            .code,
        "mcp_generation_staging",
        "H25"
    );
    host.session_slots.modify("mcp-exact", |slot| {
        slot.mcp[0].state = crate::session_slot::McpProjectionState::Staged;
        slot.mcp[0].staging = None;
    });
    let mut expired = request("mcp-expired", "workspace-a", 1);
    expired.generation.lease_expires_at_unix_ms = 0;
    assert_eq!(
        managed
            .stage_mcp_attachment(expired)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_ownership",
        "H9"
    );
    assert!(host.mcp_projection(&generation("mcp-expired")).is_none());
    let mut admitted_before_renewal = request("mcp-renewed-authority", "workspace-a", 1);
    admitted_before_renewal.generation.lease_expires_at_unix_ms = 0;
    host.install_session_realization_lease(
        "mcp-renewed-authority",
        awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "runtime-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        },
    );
    let admitted_receipt = managed
        .stage_mcp_attachment(admitted_before_renewal)
        .await
        .expect("H19 stage admitted before renewal");
    managed
        .publish_mcp_generation(admitted_receipt.generation)
        .await
        .expect("H19 publish admitted before renewal");
    let first = request("mcp-conflict", "workspace-a", 1);
    managed
        .stage_mcp_attachment(first)
        .await
        .expect("H10 setup");
    let mut conflict = request("mcp-conflict", "workspace-a", 1);
    conflict.stage_idempotency_key = "another-key".into();
    assert_eq!(
        managed
            .stage_mcp_attachment(conflict)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_generation",
        "H10"
    );
    assert_eq!(
        host.session_slots
            .read("mcp-conflict", |slot| slot.mcp.len()),
        Some(1),
        "H10"
    );
    managed
        .drain_mcp_generation(generation("mcp-never-staged"))
        .await
        .expect("H11");

    let launch = awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]);
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        launch,
    ));
    let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    let acp_host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_acp(executor.clone()));
    acp_host.register_thread_backend_projection("mcp-acp-forbidden", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-protected", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-anonymous", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-prompt-skill", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-client-basic", "acp:claude");
    let host_acp_snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new(
                    "provider", "model", "acp:test",
                ),
            ),
        )
        .build();
    // H12 has one protected-session case: install its exact publication
    // directly so the fixture does not imply an untested multi-case partition.
    let protected_session = "mcp-acp-protected";
    acp_host.register_thread_agent_projection(protected_session, "assistant");
    acp_host
        .retain_session_publication(protected_session, Some(&host_acp_snapshot))
        .expect("H12 exact HostExecutor publication");
    // Deliberately install no credential resolver: the no-bypass failure must mask
    // material-source availability and prove no secret lookup was attempted.
    let acp_managed = crate::ManagedHost::new(acp_host.clone());
    let mut forbidden = request("mcp-acp-forbidden", "workspace-a", 1);
    forbidden.credential.as_mut().unwrap().policy.model_exposure = ModelExposurePolicy::Forbidden;
    assert_eq!(
        acp_managed
            .stage_mcp_attachment(forbidden)
            .await
            .unwrap_err()
            .code,
        "mcp_model_exposure_forbidden",
        "H18"
    );
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-forbidden"))
            .is_none(),
        "H18"
    );
    let acp_error = acp_managed
        .stage_mcp_attachment(request("mcp-acp-protected", "workspace-a", 1))
        .await
        .unwrap_err();
    assert_eq!(acp_error.code, "mcp_holder_unsupported", "H12");
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-protected"))
            .is_none(),
        "H12"
    );

    let workload_holder = PlaintextHolder::new(
        PlaintextBoundary::Workload,
        awaken_credential_contract::SELF_HOSTED_ACP_TRUST_DOMAIN,
    );
    let basic_client = crate::ManagedHost::new(acp_host.clone())
        .with_credentials(credentials.clone(), secrets.clone());
    let mut basic_client_request = request("mcp-acp-client-basic", "workspace-a", 1);
    basic_client_request.selected_plaintext_holder = Some(workload_holder.clone());
    basic_client_request.credential.as_mut().unwrap().policy =
        CredentialExecutionPolicy::exact(workload_holder.clone(), ModelExposurePolicy::Forbidden);
    let basic_client_receipt = basic_client
        .stage_mcp_attachment(basic_client_request)
        .await
        .expect("H23 client injection does not require WorkerRelay provider custody");
    assert_eq!(
        basic_client_receipt.actual_realization_kind,
        Some(awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField),
        "H23"
    );

    let mut anonymous = request("mcp-acp-anonymous", "workspace-a", 1);
    anonymous.credential = None;
    anonymous.selected_plaintext_holder = None;
    let mut prompt_skill = anonymous.clone();
    prompt_skill.generation = generation("mcp-acp-prompt-skill");
    prompt_skill.realization_id = "realize-mcp-acp-prompt-skill".into();
    prompt_skill.stage_idempotency_key = "stage-mcp-acp-prompt-skill".into();
    prompt_skill.prompts_as_skills = true;
    assert_eq!(
        acp_managed
            .stage_mcp_attachment(prompt_skill)
            .await
            .unwrap_err()
            .code,
        "mcp_prompt_skills_unsupported",
        "H24 backend-independent Managed rejection"
    );
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-prompt-skill"))
            .is_none(),
        "H24"
    );
    acp_managed
        .stage_mcp_attachment(anonymous)
        .await
        .expect("H13");
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-anonymous"))
            .and_then(|projection| projection.server)
            .is_some_and(|server| server.bearer().is_none()),
        "H13"
    );

    // H20 is the Host-side positive half of EF11. The provider's own
    // substitution/no-bypass implementation remains an independently testable
    // production-adapter responsibility; this fixture supplies its exact
    // capability evidence and proves the Host consumes that single public seam
    // without a second MCP credential/provider contract.
    struct SecureExternalProvider;

    #[async_trait::async_trait]
    impl awaken_sandbox_container::ContainerEnvironmentProvider for SecureExternalProvider {
        async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
            Ok(())
        }

        fn sandbox_capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
            managed_test_container_capabilities()
        }

        async fn create_environment(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
        ) -> Result<
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::new(
                "H20 exercises pre-environment MCP staging only",
            ))
        }

        async fn adopt_environment(
            &self,
            _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        ) -> Result<
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::new(
                "H20 exercises pre-environment MCP staging only",
            ))
        }
    }

    struct UnusedHandFactory;

    impl crate::HandExecutorFactory for UnusedHandFactory {
        fn bind(
            &self,
            _channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
            _operation_scope: &str,
            _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
        ) -> Arc<dyn awaken_runtime_contract::tool::ToolExecutor> {
            panic!("H20 does not launch an Agent process")
        }
    }

    let secure_acp_host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_acp(executor.clone())
            .with_session_container_provider(
                Arc::new(SecureExternalProvider),
                Arc::new(UnusedHandFactory),
            ),
    );
    secure_acp_host.register_thread_backend_projection("mcp-acp-secure", "acp:test");
    secure_acp_host.register_thread_backend_projection("mcp-acp-client", "acp:claude");
    secure_acp_host.register_thread_backend_projection("mcp-acp-refresh", "acp:claude");
    secure_acp_host.register_thread_agent_projection("mcp-acp-secure", "assistant");
    secure_acp_host
        .retain_session_publication("mcp-acp-secure", Some(&host_acp_snapshot))
        .expect("H20 exact HostExecutor publication");

    let backend_owned_snapshot =
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
            .resolved_model(
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
                    awaken_runtime_contract::resolved::ModelBinding::new("local", "", "acp:test"),
                    awaken_runtime_contract::CredentialRef {
                        id: "backend-login".into(),
                        revision: 1,
                    },
                    awaken_runtime_contract::resolved::BackendModelSelection::Default,
                    "test",
                    "sha256:test-capability",
                    Default::default(),
                )
                .expect("coherent BackendOwned fixture"),
            )
            .build();

    let mut backend_secure_host =
        SharedHost::new(Arc::new(OkModel), "stub").with_acp(executor.clone());
    backend_secure_host.backend_owned_session_provider = Some(
        crate::session_environment::SessionEnvironmentProvider::container(
            Arc::new(SecureExternalProvider),
            Vec::new(),
            Arc::new(UnusedHandFactory),
            "/bin/sh",
        ),
    );
    let backend_secure_host = Arc::new(backend_secure_host);
    backend_secure_host.register_thread_backend_projection("mcp-acp-backend-secure", "acp:test");
    backend_secure_host.register_thread_agent_projection("mcp-acp-backend-secure", "assistant");
    backend_secure_host
        .retain_session_publication("mcp-acp-backend-secure", Some(&backend_owned_snapshot))
        .expect("H26 frozen BackendOwned publication");
    let backend_secure = crate::ManagedHost::new(backend_secure_host.clone())
        .with_credentials(credentials.clone(), secrets.clone());
    backend_secure
        .stage_mcp_attachment(request("mcp-acp-backend-secure", "workspace-a", 1))
        .await
        .expect("H26 selected BackendOwned provider admits WorkerRelay");

    let weak_backend_root = tempfile::tempdir().expect("H27 weak BackendOwned root");
    let mut default_secure_host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_acp(executor.clone())
        .with_session_container_provider(
            Arc::new(SecureExternalProvider),
            Arc::new(UnusedHandFactory),
        );
    default_secure_host.backend_owned_session_provider = Some(
        crate::session_environment::SessionEnvironmentProvider::workdir(weak_backend_root.path()),
    );
    let default_secure_host = Arc::new(default_secure_host);
    default_secure_host.register_thread_backend_projection("mcp-acp-backend-weak", "acp:test");
    default_secure_host.register_thread_agent_projection("mcp-acp-backend-weak", "assistant");
    default_secure_host
        .retain_session_publication("mcp-acp-backend-weak", Some(&backend_owned_snapshot))
        .expect("H27 frozen BackendOwned publication");
    let error = crate::ManagedHost::new(default_secure_host.clone())
        .stage_mcp_attachment(request("mcp-acp-backend-weak", "workspace-a", 1))
        .await
        .expect_err("H27 selected BackendOwned provider must reject WorkerRelay");
    assert_eq!(error.code, "mcp_holder_unsupported", "H27");
    assert!(
        default_secure_host
            .mcp_projection(&generation("mcp-acp-backend-weak"))
            .is_none(),
        "H27 rejects before materialization"
    );

    let secure_managed =
        crate::ManagedHost::new(secure_acp_host.clone()).with_credentials(credentials, secrets);
    let secure_generation = generation("mcp-acp-secure");
    let secure_receipt = secure_managed
        .stage_mcp_attachment(request("mcp-acp-secure", "workspace-a", 1))
        .await
        .expect("H20 complete provider evidence admits exact Worker relay staging");
    assert_eq!(
        secure_receipt.actual_realization_kind,
        Some(awaken_runtime_contract::CredentialRealizationKind::WorkerRelay),
        "H20"
    );
    secure_managed
        .publish_mcp_generation(secure_generation.clone())
        .await
        .expect("H20 publish exact staged route");
    let projection = secure_acp_host
        .mcp_projection(&secure_generation)
        .expect("H20 exact projection");
    let projected = crate::mcp::project_mcp_session_transport(
        projection.server.as_ref().expect("H20 private material"),
        &projection.request,
        secure_acp_host.mcp_relay.get(),
        &projection.receipt,
        awaken_run_executor_acp::acp_cli("codex").unwrap(),
    )
    .expect("H20 project opaque route");
    let route = projected.url.expect("H20 expected HTTP relay route");
    assert!(projected.auth.is_none(), "H20 relay owns auth");
    assert!(
        !route.contains("published-mcp-token"),
        "H20 secret-free route"
    );
    assert!(
        !route.contains(&mcp_url),
        "H20 original target is not exposed"
    );
    let seen_before = seen.lock().unwrap().len();
    let transport = awaken_ext_mcp::HttpTransportBuilder::new(route)
        .credential(awaken_ext_mcp::Credential::None)
        .connect()
        .await
        .expect("H20 initialize through Worker relay");
    let tools = transport.list_tools().await.expect("H20 tools/list");
    assert_eq!(tools.len(), 1, "H20");
    let result = transport
        .call_tool("echo", serde_json::json!({"value": "worker-held"}))
        .await
        .expect("H20 tools/call");
    assert_eq!(
        serde_json::to_value(result).unwrap()["content"][0]["text"],
        "worker-held",
        "H20"
    );

    // H23 proves the distinct ClientInjection branch. The holder and mechanism
    // are exact, no relay route is manufactured, and raw material exists only
    // in the process-local Session field consumed by the declared ACP adapter.
    let mut refreshable = request("mcp-acp-refresh", "workspace-a", 1);
    refreshable.selected_plaintext_holder = Some(workload_holder.clone());
    let access = refreshable.credential.take().unwrap();
    refreshable.credential = Some(access.with_refresh(
        awaken_runtime_contract::CredentialRefreshAccess::new(
            1,
            "https://auth.example/token".into(),
            "client".into(),
            awaken_runtime_contract::TokenEndpointAuth::None,
            None,
            "refresh-ref".into(),
            "access-ref".into(),
            None,
            None,
        ),
    ));
    refreshable.credential.as_mut().unwrap().policy =
        CredentialExecutionPolicy::exact(workload_holder.clone(), ModelExposurePolicy::VirtualOnly);
    assert_eq!(
        secure_managed
            .stage_mcp_attachment(refreshable)
            .await
            .unwrap_err()
            .code,
        "mcp_client_refresh_unsupported",
        "H24 refresh is never silently discarded"
    );
    assert!(
        secure_acp_host
            .mcp_projection(&generation("mcp-acp-refresh"))
            .is_none(),
        "H24"
    );
    let mut client_request = request("mcp-acp-client", "workspace-a", 1);
    client_request.selected_plaintext_holder = Some(workload_holder.clone());
    client_request.credential.as_mut().unwrap().policy =
        CredentialExecutionPolicy::exact(workload_holder, ModelExposurePolicy::VirtualOnly);
    let client_receipt = secure_managed
        .stage_mcp_attachment(client_request)
        .await
        .expect("H23 exact process-protocol client injection");
    assert_eq!(
        client_receipt.actual_realization_kind,
        Some(awaken_runtime_contract::CredentialRealizationKind::ProcessProtocolField),
        "H23"
    );
    secure_managed
        .publish_mcp_generation(client_receipt.generation.clone())
        .await
        .expect("H23 publish");
    let client_projection = secure_acp_host
        .mcp_projection(&client_receipt.generation)
        .expect("H23 projection");
    let client_server = crate::mcp::project_mcp_session_transport(
        client_projection
            .server
            .as_ref()
            .expect("H23 private material"),
        &client_projection.request,
        secure_acp_host.mcp_relay.get(),
        &client_projection.receipt,
        awaken_run_executor_acp::acp_cli("claude").unwrap(),
    )
    .expect("H23 exact ACP Session projection");
    assert_eq!(client_server.url.as_deref(), Some(mcp_url.as_str()), "H23");
    assert_eq!(
        client_server
            .auth
            .as_ref()
            .map(|(name, value)| (name.as_str(), value.as_str())),
        Some(("Authorization", "Bearer published-mcp-token")),
        "H23"
    );
    assert!(
        !format!("{client_server:?}").contains("published-mcp-token"),
        "H23"
    );
    let seen = seen.lock().unwrap();
    assert!(seen.len() > seen_before, "H20 route reached upstream");
    assert!(
        seen[seen_before..]
            .iter()
            .all(|(_, bearer)| bearer == "Bearer published-mcp-token"),
        "H20 every relay request uses only Worker-held material: {seen:?}"
    );
}

#[tokio::test]
async fn published_mcp_envelope_uses_the_authoring_target_binding_at_realization() {
    use awaken_credential_contract::{
        CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
        ResolvedCredentialMaterial,
    };
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_contract::McpAttachmentRealizer;

    struct ExactTargetResolver {
        expected: awaken_credential_contract::CredentialMaterialBinding,
        holder: PlaintextHolder,
    }

    #[async_trait::async_trait]
    impl CredentialMaterialResolver for ExactTargetResolver {
        fn supported_material_sources(
            &self,
        ) -> std::collections::BTreeSet<CredentialMaterialSource> {
            [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect()
        }

        fn supports_recipient_bound_envelopes(&self) -> bool {
            true
        }

        async fn resolve_exact(
            &self,
            request: CredentialMaterialRequest<'_>,
        ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
            if request.binding != &self.expected {
                return Err(CredentialMaterialError::BindingMismatch);
            }
            Ok(ResolvedCredentialMaterial {
                credential: request.access.credential.clone(),
                holder: self.holder.clone(),
                material: awaken_runtime_contract::CredentialMaterial::secret(
                    awaken_agent_contract::RedactedString::new("mcp-bearer"),
                ),
            })
        }
    }

    let workspace = "workspace-a";
    let (mcp_url, _seen) = crate::test_mcp::start(Some("Bearer mcp-bearer")).await;
    let target =
        awaken_session_contract::McpTarget::parse_http(&mcp_url).expect("canonical MCP target");
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    let usage = CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    };
    let access = CredentialAccess::new(
        CredentialRef {
            id: "flow-mcp".into(),
            revision: 1,
        },
        CredentialMaterialSource::ControlPlaneReference,
        usage.clone(),
        CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
    )
    .with_target(awaken_runtime_contract::CredentialTarget::new(
        awaken_credential_contract::CredentialPurpose::McpAuthorization,
        awaken_session_contract::McpTarget::identity(target.http_url().unwrap())
            .unwrap()
            .canonical_url(),
    ));
    let resolver = Arc::new(ExactTargetResolver {
        expected: awaken_credential_contract::CredentialMaterialBinding::for_target(
            workspace, &target, &usage,
        ),
        holder: holder.clone(),
    });
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host).with_credential_materializer(
        awaken_credential_materializer::PinnedCredentialMaterializer::external_only(resolver),
    );

    // Cause/effect decision table:
    // | Rule | authoring binding | runtime server label | runtime target | Effect |
    // | R1 | exact target | arbitrary display name | same target | stage succeeds |
    // The display name is not credential authority and must not enter the
    // recipient-bound payload fingerprint produced during Session authoring.
    managed
        .stage_mcp_attachment(awaken_session_contract::StageMcpAttachment {
            workspace_id: workspace.into(),
            generation: awaken_session_contract::McpGenerationRef {
                session_id: "session-a".into(),
                attachment_id: awaken_session_contract::McpAttachmentId("flow".into()),
                generation: awaken_session_contract::McpGeneration(1),
                runtime_incarnation: "runtime-a".into(),
                lease_epoch: 1,
                lease_expires_at_unix_ms: u64::MAX,
            },
            realization_id: "realize-flow".into(),
            stage_idempotency_key: "stage-flow".into(),
            name: "display-name-not-in-binding".into(),
            target,
            credential: Some(access),
            prompts_as_skills: false,
            selected_plaintext_holder: Some(holder),
        })
        .await
        .expect("R1 exact authoring target binding");
}

#[test]
fn dispatch_session_runtime_requires_explicit_installation() {
    // Cause/effect graph: C1 a ManagedHost is only constructed; C2 the fully
    // configured adapter is explicitly installed. Effects: E1 SharedHost has no
    // dispatch adapter; E2 SharedHost exposes exactly the installed adapter.
    // Constraint: C2 follows C1. Decision table: R1 C1&&!C2 -> E1; R2 C1&&C2
    // -> E2. This guards against constructor/builder side effects reintroducing
    // partially configured durable-dispatch state.
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());

    assert!(host.dispatch_session_runtime().is_err(), "R1/E1");
    let _managed = managed.install_dispatch_session_runtime();
    assert!(host.dispatch_session_runtime().is_ok(), "R2/E2");
}

/// Public-realizer selection cause graph:
///
/// C1 an external realizer is installed -> every MCP lifecycle effect is sent
/// to it. C2 that realizer rejects staging -> the error is terminal and the
/// local Host projection remains untouched. Without C1, the canonical local
/// Host realizer remains the sole implementation.
///
/// | Rule | external | external stage | Expected path | Local fallback |
/// |---|---|---|---|---|
/// | R1 | yes | success | external stage/publish/drain | never |
/// | R2 | yes | failure | exact external error | never |
#[tokio::test]
async fn injected_mcp_realizer_is_exclusive_and_fails_without_local_fallback() {
    use awaken_session_contract::{
        McpAttachmentId, McpAttachmentRealizer, McpGeneration, McpGenerationRef,
        McpRealizationReceipt, McpTarget, StageMcpAttachment,
    };

    #[derive(Default)]
    struct RecordingRealizer {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_stage: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl McpAttachmentRealizer for RecordingRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: StageMcpAttachment,
        ) -> Result<McpRealizationReceipt, awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("stage");
            if self.fail_stage.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(awaken_session_contract::RunError::classified(
                    "external_stage_rejected",
                    "external realizer rejected stage",
                ));
            }
            let receipt_fingerprint = request.fingerprint();
            Ok(McpRealizationReceipt {
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
                receipt_fingerprint,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), awaken_session_contract::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let external = Arc::new(RecordingRealizer::default());
    let _managed = crate::ManagedHost::new(host.clone())
        .with_mcp_attachment_realizer(external.clone())
        .install_dispatch_session_runtime();
    let generation = McpGenerationRef {
        session_id: "external-mcp".into(),
        attachment_id: McpAttachmentId("docs".into()),
        generation: McpGeneration(1),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: McpTarget::parse_http("https://mcp.example.test/sse").unwrap(),
        credential: None,
        prompts_as_skills: false,
        selected_plaintext_holder: None,
    };

    host.stage_dispatched_mcp(request.clone())
        .await
        .expect("R1");
    host.publish_dispatched_mcp(generation.clone())
        .await
        .expect("R1");
    host.drain_dispatched_mcp(generation.clone())
        .await
        .expect("R1");
    assert_eq!(
        *external.calls.lock().unwrap(),
        ["stage", "publish", "drain"]
    );
    assert!(
        host.mcp_projection(&generation).is_none(),
        "R1 no local path"
    );

    external
        .fail_stage
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = host
        .stage_dispatched_mcp(request)
        .await
        .expect_err("R2 external rejection is terminal");
    assert_eq!(error.code, "external_stage_rejected");
    assert!(host.mcp_projection(&generation).is_none(), "R2 no fallback");
}

/// Native and ACP are projections of the same exact durable generation.  This
/// test deliberately drives the Host projection state directly: transport
/// differences may change how a visible generation is consumed, but may never
/// change which generation is visible.
#[tokio::test]
async fn native_and_acp_project_the_same_generation_across_hot_replacement() {
    use crate::mcp::{McpTransportMaterial, McpWiring, project_mcp_session_transport};
    use crate::session_slot::{McpGenerationProjection, McpProjectionState};
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt,
    };

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let generation = |number: u64| McpGenerationRef {
        session_id: "mcp-parity".into(),
        attachment_id: McpAttachmentId("mcp-docs".into()),
        generation: McpGeneration(number),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 7,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let projection = |number: u64, secret: &str| {
        let request = awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation(number),
            realization_id: format!("realize-{number}"),
            stage_idempotency_key: format!("stage-{number}"),
            name: "docs".into(),
            target: awaken_session_contract::McpTarget::parse_http(format!(
                "https://mcp-{number}.example.test"
            ))
            .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        McpGenerationProjection {
            receipt: McpRealizationReceipt {
                generation: request.generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: None,
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            },
            request,
            server: Some(McpTransportMaterial {
                name: "docs".into(),
                prompts_as_skills: false,
                transport: crate::mcp::McpTransportMaterialKind::Http {
                    url: format!("https://mcp-{number}.example.test"),
                    bearer: Some(awaken_agent_contract::RedactedString::new(secret)),
                    refresh: None,
                },
            }),
            native_wiring: Some(McpWiring {
                plugins: Vec::new(),
                tool_ids: vec![format!("docs-generation-{number}")],
                skill_registries: Vec::new(),
                call_fences: Vec::new(),
            }),
            mcp_process: None,
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Staged,
        }
    };
    let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
    assert!(
        host.mcp_relay.set(relay.clone()).is_ok(),
        "install the canonical Host relay"
    );

    // Cause graph:
    // exact generation + private route staged --publish--> active/visible --replacement publish-->
    // old draining + new active --old drain--> old route removed.  Both adapter
    // projections consume only `active_mcp_projections`; ACP additionally turns
    // that exact generation into a loopback route without a credential.
    //
    // | Rule | g1 state | g2 state | Native visible | ACP visible | old route |
    // |------|----------|----------|----------------|-------------|-----------|
    // | P1   | staged   | absent   | none           | none        | staged/private |
    // | P2   | active   | absent   | g1             | g1          | present   |
    // | P3   | active   | staged   | g1             | g1          | present   |
    // | P4   | draining | active   | g2             | g2          | present   |
    // | P5   | removed  | active   | g2             | g2          | absent    |
    host.insert_mcp_projection(projection(1, "secret-one"))
        .unwrap();
    let staged = host.mcp_projection(&generation(1)).unwrap();
    relay.set_route(&staged.request.generation, staged.server.as_ref().unwrap());
    let staged_route = relay.route_url(&generation(1)).expect("P1 staged route");
    assert!(host.active_mcp_projections("mcp-parity").is_empty(), "P1");

    host.publish_mcp_projection(&generation(1)).await.unwrap();
    let visible = host.active_mcp_projections("mcp-parity");
    assert_eq!(
        visible[0].native_wiring.as_ref().unwrap().tool_ids[0],
        "docs-generation-1",
        "P2"
    );
    let acp = project_mcp_session_transport(
        visible[0].server.as_ref().unwrap(),
        &visible[0].request,
        Some(&relay),
        &visible[0].receipt,
        awaken_run_executor_acp::acp_cli("codex").unwrap(),
    )
    .unwrap();
    assert!(acp.auth.is_none(), "P2 legacy route remains secret-free");
    let old_route = acp.url.expect("P2 expected HTTP transport");
    assert_eq!(old_route, staged_route, "P2 publish reuses staged effect");
    assert!(old_route.contains("/mcp-parity/mcp-docs/1/"), "P2");

    host.insert_mcp_projection(projection(2, "secret-two"))
        .unwrap();
    let staged = host.mcp_projection(&generation(2)).unwrap();
    relay.set_route(&staged.request.generation, staged.server.as_ref().unwrap());
    assert_eq!(
        host.active_mcp_projections("mcp-parity")[0]
            .request
            .generation
            .generation,
        McpGeneration(1),
        "P3"
    );

    host.publish_mcp_projection(&generation(2)).await.unwrap();
    let visible = host.active_mcp_projections("mcp-parity");
    assert_eq!(visible.len(), 1, "P4");
    assert_eq!(
        visible[0].native_wiring.as_ref().unwrap().tool_ids[0],
        "docs-generation-2",
        "P4"
    );
    let replacement = project_mcp_session_transport(
        visible[0].server.as_ref().unwrap(),
        &visible[0].request,
        Some(&relay),
        &visible[0].receipt,
        awaken_run_executor_acp::acp_cli("codex").unwrap(),
    )
    .unwrap();
    assert!(
        replacement.auth.is_none(),
        "P4 legacy route remains secret-free"
    );
    let new_route = replacement.url.expect("P4 expected HTTP transport");
    assert!(new_route.contains("/mcp-parity/mcp-docs/2/"), "P4");
    assert_ne!(old_route, new_route, "P4");

    host.drain_mcp_projection(&generation(1)).await.unwrap();
    assert_eq!(
        host.active_mcp_projections("mcp-parity")[0]
            .request
            .generation
            .generation,
        McpGeneration(2),
        "P5"
    );
    assert_eq!(
        reqwest::Client::new()
            .post(old_route)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND,
        "P5"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_drain_acknowledges_removed_only_after_busy_call_quiesces() {
    use std::sync::{Arc, Condvar, Mutex};

    use async_trait::async_trait;
    use awaken_ext_mcp::transport::{McpToolTransport, revocable_transport};
    use awaken_ext_mcp::{CallToolResult, McpToolDefinition, McpTransportError};
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
    };

    use crate::mcp::{McpTransportMaterial, McpTransportMaterialKind, McpWiring};
    use crate::session_slot::{McpGenerationProjection, McpProjectionState};

    struct DropGate(Arc<(Mutex<bool>, Condvar)>);

    impl Drop for DropGate {
        fn drop(&mut self) {
            let (released, signal) = self.0.as_ref();
            let mut released = released.lock().expect("drop gate mutex");
            while !*released {
                released = signal.wait(released).expect("drop gate wait");
            }
        }
    }

    struct BusyTransport {
        started: Arc<tokio::sync::Notify>,
        drop_gate: Arc<(Mutex<bool>, Condvar)>,
    }

    #[async_trait]
    impl McpToolTransport for BusyTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: serde_json::Value,
        ) -> Result<CallToolResult, McpTransportError> {
            let _drop_gate = DropGate(self.drop_gate.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }

    // Cause/effect graph: C0 the busy task may enter before this test begins
    // awaiting its single start signal; C1 one exact generation is Active; C2 one local tool
    // call is in flight; C3 drain is requested; C4 cancellation has begun but
    // the call future has not completed its drop. Effects: E1 new visibility is
    // closed immediately; E2 state remains Draining and no Removed receipt can
    // be observed during C4; E3 releasing the last call guard permits Removed;
    // E4 the busy call terminates with revocation instead of producing a late
    // result. Constraints: C0 uses one retained Notify permit, never a broadcast
    // that can disappear before registration. Decision rules:
    // Q0 C0=>the start observation is retained; Q1 C1+C2+C3+C4=>E1+E2;
    // Q2 Q1+quiesced=>E3+E4.
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let generation = McpGenerationRef {
        session_id: "mcp-busy-drain".into(),
        attachment_id: McpAttachmentId("docs".into()),
        generation: McpGeneration(1),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_session_contract::McpTarget::parse_http("https://mcp.example.test/sse")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let started = Arc::new(tokio::sync::Notify::new());
    let drop_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (transport, fence) = revocable_transport(Arc::new(BusyTransport {
        started: started.clone(),
        drop_gate: drop_gate.clone(),
    }));
    host.insert_mcp_projection(McpGenerationProjection {
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: Some(McpTransportMaterial {
            name: "docs".into(),
            prompts_as_skills: false,
            transport: McpTransportMaterialKind::Http {
                url: "https://mcp.example.test/sse".into(),
                bearer: None,
                refresh: None,
            },
        }),
        native_wiring: Some(McpWiring {
            plugins: Vec::new(),
            tool_ids: Vec::new(),
            skill_registries: Vec::new(),
            call_fences: vec![fence],
        }),
        mcp_process: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Active,
    })
    .unwrap();

    let busy_call =
        tokio::spawn(async move { transport.call_tool("busy", serde_json::Value::Null).await });
    started.notified().await;
    let drain_host = host.clone();
    let drain_generation = generation.clone();
    let drain =
        tokio::spawn(async move { drain_host.drain_mcp_projection(&drain_generation).await });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if host
                .mcp_projection(&generation)
                .is_some_and(|projection| projection.state == McpProjectionState::Draining)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Q1 drain enters Draining");
    assert!(
        !drain.is_finished(),
        "Q1/E2 no early Removed acknowledgement"
    );

    let (released, signal) = drop_gate.as_ref();
    *released.lock().expect("release mutex") = true;
    signal.notify_all();
    assert!(busy_call.await.unwrap().is_err(), "Q2/E4 busy call revoked");
    drain.await.unwrap().unwrap();
    assert_eq!(
        host.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Removed,
        "Q2/E3"
    );
}

#[tokio::test(start_paused = true)]
async fn worker_relay_drain_retains_exact_route_across_cancel_and_timeout_retry() {
    use awaken_runtime_contract::CredentialRealizationKind;
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
    };

    use crate::mcp::{McpTransportMaterial, McpTransportMaterialKind};
    use crate::session_slot::{McpGenerationProjection, McpProjectionState};

    // Route-drain cause/effect table (paired with mcp_relay's actual-forward
    // acceptance test):
    // | Rule | exact relay permit | drain attempt | Effect |
    // | R1 | active | Future cancelled | Draining + same closed Route retained; no proof |
    // | R2 | active | outer revoke wait expires | Unavailable; Draining + owner retained; no proof |
    // | R3 | settled | retry | exact Route removed, then projection Removed + proof |
    // C1 the receipt is WorkerRelay and C2 this is its exact generation are
    // constraints for every rule. E1 no new route permit is admitted after the
    // first close. Cancellation/timeout never manufactures a different fence,
    // route registry, or Removed receipt.
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
    assert!(host.mcp_relay.set(relay.clone()).is_ok());
    let generation = McpGenerationRef {
        session_id: "mcp-worker-relay-drain".into(),
        attachment_id: McpAttachmentId("docs".into()),
        generation: McpGeneration(1),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_session_contract::McpTarget::parse_http("https://mcp.example.test/sse")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let server = McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: McpTransportMaterialKind::Http {
            url: "https://mcp.example.test/sse".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("relay-secret")),
            refresh: None,
        },
    };
    relay.set_route(&generation, &server);
    let permit = relay
        .route_call_fence(&generation)
        .unwrap()
        .try_enter()
        .expect("accepted exact route activity");
    host.insert_mcp_projection(McpGenerationProjection {
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: Some(CredentialRealizationKind::WorkerRelay),
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: Some(server),
        native_wiring: None,
        mcp_process: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Active,
    })
    .unwrap();

    let cancelled_host = host.clone();
    let cancelled =
        tokio::spawn(async move { cancelled_host.revoke_all_session_realizations().await });
    loop {
        if host
            .mcp_projection(&generation)
            .is_some_and(|projection| projection.state == McpProjectionState::Draining)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancelled.abort();
    let _ = cancelled.await;
    assert!(
        relay.route_url(&generation).is_some(),
        "R1 exact Route retained"
    );
    assert!(
        relay
            .route_call_fence(&generation)
            .unwrap()
            .try_enter()
            .is_none(),
        "R1/E1 closed admission is retry-stable"
    );
    assert_eq!(
        host.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Draining,
        "R1 no Removed proof"
    );

    let timeout = host
        .revoke_all_session_realizations()
        .await
        .expect_err("R2 outer revoke must propagate held route timeout");
    assert_eq!(timeout.code, "mcp_generation_call_quiescence_timeout", "R2");
    assert!(relay.route_url(&generation).is_some(), "R2 owner retained");
    assert_eq!(
        host.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Draining,
        "R2 no Removed proof"
    );

    drop(permit);
    let proof = host
        .drain_mcp_projections(&generation.session_id, std::slice::from_ref(&generation))
        .await
        .expect("R3 settled retry");
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&generation),
        "R3 proof"
    );
    assert!(
        relay.route_url(&generation).is_none(),
        "R3 exact Route removed"
    );
    assert_eq!(
        host.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Removed,
        "R3 Removed follows route quiescence"
    );
}

#[tokio::test(start_paused = true)]
async fn mcp_quiescence_retains_failed_effect_owners_for_retry() {
    use async_trait::async_trait;
    use awaken_provisioning_contract as pc;
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
    };

    use crate::session_slot::{McpGenerationProjection, McpProjectionState};

    struct Process {
        id: String,
        unreapable: bool,
    }

    #[async_trait]
    impl pc::ProcessHandle for Process {
        fn id(&self) -> &str {
            &self.id
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            if self.unreapable {
                std::future::pending().await
            } else {
                Ok(pc::ExitStatus {
                    code: Some(0),
                    signaled: false,
                })
            }
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            if self.unreapable {
                Ok(None)
            } else {
                Ok(Some(self.wait().await?))
            }
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            if self.unreapable {
                Err(pc::SandboxError::new("scripted MCP signal failure"))
            } else {
                Ok(())
            }
        }
    }

    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let generation = |session: &str, number| McpGenerationRef {
        session_id: session.into(),
        attachment_id: McpAttachmentId(format!("mcp-{number}")),
        generation: McpGeneration(number),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let projection = |generation: McpGenerationRef, unreapable| {
        let request = StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation.clone(),
            realization_id: format!("realize-{}", generation.generation.0),
            stage_idempotency_key: format!("stage-{}", generation.generation.0),
            name: format!("mcp-{}", generation.generation.0),
            target: awaken_session_contract::McpTarget::parse_http(format!(
                "https://mcp-{}.example.test",
                generation.generation.0
            ))
            .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        McpGenerationProjection {
            receipt: McpRealizationReceipt {
                generation: generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: None,
                actual_realization_kind: None,
                receipt_fingerprint: request.fingerprint(),
            },
            request,
            server: None,
            native_wiring: None,
            mcp_process: Some(Arc::new(Process {
                id: format!("process-{}", generation.generation.0),
                unreapable,
            })),
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state: McpProjectionState::Active,
        }
    };
    let stuck = generation("mcp-partial-quiescence", 1);
    let clean = generation("mcp-partial-quiescence", 2);
    host.insert_mcp_projection(projection(stuck.clone(), true))
        .unwrap();
    host.insert_mcp_projection(projection(clean.clone(), false))
        .unwrap();

    // Cause/effect decision table:
    // | Rule | C2 exact expected | C3 process reap | C6 cancellation | Effect |
    // | P1 | both generations | first fails, second succeeds | no | no proof;
    // |    |                  |                             |    | first owner retained Draining, second Removed |
    // The loop must continue after P1's first failure so partial cleanup never
    // strands an independently reapable generation.
    assert!(
        host.drain_mcp_projections("mcp-partial-quiescence", &[stuck.clone(), clean.clone()])
            .await
            .is_err(),
        "P1"
    );
    let stuck_projection = host.mcp_projection(&stuck).unwrap();
    assert_eq!(stuck_projection.state, McpProjectionState::Draining, "P1");
    assert!(stuck_projection.mcp_process.is_some(), "P1 owner retained");
    assert_eq!(
        host.mcp_projection(&clean).unwrap().state,
        McpProjectionState::Removed,
        "P1 partial progress"
    );

    let runtime_thread = "mcp-runtime-quiescence";
    let (runtime_host, _runtime_managed) =
        dispatch_test_host(SharedHost::new(Arc::new(OkModel), "stub"));
    let runtime = runtime_host
        .ctx_for(runtime_thread, None)
        .await
        .expect("runtime owner");
    *runtime.active_run.lock().expect("active run mutex") =
        Some(RunId("mcp-runtime-active-run".into()));
    let runtime_generation = generation(runtime_thread, 3);
    runtime_host
        .insert_mcp_projection(projection(runtime_generation.clone(), false))
        .unwrap();

    // Runtime-owner cause/effect decision table:
    // | Rule | C4 active Run | C6 drain cancellation/timeout | Effect |
    // | P2 | yes | Future cancelled after Draining | same slot Runtime retained; no proof |
    // | P3 | yes | retry reaches timeout | same Runtime + Draining process retained; no proof |
    // | P4 | settled | retry | process Removed + exact proof; outer quiesce still owns Runtime removal |
    // P2/P3 prevent a retry from losing the only active-run fence and turning a
    // failed attempt into false quiescence. P4 preserves the single existing
    // Session-slot owner until the whole Environment transaction succeeds.
    let cancelled_host = runtime_host.clone();
    let cancelled_generation = runtime_generation.clone();
    let cancelled = tokio::spawn(async move {
        cancelled_host
            .drain_mcp_projections(runtime_thread, &[cancelled_generation])
            .await
    });
    loop {
        if runtime_host
            .mcp_projection(&runtime_generation)
            .is_some_and(|projection| projection.state == McpProjectionState::Draining)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    cancelled.abort();
    let _ = cancelled.await;
    assert!(
        runtime_host
            .session_slots
            .read(runtime_thread, |slot| slot
                .runtime
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &runtime)))
            .unwrap_or(false),
        "P2 retains the exact Runtime owner"
    );
    assert_eq!(
        runtime_host
            .mcp_projection(&runtime_generation)
            .unwrap()
            .state,
        McpProjectionState::Draining,
        "P2"
    );

    let timeout = runtime_host
        .drain_mcp_projections(runtime_thread, std::slice::from_ref(&runtime_generation))
        .await
        .expect_err("P3 active Run must time out");
    assert!(
        timeout
            .to_string()
            .contains("could not quiesce the active Session Run"),
        "P3 reports the active owner"
    );
    assert!(
        runtime_host
            .session_slots
            .read(runtime_thread, |slot| slot
                .runtime
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &runtime)))
            .unwrap_or(false),
        "P3 retains the exact Runtime owner"
    );
    assert!(
        runtime_host
            .mcp_projection(&runtime_generation)
            .is_some_and(
                |projection| projection.state == McpProjectionState::Draining
                    && projection.mcp_process.is_some()
            ),
        "P3 retains the process owner"
    );

    *runtime.active_run.lock().expect("active run mutex") = None;
    let proof = runtime_host
        .drain_mcp_projections(runtime_thread, std::slice::from_ref(&runtime_generation))
        .await
        .expect("P4 settled retry");
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&runtime_generation),
        "P4"
    );
    assert_eq!(
        runtime_host
            .mcp_projection(&runtime_generation)
            .unwrap()
            .state,
        McpProjectionState::Removed,
        "P4"
    );
    assert!(
        runtime_host
            .session_slots
            .read(runtime_thread, |slot| slot
                .runtime
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &runtime)))
            .unwrap_or(false),
        "P4 leaves Runtime removal to the outer quiesce owner"
    );
}

#[tokio::test]
async fn mcp_quiescence_waits_for_staging_activity_before_removed_proof() {
    use async_trait::async_trait;
    use awaken_provisioning_contract as pc;
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
    };

    use crate::session_slot::{McpGenerationProjection, McpProjectionState, McpStagingActivity};

    struct Process {
        reaped: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl pc::ProcessHandle for Process {
        fn id(&self) -> &str {
            "mcp-staging-process"
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            self.reaped.store(true, Ordering::SeqCst);
            Ok(pc::ExitStatus {
                code: Some(0),
                signaled: false,
            })
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(Some(self.wait().await?))
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let generation = McpGenerationRef {
        session_id: "mcp-staging-quiescence".into(),
        attachment_id: McpAttachmentId("docs".into()),
        generation: McpGeneration(1),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 1,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realize-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: awaken_session_contract::McpTarget::parse_http("https://mcp.example.test").unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    let staging = McpStagingActivity::default();
    let projection = McpGenerationProjection {
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: request.realization_id.clone(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: request.fingerprint(),
        },
        request,
        server: None,
        native_wiring: None,
        mcp_process: None,
        staging: Some(staging.clone()),
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Staging,
    };
    host.insert_mcp_projection(projection.clone()).unwrap();

    let sandbox_generation = awaken_session_contract::SandboxGeneration::new(
        "mcp-staging-quiescence",
        1,
        100,
        "environment",
        "base",
    );
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "workspace-a",
        "mcp-staging-quiescence",
        "suspend",
        &sandbox_generation,
        3,
        None,
        None,
    );
    host.session_slots
        .close_mcp_realization_admission(
            "mcp-staging-quiescence",
            crate::session_slot::McpQuiescenceAdmissionFence::new(
                &operation,
                "source-effect",
                "source-binding",
                &sandbox_generation,
            ),
        )
        .unwrap();

    // Cause/effect decision table:
    // | Rule | Stage boundary vs close | Drain/staging result | Effect |
    // | S1 | insert commit after close | not owned | reject; residual set cannot grow |
    // | S2 | inserted before close; spawn commits after close | drain cancelled | tracked Draining process retained; no proof |
    // | S3 | publish/renew after close | not applicable | reject without visibility/claim mutation |
    // | S4 | tracked staging finishes; retry | reap succeeds | Removed + exact proof |
    // | S5 | Unmaterialized projection before/after S4 | residual / none | stay closed / reopen and admit a new stage |
    host.install_session_environment_owner_projection(
        "mcp-staging-quiescence",
        "workspace-a",
        &awaken_session_contract::SessionEnvironmentState::Unmaterialized,
    )
    .unwrap();
    assert!(
        !host
            .session_slots
            .mcp_realization_admitted("mcp-staging-quiescence"),
        "S5 residual staging owner keeps expiry projection closed"
    );
    let mut late_projection = projection.clone();
    late_projection.request.generation.generation = McpGeneration(2);
    late_projection.receipt.generation = late_projection.request.generation.clone();
    let late_generation = late_projection.request.generation.clone();
    assert_eq!(
        host.insert_mcp_projection(late_projection)
            .unwrap_err()
            .code,
        "session_environment_quiescing",
        "S1 late stage commit is rejected by the closed fence, not by an existing owner"
    );
    assert!(
        host.mcp_projection(&late_generation).is_none(),
        "S1 rejected commit cannot grow the residual owner set"
    );
    assert!(
        host.publish_mcp_projection(&generation).await.is_err(),
        "S3 publish fenced"
    );
    assert!(
        host.renew_mcp_projection(&host.mcp_projection(&generation).unwrap().request)
            .is_err(),
        "S3 renewal fenced"
    );
    let drain_host = host.clone();
    let drain_generation = generation.clone();
    let draining = tokio::spawn(async move {
        drain_host
            .drain_mcp_projections("mcp-staging-quiescence", &[drain_generation])
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if host
                .mcp_projection(&generation)
                .is_some_and(|projection| projection.state == McpProjectionState::Draining)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let reaped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    assert!(
        !host
            .attach_staging_mcp_process(
                &generation,
                Arc::new(Process {
                    reaped: reaped.clone(),
                }),
            )
            .unwrap(),
        "S2 spawn transfers to the Draining owner"
    );
    assert!(
        host.complete_staging_mcp_projection(&generation, crate::mcp::McpWiring::empty())
            .is_err(),
        "S2 commit after close is rejected"
    );
    assert!(!draining.is_finished(), "S2");
    draining.abort();
    let _ = draining.await;
    assert_eq!(
        host.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Draining,
        "S2 cancellation retains owner"
    );
    assert!(
        host.mcp_projection(&generation)
            .unwrap()
            .mcp_process
            .is_some(),
        "S2 process owner retained"
    );
    staging.finish();
    let proof = host
        .drain_mcp_projections("mcp-staging-quiescence", std::slice::from_ref(&generation))
        .await
        .unwrap();
    assert_eq!(
        proof.generations.as_slice(),
        std::slice::from_ref(&generation),
        "S4"
    );
    assert_eq!(
        host.mcp_projection(&generation).unwrap().state,
        McpProjectionState::Removed,
        "S4"
    );
    assert!(reaped.load(Ordering::SeqCst), "S4 process reaped");
    host.install_session_environment_owner_projection(
        "mcp-staging-quiescence",
        "workspace-a",
        &awaken_session_contract::SessionEnvironmentState::Unmaterialized,
    )
    .unwrap();
    assert!(
        host.session_slots
            .mcp_realization_admitted("mcp-staging-quiescence"),
        "S5 exact source-free projection reopens only after residual cleanup"
    );
    let removed_request = host.mcp_projection(&generation).unwrap().request;
    assert!(
        host.forget_exact_removed_mcp_projection(&removed_request),
        "S5 old Removed owner can be forgotten after reopen"
    );
    host.insert_mcp_projection(projection)
        .expect("S5 new realization is admitted after exact reopen");
}

/// MCP-effect authority cause/effect graph: C1 the asserted generation lease is
/// live; C2 the local Control projection has the same Runtime incarnation and
/// epoch; C3 that projection monotonically extends the asserted expiry; C4 the
/// projected lease is live. Effects: E1 permit the already-admitted effect; E2
/// fence it before any MCP I/O.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// |---|---|---|---|---|---|
/// | A1 | yes | any | any | any | E1 |
/// | A2 | no | yes | yes | yes | E1 |
/// | A3 | no | no | any | yes | E2 |
/// | A4 | no | yes | no | yes | E2 |
/// | A5 | no | yes | yes | no | E2 |
/// | A6 | no | absent | absent | absent | E2 |
#[test]
fn mcp_effect_authority_accepts_only_a_live_assertion_or_its_live_same_epoch_renewal() {
    let generation = awaken_session_contract::McpGenerationRef {
        session_id: "mcp-effect-authority".into(),
        attachment_id: awaken_session_contract::McpAttachmentId("browser".into()),
        generation: awaken_session_contract::McpGeneration(1),
        runtime_incarnation: "runtime-a/boot-1".into(),
        lease_epoch: 3,
        lease_expires_at_unix_ms: 101,
    };
    let live = SharedHost::new(Arc::new(OkModel), "stub");
    assert!(
        live.mcp_generation_is_authorized_at(&generation, 100),
        "A1/E1"
    );

    let mut expired = generation;
    expired.lease_expires_at_unix_ms = 90;
    for (rule, lease, expected) in [
        (
            "A2",
            Some(awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-a/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: 110,
            }),
            true,
        ),
        (
            "A3",
            Some(awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-b/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: 110,
            }),
            false,
        ),
        (
            "A4",
            Some(awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-a/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: 80,
            }),
            false,
        ),
        (
            "A5",
            Some(awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "runtime-a/boot-1".into(),
                epoch: 3,
                expires_at_unix_ms: 100,
            }),
            false,
        ),
        ("A6", None, false),
    ] {
        let host = SharedHost::new(Arc::new(OkModel), "stub");
        if let Some(lease) = lease {
            host.install_session_realization_lease(&expired.session_id, lease);
        }
        assert_eq!(
            host.mcp_generation_is_authorized_at(&expired, 100),
            expected,
            "{rule}"
        );
    }
}

#[test]
fn mcp_projection_renewal_updates_the_same_staged_or_active_slot() {
    use crate::session_slot::{McpGenerationProjection, McpProjectionState};
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
    };

    /* Runtime renewal cause/effect decision table. C1 the same logical
     * generation/incarnation/epoch and immutable binding exists; C2 its state is
     * Staged or Active; C3 expiry advances. E1 rewrites that one projection and
     * receipt without reconnecting; E2 no second slot appears. R1 C1+C2(Staged)+
     * C3=>E1+E2 closes renewal during first Stage; R2 C1+C2(Active)+C3=>E1+E2
     * covers steady renewal; R3 no matching projection=>no-op. Conflicting
     * bindings and non-monotonic expiry are rejected by the aggregate N2–N5
     * table and this adapter's shared conflict gate. */
    for (rule, state) in [
        ("R1", McpProjectionState::Staged),
        ("R2", McpProjectionState::Active),
    ] {
        let host = SharedHost::new(Arc::new(OkModel), "stub");
        let old_generation = McpGenerationRef {
            session_id: format!("renew-{rule}"),
            attachment_id: McpAttachmentId("browser".into()),
            generation: McpGeneration(1),
            runtime_incarnation: "runtime-a/boot-1".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: u64::MAX - 1,
        };
        let request = StageMcpAttachment {
            workspace_id: "workspace".into(),
            generation: old_generation.clone(),
            realization_id: "realization-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "browser".into(),
            target: awaken_session_contract::McpTarget::parse_http(
                "https://browser.example.test/mcp",
            )
            .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        host.insert_mcp_projection(McpGenerationProjection {
            receipt: McpRealizationReceipt {
                receipt_fingerprint: request.fingerprint(),
                generation: old_generation.clone(),
                realization_id: request.realization_id.clone(),
                selected_plaintext_holder: None,
                actual_realization_kind: None,
            },
            request: request.clone(),
            server: None,
            native_wiring: None,
            mcp_process: None,
            staging: None,
            drain: Arc::new(tokio::sync::Mutex::new(())),
            state,
        })
        .unwrap();
        let mut renewed = request;
        renewed.generation.lease_expires_at_unix_ms = u64::MAX;
        renewed.stage_idempotency_key = "renew-max".into();
        let receipt = host
            .renew_mcp_projection(&renewed)
            .expect(rule)
            .expect(rule);
        assert_eq!(receipt.generation, renewed.generation, "{rule}/E1");
        assert!(host.mcp_projection(&old_generation).is_none(), "{rule}/E1");
        assert_eq!(
            host.mcp_projection(&renewed.generation).expect(rule).state,
            state,
            "{rule}/E1"
        );
        assert_eq!(
            host.session_slots
                .read(&renewed.generation.session_id, |slot| slot.mcp.len()),
            Some(1),
            "{rule}/E2"
        );
    }

    let absent = SharedHost::new(Arc::new(OkModel), "stub");
    let request = StageMcpAttachment {
        workspace_id: "workspace".into(),
        generation: McpGenerationRef {
            session_id: "renew-absent".into(),
            attachment_id: McpAttachmentId("browser".into()),
            generation: McpGeneration(1),
            runtime_incarnation: "runtime-a/boot-1".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: u64::MAX,
        },
        realization_id: "realization-1".into(),
        stage_idempotency_key: "renew-max".into(),
        name: "browser".into(),
        target: awaken_session_contract::McpTarget::parse_http("https://browser.example.test/mcp")
            .unwrap(),
        prompts_as_skills: false,
        credential: None,
        selected_plaintext_holder: None,
    };
    assert_eq!(absent.renew_mcp_projection(&request).unwrap(), None, "R3");
}

/// Relay effects are created only by MCP staging. Publication may expose an
/// exact staged effect, but must never manufacture the missing route as a
/// compatibility/recovery path.
#[tokio::test]
async fn authenticated_acp_publication_requires_the_exact_staged_relay_route() {
    use crate::mcp::McpTransportMaterial;
    use crate::session_slot::{McpGenerationProjection, McpProjectionState};
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt,
    };

    let generation = McpGenerationRef {
        session_id: "mcp-stage-authority".into(),
        attachment_id: McpAttachmentId("mcp-docs".into()),
        generation: McpGeneration(1),
        runtime_incarnation: "runtime-1".into(),
        lease_epoch: 4,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let server = McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: crate::mcp::McpTransportMaterialKind::Http {
            url: "https://mcp.example.test".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("secret")),
            refresh: None,
        },
    };
    let projection = McpGenerationProjection {
        request: awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "docs".into(),
            target: awaken_session_contract::McpTarget::parse_http("https://mcp.example.test")
                .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        },
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: "receipt-1".into(),
        },
        server: Some(server.clone()),
        // ACP has no in-process MCP connection. This is the stable transport
        // discriminator used by the private Host projection.
        native_wiring: None,
        mcp_process: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Staged,
    };

    // Cause graph:
    // authenticated ACP projection + no relay/route -> reject publication;
    // stage exact private route -> publish exact generation -> active.
    //
    // | Rule | relay | exact route | publish | projection state |
    // |---|---|---|---|---|
    // | S1 | absent | absent | reject | staged |
    // | S2 | present | absent | reject | staged |
    // | S3 | present | present | accept | active |
    let host = SharedHost::new(Arc::new(OkModel), "stub");
    host.insert_mcp_projection(projection).unwrap();
    assert!(
        host.publish_mcp_projection(&generation).await.is_err(),
        "S1"
    );
    assert!(
        host.active_mcp_projections(&generation.session_id)
            .is_empty(),
        "S1"
    );

    let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
    assert!(host.mcp_relay.set(relay.clone()).is_ok(), "S2 setup");
    assert!(
        host.publish_mcp_projection(&generation).await.is_err(),
        "S2"
    );
    assert!(
        host.active_mcp_projections(&generation.session_id)
            .is_empty(),
        "S2"
    );

    relay.set_route(&generation, &server);
    host.publish_mcp_projection(&generation).await.expect("S3");
    assert_eq!(
        host.active_mcp_projections(&generation.session_id).len(),
        1,
        "S3"
    );
}

#[tokio::test]
async fn worker_authority_loss_revokes_every_session_projection() {
    use crate::mcp::{McpTransportMaterial, McpWiring};
    use crate::session_slot::{McpGenerationProjection, McpProjectionState};
    use awaken_session_contract::{
        McpAttachmentId, McpGeneration, McpGenerationRef, McpRealizationReceipt,
    };

    // Cause graph: unprovable Worker authority -> enumerate the canonical
    // process-local Session projections -> nonterminal realization revocation ->
    // route, material, and slot absent while durable environments remain owned
    // by the Session. Repeating the fence is idempotent.
    //
    // | Rule | Authority | Live projections | Effect |
    // |---|---|---|---|
    // | A1 | lost/unprovable | one | detach + revoke route + remove slot |
    // | A2 | lost/unprovable | none | zero/no-op |
    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let generation = McpGenerationRef {
        session_id: "authority-loss".into(),
        attachment_id: McpAttachmentId("mcp-docs".into()),
        generation: McpGeneration(1),
        runtime_incarnation: "worker-a/boot-1".into(),
        lease_epoch: 3,
        lease_expires_at_unix_ms: u64::MAX,
    };
    let server = McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: crate::mcp::McpTransportMaterialKind::Http {
            url: "https://mcp.example.test".into(),
            bearer: Some(awaken_agent_contract::RedactedString::new("secret")),
            refresh: None,
        },
    };
    host.insert_mcp_projection(McpGenerationProjection {
        request: awaken_session_contract::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "docs".into(),
            target: awaken_session_contract::McpTarget::parse_http("https://mcp.example.test")
                .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        },
        receipt: McpRealizationReceipt {
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: "receipt-1".into(),
        },
        server: Some(server.clone()),
        native_wiring: Some(McpWiring::empty()),
        mcp_process: None,
        staging: None,
        drain: Arc::new(tokio::sync::Mutex::new(())),
        state: McpProjectionState::Staged,
    })
    .unwrap();
    host.publish_mcp_projection(&generation).await.unwrap();
    let relay = crate::mcp_relay::McpRelay::start().await.unwrap();
    relay.set_route(&generation, &server);
    assert!(host.mcp_relay.set(relay.clone()).is_ok(), "A1 setup");
    assert!(relay.route_url(&generation).is_some(), "A1 setup");

    assert_eq!(
        host.revoke_all_session_realizations().await.unwrap(),
        1,
        "A1"
    );
    assert!(!host.session_slots.contains("authority-loss"), "A1");
    assert!(relay.route_url(&generation).is_none(), "A1");
    assert_eq!(
        host.revoke_all_session_realizations().await.unwrap(),
        0,
        "A2"
    );
}

/// Repository credentials remain owned by Resource realization. They must not
/// create a hidden MCP desired-state or credential path beside Session MCP
/// generations.
#[tokio::test]
async fn a_github_repository_resource_does_not_create_a_parallel_mcp_projection() {
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(http_basic_material("x-access-token", "ghp_secret_token")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .unwrap();
    let managed = managed_with_resource_source(host.clone())
        .with_credentials(credentials, secrets)
        .install_dispatch_session_runtime();

    managed
        .install_test_session_init(
            "t-gh",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: effective_repository(
                    "repo-1",
                    "https://github.com/awaken/example.git",
                    "/workspace/repo",
                    Some(credential.id.0.clone()),
                ),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();

    // The repo is staged for a host-side clone...
    assert_eq!(
        host.thread_repository_activations("t-gh").len(),
        1,
        "repo staged for cloning"
    );

    let activations = host.thread_repository_activations("t-gh");
    let activation = &activations[0];
    assert_eq!(
        activation
            .credential_pin
            .as_ref()
            .map(|pin| pin.access.credential.id.as_str()),
        Some(credential.id.0.as_str()),
        "the activation retains only the exact secret-free source pin"
    );
    assert!(
        host.active_mcp_projections("t-gh").is_empty(),
        "Repository realization cannot manufacture Session MCP authority"
    );
}

/// Worker Repository effect cause graph: C1 dispatch Session Runtime installed
/// from the Managed adapter -> C2 exact secret-free pin stages -> C3 the Host
/// owns the canonical materializer at the Git edge -> E1 one operation-scoped
/// HTTP Basic value reaches the realizer. Missing C3 preserves E2 (the pin may
/// stage) but terminates before Git without another credential path.
///
/// | Rule | C1 | C2 | C3 | Stage result | Git effect |
/// |---|---|---|---|---|---|
/// | D1 | T | T | T | secret-free activation | exact transient material |
/// | D2 | T | T | F | same secret-free activation | reject, zero realizer calls |
#[tokio::test]
async fn worker_dispatch_resource_runtime_survives_assembly_and_fails_closed() {
    for (rule, install_credentials) in [("D1", true), ("D2", false)] {
        let host = SharedHost::new(Arc::new(OkModel), "stub");
        let workspace = host.local_workspace().to_owned();
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let source = awaken_credential_vault::repo::enter_credential_described(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: workspace.clone(),
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(http_basic_material("git", "dispatch-repository-secret")),
                oauth_command: None,
            },
            repository_credential_descriptor("https://github.com/awaken/example.git"),
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("author dispatch Repository credential");
        let materializer =
            awaken_credential_materializer::PinnedCredentialMaterializer::new(credentials, secrets);
        let host = Arc::new(if install_credentials {
            host.with_credential_materializer(materializer.clone())
        } else {
            host
        });
        let managed = managed_with_resource_source(host.clone());
        let managed = if install_credentials {
            managed.with_credential_materializer(materializer)
        } else {
            managed
        };
        let _managed = managed.install_dispatch_session_runtime();

        let thread = format!("worker-dispatch-repository-{rule}");
        let manifest = awaken_session_contract::SessionResourceManifest::new(
            host.local_workspace(),
            effective_repository(
                "repo-1",
                "https://github.com/awaken/example.git",
                "/workspace/repo",
                Some(source.id.0),
            ),
        );
        let result = host
            .install_dispatched_resources(&thread, &manifest, None)
            .await;
        result.unwrap_or_else(|error| panic!("{rule}: {error}"));
        assert_eq!(
            host.thread_repository_activations(&thread).len(),
            1,
            "{rule}"
        );
        assert!(
            host.thread_repository_activations(&thread)[0]
                .credential_pin
                .is_some(),
            "{rule}: staging retains only the exact pin"
        );
        let realizer = RecordingRepositoryRealizer::default();
        let effect = host.realize_thread_repositories(&thread, &realizer).await;
        if install_credentials {
            effect.unwrap_or_else(|error| panic!("{rule}: {error}"));
            assert_eq!(
                *realizer.0.lock().unwrap(),
                vec!["dispatch-repository-secret".to_owned()],
                "{rule}"
            );
        } else {
            let error = effect
                .expect_err("D2 must reject at the Git edge")
                .to_string();
            assert!(
                error.contains("configured credential materializer"),
                "{rule}: {error}"
            );
            assert!(
                realizer.0.lock().unwrap().is_empty(),
                "{rule}: no Git realizer call after materialization failure"
            );
        }
    }
}

/// Platform-held Repository injection decision table:
///
/// | Rule | dispatch claim | verifier transport | local materializer | Effect |
/// |---|---|---|---|---|
/// | P1 | exact | Gateway mediated | absent | preserve source URL, stage Gateway transport + secret-free pin |
/// | P2 | exact | Direct | any | reject; never fall back to Worker plaintext |
/// | P3 | absent Coordinator staging | Direct | absent | reject; never erase the Platform pin into anonymous Git |
/// | P4 | exact, delayed use | Gateway mediated again | absent | refresh capability at Git operation edge |
/// | P5 | exact, tampered source or target before use | Gateway mediated | absent | reject before verifier/I/O |
/// | P6 | exact, delayed use | changes to Direct | any | reject; zero direct fallback |
#[tokio::test]
async fn platform_repository_credentials_are_gateway_mediated_without_fallback() {
    fn platform_resources() -> awaken_session_contract::ResolvedSessionResources {
        let resources = effective_repository(
            "repo-platform",
            "https://github.com/awaken/example.git",
            "/workspace/repo",
            Some("credential-platform".into()),
        );
        let binding_id = resources.inputs()[0].binding_id.clone();
        let holder = awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Platform,
            "awaken.platform.egress-gateway",
        );
        resources
            .update_input(&binding_id, |input| {
                let awaken_session_contract::ResolvedInputSource::Repository {
                    credential: Some(credential),
                    ..
                } = &mut input.source
                else {
                    unreachable!()
                };
                credential.selected_plaintext_holder = holder.clone();
                credential.access.policy =
                    awaken_runtime_contract::CredentialExecutionPolicy::exact(
                        holder,
                        awaken_runtime_contract::ModelExposurePolicy::Forbidden,
                    );
            })
            .unwrap()
    }

    let claim = awaken_run_ingress::RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("run-platform-repository".into()),
        owner: "worker-platform".into(),
        epoch: 7,
    };
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sequenced = Arc::new(SequencedRepositoryTransport(AtomicUsize::new(0)));
    let _runtime = crate::ManagedHost::new(host.clone())
        .with_repository_binding_verifier(sequenced.clone())
        .install_dispatch_session_runtime();
    let manifest = awaken_session_contract::SessionResourceManifest::new(
        host.local_workspace(),
        platform_resources(),
    );
    host.install_dispatched_resources("platform-mediated", &manifest, Some(&claim))
        .await
        .expect("P1 Gateway mediation needs no Worker materializer");
    let activations = host.thread_repository_activations("platform-mediated");
    let activation = &activations[0];
    assert_eq!(
        activation.plan.source_remote_url, "https://github.com/awaken/example.git",
        "P1 frozen source remains canonical"
    );
    assert_eq!(
        activation.plan.transport_url, "https://gateway.internal/git/repo-platform",
        "P1 Gateway endpoint is effect-only transport"
    );
    assert_eq!(
        activation
            .credential_pin
            .as_ref()
            .map(|pin| pin.access.credential.id.as_str()),
        Some("credential-platform"),
        "P1 retains the secret-free pin, never the staged capability"
    );
    let resources = host.thread_resources_snapshot("platform-mediated");
    let mut wrong_target = activation.clone();
    wrong_target
        .credential_pin
        .as_mut()
        .expect("P5 protected activation")
        .access
        .target = Some(
        awaken_session_contract::repository_transport_credential_target(
            "https://gitlab.example.test/awaken/example.git",
        )
        .expect("P5 alternate HTTPS target"),
    );
    let invalid_realizer = RecordingRepositoryRealizer::default();
    let invalid = host
        .realize_repository_activation(
            "platform-mediated",
            &wrong_target,
            &resources.binding_checks,
            &invalid_realizer,
        )
        .await
        .expect_err("P5 target mismatch rejects before Gateway refresh");
    assert!(invalid.message.contains("another HTTPS origin"), "P5");
    assert_eq!(sequenced.0.load(Ordering::SeqCst), 1, "P5");
    assert!(invalid_realizer.0.lock().unwrap().is_empty(), "P5");

    let mut wrong_source = activation.clone();
    wrong_source
        .credential_pin
        .as_mut()
        .expect("P5 protected activation")
        .access
        .credential
        .id = "another-source".into();
    let invalid_source = host
        .realize_repository_activation(
            "platform-mediated",
            &wrong_source,
            &resources.binding_checks,
            &invalid_realizer,
        )
        .await
        .expect_err("P5 source mismatch rejects against the authored binding");
    assert!(
        invalid_source.message.contains("selects another source"),
        "P5"
    );
    assert_eq!(sequenced.0.load(Ordering::SeqCst), 1, "P5");
    assert!(invalid_realizer.0.lock().unwrap().is_empty(), "P5");

    let realizer = RecordingRepositoryRealizer::default();
    host.realize_thread_repositories("platform-mediated", &realizer)
        .await
        .expect("P4 refreshes at the Git operation edge");
    assert_eq!(sequenced.0.load(Ordering::SeqCst), 2, "P4");
    assert_eq!(
        *realizer.0.lock().unwrap(),
        vec!["repository-capability-2".to_owned()],
        "P4 never reuses the capability staged before package or Sandbox preparation"
    );

    let downgrade_host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let downgrade = Arc::new(GatewayThenDirectRepositoryTransport(AtomicUsize::new(0)));
    let _downgrade_runtime = crate::ManagedHost::new(downgrade_host.clone())
        .with_repository_binding_verifier(downgrade.clone())
        .install_dispatch_session_runtime();
    let downgrade_manifest = awaken_session_contract::SessionResourceManifest::new(
        downgrade_host.local_workspace(),
        platform_resources(),
    );
    downgrade_host
        .install_dispatched_resources("platform-downgrade", &downgrade_manifest, Some(&claim))
        .await
        .expect("P6 initial Gateway route stages");
    let downgrade_realizer = RecordingRepositoryRealizer::default();
    let downgrade_error = downgrade_host
        .realize_thread_repositories("platform-downgrade", &downgrade_realizer)
        .await
        .expect_err("P6 refreshed Direct transport must not fall back");
    assert!(
        downgrade_error
            .message
            .contains("cannot fall back to direct credentials"),
        "P6"
    );
    assert_eq!(downgrade.0.load(Ordering::SeqCst), 2, "P6");
    assert!(downgrade_realizer.0.lock().unwrap().is_empty(), "P6");

    let direct_host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let _direct_runtime = crate::ManagedHost::new(direct_host.clone())
        .with_repository_binding_verifier(Arc::new(FixedRepositoryTransport(
            awaken_resource_contract::RepositoryTransport::Direct,
        )))
        .install_dispatch_session_runtime();
    let direct_manifest = awaken_session_contract::SessionResourceManifest::new(
        direct_host.local_workspace(),
        platform_resources(),
    );
    let denied = direct_host
        .install_dispatched_resources("platform-direct-denied", &direct_manifest, Some(&claim))
        .await
        .expect_err("P2 direct transport cannot receive Platform-held material");
    assert!(denied.message.contains("requires Gateway mediation"), "P2");
    assert!(
        direct_host
            .thread_repository_activations("platform-direct-denied")
            .is_empty(),
        "P2"
    );

    let coordinator_denied = direct_host
        .install_dispatched_resources("platform-coordinator-stage", &direct_manifest, None)
        .await
        .expect_err("P3 Coordinator staging cannot erase a Platform credential pin");
    assert!(
        coordinator_denied
            .message
            .contains("requires Gateway mediation"),
        "P3"
    );
    assert!(
        direct_host
            .thread_repository_activations("platform-coordinator-stage")
            .is_empty(),
        "P3"
    );
}

/// Repository realization cause graph:
/// C1 binding/pin cardinality exact -> C2 source id/usage exact -> C3 holder
/// allowed and model exposure forbidden -> C4 Worker holder exact -> C5 Git
/// effect opens the pinned active revision in the exact Workspace -> E1 one
/// ephemeral operation material reaches the realizer. C1-C4 reject at staging;
/// C5 rejects at each effect edge. Anonymous input bypasses C2-C5; the first
/// failed cause terminates without another credential or holder selection.
///
/// | Rule | Credential | C1 | C2 | C3 | C4 | C5 | Result |
/// |---|---|---|---|---|---|---|---|
/// | H1 | absent | T | - | - | - | - | anonymous |
/// | H2 | present | T | T | T | T | T | secret-free stage, exact effect material |
/// | H3 | present | F | - | - | - | - | reject missing pin |
/// | H4 | present | T | F | - | - | - | reject source mismatch |
/// | H5 | present | T | T | F | - | - | reject usage mismatch |
/// | H6 | absent | F | - | - | - | - | reject extra pin |
/// | H7 | present | T | T | F | - | - | reject unauthorized holder |
/// | H8 | present | T | T | F | - | - | reject virtual exposure |
/// | H9 | present | T | T | T | F | - | reject unsupported holder |
/// | H10 | present | T | T | T | T | F | stage pin, reject stale revision at effect |
/// | H11 | present | T | T | T | T | F | stage pin, reject inactive source at effect |
/// | H12 | present | T | T | T | T | F | stage pin, reject cross-Workspace at effect |
/// | H13 | present | T | T | T | T | scalar | stage pin, reject material kind at effect |
/// | H14 | present | T | T | T | T | active then disabled | first effect only; retry rejects |
#[tokio::test]
async fn repository_credential_realization_follows_the_decision_table() {
    use awaken_session_contract::SessionInit;

    #[derive(Clone, Copy)]
    enum Case {
        Anonymous,
        Exact,
        MissingPin,
        PinWithoutBinding,
        WrongSource,
        WrongUsage,
        HolderNotAllowed,
        VirtualExposure,
        UnsupportedHolder,
        StaleRevision,
        InactiveSource,
        CrossWorkspace,
        WrongMaterial,
    }
    struct Rule {
        id: &'static str,
        case: Case,
        expected_error: Option<&'static str>,
    }
    let rules = [
        Rule {
            id: "H1",
            case: Case::Anonymous,
            expected_error: None,
        },
        Rule {
            id: "H2",
            case: Case::Exact,
            expected_error: None,
        },
        Rule {
            id: "H3",
            case: Case::MissingPin,
            expected_error: Some("has no exact Session pin"),
        },
        Rule {
            id: "H4",
            case: Case::WrongSource,
            expected_error: Some("selects another source"),
        },
        Rule {
            id: "H5",
            case: Case::WrongUsage,
            expected_error: Some("incompatible transport usage"),
        },
        Rule {
            id: "H6",
            case: Case::PinWithoutBinding,
            expected_error: Some("credential pin without a binding"),
        },
        Rule {
            id: "H7",
            case: Case::HolderNotAllowed,
            expected_error: Some("holder is not authorized"),
        },
        Rule {
            id: "H8",
            case: Case::VirtualExposure,
            expected_error: Some("must remain model-invisible"),
        },
        Rule {
            id: "H9",
            case: Case::UnsupportedHolder,
            expected_error: Some("unsupported plaintext holder"),
        },
        Rule {
            id: "H10",
            case: Case::StaleRevision,
            expected_error: Some("credential material revision mismatch"),
        },
        Rule {
            id: "H11",
            case: Case::InactiveSource,
            expected_error: Some("credential material unavailable"),
        },
        Rule {
            id: "H12",
            case: Case::CrossWorkspace,
            expected_error: Some("credential material recipient mismatch"),
        },
        Rule {
            id: "H13",
            case: Case::WrongMaterial,
            expected_error: Some("credential material kind is unsupported"),
        },
    ];

    for rule in rules {
        let host = SharedHost::new(Arc::new(OkModel), "stub");
        let workspace = host.local_workspace().to_owned();
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let mut source = awaken_credential_vault::repo::enter_credential_described(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: if matches!(rule.case, Case::CrossWorkspace) {
                    "another-workspace".into()
                } else {
                    workspace.clone()
                },
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(http_basic_material("git", "repository-decision-secret")),
                oauth_command: None,
            },
            repository_credential_descriptor("https://github.com/awaken/example.git"),
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("author exact Repository credential");
        if matches!(rule.case, Case::WrongMaterial) {
            let material_ref = source
                .material_ref
                .as_ref()
                .expect("described Repository material reference");
            awaken_credential_vault::SecretStore::put(
                secrets.as_ref(),
                material_ref,
                awaken_agent_contract::RedactedString::new("corrupt-scalar-token"),
            )
            .await
            .expect("simulate corrupted Repository material at rest");
        }
        if matches!(rule.case, Case::InactiveSource) {
            source.status = awaken_credential_vault::CredentialStatus::Disabled;
            awaken_credential_vault::repo::CredentialRepo::put(
                credentials.as_ref(),
                source.clone(),
            )
            .await
            .expect("disable exact Repository credential");
        }
        let materializer = awaken_credential_materializer::PinnedCredentialMaterializer::new(
            credentials.clone(),
            secrets,
        );
        let host = Arc::new(host.with_credential_materializer(materializer.clone()));
        let managed =
            managed_with_resource_source(host.clone()).with_credential_materializer(materializer);
        let managed = managed.install_dispatch_session_runtime();
        let binding = (!matches!(rule.case, Case::Anonymous)).then(|| source.id.0.clone());
        let resources = effective_repository(
            "repo-1",
            "https://github.com/awaken/example.git",
            "/workspace/repo",
            binding,
        );
        let binding_id = resources.inputs()[0].binding_id.clone();
        let resources = resources
            .update_input(&binding_id, |input| {
                let awaken_session_contract::ResolvedInputSource::Repository {
                    config,
                    credential,
                    ..
                } = &mut input.source
                else {
                    unreachable!()
                };
                match rule.case {
                    Case::Anonymous | Case::Exact => {}
                    Case::MissingPin => *credential = None,
                    Case::PinWithoutBinding => config.credential_binding = None,
                    Case::WrongSource => {
                        credential.as_mut().unwrap().access.credential.id = "another-source".into();
                    }
                    Case::WrongUsage => {
                        credential.as_mut().unwrap().access.usage =
                            awaken_runtime_contract::CredentialUsage::QueryParameter {
                                name: "token".into(),
                            };
                    }
                    Case::HolderNotAllowed => {
                        let workload = awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Workload,
                            awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
                        );
                        credential.as_mut().unwrap().access.policy =
                            awaken_runtime_contract::CredentialExecutionPolicy::exact(
                                workload,
                                awaken_runtime_contract::ModelExposurePolicy::Forbidden,
                            );
                    }
                    Case::VirtualExposure => {
                        let credential = credential.as_mut().unwrap();
                        credential.access.policy =
                            awaken_runtime_contract::CredentialExecutionPolicy::exact(
                                credential.selected_plaintext_holder.clone(),
                                awaken_runtime_contract::ModelExposurePolicy::VirtualOnly,
                            );
                    }
                    Case::UnsupportedHolder => {
                        let workload = awaken_runtime_contract::PlaintextHolder::new(
                            awaken_runtime_contract::PlaintextBoundary::Workload,
                            awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
                        );
                        let credential = credential.as_mut().unwrap();
                        credential.selected_plaintext_holder = workload.clone();
                        credential.access.policy =
                            awaken_runtime_contract::CredentialExecutionPolicy::exact(
                                workload,
                                awaken_runtime_contract::ModelExposurePolicy::Forbidden,
                            );
                    }
                    Case::StaleRevision => {
                        credential.as_mut().unwrap().access.credential.revision = 2;
                    }
                    Case::InactiveSource | Case::CrossWorkspace | Case::WrongMaterial => {}
                }
            })
            .unwrap();
        let thread = format!("repository-decision-{}", rule.id);
        let result = managed
            .install_test_session_init(
                &thread,
                SessionInit {
                    workspace_id: host.local_workspace().into(),
                    agent_id: "a".into(),
                    delegate_ids: Vec::new(),
                    tools: None,
                    resource_revision: 0,
                    resources,
                    model: None,
                    runtime: None,
                    environment: session_environment(
                        awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                        serde_json::json!({}),
                    ),
                },
            )
            .await;
        let rejects_at_effect = matches!(
            rule.case,
            Case::StaleRevision | Case::InactiveSource | Case::CrossWorkspace | Case::WrongMaterial
        );
        if rule.expected_error.is_none() || rejects_at_effect {
            result.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
            let staged = host.thread_repository_activations(&thread);
            assert_eq!(staged.len(), 1, "{}", rule.id);
            assert_eq!(
                staged[0].credential_pin.is_some(),
                !matches!(rule.case, Case::Anonymous),
                "{}",
                rule.id
            );
            let realizer = RecordingRepositoryRealizer::default();
            let effect = host.realize_thread_repositories(&thread, &realizer).await;
            match rule.expected_error {
                None => {
                    effect.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
                    assert_eq!(realizer.0.lock().unwrap().len(), 1, "{}", rule.id);
                    if matches!(rule.case, Case::Exact) {
                        source.status = awaken_credential_vault::CredentialStatus::Disabled;
                        awaken_credential_vault::repo::CredentialRepo::put(
                            credentials.as_ref(),
                            source.clone(),
                        )
                        .await
                        .expect("H14 disable the exact source between Git effects");
                        let retry = host
                            .realize_thread_repositories(&thread, &realizer)
                            .await
                            .expect_err("H14 second Git effect revalidates liveness");
                        assert!(
                            retry.message.contains("credential material unavailable"),
                            "H14"
                        );
                        assert_eq!(realizer.0.lock().unwrap().len(), 1, "H14");
                    }
                }
                Some(fragment) => {
                    let error = effect.expect_err("effect row must reject").to_string();
                    assert!(error.contains(fragment), "{}: {error}", rule.id);
                    assert!(realizer.0.lock().unwrap().is_empty(), "{}", rule.id);
                }
            }
        } else if let Some(fragment) = rule.expected_error {
            let error = result.expect_err("decision row must reject").to_string();
            assert!(error.contains(fragment), "{}: {error}", rule.id);
        }
    }
}

/// Applying a repository manifest with a new credential reference re-keys the
/// Resource realization only; it still creates no MCP projection.
#[tokio::test]
async fn rotating_a_github_repository_credential_re_keys_only_the_clone() {
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(http_basic_material("x-access-token", "ghp_old")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .unwrap();
    let managed = managed_with_resource_source(host.clone())
        .with_credentials(credentials.clone(), secrets.clone())
        .install_dispatch_session_runtime();
    let initial = effective_repository(
        "repo-1",
        "https://github.com/awaken/example.git",
        "/workspace/repo",
        Some(credential.id.0.clone()),
    );
    managed
        .install_complete_test_session(
            "t-rot",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: initial.clone(),
                model: None,
                runtime: None,
                environment: session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .unwrap();
    managed
        .apply_session_inputs(
            "t-rot",
            &resource_transition(
                host.local_workspace(),
                0,
                Default::default(),
                0,
                initial.clone(),
            ),
        )
        .await
        .expect("compile the exact initial Repository transition");

    let clone_credential_id = |h: &SharedHost| {
        h.thread_repository_activations("t-rot")[0]
            .credential_pin
            .as_ref()
            .map(|pin| pin.access.credential.id.clone())
    };
    assert_eq!(
        clone_credential_id(&host).as_deref(),
        Some(credential.id.0.as_str())
    );

    let next_credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: host.local_workspace().into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: Some("git".into()),
            env_key: None,
            secret: Some(http_basic_material("x-access-token", "ghp_new")),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .unwrap();
    let next = effective_repository(
        "repo-1",
        "https://github.com/awaken/example.git",
        "/workspace/repo",
        Some(next_credential.id.0.clone()),
    );
    // C1 the initial desired generation is staged but no Environment exists;
    // C2 the aggregate rotates only its credential pin. C1+C2 therefore carries
    // the exact initial->next transition without treating staging as completion.
    let before_manifest = awaken_session_contract::SessionResourceManifest::at_revision(
        host.local_workspace(),
        0,
        initial,
    );

    // The Managed adapter stores the supplied credential in the Vault and publishes a
    // new Repository config before invoking this complete-manifest runtime port.
    managed
        .apply_session_inputs(
            "t-rot",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                next,
            ),
        )
        .await
        .unwrap();

    assert_eq!(
        clone_credential_id(&host).as_deref(),
        Some(next_credential.id.0.as_str()),
        "clone credential pin rotated without retaining either secret"
    );
    assert!(host.active_mcp_projections("t-rot").is_empty());
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
fn bare_session(agent: &str, workspace: &str) -> awaken_session_contract::SessionInit {
    awaken_session_contract::SessionInit {
        workspace_id: workspace.into(),
        agent_id: agent.into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    }
}

/// G1 — prompt and mount are derived from the same effective input, including access.
#[tokio::test]
async fn told_equals_mounted_the_prompt_path_and_access_match_the_realized_mount() {
    use awaken_provisioning_contract::MountAccess;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    host.memory_stores
        .fs()
        .create(&store_id, "/seed.md", "seed")
        .await
        .expect("seed memory");

    let resource = TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    };
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![resource]);
    managed
        .install_test_session_init("t-g1", init)
        .await
        .unwrap();

    let realized = "/mnt/memory";
    let prompts = host.thread_session_prompts("t-g1");
    assert!(
        prompts.iter().any(|p| p.contains(realized)),
        "the compiled prompt names the realized path {realized}: {prompts:?}"
    );
    assert!(
        prompts.iter().any(|p| p.contains("read-only")),
        "a read-only binding is described read-only: {prompts:?}"
    );
    let mount = &host.sandbox_spec("t-g1").mounts[0];
    assert_eq!(mount.mount_path, realized);
    assert_eq!(mount.access, MountAccess::ReadOnly);
}

#[tokio::test]
async fn managed_multi_memory_mounts_are_backend_neutral_pairwise() {
    // Cause graph:
    // C1 backend={default/native, acp:claude, acp:codex};
    // C2 store count={1,2,8}; C3 access={all RO, all RW, alternating};
    // C4 instructions={absent,present}. Effects: E1 every store becomes one
    // mount/prompt/binding; E2 access and instructions survive projection;
    // E3 no automatic-memory binding is selected by ordinary Managed resources;
    // E4 every backend receives the same Managed absolute /mnt/memory path, never
    // a host-relative `.mnt` carrier.
    // Constraint: backend selection owns execution only and cannot change
    // resource realization. The nine rows below are a strength-2 covering array:
    // every value pair across C1..C4 occurs at least once. This test verifies
    // that property before executing the rows, rather than trusting the table.
    #[derive(Clone, Copy)]
    struct PairwiseCase {
        rule: &'static str,
        backend: &'static str,
        count: &'static str,
        access: &'static str,
        instructions: &'static str,
    }

    impl PairwiseCase {
        fn values(self) -> [&'static str; 4] {
            [self.backend, self.count, self.access, self.instructions]
        }
    }

    let cases = [
        PairwiseCase {
            rule: "P1",
            backend: "default",
            count: "1",
            access: "ro",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P2",
            backend: "acp:claude",
            count: "1",
            access: "rw",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P3",
            backend: "acp:codex",
            count: "1",
            access: "mixed",
            instructions: "present",
        },
        PairwiseCase {
            rule: "P4",
            backend: "default",
            count: "2",
            access: "rw",
            instructions: "present",
        },
        PairwiseCase {
            rule: "P5",
            backend: "acp:claude",
            count: "2",
            access: "mixed",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P6",
            backend: "acp:codex",
            count: "2",
            access: "ro",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P7",
            backend: "default",
            count: "8",
            access: "mixed",
            instructions: "absent",
        },
        PairwiseCase {
            rule: "P8",
            backend: "acp:claude",
            count: "8",
            access: "ro",
            instructions: "present",
        },
        PairwiseCase {
            rule: "P9",
            backend: "acp:codex",
            count: "8",
            access: "rw",
            instructions: "absent",
        },
    ];

    for left in 0..4 {
        for right in (left + 1)..4 {
            let left_levels = cases
                .iter()
                .map(|case| case.values()[left])
                .collect::<std::collections::BTreeSet<_>>();
            let right_levels = cases
                .iter()
                .map(|case| case.values()[right])
                .collect::<std::collections::BTreeSet<_>>();
            let observed = cases
                .iter()
                .map(|case| (case.values()[left], case.values()[right]))
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                observed.len(),
                left_levels.len() * right_levels.len(),
                "pairwise axes {left}/{right} are incomplete"
            );
        }
    }

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = managed_with_resource_source(host.clone());
    for case in cases {
        let count = case.count.parse::<usize>().unwrap();
        let inputs = (0..count)
            .map(|index| TestInput {
                kind: "memory_store".into(),
                id: format!("{}-store-{index}", case.rule),
                mount_path: format!("/mnt/memory/{}-{index}", case.rule.to_lowercase()),
                access: match case.access {
                    "rw" => ResourceAccess::ReadWrite,
                    "mixed" if index % 2 == 1 => ResourceAccess::ReadWrite,
                    "ro" | "mixed" => ResourceAccess::ReadOnly,
                    other => panic!("unknown access pattern {other}"),
                },
                instructions: (case.instructions == "present")
                    .then(|| format!("{} memory {index}", case.rule)),
                initial_branch: None,
                initial_commit: None,
            })
            .collect::<Vec<_>>();
        let expected_access = inputs.iter().map(|input| input.access).collect::<Vec<_>>();
        let thread = format!("pairwise-{}", case.rule.to_lowercase());
        let mut init = bare_session("agent", host.local_workspace());
        init.runtime = Some(case.backend.into());
        let resources = effective_resources(inputs);
        init.resources = resources.clone();
        managed
            .install_complete_test_session(&thread, init)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.rule));
        managed
            .apply_session_inputs(
                &thread,
                &resource_transition(host.local_workspace(), 0, Default::default(), 0, resources),
            )
            .await
            .unwrap_or_else(|error| panic!("{} compiles the exact transition: {error}", case.rule));

        let spec = host.sandbox_spec(&thread);
        assert_eq!(spec.mounts.len(), count, "{} E1", case.rule);
        for (index, mount) in spec.mounts.iter().enumerate() {
            let managed_path = format!("/mnt/memory/{}-{index}", case.rule.to_lowercase());
            assert_eq!(mount.mount_path, managed_path, "{} E4/{index}", case.rule);
            let expected = match expected_access[index] {
                ResourceAccess::ReadOnly => awaken_provisioning_contract::MountAccess::ReadOnly,
                ResourceAccess::ReadWrite => awaken_provisioning_contract::MountAccess::ReadWrite,
            };
            assert_eq!(mount.access, expected, "{} E2/{index}", case.rule);
        }
        let prompts = host.thread_session_prompts(&thread);
        assert_eq!(prompts.len(), count, "{} E1 prompts", case.rule);
        for (index, prompt) in prompts.iter().enumerate() {
            let managed_path = format!("/mnt/memory/{}-{index}", case.rule.to_lowercase());
            assert!(prompt.contains(&managed_path), "{} E4/{index}", case.rule);
            assert!(!prompt.contains(".mnt/"), "{} E4/{index}", case.rule);
        }
        assert_eq!(
            prompts.iter().all(|prompt| prompt.contains(case.rule)),
            case.instructions == "present",
            "{} E2 instructions",
            case.rule
        );
        let (binding_count, automatic_absent) = host
            .session_slots
            .read(&thread, |slot| {
                (slot.memory_bindings.len(), slot.memory.is_none())
            })
            .unwrap();
        assert_eq!(binding_count, count, "{} E1 bindings", case.rule);
        assert!(automatic_absent, "{} E3", case.rule);
    }
}

#[tokio::test]
async fn automatic_memory_requires_one_explicit_existing_binding() {
    // Automatic-memory cause/effect graph:
    // C1 published Agent selects the Awaken memory plugin; C2 config supplies
    // binding_id; C3 that id exists in the Session's standard mount bindings.
    // E1 leave automatic memory inactive; E2 reject ambiguous/missing config;
    // E3 select exactly the authored binding and never the first array entry.
    // Decision table: A1 !C1=>E1; A2 C1&&!C2=>E2;
    // A3 C1+C2&&!C3=>E2/E1; A4 C1+C2+C3=>E3.

    let cases = [
        ("A1", false, Some("test-input-0"), None, None),
        (
            "A2",
            true,
            None,
            None,
            Some("requires an explicit `memory.binding_id`"),
        ),
        (
            "A3",
            true,
            Some("missing-binding"),
            None,
            Some("is not mounted for this Session"),
        ),
        ("A4", true, Some("test-input-1"), Some(1_usize), None),
    ];

    for (rule, plugin_selected, binding_id, expected_index, expected_error) in cases {
        let plugin_ids = if plugin_selected {
            vec![awaken_ext_memory::MEMORY_PLUGIN_ID.to_string()]
        } else {
            Vec::new()
        };
        let plugin_config = if plugin_selected {
            std::collections::BTreeMap::from([(
                awaken_ext_memory::MEMORY_PLUGIN_ID.to_string(),
                binding_id.map_or_else(
                    || serde_json::json!({}),
                    |binding_id| serde_json::json!({"binding_id": binding_id}),
                ),
            )])
        } else {
            std::collections::BTreeMap::new()
        };
        let snapshot = crate::config::server_config(
            "agent",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &plugin_ids,
            &plugin_config,
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );
        let publications =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
                .expect("valid publication");
        let host = Arc::new(
            SharedHost::new(Arc::new(OkModel), "stub")
                .with_agent_publications(Arc::new(publications)),
        );
        install_test_memory_mounter(&host);
        let stores = [test_memory_store_id(), test_memory_store_id()];
        for store in &stores {
            host.memory_stores
                .fs()
                .create(store, "/seed.md", rule)
                .await
                .unwrap();
        }
        let mut init = bare_session("agent", host.local_workspace());
        init.resources = effective_resources(
            stores
                .iter()
                .enumerate()
                .map(|(index, store)| TestInput {
                    kind: "memory_store".into(),
                    id: store.clone(),
                    mount_path: format!("/mnt/memory/{rule}-{index}"),
                    access: ResourceAccess::ReadWrite,
                    instructions: None,
                    initial_branch: None,
                    initial_commit: None,
                })
                .collect(),
        );
        let managed = managed_with_resource_source(host.clone());
        let thread = format!("automatic-{rule}");
        managed
            .install_complete_test_session(&thread, init)
            .await
            .unwrap();
        assert!(
            host.memory_for_thread(&thread).is_none(),
            "{rule} precondition"
        );

        let result = host.ctx_for(&thread, Some("agent")).await;
        match expected_error {
            Some(fragment) => {
                let error = match result {
                    Ok(_) => panic!("{rule}: invalid automatic binding must fail"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains(fragment), "{rule}: {error}");
                assert!(host.memory_for_thread(&thread).is_none(), "{rule} E1");
            }
            None => {
                result.unwrap_or_else(|error| panic!("{rule}: {error}"));
                let selected = host.memory_for_thread(&thread);
                match expected_index {
                    Some(index) => assert_eq!(
                        selected.as_ref().map(|memory| memory.memory_store_id()),
                        Some(stores[index].as_str()),
                        "{rule} E3"
                    ),
                    None => assert!(selected.is_none(), "{rule} E1"),
                }
            }
        }
    }
}

/// Runtime stages exactly the effective resource list it receives and adds no hidden
/// Agent defaults of its own. Replacement is a Session-control-plane decision.
#[tokio::test]
async fn runtime_stages_exactly_the_effective_resource_list() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let s2 = test_memory_store_id();
    host.memory_stores
        .fs()
        .create(&s2, "/wire.md", "WIRE-BYTES")
        .await
        .unwrap();

    let managed = managed_with_resource_source(host.clone());

    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: s2.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .install_test_session_init("t-g3", init)
        .await
        .unwrap();

    // Exactly one memory mount at that path, and it is the wire store S2.
    let spec = host.sandbox_spec("t-g3");
    let at_path: Vec<_> = spec
        .mounts
        .iter()
        .filter(|mount| mount.mount_path == "/mnt/memory")
        .collect();
    assert_eq!(
        at_path.len(),
        1,
        "one mount wins the path, not both: {:?}",
        spec.mounts
    );
    assert_eq!(
        memory_mount_store_id(at_path[0]),
        s2,
        "the carried effective store is the only staged store"
    );

    assert_eq!(
        memory_mount_store_id(&host.sandbox_spec("t-g3").mounts[0]),
        s2
    );
}

/// G4 — an effective Memory input whose backing store is absent fails closed.
#[tokio::test]
async fn a_bound_resource_with_a_missing_backing_store_fails_the_session_closed() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed =
        crate::ManagedHost::new(host.clone()).with_resource_validator(resource_registry());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: "never-seeded-store".into(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    let result = managed.install_test_session_init("t-g4", init).await;
    assert!(
        result.is_err(),
        "a binding to a missing backing store must fail closed, not mount empty"
    );
}

#[tokio::test]
async fn activation_validates_the_frozen_config_without_selecting_current_again() {
    use awaken_resource_contract::{
        ChangeMemoryStoreState, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
        PublishMemoryStoreConfig, RegisterMemoryStore, ResourceAdministration as _, ResourceState,
    };
    use awaken_session_contract::ResolvedInputSource;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let workspace = host.local_workspace().to_string();
    let store_id = test_memory_store_id();
    let catalog = resource_registry();
    catalog
        .register_memory_store(RegisterMemoryStore {
            definition: MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: workspace.clone(),
                name: "memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            initial_config: MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        })
        .expect("register frozen-config MemoryStore");
    catalog
        .publish_memory_store_config(PublishMemoryStoreConfig {
            workspace_id: workspace.clone(),
            expected_current: ConfigVersion::INITIAL,
            config: MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion(2),
                retention_policy: Default::default(),
            },
        })
        .expect("publish MemoryStore config V2");

    let manifest = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    let managed = crate::ManagedHost::new(host.clone()).with_resource_validator(catalog.clone());
    let mut valid = bare_session("a", &workspace);
    valid.resources = manifest.clone();
    managed
        .install_test_session_init("frozen-v1", valid)
        .await
        .expect("v1 remains valid after current advances to v2");

    let binding_id = manifest.inputs()[0].binding_id.clone();
    let missing = manifest
        .update_input(&binding_id, |input| {
            let ResolvedInputSource::MemoryStore { config, .. } = &mut input.source else {
                panic!("expected MemoryStore input");
            };
            config.version = ConfigVersion(3);
        })
        .unwrap();
    let mut invalid = bare_session("a", &workspace);
    invalid.resources = missing;
    let error = managed
        .install_test_session_init("missing-v3", invalid)
        .await
        .expect_err("an absent frozen config must fail closed");
    assert!(error.message.contains("config version"));
    assert!(host.sandbox_spec("missing-v3").mounts.is_empty());

    catalog
        .change_memory_store_state(ChangeMemoryStoreState {
            workspace_id: workspace.clone(),
            id: store_id.clone().into(),
            state: ResourceState::Archived,
        })
        .expect("archive frozen-config MemoryStore");
    let error = match run_prepared_session(
        &managed,
        "a",
        "frozen-v1",
        vec![ContentBlock::Text {
            text: "must not reach the model".into(),
        }],
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("live lifecycle state must deny a later operation"),
    };
    assert!(error.message.contains("not active"));
}

#[tokio::test]
async fn replacing_a_manifest_removes_the_old_delivered_skill_tree_immediately() {
    use awaken_skill_store::{SkillBundleFile, SkillDefinition, SkillVersion, bundle_sha256};

    let storage = tempfile::tempdir().expect("storage");
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("a")
        .model(test_model_binding())
        .tools(crate::config::advertised_tools(
            &HashSet::new(),
            &HashSet::new(),
            &[],
        ))
        .agent_bindings(awaken_runtime_contract::agent_bindings::AgentBindings {
            skills: vec![awaken_agent_contract::AgentSkillBinding::custom("governed")],
            toolsets: vec![awaken_agent_contract::ToolsetPolicy {
                source: awaken_agent_contract::ToolsetSource::Agent,
                default: awaken_agent_contract::ToolExecutionPolicy::default(),
                overrides: Vec::new(),
            }],
            ..Default::default()
        })
        .build();
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("one immutable governed Agent");
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path())
            .with_agent_publications(Arc::new(publications)),
    );
    let workspace = host.local_workspace().to_string();
    let files = vec![
        SkillBundleFile {
            path: "SKILL.md".into(),
            content: b"---\nname: governed\ndescription: governed\n---\nuse it".to_vec(),
            executable: false,
        },
        SkillBundleFile {
            path: "scripts/old.sh".into(),
            content: b"exit 0".to_vec(),
            executable: false,
        },
    ];
    let hash = bundle_sha256(&files);
    host.skills
        .create(
            SkillDefinition {
                id: "governed".into(),
                workspace_id: workspace.clone(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            SkillVersion {
                id: "skver_governed_1".into(),
                skill_id: "governed".into(),
                version: 1,
                name: "governed".into(),
                description: "governed".into(),
                directory: "/skills/governed".into(),
                bundle_sha256: hash.clone(),
                files,
                created_unix_nanos: 0,
            },
        )
        .await
        .expect("durable SkillStore")
        .expect("create Skill");
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("a", &workspace);
    let resources = init
        .resources
        .with_skills(vec![awaken_session_contract::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "governed".into(),
            version: 1,
            bundle_sha256: hash,
        }])
        .unwrap();
    init.resources = resources.clone();
    install_complete_test_session_for_realization(&managed, "skill-revoke", init)
        .await
        .expect("prepare pinned Skill");
    managed
        .apply_session_inputs(
            "skill-revoke",
            &resource_transition(&workspace, 0, Default::default(), 0, resources),
        )
        .await
        .expect("compile pinned Skill bytes under the exact transition");
    run_prepared_session(
        &managed,
        "a",
        "skill-revoke",
        vec![ContentBlock::Text {
            text: "open the environment".into(),
        }],
    )
    .await
    .expect("materialize Skill");
    let environment = host
        .session_environment("skill-revoke")
        .await
        .expect("resident Skill environment");
    assert!(
        environment
            .scan_skill_dir(crate::skills::DELIVERED_SKILLS_SUBDIR)
            .expect("scan delivered Skill projection")
            .iter()
            .any(|skill| skill.id == "governed"),
        "pinned Skill tree is readable through the Environment"
    );
    let before_manifest = host
        .thread_resource_manifest("skill-revoke")
        .expect("installed manifest");

    managed
        .apply_session_inputs(
            "skill-revoke",
            &resource_transition(
                &before_manifest.workspace_id,
                before_manifest.revision,
                before_manifest.resources.clone(),
                before_manifest.revision + 1,
                awaken_session_contract::ResolvedSessionResources::default(),
            ),
        )
        .await
        .expect("replace with explicit empty Skill selection");
    assert!(
        environment
            .scan_skill_dir(crate::skills::DELIVERED_SKILLS_SUBDIR)
            .expect("scan after Skill removal")
            .is_empty(),
        "the old delivered tree is gone before another Run can read it"
    );
}

/// G5 — the effective resource reference crosses the node boundary, so a worker needs
/// access to the resource data plane but not to the Agent authoring repository.
#[tokio::test]
async fn an_effective_resource_mounts_on_a_worker_without_the_binding_repository() {
    let db_less = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    db_less
        .memory_stores
        .fs()
        .create(&store_id, "/carried.md", "CARRIED-BYTES")
        .await
        .expect("seed");
    let managed_worker = managed_with_resource_source(db_less.clone());
    let mut init = bare_session("a", db_less.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: store_id.clone(),
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed_worker
        .install_test_session_init("t-g5-worker", init)
        .await
        .unwrap();
    assert_eq!(
        memory_mount_store_id(&db_less.sandbox_spec("t-g5-worker").mounts[0]),
        store_id,
        "the effective input is sufficient for a DB-less worker"
    );
    assert_eq!(
        db_less.sandbox_spec("t-g5-worker").mounts[0].access,
        awaken_provisioning_contract::MountAccess::ReadOnly,
        "the carried access is sufficient; no Host-side write-back registry exists"
    );
}

#[tokio::test]
async fn ctx_for_carries_one_self_consistent_snapshot_on_any_claiming_node() {
    use awaken_runtime_contract::resolver::RunResolver;

    // A durable run is driven by whichever pool node claims it (ADR-0019). Its
    // session config is already the complete execution authority and resolves
    // without manufacturing a second node-local catalog object.
    let (host, _managed) = dispatch_test_host(SharedHost::new(Arc::new(OkModel), "stub"));
    let ctx = host
        .ctx_for("t-catalog", None)
        .await
        .expect("session builds");
    let resolved = ctx.runtime.resolve(&ctx.config).expect("snapshot resolves");
    assert_eq!(resolved.snapshot_id, ctx.config.id);
}

#[tokio::test]
async fn claimed_snapshot_is_the_worker_session_authority() {
    let published = awaken_runtime_contract::ExecutableAgentSnapshot::builder("published-agent")
        .model(test_model_binding())
        .instructions("published instructions")
        .fingerprint("sha256:published")
        .plugin_config([(
            "permission".to_string(),
            serde_json::json!({"default": "deny", "rules": []}),
        )])
        .build();
    let (host, managed) = dispatch_test_host(SharedHost::new(Arc::new(OkModel), "host-default"));
    install_test_session_application(&host);
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        "t-published-claim",
        host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &published,
    )
    .await;

    let ctx = host
        .ctx_for_snapshot(
            "t-published-claim",
            Some("published-agent"),
            Some(published.clone()),
        )
        .await
        .expect("worker session builds from claimed snapshot");

    let mut expected = published;
    expected
        .recompute_fingerprint()
        .expect("recompute the complete Session overlay identity");
    assert_eq!(ctx.config, expected);
}

#[tokio::test]
async fn background_task_fails_closed_on_a_database_less_worker() {
    // Cause/effect decision table:
    // R1 Native + co-located owner + selected BackgroundTask -> construct the
    // ordinary Runtime context; R2 Native + database-less Worker + selected
    // BackgroundTask -> reject before tool advertisement or Environment side
    // effects. ACP is the existing independent fail-closed partition. Until a
    // claim-fenced completion-attention transport exists, accepting R2 could
    // strand the process-local completion on a different Worker.
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("background-agent")
        .model(test_model_binding())
        .plugins([awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string()])
        .plugin_config([(
            awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.to_string(),
            serde_json::json!({"tools": []}),
        )])
        .build();

    let (local, _local_managed) = dispatch_test_host(SharedHost::new(Arc::new(OkModel), "stub"));
    local.register_thread_agent_projection("background-local", "background-agent");
    local
        .ctx_for_snapshot(
            "background-local",
            Some("background-agent"),
            Some(snapshot.clone()),
        )
        .await
        .expect("R1 co-located Native context");

    let worker = SharedHost::new(Arc::new(OkModel), "stub").with_worker_upstream(
        awaken_worker_transport_security::WorkerUpstream::new("http://coordinator.invalid"),
    );
    let error = match worker
        .ctx_for_snapshot(
            "background-worker",
            Some("background-agent"),
            Some(snapshot),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("R2 database-less Worker must reject BackgroundTask"),
    };
    assert_eq!(error.kind, HostErrorKind::BadRequest, "R2 classification");
    assert!(
        error.message.contains("co-located Session application"),
        "R2 actionable boundary: {error}"
    );
    assert!(
        worker
            .session_environment("background-worker")
            .await
            .is_none(),
        "R2 no Environment side effect"
    );
}

#[tokio::test]
async fn outbound_a2a_never_materializes_or_owns_a_local_environment() {
    // Cause/effect graph: C1=remote A2A backend; C2=local Environment input.
    // Effects: E1=A2A-only IO context has no Environment and no Hand;
    // E2=A2A plus a local mount is rejected before provisioning; E3=adding a
    // Native fallback makes the candidate set local-capable and retains an
    // Environment. Constraint: inbound A2A is only a protocol adapter and does
    // not alter this backend rule. Decision table: R1 C1&&!C2 -> no Environment;
    // R2 C1&&C2 -> BadRequest; R3 C1+Native fallback -> one Environment/Hand.
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("remote-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "remote-agent",
            "",
            "a2a:https://agent.example.test",
        ))
        .build();
    let (host, managed) = dispatch_test_host(SharedHost::new(Arc::new(OkModel), "host-default"));
    install_test_session_application(&host);
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        "a2a-io-only",
        host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &snapshot,
    )
    .await;
    let ctx = host
        .ctx_for_snapshot("a2a-io-only", Some("remote-agent"), Some(snapshot.clone()))
        .await
        .expect("R1 A2A context");
    assert!(ctx.env.is_none(), "R1 Environment");
    assert!(ctx.attempt_context.tool_executor.is_none(), "R1 Hand");
    assert!(
        host.session_environment("a2a-io-only").await.is_none(),
        "R1 owner"
    );

    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        "a2a-with-mount",
        host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &snapshot,
    )
    .await;
    host.register_thread_resources(
        "a2a-with-mount",
        crate::provisioning::StagedResources {
            mounts: vec![awaken_provisioning_contract::MountRequirement {
                mount_id: "input".into(),
                source: awaken_provisioning_contract::MountSource::File {
                    file_id: "file".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/input".into(),
                access: awaken_provisioning_contract::MountAccess::ReadOnly,
                lifetime: awaken_provisioning_contract::MountLifetime::Session,
                required: true,
            }],
            ..Default::default()
        },
    );
    let error = match host
        .ctx_for_snapshot("a2a-with-mount", Some("remote-agent"), Some(snapshot))
        .await
    {
        Ok(_) => panic!("R2 local input was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.kind, crate::host::HostErrorKind::BadRequest, "R2");
    assert!(
        host.session_environment("a2a-with-mount").await.is_none(),
        "R2"
    );

    let mixed = awaken_runtime_contract::ExecutableAgentSnapshot::builder("mixed-agent")
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            "remote-agent",
            "",
            "a2a:https://agent.example.test",
        ))
        .model_candidates([awaken_runtime_contract::resolved::ModelBinding::new(
            "native-fallback",
            "native-model",
            "native",
        )])
        .build();
    crate::host::worker_resolver::test_support::install_complete_projection_for_snapshot(
        &managed,
        "a2a-native-fallback",
        host.local_workspace(),
        session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
        &mixed,
    )
    .await;
    let ctx = host
        .ctx_for_snapshot("a2a-native-fallback", Some("mixed-agent"), Some(mixed))
        .await
        .expect("R3 mixed candidate context");
    assert!(ctx.env.is_some(), "R3 Environment");
    assert!(ctx.attempt_context.tool_executor.is_some(), "R3 Hand");
}

/// Session Event recovery selects durable trace provenance before constructing
/// the existing dispatch envelope; it never consults the recovery span.
#[test]
fn session_event_dispatch_uses_the_explicit_persisted_trace_source() {
    // Cause/effect graph: C1 dispatch decoration receives the ordinary ambient
    // trace, a persisted Event trace, or an explicitly absent persisted trace;
    // C2 the execution payload is otherwise identical. Effects: E1 each present
    // source is copied exactly; E2 persisted None remains None and never falls
    // back to the supervisor's ambient trace; E3 trace choice does not change
    // canonical dispatch/replay identity.
    //
    // | Rule | Explicit source | Simulated ambient | Effect |
    // | T1 | ambient parent | same parent | E1 |
    // | T2 | persisted parent | different ambient | E1+E3 |
    // | T3 | persisted None | present ambient | E2+E3 |
    // Constraint/invariant: ordinary admission is the only caller that captures
    // ambient context. Session Event reservation always calls this explicit seam,
    // so recovery cannot manufacture a parent when root provenance stores None.
    const AMBIENT: &str = "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01";
    const PERSISTED: &str = "00-11111111111111111111111111111111-2222222222222222-01";
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let activation = || {
        awaken_runtime_contract::RunActivation::new(
            awaken_agent_contract::agent::run::Id("run-explicit-trace-source".into()),
            awaken_agent_contract::agent::thread::Id("thread-explicit-trace-source".into()),
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .model(test_model_binding())
                .fingerprint("sha256:explicit-trace-source")
                .build(),
            Vec::new(),
        )
    };

    let ordinary = host
        .resolved_dispatch_with_traceparent(activation(), Some(AMBIENT.into()))
        .expect("T1 ordinary ambient source");
    let recovered_present = host
        .resolved_dispatch_with_traceparent(activation(), Some(PERSISTED.into()))
        .expect("T2 persisted source");
    let recovered_absent = host
        .resolved_dispatch_with_traceparent(activation(), None)
        .expect("T3 persisted absence");

    assert_eq!(ordinary.traceparent.as_deref(), Some(AMBIENT), "T1/E1");
    assert_eq!(
        recovered_present.traceparent.as_deref(),
        Some(PERSISTED),
        "T2/E1"
    );
    assert_eq!(recovered_absent.traceparent, None, "T3/E2");
    assert!(
        ordinary.same_canonical_dispatch(&recovered_present)
            && ordinary.same_canonical_dispatch(&recovered_absent),
        "T2+T3/E3"
    );
}

/// Dispatch Resource selection stays on the one Session-slot authority for both
/// ordinary and Session-root durable requests.
#[test]
fn durable_dispatch_carries_the_frozen_session_resource_manifest_and_scope() {
    // Cause/effect graph: C1 the slot is/is-not a prepared Session; C2 the
    // canonical root/ordinary decorator is selected; C3 an aggregate-staged
    // Resource transition exists; C4 an active manifest exists; C5 an
    // Environment runtime projection exists independently of Session admission.
    // Effects: E1 a non-Session projection carries active even with C5; E2
    // every prepared Session projection carries desired, independently of root
    // affinity; E3 desired works before active exists and wins over stale
    // active; E4 a legacy Session without a transition falls back to active;
    // E5 dispatch decoration never publishes or replaces active; E6 runtime
    // projection presence and exact root Session affinity follow C5 and C1+C2,
    // respectively. Scope and Worker capability derive from the exact selected
    // manifest.
    //
    // | Rule | C1 | Root | C3   | C4   | C5 | Carried | Runtime | Affinity | Active after |
    // | R1   | F  | T    | none | A    | T  | A       | some    | none     | A            |
    // | R2   | T  | F    | A->B | A    | F  | B       | none    | none     | A            |
    // | R3   | T  | T    | 0->B | none | F  | B       | none    | thread   | none         |
    // | R4   | T  | T    | A->B | A    | F  | B       | none    | thread   | A            |
    // | R5   | T  | T    | none | A    | F  | A       | none    | thread   | A            |
    // | R6   | T  | T    | A->B | A    | T  | B       | some    | thread   | A            |
    // Constraint: C1, C3, C4, runtime projection, and the selected manifest are
    // read under one Session-slot lock, so no dispatch can mix generations.
    for (
        rule,
        prepared_session,
        root_decorator,
        runtime_projection,
        active_revision,
        desired_revision,
        expected_revision,
    ) in [
        ("R1", false, true, true, Some(4), None, 4),
        ("R2", true, false, false, Some(4), Some(7), 7),
        ("R3", true, true, false, None, Some(7), 7),
        ("R4", true, true, false, Some(4), Some(7), 7),
        ("R5", true, true, false, Some(4), None, 4),
        ("R6", true, true, true, Some(4), Some(7), 7),
    ] {
        let host = SharedHost::new(Arc::new(OkModel), "host-default");
        let thread = format!("t-dispatch-resources-{rule}");
        let resources = |generation: &str| {
            effective_resources(vec![TestInput {
                kind: "file".into(),
                id: format!("file-{generation}-{rule}"),
                mount_path: format!("/workspace/{generation}-{rule}"),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: Some(format!("{generation} instructions for {rule}")),
                initial_branch: None,
                initial_commit: None,
            }])
        };
        let active = active_revision.map(|revision| {
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                resources("active"),
            )
        });
        let desired = desired_revision.map(|revision| {
            awaken_session_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                resources("desired"),
            )
        });
        let transition = desired.clone().map(|desired| {
            awaken_session_contract::SessionResourceTransition::new(
                active.clone().unwrap_or_else(|| {
                    awaken_session_contract::SessionResourceManifest::at_revision(
                        "workspace-a",
                        0,
                        resources("previous"),
                    )
                }),
                desired,
            )
            .expect("test transition belongs to one Workspace")
        });
        let expected = desired
            .or_else(|| active.clone())
            .expect("selected manifest");
        host.session_slots.update(&thread, |slot| {
            slot.session_dispatch = prepared_session;
            slot.manifest = active.clone();
            slot.resource_transition = transition;
            slot.environment_snapshot = runtime_projection.then(|| {
                session_environment(
                    awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                    serde_json::json!({}),
                )
            });
        });
        let activation = awaken_runtime_contract::RunActivation::new(
            awaken_agent_contract::agent::run::Id(format!("run-dispatch-resources-{rule}")),
            awaken_agent_contract::agent::thread::Id(thread.clone()),
            awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .model(test_model_binding())
                .fingerprint(format!("sha256:dispatch-resources-{rule}"))
                .build(),
            Vec::new(),
        );

        let dispatch = if root_decorator {
            host.resolved_dispatch(activation)
        } else {
            host.resolved_thread_extension_dispatch(activation)
        }
        .unwrap_or_else(|error| panic!("{rule} decorate durable dispatch: {error}"));
        let carried = dispatch
            .session_resources
            .as_ref()
            .unwrap_or_else(|| panic!("{rule} resource envelope"))
            .decode_manifest()
            .unwrap_or_else(|error| panic!("{rule} decode resource envelope: {error}"));

        assert_eq!(carried.revision, expected_revision, "{rule} selection");
        assert_eq!(carried, expected, "{rule} exact generation and payload");
        assert_eq!(
            dispatch.execution_scope,
            Some(awaken_tenancy::ExecutionScopeRef(
                awaken_tenancy::ScopeId::from("workspace-a")
            )),
            "{rule} scope",
        );
        assert!(
            dispatch
                .placement
                .required_capabilities
                .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY),
            "{rule} selected manifest requires an eligible Worker",
        );
        assert_eq!(
            dispatch.session_runtime.is_some(),
            runtime_projection,
            "{rule}/E6 runtime projection presence",
        );
        assert_eq!(
            dispatch.session_thread_id,
            (prepared_session && root_decorator).then(|| ThreadId(thread.clone())),
            "{rule}/E6 exact Session affinity",
        );
        assert_eq!(
            host.thread_resource_manifest(&thread),
            active,
            "{rule}/E5 dispatch staging must not publish active",
        );
    }
}

/// Coordinator-to-Worker publication cause/effect/FMECA design. C1 a parent
/// activation references a published child; C2 the Coordinator owns the sole
/// executable catalog. E1 `resolved_dispatch` embeds the exact child snapshot;
/// E2 no mutable catalog handle crosses the queue. Rule A1=C1+C2=>E1+E2.
/// FMECA: omitting the snapshot strands every remote delegation (high severity,
/// repeated claim failure); the assertion detects the omission at admission and
/// the cold-Worker decision-table test verifies the receiving half.
#[test]
fn durable_dispatch_freezes_the_delegation_publication_closure() {
    use awaken_runtime_contract::agent_bindings::{AgentBindings, AgentDelegateBinding};
    use awaken_runtime_contract::snapshot::AgentId;

    let child = awaken_runtime_contract::ExecutableAgentSnapshot::builder("researcher")
        .model(test_model_binding())
        .fingerprint("researcher-current")
        .build();
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([child.clone()])
            .expect("A1 publication source");
    let host = SharedHost::new(Arc::new(OkModel), "host-default")
        .with_agent_publications(Arc::new(publications));
    let parent = awaken_runtime_contract::ExecutableAgentSnapshot::builder("coordinator")
        .model(test_model_binding())
        .agent_bindings(AgentBindings {
            delegates: vec![AgentDelegateBinding {
                agent_id: AgentId("researcher".into()),
                source_revision: None,
                recursive_self: false,
            }],
            ..Default::default()
        })
        .build();
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-publication-closure".into()),
        awaken_agent_contract::agent::thread::Id("thread-publication-closure".into()),
        parent,
        Vec::new(),
    );

    let dispatch = host.resolved_dispatch(activation).expect("A1 admission");
    assert_eq!(dispatch.agent_publications, vec![child], "A1/E1+E2");
}

#[test]
fn durable_dispatch_marks_only_a_prepared_root_session_for_worker_realization() {
    // Cause/effect graph: C1 the request entered the Session application port;
    // C2 only a Resource manifest exists; C3 a child Run is parent-mediated.
    // Effects: E1 the root dispatch names its already-frozen Session; E2 an ordinary
    // resource-bearing Run remains ordinary; E3 a child retains the parent
    // Session pointer. C1 and C2 are mutually exclusive test fixtures here; C3 is owned by
    // `child_dispatch_reuses_publication_pinned_model_candidates`.
    //
    // | Rule | Session API | Resources only | Child | session_thread_id |
    // | R1   | yes            | any            | no    | root thread       |
    // | R2   | no             | yes            | no    | none              |
    // | R3   | n/a            | any            | yes   | parent thread     |
    //
    // This test owns R1. The adjacent resource-envelope test owns R2 and the
    // existing child-dispatch test owns R3, avoiding a parallel child builder.
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let thread = "prepared-root-session";
    host.session_slots
        .update(thread, |slot| slot.session_dispatch = true);
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-prepared-root-session".into()),
        awaken_agent_contract::agent::thread::Id(thread.into()),
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(test_model_binding())
            .fingerprint("sha256:prepared-root-session")
            .build(),
        Vec::new(),
    );

    let dispatch = host
        .resolved_dispatch(activation)
        .expect("decorate prepared Session dispatch");

    assert_eq!(
        dispatch.session_thread_id,
        Some(awaken_agent_contract::agent::thread::Id(thread.into())),
        "R1/E1"
    );
}

#[test]
fn thread_extension_dispatch_reuses_frozen_inputs_without_session_admission() {
    // Cause/effect graph: C1 a prepared Session marks its public root Runs for
    // Session admission; C2 the Outcome bounded context owns an ordinary Run on
    // that same Thread. Effects: E1 C1 uses root Session affinity; E2 C1+C2
    // preserves the ordinary full-dispatch shape while reusing the canonical
    // decorator. Decision table: public root => E1 (adjacent test); Outcome
    // extension => E2. A structural self-affinity must never be inferred for C2.
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let thread = "prepared-outcome-thread";
    host.session_slots
        .update(thread, |slot| slot.session_dispatch = true);
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("outcome-worker-run".into()),
        awaken_agent_contract::agent::thread::Id(thread.into()),
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
            .model(test_model_binding())
            .fingerprint("sha256:outcome-worker")
            .build(),
        Vec::new(),
    );

    let dispatch = host
        .resolved_thread_extension_dispatch(activation)
        .expect("decorate Outcome-owned ordinary Run");

    assert_eq!(dispatch.session_thread_id, None, "C2/E2");
    assert_eq!(
        dispatch.admission_shape(),
        awaken_run_ingress_contract::DispatchAdmissionShape::OrdinaryRoot,
        "C2/E2 remains eligible for generic full-dispatch admission",
    );
}

#[test]
fn cold_host_inference_holder_follows_the_candidate_backend_decision_table() {
    // Cause graph: C1=credential-bearing candidate; C2=Native; C3=ACP;
    // C4=mixed boundaries; C5=publication freezes one common holder;
    // C6=prepared Environment requests a different holder; C7=closed Remote
    // coordinate (exact A2A backend with no local model reference); C8=the
    // candidate policies have no common holder. The immutable publication
    // decision feeds root, child, direct, and dispatch paths. An exact
    // publication holder dominates the Environment fallback; a boundary-only
    // decision must agree with that Environment.
    // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | result                   |
    // | R1   | F  | -  | -  | F  | F  | -  | F  | F  | no holder                |
    // | R2   | T  | T  | F  | F  | F  | F  | F  | F  | Worker holder            |
    // | R3   | T  | F  | T  | F  | F  | F  | F  | F  | Workload holder          |
    // | R4   | T  | T  | T  | T  | F  | -  | F  | F  | reject                   |
    // | R5   | T  | -  | -  | -  | T  | F  | F  | F  | exact publication holder |
    // | R6   | T  | -  | -  | -  | T  | T  | F  | F  | exact publication holder |
    // | R7   | T  | T  | F  | F  | F  | T  | F  | F  | reject                   |
    // | R8   | T  | F  | F  | F  | F  | F  | T  | F  | Worker holder            |
    // | R9   | T  | T  | F  | F  | F  | F  | F  | T  | reject                   |
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let candidate_with_policy =
        |model: &str, backend: &str, policy: awaken_runtime_contract::CredentialExecutionPolicy| {
            let binding =
                awaken_runtime_contract::resolved::ModelBinding::new("provider", model, backend);
            let credential = Some(
                awaken_runtime_contract::CredentialAccess::new(
                    awaken_runtime_contract::CredentialRef {
                        id: format!("credential-{model}"),
                        revision: 1,
                    },
                    awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                    awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                    policy,
                )
                .with_target(awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                    "provider",
                )),
            );
            let endpoint = awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "test".into(),
                api_dialect: "test".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: model.into(),
                processing_placement: None,
            };
            if backend.starts_with("acp:") {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider_with_acp(
                    binding,
                    "provider@1",
                    "route@1",
                    "workspace-a",
                    credential,
                    endpoint,
                    awaken_runtime_contract::resolved::AcpExecutionProfile {
                        capability_fingerprint: "sha256:test-capability".into(),
                        capability_adapter_version: "test".into(),
                        session_configuration: Default::default(),
                    },
                )
            } else {
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
                    binding,
                    "provider@1",
                    "route@1",
                    "workspace-a",
                    credential,
                    endpoint,
                )
            }
            .expect("coherent holder-policy candidate")
        };
    let candidate = |model: &str, backend: &str| {
        candidate_with_policy(
            model,
            backend,
            awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
        )
    };
    let activation = |primary, fallbacks| {
        let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent")
            .resolved_model(primary)
            .build();
        snapshot.resolved_spec.model_candidates = fallbacks;
        awaken_runtime_contract::RunActivation::new(
            awaken_agent_contract::agent::run::Id("run".into()),
            awaken_agent_contract::agent::thread::Id("cold-thread".into()),
            snapshot,
            Vec::new(),
        )
    };

    let anonymous = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::resolved::ModelBinding::new("host", "anonymous", "native"),
    );
    assert_eq!(
        host.inference_plaintext_holder(&activation(anonymous, Vec::new()))
            .unwrap(),
        None,
        "R1"
    );
    let native_activation = activation(candidate("native", "native"), Vec::new());
    assert_eq!(
        super::self_hosted_inference_holder(&native_activation)
            .unwrap()
            .unwrap()
            .boundary,
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "R2"
    );
    assert_eq!(
        host.inference_plaintext_holder(&native_activation).unwrap(),
        super::self_hosted_inference_holder(&native_activation).unwrap(),
        "the public cold-start decision and Host fallback must remain identical"
    );
    assert_eq!(
        host.inference_plaintext_holder(&activation(candidate("acp", "acp:codex"), Vec::new()))
            .unwrap()
            .unwrap()
            .boundary,
        awaken_runtime_contract::PlaintextBoundary::Workload,
        "R3"
    );
    let remote = awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
        awaken_runtime_contract::resolved::ModelBinding::new(
            "remote",
            "",
            "a2a:https://agent.example",
        ),
        awaken_tenancy::ScopeId::from("workspace-a"),
        Some(awaken_runtime_contract::CredentialAccess::new(
            awaken_runtime_contract::CredentialRef {
                id: "credential-remote".into(),
                revision: 1,
            },
            awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_runtime_contract::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
        )),
        "sha256:card",
    )
    .expect("coherent remote candidate");
    assert_eq!(
        super::self_hosted_inference_holder(&activation(remote, Vec::new()))
            .unwrap()
            .unwrap()
            .boundary,
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "R8"
    );
    assert!(
        host.inference_plaintext_holder(&activation(
            candidate("native", "native"),
            vec![candidate("acp", "acp:codex")],
        ))
        .is_err(),
        "R4"
    );
    let platform_holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Platform,
        "awaken.cloud.egress-gateway",
    );
    let platform_candidate = candidate_with_policy(
        "platform",
        "hosted",
        awaken_runtime_contract::CredentialExecutionPolicy::exact(
            platform_holder.clone(),
            awaken_runtime_contract::ModelExposurePolicy::Forbidden,
        ),
    );
    let platform_activation = activation(platform_candidate, Vec::new());
    assert_eq!(
        super::self_hosted_inference_holder(&platform_activation).unwrap(),
        Some(platform_holder.clone()),
        "R5"
    );
    host.install_environment_projection(
        &platform_activation.thread_id.0,
        &session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    )
    .expect("install a prepared self-hosted Environment profile");
    assert_eq!(
        host.inference_plaintext_holder(&platform_activation)
            .unwrap(),
        Some(platform_holder),
        "R6"
    );
    let mut conflicting_environment = session_environment(
        awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        serde_json::json!({}),
    );
    conflicting_environment.credential_realization =
        awaken_runtime_contract::CredentialRealizationProfile::self_hosted_acp();
    let conflicting_host = SharedHost::new(Arc::new(OkModel), "host-default");
    conflicting_host
        .install_environment_projection("cold-thread", &conflicting_environment)
        .expect("replace the prepared Environment profile");
    assert!(
        conflicting_host
            .inference_plaintext_holder(&native_activation)
            .is_err(),
        "R7"
    );

    let worker_candidate = candidate_with_policy(
        "worker-exact",
        "native",
        awaken_runtime_contract::CredentialExecutionPolicy::exact(
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
                .inference_holder,
            awaken_runtime_contract::ModelExposurePolicy::Forbidden,
        ),
    );
    let workload_candidate = candidate_with_policy(
        "workload-exact",
        "native",
        awaken_runtime_contract::CredentialExecutionPolicy::exact(
            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_acp()
                .inference_holder,
            awaken_runtime_contract::ModelExposurePolicy::Forbidden,
        ),
    );
    let error = super::self_hosted_inference_holder(&activation(
        worker_candidate,
        vec![workload_candidate],
    ))
    .expect_err("R8 disjoint exact holder policies must fail closed");
    assert!(
        error
            .to_string()
            .contains("no common credential plaintext holder"),
        "R9 exact rejection reason: {error}"
    );
}

#[tokio::test]
async fn replacement_host_adopts_the_dispatch_sandbox_from_a_stable_root() {
    let storage = tempfile::tempdir().expect("storage dir");
    let thread = "t-sandbox-recovery";

    let (first, first_managed) = dispatch_test_host(
        SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path()),
    );
    install_test_session_application(&first);
    install_complete_test_session_for_realization(
        &first_managed,
        thread,
        bare_session("assistant", first.local_workspace()),
    )
    .await
    .expect("install first complete Session projection");
    let first_ctx = first.ctx_for(thread, None).await.expect("first session");
    let handle = first_ctx.env.as_ref().expect("eager environment").handle();
    let marker = storage
        .path()
        .join("sandboxes")
        .join(thread)
        .join("recovery-marker");
    std::fs::write(&marker, b"survived").expect("write sandbox marker");
    drop(first_ctx);
    drop(first_managed);
    drop(first);

    let (replacement, replacement_managed) = dispatch_test_host(
        SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path()),
    );
    install_test_session_application(&replacement);
    replacement_managed
        .install_complete_test_session(
            thread,
            bare_session("assistant", replacement.local_workspace()),
        )
        .await
        .expect("install replacement complete Session projection");
    let binding = serde_json::to_string(&handle).expect("encode durable handle");
    assert_eq!(
        replacement
            .adopt_bound_session_environment(
                thread,
                Some(&binding),
                &replacement.session_provider,
                None,
                false,
            )
            .await
            .expect("adopt durable handle"),
        super::session::SessionEnvironmentAdoptionDisposition::Ready,
    );
    let replacement_ctx = replacement
        .ctx_for(thread, None)
        .await
        .expect("replacement session");

    assert_eq!(
        replacement_ctx
            .env
            .as_ref()
            .expect("eager environment")
            .handle(),
        handle
    );
    assert_eq!(std::fs::read(marker).unwrap(), b"survived");
}

#[tokio::test]
async fn resident_session_accepts_only_an_adoption_of_its_exact_sandbox() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (host, managed) = dispatch_test_host(
        SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path()),
    );
    install_test_session_application(&host);
    install_complete_test_session_for_realization(
        &managed,
        "t-resident-adoption",
        bare_session("assistant", host.local_workspace()),
    )
    .await
    .expect("install resident complete Session projection");
    let resident = host
        .ctx_for("t-resident-adoption", None)
        .await
        .expect("resident session");
    let resident_handle = resident.env.as_ref().expect("eager environment").handle();

    let resident_binding = serde_json::to_string(&resident_handle).unwrap();
    assert_eq!(
        host.adopt_bound_session_environment(
            "t-resident-adoption",
            Some(&resident_binding),
            &host.session_provider,
            None,
            false,
        )
        .await
        .expect("adopt resident sandbox"),
        super::session::SessionEnvironmentAdoptionDisposition::Ready,
    );
    let reused = host
        .ctx_for("t-resident-adoption", None)
        .await
        .expect("the exact resident sandbox is idempotently accepted");
    assert_eq!(
        reused.env.as_ref().expect("reused Environment").handle(),
        resident_handle,
        "idempotent adoption may rebuild the cache but retains physical identity"
    );

    let foreign = host
        .session_provider
        .create(&host.sandbox_spec("t-foreign-resident"))
        .await
        .expect("foreign sandbox");
    let foreign_binding = serde_json::to_string(&foreign.handle()).unwrap();
    let error = match host
        .adopt_bound_session_environment(
            "t-resident-adoption",
            Some(&foreign_binding),
            &host.session_provider,
            None,
            false,
        )
        .await
    {
        Ok(_) => panic!("a resident session must reject a different sandbox"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Internal);
    assert_eq!(
        error.message,
        "sandbox t-foreign-resident does not belong to Session t-resident-adoption"
    );
    assert_eq!(
        resident.env.as_ref().expect("eager environment").handle(),
        resident_handle
    );
}

#[tokio::test]
async fn retained_session_accepts_only_an_adoption_of_its_exact_sandbox() {
    let storage = tempfile::tempdir().expect("storage dir");
    let (host, managed) = dispatch_test_host(
        SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path()),
    );
    install_test_session_application(&host);
    install_complete_test_session_for_realization(
        &managed,
        "t-retained-adoption",
        bare_session("assistant", host.local_workspace()),
    )
    .await
    .expect("install retained complete Session projection");
    let original = host
        .ctx_for("t-retained-adoption", None)
        .await
        .expect("initial session");
    let retained_handle = original.env.as_ref().expect("eager environment").handle();
    host.evict_session_for_rebuild("t-retained-adoption").await;
    drop(original);

    let foreign = host
        .session_provider
        .create(&host.sandbox_spec("t-foreign-retained"))
        .await
        .expect("foreign sandbox");
    let foreign_binding = serde_json::to_string(&foreign.handle()).unwrap();
    let error = match host
        .adopt_bound_session_environment(
            "t-retained-adoption",
            Some(&foreign_binding),
            &host.session_provider,
            None,
            false,
        )
        .await
    {
        Ok(_) => panic!("a retained session must reject a different sandbox"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Internal);
    assert_eq!(
        error.message,
        "sandbox t-foreign-retained does not belong to Session t-retained-adoption"
    );

    let retained_binding = serde_json::to_string(&retained_handle).unwrap();
    assert_eq!(
        host.adopt_bound_session_environment(
            "t-retained-adoption",
            Some(&retained_binding),
            &host.session_provider,
            None,
            false,
        )
        .await
        .expect("adopt retained sandbox"),
        super::session::SessionEnvironmentAdoptionDisposition::Ready,
    );
    let rebuilt = host
        .ctx_for("t-retained-adoption", None)
        .await
        .expect("the exact retained sandbox rebuilds the runtime context");
    assert_eq!(
        rebuilt.env.as_ref().expect("eager environment").handle(),
        retained_handle
    );
    assert_eq!(
        host.session_environment_handle("t-retained-adoption").await,
        Some(retained_handle)
    );
}

// ---------------------------------------------------------------------------
// run/resume fail-closed boundaries (ADR-0048 gap review)
//
// These guard the double-run / wrong-tool / forged-approval seams: a caller must
// not be able to start a second Run on an Awaiting Thread, resume a Run that was never
// Awaiting, answer the wrong pending tool, or cross the built-in↔client-executed
// binding when resuming. All of them must fail *closed* with a BadRequest and
// leave the run untouched.
// ---------------------------------------------------------------------------

/// Awaits on the Ask-gated built-in `write` until it sees a tool result, then ends.
struct AwaitOnWriteModel;

#[async_trait::async_trait]
impl LlmExecutor for AwaitOnWriteModel {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        use awaken_runtime_contract::llm::ToolCall;
        let saw_tool = request.messages.iter().any(|m| m.role == Role::Tool);
        let output = if saw_tool {
            AssistantOutput::text("done")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "w1".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({ "path": "note.txt", "content": "x" }),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
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

fn user(text: &str) -> Vec<Message> {
    vec![Message::text(MessageId("u1".into()), Role::User, text)]
}

/// A Thread awaiting on a tool decision must reject a fresh `run`: starting a second
/// Run over an Awaiting Run would double-execute the Awaiting Run's side effects. The
/// guard fails closed with BadRequest and does not touch the await.
#[tokio::test]
async fn run_on_an_awaiting_thread_fails_closed() {
    // Causes: C1 committed Thread truth already contains an Awaiting Run.
    // Constraint/Invariant: a new foreground Run cannot bypass the existing
    // resume ticket or create a second active writer. Decision rule: submit under
    // C1 and require the documented fail-closed effect with no new Run.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel));
    let r1 = host
        .run(None, "t-awaiting", user("hi"))
        .await
        .expect("Run 1");
    assert!(
        matches!(r1.state, RunState::Awaiting),
        "Run awaits on write"
    );

    let err = host
        .run(None, "t-awaiting", user("again"))
        .await
        .err()
        .expect("a second run on an awaiting thread must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("awaiting a tool decision"),
        "message names the await: {}",
        err.message
    );

    // The await still resumes cleanly afterwards — the rejected run was a no-op.
    let r2 = host
        .resume(
            "t-awaiting",
            "w1",
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("resume the untouched await");
    assert!(matches!(r2.state, RunState::Ended(_)));
}

/// Resuming a thread that has no awaiting run is a caller error, not a panic: there
/// is no run to answer, so it fails closed with BadRequest.
#[tokio::test]
async fn resume_with_no_awaiting_run_fails_closed() {
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel));
    let err = host
        .resume(
            "t-idle",
            "w1",
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .err()
        .expect("resume with nothing awaiting must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("no awaiting run"),
        "message names the missing await: {}",
        err.message
    );
}

/// A resume whose `tool_use_id` does not name the pending tool must be rejected —
/// otherwise a caller could resume the wrong tool. Fails closed with BadRequest and
/// the real await survives.
#[tokio::test]
async fn resume_with_a_wrong_tool_use_id_fails_closed() {
    // Test design. Causes: C1 the committed ticket awaits tool-use id A; C2 resume
    // supplies foreign id B. Effects: E1 C2 is rejected and the Run remains
    // Awaiting. Constraint/Invariant: exact tool-use identity fences replies.
    // Decision rule: exercise B != A and require E1 with no tool result commit.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel));
    let r1 = host
        .run(None, "t-wrongid", user("hi"))
        .await
        .expect("Run 1");
    assert!(matches!(r1.state, RunState::Awaiting));

    let err = host
        .resume(
            "t-wrongid",
            "not-the-pending-id",
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .err()
        .expect("a mismatched tool_use_id must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("does not match the pending tool"),
        "message names the mismatch: {}",
        err.message
    );

    // The genuine id still resumes — the mismatch did not consume the await.
    let r2 = host
        .resume(
            "t-wrongid",
            &r1.pending.expect("a pending tool").tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("the real id resumes");
    assert!(matches!(r2.state, RunState::Ended(_)));
}

/// The built-in↔client binding is enforced on resume: a client-tool *result* may
/// not answer a built-in (Ask-gated) tool. Failing open here would let a caller
/// forge an approval by delivering a fabricated result instead of a decision.
#[tokio::test]
async fn client_result_cannot_answer_a_builtin_tool() {
    // Test design R7. Causes: C1 the pending ticket is a permission target; C2 a
    // custom/generic client result targets it. Effects: E1 C2 is rejected; E2 no
    // reply is committed and the exact ticket remains resumable. Constraint:
    // client results answer only client-owned targets. Decision rule: exercise
    // the mismatch, then resume the same ticket correctly to prove E1+E2.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel));
    let r1 = host.run(None, "t-bind1", user("hi")).await.expect("Run 1");
    let pending = r1.pending.expect("awaiting on the built-in write");
    assert!(!pending.client_executed, "write is a built-in tool");

    let err = host
        .resume(
            "t-bind1",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("forged")],
                is_error: false,
            },
        )
        .await
        .err()
        .expect("a client result must not answer a built-in tool");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("built-in"),
        "message names the binding: {}",
        err.message
    );
    let resumed = host
        .resume(
            "t-bind1",
            &pending.tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("R7 rejected mismatch leaves the exact permission await intact");
    assert!(matches!(resumed.state, RunState::Ended(_)), "R7/E2");
}

/// The other direction of the binding: a confirmation may not answer a
/// client-executed tool (which expects a result, not a permission decision).
#[tokio::test]
async fn confirm_cannot_answer_a_client_tool() {
    // Test design R6. Causes: C1 the pending ticket belongs to a client-executed
    // target; C2 a confirm/permission reply targets it. Effects: E1 C2 is
    // rejected; E2 no reply is committed and the exact ticket remains resumable.
    // Constraint: typed reply kind must match target ownership. Decision rule:
    // exercise the inverse mismatch, then answer correctly to prove E1+E2.
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(ClientLookupModel), "stub")
            .with_client_tools(HashSet::from(["lookup".to_string()])),
    );
    let r1 = host.run(None, "t-bind2", user("hi")).await.expect("Run 1");
    let pending = r1.pending.expect("awaiting on the client tool");
    assert!(pending.client_executed, "lookup is client-executed");

    let err = host
        .resume(
            "t-bind2",
            &pending.tool_use_id,
            HostResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .err()
        .expect("a confirmation must not answer a client tool");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("client-executed"),
        "message names the binding: {}",
        err.message
    );
    let resumed = host
        .resume(
            "t-bind2",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        )
        .await
        .expect("R6 rejected mismatch leaves the exact client await intact");
    assert!(matches!(resumed.state, RunState::Ended(_)), "R6/E2");
}

/// The happy path for the client-executed binding: a `ClientResult` delivers the
/// caller-Run tool's output, it reaches the model's next inference, and the Run
/// ends. This is the direct-ingress row of the resume-delivery decision table;
/// `durable_client_result_settles_the_authoritative_dispatch` covers the durable
/// row against the same model and protocol-neutral Host API.
#[tokio::test]
async fn client_result_delivers_a_client_tool_result_and_ends_the_run() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 direct ingress; C2 valid committed client-tool
    // ticket; C3 exact ClientResult. Effects: E1 resume executes inline once;
    // E2 result reaches the next inference; E3 Run ends. Constraint: no durable
    // dispatch is authored in direct mode.
    //
    // | Rule | ingress | ticket | answer | Effects |
    // | R1 | direct | valid client tool | exact result | E1+E2+E3 |
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(ClientLookupModel), "stub")
            .with_client_tools(HashSet::from(["lookup".to_string()])),
    );
    let r1 = host.run(None, "t-client", user("hi")).await.expect("Run 1");
    let pending = r1.pending.expect("awaiting on the client tool");

    let r2 = host
        .resume(
            "t-client",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        )
        .await
        .expect("client result resumes");
    assert!(matches!(r2.state, RunState::Ended(_)), "the Run ends");
    let reply = r2
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(
        reply, "result was sunny",
        "the delivered client result reached the model's next inference"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_foreground_run_relays_live_progress_before_committed_completion() {
    // Cause/effect graph: C1 the Host uses durable dispatch with a local pool;
    // C2 a foreground run supplies a StreamSink; C3 the model emits a live text
    // delta; C4 the run commits its terminal state. Effects: E1 C3 reaches the
    // caller's exact sink while its run-id registration is active; E2 C4 returns
    // the same authoritative terminal result as the non-streaming durable path;
    // E3 the registration is removed at settlement (owned by the registry unit
    // test). Constraint: background runs
    // and remote workers without this process-local registration still use the
    // committed projection and never create a durable delta source of truth.
    //
    // | Rule | durable | foreground sink | local worker | Effects |
    // | R1 | yes | yes | yes | E1+E2+E3 |
    // | R2 | yes | no | yes | E2 (existing durable tests) |
    // | R3 | yes | yes | remote | E2 fallback; no false replay guarantee |
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("in-memory dispatch"),
    );
    let host =
        Arc::new(SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch));
    let _managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool();
    let sink = Arc::new(awaken_store_inmem::MemoryStreamSink::new());

    let outcome = host
        .run_streaming(
            None,
            "t-durable-live",
            user("stream this"),
            sink.clone() as Arc<dyn awaken_agent_contract::stream::sink::Sink>,
        )
        .await
        .expect("durable streaming run");

    assert!(matches!(outcome.state, RunState::Ended(_)), "R1/E2");
    assert!(!sink.events().is_empty(), "R1/E1");
}

#[tokio::test]
async fn terminal_child_report_atomically_admits_one_deterministic_primary_run() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 child boundary is terminal; C2 report provenance
    // matches the exact child Run; C3 exact delivery is retried; C4 the child
    // activity epoch is still active. Effects: E1 freeze the exact report into
    // the deterministic primary activation; E2 enqueue one deterministic primary
    // Run carrying C4; E3 retry is an exact no-op; E4 no Outbox/Inbox copy exists.
    // Awaiting is intentionally absent: Managed projects its child lifecycle
    // directly and SessionApplication never invokes this command.
    //
    // | Rule | State | Provenance | Replay | Epoch | Effect |
    // | R1 | Ended | exact | no | active | E1+E2+E4 |
    // | R2 | Ended | exact | yes | same | E3 |
    // | R3 | Ended | wrong | any | any | reject before mutation |
    // | R4 | Ended | exact | any | zero | reject before mutation |
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("in-memory dispatch"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch.clone()),
    );
    let _managed = install_test_dispatch_runtime(&host);
    let child_run = RunId("child-report-run".into());
    let command = awaken_session_contract::SessionAgentReportContinuation {
        session_id: "primary-report".into(),
        source_thread_id: ThreadId("child-report-thread".into()),
        source_run_id: child_run.clone(),
        session_activity_epoch: 23,
        message: Message::text(
            MessageId::agent_thread_report(&child_run),
            Role::User,
            "Message from agent Researcher (thread child-report-thread):\nanswer",
        ),
    };

    host.continue_session_agent_report(command.clone())
        .await
        .expect("R1 terminal continuation");
    host.continue_session_agent_report(command)
        .await
        .expect("R2 exact replay");
    let rows = dispatch.list_dispatches().await.expect("R1 dispatch");
    assert_eq!(rows.len(), 1, "R1/R2 one deterministic root");
    assert_eq!(rows[0].thread_id, ThreadId("primary-report".into()));
    let inbox = dispatch
        .list(&ThreadId("primary-report".into()))
        .await
        .expect("R1 primary Inbox");
    assert!(inbox.is_empty(), "R1/E4 no parallel Inbox message");

    let bad_run = RunId("other-child".into());
    let rejected = host
        .continue_session_agent_report(awaken_session_contract::SessionAgentReportContinuation {
            session_id: "primary-report".into(),
            source_thread_id: ThreadId("child-report-thread".into()),
            source_run_id: bad_run,
            session_activity_epoch: 23,
            message: Message::text(
                MessageId::agent_thread_report(&child_run),
                Role::User,
                "forged",
            ),
        })
        .await;
    assert!(rejected.is_err(), "R3");
    assert_eq!(dispatch.list_dispatches().await.unwrap().len(), 1, "R3");
    let zero_epoch = host
        .continue_session_agent_report(awaken_session_contract::SessionAgentReportContinuation {
            session_id: "primary-report".into(),
            source_thread_id: ThreadId("child-report-thread".into()),
            source_run_id: child_run.clone(),
            session_activity_epoch: 0,
            message: Message::text(
                MessageId::agent_thread_report(&child_run),
                Role::User,
                "missing activity",
            ),
        })
        .await;
    assert!(zero_epoch.is_err(), "R4");
    assert_eq!(dispatch.list_dispatches().await.unwrap().len(), 1, "R4");
    let claimed = dispatch
        .claim("report-epoch-inspector", 30_000, 1, &Default::default())
        .await
        .expect("R1 claim")
        .expect("R1 report root remains runnable");
    assert_eq!(
        claimed.request.session_activity_epoch,
        Some(23),
        "R1 activity handoff is owned by the canonical dispatch, not its operational summary"
    );
    assert_eq!(
        claimed.request.activation.input.len(),
        1,
        "R1/E1 exact input"
    );
    assert_eq!(
        claimed.request.activation.input[0].id,
        MessageId::agent_thread_report(&child_run),
        "R1/E1 typed provenance stays inside the target Run"
    );
}

#[tokio::test]
async fn coordinated_child_reply_rotates_activity_at_the_parent_affined_outbox_boundary() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 parent-partition committed ticket matches the
    // child/tool; C2 dispatch is the exact unleased Awaiting child; C3 reply is
    // fresh/exact/conflicting; C4 Session activity epoch is the new trusted
    // coordinate; C5 the preflight Run/correlation fence is current/stale; C6
    // an ordinary ExistingThread follow-up is already bound to a deterministic
    // fresh Run; C7 that same operation is retried exactly or with changed
    // content; C8 accompanying System is exact/changed; C9 a later await reuses
    // the provider Run/correlation/call id under a new public Event occurrence.
    // Effects: E1 one Outbox delivery is relayed; E2 dispatch epoch rotates
    // before it becomes runnable; E3 exact retry creates no second input; E4 a
    // conflicting reply is rejected and cannot rotate again; E5 a concurrent
    // await change is rejected before any Outbox or dispatch mutation; E6 C6
    // stays queued while the old Awaiting Run owns the Thread; E7 the typed reply
    // wakes only that old Run, then its settlement releases the fresh follow-up;
    // E8 exact operation retry keeps one Run/message and changed content fails
    // without another effect; E9 a changed System payload conflicts just like a
    // changed reply and cannot create another durable delivery; E10 a delayed
    // old occurrence cannot answer the later await; E11 the newly admitted
    // occurrence remains independently deliverable.
    //
    // | Rule | Ticket/affinity | Dispatch | Reply | Effect |
    // |---|---|---|---|---|
    // | CR1 | exact | Awaiting | fresh | E1+E2 |
    // | CR2 | exact | Running with exact evidence | exact | E3 |
    // | CR3 | exact | Running with exact evidence | conflict | E4 |
    // | CR4 | stale fence | Awaiting | fresh | E5 |
    // | CR5 | exact + fresh follow-up | Awaiting | fresh/exact reply | E6+E7 |
    // | CR6 | exact + fresh follow-up | Awaiting | exact/changed follow-up retry | E8 |
    // | CR7 | exact + changed System | Running with evidence | fresh | E9 |
    // | CR10 | later identical await + old Thread version | exact old receipt | E10 |
    // | CR11 | later identical await + current version/new Event id | active | E11 |
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PendingTool, ToolAwaitReason};
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_run_ingress::{
        ContinuationAdmission, DispatchOutcome, Outbox as _, SessionChildAdmission,
    };

    let parent = ThreadId("coordinated-reply-parent".into());
    let child = ThreadId("coordinated-reply-child".into());
    let child_run = RunId("coordinated-reply-run".into());
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("coordinated reply dispatch"),
    );
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch.clone()),
    );
    let ctx = host
        .ctx_for(&parent.0, None)
        .await
        .expect("parent Session context");
    let commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("parent commit partition");
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("commit child Running");
    let ticket = ResumeTicket::new(
        "coordinated-reply-correlation",
        child_run.clone(),
        child.clone(),
        "coordinated-reply-snapshot",
        "coordinated-reply-catalog",
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "coordinated-reply-tool-use".into(),
            tool: PendingTool {
                tool_id: "write".into(),
                arguments: serde_json::json!({"path": "answer.txt"}),
            },
        },
    );
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            vec![awaiting_tool_batch_state(
                &child_run,
                &ticket,
                awaken_runtime_contract::ToolWaitKind::ToolPermission,
            )],
            Vec::new(),
        ))
        .await
        .expect("commit child Awaiting ticket");
    let admitted_thread_version = commit
        .recovery_snapshot(&child, &child_run)
        .await
        .expect("read admitted reply coordinate")
        .thread_version;

    let initial_epoch = 41;
    let resumed_epoch = 42;
    let request = RunDispatch::new(RunActivation::new(
        child_run.clone(),
        child.clone(),
        ctx.config.clone(),
        Vec::new(),
    ))
    .for_session(parent.clone())
    .with_session_activity_epoch(initial_epoch);
    dispatch
        .enqueue(request)
        .await
        .expect("enqueue child dispatch");
    let claimed = dispatch
        .claim_run(
            &child_run,
            "coordinated-reply-owner",
            DEFAULT_LEASE_MS,
            0,
            &Default::default(),
        )
        .await
        .expect("claim child dispatch")
        .expect("child dispatch is runnable");
    assert_eq!(
        dispatch
            .settle(
                &child_run,
                claimed.lease.epoch,
                DispatchOutcome::Awaiting,
                &[],
            )
            .await
            .expect("settle child Awaiting"),
        awaken_run_ingress::SettleOutcome::Applied
    );

    let follow_up_run = RunId("coordinated-reply-follow-up".into());
    let follow_up_request = RunDispatch::new(RunActivation::new(
        follow_up_run.clone(),
        child.clone(),
        ctx.config.clone(),
        Vec::new(),
    ))
    .for_session(parent.clone())
    .with_session_activity_epoch(43);
    let follow_up_input = awaken_run_ingress::PendingInput {
        message_id: "coordinated-reply-follow-up-message".into(),
        run_id: follow_up_run.clone(),
        thread_id: child.clone(),
        correlation_id: String::new(),
        available_at_ms: None,
        context_messages: Vec::new(),
        result: awaken_runtime_contract::resume::ResumeResult::Input(
            "continue after the pending tool".into(),
        ),
    };
    let follow_up_admission =
        || ContinuationAdmission::SessionChild(SessionChildAdmission::new(24, Vec::new()));
    dispatch
        .relay_and_enqueue(
            follow_up_input.clone(),
            follow_up_request.clone(),
            follow_up_admission(),
        )
        .await
        .expect("CR5 admit fresh ExistingThread follow-up");
    dispatch
        .relay_and_enqueue(
            follow_up_input.clone(),
            follow_up_request.clone(),
            follow_up_admission(),
        )
        .await
        .expect("CR6 exact follow-up retry");
    let changed_follow_up = awaken_run_ingress::PendingInput {
        context_messages: Vec::new(),
        result: awaken_runtime_contract::resume::ResumeResult::Input("changed intent".into()),
        ..follow_up_input.clone()
    };
    assert!(
        dispatch
            .relay_and_enqueue(
                changed_follow_up,
                follow_up_request.clone(),
                follow_up_admission(),
            )
            .await
            .is_err(),
        "CR6/E8 one operation cannot change its message"
    );
    assert_eq!(
        dispatch.list(&child).await.expect("CR6 child Inbox"),
        vec![awaken_run_ingress::PendingRecord {
            input: follow_up_input.clone(),
            revision: 1,
        }],
        "CR6/E8 one fresh Run owns one follow-up message"
    );
    assert!(
        dispatch
            .claim_run(
                &follow_up_run,
                "coordinated-reply-owner",
                DEFAULT_LEASE_MS,
                1,
                &Default::default(),
            )
            .await
            .expect("CR5 inspect queued follow-up")
            .is_none(),
        "CR5/E6 the old Awaiting Run remains the open writer"
    );

    let command = awaken_session_contract::SessionThreadToolReplyCommand {
        session_id: parent.0.clone(),
        tool_request_event_id: Some("evt-coordinated-reply-tool-use-1".into()),
        expected_thread_version: Some(admitted_thread_version),
        target: awaken_session_contract::SessionThreadTarget::Child(child.clone()),
        expected_run_id: child_run.clone(),
        expected_correlation_id: ticket.correlation_id.clone(),
        tool_use_id: "coordinated-reply-tool-use".into(),
        reply: awaken_session_contract::SessionThreadToolReply::Confirm(
            PermissionDecision::Allow { note: None },
        ),
        accompanying_system: Some(awaken_session_contract::SessionUserRunSystemInput {
            operation_id: "coordinated-reply-system".into(),
            content: vec![ContentBlock::text("reply context")],
        }),
    };
    let fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .expect("trusted Awaiting fence");
    let mut stale_fence = fence.clone();
    *stale_fence
        .prior_session_activity_epoch
        .as_mut()
        .expect("this fixture starts from a coordinated activity") += 1;
    assert!(
        host.reply_session_thread_tool(awaken_session_contract::SessionThreadToolReplyDelivery {
            command: command.clone(),
            fence: stale_fence,
            session_activity_epoch: resumed_epoch,
        })
        .await
        .is_err(),
        "CR4/E5"
    );
    assert_eq!(
        dispatch.relay().await.expect("CR4 no delivery"),
        0,
        "CR4/E5"
    );
    host.reply_session_thread_tool(awaken_session_contract::SessionThreadToolReplyDelivery {
        command: command.clone(),
        fence: fence.clone(),
        session_activity_epoch: resumed_epoch,
    })
    .await
    .expect("CR1 stage coordinated reply");
    let resumed = dispatch
        .claim_run(
            &child_run,
            "coordinated-reply-owner",
            DEFAULT_LEASE_MS,
            1,
            &Default::default(),
        )
        .await
        .expect("CR1 claim resumed child")
        .expect("CR1 relayed reply makes child runnable");
    assert_eq!(
        resumed.request.session_activity_epoch,
        Some(resumed_epoch),
        "CR1/E2"
    );
    assert_eq!(resumed.pending.len(), 1, "CR1/E1");
    assert_eq!(
        resumed.pending[0].correlation_id, ticket.correlation_id,
        "CR1/E1"
    );
    assert_eq!(
        resumed.pending[0].run_id, child_run,
        "CR5/E7 the exact typed reply wakes the old Run, not the follow-up"
    );
    assert_eq!(
        resumed.pending[0]
            .context_messages
            .iter()
            .map(|message| (&message.role, message.text_content()))
            .collect::<Vec<_>>(),
        vec![(&Role::System, "reply context".to_string())],
        "CR1/E1 the stable accompanying System is frozen in the same PendingInput"
    );
    assert!(
        dispatch
            .claim_run(
                &follow_up_run,
                "coordinated-reply-owner",
                DEFAULT_LEASE_MS,
                2,
                &Default::default(),
            )
            .await
            .expect("CR5 inspect follow-up while old Run is resumed")
            .is_none(),
        "CR5/E6 the resumed old Run still owns the Thread"
    );

    host.reply_session_thread_tool(awaken_session_contract::SessionThreadToolReplyDelivery {
        command: command.clone(),
        fence: fence.clone(),
        session_activity_epoch: resumed_epoch,
    })
    .await
    .expect("CR2 exact reply replay");
    let retried_inbox = dispatch.list(&child).await.expect("CR2 Inbox");
    assert_eq!(
        retried_inbox.len(),
        2,
        "CR2/E3 includes the queued follow-up"
    );
    assert_eq!(
        retried_inbox
            .iter()
            .filter(|record| record.input.run_id == child_run)
            .count(),
        1,
        "CR2/E3 the exact typed reply is not duplicated"
    );
    assert_eq!(
        retried_inbox
            .iter()
            .filter(|record| record.input.run_id == follow_up_run)
            .count(),
        1,
        "CR6/E8 the follow-up remains one message"
    );

    // Model the Runtime's single successful resume commit: Running consumes
    // the active ticket while ResumeApplied remains as the durable ingress
    // receipt. This is the response-loss partition that the old implementation
    // could not distinguish from an unresolved/stale reply.
    //
    // | Rule | Active ticket | Receipt | Retried payload | Effect |
    // |---|---|---|---|---|
    // | CR8 | absent | exact O1/correlation | exact O1 | already applied/no-op |
    // | CR9 | absent | O1/correlation | changed O2 | conflict/no mutation |
    // Constraint/invariant: ticket removal and receipt creation share the
    // Runtime ThreadCommit; neither Session nor dispatch owns a shadow receipt.
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            vec![
                awaken_agent_contract::audit::run_event::RunEvent::ResumeApplied {
                    operation_id: command.delivery_operation_id(),
                    correlation_id: ticket.correlation_id.clone(),
                }
                .into(),
            ],
        ))
        .await
        .expect("commit the accepted reply receipt while consuming its ticket");
    let replay_fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .expect("CR8 receipt replaces the consumed ticket for exact retry");
    assert!(
        replay_fence.already_applied,
        "CR8 exact receipt is terminal"
    );
    host.reply_session_thread_tool(awaken_session_contract::SessionThreadToolReplyDelivery {
        command: command.clone(),
        fence: replay_fence,
        session_activity_epoch: resumed_epoch,
    })
    .await
    .expect("CR8 exact response-loss retry is a no-op");
    assert_eq!(
        dispatch
            .list(&child)
            .await
            .expect("CR8 unchanged Inbox")
            .len(),
        2,
        "CR8 does not stage a second input"
    );

    let mut conflicting_system = command.clone();
    conflicting_system
        .accompanying_system
        .as_mut()
        .expect("CR7 System")
        .content = vec![ContentBlock::text("changed reply context")];
    assert!(
        host.reply_session_thread_tool(awaken_session_contract::SessionThreadToolReplyDelivery {
            command: conflicting_system,
            fence: fence.clone(),
            session_activity_epoch: resumed_epoch + 1,
        })
        .await
        .is_err(),
        "CR7/E9"
    );
    assert_eq!(
        dispatch.relay().await.expect("CR7 no staged conflict"),
        0,
        "CR7/E9"
    );

    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("commit a later await that reuses provider coordinates");
    let later_thread_version = commit
        .recovery_snapshot(&child, &child_run)
        .await
        .expect("read later reply coordinate")
        .thread_version;
    let delayed_old_fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .expect("CR10 old receipt absorbs a delayed old occurrence");
    assert!(delayed_old_fence.already_applied, "CR10/E10");
    let mut later_occurrence = command.clone();
    later_occurrence.tool_request_event_id = Some("evt-coordinated-reply-tool-use-2".into());
    later_occurrence.expected_thread_version = Some(later_thread_version);
    assert_ne!(
        later_occurrence.delivery_operation_id(),
        command.delivery_operation_id(),
        "CR11 each public Event occurrence owns a distinct durable operation"
    );
    let later_fence = host
        .session_thread_tool_reply_fence(&later_occurrence)
        .await
        .expect("CR11 current occurrence targets the later active await");
    assert!(!later_fence.already_applied, "CR11/E11");

    let mut conflicting = command;
    conflicting.reply =
        awaken_session_contract::SessionThreadToolReply::Confirm(PermissionDecision::Deny {
            reason: Some("changed".into()),
        });
    assert!(
        host.reply_session_thread_tool(awaken_session_contract::SessionThreadToolReplyDelivery {
            command: conflicting,
            fence,
            session_activity_epoch: resumed_epoch + 1,
        },)
            .await
            .is_err(),
        "CR3/E4"
    );
    assert_eq!(
        dispatch.relay().await.expect("CR3 no staged conflict"),
        0,
        "CR3/E4"
    );
    assert_eq!(
        dispatch
            .settle(
                &child_run,
                resumed.lease.epoch,
                DispatchOutcome::Done,
                &[resumed.pending[0].message_id.clone()],
            )
            .await
            .expect("CR5 settle the replied-to old Run"),
        awaken_run_ingress::SettleOutcome::Applied,
        "CR5/E7"
    );
    let follow_up = dispatch
        .claim_run(
            &follow_up_run,
            "coordinated-reply-owner",
            DEFAULT_LEASE_MS,
            3,
            &Default::default(),
        )
        .await
        .expect("CR5 claim released follow-up")
        .expect("CR5 old Run settlement releases the follow-up");
    assert_eq!(follow_up.pending, vec![follow_up_input], "CR5/E7");
}

#[tokio::test]
async fn primary_generic_tool_result_uses_the_same_fenced_durable_reply_path() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 target is Primary; C2 committed ticket is the exact
    // ExternalEvent Run/correlation/tool; C3 the generic result is normal/error;
    // C4 expected Run/correlation is exact/stale; C5 the committed ToolBatch
    // payload agrees/conflicts with the ticket. Effects: E1 the existing root
    // dispatch activity rotates and one client ToolOutput is staged; E2 normal
    // content and is_error survive unchanged; E3 stale admission coordinates
    // fail before Outbox mutation; E4 a C5 conflict also fails before Outbox
    // mutation. Child confirmation coverage lives in the
    // sibling decision table above; both targets use this same Host boundary.
    //
    // | Rule | Target | Ticket | Result | Effect |
    // |---|---|---|---|---|
    // | PR1 | Primary | exact | normal generic | E1+E2 |
    // | PR2 | Primary | stale Run/correlation | normal generic | E3 |
    // | PR3 | Primary | exact ticket/conflicting batch | normal generic | E4 |
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, PendingTool, ToolAwaitReason};
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_run_ingress::{DispatchOutcome, Outbox as _, RunDispatch};

    let parent = ThreadId("primary-reply-session".into());
    let run_id = RunId("primary-reply-run".into());
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("primary reply dispatch"),
    );
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch.clone()),
    );
    let ctx = host
        .ctx_for(&parent.0, None)
        .await
        .expect("primary Session context");
    let commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("primary commit partition");
    commit
        .commit(ThreadCommit::assemble(
            parent.clone(),
            RunDisposition::running(run_id.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("commit primary Running");
    let ticket = ResumeTicket::new(
        "primary-reply-correlation",
        run_id.clone(),
        parent.clone(),
        "primary-reply-snapshot",
        "primary-reply-catalog",
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::ClientExecution,
            call_id: "primary-reply-tool-use".into(),
            tool: PendingTool {
                tool_id: "client_lookup".into(),
                arguments: serde_json::json!({"query": "answer"}),
            },
        },
    );
    commit
        .commit(ThreadCommit::assemble(
            parent.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            vec![awaiting_tool_batch_state(
                &run_id,
                &ticket,
                awaken_runtime_contract::ToolWaitKind::ExternalResult,
            )],
            Vec::new(),
        ))
        .await
        .expect("commit primary Awaiting ticket");

    let initial_epoch = 51;
    let resumed_epoch = 52;
    dispatch
        .enqueue(
            RunDispatch::new(RunActivation::new(
                run_id.clone(),
                parent.clone(),
                ctx.config.clone(),
                Vec::new(),
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(initial_epoch),
        )
        .await
        .expect("enqueue primary dispatch");
    let claimed = dispatch
        .claim_run(
            &run_id,
            "primary-reply-owner",
            DEFAULT_LEASE_MS,
            0,
            &Default::default(),
        )
        .await
        .expect("claim primary dispatch")
        .expect("primary dispatch runnable");
    assert_eq!(
        dispatch
            .settle(&run_id, claimed.lease.epoch, DispatchOutcome::Awaiting, &[],)
            .await
            .expect("settle primary Awaiting"),
        awaken_run_ingress::SettleOutcome::Applied
    );

    let command = awaken_session_contract::SessionThreadToolReplyCommand {
        session_id: parent.0.clone(),
        tool_request_event_id: Some("evt-primary-reply-tool-use-1".into()),
        expected_thread_version: None,
        target: awaken_session_contract::SessionThreadTarget::Primary,
        expected_run_id: run_id.clone(),
        expected_correlation_id: ticket.correlation_id.clone(),
        tool_use_id: "primary-reply-tool-use".into(),
        reply: awaken_session_contract::SessionThreadToolReply::Result {
            content: vec![ContentBlock::text("client answer")],
            is_error: false,
        },
        accompanying_system: None,
    };
    let mut stale = command.clone();
    stale.expected_run_id = RunId("later-primary-run".into());
    assert!(
        host.session_thread_tool_reply_fence(&stale).await.is_err(),
        "PR2/E3"
    );
    assert_eq!(
        dispatch.relay().await.expect("PR2 no delivery"),
        0,
        "PR2/E3"
    );

    let mut conflicting_batch = awaken_runtime_contract::ToolBatch::for_step(
        run_id.clone(),
        0,
        [(
            awaken_runtime_contract::llm::ToolCall {
                call_id: "primary-reply-tool-use".into(),
                tool_id: "client_lookup".into(),
                arguments: serde_json::json!({"query": "another request"}),
            },
            awaken_runtime_contract::ToolRecoveryPolicy::default(),
        )],
    )
    .expect("PR3 internally valid conflicting batch");
    conflicting_batch
        .mark_awaiting(
            "primary-reply-tool-use",
            awaken_runtime_contract::ToolWaitKind::ExternalResult,
            ticket.correlation_id.clone(),
        )
        .expect("PR3 conflicting batch awaits");
    commit
        .commit(ThreadCommit::assemble(
            parent.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            vec![awaken_runtime_contract::ActiveToolBatch::write(&Some(
                conflicting_batch,
            ))],
            Vec::new(),
        ))
        .await
        .expect("PR3 commit conflicting batch projection");
    assert!(
        host.session_thread_tool_reply_fence(&command)
            .await
            .is_err(),
        "PR3/E4"
    );
    assert_eq!(
        dispatch.relay().await.expect("PR3 no delivery"),
        0,
        "PR3/E4"
    );
    commit
        .commit(ThreadCommit::assemble(
            parent.clone(),
            RunDisposition::awaiting(ticket.clone()),
            true,
            Vec::new(),
            vec![awaiting_tool_batch_state(
                &run_id,
                &ticket,
                awaken_runtime_contract::ToolWaitKind::ExternalResult,
            )],
            Vec::new(),
        ))
        .await
        .expect("restore exact PR1 batch fixture");

    let fence = host
        .session_thread_tool_reply_fence(&command)
        .await
        .expect("PR1 exact primary fence");
    host.reply_session_thread_tool(awaken_session_contract::SessionThreadToolReplyDelivery {
        command,
        fence,
        session_activity_epoch: resumed_epoch,
    })
    .await
    .expect("PR1 stage primary result");
    let resumed = dispatch
        .claim_run(
            &run_id,
            "primary-reply-owner",
            DEFAULT_LEASE_MS,
            1,
            &Default::default(),
        )
        .await
        .expect("claim resumed primary")
        .expect("primary result makes dispatch runnable");
    assert_eq!(
        resumed.request.session_activity_epoch,
        Some(resumed_epoch),
        "PR1/E1"
    );
    assert_eq!(resumed.pending.len(), 1, "PR1/E1");
    match &resumed.pending[0].result {
        awaken_runtime_contract::resume::ResumeResult::ToolResult(output) => {
            assert_eq!(output.call_id, "primary-reply-tool-use", "PR1/E2");
            assert_eq!(output.text(), "client answer", "PR1/E2");
            assert!(!output.is_error, "PR1/E2");
        }
        other => panic!("PR1 expected ToolResult, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn budget_resume_reuses_the_exact_run_and_durable_dispatch_generation() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 committed BudgetReached truth belongs to a root or
    // Session child Run; C2 a durable child row retains its prior activity; C3
    // exact deliveries race/replay; C4 the same Run later reaches another budget
    // pause; C5 a stale generation or ToolPermission ticket is presented.
    // Effects: E1 root handoff creates one existing-Run Dispatch; E2 child resume
    // atomically rotates its existing row; E3 exact replay has one pending input;
    // E4 C4 has a larger commit-derived generation and a distinct input while
    // retaining Run identity; E5 C5 is a no-op. Commit recovery and the existing
    // Dispatch row remain the only pause/delivery authorities.
    //
    // | Rule | Owner | Ticket/generation | Delivery | Effect |
    // |---|---|---|---|---|
    // | B1 | root | BudgetReached/current | fresh | E1 |
    // | B2 | child | BudgetReached/current | concurrent exact | E2+E3 |
    // | B3 | same child Run | second BudgetReached/new generation | fresh | E4 |
    // | B4 | child | old generation | stale | E5 |
    // | B5 | child | ToolPermission | budget discovery | E5 |
    use awaken_agent_contract::agent::awaiting::{
        AwaitTarget, PauseReason, PendingTool, ResumeTicket, ToolAwaitReason,
    };
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_run_ingress::{DispatchOutcome, RunDispatch};
    use awaken_session_contract::{SessionBudgetResumeDelivery, SessionBudgetResumeDisposition};

    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("budget resume dispatch"),
    );
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch.clone()),
    );

    let root = ThreadId("budget-resume-root".into());
    let root_run = RunId("budget-resume-root-run".into());
    let root_ctx = host.ctx_for(&root.0, None).await.expect("B1 root context");
    let snapshot_id = root_ctx.config.id.0.clone();
    let catalog_fingerprint = root_ctx.config.resolved_spec.catalog_fingerprint.0.clone();
    let budget_ticket = |thread: &ThreadId, run: &RunId| {
        ResumeTicket::new(
            run.0.clone(),
            run.clone(),
            thread.clone(),
            snapshot_id.clone(),
            catalog_fingerprint.clone(),
            AwaitTarget::Pause(PauseReason::BudgetReached),
        )
    };
    let root_commit = host.commit_for_read(&root.0).await.expect("B1 root commit");
    root_commit
        .commit(ThreadCommit::assemble(
            root.clone(),
            RunDisposition::running(root_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B1 root Running");
    root_commit
        .commit(ThreadCommit::assemble(
            root.clone(),
            RunDisposition::awaiting(budget_ticket(&root, &root_run)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B1 root budget pause");
    let root_pause = host
        .session_budget_resume_tickets(&root.0)
        .await
        .expect("B1 discover root pause")
        .into_iter()
        .next()
        .expect("B1 root ticket");
    assert_eq!(root_pause.ticket.run_id, root_run, "B1/E1");
    assert_eq!(root_pause.prior_session_activity_epoch, None, "B1/E1");
    let root_delivery = SessionBudgetResumeDelivery {
        session_id: root.0.clone(),
        thread_id: root.clone(),
        run_id: root_run.clone(),
        correlation_id: root_pause.ticket.correlation_id,
        pause_generation: root_pause.pause_generation,
        prior_session_activity_epoch: None,
        session_activity_epoch: 31,
    };
    let mut root_stale = root_delivery.clone();
    root_stale.pause_generation = root_stale.pause_generation.saturating_add(1);
    assert_eq!(
        host.resume_budget_reached(root_stale).await.unwrap(),
        SessionBudgetResumeDisposition::Stale,
        "B4/E5"
    );
    assert!(
        dispatch.list_dispatches().await.unwrap().is_empty(),
        "B4/E5"
    );
    assert_eq!(
        host.resume_budget_reached(root_delivery.clone())
            .await
            .unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B1/E1"
    );
    assert_eq!(
        host.resume_budget_reached(root_delivery).await.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B1 exact replay remains successful"
    );
    let root_rows = dispatch.list_dispatches().await.unwrap();
    assert_eq!(root_rows.len(), 1, "B1/E1 one handoff row");
    assert_eq!(root_rows[0].run_id, root_run, "B1/E1 same Run");
    assert_eq!(root_rows[0].session_activity_epoch, Some(31), "B1/E1");
    let root_pending = dispatch.list(&root).await.unwrap();
    assert_eq!(root_pending.len(), 1, "B1 exact replay has one input");
    assert_eq!(root_pending[0].input.run_id, root_run, "B1/E1");

    let parent = ThreadId("budget-resume-child-parent".into());
    let child = ThreadId("budget-resume-child".into());
    let child_run = RunId("budget-resume-child-run".into());
    host.ctx_for(&parent.0, None)
        .await
        .expect("B2 parent context");
    let child_commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("B2 parent commit partition");
    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B2 child Running");
    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(budget_ticket(&child, &child_run)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B2 child budget pause");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &child.0,
                &child_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(41),
        )
        .await
        .expect("B2 child dispatch");
    let initial_child_claim = dispatch
        .claim_run(
            &child_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            0,
            &Default::default(),
        )
        .await
        .expect("B2 initial claim")
        .expect("B2 initial child work");
    dispatch
        .settle(
            &child_run,
            initial_child_claim.lease.epoch,
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("B2 initial Awaiting settlement");
    let first_pause = host
        .session_budget_resume_tickets(&parent.0)
        .await
        .expect("B2 discover child pause")
        .into_iter()
        .find(|pause| pause.ticket.run_id == child_run)
        .expect("B2 child ticket");
    assert_eq!(first_pause.prior_session_activity_epoch, Some(41), "B2/E2");
    let first_delivery = SessionBudgetResumeDelivery {
        session_id: parent.0.clone(),
        thread_id: child.clone(),
        run_id: child_run.clone(),
        correlation_id: first_pause.ticket.correlation_id.clone(),
        pause_generation: first_pause.pause_generation,
        prior_session_activity_epoch: Some(41),
        session_activity_epoch: 42,
    };
    let (first, replay) = tokio::join!(
        host.resume_budget_reached(first_delivery.clone()),
        host.resume_budget_reached(first_delivery.clone()),
    );
    assert_eq!(
        first.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B2"
    );
    assert_eq!(
        replay.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B2 exact concurrent replay"
    );
    let resumed = dispatch
        .claim_run(
            &child_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            1,
            &Default::default(),
        )
        .await
        .expect("B2 resumed claim")
        .expect("B2 resumed child");
    assert_eq!(resumed.request.session_activity_epoch, Some(42), "B2/E2");
    assert_eq!(resumed.pending.len(), 1, "B2/E3");
    assert_eq!(resumed.pending[0].run_id, child_run, "B2/E2 same Run");
    let first_message_id = resumed.pending[0].message_id.clone();

    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(child_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B3 same Run resumes");
    child_commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::awaiting(budget_ticket(&child, &child_run)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B3 same Run pauses again");
    dispatch
        .settle(
            &child_run,
            resumed.lease.epoch,
            DispatchOutcome::Awaiting,
            std::slice::from_ref(&first_message_id),
        )
        .await
        .expect("B3 settle second pause");
    let second_pause = host
        .session_budget_resume_tickets(&parent.0)
        .await
        .expect("B3 discover second pause")
        .into_iter()
        .find(|pause| pause.ticket.run_id == child_run)
        .expect("B3 second child ticket");
    assert_eq!(second_pause.ticket.run_id, child_run, "B3/E4 same Run");
    assert_ne!(
        second_pause.pause_generation, first_pause.pause_generation,
        "B3/E4 commit-derived generations differ"
    );
    assert_eq!(
        host.resume_budget_reached(first_delivery).await.unwrap(),
        SessionBudgetResumeDisposition::Stale,
        "B4/E5 old generation"
    );
    let after_stale = dispatch
        .list_dispatches()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.run_id == child_run)
        .expect("B4 child row");
    assert_eq!(after_stale.session_activity_epoch, Some(42), "B4/E5");
    assert!(dispatch.list(&child).await.unwrap().is_empty(), "B4/E5");
    let second_delivery = SessionBudgetResumeDelivery {
        session_id: parent.0.clone(),
        thread_id: child.clone(),
        run_id: child_run.clone(),
        correlation_id: second_pause.ticket.correlation_id,
        pause_generation: second_pause.pause_generation,
        prior_session_activity_epoch: Some(42),
        session_activity_epoch: 43,
    };
    assert_eq!(
        host.resume_budget_reached(second_delivery).await.unwrap(),
        SessionBudgetResumeDisposition::Dispatched,
        "B3/E4"
    );
    let resumed_again = dispatch
        .claim_run(
            &child_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            2,
            &Default::default(),
        )
        .await
        .expect("B3 second resumed claim")
        .expect("B3 second resumed child");
    assert_eq!(resumed_again.pending.len(), 1, "B3/E4");
    assert_eq!(resumed_again.pending[0].run_id, child_run, "B3/E4");
    assert_ne!(
        resumed_again.pending[0].message_id, first_message_id,
        "B3/E4"
    );

    let action_parent = ThreadId("budget-action-parent".into());
    let action_child = ThreadId("budget-action-child".into());
    let action_run = RunId("budget-action-run".into());
    let action_commit = host
        .commit_for_read(&action_parent.0)
        .await
        .expect("B5 parent partition");
    action_commit
        .commit(ThreadCommit::assemble(
            action_child.clone(),
            RunDisposition::running(action_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B5 Running");
    let action_ticket = ResumeTicket::new(
        action_run.0.clone(),
        action_run.clone(),
        action_child.clone(),
        snapshot_id.clone(),
        catalog_fingerprint.clone(),
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id: "budget-action-call".into(),
            tool: PendingTool {
                tool_id: "write".into(),
                arguments: serde_json::json!({"path":"requires-action.txt"}),
            },
        },
    );
    action_commit
        .commit(ThreadCommit::assemble(
            action_child.clone(),
            RunDisposition::awaiting(action_ticket),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("B5 ToolPermission pause");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &action_child.0,
                &action_run.0,
            ))
            .for_session(action_parent.clone())
            .with_session_activity_epoch(51),
        )
        .await
        .expect("B5 action dispatch");
    let action_claim = dispatch
        .claim_run(
            &action_run,
            "budget-resume-worker",
            DEFAULT_LEASE_MS,
            3,
            &Default::default(),
        )
        .await
        .unwrap()
        .expect("B5 action claim");
    dispatch
        .settle(
            &action_run,
            action_claim.lease.epoch,
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("B5 action Awaiting settlement");
    assert!(
        host.session_budget_resume_tickets(&action_parent.0)
            .await
            .expect("B5 discovery")
            .is_empty(),
        "B5/E5 ToolPermission remains the higher-priority required action"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn durable_client_result_settles_the_authoritative_dispatch() {
    use awaken_run_ingress::DispatchQueue as _;

    async fn bounded_stage<T>(
        stage: &'static str,
        future: impl std::future::Future<Output = T>,
    ) -> T {
        match tokio::time::timeout(std::time::Duration::from_secs(10), future).await {
            Ok(output) => output,
            Err(_) => panic!("R2/E6 durable client-result test timed out at {stage}"),
        }
    }

    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 durable ingress; C2 a claimed Run settles Awaiting
    // on a client-tool ticket; C3 the exact ClientResult arrives; C4 resumed work
    // ends; C5 resumed work awaits on a new ticket. Effects: E1 input is appended
    // to the canonical durable Inbox; E2 the Worker alone claims/resumes/settles;
    // E3 Done removes the dispatch row; E4 Awaiting retains exactly one row; E5
    // committed result reaches the model; E6 any missing dispatch settlement
    // fails at its exact bounded stage instead of hanging the suite. Constraints:
    // direct ingress remains the R1 path above; one ticket accepts one
    // idempotency identity.
    //
    // | Rule | ingress | initial state | resume result | Effect |
    // | R1 | direct | Awaiting | Ended | inline E1/E2 not applicable (sibling test) |
    // | R2 | durable | Awaiting(old ticket) | Ended | E1+E2+E3+E5 |
    // | R3 | durable | Awaiting(old ticket) | Awaiting(new ticket) | E1+E2+E4 |
    // | R4 | durable | Awaiting(old ticket) | exact retry | one Inbox identity |
    // R3 is owned by run-ingress worker settle tests; R4 by the resume-identity
    // test plus each Inbox backend's idempotency-conflict conformance suite.
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory().expect("in-memory dispatch"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(ClientLookupModel), "stub")
            .with_client_tools(HashSet::from(["lookup".to_string()]))
            .with_dispatch_store(dispatch.clone()),
    );
    let _managed = install_test_dispatch_runtime(&host);
    host.ensure_dispatch_pool();

    let first = bounded_stage(
        "initial host.run settlement",
        host.run(None, "t-durable-client", user("hi")),
    )
    .await
    .expect("durable Run awaits");
    assert!(matches!(first.state, RunState::Awaiting), "R2 precondition");
    let pending = first.pending.expect("client tool ticket");
    let awaiting = bounded_stage(
        "first dispatch list after initial Awaiting",
        dispatch.list_dispatches(),
    )
    .await
    .expect("list awaiting dispatch");
    assert_eq!(awaiting.len(), 1, "one durable dispatch owns the wait");
    assert_eq!(awaiting[0].run_id, first.run_id);
    assert_eq!(
        awaiting[0].state,
        awaken_run_ingress::DispatchState::Awaiting
    );

    let resumed = bounded_stage(
        "host.resume client-result settlement",
        host.resume(
            "t-durable-client",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        ),
    )
    .await
    .expect("durable worker resumes the client result");
    assert!(matches!(resumed.state, RunState::Ended(_)), "R2/E5");
    let settled_dispatches = bounded_stage(
        "final dispatch list after terminal settlement",
        dispatch.list_dispatches(),
    )
    .await
    .expect("list settled dispatches");
    assert!(
        settled_dispatches
            .iter()
            .all(|summary| summary.run_id != resumed.run_id),
        "R2/E3: terminal settlement removes the authoritative dispatch row"
    );
}

#[tokio::test]
async fn pending_client_tool_query_uses_committed_ticket_during_projection_gap() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1=the Runtime has atomically committed a client-tool
    // call and its Awaiting ticket; C2=the foreground protocol has not yet
    // published its process-local step projection; C3=a peer protocol queries the
    // pending tool; C4=a fresh user Run races that wait; C5=the exact client
    // result arrives. E1=the exact committed call is returned as client-executed;
    // E2=no pending tool is fabricated when committed truth has no open wait;
    // E3=the fresh Run is rejected; E4=the original Run resumes and ends.
    // Decision table:
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    // | R1   | T  | T  | T  | F  | F  | E1     |
    // | R2   | T  | T  | F  | T  | F  | E3     |
    // | R3   | T  | T  | F  | F  | T  | E4     |
    // | R4   | F  | T/F| T  | F  | F  | E2 (resume_with_no_awaiting_run) |
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(ClientLookupModel), "stub")
            .with_client_tools(HashSet::from(["lookup".to_string()])),
    );
    let first = host
        .run(None, "t-cross-protocol-pending", user("hi"))
        .await
        .expect("client tool awaits");
    let expected = first.pending.expect("run exposes the pending client tool");

    // The query has no mutable process-local awaiting position to corrupt or
    // synchronize; committed Thread truth is its only input.
    let observed = host
        .pending_tool("t-cross-protocol-pending")
        .await
        .expect("committed ticket read succeeds")
        .expect("committed ticket remains queryable");
    assert_eq!(observed.tool_use_id, expected.tool_use_id);
    assert_eq!(observed.name, "lookup");
    assert_eq!(observed.input, serde_json::json!({ "q": "weather" }));
    assert!(observed.client_executed);
    assert!(host.is_awaiting("t-cross-protocol-pending").await);

    let error = match host
        .run(
            None,
            "t-cross-protocol-pending",
            user("do not overtake the wait"),
        )
        .await
    {
        Ok(_) => panic!("committed wait must reject a competing user Run"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::BadRequest);

    let resumed = host
        .resume(
            "t-cross-protocol-pending",
            &observed.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        )
        .await
        .expect("committed wait resumes without the disposable position");
    assert!(matches!(resumed.state, RunState::Ended(_)));
    assert!(!host.is_awaiting("t-cross-protocol-pending").await);
}

/// Superseding a run requires durable ingress; a default (direct-ingress) host
/// must fail closed rather than silently behave like a plain run.
#[tokio::test]
async fn supersede_run_without_durable_ingress_fails_closed() {
    // Test design. Causes: C1 supersede is requested while no durable ingress
    // authority is configured. Effects: E1 the command fails without cancelling
    // or starting any Run. Constraint/Invariant: supersession is a durable queue
    // mutation and has no in-memory fallback. Decision rule: execute C1 and
    // require fail-closed zero side effects.
    let host = host_requiring_write_confirmation(Arc::new(AwaitOnWriteModel));
    // Await first so the supersede path is not short-circuited by the awaiting guard
    // (supersede is allowed on an awaiting thread; the durable check is what must fire).
    let r1 = host.run(None, "t-sup", user("hi")).await.expect("Run 1");
    assert!(matches!(r1.state, RunState::Awaiting));

    let err = host
        .supersede_run(None, "t-sup", user("newest wins"))
        .await
        .err()
        .expect("supersede without durable ingress must fail");
    assert_eq!(err.kind, HostErrorKind::BadRequest);
    assert!(
        err.message.contains("durable ingress"),
        "message names the requirement: {}",
        err.message
    );
}

#[tokio::test]
async fn managed_session_root_rejects_legacy_host_run_ingress() {
    use awaken_run_ingress::DispatchQueue as _;

    // Cause/effect graph: C1 the Thread is marked as a Managed Session root;
    // C2 a caller selects ordinary or superseding Host Run ingress. Effects:
    // E1 both commands fail as bad requests before context realization; E2 the
    // dispatch authority remains empty. Decision rules: M1 C1+ordinary=>E1+E2;
    // M2 C1+supersede=>E1+E2. Constraint: Session Run reservation and its
    // activity receipt remain the sole Managed-root ingress; no Host fallback.
    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("managed ingress fence dispatch"),
    );
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_dispatch_store(dispatch.clone());
    host.session_slots
        .update("managed-root", |slot| slot.session_dispatch = true);

    for result in [
        host.run(None, "managed-root", user("ordinary bypass"))
            .await,
        host.supersede_run(None, "managed-root", user("supersede bypass"))
            .await,
    ] {
        let error = match result {
            Ok(_) => panic!("Managed root must reject legacy Host ingress"),
            Err(error) => error,
        };
        assert_eq!(error.kind, HostErrorKind::BadRequest, "E1");
        assert!(error.message.contains("Session-owned Run ingress"), "E1");
    }
    assert!(
        dispatch
            .list_dispatches()
            .await
            .expect("E2 inspect dispatch authority")
            .is_empty(),
        "E2"
    );
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
    events: Arc<Mutex<Vec<&'static str>>>,
    scenario: TerminalRecoveryScenario,
    fence_refresh_probe: Option<Arc<TerminalFenceRefreshProbe>>,
}

#[derive(Default)]
struct TerminalFenceRefreshProbe {
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

async fn await_terminal_fence_refresh_gate<T: std::fmt::Debug, E: std::fmt::Debug>(
    gate: &tokio::sync::Notify,
    task: &mut tokio::task::JoinHandle<Result<T, E>>,
    boundary: &str,
) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        tokio::select! {
            () = gate.notified() => {}
            result = &mut *task => {
                panic!("terminal preparation ended before {boundary}: {result:?}")
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("terminal preparation did not reach {boundary}"));
}

#[async_trait::async_trait]
impl awaken_sandbox_container::ContainerEnvironmentProvider for TerminalRecoveryProvider {
    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        managed_test_container_capabilities()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn create_environment(
        &self,
        _spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "absent-auxiliary fixture never creates a container",
        ))
    }

    async fn observe_environment_for_effect(
        &self,
        adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
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
            TerminalRecoveryScenario::Restoring => Err(pc::SandboxError::new(
                "Restoring terminal cleanup must not observe an ordinary Environment",
            )),
        }
    }

    async fn adopt_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "absent-auxiliary fixture never adopts a live container",
        ))
    }

    async fn adopt_environment_for_effect(
        &self,
        adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        effect_fence: Option<&awaken_sandbox_container::ContainerEffectFence>,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        if !matches!(self.scenario, TerminalRecoveryScenario::LiveMemory) {
            return Err(pc::SandboxError::new(
                "non-live terminal recovery fixture cannot be ordinarily adopted",
            ));
        }
        let effect_fence = effect_fence.ok_or_else(|| {
            pc::SandboxError::new("live terminal Memory adoption requires an exact effect fence")
        })?;
        effect_fence.validate_identity()?;
        self.events.lock().unwrap().push("adopt-live-memory");
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
        _terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Option<Arc<dyn awaken_sandbox_container::ContainerEnvironment>>, pc::SandboxError>
    {
        match self.scenario {
            TerminalRecoveryScenario::OrphanClaim => Err(pc::SandboxError::new(
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
                            pc::SandboxError::new(
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
                            pc::SandboxError::new(
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
                            pc::SandboxError::new(
                                "live Memory fixture requires an exact durable handle",
                            )
                        })?
                        .clone(),
                    events: self.events.clone(),
                    mode: TerminalRecoveryEnvironmentMode::LiveMemory,
                    fence_refresh_probe: self.fence_refresh_probe.clone(),
                })))
            }
            TerminalRecoveryScenario::Restoring => Err(pc::SandboxError::new(
                "Restoring terminal cleanup must use exact target disposal",
            )),
        }
    }

    async fn dispose_restored_environment(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
    ) -> Result<(), pc::SandboxError> {
        if !matches!(self.scenario, TerminalRecoveryScenario::Restoring) {
            return Err(pc::SandboxError::new(
                "non-Restoring fixture cannot dispose a restore target",
            ));
        }
        self.events.lock().unwrap().push("dispose-restored");
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
    fn id(&self) -> &str {
        &self.handle.sandbox_id
    }

    fn handle(&self) -> pc::SandboxHandle {
        self.handle.clone()
    }

    async fn cleanup_checkpoint_for_terminal(
        &self,
        _request: &pc::SandboxCheckpointRequest,
        _store: &dyn pc::SandboxCheckpointStore,
        _expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<(), pc::SandboxError> {
        self.events.lock().unwrap().push("checkpoint-live-io");
        if let Some(probe) = &self.fence_refresh_probe {
            probe.record("checkpoint", terminal_effect_fence);
            probe.checkpoint_entered.notify_one();
            probe.checkpoint_release.notified().await;
            return Ok(());
        }
        Err(pc::SandboxError::new(
            "terminal recovery fixture has no pending checkpoint upload",
        ))
    }

    async fn spawn(
        &self,
        _command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        self.events.lock().unwrap().push("spawn-live-io");
        Err(pc::SandboxError::new(
            "terminal recovery fixture must not spawn a process",
        ))
    }

    async fn attach(
        &self,
        _requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "terminal recovery fixture must not attach a mount",
        ))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        self.events.lock().unwrap().push("artifacts-live-io");
        Err(pc::SandboxError::new(
            "container Artifact capture must use the batch file seam",
        ))
    }

    async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.events.lock().unwrap().push("artifact-read-live-io");
        Err(pc::SandboxError::new(
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
        Err(pc::SandboxError::new(
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
            Err(pc::SandboxError::new(
                "cleanup-only terminal recovery fixture must not renew a lease",
            ))
        }
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new(
            "terminal recovery fixture requires the exact effect fence",
        ))
    }

    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
    ) -> Result<(), pc::SandboxError> {
        let expected = self.handle.memory_materializations()?.unwrap_or_default();
        if expected != complete_materializations {
            return Err(pc::SandboxError::new(
                "terminal recovery Memory acknowledgement changed durable evidence",
            ));
        }
        if matches!(
            self.mode,
            TerminalRecoveryEnvironmentMode::LiveMemory
                | TerminalRecoveryEnvironmentMode::UnavailableAuxiliaryMemory
        ) {
            self.events.lock().unwrap().push("ack-memory");
        }
        if let Some(probe) = &self.fence_refresh_probe {
            probe.record("memory", effect_fence);
            probe.memory_entered.notify_one();
            probe.memory_release.notified().await;
        }
        Ok(())
    }

    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        self.events.lock().unwrap().push("prepare-source");
        if let Some(probe) = &self.fence_refresh_probe {
            probe.record("provider", effect_fence);
        }
        Ok(effect_fence.clone())
    }

    async fn dispose_for_effect(
        &self,
        _authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), pc::SandboxError> {
        self.events.lock().unwrap().push("dispose-physical");
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_sandbox_container::SandboxControlServicePublisher for TerminalRecoveryEnvironment {
    async fn publish_sandbox_control_service(
        &self,
        _kind: awaken_sandbox_container::SandboxControlServiceKind,
        _service: Arc<dyn awaken_sandbox_container::SandboxControlService>,
    ) -> Result<
        Box<dyn awaken_sandbox_container::PublishedSandboxControlService>,
        awaken_sandbox_container::SandboxControlPublishError,
    > {
        // This recovery fixture declares no control-service topology. It must
        // fail closed instead of manufacturing a provider publication lease.
        Err(awaken_sandbox_container::SandboxControlPublishError)
    }
}

#[async_trait::async_trait]
impl awaken_sandbox_container::ContainerEnvironment for TerminalRecoveryEnvironment {
    async fn spawn_agent_process(
        &self,
        _command: pc::Command,
    ) -> Result<awaken_sandbox_container::RuntimeAgentProcess, pc::SandboxError> {
        self.events.lock().unwrap().push("agent-live-io");
        Err(pc::SandboxError::new(
            "terminal recovery fixture must not launch an Agent process",
        ))
    }

    async fn read_files(
        &self,
        root: &str,
    ) -> Result<Vec<awaken_sandbox_container::EnvironmentFile>, pc::SandboxError> {
        match self.mode {
            TerminalRecoveryEnvironmentMode::Disposing => {
                self.events.lock().unwrap().push("read-files-live-io");
                Err(pc::SandboxError::new(
                    "Disposing cleanup must not read the container filesystem",
                ))
            }
            TerminalRecoveryEnvironmentMode::UnavailableAuxiliaryMemory => {
                self.events.lock().unwrap().push("read-files-live-io");
                Err(pc::SandboxError::new(
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
                Ok(vec![awaken_sandbox_container::EnvironmentFile {
                    path: "changed.txt".into(),
                    bytes: b"changed-under-terminal-v2".to_vec(),
                }])
            }
            TerminalRecoveryEnvironmentMode::LiveMemory => Err(pc::SandboxError::new(
                "live terminal fixture was asked to read an unfrozen path",
            )),
        }
    }
}

fn terminal_fixture_container_handle(thread: &str, spec: &pc::SandboxSpec) -> pc::SandboxHandle {
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
    pc::SandboxHandle::container_v2(
        thread,
        pc::ContainerSandboxHandleV2 {
            previous: pc::ContainerSandboxHandleV1 {
                container_id: "awaken-terminal-fixture".into(),
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection: false,
                continuation_excluded_paths: Vec::new(),
                runtime_handle: Some(pc::ContainerContinuationHandle::KubernetesContinuationV2 {
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

fn terminal_memory_resources() -> awaken_session_contract::ResolvedSessionResources {
    effective_resources(vec![TestInput {
        kind: "memory_store".into(),
        id: "terminal-memory-store".into(),
        mount_path: "/workspace/.mnt/terminal-memory".into(),
        access: awaken_resource_contract::ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }])
}

fn terminal_memory_projection() -> awaken_session_contract::FrozenSessionProjection {
    terminal_memory_projection_with_idle_retention(Default::default())
}

fn terminal_memory_projection_with_idle_retention(
    idle_retention: awaken_session_contract::EnvironmentIdleRetentionPolicy,
) -> awaken_session_contract::FrozenSessionProjection {
    let resources = terminal_memory_resources();
    let mut projection = remote_terminal_cleanup_projection_with_idle_retention(idle_retention);
    projection.resource_revision = 1;
    projection.resources = resources.clone();
    projection.previous_resource_manifest = Some(
        awaken_session_contract::SessionResourceManifest::at_revision(
            "terminal-workspace",
            1,
            resources,
        ),
    );
    projection
}

fn terminal_memory_evidence() -> pc::MemoryMaterializationEvidence {
    pc::MemoryMaterializationEvidence::new(
        "terminal-memory-store",
        "/workspace/.mnt/terminal-memory",
        vec![pc::MemoryMaterializationHead {
            path: "seed.txt".into(),
            id: "terminal-memory-head".into(),
            content_sha256: "terminal-memory-seed-sha".into(),
        }],
    )
    .expect("canonical terminal Memory evidence")
}

struct RecordingTerminalMemoryReferenceEncoder {
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl awaken_resource_contract::MemoryMaterializationReferenceEncoder<awaken_run_ingress::RunClaim>
    for RecordingTerminalMemoryReferenceEncoder
{
    fn encode(
        &self,
        _workspace_id: &str,
        _memory_store_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        _access: awaken_resource_contract::ResourceAccess,
        _fence: &awaken_run_ingress::RunClaim,
    ) -> Result<String, awaken_resource_contract::MemoryMaterializationReferenceError> {
        self.events.lock().unwrap().push("encode-run-v1");
        Ok("run-v1-reference".into())
    }
}

impl
    awaken_resource_contract::MemoryMaterializationReferenceEncoder<
        awaken_session_contract::SessionTerminalMemoryIntent,
    > for RecordingTerminalMemoryReferenceEncoder
{
    fn encode(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        _config_version: awaken_resource_contract::ConfigVersion,
        access: awaken_resource_contract::ResourceAccess,
        _fence: &awaken_session_contract::SessionTerminalMemoryIntent,
    ) -> Result<String, awaken_resource_contract::MemoryMaterializationReferenceError> {
        if workspace_id != "terminal-workspace"
            || memory_store_id != "terminal-memory-store"
            || access != awaken_resource_contract::ResourceAccess::ReadWrite
        {
            return Err(
                awaken_resource_contract::MemoryMaterializationReferenceError::new(
                    "terminal Memory encoder received a foreign frozen input",
                ),
            );
        }
        self.events.lock().unwrap().push("encode-terminal-v2");
        Ok("terminal-v2-reference".into())
    }
}

struct RecordingTerminalMemoryMounter {
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait::async_trait]
impl pc::MemoryMounter for RecordingTerminalMemoryMounter {
    async fn mount(
        &self,
        _store_id: &str,
        _host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> Result<Box<dyn pc::MemoryMount>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "terminal reconciliation fixture must not materialize a new Run mount",
        ))
    }

    async fn reconcile_recovered_copy(
        &self,
        operation_reference: &str,
        evidence: &pc::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        if operation_reference != "terminal-v2-reference"
            || evidence.store_id != "terminal-memory-store"
            || evidence.mount_path != "/workspace/.mnt/terminal-memory"
            || files != [("changed.txt".into(), b"changed-under-terminal-v2".to_vec())]
            || access != pc::MountAccess::ReadWrite
        {
            return Err(pc::SandboxError::new(
                "terminal Memory reconciliation changed its frozen inputs",
            ));
        }
        self.events.lock().unwrap().push("reconcile-terminal-v2");
        Ok(())
    }
}

#[tokio::test]
async fn source_release_preparation_is_withheld_for_an_absent_pod_with_live_claim() {
    /* Host continuation table HS1. Causes: C1 aggregate binding carries exact
     * V2 Pod UID A plus retained claim UID P; C2 A is absent while P remains
     * live; C3 the current realization lease authorizes source disposal.
     * Effects: E1 provider rejects before mutation because no Pod finalizer can
     * serialize P against Rebuild; E2 Host emits no preparation receipt and
     * retains the exact binding for explicit recovery. Rule HS1
     * C1+C2+C3=>E1+E2. A/P total absence is the separate effect-fenced
     * response-loss row and does not fabricate a cleanup Environment. */
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        events: events.clone(),
        scenario: TerminalRecoveryScenario::OrphanClaim,
        fence_refresh_probe: None,
    });
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub").with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        ),
    );
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);
    let thread = "absent-auxiliary-source";
    let projection = remote_terminal_cleanup_projection();
    let source_manifest = projection
        .previous_resource_manifest
        .clone()
        .expect("HS1 frozen source manifest");
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        thread,
        projection,
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("HS1 install frozen source projection");
    host.register_thread_resource_manifest(thread, source_manifest);

    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread));
    let source_binding = serde_json::to_string(&handle).unwrap();
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "source-cleanup-worker".into(),
        runtime_incarnation: "source-cleanup-worker:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    host.install_session_environment_owner_projection(
        thread,
        "terminal-workspace",
        &awaken_session_contract::SessionEnvironmentState::Resident {
            binding: source_binding.clone(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .expect("HS1 project the exact durable source binding");
    host.session_slots.update(thread, |slot| {
        slot.realization_lease = Some(lease.clone());
    });
    let generation = awaken_session_contract::SandboxGeneration::new(
        thread,
        1,
        u64::MAX,
        "environment-a",
        "image-a",
    );
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        thread,
        "dispose-source",
        &generation,
        1,
        Some(lease.clone()),
        None,
    );
    let preparation: awaken_session_contract::SourceReleasePreparationEffect =
        serde_json::from_value(serde_json::json!({
            "operation": operation,
            "lease": lease,
        }))
        .expect("HS1 decode the aggregate-owned preparation transport");

    let error = awaken_session_contract::SessionRuntime::prepare_checkpoint_source_disposal(
        &managed,
        thread,
        &preparation,
        &generation,
        &source_binding,
    )
    .await
    .expect_err("HS1 orphan claim cannot produce a preparation receipt");
    assert!(error.message.contains("cleanup gate"), "HS1/E1: {error:?}");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["observe-orphan-claim"],
        "HS1/E1 zero preparation/disposal"
    );
    assert_eq!(
        host.session_slots
            .read(thread, |slot| {
                slot.environment_owner.durable_binding().map(str::to_owned)
            })
            .flatten()
            .as_deref(),
        Some(source_binding.as_str()),
        "HS1/E2 binding retained"
    );
    assert!(host.session_environment(thread).await.is_none(), "HS1/E2");
}

struct RecordingTerminalArtifactRecovery {
    events: Arc<Mutex<Vec<&'static str>>>,
    receipt: awaken_resource_contract::ArtifactPublicationReceipt,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::ArtifactPublicationFence>
    for RecordingTerminalArtifactRecovery
{
    async fn publish(
        &self,
        _publication: awaken_resource_contract::ArtifactPublication<
            awaken_run_ingress::ArtifactPublicationFence,
        >,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "total-absence recovery must not publish live bytes",
        ))
    }

    async fn recover(
        &self,
        _recovery: awaken_resource_contract::ArtifactRecovery<
            awaken_run_ingress::ArtifactPublicationFence,
        >,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.events.lock().unwrap().push("recover-artifacts");
        Ok(vec![self.receipt.clone()])
    }
}

struct RecordingScopedArtifactAuthority {
    events: Arc<Mutex<Vec<&'static str>>>,
    records: Arc<Mutex<Vec<awaken_resource_contract::FileRecord>>>,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ArtifactPublisher<awaken_run_ingress::ArtifactPublicationFence>
    for RecordingScopedArtifactAuthority
{
    async fn publish(
        &self,
        _publication: awaken_resource_contract::ArtifactPublication<
            awaken_run_ingress::ArtifactPublicationFence,
        >,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "Disposing cleanup must not republish source-operation bytes",
        ))
    }

    async fn recover(
        &self,
        recovery: awaken_resource_contract::ArtifactRecovery<
            awaken_run_ingress::ArtifactPublicationFence,
        >,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.events.lock().unwrap().push("recover-terminal-scope");
        recovery.receipts_from_records(self.records.lock().unwrap().clone())
    }
}

struct RecordingTerminalCheckpointStore {
    events: Arc<Mutex<Vec<&'static str>>>,
    expected_id: String,
}

#[async_trait::async_trait]
impl pc::SandboxCheckpointStore for RecordingTerminalCheckpointStore {
    async fn put(
        &self,
        _metadata: &pc::CheckpointObjectMetadata,
        _bytes: Vec<u8>,
    ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "terminal committed-checkpoint cleanup must not upload bytes",
        ))
    }

    async fn get(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "terminal committed-checkpoint cleanup must not read bytes",
        ))
    }

    async fn delete(&self, id: &str) -> Result<(), pc::SandboxError> {
        if id != self.expected_id {
            return Err(pc::SandboxError::new(
                "terminal cleanup targeted another checkpoint object",
            ));
        }
        self.events.lock().unwrap().push("delete-checkpoint");
        Ok(())
    }
}

fn terminal_recovery_effect(
    thread: &str,
    owner: &str,
) -> (
    awaken_session_contract::SessionRealizationLease,
    awaken_session_contract::SessionTerminalCleanupEffect,
) {
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: owner.into(),
        runtime_incarnation: format!("{owner}:incarnation"),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    let mut cleanup = awaken_session_contract::SessionCleanupOperation::default();
    assert!(cleanup.request(thread));
    cleanup.freeze_targets(thread, [], 0, 0).unwrap();
    let command = cleanup.command_for(thread, thread).unwrap();
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(command, lease.clone());
    (lease, effect)
}

fn disposing_terminal_environment(
    thread: &str,
    lease: &awaken_session_contract::SessionRealizationLease,
    handle: &pc::SandboxHandle,
    checkpoint_id: &str,
) -> (
    awaken_session_contract::SessionEnvironmentState,
    awaken_session_contract::SourceReleasePreparationEffect,
    awaken_session_contract::SourceReleasePreparedReceipt,
) {
    let generation = awaken_session_contract::SandboxGeneration::new(
        thread,
        1,
        u64::MAX,
        "environment-disposing",
        "image-disposing",
    );
    let operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        thread,
        "suspend",
        &generation,
        7,
        Some(lease.clone()),
        None,
    );
    let preparation: awaken_session_contract::SourceReleasePreparationEffect =
        serde_json::from_value(serde_json::json!({
            "operation": operation.clone(),
            "lease": lease.clone(),
        }))
        .expect("decode the aggregate-owned continuation preparation");
    let source_binding = serde_json::to_string(handle).expect("encode disposing source binding");
    let receipt = awaken_session_contract::SourceReleasePreparedReceipt::try_new(
        preparation.clone(),
        preparation.sandbox_effect_fence().unwrap(),
        &generation,
        &source_binding,
    )
    .unwrap();
    let state = awaken_session_contract::SessionEnvironmentState::Suspending {
        operation: operation.clone(),
        source_effect_id: Box::new("terminal-source-effect".into()),
        source_binding,
        generation,
        suspend_phase: awaken_session_contract::SuspendPhase::Disposing,
        checkpoint: Some(awaken_session_contract::SandboxCheckpointRef {
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
    projection: &awaken_session_contract::FrozenSessionProjection,
    lease: awaken_session_contract::SessionRealizationLease,
    children: impl IntoIterator<Item = String>,
    publication: Option<awaken_session_contract::SessionRepositoryPublicationIntent>,
) -> awaken_session_contract::PersistedSession {
    let mut session = awaken_session_contract::PersistedSession::frozen_with_budget(
        session_id,
        projection.baseline.clone(),
        awaken_session_contract::SessionResourceState::from_active(projection.resources.clone()),
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
                .request_with_publication(session_id, intent)
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
    effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    projection: &awaken_session_contract::FrozenSessionProjection,
) -> awaken_session_contract::SessionTerminalCleanupPreparationAuthorization {
    let session_id = &effect.command.session_id;
    let thread_id = &effect.command.thread_id;
    let session = terminal_test_session(
        session_id,
        projection,
        effect.lease.clone(),
        (thread_id != session_id).then_some(thread_id.clone()),
        None,
    );
    let inherited_provider_disposal = session
        .authorize_terminal_cleanup_effect(effect)
        .expect("derive preparation authorization from the complete Session aggregate");
    awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
        effect.clone(),
        projection.workspace_id.clone(),
        inherited_provider_disposal,
    )
    .expect("close the aggregate-derived terminal preparation authorization")
}

fn terminal_disposal_effect(
    preparation: awaken_session_contract::SessionCleanupPreparation,
    current_lease: awaken_session_contract::SessionRealizationLease,
    projection: &awaken_session_contract::FrozenSessionProjection,
) -> awaken_session_contract::SessionTerminalCleanupDisposalEffect {
    let session_id = preparation.effect.command.session_id.clone();
    let thread_id = preparation.effect.command.thread_id.clone();
    let mut session = terminal_test_session(
        &session_id,
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
    let repository_preparation = awaken_session_contract::SessionCleanupRepositoryPreparation::new(
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
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => command,
        action => panic!("expected aggregate disposal action, got {action:?}"),
    };
    awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(command, current_lease)
}

#[test]
fn terminal_provider_fences_use_only_the_current_same_generation_renewal() {
    // Cause/effect graph: C1 a terminal effect was admitted more than 20s ago
    // and its asserted expiry elapsed; C2 the local slot either has a live
    // monotonic renewal, no renewal, or a successor epoch; C3 the boundary is
    // preparation or destructive disposal. Effects: E1 a same-generation live
    // renewal authorizes both boundaries and projects its current expiry into
    // the provider fence; E2 an expired current generation fails closed; E3 a
    // successor epoch rejects the predecessor effect. The aggregate's durable
    // disposal preparation remains unchanged; no provider timer authority is
    // introduced.
    //
    // | Rule | asserted effect | slot current | boundary | Effect |
    // |---|---|---|---|---|
    // | R1 | expired >20s | same generation renewed/live | prepare + dispose | E1 |
    // | R2 | expired >20s | same expired lease | prepare + dispose | E2 |
    // | R3 | expired >20s | successor epoch/live | prepare + dispose | E3 |
    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let session_id = "terminal-renewed-provider-fence";
    let now_unix_ms = crate::terminal_repository_publication::runtime_unix_now_ms();
    let expired_lease = awaken_session_contract::SessionRealizationLease {
        owner: "terminal-worker".into(),
        runtime_incarnation: "terminal-worker:boot".into(),
        epoch: 12,
        expires_at_unix_ms: now_unix_ms.saturating_sub(20_001),
    };
    let renewed_lease = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
        ..expired_lease.clone()
    };
    let projection = remote_terminal_cleanup_projection();
    let asserted_session = terminal_test_session(
        session_id,
        &projection,
        expired_lease.clone(),
        std::iter::empty(),
        None,
    );
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        asserted_session
            .terminal_cleanup
            .command_for(session_id, session_id)
            .expect("R1 canonical terminal command"),
        expired_lease.clone(),
    );
    let disposal = terminal_disposal_effect(
        awaken_session_contract::SessionCleanupPreparation::try_new(
            &effect,
            effect.sandbox_effect_fence().unwrap(),
            Vec::new(),
        )
        .unwrap(),
        expired_lease.clone(),
        &projection,
    );

    host.session_slots.update(session_id, |slot| {
        slot.realization_lease = Some(renewed_lease.clone());
    });
    assert_eq!(
        host.terminal_cleanup_effect_fence(&effect)
            .expect("R1/E1 preparation fence")
            .expires_at_unix_ms,
        renewed_lease.expires_at_unix_ms,
        "R1/E1 preparation projects current expiry"
    );
    assert_eq!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .expect("R1/E1 disposal fence")
            .effect_fence()
            .expires_at_unix_ms,
        renewed_lease.expires_at_unix_ms,
        "R1/E1 disposal projects current expiry"
    );

    host.session_slots.update(session_id, |slot| {
        slot.realization_lease = Some(expired_lease.clone());
    });
    assert!(
        host.terminal_cleanup_effect_fence(&effect).is_err(),
        "R2/E2"
    );
    assert!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .is_err(),
        "R2/E2"
    );

    let successor = awaken_session_contract::SessionRealizationLease {
        epoch: expired_lease.epoch + 1,
        expires_at_unix_ms: now_unix_ms.saturating_add(30_000),
        ..expired_lease
    };
    host.session_slots.update(session_id, |slot| {
        slot.realization_lease = Some(successor);
    });
    assert!(
        host.terminal_cleanup_effect_fence(&effect).is_err(),
        "R3/E3"
    );
    assert!(
        host.terminal_cleanup_disposal_authorization(&disposal)
            .is_err(),
        "R3/E3"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_preparation_refreshes_the_fence_at_each_physical_boundary() {
    // Cause/effect graph: C1 one terminal preparation starts under lease L0;
    // C2 Artifact capture is a long I/O and Control installs same-generation
    // renewal L1 before the next physical boundary; C3 Memory acknowledgement
    // starts with L1, then blocks while Control installs L2; C4 checkpoint
    // cleanup starts with L2, then blocks while Control installs L3. Effects:
    // E1 Memory receives L1, not the pre-I/O L0 fence; E2 checkpoint receives
    // L2, not L0/L1; E3 provider preparation receives L3, not any predecessor;
    // E4 the receipt keeps work assertion L0 and separately binds provider P=L3.
    // The slot remains the single lease projection: this test adds no queue,
    // lock, timer, or independently advancing provider authority.
    //
    // | Rule | prior long I/O | current slot at boundary | Effect |
    // |---|---|---|---|
    // | R1 | Artifact under L0 | L1 | Memory receives L1 (E1) |
    // | R2 | Memory under L1 | L2 | checkpoint receives L2 (E2) |
    // | R3 | checkpoint under L2 | L3 | provider receives and receipt binds L3 (E3/E4) |
    let thread = "terminal-fence-refresh-between-effects";
    let (lease, effect) = terminal_recovery_effect(thread, "terminal-refresh-worker");
    let renewed = |expires_at_unix_ms| awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms,
        ..lease.clone()
    };
    let renewal_1 = renewed(lease.expires_at_unix_ms + 10_000);
    let renewal_2 = renewed(lease.expires_at_unix_ms + 20_000);
    let renewal_3 = renewed(lease.expires_at_unix_ms + 30_000);
    let events = Arc::new(Mutex::new(Vec::new()));
    let probe = Arc::new(TerminalFenceRefreshProbe::default());
    let provider = Arc::new(TerminalRecoveryProvider {
        events: events.clone(),
        scenario: TerminalRecoveryScenario::LiveMemory,
        fence_refresh_probe: Some(probe.clone()),
    });
    let raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_worker_upstream(awaken_worker_transport_security::WorkerUpstream::new(
            "http://coordinator.invalid",
        ))
        .with_memory_reference_encoder(Arc::new(RecordingTerminalMemoryReferenceEncoder {
            events: events.clone(),
        }))
        .with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        )
        .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
            events: events.clone(),
            expected_id: "unused-pending-checkpoint-id".into(),
        }));
    raw_host.install_memory_mounter(Arc::new(RecordingTerminalMemoryMounter {
        events: events.clone(),
    }));
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());

    let idle_retention = awaken_session_contract::EnvironmentIdleRetentionPolicy {
        mode: awaken_session_contract::EnvironmentIdleRetentionMode::CheckpointAndRelease,
        checkpoint_after_secs: 1,
        retention_secs: 3_600,
        expiry_behavior: Default::default(),
        max_checkpoint_bytes: 4_096,
        max_checkpoint_duration_secs: 30,
        checkpoint_format: "awaken-fs-tar-v1".into(),
    };
    let mut projection = terminal_memory_projection_with_idle_retention(idle_retention);
    let run_claim = awaken_run_ingress::RunClaim {
        run_id: awaken_agent_contract::agent::run::Id("terminal-refresh-run".into()),
        owner: "terminal-refresh-run-worker".into(),
        epoch: 1,
    };
    host.install_frozen_session_projection(
        thread,
        projection.clone(),
        Some(&run_claim),
        true,
        None,
    )
    .await
    .expect("install the exact frozen Memory/checkpoint projection");
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread))
        .with_memory_materializations(vec![terminal_memory_evidence()])
        .expect("install exact writable Memory evidence");
    let generation = awaken_session_contract::SandboxGeneration::new(
        thread,
        1,
        u64::MAX,
        "terminal-refresh-environment",
        "terminal-refresh-image",
    );
    let checkpoint_operation = awaken_session_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        thread,
        "suspend",
        &generation,
        7,
        Some(lease.clone()),
        None,
    );
    projection.environment = awaken_session_contract::SessionEnvironmentState::Suspending {
        operation: checkpoint_operation,
        source_effect_id: Box::new("terminal-source-effect".into()),
        source_binding: serde_json::to_string(&handle)
            .expect("encode the exact terminal source binding"),
        generation,
        suspend_phase: awaken_session_contract::SuspendPhase::Uploading,
        checkpoint: None,
        source_release_preparation: None,
    };
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            lease: lease.clone(),
        },
    )
    .await
    .expect("install the exact terminal generation");

    let authorization = terminal_preparation_authorization(&effect, &projection);
    let asserted_effect = effect.clone();
    let mut preparation = tokio::spawn(async move {
        managed
            .prepare_terminal_cleanup_for_effect(effect, authorization)
            .await
    });
    await_terminal_fence_refresh_gate(
        &probe.artifact_entered,
        &mut preparation,
        "Artifact capture",
    )
    .await;
    host.install_session_realization_lease(thread, renewal_1.clone());
    probe.artifact_release.notify_one();

    await_terminal_fence_refresh_gate(
        &probe.memory_entered,
        &mut preparation,
        "Memory acknowledgement",
    )
    .await;
    host.install_session_realization_lease(thread, renewal_2.clone());
    probe.memory_release.notify_one();

    await_terminal_fence_refresh_gate(
        &probe.checkpoint_entered,
        &mut preparation,
        "checkpoint cleanup",
    )
    .await;
    host.install_session_realization_lease(thread, renewal_3.clone());
    probe.checkpoint_release.notify_one();
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(2), preparation)
        .await
        .expect("terminal preparation completes after all boundary releases")
        .expect("terminal preparation task joins")
        .expect("same-generation renewals keep the preparation authorized");
    assert_eq!(
        receipt.effect, asserted_effect,
        "R3/E4 work assertion remains L0"
    );
    assert_eq!(
        receipt.provider_prepared_effect_fence(),
        &renewal_3
            .sandbox_effect_fence(asserted_effect.operation_id())
            .unwrap(),
        "R3/E4 provider predecessor is actual L3",
    );

    let fences = probe.fences.lock().unwrap();
    assert_eq!(fences.len(), 3, "R1-R3 one fence per physical boundary");
    for ((boundary, fence), (expected_boundary, expected_lease)) in fences.iter().zip([
        ("memory", &renewal_1),
        ("checkpoint", &renewal_2),
        ("provider", &renewal_3),
    ]) {
        assert_eq!(boundary, &expected_boundary, "R1-R3 boundary order");
        assert_eq!(
            fence.expires_at_unix_ms, expected_lease.expires_at_unix_ms,
            "{expected_boundary} receives the latest current fence",
        );
        assert_eq!(fence.owner, expected_lease.owner, "same owner generation");
        assert_eq!(
            fence.runtime_incarnation, expected_lease.runtime_incarnation,
            "same runtime generation",
        );
        assert_eq!(fence.epoch, expected_lease.epoch, "same epoch generation");
    }
}

fn terminal_recovery_receipt(
    thread: &str,
    effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    file_id: &str,
) -> awaken_resource_contract::ArtifactPublicationReceipt {
    let content = b"already durable";
    let content_id = awaken_resource_contract::content_id(content);
    let artifact_effect =
        awaken_resource_contract::harvest_idempotency_key(thread, "result.txt", &content_id);
    awaken_resource_contract::ArtifactPublicationReceipt {
        effect_id: artifact_effect.clone(),
        content_id: content_id.clone(),
        record: awaken_resource_contract::FileRecord {
            id: file_id.into(),
            workspace_id: "terminal-workspace".into(),
            blob_id: content_id,
            filename: "result.txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: content.len() as u64,
            created_at: "2026-08-30T00:00:00Z".into(),
            expires_at: None,
            downloadable: true,
            scope_id: Some(thread.into()),
            logical_path: Some("result.txt".into()),
            harvest_key: Some(artifact_effect),
            artifact_idempotency_scope: Some(effect.operation_id().into()),
            deleted: false,
        },
    }
}

async fn install_terminal_recovery_projection(
    host: &Arc<SharedHost>,
    managed: &crate::ManagedHost,
    thread: &str,
    lease: awaken_session_contract::SessionRealizationLease,
    environment: awaken_session_contract::SessionEnvironmentState,
) -> awaken_session_contract::FrozenSessionProjection {
    let mut projection = remote_terminal_cleanup_projection();
    awaken_session_contract::SessionRuntime::install_session_projection(
        managed,
        thread,
        projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("install frozen terminal recovery projection");
    projection.environment = environment;
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            lease,
        },
    )
    .await
    .expect("install exact terminal recovery assignment");
    projection
}

#[tokio::test]
async fn total_absence_recovers_artifacts_before_separate_terminal_disposal() {
    /* Host terminal recovery table HS2. Causes: C1 exact V2 source and every
     * typed physical participant are absent after disposal response loss; C2
     * the aggregate terminal effect remains live; C3 Resources already owns a
     * durable Artifact publication receipt. Effects: E1 provider returns no
     * Environment, so Host performs no live output read; E2 the existing
     * ArtifactHarvester invokes publisher.recover under the exact terminal
     * fence; E3 preparation carries that receipt without physical deletion; E4
     * a later aggregate-derived disposal returns exact absence evidence with no
     * live/source-dependent I/O. Rule HS2 C1+C2+C3=>E1->E2->E3; durable
     * preparation=>E4. No cleanup-only Sandbox or second receipt registry exists. */
    let thread = "total-absence-artifact-recovery";
    let (lease, effect) = terminal_recovery_effect(thread, "total-absence-worker");
    let receipt = terminal_recovery_receipt(thread, &effect, "file_total_absence_receipt");
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        events: events.clone(),
        scenario: TerminalRecoveryScenario::TotalAbsence,
        fence_refresh_probe: None,
    });
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub").with_session_container_provider(
        provider,
        Arc::new(crate::session_environment::UnusedHandExecutorFactory),
    );
    raw_host.artifact_publisher = Arc::new(RecordingTerminalArtifactRecovery {
        events: events.clone(),
        receipt: receipt.clone(),
    });
    let host = Arc::new(raw_host);
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread));
    let binding = serde_json::to_string(&handle).unwrap();
    let terminal_projection = install_terminal_recovery_projection(
        &host,
        &managed,
        thread,
        lease.clone(),
        awaken_session_contract::SessionEnvironmentState::Resident {
            binding,
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    )
    .await;

    let preparation_authorization =
        terminal_preparation_authorization(&effect, &terminal_projection);
    let preparation = managed
        .prepare_terminal_cleanup_for_effect(effect.clone(), preparation_authorization)
        .await
        .expect("HS2 total-absence terminal preparation");
    assert_eq!(preparation.artifact_receipts, vec![receipt], "HS2/E3");
    assert_eq!(
        preparation.provider_prepared_effect_fence(),
        &effect.sandbox_effect_fence().unwrap(),
        "HS2/E3 no provider boundary uses the final current fence",
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            "observe-total-absence",
            "prepare-total-absence",
            "recover-artifacts",
        ],
        "HS2/E1->E2"
    );
    assert!(
        host.session_slots.contains(thread),
        "HS2 preparation is not disposal"
    );

    events.lock().unwrap().clear();
    let disposal_effect = terminal_disposal_effect(preparation, lease, &terminal_projection);
    let disposal = managed
        .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
        .await
        .expect("HS2 total-absence physical-disposal proof");
    assert_eq!(disposal.effect_id, disposal_effect.command.effect_id);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["observe-total-absence", "prepare-total-absence"],
        "HS2 disposal performs only effect-free reconstruction and exact absence proof",
    );
}

#[tokio::test]
async fn disposing_cleanup_prepares_durability_before_resuming_physical_disposal() {
    /* Host terminal I/O table HS3. Causes: C1 the exact V2 realization is
     * observed Disposing, which is provider evidence that every required
     * source-durability effect preceded its cleanup gate without asserting a
     * terminal-scoped Artifact association; C2 the aggregate terminal fence
     * remains live; C3 Resources owns an already-durable source-operation File
     * with no terminal idempotency scope; C4 aggregate state carries a committed
     * checkpoint object. Effects: E1 Host discards any executable owner and
     * preserves the continuation's exact provider preparation A without
     * replaying raw terminal preparation T; E2 it does zero
     * live artifact/container/checkpoint I/O, preserves C3, and does not invent
     * a terminal receipt association; E3 it
     * deletes C4 through the existing checkpoint store without reading the Pod;
     * E4 only an aggregate-derived typed authorization then resumes exact
     * physical disposal; E5 preparation carries no fabricated receipt. Ordinary Ready/Terminal observations use
     * Live I/O (existing terminal tests); total absence is HS2; incompatible
     * orphan state is HS1.
     *
     * | Rule | observation | receipt/checkpoint | live I/O | dispose | Effect |
     * | H1 | Ready/Terminal | any | allowed | after durable effects | existing Live path |
     * | H2 | Disposing with durable continuation A | prior unscoped/exact | forbidden | receipt recovery, then A/fpA + live successor disposal | E1+E2+E3+E4+E5 |
     * | H3 | absent | exact/any | forbidden | none | HS2 |
     * | H4 | incompatible | any | forbidden | none | HS1 | */
    let thread = "disposing-artifact-recovery";
    let (lease, effect) = terminal_recovery_effect(thread, "disposing-worker");
    let mut source_file =
        terminal_recovery_receipt(thread, &effect, "file_disposing_source").record;
    source_file.artifact_idempotency_scope = None;
    let source_records = Arc::new(Mutex::new(vec![source_file.clone()]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        events: events.clone(),
        scenario: TerminalRecoveryScenario::Disposing,
        fence_refresh_probe: None,
    });
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub").with_session_container_provider(
        provider,
        Arc::new(crate::session_environment::UnusedHandExecutorFactory),
    );
    raw_host.artifact_publisher = Arc::new(RecordingScopedArtifactAuthority {
        events: events.clone(),
        records: source_records.clone(),
    });
    let checkpoint_id = "checkpoint-disposing".to_string();
    raw_host =
        raw_host.with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
            events: events.clone(),
            expected_id: checkpoint_id.clone(),
        }));
    let host = Arc::new(raw_host);
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread));
    let (disposing_environment, source_preparation, source_preparation_receipt) =
        disposing_terminal_environment(thread, &lease, &handle, &checkpoint_id);
    let terminal_projection = install_terminal_recovery_projection(
        &host,
        &managed,
        thread,
        lease.clone(),
        disposing_environment,
    )
    .await;

    let preparation_authorization =
        terminal_preparation_authorization(&effect, &terminal_projection);
    let preparation = managed
        .prepare_terminal_cleanup_for_effect(effect.clone(), preparation_authorization)
        .await
        .expect("HS3 prepare exact disposing cleanup");
    assert!(
        preparation.artifact_receipts.is_empty(),
        "HS3/E2 terminal recovery cannot associate an unscoped source File"
    );
    assert_eq!(
        source_records.lock().unwrap().as_slice(),
        std::slice::from_ref(&source_file),
        "HS3/E2 prior source publication remains durable"
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            "observe-disposing",
            "prepare-disposing",
            "recover-terminal-scope",
            "delete-checkpoint",
        ],
        "HS3/E1->E2->E3; preparation must not physically dispose"
    );
    assert!(
        !events.lock().unwrap().contains(&"dispose-physical"),
        "HS3 physical disposal waits for the aggregate preparation CAS",
    );

    let disposal_effect = terminal_disposal_effect(preparation, lease, &terminal_projection);
    assert_eq!(
        disposal_effect
            .command
            .provider_disposal
            .prepared_effect_fence(),
        &source_preparation.sandbox_effect_fence().unwrap(),
        "HS3/E1 physical disposal inherits continuation A exactly",
    );
    assert_eq!(
        disposal_effect
            .command
            .provider_disposal
            .preparation_fingerprint(),
        source_preparation_receipt.receipt_fingerprint(),
        "HS3/E1 physical disposal inherits continuation fpA exactly",
    );
    let disposal = managed
        .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
        .await
        .expect("HS3 resume exact physical disposal");
    assert_eq!(disposal.effect_id, disposal_effect.command.effect_id);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            "observe-disposing",
            "prepare-disposing",
            "recover-terminal-scope",
            "delete-checkpoint",
            "observe-disposing",
            "dispose-physical",
        ],
        "HS3/E1->E2->E3->E4; disposal repeats no source-dependent effect",
    );
}

#[tokio::test]
async fn inherited_preparation_closes_absent_writable_memory_response_loss() {
    /* Host continuation-takeover table HS3b. Causes: C1 the aggregate root
     * carries a verified Disposing source receipt and inherited provider
     * preparation A; C2 the provider now proves the physical source totally
     * absent after response loss; C3 the durable handle/frozen transition join
     * names exact writable Copy evidence; C4 Artifact publication and the
     * committed checkpoint are independently durable. Effects: E1 the closed
     * root authorization, not provider observation, selects AlreadyPrepared;
     * E2 Host performs receipt-only Artifact recovery and checkpoint deletion,
     * but zero live Memory read, acknowledgement, or raw terminal provider
     * preparation; E3 aggregate disposal inherits A/fpA and treats exact total
     * absence as physical response-loss success.
     *
     * | Rule | aggregate A | provider observation | RW evidence | Effect |
     * | A1 | exact | DefinitivelyUnavailable | exact | E1 -> E2 -> E3 |
     * | A2 | none | DefinitivelyUnavailable+aux | exact | HM0 fail closed |
     * | A3 | exact | Disposing | exact/RO | HS3 recover and exact re-ack |
     * | A4 | none | Ready/Terminal | exact | HM1 live reconciliation |
     *
     * Thus neither observation nor a naked boolean is a second preparation
     * authority; only Control's effect-bound closed value can override A2. */
    let thread = "disposing-absent-writable-memory";
    let (lease, effect) = terminal_recovery_effect(thread, "disposing-absent-worker");
    let mut source_file =
        terminal_recovery_receipt(thread, &effect, "file_disposing_absent_source").record;
    source_file.artifact_idempotency_scope = None;
    let source_records = Arc::new(Mutex::new(vec![source_file.clone()]));
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        events: events.clone(),
        scenario: TerminalRecoveryScenario::TotalAbsence,
        fence_refresh_probe: None,
    });
    let checkpoint_id = "checkpoint-disposing-absent";
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        )
        .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
            events: events.clone(),
            expected_id: checkpoint_id.into(),
        }));
    raw_host.artifact_publisher = Arc::new(RecordingScopedArtifactAuthority {
        events: events.clone(),
        records: source_records.clone(),
    });
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());

    let mut projection = terminal_memory_projection();
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        thread,
        projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("A1 install frozen writable Memory input");
    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread))
        .with_memory_materializations(vec![terminal_memory_evidence()])
        .expect("A1 durable handle carries exact writable Copy evidence");
    let (disposing_environment, source_preparation, source_preparation_receipt) =
        disposing_terminal_environment(thread, &lease, &handle, checkpoint_id);
    projection.environment = disposing_environment;
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            lease: lease.clone(),
        },
    )
    .await
    .expect("A1 install exact Disposing assignment");

    let authorization = terminal_preparation_authorization(&effect, &projection);
    let expected_provider_preparation =
        awaken_provisioning_contract::SandboxDisposalPreparation::new(
            source_preparation.sandbox_effect_fence().unwrap(),
            source_preparation_receipt.receipt_fingerprint(),
        )
        .unwrap();
    assert_eq!(
        authorization.inherited_provider_disposal(),
        Some(&expected_provider_preparation),
        "A1/E1 closed authorization carries exact continuation A/fpA",
    );
    let preparation = managed
        .prepare_terminal_cleanup_for_effect(effect, authorization)
        .await
        .expect("A1 exact inherited preparation closes physical response loss");
    assert!(preparation.artifact_receipts.is_empty(), "A1/E2");
    assert_eq!(
        source_records.lock().unwrap().as_slice(),
        std::slice::from_ref(&source_file),
        "A1/E2 preserves the unscoped source publication",
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            "observe-total-absence",
            "prepare-total-absence",
            "recover-terminal-scope",
            "delete-checkpoint",
        ],
        "A1/E2 zero live Memory/ack/raw provider preparation/physical disposal",
    );

    events.lock().unwrap().clear();
    let disposal_effect = terminal_disposal_effect(preparation, lease, &projection);
    assert_eq!(
        disposal_effect.command.provider_disposal, expected_provider_preparation,
        "A1/E3 aggregate disposal keeps exact A/fpA",
    );
    managed
        .dispose_terminal_cleanup_for_effect(disposal_effect)
        .await
        .expect("A1/E3 total absence is exact physical response-loss success");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        ["observe-total-absence", "prepare-total-absence"],
        "A1/E3 physical phase performs observation/reconstruction only",
    );
}

#[tokio::test]
async fn unavailable_auxiliary_writable_memory_fails_before_terminal_effects() {
    /* Host unavailable-Memory table HM0. Causes: C1 provider proves the primary
     * Sandbox definitively absent but reconstructs an exact auxiliary owner;
     * C2 the durable V2 handle carries one writable Copy materialization; C3
     * the frozen Resource transition names that same RW MemoryStore; C4 no
     * prior provider disposal preparation/gate exists. Effects: E1 the shared
     * Option-aware Memory join recognizes the exact writable intent; E2 Host
     * fails closed before Artifact publication/recovery, Memory acknowledgement,
     * checkpoint cleanup, provider preparation, or physical disposal; E3 the
     * reconstructed exact auxiliary owner remains available for retry/takeover.
     *
     * | Rule | primary | auxiliary | Memory | prior prep | Effect |
     * | M0 | absent | exact | RW Copy | none | E1 -> E2 + E3 |
     * | M1 | absent | none | none/RO | none | HS2 receipt-only recovery |
     * | M2 | Disposing gate | exact | exact evidence | yes | HS3 recovery/ack |
     * | M3 | Ready/Terminal | live | RW Copy | none | HM1 live reconciliation |
     *
     * Observation and effect-free reconstruction are provider evidence reads,
     * not source effects; the exact event list therefore contains only C1. */
    let thread = "unavailable-auxiliary-writable-memory";
    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        events: events.clone(),
        scenario: TerminalRecoveryScenario::UnavailableAuxiliaryMemory,
        fence_refresh_probe: None,
    });
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub").with_session_container_provider(
            provider,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
        ),
    );
    let managed = managed_with_resource_source(host.clone());

    let mut projection = terminal_memory_projection();
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        thread,
        projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("M0 install frozen writable Memory input");

    let handle = terminal_fixture_container_handle(thread, &host.sandbox_spec(thread))
        .with_memory_materializations(vec![terminal_memory_evidence()])
        .expect("M0 durable handle carries exact writable Copy evidence");
    let binding = serde_json::to_string(&handle).expect("M0 encode durable binding");
    projection.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding,
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    let (lease, effect) = terminal_recovery_effect(thread, "unavailable-memory-worker");
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            lease,
        },
    )
    .await
    .expect("M0 install exact terminal assignment");

    let preparation_authorization = terminal_preparation_authorization(&effect, &projection);
    let error = managed
        .prepare_terminal_cleanup_for_effect(effect, preparation_authorization)
        .await
        .expect_err("M0 unavailable writable Memory must fail before source effects");
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::Unavailable,
        "M0/E2",
    );
    assert_eq!(error.code, "session_terminal_memory_source_absent", "M0/E2");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            "observe-unavailable-memory",
            "reconstruct-unavailable-memory"
        ],
        "M0/E2 publish/ack/checkpoint/provider-prep/dispose are all zero",
    );
    assert!(
        host.session_slots
            .read(thread, |slot| {
                slot.environment_owner
                    .terminal_bound_environment()
                    .is_some()
            })
            .unwrap_or(false),
        "M0/E3 exact auxiliary owner remains Retiring for retry/takeover"
    );
}

#[tokio::test]
async fn legacy_missing_memory_evidence_fails_before_terminal_effects() {
    /* Host legacy-Memory table HM0b. Causes: C1 frozen inputs contain RW
     * MemoryStore; C2 the aggregate has no Environment binding and therefore
     * no durable handle; C3 `None` means legacy/unknown, unlike current
     * `Some([])` WTR/FUSE evidence. Effects: E1 Host delegates C1+C2 unchanged
     * to the one Option-aware contract join; E2 it returns the typed ambiguous
     * evidence failure before Artifact recovery or any provider boundary; E3
     * the exact terminal projection remains installed. Rule HM0b
     * C1+C2+C3=>E1->E2+E3. `Some([])+RW` and exact RO/RW partitions are owned
     * by the shared join's adjacent decision table and HM0/HM1 exercise its
     * Host effect ordering. */
    let thread = "legacy-missing-terminal-memory-evidence";
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.artifact_publisher = Arc::new(RecordingScopedArtifactAuthority {
        events: events.clone(),
        records: Arc::new(Mutex::new(Vec::new())),
    });
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());

    let projection = terminal_memory_projection();
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        thread,
        projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("HM0b install frozen writable Memory input");
    let (lease, effect) = terminal_recovery_effect(thread, "legacy-memory-worker");
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: thread.into(),
            projection: projection.clone(),
            lease,
        },
    )
    .await
    .expect("HM0b install exact terminal assignment");

    let preparation_authorization = terminal_preparation_authorization(&effect, &projection);
    let error = managed
        .prepare_terminal_cleanup_for_effect(effect, preparation_authorization)
        .await
        .expect_err("HM0b missing handle evidence must remain ambiguous");
    assert_eq!(
        error.kind,
        awaken_session_contract::RunErrorKind::Unavailable,
        "HM0b/E2",
    );
    assert_eq!(
        error.code, "session_terminal_memory_evidence_unreconciled",
        "HM0b/E2",
    );
    assert!(
        events.lock().unwrap().is_empty(),
        "HM0b/E2 zero Artifact effects"
    );
    assert!(host.session_slots.contains(thread), "HM0b/E3");
}

#[tokio::test]
async fn resident_and_cold_terminal_memory_use_one_terminal_v2_authority() {
    /* Host terminal Memory table HM1. Provider tests own fresh Copy-guard
     * creation and exact acknowledgement semantics; this table owns the Host
     * caller shared by a resident same-process Environment and a cold
     * reconstruction after Worker loss. Causes: C1 the V2 binding carries one
     * complete writable Copy evidence M; C2 the frozen aggregate input exactly
     * joins M; C3 the Environment is resident/cold; C4 the Host is an upstream
     * Worker whose frozen input was materialized under an exact Run claim and
     * whose resulting RunV1 reference would be expired at terminal time.
     * Effects: E1 one live Artifact batch precedes Memory; E2 the Sandbox copy
     * is read once; E3 only SessionTerminalMemoryIntent is encoded and
     * reconciled; E4 the exact complete evidence is acknowledged after E3 and
     * provider preparation returns without deletion; E5 only a separately
     * aggregate-derived authorization physically disposes. A Disposing retry instead uses HS3's
     * ReceiptOnly/zero-live-I/O row, while foreign evidence and takeover are
     * owned by NM2 and the shared acknowledgement kernel.
     *
     * | Rule | M/input | owner | encoded reference | Effect |
     * | H1 | exact RW | resident | terminal-v2 only | E1->E2->E3->E4->E5 |
     * | H2 | exact RW | cold | terminal-v2 only | prepare->E1->E2->E3->E4->E5 |
     * | H3 | missing/foreign | any | none | fail closed (contract tests) |
     * | H4 | Disposing | cleanup-only | none | HS3 | */
    #[derive(Clone, Copy)]
    struct Rule {
        name: &'static str,
        resident: bool,
    }

    for rule in [
        Rule {
            name: "resident",
            resident: true,
        },
        Rule {
            name: "cold",
            resident: false,
        },
    ] {
        let thread = format!("terminal-memory-{}", rule.name);
        let events = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(TerminalRecoveryProvider {
            events: events.clone(),
            scenario: TerminalRecoveryScenario::LiveMemory,
            fence_refresh_probe: None,
        });
        let raw_host = SharedHost::new(Arc::new(OkModel), "stub")
            .with_worker_upstream(awaken_worker_transport_security::WorkerUpstream::new(
                "http://coordinator.invalid",
            ))
            .with_memory_reference_encoder(Arc::new(RecordingTerminalMemoryReferenceEncoder {
                events: events.clone(),
            }))
            .with_session_container_provider(
                provider,
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            );
        raw_host.install_memory_mounter(Arc::new(RecordingTerminalMemoryMounter {
            events: events.clone(),
        }));
        let host = Arc::new(raw_host);
        let managed = managed_with_resource_source(host.clone());

        let mut projection = terminal_memory_projection();
        let resources = projection.resources.clone();
        let run_claim = awaken_run_ingress::RunClaim {
            run_id: awaken_agent_contract::agent::run::Id(format!(
                "terminal-memory-{}-run",
                rule.name
            )),
            owner: "terminal-memory-run-worker".into(),
            epoch: 1,
        };
        host.install_frozen_session_projection(
            &thread,
            projection.clone(),
            Some(&run_claim),
            true,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{} installs frozen Memory input: {error}", rule.name));

        let evidence = terminal_memory_evidence();
        let handle = terminal_fixture_container_handle(&thread, &host.sandbox_spec(&thread))
            .with_memory_materializations(vec![evidence])
            .expect("HM1 current V2 handle carries exact Memory evidence");
        let binding = serde_json::to_string(&handle).expect("HM1 encode terminal binding");
        projection.environment = awaken_session_contract::SessionEnvironmentState::Resident {
            binding: binding.clone(),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        let (lease, effect) = terminal_recovery_effect(&thread, "terminal-memory-worker");
        host.install_terminal_cleanup_projection(
            &awaken_session_contract::SessionTerminalCleanupAssignment {
                session_id: thread.clone(),
                projection: projection.clone(),
                lease: lease.clone(),
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{} installs terminal assignment: {error}", rule.name));

        if rule.resident {
            let warm_fence = lease
                .sandbox_effect_fence("resident-terminal-memory-preparation")
                .expect("HM1 live resident preparation fence");
            host.prepare_bound_environment_for_effect_under_lifecycle(
                &thread,
                &binding,
                &warm_fence,
                Some(&resources),
                crate::host::BoundEnvironmentPreparationMode::LiveSource,
            )
            .await
            .unwrap_or_else(|error| panic!("{} prepares resident owner: {error}", rule.name));
            assert!(
                host.session_slots
                    .read(&thread, |slot| {
                        slot.environment_owner
                            .terminal_bound_environment()
                            .is_some()
                    })
                    .unwrap_or(false),
                "H1/C3 exact same-process owner is retained behind Preparation"
            );
        } else {
            assert!(host.session_environment(&thread).await.is_none(), "H2/C3");
        }
        events.lock().unwrap().clear();

        let preparation_authorization = terminal_preparation_authorization(&effect, &projection);
        let preparation = managed
            .prepare_terminal_cleanup_for_effect(effect.clone(), preparation_authorization)
            .await
            .unwrap_or_else(|error| panic!("{} terminal Memory cleanup: {error}", rule.name));
        assert!(preparation.artifact_receipts.is_empty(), "HM1/E1");
        let preparation_events = if rule.resident {
            vec![
                "observe-live-memory",
                "capture-artifacts",
                "read-terminal-memory",
                "encode-terminal-v2",
                "reconcile-terminal-v2",
                "ack-memory",
                "prepare-source",
            ]
        } else {
            vec![
                "observe-live-memory",
                "prepare-live-memory",
                "capture-artifacts",
                "read-terminal-memory",
                "encode-terminal-v2",
                "reconcile-terminal-v2",
                "ack-memory",
                "prepare-source",
            ]
        };
        assert_eq!(
            events.lock().unwrap().as_slice(),
            preparation_events,
            "HM1 {} preparation",
            rule.name,
        );
        assert!(
            !events.lock().unwrap().contains(&"dispose-physical"),
            "HM1/E5 aggregate preparation must precede physical disposal",
        );
        assert!(
            !events.lock().unwrap().contains(&"encode-run-v1"),
            "HM1/E3 old RunV1 reference is never reused"
        );
        assert!(
            host.session_slots
                .read(&thread, |slot| {
                    slot.environment_owner
                        .terminal_bound_environment()
                        .is_some()
                })
                .unwrap_or(false),
            "HM1/E4 exact owner remains Retiring until Disposal"
        );

        let disposal_effect = terminal_disposal_effect(preparation, lease, &projection);
        let disposal = managed
            .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
            .await
            .unwrap_or_else(|error| panic!("{} terminal physical disposal: {error}", rule.name));
        assert_eq!(disposal.effect_id, disposal_effect.command.effect_id);
        assert_eq!(
            events.lock().unwrap().last(),
            Some(&"dispose-physical"),
            "HM1/E5 physical disposal is the final effect",
        );
        assert!(host.session_environment(&thread).await.is_none(), "HM1/E5");
    }
}

/// Terminal cleanup prepares every durability participant before the aggregate
/// opens the one physical-disposal edge. The exact sandbox is reaped only by
/// `SessionRuntime::dispose_terminal_cleanup_for_effect`: preparation leaves
/// its status `Ready`, while disposal flips it to `Terminated`. The process-local
/// terminal projection remains until the aggregate acknowledges the disposal
/// receipt, unlike evict-to-rebuild edges that retain the physical workspace.
#[tokio::test]
async fn exact_terminal_cleanup_disposes_the_threads_sandbox() {
    use awaken_provisioning_contract::SandboxStatus;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);

    let mut terminal_projection = remote_terminal_cleanup_projection();
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        "t-end",
        terminal_projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("install the complete terminal-test projection before realization");
    // Stage representative resource/config projections before realizing the
    // exact durable V2 Sandbox. Terminal cleanup must erase all of them so
    // reusing the opaque thread id cannot inherit stale scope or model state.
    host.register_thread_memory("t-end", None);
    host.register_thread_resources("t-end", crate::provisioning::StagedResources::default());
    host.register_thread_model("t-end", "private-model");
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "terminal-test-worker".into(),
        runtime_incarnation: "terminal-test-worker:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    let create_fence = lease
        .sandbox_effect_fence("terminal-test-create")
        .expect("project the exact create fence");
    let physical = host
        .provider
        .create_sandbox_for_effect(&host.sandbox_spec("t-end"), &create_fence, None)
        .await
        .expect("create the exact durable terminal-test Sandbox");
    let env = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        physical,
    ));
    let binding = serde_json::to_string(&env.handle()).expect("encode terminal binding");
    let environment_generation = awaken_session_contract::SandboxGeneration::new(
        "t-end",
        lease.epoch,
        lease.expires_at_unix_ms,
        "terminal-test-environment",
        "terminal-test-image",
    );
    let environment_effect_id = create_fence.operation_id.clone();
    let environment_state = awaken_session_contract::SessionEnvironmentState::Resident {
        binding: binding.clone(),
        effect_id: Some(environment_effect_id.clone()),
        generation: Some(environment_generation.clone()),
        idle_since_unix_ms: None,
    };
    let candidate = host
        .begin_session_environment_preparation("t-end", env.clone())
        .expect("retain the exact terminal-test candidate");
    host.install_session_environment_owner_projection(
        "t-end",
        "terminal-workspace",
        &environment_state,
    )
    .expect("project the terminal-test durable Environment identity");
    host.publish_prepared_session_environment(
        "t-end",
        &candidate,
        crate::session_slot::BoundSessionEnvironmentIdentity::Durable {
            effect_id: environment_effect_id,
            generation: environment_generation,
        },
    )
    .expect("publish the exact terminal-test Environment owner");
    host.ctx_for("t-end", None)
        .await
        .expect("cache the Runtime around the exact fenced Environment");
    assert_eq!(
        env.status().await.expect("status"),
        SandboxStatus::Ready,
        "the fenced sandbox workspace exists while the Session is live"
    );

    // Cause/effect graph: C1 the application-owned operation freezes one exact
    // root preparation and realization generation; C2 archive and recovery replay
    // that preparation concurrently; C3 the aggregate has not durably admitted
    // preparation; C4 durable preparation projects one typed disposal; C5 physical
    // disposal succeeds but aggregate acknowledgement is absent; C6 the exact
    // acknowledgement arrives; C7 an effect has no installed generation. Effects:
    // E1 preparation is idempotent and leaves the substrate live; E2 no physical
    // mutation crosses C3; E3 exact disposal replay returns one receipt and reaps
    // the substrate; E4 the frozen slot/lease remains retryable; E5 acknowledgement
    // alone retires every local projection; E6 an unfenced/retired effect fails
    // before provider I/O. Constraint: preparation/disposal receipts live only in
    // the aggregate; Host has no combined completion registry or alternate fence.
    // Decision table:
    // | Rule | prep exact/replay | prep durable | disposal exact/replay | ack | stale | Effect |
    // | T1 | T | F | F | F | F | E1 + E2 |
    // | T2 | T | T | T | F | F | E3 + E4 |
    // | T3 | T | T | T | T | F | E5 |
    // | T4 | F | F | F | F | T | E6 |
    let mut cleanup = awaken_session_contract::SessionCleanupOperation::default();
    assert!(cleanup.request("t-end"), "T1 freezes the terminal fence");
    cleanup
        .freeze_targets("t-end", [], 0, 0)
        .expect("T1 freezes the root target");
    let command = cleanup
        .command_for("t-end", "t-end")
        .expect("T1 exact root command");
    terminal_projection.environment = environment_state;
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: "t-end".into(),
            projection: terminal_projection.clone(),
            lease: lease.clone(),
        },
    )
    .await
    .expect("install the exact terminal projection");
    let effect =
        awaken_session_contract::SessionTerminalCleanupEffect::new(command.clone(), lease.clone());
    let preparation_authorization =
        terminal_preparation_authorization(&effect, &terminal_projection);
    let (archive, recovery) = tokio::join!(
        managed.prepare_terminal_cleanup_for_effect(
            effect.clone(),
            preparation_authorization.clone(),
        ),
        managed.prepare_terminal_cleanup_for_effect(
            effect.clone(),
            preparation_authorization.clone(),
        )
    );
    let archive = archive.expect("archive terminal preparation");
    let recovery = recovery.expect("recovery terminal preparation replay");
    assert_eq!(archive, recovery, "T1/E1 exact response-loss replay");
    assert_eq!(
        env.status().await.expect("status after preparation"),
        SandboxStatus::Ready,
        "T1/E2 preparation cannot reap the workspace",
    );
    assert!(
        host.session_slots
            .read("t-end", |slot| {
                slot.environment_owner
                    .terminal_bound_environment()
                    .is_some()
            })
            .unwrap_or(false),
        "T1/E2 exact owner is hidden from tools but retained for Disposal"
    );
    managed
        .acknowledge_terminal_cleanup_preparation(&effect)
        .await;
    assert!(
        host.session_slots.contains("t-end"),
        "T1 root preparation ack is non-retiring"
    );

    let disposal_effect = terminal_disposal_effect(archive, lease, &terminal_projection);
    let (archive, recovery) = tokio::join!(
        managed.dispose_terminal_cleanup_for_effect(disposal_effect.clone()),
        managed.dispose_terminal_cleanup_for_effect(disposal_effect.clone())
    );
    let archive = archive.expect("archive terminal disposal");
    let recovery = recovery.expect("recovery terminal disposal replay");
    assert_eq!(archive, recovery, "T2/E3 exact response-loss replay");

    // The cached ctx is evicted ...
    assert!(
        !host
            .session_slots
            .read("t-end", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "terminal cleanup evicts the cached ctx"
    );
    assert!(
        host.session_environment("t-end").await.is_none(),
        "terminal end removes the independent environment owner"
    );
    // ... and the sandbox is ACTUALLY disposed: its workspace dir was reaped, so a
    // subsequent status reports Terminated (proving dispose ran, not just an evict).
    assert_eq!(
        env.status().await.expect("status"),
        SandboxStatus::Terminated,
        "terminal cleanup disposes the sandbox (workspace reaped), unlike an evict-rebuild"
    );
    assert!(
        host.session_slots.contains("t-end"),
        "T2/E4 retains the exact terminal projection until durable acknowledgement"
    );
    managed
        .acknowledge_terminal_cleanup_disposal(&disposal_effect)
        .await;
    assert!(host.registered_thread_workspace("t-end").is_none(), "T3/E5");
    assert!(!host.session_slots.contains("t-end"), "T3/E5");
    assert!(
        host.inference_routing.override_for("t-end").is_none(),
        "T3/E5"
    );

    let replay_error = managed
        .prepare_terminal_cleanup_for_effect(effect, preparation_authorization)
        .await
        .expect_err("T4 retired generation fails closed");
    assert_eq!(
        replay_error.kind,
        awaken_session_contract::RunErrorKind::Unavailable
    );
    let mut missing = awaken_session_contract::SessionCleanupOperation::default();
    assert!(missing.request("never-existed"));
    missing.freeze_targets("never-existed", [], 0, 0).unwrap();
    let missing_effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        missing
            .command_for("never-existed", "never-existed")
            .unwrap(),
        awaken_session_contract::SessionRealizationLease {
            owner: "terminal-test-worker".into(),
            runtime_incarnation: "terminal-test-worker:incarnation".into(),
            epoch: 1,
            expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
                + 60_000,
        },
    );
    let missing_authorization =
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
            missing_effect.clone(),
            "terminal-workspace".into(),
            None,
        )
        .expect("T4 close the foreign Host preparation input");
    let missing_error = managed
        .prepare_terminal_cleanup_for_effect(missing_effect, missing_authorization)
        .await
        .expect_err("T4 unknown generation fails closed");
    assert_eq!(
        missing_error.kind,
        awaken_session_contract::RunErrorKind::Unavailable
    );
}

#[tokio::test]
async fn restoring_terminal_disposal_replays_until_aggregate_acknowledgement() {
    // Terminal restore-target table TR1. Causes: C1 the frozen aggregate is
    // Restoring and names exact unpublished target R; C2 Preparation has
    // deleted the committed checkpoint and is durably admitted; C3 Disposal
    // physically removes R but its receipt is lost; C4 the same Disposal is
    // replayed; C5 aggregate acknowledgement arrives. Effects: E1 Preparation
    // performs no restore/ordinary Environment observation and retains the
    // exact hidden Restoring request; E2 both Disposal calls use the provider's
    // idempotent exact-target port; E3 response loss retains the same request
    // fence; E4 acknowledgement alone retires the terminal projection. Rules:
    // TR1=C1+C2=>E1; TR2=TR1+C3+C4=>E2+E3; TR3=TR2+C5=>E4. There is no
    // Disposal-local Vacant completion or second receipt registry.
    let thread = "restoring-terminal-response-loss";
    let (lease, base_effect) = terminal_recovery_effect(thread, "restoring-terminal-worker");
    let generation = awaken_session_contract::SandboxGeneration::new(
        thread,
        lease.epoch,
        lease.expires_at_unix_ms,
        "restoring-terminal-environment",
        "restoring-terminal-image",
    );
    let suspend = awaken_session_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        thread,
        "suspend",
        &generation,
        6,
        Some(lease.clone()),
        None,
    );
    let checkpoint = awaken_session_contract::SandboxCheckpointRef {
        id: "restoring-terminal-checkpoint".into(),
        format: "awaken-fs-v1".into(),
        digest: "restoring-terminal-checkpoint-digest".into(),
        size_bytes: 1,
        created_at_unix_ms: 1,
        expires_at_unix_ms: u64::MAX,
        environment_fingerprint: generation.environment_fingerprint.clone(),
        base_image_fingerprint: generation.base_image_fingerprint.clone(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: suspend.effect_id,
    };
    let restore = awaken_session_contract::SessionEnvironmentOperation::new(
        "terminal-workspace",
        thread,
        "restore",
        &generation,
        7,
        Some(lease.clone()),
        Some(&checkpoint),
    );
    let restoring = awaken_session_contract::SessionEnvironmentState::Restoring {
        operation: restore,
        checkpoint: checkpoint.clone(),
        generation,
    };
    let restore_target = restoring
        .restoring_request("terminal-workspace", thread)
        .expect("TR1 exact aggregate restore target");
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
        base_effect
            .command
            .with_restore_target(restore_target.clone())
            .expect("TR1 root command owns R"),
        lease.clone(),
    );

    let events = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(TerminalRecoveryProvider {
        events: events.clone(),
        scenario: TerminalRecoveryScenario::Restoring,
        fence_refresh_probe: None,
    });
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_session_container_provider(
                provider,
                Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            )
            .with_environment_checkpoint_store(Arc::new(RecordingTerminalCheckpointStore {
                events: events.clone(),
                expected_id: checkpoint.id,
            })),
    );
    let managed = install_test_dispatch_runtime(&host);
    install_test_session_application(&host);
    let projection =
        install_terminal_recovery_projection(&host, &managed, thread, lease.clone(), restoring)
            .await;

    let preparation = managed
        .prepare_terminal_cleanup_for_effect(
            effect.clone(),
            terminal_preparation_authorization(&effect, &projection),
        )
        .await
        .expect("TR1 prepare the exact restoring target");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["delete-checkpoint"],
        "TR1/E1"
    );
    assert!(matches!(
        host.session_slots
            .read(thread, |slot| slot.environment_owner.clone()),
        Some(crate::session_slot::SessionEnvironmentOwner::Restoring(
            crate::session_slot::SessionEnvironmentRestoration::Awaiting { request }
        )) if request == restore_target
    ));

    let disposal_effect = terminal_disposal_effect(preparation, lease, &projection);
    let first = managed
        .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
        .await
        .expect("TR2 dispose exact restore target");
    let replay = managed
        .dispose_terminal_cleanup_for_effect(disposal_effect.clone())
        .await
        .expect("TR2 replay after disposal receipt loss");
    assert_eq!(first, replay, "TR2/E2 deterministic receipt");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &["delete-checkpoint", "dispose-restored", "dispose-restored"],
        "TR2/E2 exact provider port handles absence replay"
    );
    assert!(
        matches!(
            host.session_slots
                .read(thread, |slot| slot.environment_owner.clone()),
            Some(crate::session_slot::SessionEnvironmentOwner::Restoring(
                crate::session_slot::SessionEnvironmentRestoration::Awaiting { request }
            )) if request == restore_target
        ),
        "TR2/E3 response-loss fence"
    );

    managed
        .acknowledge_terminal_cleanup_disposal(&disposal_effect)
        .await;
    assert!(!host.session_slots.contains(thread), "TR3/E4");
}

#[tokio::test]
async fn terminal_projection_retirement_requires_generation_tag_and_durable_absence() {
    // Cause/effect graph: C1 root lease generation is exact; C2 child c1 has
    // one durably accepted exact preparation; C3 child c2 remains pending; C4
    // the same opaque child id has a foreign
    // replacement tag; C5 a child/root preparation is acknowledged; C6 Control
    // reports aggregate completion. Effects: E1 retire c1 only; E2 retain root+c2;
    // E3 never remove the foreign replacement; E4 root preparation acknowledgement
    // is non-retiring; E5 completed readback retires the exact tree children-first.
    // Constraint: this edge forgets process-local projection only and performs
    // no provider, Resource, or aggregate mutation.
    // Decision table:
    // | Rule | exact gen | root tag | prep target | complete | Effect |
    // | P1 | T | T | exact child | F | E1 |
    // | P2 | T | T | another child | F | E2 |
    // | P3 | T | F | child | F | E3 |
    // | P4 | T | T | root | F | E4 |
    // | P5 | T | T | none | T | E5 |
    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let session_id = "terminal-retire-root";
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "terminal-retire-worker".into(),
        runtime_incarnation: "terminal-retire-worker:incarnation".into(),
        epoch: 7,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection: remote_terminal_cleanup_projection(),
            lease: lease.clone(),
        },
    )
    .await
    .expect("P1-P4 install the aggregate-derived root projection");
    for child in ["terminal-retire-done", "terminal-retire-pending"] {
        host.register_thread_workspace(child, "terminal-retire-workspace");
        host.session_slots.update(child, |slot| {
            slot.terminal_cleanup_root = Some(session_id.into());
        });
    }
    let foreign = "terminal-retire-foreign";
    host.register_thread_workspace(foreign, "foreign-workspace");
    host.session_slots.update(foreign, |slot| {
        slot.terminal_cleanup_root = Some("replacement-root".into());
    });

    let mut cleanup = awaken_session_contract::SessionCleanupOperation::default();
    assert!(cleanup.request(session_id));
    cleanup
        .freeze_targets(
            session_id,
            [
                "terminal-retire-done".to_string(),
                "terminal-retire-pending".to_string(),
                foreign.to_string(),
            ],
            0,
            0,
        )
        .unwrap();
    let completed_child = awaken_session_contract::SessionTerminalCleanupEffect::new(
        cleanup
            .command_for(session_id, "terminal-retire-done")
            .unwrap(),
        lease.clone(),
    );
    host.acknowledge_terminal_cleanup_preparation(&completed_child)
        .await;
    assert!(
        !host.session_slots.contains("terminal-retire-done"),
        "P1/E1"
    );
    assert!(host.session_slots.contains(session_id), "P2/E2");
    assert!(
        host.session_slots.contains("terminal-retire-pending"),
        "P2/E2"
    );
    assert!(host.session_slots.contains(foreign), "P3/E3");

    let stale_foreign = awaken_session_contract::SessionTerminalCleanupEffect::new(
        cleanup.command_for(session_id, foreign).unwrap(),
        lease.clone(),
    );
    host.acknowledge_terminal_cleanup_preparation(&stale_foreign)
        .await;
    assert!(host.session_slots.contains(foreign), "P3/E3");

    let root = awaken_session_contract::SessionTerminalCleanupEffect::new(
        cleanup.command_for(session_id, session_id).unwrap(),
        lease,
    );
    host.acknowledge_terminal_cleanup_preparation(&root).await;
    assert!(host.session_slots.contains(session_id), "P4/E4");
    host.acknowledge_completed_terminal_cleanup(session_id, &root.lease)
        .await;
    assert!(!host.session_slots.contains(session_id), "P5/E5");
    assert!(
        !host.session_slots.contains("terminal-retire-pending"),
        "P5/E5"
    );
    assert!(host.session_slots.contains(foreign), "P4/E3");
}

#[tokio::test]
async fn terminal_quiescence_never_materializes_a_cold_environment() {
    let host = SharedHost::new(Arc::new(OkModel), "stub");

    let snapshot = host
        .quiesce_terminal_delegations("cold-terminal")
        .await
        .expect("cold terminal fence reads committed truth");

    assert!(snapshot.delegated_runs.is_empty());
    assert_eq!(snapshot.watermark, 0);
    assert!(
        host.session_slots
            .read("cold-terminal", |slot| {
                slot.runtime.is_none() && !slot.environment_owner.has_local_environment()
            })
            .unwrap_or(true),
        "terminal control may allocate a lock slot but not a Runtime or Environment projection"
    );
}

async fn settle_cancelled_dispatch_rows(
    dispatch: Arc<awaken_run_ingress::AnyDispatchStore>,
    expected_rows: usize,
    worker: &'static str,
) -> Vec<RunId> {
    use awaken_run_ingress::{DispatchOutcome, DispatchQueue as _};

    loop {
        let rows = dispatch.list_dispatches().await.unwrap();
        if rows.len() == expected_rows && rows.iter().all(|row| row.cancellation_requested) {
            let mut settled = Vec::with_capacity(rows.len());
            for row in rows {
                let claimed = dispatch
                    .claim_run(&row.run_id, worker, 30_000, 1, &Default::default())
                    .await
                    .unwrap()
                    .expect("cancelled dispatch remains claimable for exact settlement");
                dispatch
                    .settle(&row.run_id, claimed.lease.epoch, DispatchOutcome::Done, &[])
                    .await
                    .unwrap();
                settled.push(row.run_id);
            }
            return settled;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn terminal_quiescence_fences_ephemeral_children_before_cold_link_enrichment() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the Session has no resident Runtime or recoverable
    // current publication; C2 an ephemeral runtime authority owns a parent-affined
    // child row; C3 deployment.durable is false; C4 terminal quiescence starts.
    // C5 the child Worker observes the cancellation bit and settles its exact
    // epoch. E1 the authority-backed queue cancellation fence precedes settlement;
    // E2 the logical child Thread is frozen into cleanup after its row disappears;
    // E3 no Environment is built; E4 quiescence returns only after settlement.
    // Decision table: R1(C1+C2+C3+C4+C5)->E1+E2+E3+E4. The durable store case is
    // exercised by the terminal cleanup integration tests, while the sibling
    // cold-empty test owns !C2. This rule prevents persistence mode from
    // masquerading as dispatch authority ownership.
    let host = SharedHost::new(Arc::new(OkModel), "stub");
    assert!(!host.deployment.durable, "R1/C3");
    let dispatch = host
        .dispatch_store()
        .expect("R1 ephemeral dispatch authority");
    let parent = ThreadId("cold-terminal-parent".into());
    let child = ThreadId("cold-terminal-child".into());
    let child_run = RunId("cold-terminal-child-run".into());
    dispatch
        .enqueue(
            awaken_run_ingress::RunDispatch::new(
                crate::host::worker_resolver::test_support::test_activation(&child.0, &child_run.0),
            )
            .for_session(parent.clone()),
        )
        .await
        .expect("R1 durable child admission");
    let worker = tokio::spawn(settle_cancelled_dispatch_rows(
        dispatch.clone(),
        1,
        "ephemeral-terminal-worker",
    ));
    let snapshot = host
        .quiesce_terminal_delegations(&parent.0)
        .await
        .expect("R1 terminal fence waits for exact child settlement");
    let settled = worker.await.unwrap();

    assert_eq!(snapshot.coordinated_thread_ids, vec![child], "R1/E2");
    assert_eq!(settled, vec![child_run], "R1/E1+E4");
    let rows = dispatch.list_dispatches().await.expect("R1 inspect fence");
    assert!(rows.is_empty(), "R1/E4");
    assert!(
        host.session_slots
            .read(&parent.0, |slot| slot.runtime.is_none()
                && !slot.environment_owner.has_local_environment())
            .unwrap_or(true),
        "R1/E3"
    );
}

#[tokio::test]
async fn coordinator_terminal_quiescence_waits_for_remote_parent_and_child_settlement() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 Coordinator owns the durable queue but no local
    // pool; C2 root and parent-affined child are active; C3 cancellation intent
    // is durable but the remote Worker has not settled it; C4 the remote Worker
    // settles both exact epochs. Effects: E1 quiescence does not freeze early;
    // E2 both rows are cancelled; E3 the child identity remains in the frozen
    // snapshot after its queue row disappears; E4 no Environment is built.
    //
    // | Rule | topology | cancellation | settlement | Effect |
    // | Q1 | coordinator-only | requested | pending | E1 + E2 |
    // | Q2 | coordinator-only | requested | exact Done | E3 + E4 |
    use awaken_run_ingress::{DispatchQueue, RunDispatch};

    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("remote quiescence dispatch"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_coordinator_dispatch_store(dispatch.clone()),
    );
    let parent = ThreadId("remote-quiescence-parent".into());
    let child = ThreadId("remote-quiescence-child".into());
    for (thread, run) in [
        (&parent, RunId("remote-quiescence-root-run".into())),
        (&child, RunId("remote-quiescence-child-run".into())),
    ] {
        dispatch
            .enqueue(
                RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                    &thread.0, &run.0,
                ))
                .for_session(parent.clone())
                .with_session_activity_epoch(1),
            )
            .await
            .expect("Q1 active dispatch");
    }
    let remote = tokio::spawn(settle_cancelled_dispatch_rows(
        dispatch.clone(),
        2,
        "remote-worker",
    ));

    let snapshot = host
        .quiesce_terminal_delegations(&parent.0)
        .await
        .expect("Q2 remote settlement proves quiescence");
    assert_eq!(remote.await.unwrap().len(), 2, "Q1/E1");
    assert_eq!(snapshot.coordinated_thread_ids, vec![child], "Q2/E3");
    assert!(
        dispatch.list_dispatches().await.unwrap().is_empty(),
        "Q2/E2"
    );
    assert!(host.session_environment(&parent.0).await.is_none(), "Q2/E4");
}

#[derive(Default)]
struct RemoteTerminalCleanupControl {
    assignments: Mutex<
        std::collections::VecDeque<awaken_session_contract::SessionTerminalCleanupAssignment>,
    >,
    session: Mutex<Option<awaken_session_contract::PersistedSession>>,
    preparations: Mutex<Vec<awaken_session_contract::SessionCleanupPreparation>>,
    disposals: Mutex<Vec<awaken_session_contract::SessionCleanupDisposalReceipt>>,
    publication_receipts: Mutex<Vec<awaken_session_contract::SessionRepositoryPublicationReceipt>>,
    publication_rejections:
        Mutex<Vec<awaken_session_contract::SessionRepositoryPublicationRejection>>,
    events: Mutex<Vec<String>>,
    disposal_response_loss_once: AtomicBool,
    authorization_barrier: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    renewal_failure: Mutex<Option<awaken_session_contract::SessionRealizationControlFailure>>,
    claim_targets: Mutex<Vec<awaken_session_contract::SessionRealizationTarget>>,
    renewal_sessions: Mutex<Vec<String>>,
    renewal_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    renewals_active: AtomicUsize,
    max_renewals_active: AtomicUsize,
    renewals: Mutex<BTreeMap<String, awaken_session_contract::SessionRealizationLease>>,
    authority: Mutex<
        Option<(
            awaken_session_contract::FrozenSessionProjection,
            awaken_session_contract::SessionRealizationLease,
        )>,
    >,
}

struct ActiveRenewalGuard<'a>(&'a AtomicUsize);

impl Drop for ActiveRenewalGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for RemoteTerminalCleanupControl {
    async fn begin_session_realization(
        &self,
        command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let _ = command;
        Err(self
            .renewal_failure
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(awaken_session_contract::SessionRealizationControlFailure::NotReady))
    }

    async fn renew_session_realization(
        &self,
        command: awaken_session_contract::RenewSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationLease,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        use std::sync::atomic::Ordering;

        let active = self.renewals_active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active = ActiveRenewalGuard(&self.renewals_active);
        self.max_renewals_active.fetch_max(active, Ordering::SeqCst);
        self.renewal_sessions
            .lock()
            .unwrap()
            .push(command.session_id.clone());
        let gate = self.renewal_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire_owned()
                .await
                .expect("renewal gate remains open")
                .forget();
        }
        if let Some(failure) = self.renewal_failure.lock().unwrap().clone() {
            return Err(failure);
        }
        if let Some(current) = self.renewals.lock().unwrap().get_mut(&command.session_id) {
            if !awaken_session_contract::realization_lease_generation_authorizes(
                current,
                &command.asserted_lease,
            ) || command.requested_expires_at_unix_ms < current.expires_at_unix_ms
            {
                return Err(
                    awaken_session_contract::SessionRealizationControlFailure::StaleOwnership,
                );
            }
            current.expires_at_unix_ms = command.requested_expires_at_unix_ms;
            return Ok(current.clone());
        }

        let (projection, current) = self
            .authority
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if !awaken_session_contract::realization_lease_generation_authorizes(
            &current,
            &command.asserted_lease,
        ) || command.requested_expires_at_unix_ms < current.expires_at_unix_ms
        {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let mut renewed = current;
        renewed.expires_at_unix_ms = command.requested_expires_at_unix_ms;
        let mut session = self.session.lock().unwrap();
        let session = session
            .as_mut()
            .filter(|session| session.session_id == command.session_id)
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if !session.realization.as_ref().is_some_and(|lease| {
            awaken_session_contract::realization_lease_generation_authorizes(
                lease,
                &command.asserted_lease,
            )
        }) {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        session.realization = Some(renewed.clone());
        *self.authority.lock().unwrap() = Some((projection, renewed.clone()));
        Ok(renewed)
    }

    async fn activate_session_realization(
        &self,
        _command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn acknowledge_session_realization(
        &self,
        _command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
    }

    async fn claim_next_terminal_cleanup(
        &self,
        target: awaken_session_contract::SessionRealizationTarget,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupAssignment>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.claim_targets.lock().unwrap().push(target);
        let assignment = self.assignments.lock().unwrap().pop_front();
        if let Some(assignment) = &assignment {
            *self.authority.lock().unwrap() =
                Some((assignment.projection.clone(), assignment.lease.clone()));
        }
        Ok(assignment)
    }

    async fn authorize_terminal_cleanup_effect(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) -> Result<
        awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let authority = self
            .authority
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if !awaken_session_contract::realization_lease_generation_authorizes(
            &authority.1,
            &effect.lease,
        ) {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let inherited_provider_disposal = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?
            .authorize_terminal_cleanup_effect(effect)
            .map_err(|error| {
                awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })?;
        let authorization =
            awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
                effect.clone(),
                authority.0.workspace_id,
                inherited_provider_disposal,
            )?;
        let barrier = self.authorization_barrier.lock().unwrap().clone();
        if let Some((started, proceed)) = barrier {
            started.notify_one();
            proceed.notified().await;
        }
        Ok(authorization)
    }

    async fn authorize_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<String, awaken_session_contract::SessionRealizationControlFailure> {
        let authority = self
            .authority
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        let exact = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|session| {
                session
                    .authorize_terminal_cleanup_disposal_effect(&authority.0.workspace_id, effect)
                    .is_ok()
            });
        if !awaken_session_contract::realization_lease_generation_authorizes(
            &authority.1,
            &effect.lease,
        ) || !exact
        {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        Ok(authority.0.workspace_id)
    }

    async fn terminal_cleanup_work(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionTerminalCleanupWork>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.events.lock().unwrap().push("cleanup:poll".into());
        if self.renewals.lock().unwrap().contains_key(session_id) {
            return Ok(None);
        }
        let Some(session) = self.session.lock().unwrap().clone() else {
            return Ok(None);
        };
        let (projection, current) = self
            .authority
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if session.session_id != session_id
            || session.realization.as_ref() != Some(&current)
            || !awaken_session_contract::realization_lease_generation_authorizes(&current, lease)
        {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let action = session.terminal_cleanup_work_action().map_err(|error| {
            awaken_session_contract::SessionRealizationControlFailure::Invalid(error.to_string())
        })?;
        let Some(action) = action else {
            return Ok(None);
        };
        Ok(Some(awaken_session_contract::SessionTerminalCleanupWork {
            assignment: awaken_session_contract::SessionTerminalCleanupAssignment {
                session_id: session_id.into(),
                projection,
                lease: current,
            },
            action,
        }))
    }

    async fn terminal_repository_publication_command(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.events.lock().unwrap().push("publication:poll".into());
        let (projection, current) = self
            .authority
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        let session = self
            .session
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if session.session_id != session_id
            || session.realization.as_ref() != Some(&current)
            || !awaken_session_contract::realization_lease_generation_authorizes(&current, lease)
        {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        session
            .terminal_cleanup
            .publication_command(session_id)
            .map(|command| {
                command.map(|command| {
                    awaken_session_contract::SessionRepositoryPublicationProjection {
                        workspace_id: projection.workspace_id,
                        command,
                        current_lease: current,
                    }
                })
            })
            .map_err(|error| {
                awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        let current = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|session| session.realization.clone())
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if !awaken_session_contract::realization_lease_generation_authorizes(&current, lease) {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        self.session
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?
            .terminal_cleanup
            .record_repository_publication_receipt(session_id, receipt.clone())
            .map_err(|error| {
                awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })?;
        self.events
            .lock()
            .unwrap()
            .push("publication:receipt".into());
        self.publication_receipts.lock().unwrap().push(receipt);
        Ok(())
    }

    async fn record_terminal_repository_publication_rejection(
        &self,
        session_id: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
        rejection: awaken_session_contract::SessionRepositoryPublicationRejection,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        let current = self
            .session
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|session| session.realization.clone())
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if !awaken_session_contract::realization_lease_generation_authorizes(&current, lease) {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        self.session
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?
            .terminal_cleanup
            .record_repository_publication_rejection(session_id, rejection.clone())
            .map_err(|error| {
                awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })?;
        self.events
            .lock()
            .unwrap()
            .push("publication:rejection".into());
        self.publication_rejections.lock().unwrap().push(rejection);
        Ok(())
    }

    async fn record_terminal_cleanup_preparation(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        preparation: awaken_session_contract::SessionCleanupPreparation,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        let (projection, current) = self
            .authority
            .lock()
            .unwrap()
            .clone()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if !awaken_session_contract::realization_lease_generation_authorizes(&current, lease) {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let thread_id = preparation.effect.command.thread_id.clone();
        let mut session = self.session.lock().unwrap();
        let session = session
            .as_mut()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        let mut candidate = session.clone();
        let admitted = match candidate.record_terminal_cleanup_preparation(
            &projection.workspace_id,
            lease,
            preparation.clone(),
            None,
        ) {
            Ok(_) => Ok(candidate),
            Err(
                awaken_session_contract::SessionCleanupError::RepositoryPreparationReceiptMismatch,
            ) => {
                let repository_preparation =
                    awaken_session_contract::SessionCleanupRepositoryPreparation::new(
                        &preparation.effect.command.session_id,
                        &projection.workspace_id,
                        &session.resources,
                    )
                    .map_err(|error| {
                        awaken_session_contract::SessionRealizationControlFailure::Invalid(
                            error.to_string(),
                        )
                    })?;
                let mut candidate = session.clone();
                candidate
                    .record_terminal_cleanup_preparation(
                        &projection.workspace_id,
                        lease,
                        preparation.clone(),
                        Some(repository_preparation),
                    )
                    .map(|_| candidate)
            }
            Err(error) => Err(error),
        }
        .map_err(|error| {
            awaken_session_contract::SessionRealizationControlFailure::Invalid(error.to_string())
        })?;
        *session = admitted;
        self.events
            .lock()
            .unwrap()
            .push(format!("cleanup:prepared:{thread_id}"));
        self.preparations.lock().unwrap().push(preparation);
        Ok(())
    }

    async fn record_terminal_cleanup_disposal(
        &self,
        lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionCleanupDisposalReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        let current = self
            .authority
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, lease)| lease.clone())
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        if !awaken_session_contract::realization_lease_generation_authorizes(&current, lease) {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        let workspace_id = self
            .authority
            .lock()
            .unwrap()
            .as_ref()
            .map(|(projection, _)| projection.workspace_id.clone())
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?;
        self.session
            .lock()
            .unwrap()
            .as_mut()
            .ok_or(awaken_session_contract::SessionRealizationControlFailure::NotReady)?
            .record_terminal_cleanup_disposal(
                &workspace_id,
                lease,
                receipt.clone(),
                "remote-terminal-test",
            )
            .map_err(|error| {
                awaken_session_contract::SessionRealizationControlFailure::Invalid(
                    error.to_string(),
                )
            })?;
        self.events.lock().unwrap().push("cleanup:disposed".into());
        self.disposals.lock().unwrap().push(receipt);
        if self
            .disposal_response_loss_once
            .swap(false, Ordering::SeqCst)
        {
            // Model an aggregate disposal CAS whose transport response is lost.
            // The next canonical poll returns Completed (`None`); no Worker-local
            // receipt cache is involved.
            return Err(
                awaken_session_contract::SessionRealizationControlFailure::Unavailable(
                    "injected disposal response loss".into(),
                ),
            );
        }
        Ok(())
    }
}

fn remote_terminal_cleanup_projection() -> awaken_session_contract::FrozenSessionProjection {
    remote_terminal_cleanup_projection_with_idle_retention(Default::default())
}

fn remote_terminal_cleanup_projection_with_idle_retention(
    idle_retention: awaken_session_contract::EnvironmentIdleRetentionPolicy,
) -> awaken_session_contract::FrozenSessionProjection {
    let mut environment = session_environment(
        awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        serde_json::json!({}),
    );
    environment.idle_retention = idle_retention;
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment,
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Worker,
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
    awaken_session_contract::FrozenSessionProjection {
        workspace_id: "terminal-workspace".into(),
        revision: awaken_session_contract::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: Some(awaken_session_contract::SessionResourceManifest::new(
            "terminal-workspace",
            awaken_session_contract::ResolvedSessionResources::default(),
        )),
        mcp: Vec::new(),
        tools: Default::default(),
        request_context: Vec::new(),
    }
}

fn install_test_renewal_authority(
    host: &SharedHost,
    control: &RemoteTerminalCleanupControl,
    session_id: &str,
    lease: awaken_session_contract::SessionRealizationLease,
) {
    host.install_session_realization_lease(session_id, lease.clone());
    control
        .renewals
        .lock()
        .unwrap()
        .insert(session_id.into(), lease);
}

#[async_trait::async_trait]
impl awaken_run_ingress_contract::ClaimedSessionControl for RemoteTerminalCleanupControl {
    async fn resume_frozen(
        &self,
        _claim: &awaken_run_ingress::RunClaim,
        _session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionRealizationDirective>,
        awaken_run_ingress_contract::ClaimedSessionControlError,
    > {
        Ok(None)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_session_reconciliation_is_earliest_deadline_first_and_bounded() {
    use std::sync::atomic::Ordering;

    /* Large reconciliation cause/effect graph: C1 sixty-four independent
     * resident Sessions are simultaneously due at the same Control renewal port;
     * C2 every renewal blocks at one deterministic gate; C3 the Worker-wide bound
     * is eight; C4 lease deadlines differ; C5 the gate is released. Effects:
     * E1 exactly eight renewals enter before release; E2 no ninth enters; E3
     * the first wave contains the eight earliest deadlines; E4 all sixty-four
     * eventually renew; E5 each Session is called exactly once. Constraint:
     * the per-Session realization lock and aggregate Fence remain the only
     * effect authority; this scheduler owns capacity and ordering only.
     *
     * | Rule | Sessions | blocked | cap | Effect |
     * |---|---:|---|---:|---|
     * | L1 | 64 | yes | 8 | E1 + E2 + E3 |
     * | L2 | 64 | released | 8 | E4 + E5 |
     */
    const SESSION_COUNT: usize = 64;
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *control.renewal_gate.lock().unwrap() = Some(gate.clone());
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone()));
    for index in 0..SESSION_COUNT {
        let session_id = format!("mass-session-{index:03}");
        install_test_renewal_authority(
            &host,
            &control,
            &session_id,
            awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: 50_000 + index as u64,
            },
        );
    }

    let running_host = host.clone();
    let reconciliation = tokio::spawn(async move {
        running_host
            .renew_due_session_realizations(
                50_000,
                awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(15_000),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if control.renewal_sessions.lock().unwrap().len()
                == crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("L1 first bounded wave enters");
    assert_eq!(
        control.renewals_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "L1/E1"
    );
    assert_eq!(
        control.max_renewals_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "L1/E2"
    );
    let first_wave = control
        .renewal_sessions
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        first_wave,
        (0..crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS)
            .map(|index| format!("mass-session-{index:03}"))
            .collect::<Vec<_>>(),
        "L1/E3"
    );

    gate.add_permits(SESSION_COUNT);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), reconciliation)
            .await
            .expect("L2 bounded waves finish")
            .expect("L2 reconciliation task")
            .expect("L2 reconciliation result"),
        SESSION_COUNT,
        "L2 every due lease renewed"
    );
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        SESSION_COUNT,
        "L2/E4"
    );
    assert_eq!(control.renewals_active.load(Ordering::SeqCst), 0, "L2/E4");
    let mut observed = control
        .renewal_sessions
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    observed.sort();
    observed.dedup();
    assert_eq!(observed.len(), SESSION_COUNT, "L2/E5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_twelve_session_reconciliation_remains_bounded_and_complete() {
    use std::sync::atomic::Ordering;

    /* Scale cause/effect graph: C1 five hundred twelve resident Sessions have
     * exact live leases outside the renewal proof window; C2 repeated sweeps run
     * before any lease is due; C3 one later sweep makes every lease due; C4 the
     * production renewal cap is eight; C5 an immediate post-renewal sweep is
     * again non-due. Effects: E1 C2 emits no Control renewal, terminal poll, or
     * cold claim and retains every local projection; E2 C3 renews every Session
     * exactly once; E3 concurrency remains bounded; E4 C5 emits no additional
     * Control traffic. This is a deterministic scheduler test, not a production
     * latency claim; live HTTP/PostgreSQL latency remains a deployment
     * measurement.
     *
     * | Rule | sweep time | renewal due | Sessions | Effect |
     * |---|---:|---|---:|---|
     * | S1 | 0, 1000, 5000 | no | 512 | E1 |
     * | S2 | 100000 | yes | 512 | E2 + E3 |
     * | S3 | 100001 | no after S2 | 512 | E4 |
     */
    const SESSION_COUNT: usize = 512;
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone());
    for index in 0..SESSION_COUNT {
        let session_id = format!("scale-session-{index:04}");
        install_test_renewal_authority(
            &host,
            &control,
            &session_id,
            awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: 100_000 + index as u64,
            },
        );
    }
    let timing =
        awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(15_000);
    for now_unix_ms in [0, 1_000, 5_000] {
        assert_eq!(
            host.renew_due_session_realizations(now_unix_ms, timing)
                .await
                .expect("S1 repeated non-due scale sweep succeeds"),
            0,
            "S1/E1"
        );
    }
    assert!(
        control.renewal_sessions.lock().unwrap().is_empty(),
        "S1/E1 no renewal Control traffic"
    );
    assert!(control.events.lock().unwrap().is_empty(), "S1/E1 no poll");
    assert!(
        control.claim_targets.lock().unwrap().is_empty(),
        "S1/E1 no cold claim"
    );
    for index in 0..SESSION_COUNT {
        assert!(
            host.session_slots
                .contains(&format!("scale-session-{index:04}")),
            "S1/E1 retains every local projection"
        );
    }

    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.renew_due_session_realizations(100_000, timing),
        )
        .await
        .expect("S2/E3 scale scan remains bounded")
        .expect("S2 scale renewal succeeds"),
        SESSION_COUNT,
        "S2/E2 every due lease renews"
    );
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        SESSION_COUNT,
        "S2/E2"
    );
    assert!(
        control.max_renewals_active.load(Ordering::SeqCst)
            <= crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "S2/E3"
    );
    assert_eq!(control.renewals_active.load(Ordering::SeqCst), 0, "S2/E3");

    assert_eq!(
        host.renew_due_session_realizations(100_001, timing)
            .await
            .expect("S3 post-renewal non-due sweep succeeds"),
        0,
        "S3/E4"
    );
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        SESSION_COUNT,
        "S3/E4 no additional renewal Control traffic"
    );
    assert!(
        control.events.lock().unwrap().is_empty(),
        "S3/E4 no terminal poll"
    );
    assert!(
        control.claim_targets.lock().unwrap().is_empty(),
        "S3/E4 no cold claim"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn temporary_control_failure_retains_only_a_still_live_session_lease() {
    /* Renewal failure cause/effect table: C1 Control does not answer before its
     * authority-derived request deadline; C2 the prior durable lease is either
     * still live or already expired. Effects: E1 a live projection remains installed for the next
     * sweep and execution is not spuriously cancelled; E2 an expired
     * projection is interrupted and revoked. Constraint: no local grace period
     * extends the durable expiry.
     *
     * | Rule | Control | prior lease | Effect |
     * |---|---|---|---|
     * | F1 | timeout/Unavailable | live | E1 + diagnostic error |
     * | F2 | timeout/Unavailable | expired | E2 |
     */
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    *control.renewal_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone());
    let now = crate::terminal_repository_publication::runtime_unix_now_ms();
    for (session_id, expires_at_unix_ms) in [
        ("renewal-live", now.saturating_add(150)),
        ("renewal-expired", now.saturating_sub(1)),
    ] {
        host.install_session_realization_lease(
            session_id,
            awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms,
            },
        );
    }

    host.renew_due_session_realizations(
        now,
        awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(300),
    )
    .await
    .expect_err("F1 keeps the retry diagnostic visible");
    assert!(host.session_slots.contains("renewal-live"), "F1/E1");
    assert!(!host.session_slots.contains("renewal-expired"), "F2/E2");
}

#[tokio::test]
async fn stale_ownership_revokes_even_when_the_old_deadline_is_still_live() {
    /* Explicit-loss rule F3: a live timestamp is necessary but not sufficient
     * after Control reports StaleOwnership. The newer durable owner wins, so
     * the old Worker must interrupt and remove its local projection in the same
     * reconciliation; it may not consume the remaining timestamp as grace.
     */
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    *control.renewal_failure.lock().unwrap() =
        Some(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control);
    let now = crate::terminal_repository_publication::runtime_unix_now_ms();
    host.install_session_realization_lease(
        "renewal-stale",
        awaken_session_contract::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "worker-a:old".into(),
            epoch: 1,
            expires_at_unix_ms: now.saturating_add(5_000),
        },
    );

    assert_eq!(
        host.renew_due_session_realizations(
            now,
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(10_000),
        )
        .await
        .expect("F3 explicit loss is a handled terminal outcome"),
        0,
        "F3"
    );
    assert!(!host.session_slots.contains("renewal-stale"), "F3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_request_deadlines_release_every_renewal_capacity_slot() {
    use std::sync::atomic::Ordering;

    /* Deadline/capacity cause-effect table: C1 sixteen due lease renewals never
     * answer; C2 the renewal cap is eight; C3 the
     * authority timing yields a 10ms per-request deadline; C4 all old leases
     * remain live. Effects: E1 the first eight enter, time out, and release
     * their slots; E2 the second eight then enter; E3 no cancelled future leaks
     * an active slot; E4 all local projections remain for retry.
     *
     * | Rule | renewals | response | deadline/cap | Effect |
     * |---|---:|---|---|---|
     * | D1 | 16 | never | 10ms / 8 | E1 + E2 + E3 + E4 |
     */
    const SESSION_COUNT: usize = 16;
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    *control.renewal_gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone());
    let now = crate::terminal_repository_publication::runtime_unix_now_ms();
    for index in 0..SESSION_COUNT {
        host.install_session_realization_lease(
            &format!("deadline-session-{index:02}"),
            awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: now.saturating_add(35),
            },
        );
    }

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        host.renew_due_session_realizations(
            now,
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(60),
        ),
    )
    .await
    .expect("D1 the batch cannot inherit the transport's 30s timeout")
    .expect_err("D1 each live Session retains a retry diagnostic");
    assert_eq!(
        control.renewal_sessions.lock().unwrap().len(),
        SESSION_COUNT,
        "D1/E1-E2"
    );
    assert_eq!(control.renewals_active.load(Ordering::SeqCst), 0, "D1/E3");
    assert_eq!(
        control.max_renewals_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RENEWALS,
        "D1/E1"
    );
    for index in 0..SESSION_COUNT {
        assert!(
            host.session_slots
                .contains(&format!("deadline-session-{index:02}")),
            "D1/E4"
        );
    }
}

#[tokio::test]
async fn remote_worker_executes_the_canonical_terminal_cleanup_command_locally() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a Worker-local Session owns a live Sandbox; C2 the
    // global claim-next authority returns its exact durable terminal assignment;
    // C3 the lease is not yet due for ordinary renewal; C4 the aggregate first
    // durably accepts source preparation, then accepts physical disposal but
    // that second transport response is lost; C5 Control reoffers the same
    // terminal assignment on the next recovery scan. Effects: E1 only global
    // assignment recovery enters the aggregate driver; E2 the driver invokes
    // the separate Host preparation and typed physical-disposal ports; E3 the
    // exact preparation and disposal evidence return to Control in order; E4
    // the failed disposal call retains the exact projection; E5 the completed
    // readback retires it without replaying live preparation, physical disposal,
    // or entering renewal/revoke. No resident scan or second scheduler exists.
    //
    // | Rule | cleanup | renewal due | Effect |
    // | R1 | claimed prepare then disposal/response lost | no | E1 + E2 + E3 + E4 |
    // | R2 | re-claimed Completed readback | no | E5 |
    // | R3 | fenced empty | any | retain (covered by application table) |
    // R1 receipt observation is scoped and released before the asynchronous R2
    // readback, so the fixture cannot serialize behavior by holding Control's lock.
    use awaken_provisioning_contract::SandboxStatus;

    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone()));
    let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let mut projection = remote_terminal_cleanup_projection();
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        "remote-terminal-worker",
        projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("R1 complete projection precedes the live Environment");
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 3,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    let create_fence = lease
        .sandbox_effect_fence("remote-terminal-create")
        .expect("R1 create fence");
    let physical = host
        .provider
        .create_sandbox_for_effect(
            &host.sandbox_spec("remote-terminal-worker"),
            &create_fence,
            None,
        )
        .await
        .expect("R1 exact Worker-local Sandbox");
    let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        physical,
    ));
    let binding =
        serde_json::to_string(&environment.handle()).expect("R1 encode live Environment binding");
    let environment_generation = awaken_session_contract::SandboxGeneration::new(
        "remote-terminal-worker",
        lease.epoch,
        lease.expires_at_unix_ms,
        "remote-terminal-environment",
        "remote-terminal-image",
    );
    let environment_effect_id = create_fence.operation_id.clone();
    projection.environment = awaken_session_contract::SessionEnvironmentState::Resident {
        binding,
        effect_id: Some(environment_effect_id.clone()),
        generation: Some(environment_generation.clone()),
        idle_since_unix_ms: None,
    };
    let candidate = host
        .begin_session_environment_preparation("remote-terminal-worker", environment.clone())
        .expect("R1 retain the exact Worker-local candidate");
    host.install_session_environment_owner_projection(
        "remote-terminal-worker",
        &projection.workspace_id,
        &projection.environment,
    )
    .expect("R1 project the durable Worker-local Environment identity");
    host.publish_prepared_session_environment(
        "remote-terminal-worker",
        &candidate,
        crate::session_slot::BoundSessionEnvironmentIdentity::Durable {
            effect_id: environment_effect_id,
            generation: environment_generation,
        },
    )
    .expect("R1 publish the exact Worker-local Environment owner");
    let assignment = awaken_session_contract::SessionTerminalCleanupAssignment {
        session_id: "remote-terminal-worker".into(),
        projection: projection.clone(),
        lease: lease.clone(),
    };
    control
        .assignments
        .lock()
        .unwrap()
        .push_back(assignment.clone());
    let terminal_session = terminal_test_session(
        "remote-terminal-worker",
        &projection,
        lease.clone(),
        std::iter::empty(),
        None,
    );
    let command = terminal_session
        .terminal_cleanup
        .command_for("remote-terminal-worker", "remote-terminal-worker")
        .expect("R1 canonical command");
    *control.session.lock().unwrap() = Some(terminal_session);
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };
    control
        .disposal_response_loss_once
        .store(true, Ordering::SeqCst);

    host.recover_terminal_cleanup_assignments(target.clone())
        .await
        .expect_err("R1/C4 reports the ambiguous completion response");
    assert!(
        host.session_slots.contains("remote-terminal-worker"),
        "R1/E4 keeps the exact projection for aggregate readback"
    );
    assert_eq!(
        environment.status().await.unwrap(),
        SandboxStatus::Terminated,
        "R1/E2"
    );
    {
        let disposals = control.disposals.lock().unwrap();
        assert_eq!(disposals.len(), 1, "R1/E3");
        assert_eq!(
            disposals[0]
                .provider_disposal
                .prepared_effect_fence()
                .operation_id,
            command.effect_id,
            "R1/E3",
        );
    }
    control.assignments.lock().unwrap().push_back(assignment);
    assert_eq!(
        host.recover_terminal_cleanup_assignments(target.clone())
            .await
            .expect("R2 Completed readback"),
        1,
        "R2/E5 reconciles the completed terminal generation without renewal"
    );
    assert!(
        !host.session_slots.contains("remote-terminal-worker"),
        "R2/E5 retires the exact local tree"
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target.clone(), target.clone(), target],
        "R1-R2 each recovery claims one assignment then observes the empty queue"
    );
    drop(managed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_terminal_cleanup_renews_its_generation_and_another_session() {
    // Cause/effect graph: C1 global claim-next returns one cold terminal
    // assignment and its canonical driver blocks after aggregate authorization;
    // C2 that terminal generation and another ordinary Session owned by the
    // Worker are both due for renewal; C3 the terminal authorization is later
    // released. Effects: E1 C1 uniquely installs and retains the terminal slot;
    // E2 both Sessions reach the sole aggregate lease-renewal port and install
    // monotonic same-generation readback before C3; E3 no resident scan, second
    // cleanup driver, or local work queue is introduced; E4 the old asserted
    // terminal effect continues under its renewed current generation and reaches
    // its durable boundary. Constraint: the Session root remains the only
    // renewal/terminal decision owner; renewal does not wait for long-lived
    // terminal I/O.
    //
    // | Rule | cleanup blocked | terminal due | ordinary due | release | Effect |
    // |---|---|---|---|---|---|
    // | R1 | yes | yes | yes | no | E1 + E2 + E3 |
    // | R2 | yes | renewed | renewed | yes | E4 |
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone()));
    let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let now_unix_ms = crate::terminal_repository_publication::runtime_unix_now_ms();
    let expiring_unix_ms = now_unix_ms.saturating_add(5_000);
    let requested_expiry_unix_ms = now_unix_ms.saturating_add(30_000);

    let ordinary_id = "renewal-beside-terminal";
    let ordinary_projection = remote_terminal_cleanup_projection();
    awaken_session_contract::SessionRuntime::install_session_projection(
        &managed,
        ordinary_id,
        ordinary_projection.clone(),
        awaken_session_contract::SessionProjectionInstallMode::Dispatch,
    )
    .await
    .expect("R1/C2 install ordinary frozen projection");
    let ordinary_lease = awaken_session_contract::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 8,
        expires_at_unix_ms: expiring_unix_ms,
    };
    host.install_session_realization_lease(ordinary_id, ordinary_lease.clone());
    control
        .renewals
        .lock()
        .unwrap()
        .insert(ordinary_id.into(), ordinary_lease.clone());

    let terminal_id = "blocked-terminal-cleanup";
    let terminal_projection = remote_terminal_cleanup_projection();
    let terminal_lease = awaken_session_contract::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 9,
        expires_at_unix_ms: expiring_unix_ms,
    };
    control.assignments.lock().unwrap().push_back(
        awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: terminal_id.into(),
            projection: terminal_projection.clone(),
            lease: terminal_lease.clone(),
        },
    );
    *control.session.lock().unwrap() = Some(terminal_test_session(
        terminal_id,
        &terminal_projection,
        terminal_lease.clone(),
        std::iter::empty(),
        None,
    ));
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: terminal_lease.owner.clone(),
        runtime_incarnation: terminal_lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: terminal_lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };
    let cleanup_started = Arc::new(tokio::sync::Notify::new());
    let cleanup_release = Arc::new(tokio::sync::Notify::new());
    *control.authorization_barrier.lock().unwrap() =
        Some((cleanup_started.clone(), cleanup_release.clone()));

    let cleanup = tokio::spawn({
        let host = host.clone();
        let target = target.clone();
        async move { host.recover_terminal_cleanup_assignments(target).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        cleanup_started.notified(),
    )
    .await
    .expect("R1/C1 terminal driver reaches the blocking authorization");

    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            host.renew_due_session_realizations(
                now_unix_ms,
                awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(
                    30_000,
                ),
            ),
        )
        .await
        .expect("R1/E2 renewal cadence is independent from terminal I/O")
        .expect("R1/E2 aggregate renewal succeeds"),
        2,
        "R1/E2"
    );
    assert!(!cleanup.is_finished(), "R1/E1");
    let mut renewed_sessions = control.renewal_sessions.lock().unwrap().clone();
    renewed_sessions.sort();
    assert_eq!(
        renewed_sessions,
        [terminal_id.to_owned(), ordinary_id.to_owned()],
        "R1/E2 both Sessions use the sole lease-only renewal port"
    );
    assert_eq!(
        host.session_slots
            .read(ordinary_id, |slot| {
                slot.realization_lease
                    .as_ref()
                    .map(|lease| lease.expires_at_unix_ms)
            })
            .flatten(),
        Some(requested_expiry_unix_ms),
        "R1/E2"
    );
    assert_eq!(
        host.session_slots
            .read(terminal_id, |slot| {
                slot.realization_lease
                    .as_ref()
                    .map(|lease| lease.expires_at_unix_ms)
            })
            .flatten(),
        Some(requested_expiry_unix_ms),
        "R1/E2 terminal slot installs authoritative renewal readback"
    );

    cleanup_release.notify_one();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), cleanup)
            .await
            .expect("R2/E4 cleanup reaches its durable boundary")
            .expect("R2/E4 cleanup task joins")
            .expect("R2/E4 cleanup succeeds"),
        1,
        "R2/E4 one claimed terminal assignment completes"
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target],
        "R1-R2 one exact claim plus the terminating empty scan"
    );
    drop(managed);
}

#[tokio::test]
async fn cold_terminal_assignment_installs_then_uses_the_canonical_cleanup_path() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a terminal Session has no process-local slot after
    // Worker replacement; C2 claim-next returns a typed routing assignment with
    // the frozen projection, an active Repository manifest, and a newly fenced
    // lease but no Run claim; C3 the first Control poll classifies the terminal
    // fence; C4 the canonical driver re-reads the same aggregate work; C5 the
    // closed preparation authorization succeeds. Effects: E1 the driver is the
    // sole installer of the exact baseline and lease before C5, without
    // re-materializing the Resource being destroyed; E2 it durably records
    // source preparation before projecting typed physical disposal; E3 the
    // exact disposal receipt returns through Control and removes the slot only
    // after aggregate completion; E4 claim-next then returns empty and the
    // bounded recovery scan stops without a Worker-local queue or duplicate
    // cleanup path. Constraint: a claim never installs a projection or carries
    // commands; the driver installs current readback before source I/O.
    //
    // | Rule | local slot | Control boundary | Effect |
    // |---|---|---|---|
    // | C1 | absent | before claim | remains absent |
    // | C2 | installed | authorization admitted | E1 before source I/O |
    // | C3 | installed | root prepare then dispose | E2 + E3 |
    // | C4 | removed | next claim is empty | E4 |
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let authorization_started = Arc::new(tokio::sync::Notify::new());
    let authorization_proceed = Arc::new(tokio::sync::Notify::new());
    *control.authorization_barrier.lock().unwrap() =
        Some((authorization_started.clone(), authorization_proceed.clone()));
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone()));
    let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let session_id = "cold-terminal-worker";
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:replacement".into(),
        epoch: 4,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    };
    let mut projection = remote_terminal_cleanup_projection();
    projection.resource_revision = 1;
    projection.resources = effective_repository(
        "terminal-cleanup-resource",
        "/must-not-be-cloned-during-terminal-cleanup",
        "/workspace/cleanup",
        None,
    );
    control.assignments.lock().unwrap().push_back(
        awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection: projection.clone(),
            lease: lease.clone(),
        },
    );
    let terminal_session = terminal_test_session(
        session_id,
        &projection,
        lease.clone(),
        std::iter::empty(),
        None,
    );
    let command = terminal_session
        .terminal_cleanup
        .command_for(session_id, session_id)
        .expect("C3 canonical root command");
    *control.session.lock().unwrap() = Some(terminal_session);
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        reassign_existing_lease: false,
    };
    assert!(!host.session_slots.contains(session_id), "C1");

    let recovery = tokio::spawn({
        let host = host.clone();
        let target = target.clone();
        async move { host.recover_terminal_cleanup_assignments(target).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        authorization_started.notified(),
    )
    .await
    .expect("C2 canonical driver reaches authorization after its unique install");
    assert_eq!(
        host.session_slots
            .read(session_id, |slot| slot.baseline.clone())
            .flatten()
            .expect("C1/E1 frozen baseline")
            .fingerprint,
        remote_terminal_cleanup_projection().baseline.fingerprint,
        "C1/E1"
    );
    assert!(
        host.session_slots
            .read(session_id, |slot| slot.resources.mounts.is_empty())
            .unwrap_or(false),
        "C1/E1 terminal installation must not materialize active Resources"
    );
    assert_eq!(
        host.session_slots
            .read(session_id, |slot| slot.realization_lease.clone())
            .flatten(),
        Some(lease.clone()),
        "C1/E1"
    );
    authorization_proceed.notify_one();
    assert_eq!(
        recovery.await.unwrap().expect("C2-C4 cold recovery"),
        1,
        "C2/E2"
    );
    assert!(!host.session_slots.contains(session_id), "C2/E2/E3");
    let disposals = control.disposals.lock().unwrap();
    assert_eq!(disposals.len(), 1, "C2/E3");
    assert_eq!(
        disposals[0]
            .provider_disposal
            .prepared_effect_fence()
            .operation_id,
        command.effect_id,
        "C2/E3",
    );
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target],
        "C1-C4/E4"
    );
    drop(disposals);
    drop(managed);
}

#[tokio::test]
async fn coordinated_thread_archive_uses_disposition_and_dispatch_as_one_recoverable_saga() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 child disposition is Active/Archived; C2 its queue
    // state is absent, Pending, or Awaiting; C3 archive/admission ordering is
    // ordinary or raced. Effects: E1 idle archive commits the one Thread
    // disposition and exact retry is a no-op; E2 active Pending is rejected before
    // disposition mutation; E3 one Awaiting-Thread archive call records ordinary
    // cancellation, waits for the Worker-owned terminal settlement, then commits
    // Archived; E4 an exact archive replay also waits for a stale raced admission
    // to settle without another relationship/archive store or client retry loop.
    //
    // | Rule | Disposition | Dispatch | Ordering | Effect |
    // |---|---|---|---|---|
    // | A1 | Active | absent/idle | archive | E1 |
    // | A2 | Active | Pending | archive | E2 |
    // | A3 | Active | Awaiting | archive | E3 |
    // | A4 | Archived | Pending | stale admission | E4 |
    use awaken_agent_contract::agent::awaiting::{AwaitTarget, RemoteInputReason, ResumeTicket};
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_run_ingress::{DispatchOutcome, DispatchState, RunDispatch};

    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("archive saga dispatch"),
    );
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_coordinator_dispatch_store(dispatch.clone()),
    );
    let parent = ThreadId("archive-saga-parent".into());
    let commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("parent partition");

    let idle = ThreadId("archive-saga-idle".into());
    let idle_run = RunId("archive-saga-idle-run".into());
    commit
        .commit(ThreadCommit::assemble(
            idle.clone(),
            RunDisposition::ended(idle_run.clone(), EndCause::NaturalEnd),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A1 idle child truth");
    host.archive_session_thread(&parent.0, &idle)
        .await
        .expect("A1 archive");
    host.archive_session_thread(&parent.0, &idle)
        .await
        .expect("A1 exact retry");
    assert_eq!(
        host.session_thread_disposition(&parent.0, &idle)
            .await
            .expect("A1 disposition"),
        awaken_agent_contract::ThreadDisposition::Archived,
        "A1/E1"
    );

    let pending = ThreadId("archive-saga-pending".into());
    let pending_run = RunId("archive-saga-pending-run".into());
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &pending.0,
                &pending_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(11),
        )
        .await
        .expect("A2 pending admission");
    let rejected = host
        .archive_session_thread(&parent.0, &pending)
        .await
        .expect_err("A2 running work cannot archive");
    assert_eq!(rejected.kind, HostErrorKind::BadRequest, "A2/E2");
    assert_eq!(
        host.session_thread_disposition(&parent.0, &pending)
            .await
            .expect("A2 disposition"),
        awaken_agent_contract::ThreadDisposition::Active,
        "A2/E2"
    );
    dispatch
        .cancel(&pending_run)
        .await
        .expect("A2 cleanup intent");
    let pending_cancel = dispatch
        .claim("archive-test", 100, 0, &Default::default())
        .await
        .expect("A2 cancellation claim")
        .expect("A2 cancellation work");
    dispatch
        .settle(
            &pending_run,
            pending_cancel.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("A2 cleanup settle");

    let awaiting = ThreadId("archive-saga-awaiting".into());
    let awaiting_run = RunId("archive-saga-awaiting-run".into());
    commit
        .commit(ThreadCommit::assemble(
            awaiting.clone(),
            RunDisposition::running(awaiting_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A3 running child truth");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &awaiting.0,
                &awaiting_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(12),
        )
        .await
        .expect("A3 child admission");
    let awaiting_claim = dispatch
        .claim("archive-test", 100, 0, &Default::default())
        .await
        .expect("A3 claim")
        .expect("A3 work");
    dispatch
        .settle(
            &awaiting_run,
            awaiting_claim.lease.epoch,
            DispatchOutcome::Awaiting,
            &[],
        )
        .await
        .expect("A3 await");
    let ticket = ResumeTicket::new(
        "archive-saga-correlation",
        awaiting_run.clone(),
        awaiting.clone(),
        "archive-saga-snapshot",
        "archive-saga-catalog",
        AwaitTarget::RemoteInput {
            reason: RemoteInputReason::UserInput,
            call_id: "archive-saga-call".into(),
        },
    );
    commit
        .commit(ThreadCommit::assemble(
            awaiting.clone(),
            RunDisposition::awaiting(ticket),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A3 Awaiting child truth");
    let archive_awaiting = {
        let host = host.clone();
        let parent = parent.clone();
        let awaiting = awaiting.clone();
        tokio::spawn(async move { host.archive_session_thread(&parent.0, &awaiting).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let rows = dispatch.list_dispatches().await.expect("A3 cancellation");
            if rows.iter().any(|row| {
                row.run_id == awaiting_run
                    && matches!(row.state, DispatchState::Pending | DispatchState::Awaiting)
                    && row.cancellation_requested
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A3 archive records ordinary cancellation before waiting");
    let awaiting_cancel = dispatch
        .claim("archive-test", 100, 0, &Default::default())
        .await
        .expect("A3 cancellation claim")
        .expect("A3 cancellation work");
    commit
        .commit(ThreadCommit::assemble(
            awaiting.clone(),
            RunDisposition::ended(awaiting_run.clone(), EndCause::NaturalEnd),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("A3 terminal interruption truth");
    dispatch
        .settle(
            &awaiting_run,
            awaiting_cancel.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("A3 terminal settle");
    archive_awaiting
        .await
        .expect("A3 archive task")
        .expect("A3/E3 one call archives after terminal settle");
    assert_eq!(
        host.session_thread_disposition(&parent.0, &awaiting)
            .await
            .expect("A3 disposition"),
        awaken_agent_contract::ThreadDisposition::Archived,
        "A3/E3"
    );

    let raced_run = RunId("archive-saga-raced-run".into());
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &idle.0,
                &raced_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(13),
        )
        .await
        .expect("A4 stale admission after archive");
    let archive_raced = {
        let host = host.clone();
        let parent = parent.clone();
        let idle = idle.clone();
        tokio::spawn(async move { host.archive_session_thread(&parent.0, &idle).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let rows = dispatch.list_dispatches().await.expect("A4 cancellation");
            if rows
                .iter()
                .any(|row| row.run_id == raced_run && row.cancellation_requested)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A4 archived replay records cancellation before waiting");
    let raced_cancel = dispatch
        .claim("archive-test", 100, 0, &Default::default())
        .await
        .expect("A4 cancellation claim")
        .expect("A4 cancellation work");
    assert!(raced_cancel.cancellation_requested, "A4/E4");
    dispatch
        .settle(
            &raced_run,
            raced_cancel.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("A4 cancellation settle");
    archive_raced
        .await
        .expect("A4 archive task")
        .expect("A4/E4 one replay converges after raced work settles");
}

#[tokio::test]
async fn failed_child_boundary_cancels_raced_follow_up_without_fencing_its_own_settlement() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 the current coordinated child dispatch still owns
    // its lease but its Run has committed Failed; C2 a later follow-up for the
    // same logical Thread was admitted just before that failure became visible;
    // C3 the failed boundary invokes the existing Thread interrupt path. Effects:
    // E1 committed Run truth closes future admission; E2 C3 leaves the already
    // terminal current claim untouched; E3 C3 durably marks the queued follow-up
    // cancelled; E4 current settlement succeeds, then canonical cancellation
    // settlement consumes the raced row. No Thread terminal flag is stored.
    //
    // | Rule | Current Run | Later row | Interrupt | Effects |
    // |---|---|---|---|---|
    // | R1 | Failed + Leased | Pending | no | E1 only |
    // | R2 | Failed + Leased | Pending | yes | E1+E2+E3 |
    // | R3 | R2 | cancel claim | settled | E4 |
    use awaken_agent_contract::agent::run::Failure;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_agent_contract::thread::commit::staged::{RunDisposition, ThreadCommit};
    use awaken_run_ingress::{DispatchOutcome, RunDispatch};

    let dispatch = Arc::new(
        awaken_run_ingress::AnyDispatchStore::open_sqlite_in_memory()
            .expect("failed follow-up race dispatch"),
    );
    let host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_coordinator_dispatch_store(dispatch.clone());
    let parent = ThreadId("failed-race-parent".into());
    let child = ThreadId("failed-race-child".into());
    let failed_run = RunId("failed-race-current".into());
    let follow_up_run = RunId("failed-race-follow-up".into());
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &child.0,
                &failed_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(21),
        )
        .await
        .expect("R1 current admission");
    let current_claim = dispatch
        .claim("failed-race-worker", 100, 0, &Default::default())
        .await
        .expect("R1 claim")
        .expect("R1 current work");
    dispatch
        .enqueue(
            RunDispatch::new(crate::host::worker_resolver::test_support::test_activation(
                &child.0,
                &follow_up_run.0,
            ))
            .for_session(parent.clone())
            .with_session_activity_epoch(22),
        )
        .await
        .expect("R1 raced later admission");
    let commit = host
        .commit_for_read(&parent.0)
        .await
        .expect("parent partition");
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::running(failed_run.clone()),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("R1 Running truth");
    commit
        .commit(ThreadCommit::assemble(
            child.clone(),
            RunDisposition::ended(failed_run.clone(), EndCause::Error(Failure::StateConflict)),
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ))
        .await
        .expect("R1 Failed truth");
    assert!(
        host.coordinated_thread_has_failed_run(&parent.0, &child)
            .await
            .expect("R1 admission fence"),
        "R1/E1"
    );

    host.interrupt_session_thread(&parent.0, &child)
        .await
        .expect("R2 boundary interruption");
    let rows = dispatch.list_dispatches().await.expect("R2 inspect rows");
    let current = rows
        .iter()
        .find(|row| row.run_id == failed_run)
        .expect("R2 current row");
    let raced = rows
        .iter()
        .find(|row| row.run_id == follow_up_run)
        .expect("R2 raced row");
    assert!(!current.cancellation_requested, "R2/E2");
    assert!(raced.cancellation_requested, "R2/E3");

    let settled = dispatch
        .settle(
            &failed_run,
            current_claim.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("R3 current settlement");
    assert!(settled.applied(), "R3/E4 current claim was not fenced");
    let cancellation = dispatch
        .claim("failed-race-worker", 100, 0, &Default::default())
        .await
        .expect("R3 cancellation claim")
        .expect("R3 cancellation work");
    assert_eq!(cancellation.request.run_id(), &follow_up_run, "R3/E4");
    assert!(cancellation.cancellation_requested, "R3/E4");
    dispatch
        .settle(
            &follow_up_run,
            cancellation.lease.epoch,
            DispatchOutcome::Done,
            &[],
        )
        .await
        .expect("R3 cancellation settlement");
}

#[tokio::test]
async fn host_accepts_only_backend_projections_that_match_the_publication() {
    // Cause graph:
    // C1 immutable publication -> E1 execution backend authority.
    // C2 non-default frozen baseline projection -> E2 equality check only;
    // canonical `default` is Native absence and is not stored redundantly.
    // C3 projection without publication -> E3 reject; a cache cannot become an
    // authoring source merely because the publication is unavailable.
    //
    // | Rule | Publication | Projection | Result |
    // | H1 | default | default (canonical absence) | accept native |
    // | H2 | default | acp:claude | reject mismatch |
    // | H3 | default | absent | accept publication |
    // | H4 | absent | acp:claude | reject missing authority |
    let published_host = || {
        let snapshot = crate::config::server_config(
            "assistant",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &Default::default(),
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );
        let publications =
            awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
                .expect("valid publication");
        dispatch_test_host(
            SharedHost::new(Arc::new(OkModel), "stub")
                .with_agent_publications(Arc::new(publications)),
        )
    };

    let (matching, _matching_managed) = published_host();
    matching.register_thread_backend_projection("backend-h1", "default");
    assert!(
        matching
            .session_slots
            .read("backend-h1", |slot| slot.backend_ref.is_none())
            .unwrap_or(false),
        "H1 default has no redundant projection"
    );
    matching
        .ctx_for("backend-h1", Some("assistant"))
        .await
        .expect("H1 matching projection");

    let (mismatch, _mismatch_managed) = published_host();
    mismatch.register_thread_backend_projection("backend-h2", "acp:claude");
    let error = match mismatch.ctx_for("backend-h2", Some("assistant")).await {
        Ok(_) => panic!("H2 accepted a mismatched backend projection"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("does not match publication"),
        "H2"
    );

    let (publication_only, _publication_only_managed) = published_host();
    publication_only
        .ctx_for("backend-h3", Some("assistant"))
        .await
        .expect("H3 publication without redundant projection");

    let (orphan, _orphan_managed) = dispatch_test_host(SharedHost::new(Arc::new(OkModel), "stub"));
    orphan.register_thread_backend_projection("backend-h4", "acp:claude");
    let error = match orphan.ctx_for("backend-h4", Some("assistant")).await {
        Ok(_) => panic!("H4 accepted a backend projection without a publication"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("no immutable model publication"),
        "H4: {error}"
    );
}

#[test]
fn frozen_session_model_override_replaces_the_complete_route_exactly_once() {
    use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
    use awaken_session_contract::{SessionModelOverride, SessionModelPublication};

    let base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(ResolvedModelCandidate::host(ModelBinding::new(
            "agent-provider",
            "base",
            "genai",
        )))
        .model_candidates([ModelBinding::new(
            "agent-provider",
            "base-fallback",
            "genai",
        )])
        .build();
    let primary =
        ResolvedModelCandidate::host(ModelBinding::new("third-party/gateway", "fast", "genai"));
    let fallback =
        ResolvedModelCandidate::host(ModelBinding::new("third-party/gateway", "slow", "genai"));
    let frozen = SessionModelOverride {
        publication: Some(Box::new(SessionModelPublication {
            primary: primary.clone(),
            candidates: vec![fallback.clone()],
        })),
        inference: Default::default(),
    };

    // Cause/effect table:
    // O1 complete override -> only its exact primary + roster survive;
    // O2 replay -> byte-identical fingerprint (idempotent);
    // O3 equal-id inference-only override -> Agent roster stays authoritative;
    // O4 malformed frozen roster -> fail before executor materialization.
    let inherit = awaken_session_contract::SessionSystemPromptSelection::Inherit;
    let projected = awaken_session_contract::project_effective_agent_publication(
        Some(&frozen),
        &inherit,
        "workspace",
        base.clone(),
    )
    .expect("O1 exact projection");
    assert_eq!(projected.resolved_spec.model_binding, primary, "O1");
    assert_eq!(projected.resolved_spec.model_candidates, [fallback], "O1");
    assert!(
        projected
            .resolved_spec
            .candidate_for_model("base")
            .is_none(),
        "O1 superseded Agent route must not leak"
    );
    let replayed = awaken_session_contract::project_effective_agent_publication(
        Some(&frozen),
        &inherit,
        "workspace",
        projected.clone(),
    )
    .expect("O2 idempotent replay");
    assert_eq!(replayed.fingerprint, projected.fingerprint, "O2");

    let inference_only = SessionModelOverride {
        publication: None,
        inference: Default::default(),
    };
    let reused = awaken_session_contract::project_effective_agent_publication(
        Some(&inference_only),
        &inherit,
        "workspace",
        base.clone(),
    )
    .expect("O3 Agent publication reuse");
    assert_eq!(
        reused.resolved_spec.candidate_bindings(),
        base.resolved_spec.candidate_bindings(),
        "O3"
    );

    let malformed = SessionModelOverride {
        publication: Some(Box::new(SessionModelPublication {
            primary: primary.clone(),
            candidates: vec![primary],
        })),
        inference: Default::default(),
    };
    assert!(
        awaken_session_contract::project_effective_agent_publication(
            Some(&malformed),
            &inherit,
            "workspace",
            base,
        )
        .is_err(),
        "O4 duplicate route must fail closed"
    );
}

#[tokio::test]
async fn session_tool_policy_preserves_the_published_snapshot_identity() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    // Cause/effect decision table:
    // | publication | frozen Session policy | effect |
    // | present     | present               | Runtime gate reads Session policy separately; snapshot and fingerprint unchanged |
    // | generated   | present               | generated execution config receives policy |
    // The second rule is owned by the fallback construction path. This test owns
    // the immutable-publication and distributed-continuation boundary: another
    // Worker must reconstruct the exact publication fingerprint. The executable
    // gate behavior of the same `effective_tool_authorization` owner is covered by
    // `config::tests::toolset_policy_controls_execution_gate_behavior`; copying
    // policy into this snapshot would create a second authority and fingerprint.
    let publication = crate::config::server_config(
        "assistant",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([publication.clone()])
            .expect("valid publication");
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications)),
    );
    host.session_slots.update("policy-session", |slot| {
        slot.tools = Some(awaken_session_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Mcp {
                    server_name: "flow".into(),
                },
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::new(
                    "write",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                )],
            }],
            client_tools: Vec::new(),
        });
    });

    let context = host
        .ctx_for("policy-session", Some("assistant"))
        .await
        .expect("build Session from immutable publication");
    assert!(
        context
            .config
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .is_empty(),
        "the attempt clone preserves the immutable publication; Session policy is consumed by the one authorization owner"
    );
    assert!(
        publication
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .is_empty()
    );
    assert_eq!(context.config.fingerprint, publication.fingerprint);
}

#[tokio::test]
async fn session_web_policy_overlay_reaches_the_configured_plugin_before_inference() {
    // Cause/effect graph: C1 an immutable root publication selects the
    // provider-server WebFetch realization; C2 the final Session tool overlay
    // adds one content restriction. E1 root Session composition passes C2 to the
    // same configured WebFetch plugin; E2 that plugin rejects C1+C2 before the
    // first model request, so a provider-internal fetch is also impossible.
    // The extension-level matrix owns the individual Web policy fields; this
    // composition rule owns only the Session overlay wiring.
    //
    // | Rule | realization | final Session policy | Effect |
    // | S1 | provider-server | max_content_tokens | capability-bound terminal; inference=0 |
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
    };

    struct SessionInferenceProbe(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl LlmExecutor for SessionInferenceProbe {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ChatResponse {
                output: AssistantOutput::text("unexpected inference"),
                usage: None,
                stop_reason: None,
            })
        }
    }

    let mut publication = crate::config::server_config(
        "assistant",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string()],
        &BTreeMap::from([(
            awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID.to_string(),
            serde_json::json!({"provider_id": "openrouter", "options": {}}),
        )]),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    publication
        .recompute_fingerprint()
        .expect("provider-server root publication remains coherent");
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([publication])
            .expect("valid provider-server publication");
    let inferences = Arc::new(AtomicUsize::new(0));
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(SessionInferenceProbe(inferences.clone())), "stub")
            .with_agent_publications(Arc::new(publications)),
    );
    host.session_slots.update("provider-server-policy", |slot| {
        slot.tools = Some(awaken_session_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy::default(),
                overrides: vec![ToolPolicyOverride::with_optional_configuration(
                    awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID,
                    ToolExecutionPolicy::default(),
                    Some(serde_json::json!({
                        "type": "web_fetch",
                        "max_content_tokens": 512
                    })),
                )],
            }],
            client_tools: Vec::new(),
        });
    });

    let receipt = host
        .run(
            Some("assistant"),
            "provider-server-policy",
            user("fetch docs"),
        )
        .await
        .expect("S1 capability violations are committed as terminal Run truth");
    assert!(
        matches!(
            receipt.state,
            RunState::Ended(EndCause::Error(
                awaken_agent_contract::agent::run::Failure::CapabilityBound
            ))
        ),
        "S1/E1"
    );
    assert_eq!(inferences.load(Ordering::SeqCst), 0, "S1/E2");
}

#[test]
fn repository_skill_discovery_requires_read_not_merely_a_filesystem_tool() {
    // Managed repository Skill cause/effect rules R2/R3. C1 a Managed Agent
    // toolset defaults disabled; C2 `bash` is explicitly enabled; C3 exact
    // `read` is disabled or enabled; C4 workspace-qualified and legacy-relative
    // resolved Repository mount plans exist; C5 a cold Managed recovery has no
    // publication snapshot but retains the same slot tool policy.
    // E1 general filesystem delivery remains possible from C2; E2 repository
    // roots are excluded for C3=false and admitted for C3=true.
    //
    // | Rule | Managed | repo | bash | read | admitted roots |
    // | R3   | yes     | yes  | on   | off  | none           |
    // | R2   | yes     | yes  | on   | on   | fixed repo path|
    // | R2c  | yes/cold| yes  | on   | on   | fixed repo path|
    // Constraint: another filesystem capability cannot widen `read` and
    // `plugin_config.skills_dir` cannot alter the Managed repository path.
    // Direct compatibility rows remain unchanged: a published/read-enabled
    // caller includes its authored root plus repository roots; no publication
    // retains `Some([])` as the live authored-source marker.
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let enabled = ToolExecutionPolicy {
        enabled: true,
        permission: ToolPermissionRequirement::AlwaysAllow,
    };
    let repository =
        |repository_id: &str, mount_path: &str| crate::provisioning::RepositoryActivation {
            plan: awaken_provisioning_contract::RepositoryRealizationPlan {
                repository_id: repository_id.into(),
                mount_path: mount_path.into(),
                source_remote_url: format!("https://example.invalid/{repository_id}.git"),
                transport_url: format!("https://example.invalid/{repository_id}.git"),
                initial_branch: None,
                initial_commit: None,
                access: awaken_provisioning_contract::MountAccess::ReadWrite,
            },
            credential_pin: None,
        };
    host.session_slots.update("repository-policy", |slot| {
        slot.session_dispatch = true;
        slot.tools = Some(awaken_session_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
                overrides: vec![ToolPolicyOverride::new("bash", enabled)],
            }],
            client_tools: Vec::new(),
        });
        slot.resources.repositories.extend([
            repository("repo-a", "/workspace/repo-a"),
            repository("legacy-repo", "legacy-repo"),
        ]);
    });
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("repository-agent")
        .model(test_model_binding())
        .build();

    assert!(
        host.session_allows_filesystem_tools("repository-policy", None),
        "R3 filesystem delivery"
    );
    assert!(
        !host.session_allows_repository_skill_discovery("repository-policy", Some(&snapshot)),
        "R3 exact read denial"
    );
    assert_eq!(
        host.session_skill_source_roots(
            "repository-policy",
            Some(&snapshot),
            "custom-authored-skills",
        ),
        None,
        "R3 no Managed repository source"
    );

    host.session_slots.update("repository-policy", |slot| {
        slot.tools.as_mut().expect("Session tools").toolsets[0]
            .overrides
            .push(ToolPolicyOverride::new("read", enabled));
    });
    assert!(
        host.session_allows_repository_skill_discovery("repository-policy", Some(&snapshot)),
        "R2 exact read admission"
    );
    assert_eq!(
        host.session_skill_source_roots(
            "repository-policy",
            Some(&snapshot),
            "custom-authored-skills",
        ),
        Some(vec![
            "legacy-repo/.claude/skills".to_string(),
            "workspace/repo-a/.claude/skills".to_string(),
        ]),
        "R2 fixed relative path ignores the direct authored setting"
    );
    assert_eq!(
        host.session_skill_source_roots("repository-policy", None, "custom-authored-skills",),
        Some(vec![
            "legacy-repo/.claude/skills".to_string(),
            "workspace/repo-a/.claude/skills".to_string(),
        ]),
        "R2c cold Managed recovery uses the durable slot read policy"
    );

    host.session_slots
        .update("repository-policy", |slot| slot.session_dispatch = false);
    assert_eq!(
        host.session_skill_source_roots(
            "repository-policy",
            Some(&snapshot),
            "custom-authored-skills",
        ),
        Some(vec![
            "custom-authored-skills".to_string(),
            "legacy-repo/.claude/skills".to_string(),
            "workspace/repo-a/.claude/skills".to_string(),
        ]),
        "direct published compatibility keeps authored and repository roots"
    );
    assert_eq!(
        host.session_skill_source_roots("unpublished-direct", None, "skills"),
        Some(Vec::new()),
        "direct unpublished compatibility keeps the live authored marker"
    );
}

#[test]
fn skill_delivery_profile_uses_one_managed_filesystem_projection() {
    // Cause/effect graph: C1 profile is Managed; C2 an exact frozen Skill is
    // selected; C3 at least one filesystem tool is enabled; C4 the frozen
    // projection is present. Effects: E1 ManagedFilesystem is selected; E2 the
    // direct compatibility SemanticTools projection is selected; E3 admission
    // fails before inference; E4 a Skill-free Managed Session may retain
    // SemanticTools for non-Skill content such as Memory.
    //
    // | Rule | C1 Managed | C2 Skill | C3 FS | C4 frozen | Effect |
    // | M1 | no  | yes | no  | n/a | E2 direct compatibility adapter |
    // | M2 | yes | yes | yes | yes | E1 filesystem-only Skill projection |
    // | M3 | yes | yes | no  | yes | E3 reject; no semantic fallback |
    // | M4 | yes | no  | no  | yes | E4 semantic non-Skill content allowed |
    // | M5 | yes | bound | any | no | E3 missing selected version rejected |
    use crate::session_slot::ManagedContentDelivery;
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
    };

    let deny_filesystem = || awaken_session_contract::SessionToolConfiguration {
        toolsets: vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: false,
                permission: ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: Vec::new(),
        }],
        client_tools: Vec::new(),
    };
    let frozen = vec![frozen_skill_version(
        "think",
        "Think",
        "reason",
        "Think carefully.",
        &[],
    )];
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_skills(vec![
        awaken_ext_skills::SkillSpec::new("think", "Think", "reason", "Think carefully."),
    ]);

    host.session_slots.update("direct-semantic", |slot| {
        slot.tools = Some(deny_filesystem());
    });
    assert_eq!(
        host.select_content_delivery("direct-semantic", None, None)
            .expect("M1 direct compatibility projection"),
        ManagedContentDelivery::SemanticTools,
        "M1/E2"
    );

    host.session_slots.update("managed-filesystem", |slot| {
        slot.session_dispatch = true;
    });
    assert_eq!(
        host.select_content_delivery("managed-filesystem", None, Some(&frozen))
            .expect("M2 Managed filesystem projection"),
        ManagedContentDelivery::ManagedFilesystem,
        "M2/E1"
    );

    host.session_slots.update("managed-denied", |slot| {
        slot.session_dispatch = true;
        slot.tools = Some(deny_filesystem());
    });
    let denied = host
        .select_content_delivery("managed-denied", None, Some(&frozen))
        .expect_err("M3 must not construct the semantic Skill adapter");
    assert!(
        denied
            .to_string()
            .contains("require filesystem progressive disclosure"),
        "M3/E3: {denied}"
    );

    let empty_host = SharedHost::new(Arc::new(OkModel), "stub");
    empty_host.session_slots.update("managed-no-skill", |slot| {
        slot.session_dispatch = true;
        slot.tools = Some(deny_filesystem());
    });
    assert_eq!(
        empty_host
            .select_content_delivery("managed-no-skill", None, Some(&[]))
            .expect("M4 keeps semantic delivery available to non-Skill content"),
        ManagedContentDelivery::SemanticTools,
        "M4/E4"
    );

    empty_host
        .session_slots
        .update("managed-missing-freeze", |slot| {
            slot.session_dispatch = true;
        });
    let selected_snapshot =
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("managed-selected-skill")
            .model(test_model_binding())
            .agent_bindings(awaken_runtime_contract::agent_bindings::AgentBindings {
                skills: vec![awaken_agent_contract::AgentSkillBinding::custom("think")],
                ..Default::default()
            })
            .build();
    let missing = empty_host
        .select_content_delivery("managed-missing-freeze", Some(&selected_snapshot), None)
        .expect_err("M5 rejects an incomplete Managed projection");
    assert!(
        missing.to_string().contains("has no frozen version bytes"),
        "M5/E3: {missing}"
    );
}

#[test]
fn session_content_delivery_is_frozen_across_auxiliary_agent_snapshots() {
    // Content-delivery cause/effect graph: C1 the physical Session has no prior
    // choice; C2 it already chose filesystem and a deny-all Outcome grader is
    // projected; C3 it already chose semantic tools and a later Agent snapshot
    // exposes filesystem capability. Effects: E1 C1 derives exactly once; E2
    // C2/C3 reuse the existing Session choice without mutation or rejection.
    // Constraint K1 Agent tool capability still gates what that Agent may call;
    // reusing delivery neither widens tools nor creates a child-owned mount
    // authority. Rules CD1=C1=>E1; CD2=C2=>E2; CD3=C3=>E2.
    use crate::session_slot::ManagedContentDelivery;
    use awaken_agent_contract::{
        ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
        ToolsetSource,
    };

    let host = SharedHost::new(Arc::new(OkModel), "stub");
    let restricted = awaken_runtime_contract::ExecutableAgentSnapshot::builder("grader")
        .model(test_model_binding())
        .build();
    assert!(
        !host.session_allows_filesystem_tools("filesystem-session", Some(&restricted)),
        "CD2 auxiliary snapshot would independently derive semantic delivery"
    );
    host.session_slots.update("filesystem-session", |slot| {
        slot.content_delivery = Some(ManagedContentDelivery::ManagedFilesystem)
    });
    assert_eq!(
        host.select_content_delivery("filesystem-session", Some(&restricted), None)
            .expect("CD2 reuses physical Session delivery"),
        ManagedContentDelivery::ManagedFilesystem,
        "CD2/E2"
    );

    host.session_slots.update("semantic-session", |slot| {
        slot.content_delivery = Some(ManagedContentDelivery::SemanticTools);
        slot.tools = Some(awaken_session_contract::SessionToolConfiguration {
            toolsets: vec![ToolsetPolicy {
                source: ToolsetSource::Agent,
                default: ToolExecutionPolicy {
                    enabled: false,
                    permission: ToolPermissionRequirement::AlwaysAllow,
                },
                overrides: vec![ToolPolicyOverride::new(
                    "write",
                    ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAllow,
                    },
                )],
            }],
            client_tools: Vec::new(),
        });
    });
    assert!(
        host.session_allows_filesystem_tools("semantic-session", None),
        "CD3 later snapshot would independently derive filesystem delivery"
    );
    assert_eq!(
        host.select_content_delivery("semantic-session", None, None)
            .expect("CD3 reuses physical Session delivery"),
        ManagedContentDelivery::SemanticTools,
        "CD3/E2"
    );
}

#[test]
fn managed_tool_projection_has_one_role_and_session_override_decision_table() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use super::session::{
        ManagedCoordinationRole, SessionToolsetProjection, project_managed_coordination_surface,
        project_session_tool_override,
    };
    use awaken_agent_contract::{
        ClientToolDescriptor, ToolExecutionPolicy, ToolsetPolicy, ToolsetSource,
    };
    use awaken_runtime_contract::agent_bindings::{AgentAdvisorBinding, AgentDelegateBinding};
    use awaken_runtime_contract::resolved::{
        ADVISOR_TOOL_ID, ModelBinding, ResolvedModelCandidate, ToolDescriptor, ToolKind,
    };

    // Cause/effect graph: C1 native vs Managed root/child role selects exactly
    // one coordination surface; C2 generated/inherited vs immutable publication
    // selects whether Session toolsets enter the executable clone; C3 an explicit
    // Session client descriptor replaces published client ownership. Effects:
    // E1 Managed primary replaces native `agent_run` with fixed list/send;
    // E2 every Managed child loses nested delegation/advisor and receives no
    // coordination command; E3 generated/self-child embeds Session toolsets;
    // E4 published/non-self retains its publication toolsets; E5 explicit client
    // tools exact-replace. No obsolete Task descriptor participates in projection.
    //
    // | Rule | role | snapshot owner | Session overlay | Effects |
    // | M0 | native | published | preserve | agent_run only; no Managed/Task commands |
    // | M1 | primary | generated | project | E1,E3,E5 |
    // | M2 | primary | published | preserve | E1,E4,E5 |
    // | M3 | child | self/inherited | project | E2,E3,E5 |
    // | M4 | child | other Agent | absent | E2,E4 |
    let published_policy = ToolsetPolicy {
        source: ToolsetSource::Agent,
        default: ToolExecutionPolicy::default(),
        overrides: Vec::new(),
    };
    let session_policy = ToolsetPolicy {
        source: ToolsetSource::Mcp {
            server_name: "session-policy".into(),
        },
        default: ToolExecutionPolicy::default(),
        overrides: Vec::new(),
    };
    let published_client = ClientToolDescriptor {
        name: "published_client".into(),
        description: "published".into(),
        input_schema: serde_json::json!({"type": "object"}),
    };
    let session_client = ClientToolDescriptor {
        name: "session_client".into(),
        description: "session".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {"q": {"type": "string"}}}),
    };
    let tools = awaken_session_contract::SessionToolConfiguration {
        toolsets: vec![session_policy.clone()],
        client_tools: vec![session_client.clone()],
    };
    let builtin = |id: &str| {
        awaken_ext_builtin_tools::builtin_tools()
            .into_iter()
            .find(|tool| tool.descriptor().id == id)
            .expect("canonical builtin descriptor")
            .into_descriptor()
    };
    let native_ids = crate::config::advertised_tools(
        &HashSet::new(),
        &HashSet::from(["worker".to_string()]),
        &[],
    )
    .into_iter()
    .map(|descriptor| descriptor.id)
    .collect::<std::collections::BTreeSet<_>>();
    assert!(
        native_ids.contains(awaken_ext_builtin_tools::AGENT_RUN),
        "M0 native delegation"
    );
    for forbidden in [
        awaken_ext_builtin_tools::LIST_AGENTS,
        awaken_ext_builtin_tools::SEND_MESSAGE,
        "cancel_task",
        "recover_failed_messages",
    ] {
        assert!(
            !native_ids.contains(forbidden),
            "M0 forbids parallel command {forbidden}"
        );
    }
    let mut base = awaken_runtime_contract::ExecutableAgentSnapshot::builder("coordinator")
        .model(test_model_binding())
        .build();
    base.resolved_spec.tool_descriptors = vec![
        ToolDescriptor::pinned(
            "test",
            "ordinary",
            "ordinary",
            serde_json::json!({"type": "object"}),
        ),
        crate::config::session_client_tool_descriptor(&published_client),
        builtin(awaken_ext_builtin_tools::AGENT_RUN),
        builtin(awaken_ext_builtin_tools::LIST_AGENTS),
        builtin(awaken_ext_builtin_tools::SEND_MESSAGE),
        ToolDescriptor::pinned(
            "test",
            ADVISOR_TOOL_ID,
            "advisor",
            serde_json::json!({"type": "object"}),
        )
        .with_kind(ToolKind::Advisor),
    ];
    base.resolved_spec.plugin_config.agent.delegates = vec![AgentDelegateBinding {
        agent_id: awaken_runtime_contract::snapshot::AgentId("worker".into()),
        source_revision: Some(1),
        recursive_self: false,
    }];
    base.resolved_spec.plugin_config.agent.advisor = Some(AgentAdvisorBinding {
        model: "advisor-model".into(),
        candidate: ResolvedModelCandidate::host(ModelBinding::new(
            "provider",
            "advisor-model",
            "backend",
        )),
    });
    base.resolved_spec.plugin_config.agent.toolsets = vec![published_policy.clone()];

    let descriptor_ids = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot| {
        snapshot
            .resolved_spec
            .tool_descriptors
            .iter()
            .map(|descriptor| descriptor.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
    };
    let assert_primary = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
                          rule: &str| {
        let ids = descriptor_ids(snapshot);
        assert!(
            ids.contains(awaken_ext_builtin_tools::LIST_AGENTS),
            "{rule}/E1"
        );
        assert!(
            ids.contains(awaken_ext_builtin_tools::SEND_MESSAGE),
            "{rule}/E1"
        );
        assert_eq!(
            snapshot
                .resolved_spec
                .tool_descriptors
                .iter()
                .filter(|descriptor| descriptor.id == awaken_ext_builtin_tools::SEND_MESSAGE)
                .count(),
            1,
            "{rule}/E1 one Agent-message implementation"
        );
        assert!(
            !ids.contains(awaken_ext_builtin_tools::AGENT_RUN),
            "{rule}/E1"
        );
        assert!(
            ids.contains(ADVISOR_TOOL_ID),
            "{rule}/E1 advisor remains internal"
        );
    };
    let assert_child = |snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot, rule: &str| {
        let ids = descriptor_ids(snapshot);
        for forbidden in [
            awaken_ext_builtin_tools::AGENT_RUN,
            awaken_ext_builtin_tools::LIST_AGENTS,
            awaken_ext_builtin_tools::SEND_MESSAGE,
            ADVISOR_TOOL_ID,
        ] {
            assert!(!ids.contains(forbidden), "{rule}/E2 forbids {forbidden}");
        }
        assert!(
            snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegates
                .is_empty(),
            "{rule}/E2"
        );
        assert!(
            snapshot.resolved_spec.plugin_config.agent.advisor.is_none(),
            "{rule}/E2"
        );
    };

    let mut generated_primary = base.clone();
    project_session_tool_override(
        &mut generated_primary,
        &tools,
        SessionToolsetProjection::ProjectIntoSnapshot,
    );
    project_managed_coordination_surface(&mut generated_primary, ManagedCoordinationRole::Primary);
    assert_primary(&generated_primary, "M1");
    assert_eq!(
        generated_primary
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .as_slice(),
        std::slice::from_ref(&session_policy),
        "M1/E3"
    );

    let mut published_primary = base.clone();
    project_session_tool_override(
        &mut published_primary,
        &tools,
        SessionToolsetProjection::PreservePublished,
    );
    project_managed_coordination_surface(&mut published_primary, ManagedCoordinationRole::Primary);
    assert_primary(&published_primary, "M2");
    assert_eq!(
        published_primary
            .resolved_spec
            .plugin_config
            .agent
            .toolsets
            .as_slice(),
        std::slice::from_ref(&published_policy),
        "M2/E4"
    );

    for (rule, snapshot) in [("M1", &generated_primary), ("M2", &published_primary)] {
        let clients = snapshot
            .resolved_spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
            .collect::<Vec<_>>();
        assert_eq!(clients.len(), 1, "{rule}/E5");
        assert_eq!(clients[0].id, session_client.name, "{rule}/E5");
    }

    let mut inherited_child = base.clone();
    project_session_tool_override(
        &mut inherited_child,
        &tools,
        SessionToolsetProjection::ProjectIntoSnapshot,
    );
    project_managed_coordination_surface(&mut inherited_child, ManagedCoordinationRole::Child);
    assert_child(&inherited_child, "M3");
    assert_eq!(
        inherited_child.resolved_spec.plugin_config.agent.toolsets,
        [session_policy],
        "M3/E3"
    );
    assert_eq!(
        inherited_child
            .resolved_spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
            .map(|descriptor| descriptor.id.as_str())
            .collect::<Vec<_>>(),
        [session_client.name.as_str()],
        "M3/E5"
    );

    let mut other_child = base;
    project_managed_coordination_surface(&mut other_child, ManagedCoordinationRole::Child);
    assert_child(&other_child, "M4");
    assert_eq!(
        other_child.resolved_spec.plugin_config.agent.toolsets,
        [published_policy],
        "M4/E4"
    );
    assert_eq!(
        other_child
            .resolved_spec
            .tool_descriptors
            .iter()
            .filter(|descriptor| descriptor.kind == ToolKind::ClientExecuted)
            .map(|descriptor| descriptor.id.as_str())
            .collect::<Vec<_>>(),
        [published_client.name.as_str()],
        "M4/E4"
    );
}

#[tokio::test]
async fn session_client_tools_replace_the_published_surface_with_exact_ownership() {
    // Cause/effect graph: C1 no Session override inherits the immutable
    // publication; C2 an explicit empty override clears published client tools;
    // C3 a non-empty override supplies an exact descriptor. Effects: E1 the
    // publication object remains unchanged; E2 the execution clone exposes only
    // the Session descriptor with its exact schema; E3 a model call awaits a
    // client result rather than built-in approval.
    //
    // | Rule | Session client tools | execution surface | pending owner |
    // | R1 | None | published tools | published owner |
    // | R2 | Some([]) | no client tools | none |
    // | R3 | Some([lookup]) | exact lookup schema | protocol client |
    // R1 is covered by ordinary published-Agent tests; this regression owns R2
    // and R3, including the real classification boundary that Managed events use.
    let publication = crate::config::server_config(
        "assistant",
        "stub",
        &HashSet::from(["published_lookup".to_string()]),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let publications =
        awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([publication.clone()])
            .expect("valid publication");
    let (host, _managed) = dispatch_test_host(
        SharedHost::new(Arc::new(ClientLookupModel), "stub")
            .with_agent_publications(Arc::new(publications)),
    );

    host.session_slots.update("cleared-client-tools", |slot| {
        slot.tools = Some(awaken_session_contract::SessionToolConfiguration::default());
    });
    let cleared = host
        .ctx_for("cleared-client-tools", Some("assistant"))
        .await
        .expect("R2 build explicit empty Session tool surface");
    assert!(
        cleared
            .config
            .resolved_spec
            .tool_descriptors
            .iter()
            .all(|tool| tool.kind != awaken_runtime_contract::resolved::ToolKind::ClientExecuted),
        "R2/E2"
    );

    let lookup = awaken_agent_contract::ClientToolDescriptor {
        name: "lookup".into(),
        description: "Look up the requested city".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
            "additionalProperties": false
        }),
    };
    host.session_slots.update("session-client-tools", |slot| {
        slot.tools = Some(awaken_session_contract::SessionToolConfiguration {
            toolsets: Vec::new(),
            client_tools: vec![lookup.clone()],
        });
    });
    let context = host
        .ctx_for("session-client-tools", Some("assistant"))
        .await
        .expect("R3 build exact Session tool surface");
    let client_tools = context
        .config
        .resolved_spec
        .tool_descriptors
        .iter()
        .filter(|tool| tool.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted)
        .collect::<Vec<_>>();
    assert_eq!(client_tools.len(), 1, "R3/E2");
    assert_eq!(client_tools[0].id, lookup.name, "R3/E2");
    assert_eq!(client_tools[0].description, lookup.description, "R3/E2");
    assert_eq!(
        client_tools[0].model_parameters(),
        lookup.input_schema,
        "R3/E2"
    );
    assert!(
        publication
            .resolved_spec
            .tool_descriptors
            .iter()
            .any(|tool| {
                tool.id == "published_lookup"
                    && tool.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted
            }),
        "R2+R3/E1"
    );

    let result = host
        .run(
            Some("assistant"),
            "session-client-tools",
            user("look it up"),
        )
        .await
        .expect("R3 model calls Session client tool");
    let pending = result.pending.expect("R3 awaits lookup result");
    assert_eq!(pending.name, "lookup", "R3/E3");
    assert!(pending.client_executed, "R3/E3");
}

#[tokio::test]
async fn cold_session_uses_its_frozen_agent_projection_for_internal_history_reads() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Causes: C1 a cold Session has a frozen non-default Agent projection and an
    // internal history read carries no repeated Agent argument; C2 the caller
    // repeats the same Agent; C3 it asserts a different Agent. Effects: E1/E2
    // resolve the exact publication; E3 fail closed before constructing Runtime.
    //
    // Decision table:
    // | rule | projected Agent | requested Agent | result                  |
    // | R1   | agent-a         | absent          | exact agent-a snapshot  |
    // | R2   | agent-a         | agent-a         | exact agent-a snapshot  |
    // | R3   | agent-a         | assistant       | projection mismatch     |
    let snapshot = crate::config::server_config(
        "agent-a",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications)),
    );
    install_test_session_application(&host);
    install_test_dispatch_runtime(&host)
        .install_test_session_init(
            "cold-agent",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: Some("stub".into()),
                runtime: Some("default".into()),
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("project the frozen Session baseline");

    let recovered = host
        .ctx_for("cold-agent", None)
        .await
        .expect("R1 internal history read resolves the frozen Agent");
    assert_eq!(recovered.config.root_agent_id.0, "agent-a", "R1");

    host.register_thread_agent_projection("same-agent", "agent-a");
    host.ctx_for("same-agent", Some("agent-a"))
        .await
        .expect("R2 repeated exact Agent");

    host.register_thread_agent_projection("mismatch-agent", "agent-a");
    let error = match host.ctx_for("mismatch-agent", Some("assistant")).await {
        Ok(_) => panic!("R3 accepted a different requested Agent"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("projection"), "R3");
}

#[tokio::test]
async fn frozen_session_resolves_its_exact_agent_revision_instead_of_current() {
    // Cause/effect graph: C1 the publication catalog retains exact revision 2;
    // C2 revision 3 is current; C3 the Session baseline is frozen at revision 2.
    // The Run activation must use revision 2. Resolving current would create a
    // second publication authority and make the Worker reject the Session.
    //
    // | Rule | frozen revision | current revision | effect |
    // |---|---:|---:|---|
    // | R1 | 2 | 3 | resolve exact revision 2 |
    // | R2 | absent | 3 | retain legacy current lookup |
    fn publication(revision: u64, instructions: &str) -> ExecutableAgentSnapshot {
        let mut snapshot = crate::config::server_config(
            "agent-a",
            "stub",
            &HashSet::new(),
            &HashSet::new(),
            &[],
            &Default::default(),
            &[],
            awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
        );
        snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
        snapshot.metadata.source.revision = revision;
        snapshot.resolved_spec.instructions = instructions.into();
        snapshot.recompute_fingerprint().unwrap();
        snapshot
    }

    let frozen = publication(2, "frozen");
    let current = publication(3, "current");
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([
        frozen.clone(),
        current.clone(),
    ])
    .expect("valid revisioned publications");
    let host =
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications));
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment: on_tool_use_environment(),
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "agent-a".into(),
            agent_revision: Some(2),
            model: "stub".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    host.install_frozen_session_projection(
        "frozen-revision",
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: host.local_workspace().to_string(),
            revision: awaken_session_contract::SessionRevision(1),
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            previous_resource_manifest: Some(
                awaken_session_contract::SessionResourceManifest::new(
                    host.local_workspace(),
                    awaken_session_contract::ResolvedSessionResources::default(),
                ),
            ),
            tools: Default::default(),
            mcp: Vec::new(),
            request_context: Vec::new(),
            agent_publication: None,
            baseline,
        },
        None,
        true,
        None,
    )
    .await
    .expect("frozen baseline");

    let (_, _, selected, _) = host
        .resolve_session_publication("frozen-revision", None, None)
        .expect("R1 exact frozen publication");
    assert_eq!(selected, Some(frozen), "R1");

    let (_, _, selected, _) = host
        .resolve_session_publication("legacy-current", Some("agent-a"), None)
        .expect("R2 legacy current publication");
    assert_eq!(selected, Some(current), "R2");
}

#[tokio::test]
async fn frozen_projection_replaces_an_inactive_default_runtime_context() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a durable-thread operation may open a context before
    // the frozen projection is installed; C2 that context is inactive or active;
    // C3 the later projection selects the default or a published non-default
    // Agent. Effects: E1 inactive cached defaults are discarded and rebuilt from
    // the frozen Agent; E2 an active activation is never rebound; E3 an already
    // prepared context remains rebuildable from the same immutable facts.
    //
    // | Rule | resident context | active run | frozen Agent | effect |
    // |---|---|---|---|---|
    // | P1 | default | no | published agent-a | evict; rebuild agent-a |
    // | P2 | default | yes | agent-a | reject projection install |
    // | P3 | absent/matching | no | same baseline | install/rebuild safely |
    // | P4 | frozen matching | yes | same baseline | reuse; successor queues |
    //
    // P1 and P2 are the distributed authority-transition regressions exercised
    // here. P3 is covered by
    // `cold_session_uses_its_frozen_agent_projection_for_internal_history_reads`
    // and the idempotent projection tests above. P4 prevents an approval event
    // racing the preceding Run's terminal cleanup from rebinding or rejecting
    // the already-frozen Runtime. FMECA: rejecting P4 loses the approved input;
    // rebuilding it risks changing Hand/MCP effects under an active run.
    let snapshot = crate::config::server_config(
        "agent-a",
        "stub",
        &HashSet::new(),
        &HashSet::new(),
        &[],
        &Default::default(),
        &[],
        awaken_runtime_contract::resolved::ContextPolicy::KeepAll,
    );
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("valid publication");
    let mut host =
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications));
    host.deployment.disable_local_pool = true;
    let host = Arc::new(host);
    install_test_session_application(&host);

    let stale = host
        .ctx_for("late-projection", None)
        .await
        .expect("pre-projection durable operation can open a default context");
    assert_eq!(stale.config.root_agent_id.0, "assistant", "P1 precondition");

    crate::ManagedHost::new(host.clone())
        .install_test_session_init(
            "late-projection",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: Some("stub".into()),
                runtime: Some("default".into()),
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect("P1 installs the frozen projection");
    assert!(
        host.session_slots
            .read("late-projection", |slot| slot.runtime.is_none())
            .unwrap_or(false),
        "P1 stale context must not survive the authority transition"
    );

    let rebuilt = host
        .ctx_for("late-projection", None)
        .await
        .expect("P1 rebuilds from the frozen publication");
    assert_eq!(rebuilt.config.root_agent_id.0, "agent-a", "P1/E1");

    let active = host
        .ctx_for("active-projection", None)
        .await
        .expect("P2 pre-projection context");
    *active.active_run.lock().expect("active run mutex") = Some(RunId("active-run".into()));
    let error = crate::ManagedHost::new(host.clone())
        .install_test_session_init(
            "active-projection",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 0,
                resources: Default::default(),
                model: Some("stub".into()),
                runtime: Some("default".into()),
                environment: on_tool_use_environment(),
            },
        )
        .await
        .expect_err("P2 must not rebind an active Runtime");
    assert!(
        error.to_string().contains("while its Runtime is active"),
        "P2/E2"
    );
    assert!(
        host.thread_agent_projection("active-projection").is_none(),
        "P2 rejection precedes every projection mutation"
    );

    let frozen_init = awaken_session_contract::SessionInit {
        workspace_id: host.local_workspace().into(),
        agent_id: "agent-a".into(),
        delegate_ids: Vec::new(),
        tools: None,
        resource_revision: 0,
        resources: Default::default(),
        model: Some("stub".into()),
        runtime: Some("default".into()),
        environment: on_tool_use_environment(),
    };
    crate::ManagedHost::new(host.clone())
        .install_test_session_init("active-frozen-projection", frozen_init.clone())
        .await
        .expect("P4 initial frozen coordinates");
    let frozen_active = host
        .ctx_for("active-frozen-projection", None)
        .await
        .expect("P4 context built after frozen authority");
    *frozen_active.active_run.lock().expect("active run mutex") =
        Some(RunId("frozen-active-run".into()));
    crate::ManagedHost::new(host.clone())
        .install_test_session_init("active-frozen-projection", frozen_init)
        .await
        .expect("P4 reuses the immutable active projection");
    let retained = host
        .session_slots
        .read("active-frozen-projection", |slot| {
            Arc::ptr_eq(slot.runtime.as_ref().expect("P4 runtime"), &frozen_active)
        })
        .unwrap_or(false);
    assert!(retained, "P4 keeps the one resident Runtime");
}

#[tokio::test]
async fn live_inbox_is_advertised_only_for_a_locally_reachable_active_attempt() {
    // FMECA cause/effect graph: C1 a direct or durable-local physical attempt has
    // opened its exact scope; C2 the scope supports safe-boundary live input; C3
    // the exact scope has settled; C4 a Coordinator-only context exists. Effects:
    // E1 C1+C2 exposes that scope's process-local inbox; E2 no current scope stays
    // inactive; E3 settled and remote attempts fail closed.
    // Constraint: Runtime's generation/ownership-fenced active-attempt registry is
    // the sole discovery and lifecycle authority; no Session/ingress queue exists.
    //
    // | Rule | topology | attempt scope | effect |
    // |---|---|---|---|
    // | L1 | direct idle | absent | E2 inactive |
    // | L2 | direct Runtime | current | E1 exact inbox |
    // | L3 | direct settled | removed/closed | E2+E3 inactive |
    // | L4 | pool-owned Event | current | E1 exact inbox |
    // | L5 | Event settled | removed/closed | E3 inactive |
    // | L6 | durable local Worker | current | E1 exact inbox |
    // | L7 | Coordinator-only | absent | E3 inactive |
    // Decision rule: execute L1-L7; only a current registry entry may produce E1.
    let direct = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let _direct_managed = install_test_dispatch_runtime(&direct);
    let direct_ctx = direct
        .ctx_for("direct-live", None)
        .await
        .expect("L1 direct context");
    assert!(
        !direct_ctx.delivery.is_durable(),
        "L1 direct foreground precondition"
    );
    assert!(direct.live_inbox("direct-live").await.is_none(), "L1/E2");

    let direct_tracking = direct_ctx.runtime.begin_active_attempt(
        &RunId("run-direct".into()),
        &direct_ctx.thread_id,
        awaken_runtime_contract::RuntimeRunContext::new(),
        awaken_runtime_contract::execution::LiveInput::SafeBoundary,
    );
    assert!(direct.live_inbox("direct-live").await.is_some(), "L2/E1");
    drop(direct_tracking);
    assert!(
        direct.live_inbox("direct-live").await.is_none(),
        "L3/E2+E3 a settled scope is not a fallback authority"
    );

    let event_tracking = direct_ctx.runtime.begin_active_attempt(
        &RunId("run-session-event".into()),
        &direct_ctx.thread_id,
        awaken_runtime_contract::RuntimeRunContext::new(),
        awaken_runtime_contract::execution::LiveInput::SafeBoundary,
    );
    let event_inbox = event_tracking
        .context()
        .live_inbox
        .clone()
        .expect("L4 event attempt inbox");
    let discovered = direct
        .live_inbox("direct-live")
        .await
        .expect("L4/E1 pool-owned Session Event inbox");
    let _ = discovered.offer(Message::text(
        MessageId("event-live-message".into()),
        Role::User,
        "event",
    ));
    assert_eq!(event_inbox.list().len(), 1, "L4/E1 exact Event inbox");
    drop(event_tracking);
    assert!(direct.live_inbox("direct-live").await.is_none(), "L5/E3");

    let mut local_deployment = crate::DeploymentConfig::ephemeral();
    local_deployment.durable = true;
    let local = Arc::new(SharedHost::new_with_deployment(
        Arc::new(OkModel),
        "stub",
        local_deployment,
    ));
    let _local_managed = install_test_dispatch_runtime(&local);
    let local_ctx = local.ctx_for("local-live", None).await.expect("L6 context");
    *local_ctx.active_run.lock().expect("active run mutex") = Some(RunId("run-local".into()));
    let tracking = local_ctx.runtime.begin_active_attempt(
        &RunId("run-local".into()),
        &local_ctx.thread_id,
        awaken_runtime_contract::RuntimeRunContext::new(),
        awaken_runtime_contract::execution::LiveInput::SafeBoundary,
    );
    assert!(local.live_inbox("local-live").await.is_some(), "L6/E1");
    drop(tracking);
    assert!(
        local.live_inbox("local-live").await.is_none(),
        "L6/E3 settled"
    );

    let mut remote_deployment = crate::DeploymentConfig::ephemeral();
    remote_deployment.durable = true;
    remote_deployment.disable_local_pool = true;
    let remote = Arc::new(SharedHost::new_with_deployment(
        Arc::new(OkModel),
        "stub",
        remote_deployment,
    ));
    let remote_ctx = remote
        .ctx_for("remote-live", None)
        .await
        .expect("L7 context");
    *remote_ctx.active_run.lock().expect("active run mutex") = Some(RunId("run-remote".into()));
    assert!(remote.live_inbox("remote-live").await.is_none(), "L7/E3");
}

/// FMECA: FM1 explicit warmup and Session creation use separate preparation
/// registries, causing duplicate work or different readiness truth; FM2 Session
/// creation reaches the provider before cache preparation succeeds.
/// Cause/effect graph:
/// C1=Session spec declares CacheVolume, C2=no eager preparation exists,
/// C3=initializer succeeds, C4=the same identity is explicitly prewarmed later.
/// E1=initialization precedes provider creation, E2=Session creation succeeds,
/// E3=explicit and implicit entry points reuse one successful preparation.
/// Decision rules: (C1,C2,C3)->(E1,E2); (C1,C3,C4)->E3.
#[tokio::test]
async fn session_creation_and_explicit_cache_warmup_share_one_preparation_path() {
    struct RecordingInitializer(AtomicUsize);

    #[async_trait::async_trait]
    impl crate::CacheVolumeInitializer for RecordingInitializer {
        async fn initialize(&self, _volume: &crate::CacheVolumeWarmup) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let initializer = Arc::new(RecordingInitializer(AtomicUsize::new(0)));
    let host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_cache_volume_initializer(initializer.clone());
    let mut spec = crate::provisioning::agent_run_sandbox_spec("cache-wiring");
    spec.mounts
        .push(awaken_provisioning_contract::MountRequirement {
            mount_id: "build-cache".into(),
            source: awaken_provisioning_contract::MountSource::CacheVolume {
                location: awaken_provisioning_contract::CacheVolumeLocation::HostPath {
                    path: "/tmp/awaken-cache-volume-wiring".into(),
                },
                key: "build-cache-v1".into(),
            },
            mount_path: "cache-placeholder".into(),
            access: awaken_provisioning_contract::MountAccess::ReadWrite,
            lifetime: awaken_provisioning_contract::MountLifetime::Durable,
            // Workdir cannot bind a host directory; the wiring fixture keeps the
            // provider effect optional. Real bind behavior is covered by the
            // container/namespace substrate tests.
            required: false,
        });

    let environment = host
        .create_session_environment(&host.session_provider, &spec)
        .await
        .expect("cache preparation precedes Session environment creation");
    assert_eq!(initializer.0.load(Ordering::SeqCst), 1, "E1/E2");

    host.prewarm_cache_volume(crate::CacheVolumeWarmup::host_path(
        "build-cache-v1",
        "/tmp/awaken-cache-volume-wiring",
    ))
    .await
    .expect("same identity is already prepared");
    assert_eq!(initializer.0.load(Ordering::SeqCst), 1, "E3");
    environment
        .dispose()
        .await
        .expect("dispose fixture environment");
}

/// Final-layout/prewarm decision rule: P1 a staged Repository owns
/// `/workspace/repo`; P2 the effective SandboxSpec adds a CacheVolume below that
/// tree. P1+P2 => reject before the shared CacheVolume initializer or provider
/// create edge. This proves the call ordering in addition to the contract's pure
/// overlap matrix.
#[tokio::test]
async fn repository_layout_conflict_fails_before_cache_prewarm() {
    struct CountingInitializer(AtomicUsize);

    #[async_trait::async_trait]
    impl crate::CacheVolumeInitializer for CountingInitializer {
        async fn initialize(&self, _volume: &crate::CacheVolumeWarmup) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let initializer = Arc::new(CountingInitializer(AtomicUsize::new(0)));
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_cache_volume_initializer(initializer.clone()),
    );
    let managed = managed_with_resource_source(host.clone());
    let mut init = bare_session("agent", host.local_workspace());
    init.resources = effective_resources(vec![TestInput {
        kind: "github_repository".into(),
        id: "https://github.com/awaken/prewarm-fence.git".into(),
        mount_path: "/workspace/repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);
    managed
        .install_test_session_init("repository-prewarm-fence", init)
        .await
        .expect("P1 stage Repository without opening an Environment");

    let mut spec = host.sandbox_spec("repository-prewarm-fence");
    spec.mounts
        .push(awaken_provisioning_contract::MountRequirement {
            mount_id: "conflicting-cache".into(),
            source: awaken_provisioning_contract::MountSource::CacheVolume {
                location: awaken_provisioning_contract::CacheVolumeLocation::HostPath {
                    path: "/tmp/awaken-conflicting-cache".into(),
                },
                key: "conflicting-cache-v1".into(),
            },
            mount_path: "/workspace/repo/cache".into(),
            access: awaken_provisioning_contract::MountAccess::ReadWrite,
            lifetime: awaken_provisioning_contract::MountLifetime::Durable,
            required: false,
        });
    let error = match host
        .create_session_environment(&host.session_provider, &spec)
        .await
    {
        Ok(_) => panic!("P2 must fail before cache preparation"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("overlaps"), "P1/P2: {error}");
    assert_eq!(
        initializer.0.load(Ordering::SeqCst),
        0,
        "P1/P2 no prewarm effect"
    );
}
