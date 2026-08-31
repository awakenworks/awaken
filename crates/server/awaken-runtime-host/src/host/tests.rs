use super::*;
use crate::config::block_text;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
use awaken_session_contract::SessionRuntime;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

fn test_model_binding() -> awaken_runtime_contract::resolved::ModelBinding {
    awaken_runtime_contract::resolved::ModelBinding::new("test", "model", "native")
}

/// Approval-state tests name their precondition explicitly. Managed Agent
/// members default to always-allow; this exact test override asks only for
/// `write` while leaving unrelated follow-up effects unchanged.
fn host_requiring_write_confirmation(model: Arc<dyn LlmExecutor>) -> SharedHost {
    let toolsets = [crate::config::test_agent_toolset_permission(
        "write",
        awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
    )];
    let policy = awaken_ext_permission::RuleBasedToolPermissionPolicy::new(
        crate::config::effective_ruleset_with_toolsets(None, &[], &toolsets),
    );
    SharedHost::new(model, "stub").with_gate_override(Arc::new(
        awaken_runtime::PermissionGate::new(Arc::new(policy)),
    ))
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
    managed.finish_step(thread, result).await
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

/// One test fixture for the production SkillVersion authority. Callers vary
/// only identity/body/supporting files, so Managed tests cannot accidentally
/// reintroduce host-static SkillSpec setup as a parallel source.
fn frozen_skill_version(
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
        awaken_provisioning_contract::SandboxError,
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

struct TestMemoryMount;

#[async_trait::async_trait]
impl awaken_provisioning_contract::MemoryMount for TestMemoryMount {
    fn realization(&self) -> awaken_provisioning_contract::Realization {
        awaken_provisioning_contract::Realization::Copy
    }

    async fn teardown(self: Box<Self>) {}
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
        Ok(Box::new(TestMemoryMount))
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

    // Drive an outcome whose rubric is never met, so it would loop; the model
    // blocks it in the second Worker Run (after the first Judge Run).
    let driver = host.clone();
    let task = tokio::spawn(async move { driver.define_outcome("t1", "finish", "FINAL", 5).await });

    // Once the loop is blocked mid-run, interrupt it, then release the gate.
    reached.notified().await;
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
    // Cause/effect graph: C1 max_iterations=1 and a needs_revision Grade enter
    // the stable acknowledgment Run; C2 that Run owns the active cancellation
    // slot; C3 user.interrupt lands while its model request is blocked. Effects:
    // E1 the existing Host cancellation path ends the Outcome as interrupted;
    // E2 the one graded cycle remains iteration 0; E3 its public terminal is
    // interrupted, with neither max_iterations_reached nor an invented cycle 1.
    //
    // | Rule | At cap | Ack active | Interrupt | Public terminal          |
    // | R1   | yes    | yes        | yes       | iteration 0 interrupted  |
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = Arc::new(SharedHost::new(model, "scripted"));

    let driver = host.clone();
    let task = tokio::spawn(async move {
        driver
            .define_outcome("ack-interrupt", "finish", "FINAL", 1)
            .await
    });

    reached.notified().await;
    host.interrupt("ack-interrupt").await.expect("interrupt");
    gate.notify_one();

    let report = completed_outcome(task.await.expect("join").expect("define_outcome"));
    assert_eq!(report.iterations.len(), 1, "R1/E2-E3");
    assert_eq!(report.iterations[0].iteration, 0, "R1/E2");
    assert_eq!(report.iterations[0].result, "interrupted", "R1/E1+E3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_authority_loss_interrupts_active_session_before_revocation() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let reached = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(GatedModel {
        gate: gate.clone(),
        reached: reached.clone(),
        calls: AtomicUsize::new(0),
    });
    let host = Arc::new(SharedHost::new(model, "scripted"));

    let driver = host.clone();
    let task = tokio::spawn(async move {
        driver
            .define_outcome("authority-loss-active", "finish", "FINAL", 5)
            .await
    });

    reached.notified().await;
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
    let host = SharedHost::new_with_deployment(Arc::new(MemoryHostModel), "stub", deployment)
        .with_capture_sink(captured.clone())
        .with_data_subject_consent_source(Arc::new(SubjectConsent));

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

    let host = SharedHost::new(Arc::new(MemoryHostModel), "stub");
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
    let host = SharedHost::new(Arc::new(MemoryHostModel), "stub").with_attempt_decorator(Arc::new(
        move |inner| {
            Arc::new(ObservingAttemptExecutor {
                calls: decorator_calls.clone(),
                inner,
            })
        },
    ));
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

    // Co-located Native baseline installation has the same immutable binding
    // rules as the claimed Worker projection. Decision table:
    // B1 valid first install -> accept; B2 identical replay -> idempotent;
    // B3 empty fingerprint -> reject; B4 different fingerprint -> reject;
    // B5 Environment already realized -> reject rather than run without the
    // frozen mounts/env/prompts.
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

    let recorder = PromptRecorder::default();
    let observed = recorder.0.clone();
    let host = Arc::new(SharedHost::new(Arc::new(recorder), "stub"));
    let repository_claims = Arc::new(RepositoryClaimRecorder::default());
    let managed = crate::ManagedHost::new(host.clone())
        .with_repository_binding_verifier(repository_claims.clone())
        .install_dispatch_session_runtime();

    // Complete-install and unattempted Resource-amendment cause/effect table.
    // C0 the application calls the sole complete-projection port in Dispatch
    // mode; C1 a complete frozen dispatch projection already names generation
    // 7; C2 the Session aggregate
    // dispatch projection already names generation 7; C2 the Session aggregate
    // amends its still-unattempted desired content at generation 7; C3 the
    // caller is the Coordinator Dispatch installer or a claimed Worker.
    // Effects: E0 the one call publishes baseline, Resource manifest, and the
    // Managed execution marker together (there is no second SessionInit port);
    // E1 Coordinator replaces its disposable projection and preserves
    // generation 7; E2 Worker rejects the same content change and leaves the
    // old projection intact. A newer generation and exact replay remain covered
    // by the contract decision table.
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
                    && slot.manifest.as_ref().is_some_and(|manifest| {
                        manifest.revision == initial_dispatch.resource_revision
                    })
            })
            .unwrap_or(false),
        "A0/E0 complete projection is prepared by one port call"
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
            .contains("cannot replace the active Session Resource generation"),
        "A1/E2 claimed Worker remains fenced"
    );
    assert_eq!(
        amendment_host
            .thread_resource_manifest("authority-amendment")
            .expect("A1 retained projection")
            .resources,
        awaken_session_contract::ResolvedSessionResources::default(),
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
    assert_eq!(
        amendment_host
            .thread_resource_manifest("authority-amendment")
            .expect("A2 amended projection"),
        awaken_session_contract::SessionResourceManifest::at_revision(
            amended_dispatch.workspace_id,
            amended_dispatch.resource_revision,
            amended_dispatch.resources,
        ),
        "A2/E1 exact same-generation content is projected"
    );

    let frozen = projection("Use the bound Flow project.", true);
    host.install_frozen_session_projection("flow-thread", frozen.clone(), None, true, None)
        .await
        .expect("first frozen projection installs");
    host.install_frozen_session_projection("flow-thread", frozen, None, true, None)
        .await
        .expect("same frozen fingerprint is idempotent");

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
    let committed_environment = Arc::new(Mutex::new(None));
    let branch_sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&branch_host),
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

    // Frozen-projection Resource-generation cause/effect decision table.
    // C1=projection has an explicit non-legacy Resource generation;
    // C2=resources are non-default and must be installed. E1=the Runtime's
    // canonical manifest preserves that exact generation; E2=it never silently
    // falls back to generation zero. R1 C1+C2=>E1,E2.
    assert_eq!(
        host.thread_resource_manifest("flow-thread")
            .expect("frozen resources installed")
            .revision,
        7,
        "R1 preserves the SessionResourceState generation"
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
    host.run(None, "prompt-thread", user("P2"))
        .await
        .expect("P2");
    host.run(None, "prompt-thread", user("P4"))
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
    host.run(
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
        let prompt_count = |request: &ChatRequest, prompt: &str| {
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
    host.run(None, "context-thread", user("current branch input"))
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

    let replacement = projection("different", true);
    assert!(
        host.install_frozen_session_projection("flow-thread", replacement, None, true, None)
            .await
            .is_err(),
        "a bound Session cannot switch frozen baselines"
    );

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

#[tokio::test]
async fn tool_bearing_snapshot_can_be_restricted_at_the_run_boundary() {
    use crate::run_exec::BoundRunExecutor;
    use awaken_runtime_contract::execution::RunExecutor;
    use awaken_runtime_contract::resolved::ToolDescriptor;

    let host = SharedHost::new(Arc::new(MemoryHostModel), "stub");
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
    // Test design. Causes: C1 a Managed Run is blocked in inference; C2 interrupt
    // is accepted before inference returns. Effects: E1 C2 ends the exact Run as
    // Cancelled rather than committing the late model reply. Constraint/Invariant:
    // interruption targets the active Run generation only. Decision rule: block,
    // interrupt, release inference, and require E1.
    let reached = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let host = Arc::new(SharedHost::new(
        Arc::new(BlockOnceModel {
            reached: reached.clone(),
            gate: gate.clone(),
        }),
        "scripted",
    ));

    let driver = host.clone();
    let task = tokio::spawn(async move { driver.run(None, "t-int", user("go")).await });

    // The Run is blocked mid-inference; interrupt it, then release the gate.
    reached.notified().await;
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
    // Cause/effect: C1 is a durable Run blocked inside Provider inference; C2 is
    // an interrupt accepted by the Session control edge. E1 is that C2 returns
    // while C1 remains blocked; E2 is the existing pool drainer committing one
    // Cancelled terminal fact under the new claim epoch. Constraint: the caller
    // may wake the pool but must never become a synchronous dispatch driver.
    //
    // | Rule | durable Run | Provider | interrupt | Effects |
    // |---|---|---|---|---|
    // | R1 | active | blocked | accepted | E1 + eventual E2 |
    // | R2 | absent | n/a | replay/no-op | immediate success (sibling test) |
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
    host.ensure_dispatch_pool();

    let driver = host.clone();
    let run = tokio::spawn(async move { driver.run(None, "durable-interrupt", user("go")).await });
    reached.notified().await;

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
    let managed = crate::ManagedHost::new(host.clone());
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
    // reservation caller evicts it. Effects: E1 execution replaces that context;
    // E2 the replacement owns a physical Environment, which is the prerequisite
    // for registering the exact ACP executor. Decision table: ACP+C1-C3=>E1+E2;
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
    let host = SharedHost::new(Arc::new(OkModel), "stub")
        .with_agent_publications(Arc::new(publications))
        .with_acp(Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(
            source,
        )));

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
        ctx.durable_ingress.is_none()
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
    // Test design. Causes: C1 the primary Session has no local run/cancel hint;
    // C2 exactly one executable durable dispatch is active; C3 a remote attempt
    // owns it. Effects: E1 interrupt selects C2, persists cancellation, invokes
    // remote cancel, and settles Cancelled. Constraint/Invariant: selection uses
    // durable dispatch truth, never guessed process state. Decision rule: unique
    // C2+C3 yields E1; ambiguity is owned by the fail-closed sibling test.
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
    let host = SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction_tokens(1, 1);
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
    let host = SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction_tokens(100, 1);
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
        .ctx_for_snapshot_with_sandbox(
            "durable-compact-config",
            Some("assistant"),
            Some(provisional.config.clone()),
            None,
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
    let publications = awaken_runtime_contract::StaticPublishedAgentSnapshots::try_new([snapshot])
        .expect("one immutable published Agent");
    let host =
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications));

    let outcome = host
        .run(
            Some("published-compact"),
            "published-compact-thread",
            vec![Message::text(
                MessageId("published-compact-input".into()),
                Role::User,
                "prepare the handoff",
            )],
        )
        .await
        .expect("publication-selected compact plugin runs without ambient enablement");

    assert_eq!(outcome.state, RunState::Ended(EndCause::NaturalEnd));
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
    let host = SharedHost::new(Arc::new(MemLoopModel), "stub");
    let store = test_memory_store_id();
    bind_test_memory(&host, "thread-1", &store, true);
    bind_test_memory(&host, "thread-2", &store, true);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // Thread 1: the user states a preference; extraction saves it.
    host.run(None, "thread-1", user("I really enjoy tea in the morning"))
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
    let r = host
        .run(None, "thread-2", user("What beverage do I prefer?"))
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
    // intent is absent; C3 the replacement Host uses DirectRunIngress. Effects:
    // E1 cold context construction redelivers the terminal exactly once; E2 the
    // completed extraction survives Host replacement. Constraint K1: only Direct
    // ingress owns this cold self-heal; durable ingress is covered at the guarded
    // settlement boundary. Decision rule D1=C1+C2+C3 => E1+E2. The test injects
    // authority because runtime-host no longer opens a commit Store.
    let first = SharedHost::new(Arc::new(MemoryHostModel), "stub")
        .with_store_dir(&dir)
        .with_runtime_authority(authority.clone());
    first
        .run(None, thread, user("remember rust"))
        .await
        .expect("terminal run");
    drop(first);

    // Rebind the frozen resource and reopen the committed thread. Context recovery
    // derives the missing outbox identity from the latest terminal run and inserts
    // the same durable intent normal after-commit delivery would have produced.
    let second = SharedHost::new(Arc::new(MemoryHostModel), "stub")
        .with_store_dir(&dir)
        .with_runtime_authority(authority);
    bind_test_memory(&second, thread, "outbox-store", true);
    let ctx = second
        .ctx_for(thread, None)
        .await
        .expect("rehydrate thread");
    assert!(!ctx.durable, "D1/C3 must use DirectRunIngress");
    let run = ctx
        .commit
        .latest_run(&ctx.thread_id)
        .expect("terminal run record");
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
        .install_test_session_init("managed-write-a", init("agent", Some(&store_a)))
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
        .install_test_session_init("managed-read-a", init("agent", Some(&store_a)))
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
        .install_test_session_init("managed-read-b", init("agent", Some(&store_b)))
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
        .install_test_session_init("managed-unbound", init("unmanaged-agent", None))
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
                .install_test_session_init("memory-replay", init)
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
    managed
        .apply_session_inputs("memory-replay", host.local_workspace(), 1, &resources)
        .await
        .expect("cold durable generation installs beside the adopted Environment");

    let (left, right) = tokio::join!(
        managed.apply_session_inputs("memory-replay", host.local_workspace(), 1, &resources),
        managed.apply_session_inputs("memory-replay", host.local_workspace(), 1, &resources),
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
            host.local_workspace(),
            2,
            &awaken_session_contract::ResolvedSessionResources::default(),
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
        .install_test_session_init("managed-policy", init)
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
    let host = host_requiring_write_confirmation(Arc::new(ResumeMemModel));
    let store = test_memory_store_id();
    bind_test_memory(&host, "t-res", &store, true);

    // Run 1 awaits on the Ask-gated `write`.
    let r1 = host
        .run(
            None,
            "t-res",
            vec![Message::text(MessageId("u1".into()), Role::User, "hi")],
        )
        .await
        .expect("Run 1");
    assert!(
        matches!(r1.state, RunState::Awaiting),
        "Run should await on write"
    );
    let pending = r1.pending.expect("a pending tool");

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
    let host = SharedHost::new(Arc::new(CursorModel), "stub");
    let store = test_memory_store_id();
    bind_test_memory(&host, "t-cur", &store, true);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    host.run(None, "t-cur", user("alpha")).await.expect("Run 1");
    assert!(host.drain_runtime(std::time::Duration::from_secs(10)).await);
    host.run(None, "t-cur", user("beta")).await.expect("Run 2");
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
    let host = SharedHost::new(Arc::new(MemoryHostModel), "stub");
    let store = test_memory_store_id();
    bind_test_memory(&host, "t-mem", &store, true);

    let input = vec![Message::text(
        MessageId("u1".into()),
        Role::User,
        "I really like rust",
    )];
    let result = host.run(None, "t-mem", input).await.expect("Run");
    assert!(matches!(result.state, RunState::Ended(_)), "Run should end");

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
    // C2 a cached sandbox/projection exists. Constraint/Invariant: active generation
    // and installed sandbox projection advance as one authoritative pair. Decision rule:
    // apply C1 with C2 and require the documented rebuild effect, never a
    // mixed old/new projection.
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
    host.run(None, "t-attach", user("hi"))
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
        .apply_session_inputs("t-attach", host.local_workspace(), 1, &attached)
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
    assert_eq!(
        host.session_environment_handle("t-attach").await,
        Some(handle_before.clone()),
        "runtime rebuild retains the one Session environment"
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

    host.run(None, "t-attach", user("after attach"))
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
            host.local_workspace(),
            2,
            &awaken_session_contract::ResolvedSessionResources::default(),
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
        Some(handle_before)
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
    managed
        .install_test_session_init(
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
    let handle = environment.handle();
    assert_eq!(
        environment
            .list_workspace_files("workspace/live-repo")
            .await
            .unwrap()
            .iter()
            .find(|(path, _)| path == "README.md")
            .map(|(_, bytes)| bytes.as_slice()),
        Some(b"resident repository".as_slice()),
        "RD1 create-time checkout is physically present"
    );

    managed
        .apply_session_inputs(
            "t-repo-detach",
            host.local_workspace(),
            1,
            &awaken_session_contract::ResolvedSessionResources::default(),
        )
        .await
        .expect("detach repository");
    assert!(
        environment
            .list_workspace_files("workspace/live-repo")
            .await
            .unwrap()
            .is_empty(),
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
        .apply_session_inputs("t-local-attach", host.local_workspace(), 1, &attached)
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
/// C1 a frozen read-only File is staged; C2 Workdir cannot enforce immutability;
/// C3 a side-effect-free reservation preflight occurs; C4 no runtime/environment
/// becomes resident; C5 a committed-state GET follows. C1+C2+C3 cause E1 the
/// reservation to fail before activity or inference. C1+C2 cause E2 direct Run
/// construction to fail identically. C4+C5 cause E3 the query to open only
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
        .install_test_session_init(
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
                    serde_json::json!({}),
                ),
            },
        )
        .await
        .expect("stage frozen Session");

    let reservation_error = match host
        .ctx_for_session_reservation("t-query-after-provisioning-denial", Some("assistant"))
        .await
    {
        Ok(_) => panic!("Q1 reservation preflight must reject an impossible projection"),
        Err(error) => error,
    };
    assert!(
        reservation_error
            .message
            .contains("does not enforce read-only"),
        "Q1"
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
    assert!(error.message.contains("does not enforce read-only"), "Q2");
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
    let managed = crate::ManagedHost::new(host.clone());

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
        .install_test_session_init("t-sb", init)
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
        .install_test_session_init("t-bare", bare)
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
    let managed = crate::ManagedHost::new(host.clone());
    managed
        .install_test_session_init(
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
    crate::ManagedHost::new(host.clone())
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
/// provisioning is BackendOwned; C3 no trusted-host provider is installed on
/// the Coordinator. C1 dominates C2+C3 because only the claimed Worker may
/// realize host identity. Effects: E1 context construction succeeds, E2 no
/// physical environment is created, E3 no provider is requested.
///
/// | Rule | Coordinator-only | Provisioning | Trusted provider | Context | Environment |
/// |---|---|---|---|---|---|
/// | B1 | yes | BackendOwned | absent | built | absent |
/// | B2 | no | BackendOwned | absent | error | absent |
///
/// This regression test owns both rules at the dispatch/context boundary.
#[tokio::test]
async fn coordinator_defers_backend_owned_environment_to_the_claimed_worker() {
    let mut host = SharedHost::new(Arc::new(OkModel), "stub");
    host.deployment.disable_local_pool = true;
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("local-codex")
        .resolved_model(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
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
            .expect("coherent backend-owned candidate"),
        )
        .build();

    let context = host
        .ctx_for_snapshot_with_sandbox(
            "coordinator-backend-owned",
            Some("local-codex"),
            Some(snapshot.clone()),
            None,
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

    let local = SharedHost::new(Arc::new(OkModel), "stub");
    let error = match local
        .ctx_for_snapshot_with_sandbox(
            "local-backend-owned",
            Some("local-codex"),
            Some(snapshot),
            None,
        )
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
    let managed = crate::ManagedHost::new(host.clone());
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
    let managed = crate::ManagedHost::new(host.clone());
    managed
        .install_test_session_init(
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
    let direct_context = direct.ctx_for("gate-direct-absent", None).await.unwrap();
    assert!(
        direct_context.attempt_context.model_request_gate.is_none(),
        "G1"
    );

    let coordination: Arc<dyn awaken_session_contract::SessionAgentCoordination> =
        Arc::new(RejectingSessionAgentCoordination);
    let direct_with_endpoint = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    crate::ManagedHost::new(direct_with_endpoint.clone())
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
    let managed_runtime = crate::ManagedHost::new(managed.clone());
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
    crate::ManagedHost::new(missing.clone())
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
/// | S2 | yes | no | no metadata/materialization/read |
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
        StaticPublishedAgentSnapshots::try_new([snapshot]).expect("one immutable published Agent"),
    );
    let recorder = ToolFaceRecorder::default();
    let observed = recorder.0.clone();
    let host = Arc::new(
        SharedHost::new(Arc::new(recorder), "stub").with_agent_publications(publications.clone()),
    );
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

    host.run(
        Some("published-skill"),
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
            .iter()
            .any(|file| file.id == selected),
        "S1/E2"
    );

    let missing_recorder = ToolFaceRecorder::default();
    let missing_observed = missing_recorder.0.clone();
    let missing_host = Arc::new(
        SharedHost::new(Arc::new(missing_recorder), "stub").with_agent_publications(publications),
    );
    missing_host
        .run(
            Some("published-skill"),
            "selected-without-delivery",
            vec![Message::text(
                MessageId("selected-without-delivery-user".into()),
                Role::User,
                "Release signal",
            )],
        )
        .await
        .expect("S2 selected but unavailable Skill cannot become ambient content");
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
            .expect("S2 sandbox")
            .scan_skill_dir(crate::skills::DELIVERED_SKILLS_SUBDIR)
            .is_empty(),
        "S2"
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
    let unselected_publications = StaticPublishedAgentSnapshots::try_new([unselected_snapshot])
        .expect("one immutable unselected Agent");
    let unselected_recorder = ToolFaceRecorder::default();
    let unselected_observed = unselected_recorder.0.clone();
    let unselected_host = Arc::new(
        SharedHost::new(Arc::new(unselected_recorder), "stub")
            .with_agent_publications(Arc::new(unselected_publications)),
    );
    unselected_host
        .session_slots
        .update("unselected-delivery", |slot| {
            slot.skills = host
                .session_slots
                .read("published-skill-thread", |slot| slot.skills.clone())
                .flatten();
        });
    unselected_host
        .run(
            Some("unselected-skill"),
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
    crate::ManagedHost::new(host.clone())
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
    let managed = crate::ManagedHost::new(host.clone());
    managed
        .install_test_session_init(
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
    crate::ManagedHost::new(host.clone())
        .install_test_session_init(
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
    crate::ManagedHost::new(host.clone())
        .install_test_session_init(
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
        .ctx_for_snapshot_with_sandbox(
            "deferred-managed-filesystem-skill",
            Some("assistant"),
            Some(provisional.config.clone()),
            None,
        )
        .await
        .expect("L10 claimed-style executable context");
    let environment = executable.env.as_ref().expect("L10/E2 eager Environment");
    let files = environment.scan_skill_dir(crate::skills::DELIVERED_SKILLS_SUBDIR);
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
/// with a support file. Effects: E1 catalog capability classification defeats
/// deferral; E2 one Environment exists before Skill wiring; E3 the support file
/// is materialized there. Rule L8: C1+C2+C3=>E1+E2+E3. L1/L2 above cover the
/// negative text-only and instruction-only rows.
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
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment: on_tool_use_environment(),
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "assistant".into(),
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
    host.install_frozen_session_projection(
        "deferred-legacy-delivered-skill",
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: workspace,
            revision: awaken_session_contract::SessionRevision(1),
            baseline,
            agent_publication: None,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            mcp: Vec::new(),
            tools: Default::default(),
            request_context: Vec::new(),
        },
        None,
        true,
        None,
    )
    .await
    .expect("L8 cold legacy projection");
    let executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor> =
        Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
            Arc::downgrade(&host),
            "deferred-legacy-delivered-skill",
        ));
    host.session_slots
        .update("deferred-legacy-delivered-skill", |slot| {
            slot.deferred_executor = Some(executor)
        });

    let context = host
        .ctx_for("deferred-legacy-delivered-skill", Some("assistant"))
        .await
        .expect("L8 context");
    assert!(context.env.is_some(), "L8/E1+E2");
    assert!(
        storage
            .path()
            .join("sandboxes/deferred-legacy-delivered-skill/.skills/files/references/guide.md")
            .is_file(),
        "L8/E3"
    );
}

struct BindingOrderSink {
    host: std::sync::Weak<SharedHost>,
    calls: AtomicUsize,
    observed_before_publish: std::sync::atomic::AtomicBool,
    fail: bool,
    require_realization: bool,
    owned_session_id: Option<String>,
    committed_environment:
        Option<Arc<Mutex<Option<awaken_session_contract::SessionEnvironmentState>>>>,
}

struct MovingRealizationFenceSink {
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

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for MovingRealizationFenceSink {
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
            Err(awaken_session_contract::RunError::classified(
                "session_realization_stale",
                "a newer exact realization fence owns the Session",
            ))
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for BindingOrderSink {
    async fn owns(&self, session_id: &str) -> Result<bool, awaken_session_contract::RunError> {
        Ok(self
            .owned_session_id
            .as_deref()
            .is_none_or(|owned| owned == session_id))
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
        if self.require_realization && receipt.realization.is_none() {
            Err(awaken_session_contract::RunError::classified(
                "session_realization_stale",
                "replacement realization is not installed",
            ))
        } else if self.fail {
            Err(awaken_session_contract::RunError::internal(
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

// Immediate binding decision table:
// resident/adopted -> reuse without a write; absent + concurrent callers -> one
// lifecycle owner; successful CAS -> persist before publish; failed CAS ->
// dispose and publish nothing. Repository tests own conflict retry/exhaustion
// and already-equal idempotence at the durable aggregate boundary.
#[tokio::test]
async fn new_environment_binding_commits_once_before_concurrent_contexts_can_use_it() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    crate::ManagedHost::new(host.clone()).install_environment_binding_sink(sink.clone());

    let (left, right) = tokio::join!(
        host.ctx_for("binding-order", None),
        host.ctx_for("binding-order", None)
    );
    left.unwrap();
    right.unwrap();
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert!(sink.observed_before_publish.load(Ordering::SeqCst));
    assert!(host.session_environment("binding-order").await.is_some());
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

/// Restart synchronization cause/effect graph: C1 a recovered claim adopts the
/// Session environment before Control's replacement lease is projected; C2 the
/// first exact receipt is rejected as stale; C3 Control installs the new lease.
/// R1 C1+C2 blocks publication, R2 C1+C2+C3 retries with the exact new lease and
/// publishes once. The no-C3 timeout/fail-closed rule is owned by the adjacent
/// binding-failure test and the bounded wait in `persist_environment_before_publish`.
#[tokio::test]
async fn recovered_environment_waits_for_replacement_realization_before_publish() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: true,
        owned_session_id: None,
        committed_environment: None,
    });
    crate::ManagedHost::new(host.clone()).install_environment_binding_sink(sink.clone());

    let opening = {
        let host = host.clone();
        tokio::spawn(async move { host.ctx_for("binding-restart", None).await })
    };
    while sink.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
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
    opening.await.expect("join").expect("R2");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2, "R2");
    assert!(host.session_environment("binding-restart").await.is_some());
}

/// Multi-renewal FMECA: C1 a slow environment returns under no projected lease;
/// C2 Control installs epoch 2, then C3 epoch 3 before the retry commits. E1 each
/// stale receipt remains fenced, E2 the host follows both exact notifications,
/// and E3 only epoch 3 can publish the environment. This is scenario-neutral:
/// the same race applies to container startup, image preparation, and recovery.
#[tokio::test]
async fn environment_binding_catches_up_across_multiple_realization_fences() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(MovingRealizationFenceSink {
        calls: AtomicUsize::new(0),
        accepted_epoch: AtomicU64::new(3),
    });
    crate::ManagedHost::new(host.clone()).install_environment_binding_sink(sink.clone());

    let opening = {
        let host = host.clone();
        tokio::spawn(async move { host.ctx_for("binding-moving-fence", None).await })
    };
    while sink.calls.load(Ordering::SeqCst) < 1 {
        tokio::task::yield_now().await;
    }
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
        while sink.calls.load(Ordering::SeqCst) < usize::try_from(epoch).unwrap() {
            tokio::task::yield_now().await;
        }
    }

    opening.await.expect("join").expect("epoch 3 commits");
    assert_eq!(sink.calls.load(Ordering::SeqCst), 3, "E1/E2/E3");
    assert!(
        host.session_environment("binding-moving-fence")
            .await
            .is_some()
    );
}

/// Durable-binding decision table: no binding + no resident Environment permits
/// first creation; exact binding + adopted/resident permits reuse (covered by the
/// recovery E2E); exact binding + neither must fail before provider creation.
#[tokio::test]
async fn missing_durable_environment_adoption_never_creates_a_substitute() {
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
    assert!(error.to_string().contains("was not adopted"));
    assert!(host.session_environment("binding-corrupt").await.is_none());
}

#[tokio::test]
async fn binding_commit_failure_disposes_and_never_publishes_the_environment() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    crate::ManagedHost::new(host.clone()).install_environment_binding_sink(sink.clone());

    let error = match host.ctx_for("binding-failure", None).await {
        Ok(_) => panic!("binding failure must not publish a context"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("binding store unavailable"));
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
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
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    let managed = crate::ManagedHost::new(host.clone());
    managed.install_environment_binding_sink(sink.clone());
    managed
        .install_test_session_init(
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
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
    assert!(sink.observed_before_publish.load(Ordering::SeqCst));
    assert!(host.session_environment("deferred-hand").await.is_some());
}

/// L5: persistence failure fails closed; no deferred environment becomes visible.
#[tokio::test]
async fn on_tool_use_binding_failure_never_publishes_the_environment() {
    // Test design. Causes: C1 Environment binding fails after realization starts.
    // Effects: E1 tool execution fails; E2 no Environment is published as active.
    // Constraint/Invariant: publication follows complete binding and cannot expose
    // a partial sandbox. Decision rule: inject C1 and require E1/E2 fail-closed.
    use awaken_session_contract::SessionInit;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    install_test_session_application(&host);
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
        require_realization: false,
        owned_session_id: None,
        committed_environment: None,
    });
    let managed = crate::ManagedHost::new(host.clone());
    managed.install_environment_binding_sink(sink.clone());
    managed
        .install_test_session_init(
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
    assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
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
                        mount_path: "/mnt/repo".into(),
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
}

/// Terminal publication cause/effect decision table:
///
/// | Rule | frozen input | local branch/HEAD | remote ref | Effect |
/// |---|---|---|---|---|
/// | P1 | one writable Repository | exact expected coordinate | absent | publish and return canonical Session receipt |
/// | P2 | same command replay | exact expected coordinate | same commit | no-op with byte-identical receipt |
/// | P3 | command carries no second Resource lookup | exact | any | compile the command input through the ordinary staging owner |
/// | P4 | child cleanup complete, publication receipt absent | exact | same commit | record replay receipt before root cleanup |
///
/// Invalid coordinate and mismatched remote rules are exhaustively covered at
/// the Repository realizer boundary; this integration test proves the
/// ManagedHost uses that sole implementation and retains the Environment until
/// first execution, replay, and the ordered terminal reconciliation have
/// produced evidence.
#[tokio::test]
async fn managed_terminal_repository_publication_pushes_exact_commit_and_replays() {
    use awaken_session_contract::SessionInit;

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
            sandbox_root,
            false,
            Arc::new(crate::session_environment::UnusedHandExecutorFactory),
            "/bin/sh",
            std::time::Duration::ZERO,
        );
    let repository_path_fidelity = raw_host.session_provider.capabilities().path_fidelity;
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());
    let _dispatch_runtime = managed.clone().install_dispatch_session_runtime();
    let session_id = "terminal-publication-local";
    let resources = effective_repository(
        "repository-publication",
        remote.to_str().unwrap(),
        "repository",
        None,
    );
    managed
        .install_test_session_init(
            session_id,
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent".into(),
                delegate_ids: Vec::new(),
                tools: None,
                resource_revision: 1,
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
        .expect("P1 prepare exact input");
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
                "git -C repository rev-parse HEAD > commit.txt"
            ),
        ]))
        .await
        .expect("P1 author commit");
    assert_eq!(status.code, Some(0), "P1 exact local commit");
    let commit = environment
        .list_workspace_files("")
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
        },
    };
    let mut operation = awaken_session_contract::SessionCleanupOperation::default();
    operation
        .request_with_publication(session_id, intent)
        .expect("P1 publication fence");
    let child_id = "terminal-publication-child";
    operation
        .freeze_targets(session_id, [child_id.to_string()], 0, 0)
        .expect("P1 root target");
    let child_cleanup = operation
        .command_for(session_id, child_id)
        .expect("P4 child cleanup command");
    let root_cleanup = operation
        .command_for(session_id, session_id)
        .expect("P4 root cleanup command");
    operation
        .record_completion(
            session_id,
            awaken_session_contract::SessionCleanupCompletion::new(&child_cleanup, Vec::new()),
        )
        .expect("P4 publication becomes reachable only after its child barrier");
    let command = operation
        .publication_command(session_id)
        .expect("P1 command projection")
        .expect("P1 command");
    let first = managed
        .execute_terminal_repository_publication(command.clone())
        .await
        .expect("P1 publish");
    let replay = managed
        .execute_terminal_repository_publication(command.clone())
        .await
        .expect("P2 replay");
    assert_eq!(first, replay, "P2 canonical first/replay receipt");
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
    assert!(
        host.session_environment(session_id).await.is_some(),
        "P1-P2 Environment survives until root cleanup"
    );

    host.run(
        None,
        child_id,
        vec![Message::text(
            MessageId("terminal-publication-child-input".into()),
            Role::User,
            "child",
        )],
    )
    .await
    .expect("P4 child Environment");
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "terminal-publication-worker".into(),
        runtime_incarnation: "terminal-publication-worker:incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms()
            .saturating_add(60_000),
    };
    host.install_session_realization_lease(session_id, lease);
    control
        .cleanup_sequence
        .lock()
        .unwrap()
        .extend([Some(vec![child_cleanup]), Some(vec![root_cleanup])]);
    *control.publication_projection.lock().unwrap() = Some(
        awaken_session_contract::SessionRepositoryPublicationProjection {
            workspace_id: host.local_workspace().into(),
            command,
        },
    );

    assert_eq!(
        host.renew_due_session_realizations(0, 0)
            .await
            .expect("P4 ordered terminal reconciliation"),
        0,
        "P4 terminal work never becomes an ordinary lease renewal"
    );
    assert_eq!(
        control.events.lock().unwrap().as_slice(),
        [
            "cleanup:poll",
            "cleanup:terminal-publication-child",
            "publication:poll",
            "publication:receipt",
            "cleanup:poll",
            "cleanup:terminal-publication-local",
        ],
        "P4 child -> publication receipt -> root"
    );
    assert_eq!(control.publication_receipts.lock().unwrap().len(), 1, "P4");
    assert!(
        host.session_environment(session_id).await.is_none(),
        "P4 root cleanup runs only after the publication receipt"
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
            .with_acp(executor)
            .with_session_container_provider(
                Arc::new(SecureExternalProvider),
                Arc::new(UnusedHandFactory),
            ),
    );
    secure_acp_host.register_thread_backend_projection("mcp-acp-secure", "acp:test");
    secure_acp_host.register_thread_backend_projection("mcp-acp-client", "acp:claude");
    secure_acp_host.register_thread_backend_projection("mcp-acp-refresh", "acp:claude");
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

    let host = SharedHost::new(Arc::new(OkModel), "stub");
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
            self.started.notify_waiters();
            std::future::pending().await
        }
    }

    // Cause/effect graph: C1 one exact generation is Active; C2 one local tool
    // call is in flight; C3 drain is requested; C4 cancellation has begun but
    // the call future has not completed its drop. Effects: E1 new visibility is
    // closed immediately; E2 state remains Draining and no Removed receipt can
    // be observed during C4; E3 releasing the last call guard permits Removed;
    // E4 the busy call terminates with revocation instead of producing a late
    // result. Decision rules: Q1 C1+C2+C3+C4=>E1+E2; Q2 Q1+quiesced=>E3+E4.
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
    let runtime_host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
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
    let managed = managed_with_resource_source(host.clone()).with_credentials(credentials, secrets);

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
        .with_credentials(credentials.clone(), secrets.clone());
    managed
        .install_test_session_init(
            "t-rot",
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

    // The Managed adapter stores the supplied credential in the Vault and publishes a
    // new Repository config before invoking this complete-manifest runtime port.
    managed
        .apply_session_inputs("t-rot", host.local_workspace(), 1, &next)
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
        init.resources = effective_resources(inputs);
        managed
            .install_test_session_init(&thread, init)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.rule));

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
            .install_test_session_init(&thread, init)
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
    let host = Arc::new(
        SharedHost::new(Arc::new(OkModel), "stub")
            .with_skill_store(storage.path().join("skills"))
            .with_store_dir(storage.path()),
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
    init.resources = init
        .resources
        .with_skills(vec![awaken_session_contract::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "governed".into(),
            version: 1,
            bundle_sha256: hash,
        }])
        .unwrap();
    managed
        .install_test_session_init("skill-revoke", init)
        .await
        .expect("prepare pinned Skill");
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
    let delivered = storage
        .path()
        .join("sandboxes/skill-revoke/.skills/governed/scripts/old.sh");
    assert!(delivered.is_file(), "pinned Skill support file exists");

    managed
        .apply_session_inputs(
            "skill-revoke",
            &workspace,
            1,
            &awaken_session_contract::ResolvedSessionResources::default(),
        )
        .await
        .expect("replace with explicit empty Skill selection");
    assert!(
        !storage
            .path()
            .join("sandboxes/skill-revoke/.skills")
            .exists(),
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
    let host = SharedHost::new(Arc::new(OkModel), "stub");
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
    let host = SharedHost::new(Arc::new(OkModel), "host-default");

    let ctx = host
        .ctx_for_snapshot_with_sandbox(
            "t-published-claim",
            Some("published-agent"),
            Some(published.clone()),
            None,
        )
        .await
        .expect("worker session builds from claimed snapshot");

    assert_eq!(ctx.config, published);
    assert_eq!(
        ctx.config.resolved_spec.catalog_fingerprint.0,
        "sha256:published"
    );
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

    SharedHost::new(Arc::new(OkModel), "stub")
        .ctx_for_snapshot_with_sandbox(
            "background-local",
            Some("background-agent"),
            Some(snapshot.clone()),
            None,
        )
        .await
        .expect("R1 co-located Native context");

    let worker = SharedHost::new(Arc::new(OkModel), "stub").with_worker_upstream(
        awaken_worker_transport_security::WorkerUpstream::new("http://coordinator.invalid"),
    );
    let error = match worker
        .ctx_for_snapshot_with_sandbox(
            "background-worker",
            Some("background-agent"),
            Some(snapshot),
            None,
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
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let ctx = host
        .ctx_for_snapshot_with_sandbox(
            "a2a-io-only",
            Some("remote-agent"),
            Some(snapshot.clone()),
            None,
        )
        .await
        .expect("R1 A2A context");
    assert!(ctx.env.is_none(), "R1 Environment");
    assert!(ctx.attempt_context.tool_executor.is_none(), "R1 Hand");
    assert!(
        host.session_environment("a2a-io-only").await.is_none(),
        "R1 owner"
    );

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
        .ctx_for_snapshot_with_sandbox("a2a-with-mount", Some("remote-agent"), Some(snapshot), None)
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
    let ctx = host
        .ctx_for_snapshot_with_sandbox(
            "a2a-native-fallback",
            Some("mixed-agent"),
            Some(mixed),
            None,
        )
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

/// Dispatch projection rule: a registered manifest's Workspace, generation, and
/// resolved values are one cause tuple; the envelope decode must reproduce that
/// tuple exactly and select the Session-resource worker capability.
#[test]
fn durable_dispatch_carries_the_frozen_session_resource_manifest_and_scope() {
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let thread = "t-dispatch-resources";
    let manifest = awaken_session_contract::SessionResourceManifest::at_revision(
        "workspace-a",
        7,
        awaken_session_contract::ResolvedSessionResources::default(),
    );
    host.register_thread_resource_manifest(thread, manifest.clone());
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
        .model(test_model_binding())
        .fingerprint("sha256:dispatch-resources")
        .build();
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-dispatch-resources".into()),
        awaken_agent_contract::agent::thread::Id(thread.into()),
        snapshot,
        Vec::new(),
    );

    let dispatch = host
        .resolved_dispatch(activation)
        .expect("decorate durable dispatch");
    let carried = dispatch
        .session_resources
        .as_ref()
        .expect("resource envelope")
        .decode_manifest()
        .expect("decode resource envelope");
    assert_eq!(carried, manifest);
    assert_eq!(
        dispatch.execution_scope,
        Some(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-a")
        ))
    );
    assert!(
        dispatch
            .placement
            .required_capabilities
            .contains(awaken_run_ingress::SESSION_RESOURCES_CAPABILITY),
        "mixed local/remote deployments must not expose the manifest to an ineligible worker"
    );
    assert_eq!(
        dispatch.session_thread_id, None,
        "a resource-bearing ordinary Run must not be promoted to a Session"
    );
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

    let first = SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path());
    let first_ctx = first.ctx_for(thread, None).await.expect("first session");
    let handle = first_ctx.env.as_ref().expect("eager environment").handle();
    let marker = storage
        .path()
        .join("sandboxes")
        .join(thread)
        .join("recovery-marker");
    std::fs::write(&marker, b"survived").expect("write sandbox marker");
    drop(first_ctx);
    drop(first);

    let replacement = SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path());
    let adopted = replacement
        .session_provider
        .adopt(&replacement.sandbox_spec(thread), &handle)
        .await
        .expect("adopt durable handle");
    assert_eq!(
        adopted.status().await.unwrap(),
        awaken_provisioning_contract::SandboxStatus::Ready
    );
    let replacement_ctx = replacement
        .ctx_for_with_sandbox(thread, None, Some(adopted))
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
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path());
    let resident = host
        .ctx_for("t-resident-adoption", None)
        .await
        .expect("resident session");
    let resident_handle = resident.env.as_ref().expect("eager environment").handle();

    let same = host
        .session_provider
        .adopt(&host.sandbox_spec("t-resident-adoption"), &resident_handle)
        .await
        .expect("adopt resident sandbox");
    let reused = host
        .ctx_for_with_sandbox("t-resident-adoption", None, Some(same))
        .await
        .expect("the exact resident sandbox is idempotently accepted");
    assert!(Arc::ptr_eq(&resident, &reused));

    let foreign = host
        .session_provider
        .create(&host.sandbox_spec("t-foreign-resident"))
        .await
        .expect("foreign sandbox");
    let error = match host
        .ctx_for_with_sandbox("t-resident-adoption", None, Some(foreign))
        .await
    {
        Ok(_) => panic!("a resident session must reject a different sandbox"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Internal);
    assert_eq!(
        error.message,
        "thread t-resident-adoption is already bound to a different sandbox"
    );
    assert_eq!(
        resident.env.as_ref().expect("eager environment").handle(),
        resident_handle
    );
}

