use super::*;
use crate::config::block_text;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse};
use awaken_session_contract::SessionRuntime;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn native_credential_profile() -> awaken_runtime_contract::CredentialRealizationProfile {
    awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native()
}

fn session_environment(
    network: awaken_session_contract::SessionNetworkPolicy,
    sandbox: serde_json::Value,
) -> awaken_session_contract::EnvironmentSnapshot {
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

fn resource_catalog() -> Arc<awaken_resource_store::SqliteResourceStore> {
    Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Catalog"),
    )
}

#[derive(Default)]
pub(super) struct TestResourceLifecycle {
    intents: Mutex<BTreeMap<String, awaken_resource_contract::ResourcePurgeIntent>>,
    references: Mutex<BTreeSet<awaken_resource_contract::ResourceReferenceRecord>>,
    fences: Mutex<BTreeMap<(awaken_resource_contract::ResourceKind, String), String>>,
    fail_replace: AtomicBool,
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
        if self.fail_replace.load(Ordering::SeqCst) {
            return Err(awaken_resource_contract::ResourcePurgeError::Storage(
                "injected reference replacement failure".into(),
            ));
        }
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

pub(super) fn test_resource_lifecycle()
-> Arc<dyn awaken_resource_contract::ResourceLifecycleRepository> {
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
            Some(Arc::new(TestResourceBindingValidator)),
            &config,
            writable,
        ))),
    );
}

struct TestResourceBindingValidator;

impl awaken_resource_contract::ResourceBindingValidator for TestResourceBindingValidator {
    fn validate_memory_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceCatalogError> {
        Ok(())
    }

    fn validate_repository_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: awaken_resource_contract::ConfigVersion,
    ) -> Result<(), awaken_resource_contract::ResourceCatalogError> {
        Ok(())
    }
}