#[tokio::test]
async fn retained_session_accepts_only_an_adoption_of_its_exact_sandbox() {
    let storage = tempfile::tempdir().expect("storage dir");
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_store_dir(storage.path());
    let original = host
        .ctx_for("t-retained-adoption", None)
        .await
        .expect("initial session");
    let retained_handle = original.env.as_ref().expect("eager environment").handle();
    assert!(
        host.session_slots
            .modify("t-retained-adoption", |slot| slot.runtime.take())
            .flatten()
            .is_some(),
        "only the runtime context is evicted"
    );
    drop(original);

    let foreign = host
        .session_provider
        .create(&host.sandbox_spec("t-foreign-retained"))
        .await
        .expect("foreign sandbox");
    let error = match host
        .ctx_for_with_sandbox("t-retained-adoption", None, Some(foreign))
        .await
    {
        Ok(_) => panic!("a retained session must reject a different sandbox"),
        Err(error) => error,
    };
    assert_eq!(error.kind, HostErrorKind::Internal);
    assert_eq!(
        error.message,
        "thread t-retained-adoption is already bound to a different sandbox"
    );

    let same = host
        .session_provider
        .adopt(&host.sandbox_spec("t-retained-adoption"), &retained_handle)
        .await
        .expect("adopt retained sandbox");
    let rebuilt = host
        .ctx_for_with_sandbox("t-retained-adoption", None, Some(same))
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
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_client_tools(HashSet::from(["lookup".to_string()]));
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
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_client_tools(HashSet::from(["lookup".to_string()]));
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
    // activity epoch is still active. Effects: E1
    // stage one input bound to its deterministic primary Run; E2 atomically enqueue one deterministic
    // primary Run carrying C4; E3 retry is an exact no-op; E4 no Worker constructs
    // a parent activation. Awaiting is intentionally absent: Managed projects its child
    // lifecycle directly and SessionApplication never invokes this command.
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
    assert_eq!(inbox.len(), 1, "R1/R2 one input");
    assert_eq!(
        inbox[0].input.message_id,
        MessageId::agent_thread_report(&child_run).0,
        "R1 typed provenance"
    );
    assert_eq!(
        inbox[0].input.run_id, rows[0].run_id,
        "R1/E1 the report cannot be drained by another primary Run"
    );

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
    let host =
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch.clone());
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
            Vec::new(),
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
    // C4 expected Run/correlation is exact/stale. Effects: E1 the existing root
    // dispatch activity rotates and one client ToolOutput is staged; E2 normal
    // content and is_error survive unchanged; E3 stale admission coordinates
    // fail before Outbox mutation. Child confirmation coverage lives in the
    // sibling decision table above; both targets use this same Host boundary.
    //
    // | Rule | Target | Ticket | Result | Effect |
    // |---|---|---|---|---|
    // | PR1 | Primary | exact | normal generic | E1+E2 |
    // | PR2 | Primary | stale Run/correlation | normal generic | E3 |
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
    let host =
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch.clone());
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
            Vec::new(),
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
    let host =
        SharedHost::new(Arc::new(MemoryHostModel), "stub").with_dispatch_store(dispatch.clone());

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
    // call and its Awaiting ticket; C2=the foreground protocol has not yet copied
    // that position into disposable SessionState; C3=a peer protocol queries the
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
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_client_tools(HashSet::from(["lookup".to_string()]));
    let first = host
        .run(None, "t-cross-protocol-pending", user("hi"))
        .await
        .expect("client tool awaits");
    let expected = first.pending.expect("run exposes the pending client tool");

    // Model the small cross-protocol window after the durable commit and before
    // the foreground adapter finalizes its own in-memory projection.
    let ctx = host
        .ctx_for("t-cross-protocol-pending", None)
        .await
        .expect("session context");
    ctx.state.lock().await.awaiting_run = None;

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

/// A terminal Session cleanup disposes the thread's sandbox — the ONLY place it
/// is reaped. Proven end-to-end through the exact command-bearing
/// `SessionRuntime::execute_terminal_cleanup` port: the cached ctx is evicted
/// AND the live sandbox's workspace dir is actually reaped (its `status` flips
/// `Ready` → `Terminated`), unlike the evict-to-rebuild edges (attach/detach/
/// rebind) which keep the per-Thread workspace so the next Run reuses it.
#[tokio::test]
async fn exact_terminal_cleanup_disposes_the_threads_sandbox() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_provisioning_contract::SandboxStatus;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());

    // A first Run builds + caches the Thread's sandbox.
    host.run(
        None,
        "t-end",
        vec![Message::text(MessageId("hi".into()), Role::User, "hi")],
    )
    .await
    .expect("first Run");
    // Hold the live sandbox handle before teardown so we can observe its disposal
    // even after the ctx is evicted from the registry.
    let env = host
        .session_environment("t-end")
        .await
        .expect("the first Run caches the Thread's sandbox ctx");
    assert_eq!(
        env.status().await.expect("status"),
        SandboxStatus::Ready,
        "the sandbox workspace exists while the session is live"
    );

    // Stage representative resource/config projections after the sandbox is live;
    // terminal cleanup must erase all of them so reusing the opaque thread id cannot
    // inherit stale scope, capability, or model state.
    host.register_thread_workspace("t-end", "workspace-a");
    host.register_thread_memory("t-end", None);
    host.register_thread_resources("t-end", crate::provisioning::StagedResources::default());
    host.register_thread_model("t-end", "private-model");
    host.install_environment_projection(
        "t-end",
        &session_environment(
            awaken_session_contract::SessionNetworkPolicy::None,
            serde_json::json!({}),
        ),
    )
    .expect("freeze terminal-test Environment");

    // Cause/effect graph: C1 the application-owned operation freezes one exact
    // root command; C2 archive and recovery execute that same command
    // concurrently; C3 the command targets an already-cleaned or unknown
    // Session. Effects: E1 one lifecycle owner disposes all projections; E2 the
    // exact replay is idempotent; E3 cleanup remains a physical no-op. Decision
    // rules: T1=C1+C2=>E1+E2; T2=C1+C3=>E3. There is deliberately no unscoped
    // `end_session(thread)` compatibility path beside the durable operation.
    let mut cleanup = awaken_session_contract::SessionCleanupOperation::default();
    assert!(cleanup.request("t-end"), "T1 freezes the terminal fence");
    cleanup
        .freeze_targets("t-end", [], 0, 0)
        .expect("T1 freezes the root target");
    let command = cleanup
        .command_for("t-end", "t-end")
        .expect("T1 exact root command");
    let (archive, recovery) = tokio::join!(
        managed.execute_terminal_cleanup(command.clone()),
        managed.execute_terminal_cleanup(command)
    );
    archive.expect("archive terminal cleanup");
    recovery.expect("recovery terminal cleanup replay");

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
    assert!(host.registered_thread_workspace("t-end").is_none());
    assert!(!host.session_slots.contains("t-end"));
    assert!(host.inference_routing.override_for("t-end").is_none());
    assert_eq!(
        host.sandbox_spec("t-end").network,
        awaken_provisioning_contract::NetworkPolicy::Unrestricted
    );

    // Idempotent: ending an already-ended or never-created session is a clean no-op.
    let replay = cleanup
        .command_for("t-end", "t-end")
        .expect("T2 exact replay command");
    managed
        .execute_terminal_cleanup(replay)
        .await
        .expect("T2 cleanup replay is idempotent");
    let mut missing = awaken_session_contract::SessionCleanupOperation::default();
    assert!(missing.request("never-existed"));
    missing.freeze_targets("never-existed", [], 0, 0).unwrap();
    managed
        .execute_terminal_cleanup(
            missing
                .command_for("never-existed", "never-existed")
                .unwrap(),
        )
        .await
        .expect("T2 cleanup is a no-op for an unknown thread");
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
    commands: Mutex<Option<Vec<awaken_session_contract::SessionCleanupCommand>>>,
    cleanup_sequence: Mutex<
        std::collections::VecDeque<Option<Vec<awaken_session_contract::SessionCleanupCommand>>>,
    >,
    completions: Mutex<Vec<awaken_session_contract::SessionCleanupCompletion>>,
    publication_projection:
        Mutex<Option<awaken_session_contract::SessionRepositoryPublicationProjection>>,
    publication_receipts: Mutex<Vec<awaken_session_contract::SessionRepositoryPublicationReceipt>>,
    events: Mutex<Vec<String>>,
    poll_barrier: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    poll_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    poll_sessions: Mutex<Vec<String>>,
    polls_entered: std::sync::atomic::AtomicUsize,
    polls_active: std::sync::atomic::AtomicUsize,
    max_polls_active: std::sync::atomic::AtomicUsize,
    claim_targets: Mutex<Vec<awaken_session_contract::SessionRealizationTarget>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for RemoteTerminalCleanupControl {
    async fn begin_session_realization(
        &self,
        _command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(awaken_session_contract::SessionRealizationControlFailure::NotReady)
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
        Ok(self.assignments.lock().unwrap().pop_front())
    }

    async fn terminal_cleanup_commands(
        &self,
        session_id: &str,
        _lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<Vec<awaken_session_contract::SessionCleanupCommand>>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        use std::sync::atomic::Ordering;

        let active = self.polls_active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_polls_active.fetch_max(active, Ordering::SeqCst);
        self.polls_entered.fetch_add(1, Ordering::SeqCst);
        self.poll_sessions.lock().unwrap().push(session_id.into());
        self.events.lock().unwrap().push("cleanup:poll".into());
        let barrier = self.poll_barrier.lock().unwrap().clone();
        if let Some((started, proceed)) = barrier {
            started.notify_one();
            proceed.notified().await;
        }
        let gate = self.poll_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire_owned()
                .await
                .expect("poll gate remains open")
                .forget();
        }
        let result = match self.cleanup_sequence.lock().unwrap().pop_front() {
            Some(commands) => commands,
            None => self.commands.lock().unwrap().clone(),
        };
        self.polls_active.fetch_sub(1, Ordering::SeqCst);
        Ok(result)
    }

    async fn terminal_repository_publication_command(
        &self,
        _session_id: &str,
        _lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        self.events.lock().unwrap().push("publication:poll".into());
        Ok(self.publication_projection.lock().unwrap().clone())
    }

    async fn record_terminal_repository_publication_receipt(
        &self,
        _session_id: &str,
        _lease: &awaken_session_contract::SessionRealizationLease,
        receipt: awaken_session_contract::SessionRepositoryPublicationReceipt,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.events
            .lock()
            .unwrap()
            .push("publication:receipt".into());
        self.publication_receipts.lock().unwrap().push(receipt);
        Ok(())
    }

    async fn record_terminal_cleanup_completion(
        &self,
        _lease: &awaken_session_contract::SessionRealizationLease,
        completion: awaken_session_contract::SessionCleanupCompletion,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        self.events
            .lock()
            .unwrap()
            .push(format!("cleanup:{}", completion.thread_id));
        self.completions.lock().unwrap().push(completion);
        Ok(())
    }
}