fn managed_with_resource_source(host: Arc<SharedHost>) -> crate::ManagedHost {
    let validator = Arc::new(TestResourceBindingValidator);
    crate::ManagedHost::new(host)
        .with_resource_validator(validator.clone())
        .with_repository_binding_verifier(Arc::new(
            awaken_resource_application::CatalogRepositoryBindingVerifier::new(validator),
        ))
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

fn effective_resources(
    resources: Vec<TestInput>,
) -> awaken_session_contract::ResolvedSessionResources {
    use awaken_resource_contract::{
        BindingId, FileId, MemoryStoreId, RepositoryId, ResourceAccess,
    };
    use awaken_session_contract::{ResolvedInput, ResolvedInputSource};

    awaken_session_contract::ResolvedSessionResources {
        inputs: resources
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
        skills: None,
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
            ),
            selected_plaintext_holder: holder,
        })
    });
    awaken_session_contract::ResolvedSessionResources {
        inputs: vec![awaken_session_contract::ResolvedInput {
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
        skills: None,
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

/// Test composition adapter for runtime-host's dependency-inverted MemoryMounter
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

fn install_test_memory_mounter(host: &SharedHost) {
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

    let report = task.await.expect("join").expect("define_outcome");
    // Round 1 graded needs_revision; the interrupt ended the run before the
    // second round could conclude, so the outcome reports interrupted.
    assert_eq!(report.iterations[0].result, "needs_revision");
    assert_eq!(
        report.iterations.last().expect("a round").result,
        "interrupted"
    );
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

    let report = task.await.expect("join").expect("define_outcome");
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
    let host = SharedHost::new(Arc::new(MemoryHostModel), "stub")
        .with_application_attempt_decorator(Arc::new(move |inner| {
            Arc::new(ObservingAttemptExecutor {
                calls: decorator_calls.clone(),
                inner,
            })
        }));
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
async fn control_frozen_baseline_is_the_only_application_runtime_projection() {
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
        let input = awaken_session_contract::ApplicationSessionInput {
            mounts: with_environment_inputs
                .then(|| serde_json::to_value(&mount).unwrap())
                .into_iter()
                .collect(),
            env: with_environment_inputs
                .then(|| serde_json::to_value(&env).unwrap())
                .into_iter()
                .collect(),
            prompts: vec![prompt.into()],
            mcp_inputs: Vec::new(),
            network_restriction: with_environment_inputs
                .then_some(awaken_session_contract::SessionNetworkPolicy::None),
        };
        let baseline = awaken_session_contract::SessionBaseline::compile(
            awaken_session_contract::SessionBaselineInputs {
                environment: awaken_session_contract::EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_session_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                        "env-fingerprint".into(),
                    ),
                    sandbox: serde_json::json!({}),
                    sandbox_provisioning: Default::default(),
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
                model: "model".into(),
                runtime: None,
                application: Some(
                    awaken_session_contract::ApplicationContributionReceipt::from_input(
                        "plan".into(),
                        &input,
                    ),
                ),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: input.mounts,
                env: input.env,
                prompts: input.prompts,
            },
        );
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: "workspace".into(),
            revision: awaken_session_contract::SessionRevision(2),
            baseline,
            environment: Default::default(),
            resource_revision: 7,
            resources: awaken_session_contract::ResolvedSessionResources {
                inputs: Vec::new(),
                skills: Some(Vec::new()),
            },
            mcp: Vec::new(),
            toolsets: Vec::new(),
        }
    }

    #[derive(Clone, Default)]
    struct PromptRecorder(Arc<Mutex<Vec<ChatRequest>>>);

    #[derive(Default)]
    struct RepositoryClaimRecorder(Mutex<Vec<Option<awaken_run_ingress::RunClaim>>>);

    #[async_trait::async_trait]
    impl crate::RepositoryBindingVerifier<awaken_run_ingress::RunClaim> for RepositoryClaimRecorder {
        async fn verify(
            &self,
            _workspace_id: &str,
            _repository_id: &str,
            _config_version: awaken_resource_contract::ConfigVersion,
            claim: Option<&awaken_run_ingress::RunClaim>,
        ) -> Result<(), awaken_resource_contract::RepositoryBindingVerifierError> {
            self.0.lock().unwrap().push(claim.cloned());
            Ok(())
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

    let recorder = PromptRecorder::default();
    let observed = recorder.0.clone();
    let host = Arc::new(SharedHost::new(Arc::new(recorder), "stub"));
    let repository_claims = Arc::new(RepositoryClaimRecorder::default());
    let _managed = crate::ManagedHost::new(host.clone())
        .with_repository_binding_verifier(repository_claims.clone());
    let frozen = projection("Use the bound Flow project.", true);
    host.install_frozen_session_projection("flow-thread", frozen.clone(), None, true)
        .await
        .expect("first frozen projection installs");
    host.install_frozen_session_projection("flow-thread", frozen, None, true)
        .await
        .expect("same frozen fingerprint is idempotent");

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
    assert_eq!(
        spec.extra
            .as_ref()
            .and_then(|value| value.get("deny_egress"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the Workdir tool wrapper retains the frozen deny intent"
    );
    assert_eq!(
        host.thread_session_prompts("flow-thread"),
        vec!["Use the bound Flow project."]
    );

    // Cause graph:
    // C1 = the frozen Session has a prompt; C2 = committed history is empty;
    // C3 = the activation already carries the exact prompt.
    // Effect E = prepend exactly one deterministic System message.
    //
    // | Rule | C1 | C2 | C3 | E |
    // | P1   | 0  | *  | *  | 0 |
    // | P2   | 1  | 1  | 0  | 1 |
    // | P3   | 1  | 1  | 1  | 0 (deduplicate) |
    // | P4   | 1  | 0  | *  | 0 |
    host.run(None, "no-baseline", user("P1")).await.expect("P1");
    host.install_frozen_session_projection(
        "prompt-thread",
        projection("Use the bound Flow project.", false),
        None,
        true,
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
            "P4 history retains the original fact without reinjection"
        );
        assert_eq!(prompt_count(&requests[3], "exact prompt"), 1, "P3");
    }

    let replacement = projection("different", true);
    assert!(
        host.install_frozen_session_projection("flow-thread", replacement, None, true)
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
    )
    .await
    .expect("C1 first claim");
    host.install_frozen_session_projection(
        "repository-claim-thread",
        repository_projection,
        Some(&claim(2)),
        true,
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
    let state = BoundRunExecutor::new(&host, ctx)
        .execute(activation, RuntimeRunContext::new())
        .await
        .expect("declared tools do not bypass a per-Run deny-all restriction");

    assert!(matches!(state, RunState::Ended(EndCause::NaturalEnd)));
}

/// A model that blocks on its first inference until released, so a concurrent
/// `interrupt` lands while a plain `run` turn is mid-flight.
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

/// The real-turn interrupt the conformance matrix flagged as unasserted: a plain
/// `run` (a managed session's normal turn), interrupted while its inference is in
/// flight, ends `Cancelled` promptly instead of running to completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_ends_an_in_flight_run_as_cancelled() {
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

    // The turn is blocked mid-inference; interrupt it, then release the gate.
    reached.notified().await;
    host.interrupt("t-int").await.expect("interrupt");
    gate.notify_one();

    let result = task.await.expect("join").expect("run");
    assert!(
        matches!(result.state, RunState::Ended(EndCause::Cancelled)),
        "an interrupted in-flight turn ends Cancelled, not run to completion: {:?}",
        result.state
    );
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
/// messages, proving the summary reached the next turn's model input.
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
async fn compaction_summary_reaches_the_same_long_turn() {
    let host = SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction(1, 1);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, "hello")];

    // Turn 1: only the single user message → below threshold, no summary injected.
    let r1 = host.run(None, "t-c", user("u1")).await.expect("turn 1");
    assert!(matches!(r1.state, RunState::Ended(_)));
    let reply1 = r1
        .new_messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| block_text(&m.content))
        .unwrap_or_default();
    assert_eq!(reply1, "no-summary-users=1", "short turn is not compacted");

    // Turn 2: the conversation now exceeds the threshold, so the compact plugin's
    // BeforeInference hook summarizes the older slice inline and the model sees it.
    let r2 = host.run(None, "t-c", user("u2")).await.expect("turn 2");
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
    let host = SharedHost::new(Arc::new(CompactHostModel), "stub").with_compaction(10, 1);
    let user = |id: &str| vec![Message::text(MessageId(id.into()), Role::User, id)];

    host.run(None, "t-before-fold", user("u1"))
        .await
        .expect("turn 1");
    let turn = host
        .run(None, "t-before-fold", user("u2"))
        .await
        .expect("turn 2");
    let reply = turn
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
    let host = SharedHost::new(Arc::new(MemLoopModel), "stub");
    let store = test_memory_store_id();
    bind_test_memory(&host, "thread-1", &store, true);
    bind_test_memory(&host, "thread-2", &store, true);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // Thread 1: the user states a preference; extraction saves it.
    host.run(None, "thread-1", user("I really enjoy tea in the morning"))
        .await
        .expect("thread 1 turn");
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
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
        .expect("thread 2 turn");
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
async fn reopening_a_terminal_thread_recovers_a_missing_extraction_outbox_intent() {
    use awaken_ext_memory::MemoryExtractionRepository as _;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("awaken-memory-outbox-{stamp}"));
    let thread = "memory-outbox-thread";
    let authority = Arc::new(crate::EphemeralRuntimeAuthority::new());

    // Cause/effect recovery table: R1 terminal truth exists and extraction intent
    // is absent -> reopening through the same injected commit authority creates
    // and completes the intent; R2 the extraction repository is durable -> the
    // completed receipt survives the Host replacement. The test intentionally
    // injects authority because runtime-host no longer opens a commit Store.
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
    let run = ctx
        .commit
        .latest_run(&ctx.thread_id)
        .expect("terminal run record");
    assert!(
        second
            .drain_memory(std::time::Duration::from_secs(10))
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
    use awaken_session_contract::{SessionInit, SessionRuntime};

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
            toolsets: None,
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
        .prepare_session("managed-write-a", init("agent", Some(&store_a)))
        .await
        .unwrap();
    managed
        .run(
            "agent",
            "managed-write-a",
            vec![ContentBlock::text("I enjoy tea")],
        )
        .await
        .unwrap();
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
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
        .prepare_session("managed-read-a", init("agent", Some(&store_a)))
        .await
        .unwrap();
    let same = managed
        .run(
            "agent",
            "managed-read-a",
            vec![ContentBlock::text("What do I prefer?")],
        )
        .await
        .unwrap();
    assert_eq!(reply(&same), "tea", "recall reads the same bound store");

    managed
        .prepare_session("managed-read-b", init("agent", Some(&store_b)))
        .await
        .unwrap();
    let other = managed
        .run(
            "agent",
            "managed-read-b",
            vec![ContentBlock::text("What do I prefer?")],
        )
        .await
        .unwrap();
    assert_eq!(reply(&other), "ok", "store B cannot recall store A");

    managed
        .prepare_session("managed-unbound", init("unmanaged-agent", None))
        .await
        .unwrap();
    let unbound = managed
        .run(
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
    use awaken_session_contract::{SessionInit, SessionRuntime};

    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            std::env::temp_dir().join(format!(
                "awaken-memory-recovery-namespace-{}",
                std::process::id()
            )),
            false,
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
        toolsets: None,
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
        async move { managed.prepare_session("memory-replay", init).await }
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
    managed
        .run("agent", "memory-replay", vec![ContentBlock::text("open")])
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
    use awaken_session_contract::SessionRuntime;

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
        .prepare_session("managed-policy", init)
        .await
        .unwrap();
    let outcome = managed
        .run(
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
    assert!(host.drain_memory(std::time::Duration::from_secs(1)).await);
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
/// extractor saves a memory. Proves resume-ended turns trigger the aux agents.
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
        // from the main turn's `write` result that is also in its seeded context.
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
async fn resume_ended_turn_triggers_memory_extraction() {
    let host = SharedHost::new(Arc::new(ResumeMemModel), "stub");
    let store = test_memory_store_id();
    bind_test_memory(&host, "t-res", &store, true);

    // Turn 1 awaits on the Ask-gated `write`.
    let r1 = host
        .run(
            None,
            "t-res",
            vec![Message::text(MessageId("u1".into()), Role::User, "hi")],
        )
        .await
        .expect("turn 1");
    assert!(
        matches!(r1.state, RunState::Awaiting),
        "turn should await on write"
    );
    let pending = r1.pending.expect("a pending tool");

    // RunResume approves the write; the turn now ends and extraction fires.
    let r2 = host
        .resume(
            "t-res",
            &pending.tool_use_id,
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .expect("resume");
    assert!(
        matches!(r2.state, RunState::Ended(_)),
        "resume should end the turn"
    );

    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
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

#[tokio::test]
async fn extraction_cursor_only_processes_new_messages() {
    let host = SharedHost::new(Arc::new(CursorModel), "stub");
    let store = test_memory_store_id();
    bind_test_memory(&host, "t-cur", &store, true);
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    host.run(None, "t-cur", user("alpha"))
        .await
        .expect("turn 1");
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);
    host.run(None, "t-cur", user("beta")).await.expect("turn 2");
    assert!(host.drain_memory(std::time::Duration::from_secs(10)).await);

    // The second extraction saw only "beta" — turn 1's "alpha" was past the cursor.
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
async fn turn_end_fires_background_memory_extraction() {
    let host = SharedHost::new(Arc::new(MemoryHostModel), "stub");
    let store = test_memory_store_id();
    bind_test_memory(&host, "t-mem", &store, true);

    let input = vec![Message::text(
        MessageId("u1".into()),
        Role::User,
        "I really like rust",
    )];
    let result = host.run(None, "t-mem", input).await.expect("run turn");
    assert!(
        matches!(result.state, RunState::Ended(_)),
        "turn should end"
    );

    let drained = host.drain_memory(std::time::Duration::from_secs(10)).await;
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

/// A trivial model for resource-lifecycle turns.
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
    use awaken_session_contract::SessionRuntime;
    let mut raw_host = SharedHost::new(Arc::new(OkModel), "stub");
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            std::env::temp_dir().join(format!(
                "awaken-hot-attach-namespace-{}",
                std::process::id()
            )),
            false,
        );
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());
    let user = |t: &str| vec![Message::text(MessageId(t.into()), Role::User, t)];

    // A blob to mount, and a first turn that builds + caches the thread's sandbox.
    let file_id = host
        .file_application()
        .expect("test composition installs File application")
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
        .expect("first turn");
    let environment_before = host
        .session_environment("t-attach")
        .await
        .expect("session environment");
    let handle_before = environment_before.handle();
    assert!(
        host.session_slots
            .read("t-attach", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "the first turn caches the thread's sandbox ctx"
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

    // The cached sandbox was evicted (so the next turn rebuilds) ...
    assert!(
        !host
            .session_slots
            .read("t-attach", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "attach evicts the cached ctx so the next turn rebuilds with the mount"
    );
    assert_eq!(
        host.session_environment_handle("t-attach").await,
        Some(handle_before.clone()),
        "runtime rebuild retains the one Session environment"
    );
    assert_eq!(
        environment_before
            .list_files("/mnt/session/uploads")
            .await
            .unwrap(),
        vec![("data.txt".into(), b"hello-attached".to_vec())],
        "the live environment receives the file before attach returns"
    );
    // ... and the spec the next turn will build now carries the mount + its bytes.
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
            .list_files(".mnt")
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

/// Repository detach decision table on the resident Workdir environment:
/// C1 a create-time Repository is physically realized; C2 the next durable
/// generation omits it; C3 the Session environment remains resident. E1 removes
/// the exact working tree before the mutation returns and E2 preserves the same
/// environment handle. A stale readable checkout is forbidden.
///
/// | Rule | C1 | C2 | C3 | Effect |
/// | RD1  | T  | T  | T  | E1 removed + E2 retained environment |
#[tokio::test]
async fn applying_repository_detach_removes_the_resident_workdir_checkout() {
    use awaken_session_contract::SessionRuntime;

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
    raw_host.session_provider = crate::session_environment::SessionEnvironmentProvider::workdir(
        fixture.path().join("sandboxes"),
    );
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
        .prepare_session(
            "t-repo-detach",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
    host.run(
        None,
        "t-repo-detach",
        vec![Message::text(
            MessageId("repo-before".into()),
            Role::User,
            "observe repository",
        )],
    )
    .await
    .unwrap();
    let environment = host
        .session_environment("t-repo-detach")
        .await
        .expect("resident environment");
    let handle = environment.handle();
    assert_eq!(
        environment
            .list_files("workspace/live-repo")
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
            .list_files("workspace/live-repo")
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

/// Live replacement commit-failure decision table:
/// | physical realization | reference/manifest commit | effect |
/// |---|---|---|
/// | succeeds | fails | old logical manifest remains; realized target is retryable |
/// | succeeds again | succeeds | desired manifest commits exactly once |
///
/// Constraint: the persisted Session owns the pending generation and retries the
/// complete replacement. Rule C1 proves Runtime ordering does not turn a transient
/// commit failure into a permanently missing mount on that retry.
#[tokio::test]
async fn live_mount_realization_precedes_logical_commit_and_retry_converges() {
    use awaken_session_contract::SessionRuntime;

    let lifecycle = Arc::new(TestResourceLifecycle::default());
    let mut raw_host =
        SharedHost::new(Arc::new(OkModel), "stub").with_resource_lifecycle(lifecycle.clone());
    raw_host.session_provider =
        crate::session_environment::SessionEnvironmentProvider::namespace_with_agent_stderr(
            std::env::temp_dir().join(format!("awaken-hot-attach-retry-{}", std::process::id())),
            false,
        );
    let host = Arc::new(raw_host);
    let managed = managed_with_resource_source(host.clone());
    host.run(
        None,
        "t-attach-retry",
        vec![Message::text(MessageId("initial".into()), Role::User, "hi")],
    )
    .await
    .expect("first turn");
    let environment = host
        .session_environment("t-attach-retry")
        .await
        .expect("live Namespace environment");
    let file_id = host
        .file_application()
        .expect("test composition installs File application")
        .create_uploaded_file(
            host.local_workspace(),
            "retry.txt".into(),
            "text/plain".into(),
            b"retry-safe",
        )
        .await
        .expect("create File")
        .id;
    let desired = effective_resources(vec![TestInput {
        kind: "file".into(),
        id: file_id,
        mount_path: "/retry.txt".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
        initial_branch: None,
        initial_commit: None,
    }]);

    lifecycle.fail_replace.store(true, Ordering::SeqCst);
    managed
        .apply_session_inputs("t-attach-retry", host.local_workspace(), 1, &desired)
        .await
        .expect_err("injected logical commit failure");
    assert!(host.sandbox_spec("t-attach-retry").mounts.is_empty());
    assert_eq!(
        environment
            .list_files("/mnt/session/uploads")
            .await
            .unwrap(),
        vec![("retry.txt".into(), b"retry-safe".to_vec())]
    );

    lifecycle.fail_replace.store(false, Ordering::SeqCst);
    managed
        .apply_session_inputs("t-attach-retry", host.local_workspace(), 1, &desired)
        .await
        .expect("idempotent retry");
    assert_eq!(host.sandbox_spec("t-attach-retry").mounts.len(), 1);
}

/// Causes: a live Workdir environment exists and the replacement manifest adds
/// a read-only File. Constraint: Workdir provides lexical containment but cannot
/// enforce mount immutability. Effect/rule W1: reject before changing the staged
/// manifest, resident files, or cached context.
#[tokio::test]
async fn applying_readonly_file_to_live_workdir_fails_closed_without_partial_projection() {
    use awaken_session_contract::SessionRuntime;

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
    .expect("first turn");
    let environment = host
        .session_environment("t-local-attach")
        .await
        .expect("live Workdir environment");
    let file_id = host
        .file_application()
        .expect("test composition installs File application")
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
            .list_files("mnt/session/uploads")
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
/// C3 no runtime/environment becomes resident; C4 a committed-state GET follows.
/// C1+C2 cause E1 the turn to fail before inference. C3+C4 must cause E2 the
/// query to open only committed truth, return the empty page, and leave the
/// execution environment absent instead of retrying the failing provisioning.
///
/// | Rule | C1 | C2 | C3 | C4 | E1 turn denied | E2 query succeeds/no env |
/// |---|---|---|---|---|---|---|
/// | Q1 | T | T | T | T | T | T |
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
        .expect("test composition installs File application")
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
        .prepare_session(
            "t-query-after-provisioning-denial",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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

    let error = match host
        .run(
            None,
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
    assert!(error.message.contains("does not enforce read-only"), "Q1");
    assert!(
        host.session_environment("t-query-after-provisioning-denial")
            .await
            .is_none(),
        "Q1 failed provisioning must not publish an environment"
    );

    let feed = host
        .run_lifecycle_feed("t-query-after-provisioning-denial")
        .await
        .expect("committed query must not retry Sandbox provisioning");
    let page = awaken_agent_contract::RunLifecycleFeed::events_after(
        feed.as_ref(),
        awaken_agent_contract::LifecycleCursor(0),
        100,
    )
    .await
    .expect("read empty committed lifecycle page");
    assert!(page.events.is_empty(), "Q1 inference never committed a Run");
    assert!(
        host.session_environment("t-query-after-provisioning-denial")
            .await
            .is_none(),
        "Q1 committed query remains free of environment side effects"
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
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = managed_with_resource_source(host.clone());
    let denies = |spec: awaken_provisioning_contract::SandboxSpec| {
        spec.extra
            .as_ref()
            .and_then(|v| v.get("deny_egress"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };
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
            .prepare_session(
                &thread,
                SessionInit {
                    workspace_id: host.local_workspace().into(),
                    agent_id: "a".into(),
                    delegate_ids: Vec::new(),
                    toolsets: None,
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
async fn prepare_session_overlays_the_environment_sandbox_onto_the_spec() {
    use awaken_provisioning_contract::{IsolationClass, NetworkPolicy};
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());

    let init = SessionInit {
        workspace_id: "ws".into(),
        agent_id: "a".into(),
        delegate_ids: Vec::new(),
        toolsets: None,
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
    managed.prepare_session("t-sb", init).await.unwrap();

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
    assert_eq!(
        spec.extra
            .as_ref()
            .and_then(|value| value.get("deny_egress"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
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
        toolsets: None,
        resource_revision: 0,
        resources: Default::default(),
        model: None,
        runtime: None,
        environment: session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    };
    managed.prepare_session("t-bare", bare).await.unwrap();
    assert_eq!(
        host.registered_thread_workspace("t-bare").as_deref(),
        Some("ws"),
        "W2"
    );
    assert_eq!(
        host.sandbox_spec("t-bare").isolation,
        IsolationClass::Workdir
    );
    assert!(!host.sandbox_spec("t-bare").limits.is_set());
}

/// Eager Session creation freezes configuration first; its first context build
/// joins the per-Session lifecycle mutex and waits until the environment is ready.
#[tokio::test]
async fn prepare_session_is_lazy_and_first_turn_materializes_the_environment() {
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());
    managed
        .prepare_session(
            "lazy-environment",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
    host.run(
        Some("assistant"),
        "lazy-environment",
        vec![Message::text(
            MessageId("lazy-user".into()),
            Role::User,
            "hello",
        )],
    )
    .await
    .expect("first turn waits for environment readiness");
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
    use awaken_session_contract::{SessionInit, SessionRuntime};
    use awaken_skill_store::{SkillBundleFile, SkillVersion, bundle_sha256};
    let mut host = SharedHost::new(Arc::new(OkModel), "stub");
    host.deployment.disable_local_pool = true;
    let host = Arc::new(host);
    crate::ManagedHost::new(host.clone())
        .prepare_session(
            "coordinator-dispatch-only",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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

/// L1: `on_tool_use` means inference alone must not allocate a Sandbox.
#[tokio::test]
async fn on_tool_use_text_only_turn_keeps_the_environment_absent() {
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    crate::ManagedHost::new(host.clone())
        .prepare_session(
            "deferred-text",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    host.run(
        Some("assistant"),
        "deferred-text",
        vec![Message::text(MessageId("u1".into()), Role::User, "hello")],
    )
    .await
    .unwrap();
    assert!(host.session_environment("deferred-text").await.is_none());
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

/// L2: instruction-only Skill tools execute in the Brain and do not awaken Hand.
#[tokio::test]
async fn on_tool_use_brain_skill_call_keeps_the_environment_absent() {
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(
        SharedHost::new(Arc::new(BrainSkillModel), "stub").with_skills(vec![
            awaken_ext_skills::SkillSpec::new("think", "Think", "reason", "Think carefully."),
        ]),
    );
    crate::ManagedHost::new(host.clone())
        .prepare_session(
            "deferred-brain",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    host.run(
        Some("assistant"),
        "deferred-brain",
        vec![Message::text(MessageId("u2".into()), Role::User, "skills")],
    )
    .await
    .unwrap();
    assert!(host.session_environment("deferred-brain").await.is_none());
}

/// L3: the Runtime's per-tool target routing sends a Sandbox tool through the
/// deferred Hand; the invoking turn blocks until materialization completes.
#[tokio::test]
async fn on_tool_use_runtime_hand_call_materializes_before_tool_execution() {
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(HandReadModel), "stub"));
    crate::ManagedHost::new(host.clone())
        .prepare_session(
            "deferred-runtime-hand",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

    host.run(
        Some("assistant"),
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
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let filesystem_skill = awaken_ext_skills::SkillSpec {
        environment: awaken_ext_skills::SkillEnvironment::Filesystem,
        dir: Some("skills/files".into()),
        ..awaken_ext_skills::SkillSpec::new("files", "Files", "inspect files", "Read files.")
    };
    let host =
        Arc::new(SharedHost::new(Arc::new(OkModel), "stub").with_skills(vec![filesystem_skill]));
    crate::ManagedHost::new(host.clone())
        .prepare_session(
            "deferred-filesystem-skill",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
                resource_revision: 0,
                resources: Default::default(),
                model: None,
                runtime: None,
                environment: on_tool_use_environment(),
            },
        )
        .await
        .unwrap();

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
            model: "stub".into(),
            runtime: None,
            application: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
        },
    );
    host.install_frozen_session_projection(
        "deferred-legacy-delivered-skill",
        awaken_session_contract::FrozenSessionProjection {
            workspace_id: workspace,
            revision: awaken_session_contract::SessionRevision(1),
            baseline,
            environment: Default::default(),
            resource_revision: 0,
            resources: Default::default(),
            mcp: Vec::new(),
            toolsets: Vec::new(),
        },
        None,
        true,
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
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionEnvironmentBindingSink for BindingOrderSink {
    async fn persist(
        &self,
        session_id: &str,
        _binding: &str,
        _realization: Option<&awaken_session_contract::SessionRealizationLease>,
    ) -> Result<(), awaken_session_contract::RunError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let host = self.host.upgrade().expect("host remains live");
        self.observed_before_publish.store(
            host.session_environment(session_id).await.is_none(),
            Ordering::SeqCst,
        );
        if self.fail {
            Err(awaken_session_contract::RunError::internal(
                "binding store unavailable",
            ))
        } else {
            Ok(())
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
    use awaken_session_contract::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
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
async fn binding_commit_failure_disposes_and_never_publishes_the_environment() {
    use awaken_session_contract::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
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
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: false,
    });
    let managed = crate::ManagedHost::new(host.clone());
    managed.install_environment_binding_sink(sink.clone());
    managed
        .prepare_session(
            "deferred-hand",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let sink = Arc::new(BindingOrderSink {
        host: Arc::downgrade(&host),
        calls: AtomicUsize::new(0),
        observed_before_publish: std::sync::atomic::AtomicBool::new(false),
        fail: true,
    });
    let managed = crate::ManagedHost::new(host.clone());
    managed.install_environment_binding_sink(sink.clone());
    managed
        .prepare_session(
            "deferred-failure",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "assistant".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
async fn prepare_session_mounts_an_effective_memory_resource() {
    use awaken_session_contract::{SessionInit, SessionRuntime};
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
        toolsets: None,
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
    managed.prepare_session("t-bound", bare("a")).await.unwrap();
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
    read_only.resources.inputs[0].access = awaken_resource_contract::ResourceAccess::ReadOnly;
    managed
        .prepare_session("t-read-only", read_only)
        .await
        .unwrap();
    assert_eq!(
        host.sandbox_spec("t-read-only").mounts[0].access,
        awaken_provisioning_contract::MountAccess::ReadOnly
    );

    // An empty effective input set mounts nothing.
    managed
        .prepare_session("t-unbound", bare("no-bindings"))
        .await
        .unwrap();
    let empty = serde_json::to_string(&host.sandbox_spec("t-unbound").mounts).unwrap();
    assert!(
        !empty.contains("BANANA-42"),
        "an unbound agent mounts nothing extra: {empty}"
    );
}

#[tokio::test]
async fn activation_applies_current_resource_state_as_a_deny_only_overlay() {
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceCatalog,
        ResourceState,
    };
    use awaken_session_contract::SessionRuntime;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    let catalog = resource_catalog();
    catalog
        .create_memory_store(
            MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: host.local_workspace().into(),
                name: "memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        )
        .unwrap();
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
        .set_memory_state(host.local_workspace(), &store_id, ResourceState::Suspended)
        .unwrap();
    let managed = crate::ManagedHost::new(host.clone()).with_resource_validator(catalog.clone());
    let mut init = bare_session("a", host.local_workspace());
    init.resources = manifest;

    let error = managed
        .prepare_session("t-suspended", init)
        .await
        .unwrap_err();

    assert!(error.message.contains("not active"));
    assert!(host.sandbox_spec("t-suspended").mounts.is_empty());
}

#[tokio::test]
async fn memory_activation_enforces_catalog_workspace_without_iam_policy_logic() {
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceCatalog,
        ResourceState,
    };
    use awaken_session_contract::SessionRuntime;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let store_id = test_memory_store_id();
    let catalog = resource_catalog();
    catalog
        .create_memory_store(
            MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: "workspace-a".into(),
                name: "private-memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        )
        .unwrap();
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
        .prepare_session("wrong-workspace", init)
        .await
        .unwrap_err();
    assert!(error.message.contains("not found"));
    assert!(host.sandbox_spec("wrong-workspace").mounts.is_empty());
    assert!(host.memory_for_thread("wrong-workspace").is_none());
}

/// The same effective input contract realizes File and Repository resources without
/// exposing their authoring repository to Runtime.
#[tokio::test]
async fn prepare_session_mounts_effective_file_and_stages_effective_repo() {
    use awaken_provisioning_contract::{MountAccess, MountSource};
    use awaken_session_contract::{SessionInit, SessionRuntime};
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));

    // Seed a file blob and pass the already-resolved File and Repository inputs.
    let binary = vec![0, 0xff, b'R', 0x80, b'\n'];
    let record = host
        .file_application()
        .expect("test composition installs File application")
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
        .prepare_session(
            "t-multi",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
        repositories[0].plan.remote_url,
        "https://github.com/awaken/example.git"
    );
}

#[tokio::test]
async fn file_activation_rejects_bytes_that_do_not_match_the_file_id() {
    // Cause/effect decision rule D1: the authoritative File application returns
    // bytes whose canonical digest differs from the logical record's blob id ->
    // staging fails closed before a Sandbox mount is published.
    use awaken_file_store::{FileStore, FileStoreError};
    use awaken_session_contract::SessionRuntime;

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
            .resource_lifecycle()
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

    let error = managed.prepare_session("t-corrupt-file", init).await;

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
    use awaken_session_contract::SessionRuntime;

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let file_id = host
        .file_application()
        .expect("test composition installs File application")
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

    let error = managed.prepare_session("t-cross-workspace", init).await;

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

    impl crate::CredentialRefreshFactory for ExactRefreshFactory {
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
    let credential = awaken_credential_vault::repo::enter_credential(
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
            credential: Some(CredentialAccess::new(
                CredentialRef {
                    id: credential.id.0.clone(),
                    revision,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
            )),
            prompts_as_skills: false,
            selected_plaintext_holder: Some(holder.clone()),
        }
    };

    // Cause graph: exact workspace + revision + allowed Worker holder -> Native
    // host material is staged but invisible; durable publication command -> visible.
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
        .stage_mcp_attachment(renewal)
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
            .publish_mcp_generation(renewed_generation)
            .await
            .is_err(),
        "H8"
    );
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
        "H19"
    );
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-prompt-skill"))
            .is_none(),
        "H19"
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
            _handle: &awaken_provisioning_contract::SandboxHandle,
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
    let projected = crate::mcp::project_mcp_transport(
        projection.server.as_ref().expect("H20 private material"),
        &secure_generation,
        secure_acp_host.mcp_relay.get(),
    )
    .expect("H20 project opaque route");
    let route = match projected.transport {
        awaken_run_executor_acp::McpTransport::Http { url } => url,
        other => panic!("H20 expected HTTP relay route, got {other:?}"),
    };
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
    let seen = seen.lock().unwrap();
    assert!(seen.len() > seen_before, "H20 route reached upstream");
    assert!(
        seen[seen_before..]
            .iter()
            .all(|(_, bearer)| bearer == "Bearer published-mcp-token"),
        "H20 every relay request uses only Worker-held material: {seen:?}"
    );
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
    let managed =
        crate::ManagedHost::new(host.clone()).with_mcp_attachment_realizer(external.clone());
    drop(managed);
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
    use crate::mcp::{McpTransportMaterial, McpWiring, project_mcp_transport};
    use crate::session_slot::{McpGenerationProjection, McpProjectionState};
    use awaken_run_executor_acp::McpTransport;
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
    let projection = |number: u64, secret: &str| McpGenerationProjection {
        request: awaken_session_contract::StageMcpAttachment {
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
        },
        receipt: McpRealizationReceipt {
            generation: generation(number),
            realization_id: format!("realize-{number}"),
            selected_plaintext_holder: None,
            actual_realization_kind: None,
            receipt_fingerprint: format!("fingerprint-{number}"),
        },
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
        }),
        mcp_process: None,
        state: McpProjectionState::Staged,
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
    let acp = project_mcp_transport(
        visible[0].server.as_ref().unwrap(),
        &visible[0].request.generation,
        Some(&relay),
    )
    .unwrap();
    let old_route = match acp.transport {
        McpTransport::Http { url } => url,
        other => panic!("P2 expected HTTP transport, got {other:?}"),
    };
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
    let replacement = project_mcp_transport(
        visible[0].server.as_ref().unwrap(),
        &visible[0].request.generation,
        Some(&relay),
    )
    .unwrap();
    let new_route = match replacement.transport {
        McpTransport::Http { url } => url,
        other => panic!("P4 expected HTTP transport, got {other:?}"),
    };
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
    use awaken_session_contract::{SessionInit, SessionRuntime};
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
        .prepare_session(
            "t-gh",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
                resource_revision: 0,
                resources: effective_repository(
                    "repo-1",
                    "https://github.com/awaken/example.git",
                    "/workspace/repo",
                    Some(credential.id.0),
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

    assert_eq!(
        host.thread_repository_activations("t-gh")[0]
            .credential
            .as_ref()
            .map(|credential| credential.expose_password().to_string()),
        Some("ghp_secret_token".to_string()),
        "the exact Resource realization still receives its credential"
    );
    assert!(
        host.active_mcp_projections("t-gh").is_empty(),
        "Repository realization cannot manufacture Session MCP authority"
    );
}

/// Worker composition cause graph: C1 dispatch Session Runtime installed from
/// the Managed adapter -> C2 outer builder released -> C3 validator present ->
/// C4 exact credential materializer present -> E1 Repository material stages.
/// Missing C4 rejects through the same runtime rather than a fallback path.
///
/// | Rule | C1 | C2 | C3 | C4 | Result |
/// |---|---|---|---|---|---|
/// | D1 | T | T | T | T | exact material staged |
/// | D2 | T | T | T | F | reject, no activation |
#[tokio::test]
async fn worker_dispatch_resource_runtime_survives_assembly_and_fails_closed() {
    for (rule, install_credentials) in [("D1", true), ("D2", false)] {
        let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let source = awaken_credential_vault::repo::enter_credential(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: host.local_workspace().into(),
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: Some("git".into()),
                env_key: None,
                secret: Some(http_basic_material("git", "dispatch-repository-secret")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("author dispatch Repository credential");
        let managed = managed_with_resource_source(host.clone());
        let managed = if install_credentials {
            managed.with_credentials(credentials, secrets)
        } else {
            managed
        };
        drop(managed);

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
        if install_credentials {
            result.unwrap_or_else(|error| panic!("{rule}: {error}"));
            assert_eq!(
                host.thread_repository_activations(&thread).len(),
                1,
                "{rule}"
            );
            assert_eq!(
                host.thread_repository_activations(&thread)[0]
                    .credential
                    .as_ref()
                    .map(|material| material.expose_password()),
                Some("dispatch-repository-secret"),
                "{rule}"
            );
        } else {
            let error = result.expect_err("D2 must reject").to_string();
            assert!(
                error.contains("configured credential vault"),
                "{rule}: {error}"
            );
            assert!(
                host.thread_repository_activations(&thread).is_empty(),
                "{rule}"
            );
        }
    }
}

/// Repository realization cause graph:
/// C1 binding/pin cardinality exact -> C2 source id/usage exact -> C3 holder
/// allowed and model exposure forbidden -> C4 Worker holder exact -> C5 source
/// revision active in the exact Workspace -> E1 ephemeral material staged.
/// Anonymous input bypasses C2-C5; the first failed cause
/// terminates without another credential or holder selection.
///
/// | Rule | Credential | C1 | C2 | C3 | C4 | C5 | Result |
/// |---|---|---|---|---|---|---|---|
/// | H1 | absent | T | - | - | - | - | anonymous |
/// | H2 | present | T | T | T | T | T | exact material |
/// | H3 | present | F | - | - | - | - | reject missing pin |
/// | H4 | present | T | F | - | - | - | reject source mismatch |
/// | H5 | present | T | T | F | - | - | reject usage mismatch |
/// | H6 | absent | F | - | - | - | - | reject extra pin |
/// | H7 | present | T | T | F | - | - | reject unauthorized holder |
/// | H8 | present | T | T | F | - | - | reject virtual exposure |
/// | H9 | present | T | T | T | F | - | reject unsupported holder |
/// | H10 | present | T | T | T | T | F | reject stale revision |
/// | H11 | present | T | T | T | T | F | reject inactive source |
/// | H12 | present | T | T | T | T | F | reject cross-Workspace source |
/// | H13 | present | T | T | T | T | scalar | reject material kind |
#[tokio::test]
async fn repository_credential_realization_follows_the_decision_table() {
    use awaken_session_contract::{SessionInit, SessionRuntime};

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
        let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let mut source = awaken_credential_vault::repo::enter_credential(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: if matches!(rule.case, Case::CrossWorkspace) {
                    "another-workspace".into()
                } else {
                    host.local_workspace().into()
                },
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: Some("git".into()),
                env_key: None,
                secret: Some(if matches!(rule.case, Case::WrongMaterial) {
                    awaken_agent_contract::RedactedString::new("legacy-scalar-token")
                } else {
                    http_basic_material("git", "repository-decision-secret")
                }),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
        )
        .await
        .expect("author exact Repository credential");
        if matches!(rule.case, Case::InactiveSource) {
            source.status = awaken_credential_vault::CredentialStatus::Disabled;
            awaken_credential_vault::repo::CredentialRepo::put(
                credentials.as_ref(),
                source.clone(),
            )
            .await
            .expect("disable exact Repository credential");
        }
        let managed =
            managed_with_resource_source(host.clone()).with_credentials(credentials, secrets);
        let binding = (!matches!(rule.case, Case::Anonymous)).then(|| source.id.0.clone());
        let mut resources = effective_repository(
            "repo-1",
            "https://github.com/awaken/example.git",
            "/workspace/repo",
            binding,
        );
        let awaken_session_contract::ResolvedInputSource::Repository {
            config, credential, ..
        } = &mut resources.inputs[0].source
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
        let thread = format!("repository-decision-{}", rule.id);
        let result = managed
            .prepare_session(
                &thread,
                SessionInit {
                    workspace_id: host.local_workspace().into(),
                    agent_id: "a".into(),
                    delegate_ids: Vec::new(),
                    toolsets: None,
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
        match rule.expected_error {
            None => {
                result.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
                let staged = host.thread_repository_activations(&thread);
                assert_eq!(staged.len(), 1, "{}", rule.id);
                assert_eq!(
                    staged[0].credential.is_some(),
                    matches!(rule.case, Case::Exact),
                    "{}",
                    rule.id
                );
            }
            Some(fragment) => {
                let error = result.expect_err("decision row must reject").to_string();
                assert!(error.contains(fragment), "{}: {error}", rule.id);
            }
        }
    }
}

/// Applying a repository manifest with a new credential reference re-keys the
/// Resource realization only; it still creates no MCP projection.
#[tokio::test]
async fn rotating_a_github_repository_credential_re_keys_only_the_clone() {
    use awaken_session_contract::{SessionInit, SessionRuntime};
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
        .prepare_session(
            "t-rot",
            SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "a".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
                resource_revision: 0,
                resources: effective_repository(
                    "repo-1",
                    "https://github.com/awaken/example.git",
                    "/workspace/repo",
                    Some(credential.id.0),
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

    let clone_password = |h: &SharedHost| {
        h.thread_repository_activations("t-rot")[0]
            .credential
            .as_ref()
            .map(|t| t.expose_password().to_string())
    };
    assert_eq!(clone_password(&host).as_deref(), Some("ghp_old"));

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
        Some(next_credential.id.0),
    );

    // The Managed adapter stores the supplied credential in the Vault and publishes a
    // new Repository config before invoking this complete-manifest runtime port.
    managed
        .apply_session_inputs("t-rot", host.local_workspace(), 1, &next)
        .await
        .unwrap();

    assert_eq!(
        clone_password(&host).as_deref(),
        Some("ghp_new"),
        "clone credential rotated"
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
        toolsets: None,
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
    use awaken_session_contract::SessionRuntime;
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
    managed.prepare_session("t-g1", init).await.unwrap();

    let realized = ".mnt/mnt/memory";
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
    // E3 no automatic-memory binding is selected by ordinary Managed resources.
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
            .prepare_session(&thread, init)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.rule));

        let spec = host.sandbox_spec(&thread);
        assert_eq!(spec.mounts.len(), count, "{} E1", case.rule);
        for (index, mount) in spec.mounts.iter().enumerate() {
            let expected = match expected_access[index] {
                ResourceAccess::ReadOnly => awaken_provisioning_contract::MountAccess::ReadOnly,
                ResourceAccess::ReadWrite => awaken_provisioning_contract::MountAccess::ReadWrite,
            };
            assert_eq!(mount.access, expected, "{} E2/{index}", case.rule);
        }
        let prompts = host.thread_session_prompts(&thread);
        assert_eq!(prompts.len(), count, "{} E1 prompts", case.rule);
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
    use awaken_session_contract::SessionRuntime;

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
        managed.prepare_session(&thread, init).await.unwrap();
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
    use awaken_session_contract::SessionRuntime;
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
    managed.prepare_session("t-g3", init).await.unwrap();

    // Exactly one memory mount at that path, and it is the wire store S2.
    let spec = host.sandbox_spec("t-g3");
    let at_path: Vec<_> = spec
        .mounts
        .iter()
        .filter(|mount| mount.mount_path == ".mnt/mnt/memory")
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
    use awaken_session_contract::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone()).with_resource_validator(resource_catalog());
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

    let result = managed.prepare_session("t-g4", init).await;
    assert!(
        result.is_err(),
        "a binding to a missing backing store must fail closed, not mount empty"
    );
}

#[tokio::test]
async fn activation_validates_the_frozen_config_without_selecting_current_again() {
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, ResourceCatalog,
        ResourceState,
    };
    use awaken_session_contract::{ResolvedInputSource, SessionRuntime};

    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let workspace = host.local_workspace().to_string();
    let store_id = test_memory_store_id();
    let catalog = resource_catalog();
    catalog
        .create_memory_store(
            MemoryStoreDefinition {
                id: store_id.clone().into(),
                workspace_id: workspace.clone(),
                name: "memory".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        )
        .unwrap();
    catalog
        .publish_memory_config(
            &workspace,
            ConfigVersion::INITIAL,
            MemoryStoreConfigVersion {
                memory_store_id: store_id.clone().into(),
                version: ConfigVersion(2),
                retention_policy: Default::default(),
            },
        )
        .unwrap();

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
        .prepare_session("frozen-v1", valid)
        .await
        .expect("v1 remains valid after current advances to v2");

    let mut missing = manifest;
    let ResolvedInputSource::MemoryStore { config, .. } = &mut missing.inputs[0].source else {
        panic!("expected MemoryStore input");
    };
    config.version = ConfigVersion(3);
    let mut invalid = bare_session("a", &workspace);
    invalid.resources = missing;
    let error = managed
        .prepare_session("missing-v3", invalid)
        .await
        .expect_err("an absent frozen config must fail closed");
    assert!(error.message.contains("config version"));
    assert!(host.sandbox_spec("missing-v3").mounts.is_empty());

    catalog
        .set_memory_state(&workspace, &store_id, ResourceState::Archived)
        .unwrap();
    let error = match managed
        .run(
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
    use awaken_session_contract::SessionRuntime;
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
    init.resources.skills = Some(vec![awaken_session_contract::ResolvedSkillBinding {
        kind: awaken_agent_contract::AgentSkillKind::Custom,
        skill_id: "governed".into(),
        version: 1,
        bundle_sha256: hash,
    }]);
    managed
        .prepare_session("skill-revoke", init)
        .await
        .expect("prepare pinned Skill");
    managed
        .run(
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
            &awaken_session_contract::ResolvedSessionResources {
                inputs: Vec::new(),
                skills: Some(Vec::new()),
            },
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
    use awaken_session_contract::SessionRuntime;
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
        .prepare_session("t-g5-worker", init)
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
        awaken_session_contract::ResolvedSessionResources {
            inputs: Vec::new(),
            skills: Some(Vec::new()),
        },
    );
    host.register_thread_resource_manifest(thread, manifest.clone());
    let snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
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

#[test]
fn durable_dispatch_marks_only_a_prepared_root_session_for_worker_realization() {
    // Cause/effect graph: C1 a Coordinator has installed the frozen Session
    // runtime projection; C2 only a Resource manifest exists; C3 a child Run is
    // parent-mediated. Effects: E1 the root dispatch names its own Session and
    // the Worker enters Control realization; E2 an ordinary resource-bearing
    // Run remains ordinary; E3 a child retains the parent Session pointer. C1
    // and C2 are mutually exclusive test fixtures here; C3 is owned by
    // `child_dispatch_reuses_publication_pinned_model_candidates`.
    //
    // | Rule | Frozen runtime | Resources only | Child | session_thread_id |
    // | R1   | yes            | any            | no    | root thread       |
    // | R2   | no             | yes            | no    | none              |
    // | R3   | n/a            | any            | yes   | parent thread     |
    //
    // This test owns R1. The adjacent resource-envelope test owns R2 and the
    // existing child-dispatch test owns R3, avoiding a parallel child builder.
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let thread = "prepared-root-session";
    host.install_environment_projection(
        thread,
        &session_environment(
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            serde_json::json!({}),
        ),
    )
    .expect("install frozen Session runtime projection");
    let activation = awaken_runtime_contract::RunActivation::new(
        awaken_agent_contract::agent::run::Id("run-prepared-root-session".into()),
        awaken_agent_contract::agent::thread::Id(thread.into()),
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
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
fn cold_host_inference_holder_follows_the_candidate_backend_decision_table() {
    // Cause graph: C1=credential-bearing candidate; C2=Native; C3=ACP;
    // C4=mixed boundaries. The same decision feeds direct and dispatch paths.
    // | Rule | C1 | C2 | C3 | C4 | result          |
    // | R1   | F  | -  | -  | F  | no holder       |
    // | R2   | T  | T  | F  | F  | Worker holder   |
    // | R3   | T  | F  | T  | F  | Workload holder |
    // | R4   | T  | T  | T  | T  | reject          |
    let host = SharedHost::new(Arc::new(OkModel), "host-default");
    let candidate = |model: &str, backend: &str| {
        awaken_runtime_contract::resolved::ResolvedModelCandidate::provider(
            awaken_runtime_contract::resolved::ModelBinding::new("provider", model, backend),
            "provider@1",
            "route@1",
            "workspace-a",
            Some(awaken_runtime_contract::CredentialAccess::new(
                awaken_runtime_contract::CredentialRef {
                    id: format!("credential-{model}"),
                    revision: 1,
                },
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_runtime_contract::CredentialUsage::ProviderAdapter,
                awaken_runtime_contract::CredentialExecutionPolicy::self_hosted_provider(),
            )),
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "test".into(),
                api_dialect: String::new(),
                base_url: "https://example.invalid".into(),
                upstream_model: model.into(),
            },
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
    assert!(
        host.inference_plaintext_holder(&activation(
            candidate("native", "native"),
            vec![candidate("acp", "acp:codex")],
        ))
        .is_err(),
        "R4"
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
        .adopt(&handle)
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
        .adopt(&resident_handle)
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
        .adopt(&retained_handle)
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
// not be able to start a second turn on an awaiting thread, resume a run that never
// awaiting, answer the wrong pending tool, or cross the built-in↔client-executed
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

/// A thread awaiting on a tool decision must reject a fresh `run`: starting a second
/// turn over an awaiting run would double-execute the awaiting turn's side effects. The
/// guard fails closed with BadRequest and does not touch the await.
#[tokio::test]
async fn run_on_an_awaiting_thread_fails_closed() {
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let r1 = host
        .run(None, "t-awaiting", user("hi"))
        .await
        .expect("turn 1");
    assert!(
        matches!(r1.state, RunState::Awaiting),
        "turn awaits on write"
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
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
        )
        .await
        .expect("resume the untouched await");
    assert!(matches!(r2.state, RunState::Ended(_)));
}

/// Resuming a thread that has no awaiting run is a caller error, not a panic: there
/// is no run to answer, so it fails closed with BadRequest.
#[tokio::test]
async fn resume_with_no_awaiting_run_fails_closed() {
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let err = host
        .resume(
            "t-idle",
            "w1",
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
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
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let r1 = host
        .run(None, "t-wrongid", user("hi"))
        .await
        .expect("turn 1");
    assert!(matches!(r1.state, RunState::Awaiting));

    let err = host
        .resume(
            "t-wrongid",
            "not-the-pending-id",
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
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
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
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
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    let r1 = host.run(None, "t-bind1", user("hi")).await.expect("turn 1");
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
}

/// The other direction of the binding: a confirmation may not answer a
/// client-executed tool (which expects a result, not a permission decision).
#[tokio::test]
async fn confirm_cannot_answer_a_client_tool() {
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_client_tools(HashSet::from(["lookup".to_string()]));
    let r1 = host.run(None, "t-bind2", user("hi")).await.expect("turn 1");
    let pending = r1.pending.expect("awaiting on the client tool");
    assert!(pending.client_executed, "lookup is client-executed");

    let err = host
        .resume(
            "t-bind2",
            &pending.tool_use_id,
            HostResume::ToolPermission {
                allow: true,
                note: None,
            },
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
}

/// The happy path for the client-executed binding: a `ClientResult` delivers the
/// caller-run tool's output, it reaches the model's next inference, and the turn
/// ends. This is the direct-ingress row of the resume-delivery decision table;
/// `durable_client_result_settles_the_authoritative_dispatch` covers the durable
/// row against the same model and protocol-neutral Host API.
#[tokio::test]
async fn client_result_delivers_a_client_tool_result_and_ends_the_turn() {
    // Cause/effect graph: C1 direct ingress; C2 valid committed client-tool
    // ticket; C3 exact ClientResult. Effects: E1 resume executes inline once;
    // E2 result reaches the next inference; E3 Run ends. Constraint: no durable
    // dispatch is authored in direct mode.
    //
    // | Rule | ingress | ticket | answer | Effects |
    // | R1 | direct | valid client tool | exact result | E1+E2+E3 |
    let host = SharedHost::new(Arc::new(ClientLookupModel), "stub")
        .with_client_tools(HashSet::from(["lookup".to_string()]));
    let r1 = host
        .run(None, "t-client", user("hi"))
        .await
        .expect("turn 1");
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
    assert!(matches!(r2.state, RunState::Ended(_)), "the turn ends");
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

#[tokio::test(flavor = "multi_thread")]
async fn durable_client_result_settles_the_authoritative_dispatch() {
    // Cause/effect graph: C1 durable ingress; C2 a claimed Run settles Awaiting
    // on a client-tool ticket; C3 the exact ClientResult arrives; C4 resumed work
    // ends; C5 resumed work awaits on a new ticket. Effects: E1 input is appended
    // to the canonical durable Inbox; E2 the Worker alone claims/resumes/settles;
    // E3 Done removes the dispatch row; E4 Awaiting retains exactly one row; E5
    // committed result reaches the model. Constraints: direct ingress remains the
    // R1 path above; one ticket accepts one idempotency identity.
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

    let first = host
        .run(None, "t-durable-client", user("hi"))
        .await
        .expect("durable turn awaits");
    assert!(matches!(first.state, RunState::Awaiting), "R2 precondition");
    let pending = first.pending.expect("client tool ticket");
    let awaiting = dispatch
        .list_dispatches()
        .await
        .expect("list awaiting dispatch");
    assert_eq!(awaiting.len(), 1, "one durable dispatch owns the wait");
    assert_eq!(awaiting[0].run_id, first.run_id);
    assert_eq!(
        awaiting[0].state,
        awaken_run_ingress::DispatchState::Awaiting
    );

    let resumed = host
        .resume(
            "t-durable-client",
            &pending.tool_use_id,
            HostResume::ClientResult {
                content: vec![ContentBlock::text("sunny")],
                is_error: false,
            },
        )
        .await
        .expect("durable worker resumes the client result");
    assert!(matches!(resumed.state, RunState::Ended(_)), "R2/E5");
    assert!(
        dispatch
            .list_dispatches()
            .await
            .expect("list settled dispatches")
            .iter()
            .all(|summary| summary.run_id != resumed.run_id),
        "R2/E3: terminal settlement removes the authoritative dispatch row"
    );
}

#[tokio::test]
async fn pending_client_tool_query_uses_committed_ticket_during_projection_gap() {
    // Cause/effect graph: C1=the Runtime has atomically committed a client-tool
    // call and its Awaiting ticket; C2=the foreground protocol has not yet copied
    // that position into disposable SessionState; C3=a peer protocol queries the
    // pending tool; C4=a fresh user turn races that wait; C5=the exact client
    // result arrives. E1=the exact committed call is returned as client-executed;
    // E2=no pending tool is fabricated when committed truth has no open wait;
    // E3=the fresh turn is rejected; E4=the original Run resumes and ends.
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
        Ok(_) => panic!("committed wait must reject a competing user turn"),
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
    let host = SharedHost::new(Arc::new(AwaitOnWriteModel), "stub");
    // Await first so the supersede path is not short-circuited by the awaiting guard
    // (supersede is allowed on an awaiting thread; the durable check is what must fire).
    let r1 = host.run(None, "t-sup", user("hi")).await.expect("turn 1");
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

/// A terminal session end (managed session delete/archive) disposes the thread's
/// sandbox — the ONLY place it is reaped. Proven end-to-end through the
/// `SessionRuntime` port (`ManagedHost::end_session`): the cached ctx is evicted
/// AND the live sandbox's workspace dir is actually reaped (its `status` flips
/// `Ready` → `Terminated`), unlike the evict-to-rebuild edges (attach/detach/
/// rebind) which keep the per-thread workspace so the next turn reuses it.
#[tokio::test]
async fn end_session_disposes_the_threads_sandbox() {
    use awaken_provisioning_contract::SandboxStatus;
    use awaken_session_contract::SessionRuntime;
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let managed = crate::ManagedHost::new(host.clone());

    // A first turn builds + caches the thread's sandbox.
    host.run(
        None,
        "t-end",
        vec![Message::text(MessageId("hi".into()), Role::User, "hi")],
    )
    .await
    .expect("first turn");
    // Hold the live sandbox handle before teardown so we can observe its disposal
    // even after the ctx is evicted from the registry.
    let env = host
        .session_environment("t-end")
        .await
        .expect("the first turn caches the thread's sandbox ctx");
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

    // End the session at the terminal edge.
    managed.end_session("t-end").await.expect("end_session");

    // The cached ctx is evicted ...
    assert!(
        !host
            .session_slots
            .read("t-end", |slot| slot.runtime.is_some())
            .unwrap_or(false),
        "end_session evicts the cached ctx"
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
        "end_session disposes the sandbox (workspace reaped), unlike an evict-rebuild"
    );
    assert!(host.registered_thread_workspace("t-end").is_none());
    assert!(!host.session_slots.contains("t-end"));
    assert!(host.inference_routing.override_for("t-end").is_none());
    assert_eq!(
        host.sandbox_spec("t-end").network,
        awaken_provisioning_contract::NetworkPolicy::Unrestricted
    );

    // Idempotent: ending an already-ended or never-created session is a clean no-op.
    managed
        .end_session("t-end")
        .await
        .expect("end_session is idempotent");
    managed
        .end_session("never-existed")
        .await
        .expect("end_session is a no-op for an unknown thread");
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

#[tokio::test]
async fn cold_session_uses_its_frozen_agent_projection_for_internal_history_reads() {
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
    crate::ManagedHost::new(host.clone())
        .prepare_session(
            "cold-agent",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
async fn frozen_projection_replaces_an_inactive_default_runtime_context() {
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
    //
    // P1 and P2 are the distributed authority-transition regressions exercised
    // here. P3 is covered by
    // `cold_session_uses_its_frozen_agent_projection_for_internal_history_reads`
    // and the idempotent projection tests above.
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

    let stale = host
        .ctx_for("late-projection", None)
        .await
        .expect("pre-projection durable operation can open a default context");
    assert_eq!(stale.config.root_agent_id.0, "assistant", "P1 precondition");

    crate::ManagedHost::new(host.clone())
        .prepare_session(
            "late-projection",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
        .prepare_session(
            "active-projection",
            awaken_session_contract::SessionInit {
                workspace_id: host.local_workspace().into(),
                agent_id: "agent-a".into(),
                delegate_ids: Vec::new(),
                toolsets: None,
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
}

#[tokio::test]
async fn live_inbox_is_advertised_only_for_a_locally_reachable_active_attempt() {
    // FMECA cause/effect graph:
    // C1 execution topology owns a local dispatch pool; C2 the Session context
    // exists; C3 an attempt is active. E1 expose the exact process-local inbox;
    // E2 report inactive so callers use durable Session events; E3 never accept
    // a message into a Coordinator-only inbox that a remote Worker cannot read.
    //
    // | Rule | local pool | context | active | effect |
    // |---|---|---|---|---|
    // | L1 | yes | yes | yes | E1 reachable inbox |
    // | L2 | yes | yes | no | E2 inactive |
    // | L3 | no | yes | yes | E2 + E3 fail closed |
    let mut local_deployment = crate::DeploymentConfig::ephemeral();
    local_deployment.durable = true;
    let local = Arc::new(SharedHost::new_with_deployment(
        Arc::new(OkModel),
        "stub",
        local_deployment,
    ));
    let local_ctx = local.ctx_for("local-live", None).await.expect("L1 context");
    assert!(local.live_inbox("local-live").await.is_none(), "L2");
    *local_ctx.active_run.lock().expect("active run mutex") = Some(RunId("run-local".into()));
    assert!(local.live_inbox("local-live").await.is_some(), "L1");

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
        .expect("L3 context");
    *remote_ctx.active_run.lock().expect("active run mutex") = Some(RunId("run-remote".into()));
    assert!(remote.live_inbox("remote-live").await.is_none(), "L3");
}

/// Cause/effect design:
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

    host.prewarm_cache_volume("build-cache-v1", "/tmp/awaken-cache-volume-wiring")
        .await
        .expect("same identity is already prepared");
    assert_eq!(initializer.0.load(Ordering::SeqCst), 1, "E3");
    environment
        .dispose()
        .await
        .expect("dispose fixture environment");
}