fn remote_terminal_cleanup_projection() -> awaken_session_contract::FrozenSessionProjection {
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment: session_environment(
                awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                serde_json::json!({}),
            ),
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
        mcp: Vec::new(),
        tools: Default::default(),
        request_context: Vec::new(),
    }
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
     * resident Sessions are simultaneously eligible for the same Control poll;
     * C2 every poll blocks at one deterministic gate; C3 the Worker-wide bound
     * is eight; C4 lease deadlines differ; C5 the gate is released. Effects:
     * E1 exactly eight polls enter before release; E2 no ninth poll enters; E3
     * the first wave contains the eight earliest deadlines; E4 all sixty-four
     * eventually finish; E5 each Session is polled exactly once. Constraint:
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
    *control.poll_gate.lock().unwrap() = Some(gate.clone());
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone()));
    for index in 0..SESSION_COUNT {
        host.install_session_realization_lease(
            &format!("mass-session-{index:03}"),
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
            .renew_due_session_realizations(10_000, 60_000)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if control.polls_entered.load(Ordering::SeqCst)
                == crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RECONCILIATIONS
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("L1 first bounded wave enters");
    assert_eq!(
        control.polls_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RECONCILIATIONS,
        "L1/E1"
    );
    assert_eq!(
        control.max_polls_active.load(Ordering::SeqCst),
        crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RECONCILIATIONS,
        "L1/E2"
    );
    let first_wave = control.poll_sessions.lock().unwrap().clone();
    assert_eq!(
        first_wave,
        (0..crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RECONCILIATIONS)
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
        0,
        "L2 no lease was due"
    );
    assert_eq!(
        control.polls_entered.load(Ordering::SeqCst),
        SESSION_COUNT,
        "L2/E4"
    );
    assert_eq!(control.polls_active.load(Ordering::SeqCst), 0, "L2/E4");
    let mut observed = control.poll_sessions.lock().unwrap().clone();
    observed.sort();
    observed.dedup();
    assert_eq!(observed.len(), SESSION_COUNT, "L2/E5");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn five_hundred_twelve_session_reconciliation_remains_bounded_and_complete() {
    use std::sync::atomic::Ordering;

    /* Scale rule S1: C1 five hundred twelve independent, non-due Sessions and
     * C2 an immediately responsive Control port under the same production cap
     * imply E1 every Session is visited exactly once, E2 the scan completes
     * within its three-second Worker safety envelope, and E3 observed
     * concurrency never exceeds the configured bound. This is a deterministic
     * scale test, not a throughput benchmark; live HTTP/SQLite latency is owned
     * by the deployment acceptance test.
     */
    const SESSION_COUNT: usize = 512;
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host = SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone());
    for index in 0..SESSION_COUNT {
        host.install_session_realization_lease(
            &format!("scale-session-{index:04}"),
            awaken_session_contract::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: "worker-a:incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: 100_000 + index as u64,
            },
        );
    }
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.renew_due_session_realizations(10_000, 120_000),
        )
        .await
        .expect("S1/E2 scale scan remains bounded")
        .expect("S1 scale reconciliation succeeds"),
        0,
        "S1 non-due leases need no renewal"
    );
    assert_eq!(
        control.polls_entered.load(Ordering::SeqCst),
        SESSION_COUNT,
        "S1/E1"
    );
    assert!(
        control.max_polls_active.load(Ordering::SeqCst)
            <= crate::application::MAX_CONCURRENT_SESSION_REALIZATION_RECONCILIATIONS,
        "S1/E3"
    );
}

#[tokio::test]
async fn remote_worker_executes_the_canonical_terminal_cleanup_command_locally() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a Worker-local Session owns a live Sandbox and one
    // realization lease; C2 Control projects the durable root cleanup command;
    // C3 the lease is not yet due for ordinary renewal. Effects: E1 the existing
    // heartbeat reconciliation still polls terminal truth; E2 the Worker-local
    // ManagedHost publishes/harvests/disposes through its sole cleanup method;
    // E3 the exact completion returns to Control; E4 no renewal/revoke path can
    // substitute a nonterminal projection drop.
    //
    // | Rule | cleanup | renewal due | Effect |
    // | R1 | command | no | E1 + E2 + E3 |
    // | R2 | fenced empty | any | retain (covered by application table) |
    use awaken_provisioning_contract::SandboxStatus;

    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone()));
    let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    host.run(
        None,
        "remote-terminal-worker",
        vec![Message::text(
            MessageId("remote-terminal-input".into()),
            Role::User,
            "run",
        )],
    )
    .await
    .expect("Worker-local Session exists");
    let environment = host
        .session_environment("remote-terminal-worker")
        .await
        .expect("Worker-local Environment");
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:incarnation".into(),
        epoch: 3,
        expires_at_unix_ms: 50_000,
    };
    host.install_session_realization_lease("remote-terminal-worker", lease.clone());
    let mut cleanup = awaken_session_contract::SessionCleanupOperation::default();
    assert!(
        cleanup.request("remote-terminal-worker"),
        "R1 terminal fence"
    );
    cleanup
        .freeze_targets("remote-terminal-worker", [], 0, 0)
        .expect("R1 frozen root target");
    let command = cleanup
        .command_for("remote-terminal-worker", "remote-terminal-worker")
        .expect("R1 canonical command");
    *control.commands.lock().unwrap() = Some(vec![command.clone()]);

    assert_eq!(
        host.renew_due_session_realizations(10_000, 60_000)
            .await
            .expect("R1 terminal reconciliation"),
        0,
        "R1/C3"
    );
    assert_eq!(
        environment.status().await.unwrap(),
        SandboxStatus::Terminated,
        "R1/E2"
    );
    assert!(
        !host.session_slots.contains("remote-terminal-worker"),
        "R1/E2/E4"
    );
    let completions = control.completions.lock().unwrap();
    assert_eq!(completions.len(), 1, "R1/E3");
    assert_eq!(completions[0].effect_id, command.effect_id, "R1/E3");
    drop(managed);
}

#[tokio::test]
async fn cold_terminal_assignment_installs_then_uses_the_canonical_cleanup_path() {
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a terminal Session has no process-local slot after
    // Worker replacement; C2 Control returns a typed assignment containing only
    // the frozen projection, an active Repository manifest, and a newly fenced
    // lease but no Run claim; C3 the same Control port later projects the
    // aggregate-owned root command; C4 claim-next is empty after that assignment.
    // Effects: E1 the existing projection synchronizer installs the exact
    // baseline and lease before command polling without re-materializing the
    // Resource being destroyed; E2 the one ManagedHost cleanup executor applies
    // the root effect; E3 the exact receipt returns through Control and removes
    // the slot; E4 the bounded recovery scan stops without a Worker-local queue
    // or duplicate cleanup path.
    // Constraint: an assignment never carries commands, and cleanup cannot run
    // before its lease/projection is locally installed.
    //
    // | Rule | local slot | assignment | command poll | Effect |
    // |---|---|---|---|---|
    // | C1 | absent | one typed/no Run claim | blocked | E1 before poll |
    // | C2 | installed | consumed | root | E2 + E3 |
    // | C3 | removed | none | not entered | E4 |
    let control = Arc::new(RemoteTerminalCleanupControl::default());
    let poll_started = Arc::new(tokio::sync::Notify::new());
    let poll_proceed = Arc::new(tokio::sync::Notify::new());
    *control.poll_barrier.lock().unwrap() = Some((poll_started.clone(), poll_proceed.clone()));
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_session_control(control.clone()));
    let managed = crate::ManagedHost::new(host.clone()).install_dispatch_session_runtime();
    let session_id = "cold-terminal-worker";
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: "remote-worker".into(),
        runtime_incarnation: "remote-worker:replacement".into(),
        epoch: 4,
        expires_at_unix_ms: 50_000,
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
            projection,
            lease: lease.clone(),
        },
    );
    let mut cleanup = awaken_session_contract::SessionCleanupOperation::default();
    assert!(cleanup.request(session_id), "C2 terminal fence");
    cleanup
        .freeze_targets(session_id, [], 0, 0)
        .expect("C3 frozen root target");
    let command = cleanup
        .command_for(session_id, session_id)
        .expect("C3 canonical root command");
    *control.commands.lock().unwrap() = Some(vec![command.clone()]);
    let target = awaken_session_contract::SessionRealizationTarget {
        owner: lease.owner.clone(),
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_expires_at_unix_ms: lease.expires_at_unix_ms,
        renew_existing_lease: false,
        reassign_existing_lease: false,
    };

    let recovery = tokio::spawn({
        let host = host.clone();
        let target = target.clone();
        async move { host.recover_terminal_cleanup_assignments(target).await }
    });
    poll_started.notified().await;
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
    poll_proceed.notify_one();
    assert_eq!(
        recovery.await.unwrap().expect("C2-C4 cold recovery"),
        1,
        "C2/E2"
    );
    assert!(!host.session_slots.contains(session_id), "C2/E2/E3");
    let completions = control.completions.lock().unwrap();
    assert_eq!(completions.len(), 1, "C2/E3");
    assert_eq!(completions[0].effect_id, command.effect_id, "C2/E3");
    assert_eq!(
        control.claim_targets.lock().unwrap().as_slice(),
        [target.clone(), target],
        "C1-C4/E4"
    );
    drop(completions);
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
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications))
    };

    let matching = published_host();
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

    let mismatch = published_host();
    mismatch.register_thread_backend_projection("backend-h2", "acp:claude");
    let error = match mismatch.ctx_for("backend-h2", Some("assistant")).await {
        Ok(_) => panic!("H2 accepted a mismatched backend projection"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("does not match publication"),
        "H2"
    );

    published_host()
        .ctx_for("backend-h3", Some("assistant"))
        .await
        .expect("H3 publication without redundant projection");

    let orphan = SharedHost::new(Arc::new(OkModel), "stub");
    orphan.register_thread_backend_projection("backend-h4", "acp:claude");
    let error = match orphan.ctx_for("backend-h4", Some("assistant")).await {
        Ok(_) => panic!("H4 accepted a backend projection without a publication"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("no immutable Agent publication"),
        "H4"
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
    let host =
        SharedHost::new(Arc::new(OkModel), "stub").with_agent_publications(Arc::new(publications));
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
    let host = SharedHost::new(Arc::new(SessionInferenceProbe(inferences.clone())), "stub")
        .with_agent_publications(Arc::new(publications));
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

    // Cause/effect graph: C1 root/child role selects the public coordination
    // surface; C2 generated/inherited vs immutable publication selects whether
    // Session toolsets enter the executable clone; C3 an explicit Session client
    // descriptor replaces published client ownership. Effects: E1 only a primary
    // exposes fixed list/send; E2 every child loses nested delegation/advisor;
    // E3 generated/self-child embeds Session toolsets; E4 published/non-self
    // retains its publication toolsets; E5 explicit client tools exact-replace.
    //
    // | Rule | role | snapshot owner | Session overlay | Effects |
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
        builtin(awaken_ext_builtin_tools::SEND_TO_AGENT),
        builtin(awaken_ext_builtin_tools::SEND_MESSAGE_TOOL_ID),
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
            ids.contains(awaken_ext_builtin_tools::SEND_TO_AGENT),
            "{rule}/E1"
        );
        assert!(
            !ids.contains(awaken_ext_builtin_tools::AGENT_RUN),
            "{rule}/E1"
        );
        assert!(
            !ids.contains(awaken_ext_builtin_tools::SEND_MESSAGE_TOOL_ID),
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
            awaken_ext_builtin_tools::SEND_TO_AGENT,
            awaken_ext_builtin_tools::SEND_MESSAGE_TOOL_ID,
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
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_agent_publications(Arc::new(publications));

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
    crate::ManagedHost::new(host.clone())
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

    let (_, _, selected) = host
        .resolve_session_publication("frozen-revision", None, None)
        .expect("R1 exact frozen publication");
    assert_eq!(selected, Some(frozen), "R1");

    let (_, _, selected) = host
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
    // FMECA cause/effect graph: C1 the resident Session uses direct foreground
    // ingress; C2 a direct Runtime attempt or pool-owned Session Event attempt has
    // registered its exact local inbox; C3 the direct lifecycle slot is open;
    // C4 the exact registration has settled/been removed; C5 a durable local or
    // Coordinator-only context exists. Effects: E1 C2 exposes the registry's
    // process-local inbox regardless of C1/C5 topology; E2 no registration remains
    // inactive even when C3 or a foreground Run id remains; E3 settled and remote
    // attempts fail closed so callers use committed Session events.
    // Constraint: Runtime's generation/ownership-fenced active-attempt registry is
    // the sole discovery authority. The SessionCtx slot owns only direct lifecycle
    // and carry-over; `active_attempt_registry_is_exact_generation_owned_and_thread_addressed`
    // owns the deeper replacement/lost/unavailable matrix.
    //
    // | Rule | ingress/topology | direct slot | registry | effect |
    // |---|---|---|---|---|
    // | L1 | direct foreground | closed | absent | E2 inactive |
    // | L2 | direct Runtime | open | current | E1 exact inbox |
    // | L3 | direct settled | still open | removed | E2+E3 inactive |
    // | L4 | direct + pool-owned Event | closed | current | E1 exact inbox |
    // | L5 | Event settled | closed | removed | E3 inactive |
    // | L6 | durable local Worker | n/a | current | E1 exact inbox |
    // | L7 | Coordinator-only | n/a | absent | E3 inactive |
    // Decision rule: execute L1-L7; only a current registry entry may produce E1.
    let direct = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let direct_ctx = direct
        .ctx_for("direct-live", None)
        .await
        .expect("L1 direct context");
    assert!(!direct_ctx.durable, "L1 direct foreground precondition");
    assert!(direct.live_inbox("direct-live").await.is_none(), "L1/E2");

    let direct_inbox = direct_ctx.open_live_inbox();
    let direct_tracking = direct_ctx.runtime.track_active_attempt(
        &RunId("run-direct".into()),
        &direct_ctx.thread_id,
        &awaken_runtime_contract::RuntimeRunContext::new().with_live_inbox(direct_inbox.clone()),
    );
    assert!(direct.live_inbox("direct-live").await.is_some(), "L2/E1");
    drop(direct_tracking);
    assert!(
        direct.live_inbox("direct-live").await.is_none(),
        "L3/E2+E3 an open lifecycle slot is not a fallback authority"
    );
    direct_ctx.close_live_inbox();

    let event_inbox = awaken_runtime_contract::live_inbox::LiveInbox::new();
    let event_tracking = direct_ctx.runtime.track_active_attempt(
        &RunId("run-session-event".into()),
        &direct_ctx.thread_id,
        &awaken_runtime_contract::RuntimeRunContext::new().with_live_inbox(event_inbox.clone()),
    );
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
    let local_ctx = local.ctx_for("local-live", None).await.expect("L6 context");
    *local_ctx.active_run.lock().expect("active run mutex") = Some(RunId("run-local".into()));
    let tracking = local_ctx.runtime.track_active_attempt(
        &RunId("run-local".into()),
        &local_ctx.thread_id,
        &awaken_runtime_contract::RuntimeRunContext::new().with_live_inbox(
            local_ctx
                .durable_ingress
                .as_ref()
                .expect("durable ingress")
                .live_inbox()
                .clone(),
        ),
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
