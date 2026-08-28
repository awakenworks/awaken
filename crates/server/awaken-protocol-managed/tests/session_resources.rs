//! Post-creation session-resource CRUD (`/v1/sessions/{id}/resources`) mirrors the
//! Managed Agents contract, while whole-manifest replacement is explicitly owned
//! by `/v1/awaken/sessions/{id}/resources`: only `file` attaches to a live Session;
//! `github_repository` and `memory_store` bind in the create-time snapshot.
//! Repository tokens use the official write-only create/update fields and are
//! sealed into the canonical Vault before a Session snapshot is persisted.

mod support;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::{
    ClientToolDescriptor, ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy,
    ToolsetSource,
};
use awaken_credential_vault::SecretStore;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_executable_agent_contract::{
    ExecutableAgentProfileSource, ExecutableAgentSessionProfile,
};
use awaken_protocol_managed::test_support::CoordinatedRuntimeFake;
use awaken_protocol_managed::types::agent::ModelInput;
use awaken_protocol_managed::types::session::{
    AgentRef, AgentRefObject, ModelConfigParams, ModelEffortInput, ModelEffortLevel,
    ModelInferenceGeo, ModelSpeed, SessionCreateParams,
};
use awaken_protocol_managed::{ManagedState, router as managed_router};
use awaken_resource_contract::{
    BindingId, ClonePolicy, ConfigVersion, FileId, InputBinding, InputResourceId,
    MemoryStoreConfigVersion, MemoryStoreDefinition, MemoryStoreId, PublishMemoryStoreConfig,
    RegisterMemoryStore, RegisterRepository, RepositoryConfigVersion, RepositoryDefinition,
    RepositoryId, ResourceAccess, ResourceAdministration as _, ResourceInventory as _,
    ResourceState, RetentionPolicy,
};
use awaken_session_contract::{
    ManagedSessionRepository, OutcomeDrive, RunError, SessionInit, SessionRuntime, StepOutcome,
    ToolPermissionDecision,
};
use awaken_session_store::SqliteManagedSessionRepository;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use support::ScheduledConflictRepository;
use tower::ServiceExt;

fn router(state: std::sync::Arc<ManagedState>) -> Router {
    managed_router(state.clone()).merge(
        awaken_protocol_awaken::session_resource_manifest_router(
            awaken_protocol_managed::replace_resource_manifest,
        )
        .with_state(state),
    )
}

fn profiled_router(state: std::sync::Arc<ManagedState>, workspace: &str) -> Router {
    let extensions = awaken_protocol_awaken::profiled_session_router(
        awaken_protocol_managed::create_profiled_session,
    )
    .merge(awaken_protocol_awaken::profiled_session_release_router(
        awaken_protocol_managed::release_profiled_session,
    ))
    .with_state(state);
    extensions.layer(axum::Extension(awaken_tenancy::WorkspaceScope(
        workspace.to_string(),
    )))
}

fn input(
    id: &str,
    target: InputResourceId,
    mount_path: &str,
    access: ResourceAccess,
) -> InputBinding {
    InputBinding {
        binding_id: BindingId::from(id),
        target,
        mount_path: mount_path.into(),
        access,
        instructions: None,
    }
}

fn resource_registry() -> std::sync::Arc<awaken_resource_application::RegistryApplication> {
    let storage = std::sync::Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Registry"),
    );
    let registry = std::sync::Arc::new(awaken_resource_application::RegistryApplication::new(
        storage,
    ));
    for id in [
        "mem_1",
        "mem_2",
        "mem_3",
        "mem_4",
        "mem_5",
        "mem_6",
        "mem_7",
        "mem_8",
        "mem_9",
        "mem_same_1",
        "mem_same_2",
        "agent-memory",
        "session-memory",
    ] {
        registry
            .register_memory_store(RegisterMemoryStore {
                definition: MemoryStoreDefinition {
                    id: id.into(),
                    workspace_id: "default".into(),
                    name: if id.starts_with("mem_same_") {
                        "Project Memory".into()
                    } else {
                        id.into()
                    },
                    description: String::new(),
                    metadata: Default::default(),
                    state: ResourceState::Active,
                    current_config_version: ConfigVersion::INITIAL,
                    timestamps: Default::default(),
                },
                initial_config: MemoryStoreConfigVersion {
                    memory_store_id: id.into(),
                    version: ConfigVersion::INITIAL,
                    retention_policy: RetentionPolicy::default(),
                },
            })
            .expect("register session Resource fixture");
    }
    registry
}

/// A runtime that accepts every `prepare_session` — the session record exists, so
/// the resource routes can be exercised. Run methods are unused here.
#[derive(Clone, Default)]
struct AcceptingFake {
    prepared: std::sync::Arc<std::sync::Mutex<Vec<SessionInit>>>,
    staged: std::sync::Arc<std::sync::Mutex<Vec<awaken_session_contract::StageMcpAttachment>>>,
    applied:
        std::sync::Arc<std::sync::Mutex<Vec<awaken_session_contract::ResolvedSessionResources>>>,
    fail_next_apply: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail_apply_remaining: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    published: std::sync::Arc<
        std::sync::Mutex<Vec<awaken_session_contract::SessionRepositoryPublicationCommand>>,
    >,
}

struct AgentWithResources;

struct LifecycleAgent {
    unavailable: std::sync::atomic::AtomicBool,
}

impl ExecutableAgentProfileSource for LifecycleAgent {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        match agent_id {
            "lifecycle" if !self.unavailable.load(std::sync::atomic::Ordering::SeqCst) => {
                Some(ExecutableAgentSessionProfile {
                    delegates: vec![awaken_executable_agent_contract::ExecutableAgentDelegate {
                        agent_id: "researcher".into(),
                        source_revision: None,
                    }],
                    ..empty_agent_view("genai")
                })
            }
            "researcher" => Some(empty_agent_view("genai")),
            _ => None,
        }
    }

    fn agent_unavailable_in(&self, _workspace_id: &str, agent_id: &str) -> bool {
        agent_id == "lifecycle" && self.unavailable.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn empty_agent_view(backend_ref: &str) -> ExecutableAgentSessionProfile {
    ExecutableAgentSessionProfile {
        name: None,
        description: None,
        source_revision: 0,
        environment: None,
        model: None,
        inference: Default::default(),
        execution_model_ref: None,
        backend_ref: backend_ref.into(),
        system: None,
        tool_ids: Vec::new(),
        toolsets: Vec::new(),
        client_tools: Vec::new(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        delegates: Vec::new(),
        advisor_model: None,
        resources: Vec::new(),
    }
}

impl ExecutableAgentProfileSource for AgentWithResources {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (agent_id == "a").then(|| ExecutableAgentSessionProfile {
            resources: vec![input(
                "release-notes",
                InputResourceId::File(FileId::from("file-release")),
                "/mnt/release.txt",
                ResourceAccess::ReadOnly,
            )],
            ..empty_agent_view("genai")
        })
    }
}

struct AgentWithIntegrations;

struct SkillGraphAgent {
    root_count: usize,
    child_count: usize,
}

impl ExecutableAgentProfileSource for SkillGraphAgent {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        let (prefix, count, delegate) = match agent_id {
            "root" => ("root", self.root_count, "child"),
            "child" => ("child", self.child_count, "root"),
            _ => return None,
        };
        Some(ExecutableAgentSessionProfile {
            skills: skill_bindings(prefix, count),
            // A legacy cycle must not count either Agent twice.
            delegates: vec![awaken_executable_agent_contract::ExecutableAgentDelegate {
                agent_id: delegate.into(),
                source_revision: None,
            }],
            ..empty_agent_view("genai")
        })
    }
}

fn skill_bindings(prefix: &str, count: usize) -> Vec<awaken_agent_contract::AgentSkillBinding> {
    (0..count)
        .map(|index| awaken_agent_contract::AgentSkillBinding::custom(format!("{prefix}-{index}")))
        .collect()
}

fn skill_graph_source(root_count: usize, child_count: usize) -> SkillGraphAgent {
    SkillGraphAgent {
        root_count,
        child_count,
    }
}

impl ExecutableAgentProfileSource for AgentWithIntegrations {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        if agent_id == "researcher" {
            // The published roster is closed over executable Agent profiles. This
            // fixture's leaf is intentionally ordinary and integration-free; an
            // absent leaf would test the multiagent_unavailable rejection rather
            // than the inheritance rule below.
            return Some(ExecutableAgentSessionProfile {
                name: Some("Published Researcher".into()),
                description: Some("Reads primary sources".into()),
                source_revision: 3,
                model: Some("research-model".into()),
                ..empty_agent_view("genai")
            });
        }
        (agent_id == "integrated").then(|| ExecutableAgentSessionProfile {
            name: Some("Published Coordinator".into()),
            description: Some("Coordinates research".into()),
            source_revision: 7,
            mcp_servers: vec![
                awaken_executable_agent_contract::ExecutableAgentMcpServer {
                    name: "docs".into(),
                    target: awaken_session_contract::McpTarget::parse_http(
                        "https://mcp.example.test",
                    )
                    .unwrap(),
                    credential_source_id: Some("cred:workspace:docs".into()),
                    credential_revision: Some(7),
                    prompts_as_skills: false,
                },
                awaken_executable_agent_contract::ExecutableAgentMcpServer {
                    name: "public-docs".into(),
                    target: awaken_session_contract::McpTarget::parse_http(
                        "https://public.example.test",
                    )
                    .unwrap(),
                    credential_source_id: None,
                    credential_revision: None,
                    prompts_as_skills: false,
                },
            ],
            toolsets: ["docs", "public-docs"]
                .into_iter()
                .map(|server_name| ToolsetPolicy {
                    source: ToolsetSource::Mcp {
                        server_name: server_name.into(),
                    },
                    default: ToolExecutionPolicy {
                        enabled: true,
                        permission: ToolPermissionRequirement::AlwaysAsk,
                    },
                    overrides: Vec::new(),
                })
                .collect(),
            skills: vec![awaken_agent_contract::AgentSkillBinding::custom(
                "skill_release",
            )],
            delegates: vec![awaken_executable_agent_contract::ExecutableAgentDelegate {
                agent_id: "researcher".into(),
                source_revision: None,
            }],
            ..empty_agent_view("genai")
        })
    }
}

#[tokio::test]
async fn session_skill_limit_counts_the_effective_unique_agent_graph() {
    // Cause graph: C1 root effective selection; C2 recursively reachable Agent
    // selections; C3 repeated/cyclic Agent identity; C4 create-time root replace.
    // Constraints: one Agent identity contributes once and the configured Session
    // supports <=500 Skills. Effects: E1 exact boundary creates and prepares;
    // E2 overflow rejects before persistence/runtime; E3 an override replaces,
    // rather than adds to, the published root selection.
    // Decision table: G1 250+250+cycle => E1; G2 251+250 => E2;
    // G3 published 500 replaced by empty + child 1 => E3/E1.
    for (rule, root_count, child_count, request, expected) in [
        ("G1", 250, 250, json!({"agent": "root"}), StatusCode::OK),
        (
            "G2",
            251,
            250,
            json!({"agent": "root"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            "G3",
            500,
            1,
            json!({
                "agent": {
                    "id": "root",
                    "type": "agent_with_overrides",
                    "skills": []
                }
            }),
            StatusCode::OK,
        ),
    ] {
        let runtime = AcceptingFake::default();
        let prepared = runtime.prepared.clone();
        let repo = std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
        );
        let state = ManagedState::new(runtime)
            .with_config_source(std::sync::Arc::new(skill_graph_source(
                root_count,
                child_count,
            )))
            .with_session_repo(repo.clone());
        let app = router(std::sync::Arc::new(state));
        let (status, _) = call(&app, "POST", "/v1/sessions", Some(request)).await;
        assert_eq!(status, expected, "{rule}");
        if expected == StatusCode::BAD_REQUEST {
            assert!(prepared.lock().unwrap().is_empty(), "{rule}");
            assert_eq!(
                repo.get("sesn_0").await,
                Err(awaken_session_contract::SessionRepositoryError::NotFound),
                "{rule}"
            );
        } else {
            assert_eq!(prepared.lock().unwrap().len(), 1, "{rule}");
        }
    }
}

struct AgentWithClientTool;

impl ExecutableAgentProfileSource for AgentWithClientTool {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (agent_id == "client-tool-agent").then(|| ExecutableAgentSessionProfile {
            client_tools: vec![ClientToolDescriptor {
                name: "lookup".into(),
                description: "exact client lookup".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }),
            }],
            ..empty_agent_view("genai")
        })
    }
}

struct AgentWithPlatformRepository;

impl ExecutableAgentProfileSource for AgentWithPlatformRepository {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (agent_id == "repo-agent").then(|| ExecutableAgentSessionProfile {
            resources: vec![input(
                "platform-repository",
                InputResourceId::Repository(RepositoryId::from("platform-repository")),
                "/workspace/repository",
                ResourceAccess::ReadWrite,
            )],
            ..empty_agent_view("genai")
        })
    }
}

struct WorkspaceScopedAgent;

impl ExecutableAgentProfileSource for WorkspaceScopedAgent {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (workspace_id == "default" && agent_id == "scoped").then(|| ExecutableAgentSessionProfile {
            resources: vec![
                input(
                    "agent-memory",
                    InputResourceId::MemoryStore(MemoryStoreId::from("agent-memory")),
                    "/mnt/memory",
                    ResourceAccess::ReadWrite,
                ),
                input(
                    "agent-file",
                    InputResourceId::File(FileId::from("agent-file")),
                    "/mnt/agent.txt",
                    ResourceAccess::ReadOnly,
                ),
            ],
            ..empty_agent_view("genai")
        })
    }
}

struct AgentWithEnvironment {
    environment_id: String,
    revision: u64,
}

impl ExecutableAgentProfileSource for AgentWithEnvironment {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (agent_id == "environment-agent").then(|| ExecutableAgentSessionProfile {
            environment: Some(
                awaken_executable_agent_contract::ExecutableAgentEnvironment {
                    environment_id: self.environment_id.clone(),
                    revision: self.revision,
                },
            ),
            ..empty_agent_view("genai")
        })
    }
}

struct AgentWithBackend(&'static str);

impl ExecutableAgentProfileSource for AgentWithBackend {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (agent_id == "backend-agent").then(|| empty_agent_view(self.0))
    }
}

#[derive(Clone, Copy)]
struct BudgetBackendRoster {
    root_backend: &'static str,
    root_fallback_backend: Option<&'static str>,
    exact_child_backend: Option<&'static str>,
    exact_child_fallback_backend: Option<&'static str>,
    current_child_backend: Option<&'static str>,
    advisor_backend: Option<&'static str>,
}

impl BudgetBackendRoster {
    fn root_profile(self) -> ExecutableAgentSessionProfile {
        ExecutableAgentSessionProfile {
            source_revision: 7,
            model: Some("root-public-model".into()),
            execution_model_ref: Some("root-model".into()),
            backend_ref: self.root_backend.into(),
            delegates: self
                .exact_child_backend
                .map(|_| {
                    vec![awaken_executable_agent_contract::ExecutableAgentDelegate {
                        agent_id: "budget-child".into(),
                        source_revision: Some(3),
                    }]
                })
                .unwrap_or_default(),
            advisor_model: self.advisor_backend.map(|_| "advisor-model".to_string()),
            ..empty_agent_view(self.root_backend)
        }
    }

    fn child_profile(backend_ref: &str, revision: u64) -> ExecutableAgentSessionProfile {
        ExecutableAgentSessionProfile {
            source_revision: revision,
            model: Some("child-public-model".into()),
            execution_model_ref: Some("child-model".into()),
            backend_ref: backend_ref.into(),
            ..empty_agent_view(backend_ref)
        }
    }

    fn candidate(
        name: &str,
        backend_ref: &str,
    ) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::new(
                format!("{name}-provider"),
                name,
                backend_ref,
            ),
        )
    }

    fn snapshot(
        self,
        agent_id: &str,
        revision: u64,
        primary_model: &str,
        primary_backend: &str,
        fallback_backend: Option<&str>,
        include_root_roster: bool,
    ) -> awaken_runtime_contract::ExecutableAgentSnapshot {
        let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder(agent_id)
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                format!("{primary_model}-provider"),
                primary_model,
                primary_backend,
            ))
            .build();
        snapshot.metadata.source.revision = revision;
        snapshot.resolved_spec.model_candidates = fallback_backend
            .map(|backend| Self::candidate(&format!("{primary_model}-fallback"), backend))
            .into_iter()
            .collect();
        if include_root_roster {
            snapshot.resolved_spec.plugin_config.agent.delegates = self
                .exact_child_backend
                .map(
                    |_| awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                        agent_id: awaken_runtime_contract::snapshot::AgentId("budget-child".into()),
                        source_revision: Some(3),
                        recursive_self: false,
                    },
                )
                .into_iter()
                .collect();
            snapshot.resolved_spec.plugin_config.agent.advisor =
                self.advisor_backend.map(|backend| {
                    awaken_runtime_contract::agent_bindings::AgentAdvisorBinding {
                        model: "advisor-model".into(),
                        candidate: Self::candidate("advisor-model", backend),
                    }
                });
        }
        snapshot.recompute_fingerprint().unwrap();
        snapshot
    }
}

impl ExecutableAgentProfileSource for BudgetBackendRoster {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        match agent_id {
            "budget-root" => Some(self.root_profile()),
            "budget-child" => self
                .current_child_backend
                .map(|backend| Self::child_profile(backend, 4)),
            _ => None,
        }
    }

    fn session_profile_at_revision_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<ExecutableAgentSessionProfile> {
        match (agent_id, source_revision) {
            ("budget-root", 7) => Some(self.root_profile()),
            ("budget-child", 3) => self
                .exact_child_backend
                .map(|backend| Self::child_profile(backend, 3)),
            _ => None,
        }
    }

    fn executable_snapshot_at_revision_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        match (agent_id, source_revision) {
            ("budget-root", 7) => Some(self.snapshot(
                "budget-root",
                7,
                "root-model",
                self.root_backend,
                self.root_fallback_backend,
                true,
            )),
            ("budget-child", 3) => self.exact_child_backend.map(|backend| {
                self.snapshot(
                    "budget-child",
                    3,
                    "child-model",
                    backend,
                    self.exact_child_fallback_backend,
                    false,
                )
            }),
            _ => None,
        }
    }
}

#[derive(Default)]
struct RecordingListPriceProvider {
    model_rosters: std::sync::Mutex<Vec<Vec<String>>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::ManagedListPriceProvider for RecordingListPriceProvider {
    async fn resolve_snapshot(
        &self,
        request: awaken_session_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_contract::ManagedListPriceSnapshot,
        awaken_session_contract::ManagedListPriceError,
    > {
        self.model_rosters
            .lock()
            .unwrap()
            .push(request.model_refs.clone());
        let rates = awaken_session_contract::ManagedTokenListRates {
            input_micros_per_million: 1,
            output_micros_per_million: 1,
            cache_read_micros_per_million: 1,
            cache_creation_micros_per_million: 1,
        };
        Ok(awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "budget-backend-test".into(),
            version: 1,
            effective_at_unix_ms: request.occurred_at_unix_ms,
            arithmetic_version: 1,
            model_rates: request
                .model_refs
                .into_iter()
                .map(|model| (model, rates))
                .collect(),
            runtime_rates: Default::default(),
            fingerprint: "budget-backend-test-fingerprint".into(),
        })
    }
}

#[derive(Default)]
struct CountingRepositoryCredentialIngress {
    writes: std::sync::atomic::AtomicUsize,
    retirements: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_application::RepositoryCredentialIngress
    for CountingRepositoryCredentialIngress
{
    async fn enter_repository_token(
        &self,
        source_id: awaken_credential_contract::CredentialSourceId,
        _workspace_id: &str,
        _target: awaken_credential_contract::CredentialTarget,
        _token: awaken_agent_contract::RedactedString,
    ) -> Result<awaken_session_application::RepositoryCredentialEntry, String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(awaken_session_application::RepositoryCredentialEntry {
            credential: awaken_credential_contract::CredentialRef {
                id: source_id.0,
                revision: 1,
            },
            provenance: awaken_session_application::SessionParticipantProvenance::Applied,
        })
    }

    async fn retire_repository_token(
        &self,
        _credential: &awaken_credential_contract::CredentialRef,
        _workspace_id: &str,
    ) -> Result<(), String> {
        self.retirements
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn rotate_repository_token(
        &self,
        _source_id: &awaken_credential_contract::CredentialSourceId,
        expected_revision: u64,
        _workspace_id: &str,
        _target: awaken_credential_contract::CredentialTarget,
        _token: awaken_agent_contract::RedactedString,
    ) -> Result<u64, String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        expected_revision
            .checked_add(1)
            .ok_or_else(|| "repository credential revision overflow".into())
    }
}

struct AgentWithPublishedModel;

struct MutableInferenceGeoPolicy {
    allow: std::sync::atomic::AtomicBool,
    checkpoints: std::sync::Mutex<Vec<awaken_protocol_managed::InferenceGeoCheckpoint>>,
}

impl MutableInferenceGeoPolicy {
    fn new(allow: bool) -> Self {
        Self {
            allow: std::sync::atomic::AtomicBool::new(allow),
            checkpoints: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl awaken_protocol_managed::ManagedInferenceGeoPolicy for MutableInferenceGeoPolicy {
    async fn authorize(
        &self,
        workspace_id: &str,
        inference_geo: Option<ModelInferenceGeo>,
        checkpoint: awaken_protocol_managed::InferenceGeoCheckpoint,
    ) -> Result<(), awaken_protocol_managed::InferenceGeoPolicyError> {
        self.checkpoints.lock().unwrap().push(checkpoint);
        if self.allow.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err(awaken_protocol_managed::InferenceGeoPolicyError::Denied {
                workspace_id: workspace_id.into(),
                geo: awaken_protocol_managed::inference_geo_name(inference_geo),
            })
        }
    }
}

#[derive(Clone)]
struct FixedSessionModelResolver {
    result: Result<
        awaken_session_contract::SessionModelPublication,
        awaken_session_contract::SessionModelResolutionError,
    >,
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionModelPublicationResolver for FixedSessionModelResolver {
    async fn resolve_session_model(
        &self,
        workspace_id: &str,
        model_reference: &str,
    ) -> Result<
        awaken_session_contract::SessionModelPublication,
        awaken_session_contract::SessionModelResolutionError,
    > {
        self.calls
            .lock()
            .unwrap()
            .push((workspace_id.to_owned(), model_reference.to_owned()));
        self.result.clone()
    }
}

fn session_with_model(model: &str) -> SessionCreateParams {
    session_with_model_input(ModelInput::Id(model.into()))
}

fn session_with_model_input(model: ModelInput) -> SessionCreateParams {
    let mut request = SessionCreateParams::new(
        "model-agent",
        awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
    );
    request.agent = AgentRef::Object(Box::new(AgentRefObject::AgentWithOverrides {
        id: "model-agent".into(),
        mcp_servers: None,
        model: Some(model),
        skills: None,
        system: None,
        tools: None,
        version: None,
    }));
    request
}

impl ExecutableAgentProfileSource for AgentWithPublishedModel {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (agent_id == "model-agent").then(|| ExecutableAgentSessionProfile {
            source_revision: 7,
            model: Some("gpt-5;provider=openai;api=open_ai_responses;endpoint=edge".into()),
            execution_model_ref: Some("gpt-5-upstream".into()),
            inference: awaken_runtime_contract::agent_bindings::InferenceOptions {
                speed: Some(awaken_runtime_contract::agent_bindings::InferenceSpeed::Fast),
                ..Default::default()
            },
            ..empty_agent_view("genai")
        })
    }

    fn executable_snapshot_at_revision_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        (agent_id == "model-agent" && source_revision == 7).then(|| {
            let mut snapshot =
                awaken_runtime_contract::ExecutableAgentSnapshot::builder("model-agent")
                    .model(awaken_runtime_contract::resolved::ModelBinding::new(
                        "openai-account",
                        "gpt-5-upstream",
                        "genai",
                    ))
                    .inference_options(awaken_runtime_contract::agent_bindings::InferenceOptions {
                        speed: Some(awaken_runtime_contract::agent_bindings::InferenceSpeed::Fast),
                        ..Default::default()
                    })
                    .build();
            snapshot.metadata.source.revision = 7;
            snapshot
        })
    }
}

#[tokio::test]
async fn session_model_override_freezes_one_complete_resolved_route() {
    // Cause/effect decision table:
    // | Rule | Override | Resolver | Effect |
    // | R1 | equal to Agent | absent | reuse Agent execution route |
    // | R2 | different | complete | freeze complete replacement route |
    // | R3 | different | absent | unavailable before prepare/persistence |
    // | R4 | different | invalid | bad request before prepare/persistence |
    // | R5 | unsupported inference_geo | any | bad request before resolution |
    // Effects cover the public model echo, execution model/backend, persisted
    // secret-free publication, resolver inputs, and terminal failure class.
    let equal_runtime = AcceptingFake::default();
    let equal_state = ManagedState::new(equal_runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPublishedModel));
    let equal_created = equal_state
        .create_session(
            session_with_model("gpt-5;provider=openai;api=open_ai_responses;endpoint=edge"),
            None,
        )
        .await
        .expect("R1");
    assert_eq!(
        equal_runtime.prepared.lock().unwrap()[0].model.as_deref(),
        Some("gpt-5-upstream"),
        "R1"
    );
    let equal_persisted = equal_state
        .session_application()
        .session(&equal_created.id)
        .await
        .expect("R1 persisted Session");
    let equal_override = equal_persisted
        .frozen_baseline()
        .and_then(|baseline| baseline.model_override.as_ref())
        .expect("R1 official override is frozen");
    assert!(equal_override.publication.is_none(), "R1 route reuse");
    assert!(
        equal_override.inference.is_default(),
        "R1 string resets controls"
    );
    let equal_projection = equal_state
        .session_application()
        .frozen_session_projection("default".into(), &equal_persisted, true)
        .await
        .expect("R1 derived executable snapshot");
    assert!(
        equal_projection
            .agent_publication
            .expect("R1 Agent publication")
            .resolved_spec
            .plugin_config
            .inference
            .is_default(),
        "R1"
    );

    let requested = "claude-sonnet-4-5;provider=anthropic;api=anthropic;endpoint=gateway";
    let primary = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::ModelBinding {
            provider_identity_ref: "anthropic-account".into(),
            model_ref: "claude-sonnet-4-5-20250929".into(),
            backend_ref: "acp:claude".into(),
        },
    );
    let fallback = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::ModelBinding {
            provider_identity_ref: "anthropic-fallback".into(),
            model_ref: "claude-sonnet-4-5-20250929".into(),
            backend_ref: "genai".into(),
        },
    );
    let calls: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
    let resolved_runtime = AcceptingFake::default();
    let resolved_state = ManagedState::new(resolved_runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPublishedModel))
        .with_model_publication_resolver(std::sync::Arc::new(FixedSessionModelResolver {
            result: Ok(awaken_session_contract::SessionModelPublication {
                primary: primary.clone(),
                candidates: vec![fallback.clone()],
            }),
            calls: calls.clone(),
        }));
    let created = resolved_state
        .create_session(
            session_with_model_input(ModelInput::Config(ModelConfigParams {
                id: requested.into(),
                speed: Some(ModelSpeed::Fast),
                effort: Some(ModelEffortInput::Level(ModelEffortLevel::High)),
                inference_geo: Some(ModelInferenceGeo::Us),
            })),
            None,
        )
        .await
        .expect("R2");
    assert_eq!(created.agent.model.id, requested, "R2 public echo");
    {
        let prepared = resolved_runtime.prepared.lock().unwrap();
        assert_eq!(
            prepared[0].model.as_deref(),
            Some(primary.binding().model_ref.as_str()),
            "R2"
        );
        assert_eq!(prepared[0].runtime.as_deref(), Some("acp:claude"), "R2");
    }
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        &[("default".into(), requested.into())],
        "R2 uses the trusted Workspace and exact official model id"
    );
    let persisted = resolved_state
        .session_application()
        .session(&created.id)
        .await
        .expect("R2 persisted Session");
    let publication = persisted
        .frozen_baseline()
        .and_then(|baseline| baseline.model_override.as_ref())
        .and_then(|model_override| model_override.publication.as_ref())
        .expect("R2 frozen override publication");
    assert_eq!(publication.primary, primary, "R2");
    assert_eq!(publication.candidates, [fallback], "R2");
    let model_override = persisted
        .frozen_baseline()
        .and_then(|baseline| baseline.model_override.as_ref())
        .expect("R2 complete model override");
    assert_eq!(
        model_override.inference,
        awaken_runtime_contract::agent_bindings::InferenceOptions {
            speed: Some(awaken_runtime_contract::agent_bindings::InferenceSpeed::Fast),
            // Managed Agents accepts `effort` in a Session model override for
            // wire compatibility, but executes the replacement model at its
            // default effort. Agent-level effort remains independently tested.
            effort: None,
            inference_geo: Some(awaken_runtime_contract::agent_bindings::InferenceGeography::Us,),
        },
        "R2"
    );
    let projection = resolved_state
        .session_application()
        .frozen_session_projection("default".into(), &persisted, true)
        .await
        .expect("R2 derived executable snapshot");
    let snapshot = projection.agent_publication.expect("R2 Agent publication");
    assert_eq!(
        snapshot.resolved_spec.model_binding, publication.primary,
        "R2"
    );
    assert_eq!(
        snapshot.resolved_spec.model_candidates, publication.candidates,
        "R2"
    );
    assert_eq!(
        snapshot.resolved_spec.plugin_config.inference, model_override.inference,
        "R2"
    );

    let unavailable_runtime = AcceptingFake::default();
    let unavailable_state = ManagedState::new(unavailable_runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPublishedModel));
    let error = unavailable_state
        .create_session(session_with_model(requested), None)
        .await
        .expect_err("R3");
    assert!(error.to_string().contains("not configured"), "R3: {error}");
    assert!(
        unavailable_runtime.prepared.lock().unwrap().is_empty(),
        "R3"
    );

    let invalid_runtime = AcceptingFake::default();
    let invalid_state = ManagedState::new(invalid_runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPublishedModel))
        .with_model_publication_resolver(std::sync::Arc::new(FixedSessionModelResolver {
            result: Err(
                awaken_session_contract::SessionModelResolutionError::Invalid(
                    "unsupported model combination".into(),
                ),
            ),
            calls: Default::default(),
        }));
    let error = invalid_state
        .create_session(session_with_model(requested), None)
        .await
        .expect_err("R4");
    assert!(
        error
            .to_string()
            .contains("invalid Session model reference"),
        "R4: {error}"
    );
    assert!(invalid_runtime.prepared.lock().unwrap().is_empty(), "R4");

    // R5 is a wire-boundary rule: because geography is a closed enum, an
    // unsupported literal cannot be represented by the state-layer type and is
    // rejected before any resolver or Runtime call can be constructed.
    let invalid_geo = serde_json::from_value::<SessionCreateParams>(json!({
        "agent": {
            "id": "model-agent",
            "type": "agent_with_overrides",
            "model": {"id": requested, "inference_geo": "eu"}
        },
        "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
    }));
    assert!(invalid_geo.is_err(), "R5");
}

#[tokio::test]
async fn budgeted_session_backends_follow_the_exact_publication_decision_table() {
    // Causes: the fixtures below establish `budgeted session backends follow the exact publication
    // decision table` with the concrete inputs, state, dependencies, and failure triggers used by
    // this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 root backend; C2 a complete model override; C3 an
    // exact ordinary-roster revision; C4 a root-frozen Advisor model; C5 no root
    // profile or override leaves the optional Session runtime absent; C6 an
    // exact candidate carries an explicit empty backend. E1 accepts every
    // Native roster, including the implicit Host fallback, and snapshots every
    // model; E2 rejects ACP/A2A/invalid backends as a bad request; E3 performs
    // no Session, Resource credential, Runtime, or price-authority write/call
    // before rejection. A mutable current child is deliberately opposite to
    // its frozen revision, proving that only C3 is authoritative.
    // Constraints: absence is the existing implicit Host/Native runtime, not an
    // empty backend value; `Backend::from_ref("")` remains Invalid; only Native
    // execution can cross the per-request budget gate.
    //
    // | Rule | root primary/fallback | override primary/fallback | exact child primary/fallback / current | Advisor | Effect |
    // | B1 | ACP / absent       | absent          | absent               | absent   | E2,E3 |
    // | B2 | A2A / absent       | absent          | absent               | absent   | E2,E3 |
    // | B3 | Native / absent    | ACP / absent     | absent               | absent   | E2,E3 |
    // | B4 | Native / absent    | absent          | ACP / absent / Native | absent  | E2,E3 |
    // | B5 | Native / Native    | absent          | Native / Native / ACP | Native  | E1    |
    // | B6 | Native / ACP       | absent          | absent               | absent   | E2,E3 |
    // | B7 | Native / absent    | absent          | absent               | A2A      | E2,E3 |
    // | B8 | Native / absent    | absent          | Native / A2A / ACP   | absent   | E2,E3 |
    // | B9 | Native / absent    | Native / ACP     | absent               | absent   | E2,E3 |
    // | B10 | no profile (None) | absent          | absent               | absent   | E1    |
    // | B11 | Invalid("") / absent | absent       | absent               | absent   | E2,E3 |
    struct Rule {
        id: &'static str,
        agent_id: &'static str,
        roster: BudgetBackendRoster,
        override_backend: Option<&'static str>,
        override_fallback_backend: Option<&'static str>,
        expected_models: Option<&'static [&'static str]>,
    }
    let rules = [
        Rule {
            id: "B1",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "acp:claude",
                root_fallback_backend: None,
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: None,
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: None,
        },
        Rule {
            id: "B2",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "a2a:https://agent.example.test",
                root_fallback_backend: None,
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: None,
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: None,
        },
        Rule {
            id: "B3",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: None,
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: None,
            },
            override_backend: Some("acp:codex"),
            override_fallback_backend: None,
            expected_models: None,
        },
        Rule {
            id: "B4",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: None,
                exact_child_backend: Some("acp:claude"),
                exact_child_fallback_backend: None,
                current_child_backend: Some("genai"),
                advisor_backend: None,
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: None,
        },
        Rule {
            id: "B5",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: Some("genai"),
                exact_child_backend: Some("genai"),
                exact_child_fallback_backend: Some("genai"),
                current_child_backend: Some("acp:claude"),
                advisor_backend: Some("genai"),
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: Some(&[
                "advisor-model",
                "child-model",
                "child-model-fallback",
                "root-model",
                "root-model-fallback",
            ]),
        },
        Rule {
            id: "B6",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: Some("acp:codex"),
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: None,
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: None,
        },
        Rule {
            id: "B7",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: None,
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: Some("a2a:https://advisor.example.test"),
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: None,
        },
        Rule {
            id: "B8",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: None,
                exact_child_backend: Some("genai"),
                exact_child_fallback_backend: Some("a2a:https://child.example.test"),
                current_child_backend: Some("acp:claude"),
                advisor_backend: None,
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: None,
        },
        Rule {
            id: "B9",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: None,
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: None,
            },
            override_backend: Some("genai"),
            override_fallback_backend: Some("acp:codex"),
            expected_models: None,
        },
        Rule {
            id: "B10",
            agent_id: "assistant",
            roster: BudgetBackendRoster {
                root_backend: "genai",
                root_fallback_backend: None,
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: None,
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: Some(&["test-model"]),
        },
        Rule {
            id: "B11",
            agent_id: "budget-root",
            roster: BudgetBackendRoster {
                root_backend: "",
                root_fallback_backend: None,
                exact_child_backend: None,
                exact_child_fallback_backend: None,
                current_child_backend: None,
                advisor_backend: None,
            },
            override_backend: None,
            override_fallback_backend: None,
            expected_models: None,
        },
    ];

    for rule in rules {
        let runtime = AcceptingFake::default();
        let repo = std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
        );
        let prices = std::sync::Arc::new(RecordingListPriceProvider::default());
        let credential_ingress =
            std::sync::Arc::new(CountingRepositoryCredentialIngress::default());
        let mut state = ManagedState::new(runtime.clone())
            .with_config_source(std::sync::Arc::new(rule.roster))
            .with_session_repo(repo.clone())
            .with_managed_list_price_provider(prices.clone())
            .with_resource_registry(resource_registry())
            .with_repository_credential_ingress(credential_ingress.clone());
        if let Some(backend_ref) = rule.override_backend {
            state = state.with_model_publication_resolver(std::sync::Arc::new(
                FixedSessionModelResolver {
                    result: Ok(awaken_session_contract::SessionModelPublication {
                        primary: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                            awaken_runtime_contract::resolved::ModelBinding::new(
                                "override-provider",
                                "override-execution-model",
                                backend_ref,
                            ),
                        ),
                        candidates: rule
                            .override_fallback_backend
                            .map(|backend| {
                                BudgetBackendRoster::candidate(
                                    "override-execution-model-fallback",
                                    backend,
                                )
                            })
                            .into_iter()
                            .collect(),
                    }),
                    calls: Default::default(),
                },
            ));
        }
        let agent = rule.override_backend.map_or_else(
            || json!(rule.agent_id),
            |_| {
                json!({
                    "id": rule.agent_id,
                    "type": "agent_with_overrides",
                    "model": "override-public-model"
                })
            },
        );
        let mut request = json!({
            "agent": agent,
            "budget": {
                "type": "limit",
                "max_list_cost": {"amount": "100", "currency": "USD"}
            },
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
        });
        if rule.expected_models.is_none() {
            request.as_object_mut().unwrap().insert(
                "resources".into(),
                json!([{
                    "type": "github_repository",
                    "url": "https://github.com/acme/private.git",
                    "authorization_token": "must-not-be-written" // awaken-allow: secret
                }]),
            );
        }
        let request = serde_json::from_value(request).expect("valid Session request");
        let result = state.create_session(request, None).await;

        if let Some(expected_models) = rule.expected_models {
            let created = result.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
            assert!(repo.get(&created.id).await.is_ok(), "{}", rule.id);
            assert_eq!(runtime.prepared.lock().unwrap().len(), 1, "{}", rule.id);
            let expected_models = expected_models
                .iter()
                .map(|model| (*model).to_string())
                .collect::<Vec<_>>();
            assert_eq!(
                prices.model_rosters.lock().unwrap().as_slice(),
                &[expected_models],
                "{} exact compiled price roster",
                rule.id
            );
        } else {
            let error = result.expect_err(rule.id);
            let awaken_protocol_managed::StateError::Run(error) = error else {
                panic!("{}: expected a Run error", rule.id)
            };
            assert_eq!(
                error.kind,
                awaken_session_contract::RunErrorKind::BadRequest,
                "{}",
                rule.id
            );
            assert!(
                error.message.contains("budget_backend_unsupported"),
                "{}: {error}",
                rule.id
            );
            let recovery = repo.reconcilable_sessions().await.unwrap();
            assert!(recovery.sessions.is_empty(), "{} Session write", rule.id);
            assert!(
                recovery.quarantined.is_empty(),
                "{} corrupt Session write",
                rule.id
            );
            assert!(runtime.prepared.lock().unwrap().is_empty(), "{}", rule.id);
            assert!(
                prices.model_rosters.lock().unwrap().is_empty(),
                "{}",
                rule.id
            );
        }
        assert_eq!(
            credential_ingress
                .writes
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{} Resource credential write",
            rule.id
        );
    }
}

#[tokio::test]
async fn workspace_inference_geo_policy_is_rechecked_before_create_and_each_run() {
    // Causes: the fixtures below establish `workspace inference geo policy` with the concrete
    // inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `is rechecked before create and each run` and every asserted
    // state transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1=Workspace denies `us` at create -> no Session or
    // Runtime preparation; C2=Workspace allows it -> Session exists; C3=the
    // same live policy narrows before the next user Run -> the batch is
    // rejected before an inbound receipt/event can be appended.
    let runtime = AcceptingFake::default();
    let policy = std::sync::Arc::new(MutableInferenceGeoPolicy::new(false));
    let state = ManagedState::new(runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPublishedModel))
        .with_inference_geo_policy(policy.clone());
    let app = router(std::sync::Arc::new(state));
    let request = json!({
        "agent": {
            "id": "model-agent",
            "type": "agent_with_overrides",
            "model": {
                "id": "gpt-5;provider=openai;api=open_ai_responses;endpoint=edge",
                "inference_geo": "us"
            }
        },
        "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
    });

    let (status, error) = call(&app, "POST", "/v1/sessions", Some(request.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "C1: {error}");
    assert!(runtime.prepared.lock().unwrap().is_empty(), "C1");

    policy
        .allow
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, session) = call(&app, "POST", "/v1/sessions", Some(request)).await;
    assert_eq!(status, StatusCode::OK, "C2: {session}");
    let session_id = session["id"].as_str().unwrap();

    policy
        .allow
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (status, error) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/events"),
        Some(json!({
            "events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "must not run"}]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "C3: {error}");
    assert_eq!(
        policy.checkpoints.lock().unwrap().as_slice(),
        [
            awaken_protocol_managed::InferenceGeoCheckpoint::SessionCreate,
            awaken_protocol_managed::InferenceGeoCheckpoint::SessionCreate,
            awaken_protocol_managed::InferenceGeoCheckpoint::Run,
        ]
    );
}

#[tokio::test]
async fn publication_backend_is_the_only_session_backend_authority() {
    // Cause graph:
    // C1 installed publication -> E1 exact backend is copied into the baseline
    // and determines the Environment's inference holder.
    // C2 request `awaken.runtime` metadata -> E2 no backend effect.
    // C3 no installed publication -> E3 compatibility Session has no backend
    // projection and uses the native Worker holder.
    //
    // Decision table:
    // | Rule | Published backend | Metadata backend | Baseline | Inference boundary |
    // | B1 | acp:claude | genai | acp:claude | Workload |
    // | B2 | genai | acp:codex | genai | Worker |
    // | B3 | absent | acp:codex | absent | Worker |
    let cases = [
        (
            "B1",
            Some("acp:claude"),
            "genai",
            Some("acp:claude"),
            awaken_credential_contract::PlaintextBoundary::Workload,
        ),
        (
            "B2",
            Some("genai"),
            "acp:codex",
            Some("genai"),
            awaken_credential_contract::PlaintextBoundary::Worker,
        ),
        (
            "B3",
            None,
            "acp:codex",
            None,
            awaken_credential_contract::PlaintextBoundary::Worker,
        ),
    ];

    for (rule, published, metadata, expected_runtime, expected_boundary) in cases {
        let runtime = AcceptingFake::default();
        let repo = std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
        );
        let mut state = ManagedState::new(runtime.clone()).with_session_repo(repo.clone());
        if let Some(backend_ref) = published {
            state = state.with_config_source(std::sync::Arc::new(AgentWithBackend(backend_ref)));
        }
        let agent = if published.is_some() {
            "backend-agent"
        } else {
            "unmanaged-agent"
        };
        let request = serde_json::from_value(with_session_environment(json!({
            "agent": agent,
            "metadata": {"awaken.runtime": metadata}
        })))
        .unwrap();
        let id = state.create_session(request, None).await.unwrap().id;
        let durable = repo.get(&id).await.unwrap();
        let baseline = durable.frozen_baseline().expect("frozen baseline");
        assert_eq!(baseline.runtime.as_deref(), expected_runtime, "{rule}");
        assert_eq!(
            baseline
                .environment
                .credential_realization
                .inference_holder
                .boundary,
            expected_boundary,
            "{rule}"
        );
        assert_eq!(
            runtime.prepared.lock().unwrap()[0].runtime.as_deref(),
            expected_runtime,
            "{rule} runtime receives only the persisted projection"
        );
    }
}

#[tokio::test]
async fn agent_default_environment_requires_the_exact_revision() {
    let (environment_authoring, environment_execution) =
        awaken_protocol_managed::test_support::environment_components();
    let environment_id = environment_authoring
        .application()
        .create(awaken_environment_contract::CreateEnvironmentCommand {
            command_id: "test:agent-default".into(),
            name: "agent default".into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: awaken_environment_contract::EnvironmentConfig::Cloud {
                networking: awaken_environment_contract::EnvironmentNetworking::Unrestricted,
                packages: Default::default(),
            },
        })
        .await
        .unwrap()
        .id;
    let revision = environment_execution
        .snapshot(&environment_id, None)
        .await
        .expect("Environment snapshot query succeeds")
        .expect("created Environment has an executable snapshot")
        .revision
        .0;
    let runtime = AcceptingFake::default();
    let state = ManagedState::new(runtime.clone())
        .with_environments(environment_execution.clone())
        .with_config_source(std::sync::Arc::new(AgentWithEnvironment {
            environment_id: environment_id.clone(),
            revision,
        }));
    let request = serde_json::from_value(json!({
        "agent": "environment-agent",
        "environment_id": environment_id
    }))
    .unwrap();
    state.create_session(request, None).await.unwrap();
    assert_eq!(
        runtime.prepared.lock().unwrap()[0]
            .environment
            .environment_id,
        environment_id
    );

    let stale = ManagedState::new(AcceptingFake::default())
        .with_environments(environment_execution)
        .with_config_source(std::sync::Arc::new(AgentWithEnvironment {
            environment_id: environment_id.clone(),
            revision: revision + 1,
        }));
    let request = serde_json::from_value(json!({
        "agent": "environment-agent",
        "environment_id": environment_id
    }))
    .unwrap();
    let error = stale.create_session(request, None).await.unwrap_err();
    assert!(error.to_string().contains("unavailable"));
}

#[async_trait::async_trait]
impl SessionRuntime for AcceptingFake {
    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &command,
            Vec::new(),
        ))
    }

    async fn execute_terminal_repository_publication(
        &self,
        command: awaken_session_contract::SessionRepositoryPublicationCommand,
    ) -> Result<awaken_session_contract::SessionRepositoryPublicationReceipt, RunError> {
        let awaken_session_contract::ResolvedInputSource::Repository {
            repository_id,
            config,
            ..
        } = &command.intent.input.source
        else {
            return Err(RunError::internal(
                "publication command is not a Repository",
            ));
        };
        let effect_receipt = awaken_provisioning_contract::RepositoryPublicationReceipt {
            repository_id: repository_id.as_str().to_string(),
            source_remote_url: config.remote_url.clone(),
            branch: command.intent.expectation.branch.clone(),
            commit: command.intent.expectation.commit.clone(),
        };
        self.published.lock().unwrap().push(command.clone());
        Ok(
            awaken_session_contract::SessionRepositoryPublicationReceipt::new(
                &command,
                effect_receipt,
            ),
        )
    }

    async fn prepare_session(&self, _thread: &str, init: SessionInit) -> Result<(), RunError> {
        self.prepared.lock().unwrap().push(init);
        Ok(())
    }
    fn capabilities_for(&self, _thread: &str) -> awaken_session_contract::AgentCapabilities {
        awaken_session_contract::AgentCapabilities {
            delegates: self
                .prepared
                .lock()
                .unwrap()
                .last()
                .map(|init| init.delegate_ids.clone())
                .unwrap_or_default(),
            ..Default::default()
        }
    }
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resolve_session_skills(
        &self,
        _workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<awaken_session_contract::ResolvedSkillBinding>, RunError> {
        Ok(skills
            .iter()
            .map(|skill| awaken_session_contract::ResolvedSkillBinding {
                kind: skill.kind,
                skill_id: skill.skill_id.clone(),
                version: 1,
                bundle_sha256: format!("sha256:{}", skill.skill_id),
            })
            .collect())
    }
    async fn apply_session_inputs(
        &self,
        _thread: &str,
        _workspace_id: &str,
        _resource_revision: u64,
        inputs: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.applied.lock().unwrap().push(inputs.clone());
        if self
            .fail_next_apply
            .swap(false, std::sync::atomic::Ordering::SeqCst)
            || self
                .fail_apply_remaining
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
        {
            return Err(RunError::internal("injected activation failure"));
        }
        Ok(())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        // Resource-route fake cause/effect rule: C1 these tests commit no Run;
        // C2 Managed GET refreshes through the sole atomic recovery port.
        // R1 C1+C2 => Ok(None), so the durable Resource desired projection is
        // queryable. Unsupported production ports retain the trait's fail-closed
        // 503 default; this fake never reconstructs a snapshot from split reads.
        Ok(None)
    }

    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::McpAttachmentRealizer for AcceptingFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        self.staged.lock().unwrap().push(request.clone());
        let receipt_fingerprint = request.fingerprint();
        Ok(awaken_session_contract::McpRealizationReceipt {
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: Some(
                awaken_credential_contract::CredentialRealizationKind::WorkerRelay,
            ),
            receipt_fingerprint,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, _, value) = call_with_headers(app, method, uri, body, &[]).await;
    (status, value)
}

async fn settle_coordinated_user_run_activity(
    application: &awaken_session_application::SessionApplication,
    session_id: &str,
    rule: &str,
) {
    let session = application
        .session(session_id)
        .await
        .unwrap_or_else(|error| panic!("{rule}: read admitted Session: {error}"));
    assert_eq!(
        session.active_activity_epochs.len(),
        1,
        "{rule}: one canonical User Run activity"
    );
    application
        .settle_activity(
            session_id,
            *session
                .active_activity_epochs
                .first()
                .expect("one canonical User Run activity"),
        )
        .await
        .unwrap_or_else(|error| panic!("{rule}: settle canonical User Run activity: {error}"));
}

fn with_session_environment(mut value: Value) -> Value {
    value
        .as_object_mut()
        .expect("Session request fixture is an object")
        .insert(
            "environment_id".into(),
            json!(awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID),
        );
    value
}

async fn call_with_headers(
    app: &Router,
    method: &str,
    uri: &str,
    mut body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
    // Resource tests are orthogonal to Environment selection but still exercise
    // the exact SDK request: every create sends an explicit Environment.
    if method == "POST" && uri == "/v1/sessions" {
        body = body.map(with_session_environment);
    }
    let mut b = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        b = b.header(*name, *value);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, value)
}

#[tokio::test]
async fn session_projects_exact_published_client_tool_contract() {
    // Causal graph:
    // published ClientExecuted descriptor -> Session preparation -> wire Agent
    // -> exact schema/description; equal-name host defaults cannot replace it.
    //
    // Decision table:
    // | published descriptor | host projection | expected Session tool |
    // | absent | present | host capability |
    // | present | absent/equal-name | exact published client descriptor |
    let state = ManagedState::new(AcceptingFake::default())
        .with_config_source(std::sync::Arc::new(AgentWithClientTool));
    let app = router(std::sync::Arc::new(state));

    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "client-tool-agent"})),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{session}");
    let lookup = session["agent"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == "lookup"))
        .expect("published client tool is visible");
    assert_eq!(lookup["description"], "exact client lookup");
    assert_eq!(lookup["input_schema"]["required"], json!(["query"]));
}

#[tokio::test]
async fn session_inherits_published_agent_integrations_and_echoes_the_effective_set() {
    // Causal graph:
    // published Agent bindings + one policy per MCP server + no Session override
    //   -> inherit the exact published MCP server/toolset pairs
    //   -> prepare Runtime with exact delegate and credential revision
    //   -> persist one authoritative Resource/MCP realization.
    //
    // Decision table:
    // | Agent binding | Session override | Expected behavior |
    // | exact credential@7 + toolset | absent | stage credential@7 with Agent origin |
    // | public URL + toolset | absent | preserve Agent URL and Agent origin |
    // | Skill + delegate | n/a | persist Skill pin and prepare delegate once |
    // FMECA: projecting the delegate as only its executable id loses its
    // publication-owned name/version/model/tools and makes the Session response
    // incompatible with `BetaManagedAgentsSessionAgent` (high severity, SDK/UI
    // visible). Freezing the full child definition here and deriving Threads from
    // it prevents both identity drift and a second child-Agent lookup path.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let staged = runtime.staged.clone();
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let secrets = std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials =
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let target = awaken_session_contract::McpTarget::parse_http("https://mcp.example.test")
        .expect("published MCP target");
    let audience = awaken_session_contract::McpTarget::identity(
        target.http_url().expect("published HTTP MCP target"),
    )
    .expect("canonical published MCP target")
    .canonical_url();
    let mut source = awaken_credential_vault::repo::enter_credential_idempotent_described(
        awaken_credential_contract::CredentialSourceId("cred:workspace:docs".into()),
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: "default".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(awaken_agent_contract::RedactedString::new(
                "published-mcp-secret",
            )),
            oauth_command: None,
        },
        None,
        awaken_credential_contract::CredentialDescriptor::new(
            audience.clone(),
            awaken_credential_contract::CredentialMaterialDescriptor::secret(
                awaken_credential_contract::OPAQUE_SECRET_MATERIAL_TYPE,
            ),
            [awaken_credential_contract::CredentialTargetContract::new(
                awaken_credential_contract::CredentialTarget::new(
                    awaken_credential_contract::CredentialPurpose::McpAuthorization,
                    audience,
                ),
                awaken_credential_contract::CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
            )],
        ),
        secrets.as_ref(),
        credentials.as_ref(),
    )
    .await
    .expect("publish canonical MCP credential authority")
    .source;
    source.version = 7;
    credentials
        .put(source)
        .await
        .expect("pin published MCP credential revision");
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        secrets,
        credentials,
    ));
    let state = ManagedState::new_with_mcp(runtime)
        .with_config_source(std::sync::Arc::new(AgentWithIntegrations))
        .with_vaults(vaults)
        .with_session_repo(repo.clone());
    let app = router(std::sync::Arc::new(state));
    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "integrated"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["agent"]["mcp_servers"][0]["name"], "docs");
    assert_eq!(
        session["agent"]["mcp_servers"][1],
        json!({
            "name": "public-docs",
            "type": "url",
            "url": "https://public.example.test"
        }),
        "the immutable Agent publication remains authoritative"
    );
    assert_eq!(
        session["agent"]["skills"][0],
        json!({"type": "custom", "skill_id": "skill_release", "version": "latest"})
    );
    let child = &session["agent"]["multiagent"]["agents"][0];
    assert_eq!(session["agent"]["name"], "Published Coordinator");
    assert_eq!(session["agent"]["version"], 7);
    assert_eq!(child["id"], "researcher");
    assert_eq!(child["name"], "Published Researcher");
    assert_eq!(child["description"], "Reads primary sources");
    assert_eq!(child["version"], 3);
    assert_eq!(child["model"]["id"], "research-model");
    assert_eq!(
        prepared.lock().unwrap()[0].delegate_ids,
        vec!["researcher".to_string()]
    );
    {
        let staged = staged.lock().unwrap();
        assert_eq!(
            staged[0]
                .credential
                .as_ref()
                .map(|access| access.credential.id.as_str()),
            Some("cred:workspace:docs")
        );
        assert_eq!(
            staged[0]
                .credential
                .as_ref()
                .map(|access| access.credential.revision),
            Some(7)
        );
    }
    let durable = repo
        .get(session["id"].as_str().unwrap())
        .await
        .expect("created Session is durable");
    assert_eq!(
        durable.mcp.attachments[0].origin,
        awaken_session_contract::McpAttachmentOrigin::Agent,
        "credential presence does not define origin"
    );
    assert_eq!(
        durable.mcp.attachments[1].origin,
        awaken_session_contract::McpAttachmentOrigin::Agent,
        "both inherited declarations retain Agent origin"
    );
    assert_eq!(
        durable
            .resources
            .active
            .skills()
            .first()
            .map(|skill| skill.skill_id.as_str()),
        Some("skill_release"),
        "ADR-0063 Resource manifest is the durable Skill-pin authority"
    );
    assert!(
        serde_json::to_value(durable.frozen_baseline().unwrap())
            .unwrap()
            .get("skills")
            .is_none(),
        "the baseline must not persist a second Skill-pin truth"
    );
}

#[tokio::test]
async fn session_mcp_override_replaces_the_published_set() {
    // Official replacement rule: a present `agent_with_overrides.mcp_servers`
    // replaces the published set before normalization. Therefore an alias of an
    // inherited target is one effective Agent declaration, not a cross-source
    // conflict or a merged parallel authority.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let state = ManagedState::new_with_mcp(runtime)
        .with_config_source(std::sync::Arc::new(AgentWithIntegrations))
        .with_session_repo(repo.clone());
    let app = router(std::sync::Arc::new(state));
    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": {
                "id": "integrated",
                "type": "agent_with_overrides",
                "mcp_servers": [{
                    "type": "url",
                    "name": "docs-alias",
                    "url": "HTTPS://MCP.EXAMPLE.TEST:443/"
                }],
                "tools": [{"type": "mcp_toolset", "mcp_server_name": "docs-alias"}]
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(prepared.lock().unwrap().len(), 1);
    assert_eq!(session["agent"]["mcp_servers"].as_array().unwrap().len(), 1);
    assert_eq!(session["agent"]["mcp_servers"][0]["name"], "docs-alias");
    let durable = repo.get(session["id"].as_str().unwrap()).await.unwrap();
    assert_eq!(durable.mcp.attachments.len(), 1);
    assert_eq!(
        durable.mcp.attachments[0].origin,
        awaken_session_contract::McpAttachmentOrigin::Agent
    );
}

async fn app_with_session() -> (Router, String) {
    let credential_repo =
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        credential_repo,
    ));
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_vaults(vaults)
            .with_resource_registry(resource_registry()),
    ));
    let (s, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(s, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();
    (app, id)
}

#[tokio::test]
async fn create_time_resources_are_backfilled_and_addressable() {
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default()).with_resource_registry(resource_registry()),
    ));

    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [
                { "type": "file", "file_id": "file_1", "mount_path": "/w/data.csv" },
                { "type": "memory_store", "memory_store_id": "mem_1", "instructions": "notes" }
            ]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();

    // The created session echoes its create-time mounts in SDK-decodable shape.
    let res = session["resources"].as_array().unwrap();
    assert_eq!(res.len(), 2, "both create-time resources are backfilled");
    assert_eq!(res[0]["type"], "file");
    assert_eq!(res[0]["file_id"], "file_1");
    assert_eq!(res[0]["mount_path"], "/w/data.csv");
    assert!(res[0]["id"].is_string() && res[0]["created_at"].is_string());
    assert_eq!(res[1]["type"], "memory_store");
    assert_eq!(res[1]["memory_store_id"], "mem_1");
    assert_eq!(res[1]["instructions"], "notes");
    assert_eq!(res[1]["mount_path"], "/mnt/memory/mem-1");
    assert!(
        res[1].get("id").is_none() && res[1].get("created_at").is_none(),
        "the official immutable Memory projection has no synthetic address"
    );

    // Both are listable; only the official File/Repository variants are
    // individually addressable by a Session Resource id.
    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 2);
    let rid = res[0]["id"].as_str().unwrap();
    let (s, got) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/resources/{rid}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["type"], "file");
}

#[tokio::test]
async fn implicit_memory_mounts_use_catalog_names_and_disambiguate_collisions() {
    // Cause/effect graph: C1 omitted mount_path -> derive from the governed
    // display name; C2 two names sanitize equally -> qualify the later path with
    // its stable store id; C3 explicit mount_path -> preserve it exactly.
    // Effects: E1 every returned path is frozen and unique; E2 Runtime receives
    // the same paths; E3 no array-order winner or shared `/mnt/memory/store`.
    // Decision table: P1 C1&&!C2=>name slug; P2 C1+C2=>id-qualified slug;
    // P3 C3=>explicit path. All three rules execute in one Session.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(resource_registry()),
    ));
    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [
                {"type": "memory_store", "memory_store_id": "mem_4"},
                {"type": "memory_store", "memory_store_id": "mem_same_1"},
                {"type": "memory_store", "memory_store_id": "mem_same_2"},
                {
                    "type": "memory_store",
                    "memory_store_id": "mem_3",
                    "mount_path": "/mnt/memory/project-memory"
                }
            ]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let paths = session["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["mount_path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "/mnt/memory/mem-4",
            "/mnt/memory/project-memory-mem-same-1",
            "/mnt/memory/project-memory-mem-same-2",
            "/mnt/memory/project-memory",
        ]
    );
    let runtime_paths = prepared.lock().unwrap()[0]
        .resources
        .inputs()
        .iter()
        .map(|input| input.mount_path.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        runtime_paths,
        paths
            .iter()
            .map(|path| (*path).to_string())
            .collect::<Vec<_>>(),
        "Coordinator freezes the sole path truth"
    );
}

#[tokio::test]
async fn memory_store_attachment_count_and_instruction_length_use_inclusive_limits() {
    // Causes: 8/9 MemoryStore attachments and 4096/4097 Unicode characters of instructions.
    // Constraints: attachment admission is create-time only and happens before Runtime prepare.
    // Effects: inclusive boundaries create one Session; first out-of-range values return 400
    // with no partial Session or preparation side effect.
    // Decision rule: memory M1-M4.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(resource_registry()),
    ));
    let resources = (1..=8)
        .map(|index| {
            json!({
                "type": "memory_store",
                "memory_store_id": format!("mem_{index}"),
                "mount_path": format!("/mnt/memory/store-{index}"),
                "instructions": if index == 1 { "界".repeat(4096) } else { String::new() },
            })
        })
        .collect::<Vec<_>>();
    let (status, accepted) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "resources": resources })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "M1/M3: {accepted}");
    assert_eq!(
        prepared.lock().unwrap().len(),
        1,
        "one accepted preparation"
    );

    let nine = (1..=9)
        .map(|index| {
            json!({
                "type": "memory_store",
                "memory_store_id": format!("mem_{index}"),
                "mount_path": format!("/mnt/memory/nine-{index}"),
            })
        })
        .collect::<Vec<_>>();
    let (status, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "resources": nine })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "M2");
    assert_eq!(
        prepared.lock().unwrap().len(),
        1,
        "M2 no Runtime side effect"
    );

    let (status, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "memory_store",
                "memory_store_id": "mem_1",
                "instructions": "界".repeat(4097),
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "M4");
    assert_eq!(
        prepared.lock().unwrap().len(),
        1,
        "M4 no Runtime side effect"
    );
}

#[tokio::test]
async fn published_agent_resources_are_visible_as_effective_session_inputs() {
    let state = ManagedState::new(AcceptingFake::default())
        .with_config_source(std::sync::Arc::new(AgentWithResources));
    let app = router(std::sync::Arc::new(state));

    let (status, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["resources"][0]["type"], "file");
    assert_eq!(session["resources"][0]["file_id"], "file-release");
    assert_eq!(session["resources"][0]["mount_path"], "/mnt/release.txt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_get_projects_the_durable_activity_lifecycle() {
    // Causes: the fixtures below establish `session get` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the lifecycle-supervisor-owned recovery seam
    // reserves a retained User Event and commits its durable activity epoch; C2
    // the one-shot test gate holds that exact canonical activation; C3 its next
    // sequential scan observes the released Run's committed terminal truth; C4
    // the in-process fake's missing Runtime Host callback is represented by the
    // application-owned activity-settlement port used by that real callback.
    // Effects: E1 the Event receipt is returned without awaiting execution; E2
    // GET projects Running while C2 holds; E3 the same aggregate commits Idle
    // and GET projects it. Decision rules: A1=C1+C2=>E1+E2,
    // A2=C1+C2+C3+C4=>E3. Constraint: the gate wraps the shared
    // `CoordinatedRuntimeFake`, and the two sequential scans model the sole
    // supervisor; no parallel driver, request-local executor, dispatch lookup,
    // or protocol-only status owner participates.
    let runtime = CoordinatedRuntimeFake::default();
    runtime.hold_next_user_run_activation();
    let state = std::sync::Arc::new(ManagedState::new(runtime.clone()));
    let app = router(state.clone());
    let (status, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(status, StatusCode::OK);
    let session_id = session["id"].as_str().unwrap().to_owned();

    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/events"),
        Some(json!({
            "events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "hold"}]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "A1/E1: {body}");
    let application = state.session_application();
    let first_scan = tokio::spawn({
        let application = application.clone();
        let session_id = session_id.clone();
        async move { Box::pin(application.drive_session_event_batches(&session_id, None)).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.wait_for_user_run_activation(),
    )
    .await
    .expect("A1 canonical activation was not reached");

    let (status, running) = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "A1");
    assert_eq!(running["status"], "running", "A1/E2");

    runtime.release_user_run_activation();
    first_scan
        .await
        .expect("A1 first recovery scan task")
        .expect("A1 first recovery scan");
    Box::pin(application.drive_session_event_batches(&session_id, None))
        .await
        .expect("A2 terminal recovery scan");
    settle_coordinated_user_run_activity(&application, &session_id, "A2/C4").await;
    let (status, idle) = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "A2/E3: {idle}");
    assert_eq!(idle["status"], "idle", "A2/E3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_an_agent_fences_new_sessions_and_new_runs() {
    // Causes: the fixtures below establish `disabling an agent` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `fences new sessions and new runs` and every asserted state
    // transition or side effect must hold.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph:
    // C1 Published -> E1 a Session may be admitted; C2 the supervisor-owned
    // recovery seam commits a Run activity receipt and the canonical one-shot
    // activation gate holds it; C3 lifecycle changes to Disabled; C4 the next
    // sequential scan observes terminal truth and the canonical Runtime Host
    // settlement port closes its activity -> E2 that already-admitted Run still
    // settles, while E3 a new Session and a new Event on the existing Session
    // both fail before any second activation. Constraint: the shared
    // `CoordinatedRuntimeFake` remains the sole reservation/activation driver;
    // its ordinary `researcher` child is frozen in the fixture roster, and
    // recovery scans never overlap.
    //
    // Decision table:
    // | rule | lifecycle at admission | target                  | outcome |
    // | L1   | Published              | new Session             | admit   |
    // | L2   | Published then Disabled| already-admitted Run    | settle  |
    // | L3   | Disabled               | new Session             | 400     |
    // | L4   | Disabled               | existing Session Event  | 400     |
    let source = std::sync::Arc::new(LifecycleAgent {
        unavailable: std::sync::atomic::AtomicBool::new(false),
    });
    let runtime = CoordinatedRuntimeFake::default();
    runtime.hold_next_user_run_activation();
    let state =
        std::sync::Arc::new(ManagedState::new(runtime.clone()).with_config_source(source.clone()));
    let app = router(state.clone());

    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "lifecycle" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "L1");
    let session_id = session["id"].as_str().unwrap();

    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/events"),
        Some(json!({
            "events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "already admitted"}]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "L2 admitted receipt: {body}");
    let application = state.session_application();
    let first_scan = tokio::spawn({
        let application = application.clone();
        let session_id = session_id.to_owned();
        async move { Box::pin(application.drive_session_event_batches(&session_id, None)).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.wait_for_user_run_activation(),
    )
    .await
    .expect("L2 canonical activation was not reached");
    source
        .unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    runtime.release_user_run_activation();
    first_scan
        .await
        .expect("L2 first recovery scan task")
        .expect("L2 first recovery scan");
    Box::pin(application.drive_session_event_batches(session_id, None))
        .await
        .expect("L2 terminal recovery scan");
    settle_coordinated_user_run_activity(&application, session_id, "L2/C4").await;
    let settled = application
        .session(session_id)
        .await
        .expect("L2/E2 Session");
    assert_eq!(
        settled.execution,
        awaken_session_contract::SessionExecutionState::Idle,
        "L2/E2"
    );

    let (status, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "lifecycle" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "L3");

    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/events"),
        Some(json!({
            "events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "must not run"}]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "L4: {body}");
}

#[tokio::test]
async fn session_resolves_scoped_defaults_and_attachments_once_before_runtime() {
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let state = ManagedState::new(runtime)
        .with_config_source(std::sync::Arc::new(WorkspaceScopedAgent))
        .with_resource_registry(resource_registry());
    let app = router(std::sync::Arc::new(state));

    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "scoped",
            "resources": [{
                "type": "memory_store",
                "memory_store_id": "session-memory",
                "mount_path": "/mnt/memory",
                "access": "read_only"
            }]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["resources"].as_array().unwrap().len(), 2);
    assert_eq!(session["resources"][0]["memory_store_id"], "session-memory");
    assert_eq!(session["resources"][0]["access"], "read_only");

    let calls = prepared.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].workspace_id, "default");
    assert_eq!(calls[0].resources.inputs().len(), 2);
    assert_eq!(
        calls[0].resources.inputs()[0].binding_id.as_str(),
        "agent-memory",
        "a replacement retains the published logical binding identity"
    );
    assert_eq!(
        calls[0].resources.inputs()[0].access,
        ResourceAccess::ReadOnly
    );
    assert!(
        calls[0].resources.inputs().iter().all(|resource| !matches!(
            &resource.source,
                awaken_session_contract::ResolvedInputSource::MemoryStore {
                memory_store_id,
                ..
            } if memory_store_id.as_str() == "agent-memory"
        )),
        "the replaced Agent default must not cross the runtime boundary"
    );
    assert!(calls[0].resources.inputs().iter().any(|resource| matches!(
        &resource.source,
        awaken_session_contract::ResolvedInputSource::File { file_id }
            if file_id.as_str() == "agent-file"
    )));
}

#[tokio::test]
async fn resource_config_publication_only_affects_later_sessions() {
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let catalog = resource_registry();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(catalog.clone()),
    ));
    let request = || {
        json!({
            "agent": "a",
            "resources": [{
                "type": "memory_store",
                "memory_store_id": "mem_1",
                "mount_path": "/mnt/memory"
            }]
        })
    };

    assert_eq!(
        call(&app, "POST", "/v1/sessions", Some(request())).await.0,
        StatusCode::OK
    );
    catalog
        .publish_memory_store_config(PublishMemoryStoreConfig {
            workspace_id: "default".into(),
            expected_current: ConfigVersion::INITIAL,
            config: MemoryStoreConfigVersion {
                memory_store_id: "mem_1".into(),
                version: ConfigVersion(2),
                retention_policy: RetentionPolicy {
                    retention_days: Some(2),
                },
            },
        })
        .expect("publish MemoryStore config V2");
    assert_eq!(
        call(&app, "POST", "/v1/sessions", Some(request())).await.0,
        StatusCode::OK
    );

    let calls = prepared.lock().unwrap();
    let version = |call: &SessionInit| match &call.resources.inputs()[0].source {
        awaken_session_contract::ResolvedInputSource::MemoryStore { config, .. } => config.version,
        other => panic!("expected memory input, got {other:?}"),
    };
    assert_eq!(version(&calls[0]), ConfigVersion::INITIAL);
    assert_eq!(version(&calls[1]), ConfigVersion(2));
}

#[tokio::test]
/// Access-compiler cause graph:
/// C1 source exists -> C2 source active -> C3 Workspace exact -> C4 descriptor
/// declares the exact Repository target -> C5 target-declared usage equals the
/// requested usage -> C6 static descriptor expiry is live -> E1 immutable
/// id/revision/target/usage/policy access pin.
/// The first failed cause terminates without opening material.
/// Request-shape invariant: C4/C5 plus the selected policy, holder, and binding
/// move as one Session request; C1/C3 remain source-row lookup admission.
///
/// | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Result |
/// |---|---|---|---|---|---|---|---|
/// | A1 | T | T | T | T | T | T | exact access |
/// | A2 | F | - | - | - | - | - | source not found |
/// | A3 | T | F | - | - | - | - | not active |
/// | A4 | T | T | F | - | - | - | cross-Workspace rejected |
/// | A5 | T | T | T | F | - | - | undeclared audience rejected |
/// | A6 | T | T | T | T | F | - | independent usage rejected |
/// | A7 | T | T | T | T | T | F | expired descriptor rejected |
async fn repository_access_compiler_follows_the_decision_table() {
    #[derive(Clone)]
    struct Rule {
        id: &'static str,
        exists: bool,
        active: bool,
        workspace_exact: bool,
        target_exact: bool,
        usage_exact: bool,
        not_expired: bool,
        expected: Result<(), awaken_credential_vault::CredentialError>,
    }
    let valid = Rule {
        id: "A1",
        exists: true,
        active: true,
        workspace_exact: true,
        target_exact: true,
        usage_exact: true,
        not_expired: true,
        expected: Ok(()),
    };
    let rules = [
        valid.clone(),
        Rule {
            id: "A2",
            exists: false,
            expected: Err(awaken_credential_vault::CredentialError::SourceNotFound(
                "missing".into(),
            )),
            ..valid.clone()
        },
        Rule {
            id: "A3",
            active: false,
            expected: Err(awaken_credential_vault::CredentialError::NotActive(
                "disabled".into(),
            )),
            ..valid.clone()
        },
        Rule {
            id: "A4",
            workspace_exact: false,
            expected: Err(awaken_credential_vault::CredentialError::InvalidSource(
                "credential source belongs to another Workspace".into(),
            )),
            ..valid.clone()
        },
        Rule {
            id: "A5",
            target_exact: false,
            expected: Err(awaken_credential_vault::CredentialError::InvalidSource(
                awaken_credential_contract::CredentialDescriptorError::TargetMismatch.to_string(),
            )),
            ..valid.clone()
        },
        Rule {
            id: "A6",
            usage_exact: false,
            expected: Err(awaken_credential_vault::CredentialError::InvalidSource(
                awaken_credential_contract::CredentialDescriptorError::InvalidTargetUsage
                    .to_string(),
            )),
            ..valid.clone()
        },
        Rule {
            id: "A7",
            not_expired: false,
            expected: Err(awaken_credential_vault::CredentialError::InvalidSource(
                awaken_credential_contract::CredentialDescriptorError::Expired.to_string(),
            )),
            ..valid
        },
    ];

    for rule in rules {
        let secrets = std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let credentials =
            std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let declared_usage = awaken_session_contract::repository_transport_credential_usage();
        let declared_target = awaken_credential_contract::CredentialTarget::new(
            awaken_credential_contract::CredentialPurpose::RepositoryTransport,
            awaken_credential_contract::repository_transport_audience(
                "https://github.com/awaken/example.git",
            )
            .expect("canonical repository transport audience"),
        );
        let source_id = if rule.exists {
            let material = awaken_credential_vault::encode_structured_material(
                awaken_credential_contract::http_basic_material(
                    awaken_agent_contract::RedactedString::new("x-access-token"),
                    awaken_agent_contract::RedactedString::new("compiler-secret"),
                ),
            )
            .expect("encode decision-table material");
            let mut source = awaken_credential_vault::repo::enter_credential_described(
                awaken_credential_vault::CredentialCreateParams {
                    workspace_id: if rule.workspace_exact {
                        "workspace-a".into()
                    } else {
                        "workspace-b".into()
                    },
                    kind: awaken_credential_vault::CredentialKind::Vault,
                    provider_id: None,
                    env_key: None,
                    secret: Some(material),
                    oauth_command: None,
                },
                awaken_credential_contract::CredentialDescriptor::new(
                    "github",
                    awaken_credential_contract::CredentialMaterialDescriptor::structured(
                        awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE,
                        ["password", "username"],
                    ),
                    [awaken_credential_contract::CredentialTargetContract::new(
                        declared_target.clone(),
                        declared_usage.clone(),
                    )],
                )
                .with_expiry(if rule.not_expired { u64::MAX } else { 1 }),
                secrets.as_ref(),
                credentials.as_ref(),
            )
            .await
            .expect("author decision-table source");
            if !rule.active {
                source.status = awaken_credential_vault::CredentialStatus::Disabled;
                source.id.0 = "disabled".into();
                credentials
                    .put(source.clone())
                    .await
                    .expect("disable source");
            }
            source.id
        } else {
            awaken_credential_contract::CredentialSourceId("missing".into())
        };
        let holder = awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
            .resource_holder;
        let usage = if rule.usage_exact {
            declared_usage.clone()
        } else {
            awaken_credential_contract::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            }
        };
        let target = if rule.target_exact {
            declared_target.clone()
        } else {
            awaken_credential_contract::CredentialTarget::new(
                awaken_credential_contract::CredentialPurpose::RepositoryTransport,
                awaken_credential_contract::repository_transport_audience(
                    "https://github.example/awaken/other.git",
                )
                .expect("different repository transport host"),
            )
        };
        let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
            "workspace-a",
            &("repository-a", 1_u64, &target),
            &usage,
        );
        let vaults = awaken_protocol_managed::VaultState::new(secrets, credentials);
        let actual = vaults
            .credential_access_for_source(
                &source_id,
                "workspace-a",
                awaken_session_application::SessionCredentialAccessRequest {
                    target: target.clone(),
                    usage,
                    policy: awaken_credential_contract::CredentialExecutionPolicy::exact(
                        holder.clone(),
                        awaken_credential_contract::ModelExposurePolicy::Forbidden,
                    ),
                    selected_holder: holder.clone(),
                    binding,
                },
            )
            .await;
        match (rule.expected, actual) {
            (Ok(()), Ok(access)) => {
                assert_eq!(access.credential.id, source_id.0, "{}", rule.id);
                assert_eq!(access.credential.revision, 1, "{}", rule.id);
                assert_eq!(access.target, Some(target), "{}", rule.id);
                assert_eq!(
                    access.usage,
                    awaken_session_contract::repository_transport_credential_usage(),
                    "{}",
                    rule.id
                );
                assert_eq!(
                    access.policy.allowed_plaintext_holders,
                    std::collections::BTreeSet::from([holder]),
                    "{}",
                    rule.id
                );
            }
            (Err(expected), Err(actual)) => assert_eq!(actual, expected, "{}", rule.id),
            (expected, actual) => panic!(
                "{}: expected {expected:?}, got {:?}",
                rule.id,
                actual.map(|_| ())
            ),
        }
    }
}

#[tokio::test]
async fn terminal_session_retires_only_its_compatibility_repository_definition() {
    let catalog = resource_registry();
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let state = ManagedState::new(AcceptingFake::default())
        .with_resource_registry(catalog.clone())
        .with_session_repo(repo.clone());
    let request = serde_json::from_value(with_session_environment(json!({
        "agent": "a",
        "resources": [{
            "type": "github_repository",
            "url": "https://github.com/awaken/example.git"
        }]
    })))
    .unwrap();
    let id = state.create_session(request, None).await.unwrap().id;
    let persisted = repo.get(&id).await.unwrap();
    let awaken_session_contract::ResolvedInputSource::Repository { repository_id, .. } =
        &persisted.resources.active.inputs()[0].source
    else {
        panic!("expected compatibility Repository")
    };
    let definition = catalog
        .find_repository("default", repository_id.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(definition.state, ResourceState::Active);
    assert_eq!(
        definition
            .metadata
            .get("awaken.session_repository.owner_kind")
            .map(String::as_str),
        Some("managed")
    );
    assert_eq!(
        definition
            .metadata
            .get("awaken.session_repository.session_id")
            .map(String::as_str),
        Some(id.as_str())
    );

    state.archive_session(&id).await.unwrap();
    assert_eq!(
        catalog
            .find_repository("default", repository_id.as_str())
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "Session cleanup tombstones only the generated compatibility definition"
    );
}

#[tokio::test]
async fn terminal_profiled_session_retires_its_marked_repository_definition() {
    // Cause/effect graph: C1 profiled namespace matches the durable Session;
    // C2 Registry metadata independently records kind=profiled and the same
    // Session id; C3 terminal root owns cleanup. C1+C2+C3 -> E1 Deleted. The
    // adjacent shared-resource tests cover either ownership proof missing.
    //
    // | Rule | Namespace | Marker | Terminal | Registry state |
    // | P1 | profiled exact | profiled exact | yes | Deleted |
    let catalog = resource_registry();
    let repository = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let state = ManagedState::new(AcceptingFake::default())
        .with_session_repo(repository)
        .with_config_source(std::sync::Arc::new(AgentWithResources))
        .with_resource_registry(catalog.clone());
    let application = state.session_application();
    let session_id = "profiled-terminal-repository";
    let repository_id = format!("profiled:{session_id}:repository:0");
    application
        .create_profiled_session(awaken_session_application::CreateProfiledSessionCommand {
            owner_scope: "default".into(),
            session_id: session_id.into(),
            mutation_policy: awaken_session_contract::SessionMutationPolicy::Frozen,
            agent_id: "a".into(),
            source_revision: None,
            environment_id: Some(awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID.into()),
            model: None,
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            resource_inputs: Vec::new(),
            mcp_candidates: Vec::new(),
            repositories: vec![awaken_session_application::ProfiledSessionRepositoryInput {
                binding_id: awaken_resource_contract::BindingId::new("profiled-repository-binding"),
                repository: awaken_session_application::SessionRepositoryResourceInput {
                    id: repository_id.clone(),
                    workspace_id: "default".into(),
                    name: "Profiled Repository".into(),
                    description: String::new(),
                    remote_url: "https://github.com/awaken/profiled.git".into(),
                    authorization_token: None,
                    credential: None,
                    mount_path: "/workspace/profiled".into(),
                    initial_branch: Some("main".into()),
                    initial_commit: None,
                },
            }],
            network_restriction: None,
            title: None,
            metadata: Default::default(),
            tools: None,
            idempotency: None,
        })
        .await
        .expect("P1 create");
    let definition = catalog
        .find_repository("default", &repository_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        definition
            .metadata
            .get("awaken.session_repository.owner_kind")
            .map(String::as_str),
        Some("profiled"),
        "P1/C2"
    );
    application
        .terminate_session(
            session_id,
            "2026-08-27T00:00:00Z",
            awaken_session_contract::ManagedLifecycleFact {
                id: format!("session:{session_id}:terminated"),
                object_id: session_id.into(),
                workspace_id: Some("default".into()),
                event_type: "session.terminated".into(),
                timestamp: 1,
                runtime_interval: None,
            },
        )
        .await
        .expect("P1 terminal cleanup");
    assert_eq!(
        catalog
            .find_repository("default", &repository_id)
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "P1/E1"
    );
}

#[tokio::test]
async fn terminal_repository_reclaims_only_its_exact_inline_credential() {
    // Cause/effect graph: C1 Managed create supplies inline Repository material;
    // C2 Registry namespace and both owner markers match the Session; C3 the
    // terminal root still freezes the exact credential revision. Effects: E1
    // Repository becomes Deleted; E2 that exact source becomes Archived at the
    // successor revision; E3 its material is reclaimed through Vault WAL/CAS.
    //
    // | Rule | Inline source | Owned marker | Terminal | Repository | Credential/material |
    // | T1 | Applied r1 | exact | yes | Deleted | Archived r2 / reclaimed |
    let secrets = std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials =
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        secrets.clone(),
        credentials.clone(),
    ));
    let catalog = resource_registry();
    let repository = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_vaults(vaults)
            .with_resource_registry(catalog.clone())
            .with_session_repo(repository.clone()),
    );
    let app = router(state.clone());
    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/private.git",
                "authorization_token": "terminal-secret" // awaken-allow: secret
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "T1 create: {session}");
    let session_id = session["id"].as_str().unwrap();
    let persisted = repository.get(session_id).await.unwrap();
    let awaken_session_contract::ResolvedInputSource::Repository {
        repository_id,
        credential: Some(credential),
        ..
    } = &persisted.resources.active.inputs()[0].source
    else {
        panic!("T1 exact authenticated Repository")
    };
    let source_id =
        awaken_credential_contract::CredentialSourceId(credential.access.credential.id.clone());
    let source = credentials.get(&source_id).await.unwrap();
    let material_ref = source.material_ref.clone().expect("T1 sealed material");
    assert_eq!(source.version, 1, "T1 precondition");
    assert!(secrets.get(&material_ref).await.is_ok(), "T1 precondition");

    state.archive_session(session_id).await.unwrap();

    assert_eq!(
        catalog
            .find_repository("default", repository_id.as_str())
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "T1/E1"
    );
    let archived = credentials.get(&source_id).await.unwrap();
    assert_eq!(archived.version, 2, "T1/E2");
    assert_eq!(
        archived.status,
        awaken_credential_vault::CredentialStatus::Archived,
        "T1/E2"
    );
    assert!(archived.material_ref.is_none(), "T1/E2");
    assert!(secrets.get(&material_ref).await.is_err(), "T1/E3");
}

#[tokio::test]
async fn terminal_session_never_deletes_a_platform_repository_definition() {
    let catalog = resource_registry();
    catalog
        .register_repository(RegisterRepository {
            definition: RepositoryDefinition {
                id: "platform-repository".into(),
                workspace_id: "default".into(),
                name: "Platform Repository".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            initial_config: RepositoryConfigVersion {
                repository_id: "platform-repository".into(),
                version: ConfigVersion::INITIAL,
                remote_url: "https://github.com/awaken/platform.git".into(),
                credential_binding: None,
                initial_branch: None,
                initial_commit: None,
                clone_policy: ClonePolicy::default(),
            },
        })
        .expect("register platform Repository");
    let state = ManagedState::new(AcceptingFake::default())
        .with_resource_registry(catalog.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPlatformRepository));
    let request = serde_json::from_value(with_session_environment(json!({
        "agent": "repo-agent"
    })))
    .unwrap();
    let id = state.create_session(request, None).await.unwrap().id;
    state.archive_session(&id).await.unwrap();

    assert_eq!(
        catalog
            .find_repository("default", "platform-repository")
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Active
    );
}

#[tokio::test]
async fn session_namespace_without_owner_marker_is_shared_and_never_deleted() {
    // Cause/effect graph: C1 Repository id resembles a Managed Session child;
    // C2 canonical owner metadata is absent; C3 the Session cleanup helper sees
    // the detached Repository. C1 without C2 is not ownership, so E1 cleanup is
    // a successful no-op and E2 the shared definition remains Active.
    //
    // | Rule | Namespace | Owner marker | Result | Registry state |
    // | S1 | managed Session | absent | no-op | Active |
    let catalog = resource_registry();
    let repository_id = "managed:session-shared:repository:0";
    let config = RepositoryConfigVersion {
        repository_id: repository_id.into(),
        version: ConfigVersion::INITIAL,
        remote_url: "https://github.com/awaken/shared.git".into(),
        credential_binding: None,
        initial_branch: None,
        initial_commit: None,
        clone_policy: ClonePolicy::default(),
    };
    catalog
        .register_repository(RegisterRepository {
            definition: RepositoryDefinition {
                id: repository_id.into(),
                workspace_id: "default".into(),
                name: "Shared Repository".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            initial_config: config.clone(),
        })
        .unwrap();
    let state = ManagedState::new(AcceptingFake::default()).with_resource_registry(catalog.clone());
    assert!(
        state
            .session_application()
            .retire_session_repository_input(
                "default",
                "session-shared",
                &awaken_session_contract::ResolvedInputSource::Repository {
                    repository_id: repository_id.into(),
                    config,
                    credential: None,
                },
            )
            .await,
        "S1/E1"
    );
    assert_eq!(
        catalog
            .find_repository("default", repository_id)
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Active,
        "S1/E2"
    );
}

#[tokio::test]
async fn duplicate_session_mount_paths_fail_closed_before_runtime() {
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let app = router(std::sync::Arc::new(ManagedState::new(runtime)));

    let (status, error) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [
                { "type": "file", "file_id": "one", "mount_path": "/mnt/data" },
                { "type": "file", "file_id": "two", "mount_path": "mnt/data" }
            ]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    assert!(prepared.lock().unwrap().is_empty());
}

#[tokio::test]
async fn file_resource_attaches_to_a_live_session() {
    let (app, id) = app_with_session().await;

    let (s, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "file", "file_id": "file_1", "mount_path": "/workspace/data.csv" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resource["type"], "file");
    assert!(resource["id"].is_string(), "the mount is assigned an id");

    // It now shows up in the session's resource listing.
    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_live_activation_rolls_back_before_reporting_failure() {
    let runtime = AcceptingFake::default();
    let fail_next = runtime.fail_next_apply.clone();
    let applied = runtime.applied.clone();
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let state = ManagedState::new(runtime)
        .with_session_repo(repo.clone())
        .with_resource_registry(resource_registry());
    let request =
        serde_json::from_value(with_session_environment(json!({ "agent": "a" }))).unwrap();
    let id = state.create_session(request, None).await.unwrap().id;

    fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = state
        .create_resource(
            &id,
            serde_json::from_value(json!({
                "type": "file",
                "file_id": "file-rollback",
                "mount_path": "/rollback.txt"
            }))
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("injected activation failure"));

    {
        let calls = applied.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "failed desired apply plus prior-manifest rollback"
        );
        assert_eq!(calls[0].inputs().len(), 1);
        assert!(calls[1].inputs().is_empty());
    }
    let durable = repo.get(&id).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty());
    assert!(durable.resources.pending.is_none());
    assert_eq!(
        durable.resources.activations[0].state,
        awaken_session_contract::ActivationState::Failed
    );
}

#[tokio::test]
async fn failed_activation_and_failed_compensation_remain_durably_retryable() {
    // Cause graph: desired apply fails -> prior-manifest compensation fails
    // -> never claim rollback/commit -> durable pending generation remains for
    // the reconciler. The compatibility item verb and complete PUT share one
    // manifest command/read projection, so public GET exposes accepted desired
    // truth even though active remains unchanged.
    //
    // Decision table:
    // | Desired apply | Compensation | Durable state | Public projection |
    // | fail | success | Failed, no pending | old generation |
    // | fail | fail | Prepared/retryable pending | desired generation |
    // | exact retry | succeeds | Active, same generation | desired generation |
    // FMECA: the old hidden-pending row had S8/O7/D7 because a client retry
    // could collide with a binding that the control plane had already accepted.
    let runtime = AcceptingFake::default();
    runtime
        .fail_apply_remaining
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let applied = runtime.applied.clone();
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let state = ManagedState::new(runtime).with_session_repo(repo.clone());
    let id = state
        .create_session(
            serde_json::from_value(with_session_environment(json!({"agent": "a"}))).unwrap(),
            None,
        )
        .await
        .unwrap()
        .id;

    let result = state
        .create_resource(
            &id,
            serde_json::from_value(json!({
                "type": "file",
                "file_id": "file-retryable",
                "mount_path": "/retryable.txt"
            }))
            .unwrap(),
        )
        .await;
    assert!(format!("{result:?}").contains("injected activation failure"));
    assert_eq!(applied.lock().unwrap().len(), 2);
    let resources = state.list_resources(&id).unwrap();
    assert_eq!(resources.len(), 1, "accepted desired resource is queryable");
    assert_eq!(
        serde_json::to_value(&resources[0]).unwrap()["mount_path"],
        "/retryable.txt"
    );
    let durable = repo.get(&id).await.unwrap();
    assert!(durable.resources.pending.is_some());
    assert!(durable.resources.needs_reconciliation());
    assert_eq!(
        durable.resources.activations.last().unwrap().state,
        awaken_session_contract::ActivationState::Prepared
    );

    let revision = durable.resources.revision;
    let replayed = state
        .create_resource(
            &id,
            serde_json::from_value(json!({
                "type": "file",
                "file_id": "file-retryable",
                "mount_path": "/retryable.txt"
            }))
            .unwrap(),
        )
        .await
        .expect("exact accepted intent is a convergent retry");
    assert_eq!(
        serde_json::to_value(replayed).unwrap()["mount_path"],
        "/retryable.txt"
    );
    let settled = repo.get(&id).await.unwrap();
    assert_eq!(settled.resources.revision, revision, "same generation");
    assert!(settled.resources.pending.is_none(), "retry settled pending");
    assert_eq!(
        settled.resources.active.inputs().len(),
        1,
        "no duplicate binding"
    );
}

#[derive(Clone, Copy)]
enum ResourceCasRule {
    NoConflict,
    PrepareConflictOnce,
    AttemptConflictOnce,
    SettlementConflictOnce,
    AttemptConflictsExhausted,
    SettlementConflictsExhausted,
    RollbackSettlementConflictOnce,
    ConcurrentUnattemptedResourceChange,
}

/// Resource commands use the repository root CAS as their only serializer.
/// The cases are generated from this cause graph:
///
/// item command + prepare CAS conflict -> reject the stale read before I/O;
/// durable Prepared + attempt fence + runtime effect + root-only conflict ->
/// settle the same Resource revision on the latest aggregate; a changed or
/// exhausted fence leaves durable pending work for recovery. Runtime failure
/// follows the same settlement path, but records rollback rather than Active.
///
/// | Rule | Runtime | Intent CAS | Attempt CAS | Settlement CAS | Result | Runtime applies | Durable Resource |
/// |------|---------|------------|-------------|----------------|--------|-----------------|------------------|
/// | C1 | success | apply | apply | apply | success | 1 | Active |
/// | C2 | success | conflict once | - | - | conflict | 0 | unchanged |
/// | C3 | success | apply | conflict once | apply | success | 1 | Active |
/// | C4 | success | apply | apply | conflict once | success | 1 | Active |
/// | C5 | success | apply | conflict x3 | - | conflict | 0 | unattempted pending |
/// | C6 | success | apply | apply | conflict x3 | conflict | 1 | attempted pending |
/// | C7 | fail then rollback | apply | apply | conflict once | runtime error | 2 | Failed/no pending |
/// | C8 | success | another unattempted intent wins | - | - | conflict | 0 | winner pending |
#[tokio::test]
async fn resource_root_cas_cases_follow_the_decision_table_without_a_process_lock() {
    for (index, rule) in [
        ResourceCasRule::NoConflict,
        ResourceCasRule::PrepareConflictOnce,
        ResourceCasRule::AttemptConflictOnce,
        ResourceCasRule::SettlementConflictOnce,
        ResourceCasRule::AttemptConflictsExhausted,
        ResourceCasRule::SettlementConflictsExhausted,
        ResourceCasRule::RollbackSettlementConflictOnce,
        ResourceCasRule::ConcurrentUnattemptedResourceChange,
    ]
    .into_iter()
    .enumerate()
    {
        Box::pin(assert_resource_root_cas_rule(index, rule)).await;
    }
}

async fn assert_resource_root_cas_rule(index: usize, rule: ResourceCasRule) {
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let fail_next = runtime.fail_next_apply.clone();
    let inner = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let repo = std::sync::Arc::new(ScheduledConflictRepository::new(inner));
    let state = ManagedState::new(runtime)
        .with_session_repo(repo.clone())
        .with_resource_registry(resource_registry());
    let request =
        serde_json::from_value(with_session_environment(json!({ "agent": "a" }))).unwrap();
    let id = state.create_session(request, None).await.unwrap().id;

    match rule {
        ResourceCasRule::NoConflict => {}
        ResourceCasRule::PrepareConflictOnce => repo.conflict_on_next(1),
        ResourceCasRule::AttemptConflictOnce => repo.conflict_on_next(2),
        ResourceCasRule::SettlementConflictOnce => repo.conflict_on_next(3),
        ResourceCasRule::AttemptConflictsExhausted => {
            repo.conflicts_on_next(&[2, 3, 4]);
        }
        ResourceCasRule::SettlementConflictsExhausted => {
            repo.conflicts_on_next(&[3, 4, 5]);
        }
        ResourceCasRule::RollbackSettlementConflictOnce => {
            fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
            repo.conflict_on_next(3);
        }
        ResourceCasRule::ConcurrentUnattemptedResourceChange => repo.resource_change_on_next(1),
    }

    let result = state
        .create_resource(
            &id,
            serde_json::from_value(json!({
                "type": "file",
                "file_id": format!("file-cas-{index}"),
                "mount_path": format!("/cas-{index}.txt")
            }))
            .unwrap(),
        )
        .await;
    let durable = repo.get(&id).await.unwrap();
    let apply_count = applied.lock().unwrap().len();

    match rule {
        ResourceCasRule::NoConflict
        | ResourceCasRule::AttemptConflictOnce
        | ResourceCasRule::SettlementConflictOnce => {
            assert!(result.is_ok(), "C{}: {result:?}", index + 1);
            assert_eq!(apply_count, 1, "C{}", index + 1);
            assert!(durable.resources.pending.is_none(), "C{}", index + 1);
            assert_eq!(durable.resources.active.inputs().len(), 1, "C{}", index + 1);
        }
        ResourceCasRule::PrepareConflictOnce => {
            assert!(
                matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
                "C2: {result:?}"
            );
            assert_eq!(apply_count, 0, "C2");
            assert!(durable.resources.pending.is_none(), "C2");
            assert!(durable.resources.active.inputs().is_empty(), "C2");
        }
        ResourceCasRule::AttemptConflictsExhausted => {
            assert!(
                matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
                "C5: {result:?}"
            );
            assert_eq!(apply_count, 0, "C5");
            assert!(durable.resources.pending.is_some(), "C5");
            assert!(durable.resources.needs_reconciliation(), "C5");
        }
        ResourceCasRule::SettlementConflictsExhausted => {
            assert!(
                matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
                "C6: {result:?}"
            );
            assert_eq!(apply_count, 1, "C6");
            assert!(durable.resources.pending.is_some(), "C6");
            assert!(durable.resources.needs_reconciliation(), "C6");
        }
        ResourceCasRule::RollbackSettlementConflictOnce => {
            assert!(
                format!("{result:?}").contains("injected activation failure"),
                "C7"
            );
            assert_eq!(apply_count, 2, "C7");
            assert!(durable.resources.pending.is_none(), "C7");
            assert_eq!(
                durable.resources.activations.last().unwrap().state,
                awaken_session_contract::ActivationState::Failed,
                "C7"
            );
        }
        ResourceCasRule::ConcurrentUnattemptedResourceChange => {
            assert!(
                matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
                "C8: {result:?}"
            );
            assert_eq!(apply_count, 0, "C8");
            assert!(durable.resources.pending.is_some(), "C8 winner retained");
            assert!(
                durable.resources.active.inputs().is_empty(),
                "C8 loser absent"
            );
        }
    }
}

#[tokio::test]
async fn file_item_race_loser_is_http_409_without_overwriting_the_winner() {
    // Cause/effect graph: C1 create/delete reads root revision R; C2 another
    // valid root mutation commits R+1 before the item intent; C3 the item verb
    // carries R into the canonical Manifest CAS. E1 the item command is the
    // sole loser and returns 409; E2 the winner remains durable; E3 the stale
    // File add/delete is absent and Runtime never realizes it.
    //
    // | Rule | Item | Concurrent winner | Item result | Durable File |
    // | F1 | create | Resource generation | 409 | absent |
    // | F2 | delete | metadata root fact | 409 | retained |
    let runtime = AcceptingFake::default();
    let inner = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let repo = std::sync::Arc::new(ScheduledConflictRepository::new(inner));
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(repo.clone())
            .with_resource_registry(resource_registry()),
    );
    let app = router(state.clone());
    let (status, session) = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"}))).await;
    assert_eq!(status, StatusCode::OK);
    let id = session["id"].as_str().unwrap();

    repo.resource_change_on_next(1);
    let (status, error) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "file",
            "file_id": "file-race",
            "mount_path": "/race.txt"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "F1: {error}");
    assert!(
        repo.get(id)
            .await
            .unwrap()
            .resources
            .desired()
            .inputs()
            .iter()
            .all(|input| !matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::File { file_id }
                    if file_id.as_str() == "file-race"
            )),
        "F1 stale File create is absent"
    );

    state.reconcile_resource_activations().await;
    let (status, file) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "file",
            "file_id": "file-delete-race",
            "mount_path": "/delete-race.txt"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "prepare F2: {file}");
    let resource_id = file["id"].as_str().unwrap();

    repo.metadata_change_on_next(1, "concurrent", "winner");
    let (status, error) = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{id}/resources/{resource_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "F2: {error}");
    let durable = repo.get(id).await.unwrap();
    assert_eq!(
        durable.metadata.get("concurrent").map(String::as_str),
        Some("winner")
    );
    assert!(
        durable.resources.desired().inputs().iter().any(|input| {
            matches!(
                &input.source,
                awaken_session_contract::ResolvedInputSource::File { file_id }
                    if file_id.as_str() == "file-delete-race"
            )
        }),
        "F2 stale File delete cannot erase the winner's root"
    );
}

#[tokio::test]
async fn github_repository_live_attach_is_rejected_without_runtime_effect() {
    // Official subresource admission is file-only. Repository attachment is a
    // create-time snapshot, so accepting it here would create a second mutable
    // ownership path beside Session creation.
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(resource_registry()),
    ));
    let (_, session) = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"}))).await;
    let id = session["id"].as_str().unwrap();
    let before = applied.lock().unwrap().len();

    let (s, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "github_repository",
            "url": "https://github.com/owner/repo"
        })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{resource}");
    assert_eq!(applied.lock().unwrap().len(), before);

    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_file_resource_can_be_detached_from_a_live_session() {
    let (app, id) = app_with_session().await;

    let (s, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "file", "file_id": "file_1" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let rid = resource["id"].as_str().unwrap().to_string();

    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{id}/resources/{rid}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn repository_item_delete_uses_durable_manifest_retirement() {
    // Item-adapter cause/effect graph: C1 a Session-owned Repository is removed
    // by item DELETE; C2 local realization succeeds; C3 the Registry owner marker
    // matches. Effects: E1 DELETE commits omission through the canonical manifest
    // root CAS; E2 the application consumes its durable retirement intent; E3
    // the Repository is Deleted and no completed queue entry remains. The
    // adapter performs no transient post-response cleanup saga.
    //
    // | Rule | Removal adapter | Root omission | Queue consumed | Registry |
    // |---|---|---|---|---|
    // | U1 | item DELETE | yes | yes | Deleted |
    //
    let catalog = resource_registry();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(sessions.clone())
            .with_resource_registry(catalog.clone()),
    ));
    let (_, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/item-delete.git"
            }]
        })),
    )
    .await;
    let session_id = session["id"].as_str().unwrap();
    let resource_id = session["resources"][0]["id"].as_str().unwrap();
    let repository_id = match &sessions
        .get(session_id)
        .await
        .unwrap()
        .resources
        .active
        .inputs()[0]
        .source
    {
        awaken_session_contract::ResolvedInputSource::Repository { repository_id, .. } => {
            repository_id.clone()
        }
        _ => panic!("U1 Repository fixture"),
    };
    let (status, body) = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "U1/E1: {body}");
    let durable = sessions.get(session_id).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty(), "U1/E1");
    assert!(
        durable.resources.repository_retirements().is_empty(),
        "U1/E2"
    );
    assert_eq!(
        catalog
            .find_repository("default", repository_id.as_str())
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "U1/E3"
    );
}

#[tokio::test]
async fn whole_manifest_omission_uses_durable_repository_retirement() {
    // Whole-manifest cause/effect graph: C1 a Session-owned Repository is absent
    // from the accepted successor; C2 local realization succeeds; C3 the durable
    // Registry owner marker matches. Effects: E1 the root commit atomically owns
    // exact retirement intent; E2 the application consumes that intent; E3 the
    // Repository is Deleted and the completed queue entry is cleared.
    //
    // | Rule | Removal adapter | Root omission | Queue consumed | Registry |
    // |---|---|---|---|---|
    // | U2 | whole manifest | yes | yes | Deleted |
    let catalog = resource_registry();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(sessions.clone())
            .with_resource_registry(catalog.clone()),
    ));
    let (_, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/manifest-omission.git"
            }]
        })),
    )
    .await;
    let session_id = session["id"].as_str().unwrap();
    let repository_id = match &sessions
        .get(session_id)
        .await
        .unwrap()
        .resources
        .active
        .inputs()[0]
        .source
    {
        awaken_session_contract::ResolvedInputSource::Repository { repository_id, .. } => {
            repository_id.clone()
        }
        _ => panic!("U2 Repository fixture"),
    };
    let (status, _, body) = call_with_headers(
        &app,
        "PUT",
        &format!("/v1/awaken/sessions/{session_id}/resources"),
        Some(json!({ "resources": [] })),
        &[("idempotency-key", "repository-omission")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "U2/E1: {body}");
    let durable = sessions.get(session_id).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty(), "U2/E1");
    assert!(
        durable.resources.repository_retirements().is_empty(),
        "U2/E2"
    );
    assert_eq!(
        catalog
            .find_repository("default", repository_id.as_str())
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "U2/E3"
    );
}

#[tokio::test]
async fn complete_manifest_is_atomic_idempotent_and_queryable() {
    // Cause/effect graph: C1 complete manifest valid/invalid; C2 key
    // absent/same/different payload; C3 If-Match current/stale; C4 Runtime
    // realization succeeds. Effects: E1 one desired generation is persisted;
    // E2 Runtime receives the whole set once; E3 exact replay returns the
    // original command revision; E4 key mismatch or stale CAS is 409; E5 a
    // collision fails before Runtime. Decision table exercised here: M1
    // valid+new-key+current+C4 => E1+E2; M2 same-key+same-payload => E3 and no
    // generation; M3 same-key+different => E4; M4 duplicate mount => E5; M0
    // PUT on the Anthropic-compatible collection => 405, because only the
    // explicitly namespaced Awaken extension owns complete replacement.
    // FMECA: sequenced delete/add could expose partial sets or duplicate mounts
    // (severity 8, occurrence 6, detection 5); one root CAS plus whole-manifest
    // validation eliminates both intermediate states.
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(resource_registry()),
    ));
    let (_, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    let id = session["id"].as_str().unwrap();
    let uri = format!("/v1/awaken/sessions/{id}/resources");
    let compatibility_uri = format!("/v1/sessions/{id}/resources");
    let manifest = json!({
        "resources": [
            { "type": "file", "file_id": "file-a", "mount_path": "/a" },
            { "type": "file", "file_id": "file-b", "mount_path": "/b" }
        ]
    });
    let before = applied.lock().unwrap().len();
    let (status, _, rejected) = call_with_headers(
        &app,
        "PUT",
        &compatibility_uri,
        Some(manifest.clone()),
        &[("idempotency-key", "wrong-protocol-owner")],
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "M0: {rejected}");
    let (status, first_headers, first) = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(manifest.clone()),
        &[("idempotency-key", "manifest-1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "M1: {first}");
    assert_eq!(first["phase"], "active", "M1/E1");
    assert_eq!(first["resources"].as_array().unwrap().len(), 2, "M1/E1");
    let first_etag = first_headers["etag"].to_str().unwrap().to_string();
    assert_eq!(applied.lock().unwrap().len(), before + 1, "M1/E2");

    let (status, replay_headers, replay) = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(manifest),
        &[("idempotency-key", "manifest-1")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "M2: {replay}");
    assert_eq!(replay_headers["etag"], first_etag, "M2/E3");
    assert_eq!(applied.lock().unwrap().len(), before + 1, "M2/E3");

    let (status, _, mismatch) = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(json!({ "resources": [] })),
        &[("idempotency-key", "manifest-1")],
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "M3/E4: {mismatch}");

    let (status, _, collision) = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(json!({ "resources": [
            { "type": "file", "file_id": "x", "mount_path": "/same" },
            { "type": "file", "file_id": "y", "mount_path": "same" }
        ] })),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "M4/E5: {collision}");
    assert_eq!(applied.lock().unwrap().len(), before + 1, "M4/E5");

    let (status, listed) = call(&app, "GET", &compatibility_uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 2, "M1/E1");
}

#[tokio::test]
async fn whole_manifest_root_loser_compensates_its_applied_repository() {
    // Cause/effect graph: C1 whole-manifest lowering creates an Applied
    // Session-owned Repository; C2 a concurrent root fact wins after the read;
    // C3 expected_revision prevents rebase/overwrite; C4 the winner does not
    // reference the candidate Repository. Effects: E1 Conflict is preserved;
    // E2 winner metadata remains; E3 the Applied Repository is tombstoned.
    //
    // | Rule | Participant | Root race | Root reference | Result | Repository |
    // | W1 | Applied | winner R+1 | absent | Conflict | Deleted |
    let runtime = AcceptingFake::default();
    let inner = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let repo = std::sync::Arc::new(ScheduledConflictRepository::new(inner));
    let catalog = resource_registry();
    let state = ManagedState::new(runtime)
        .with_session_repo(repo.clone())
        .with_resource_registry(catalog.clone());
    let request =
        serde_json::from_value(with_session_environment(json!({ "agent": "a" }))).unwrap();
    let id = state.create_session(request, None).await.unwrap().id;
    let read_revision = repo.get(&id).await.unwrap().revision;
    let remote_url = "https://github.com/awaken/manifest-race.git";
    let normalized_mount = "manifest-race";
    let initial_branch = Some("main".to_string());
    let initial_commit: Option<String> = None;
    let repository_id = format!(
        "managed:{id}:repository:{}",
        awaken_session_contract::stable_fingerprint(&(
            normalized_mount,
            remote_url,
            &initial_branch,
            &initial_commit,
        ))
    );
    repo.metadata_change_on_next(1, "manifest-winner", "durable");
    let result = state
        .replace_resource_manifest(
            &id,
            serde_json::from_value(json!({
                "resources": [{
                    "type": "github_repository",
                    "url": remote_url,
                    "mount_path": format!("/{normalized_mount}"),
                    "checkout": {"type": "branch", "name": "main"}
                }]
            }))
            .unwrap(),
            None,
            Some(read_revision),
            "manifest-race-request".into(),
        )
        .await;
    assert!(
        matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
        "W1/E1: {result:?}"
    );
    assert_eq!(
        repo.get(&id)
            .await
            .unwrap()
            .metadata
            .get("manifest-winner")
            .map(String::as_str),
        Some("durable"),
        "W1/E2"
    );
    assert_eq!(
        catalog
            .find_repository("default", &repository_id)
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "W1/E3"
    );
}

#[tokio::test]
async fn failed_manifest_realization_keeps_active_and_exposes_durable_desired() {
    // Cause/effect rule F1: valid intent commits, desired apply fails, prior
    // manifest compensation succeeds => request reports an effect error, active
    // stays unchanged, and no pending remains. Rule F2: both desired apply and
    // compensation fail => request reports an error, active stays unchanged,
    // pending desired remains queryable and retryable. FMECA: hiding F2 makes a
    // client repeat add against an accepted mount (S7/O7/D7); GET projects
    // `desired()` so accepted intent remains visible without a second store.
    let runtime = AcceptingFake::default();
    runtime
        .fail_apply_remaining
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime)
            .with_session_repo(repo.clone())
            .with_resource_registry(resource_registry()),
    ));
    let (_, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    let id = session["id"].as_str().unwrap();
    let uri = format!("/v1/awaken/sessions/{id}/resources");
    let (status, error) = call(
        &app,
        "PUT",
        &uri,
        Some(json!({ "resources": [
            { "type": "file", "file_id": "accepted", "mount_path": "/accepted" }
        ] })),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "F2: {error}");
    let durable = repo.get(id).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty(), "F2 active");
    assert!(durable.resources.pending.is_some(), "F2 desired");

    let (status, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(status, StatusCode::OK);
    let resources = listed["data"].as_array().unwrap();
    assert_eq!(resources.len(), 1, "F2 desired is queryable");
    assert_eq!(resources[0]["mount_path"], "/accepted", "F2 desired");
}

#[tokio::test]
async fn retained_repository_manifest_inherits_binding_without_resubmitting_secret() {
    // Cause/effect: C1 desired Repository has the same mount/url/checkout as the
    // current desired input; C2 the replacement omits the write-only token.
    // Effect E1 preserve binding_id, Repository definition, and credential pin;
    // E2 never echo credential material. Decision rule K1 C1+C2 => E1+E2.
    // FMECA: reconfiguring retained entries would either demand plaintext again
    // or silently clear authentication (S9/O5/D6); semantic reuse keeps the
    // existing secret-free pin under the Session aggregate.
    let credential_repo =
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        credential_repo,
    ));
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(repo.clone())
            .with_vaults(vaults)
            .with_resource_registry(resource_registry()),
    ));
    let (_, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/acme/private.git",
                "authorization_token": "top-secret", // awaken-allow: secret
                "mount_path": "/workspace/private",
                "checkout": { "type": "branch", "name": "main" }
            }]
        })),
    )
    .await;
    let id = session["id"].as_str().unwrap();
    let before = repo.get(id).await.unwrap();
    let old = before.resources.desired().inputs()[0].clone();
    let (status, response) = call(
        &app,
        "PUT",
        &format!("/v1/awaken/sessions/{id}/resources"),
        Some(json!({ "resources": [{
            "type": "github_repository",
            "url": "https://github.com/acme/private.git",
            "mount_path": "/workspace/private",
            "checkout": { "type": "branch", "name": "main" }
        }] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "K1: {response}");
    let after = repo.get(id).await.unwrap();
    let retained = &after.resources.desired().inputs()[0];
    assert_eq!(retained.binding_id, old.binding_id, "K1/E1");
    assert_eq!(retained.source, old.source, "K1/E1 credential pin");
    let wire = response.to_string();
    assert!(!wire.contains("top-secret"), "K1/E2");
    assert!(!wire.contains("credential"), "K1/E2");
}

#[tokio::test]
async fn memory_store_cannot_attach_to_a_running_session() {
    let (app, id) = app_with_session().await;

    let (s, err) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "memory_store", "memory_store_id": "memstore_1" })),
    )
    .await;
    // Managed Agents contract: memory stores are create-time only.
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["type"], "invalid_request_error");

    // Nothing was mounted — the listing stays empty.
    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn getting_or_deleting_an_unknown_session_resource_is_404() {
    let (app, id) = app_with_session().await;
    let uri = format!("/v1/sessions/{id}/resources/res_does_not_exist");
    let (get_status, _) = call(&app, "GET", &uri, None).await;
    assert_eq!(get_status, StatusCode::NOT_FOUND);
    let (del_status, _) = call(&app, "DELETE", &uri, None).await;
    assert_eq!(del_status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn repository_authorization_fails_closed_without_vault_or_existing_binding() {
    // Cause/effect graph and decision table:
    // a write-only token requires the canonical Vault ingress; update additionally
    // requires an authenticated Repository binding. Neither failure may prepare
    // Runtime resources or silently create a parallel credential path.
    //
    // | Rule | Vault ingress | Existing binding | Command | Result | Runtime |
    // | F1 | absent | n/a | create with token | reject | zero |
    // | F2 | n/a | absent | update public repo | reject | unchanged |
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let prepared = runtime.prepared.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(resource_registry()),
    ));

    let (status, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/example.git",
                "authorization_token": "must-not-enter" // awaken-allow: secret
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(prepared.lock().unwrap().is_empty());

    let (_, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/example.git"
            }]
        })),
    )
    .await;
    let session_id = session["id"].as_str().unwrap();
    let resource_id = session["resources"][0]["id"].as_str().unwrap();
    let applied_before = applied.lock().unwrap().len();
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "must-not-enter"})), // awaken-allow: secret
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(applied.lock().unwrap().len(), applied_before);
}

#[tokio::test]
async fn repository_authorization_is_sealed_pinned_and_rotated_without_echo() {
    // Cause graph:
    // write-only token -> canonical Vault sealing -> create-time Repository config
    // -> exact access@revision -> frozen Session manifest -> Runtime apply;
    // update token -> canonical material rotation -> Session pin CAS. If the Vault
    // commits N+1 and that Session CAS is unavailable, only an exact replay of the
    // same material may recover the frozen pin; different material cannot adopt
    // N+1. Raw material and the internal binding identifier never enter the wire
    // projection.
    //
    // Decision table:
    // | Rule | Frozen pin | Vault | Incoming material | Session CAS | Result |
    // | B1 | absent | absent | absent | available | public Repository |
    // | B2 | absent | absent | supplied | available | seal and pin revision 1 |
    // | B3 | revision 1 | revision 1 | replacement | unavailable | Vault 2, Session 1 |
    // | B4 | revision 1 | revision 2 | different | available | reject, Session unchanged |
    // | B5 | revision 1 | revision 2 | byte-identical | available | recover pin revision 2 |
    let secrets = std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials =
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        secrets,
        credentials.clone(),
    ));
    let session_store = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let sessions = std::sync::Arc::new(ScheduledConflictRepository::new(session_store));
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let applied = runtime.applied.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime)
            .with_vaults(vaults)
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry()),
    ));

    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/private.git",
                "authorization_token": "never-project-this-secret" // awaken-allow: secret
            }]
        })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{session}");
    let session_id = session["id"].as_str().unwrap();
    assert_eq!(
        prepared.lock().unwrap().len(),
        1,
        "B2 Runtime prepared once"
    );
    let serialized = session.to_string();
    assert!(!serialized.contains("credential_binding"));
    assert!(!serialized.contains("never-project-this-secret"));
    let durable = sessions.get(session_id).await.unwrap();
    let awaken_session_contract::ResolvedInputSource::Repository {
        config, credential, ..
    } = &durable.resources.active.inputs()[0].source
    else {
        panic!("B2 must persist one Repository input")
    };
    assert_eq!(
        config.credential_binding.as_deref(),
        Some(format!("managed:{session_id}:repository:0:credential").as_str())
    );
    let credential = credential.as_ref().expect("B2 exact execution pin");
    assert_eq!(
        credential.access.credential.id,
        format!("managed:{session_id}:repository:0:credential")
    );
    assert_eq!(credential.access.credential.revision, 1);
    let source = credentials
        .get(&awaken_credential_contract::CredentialSourceId(
            credential.access.credential.id.clone(),
        ))
        .await
        .expect("B2 source");
    assert!(source.provider_id.is_none(), "B2 one provider authority");
    let descriptor = source.descriptor.as_ref().expect("B2 descriptor");
    assert_eq!(descriptor.provider.0, "github", "B2 provider");
    assert_eq!(
        descriptor.material,
        awaken_credential_contract::CredentialMaterialDescriptor::structured(
            awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE,
            ["password", "username"],
        ),
        "B2 material shape"
    );
    assert!(
        descriptor
            .admit(
                credential.access.target.as_ref().expect("B2 target"),
                &awaken_session_contract::repository_transport_credential_usage(),
            )
            .is_ok(),
        "B2 exact target/usage"
    );

    let resource_id = session["resources"][0]["id"].as_str().unwrap();
    let applied_before = applied.lock().unwrap().len();
    sessions.unavailable_on_next(1);
    let (crash_status, crash) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "rotated-secret"})), // awaken-allow: secret
    )
    .await;
    assert_eq!(
        crash_status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "B3: {crash}"
    );
    assert!(!crash.to_string().contains("rotated-secret"), "B3 no echo");
    assert_eq!(
        applied.lock().unwrap().len(),
        applied_before,
        "B3 changes no Runtime effect"
    );
    let stranded = sessions.get(session_id).await.unwrap();
    let awaken_session_contract::ResolvedInputSource::Repository { credential, .. } =
        &stranded.resources.active.inputs()[0].source
    else {
        panic!("B3 must retain one Repository input")
    };
    assert_eq!(
        credential
            .as_ref()
            .expect("B3 frozen pin")
            .access
            .credential
            .revision,
        1,
        "B3 Session CAS did not commit"
    );
    let source_id = awaken_credential_contract::CredentialSourceId(
        credential
            .as_ref()
            .expect("B3 frozen pin")
            .access
            .credential
            .id
            .clone(),
    );
    assert_eq!(
        credentials.get(&source_id).await.unwrap().version,
        2,
        "B3 Vault"
    );

    let stranded_revision = stranded.revision;
    let stranded_resources = stranded.resources.clone();
    let (different_status, different) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "different-secret"})), // awaken-allow: secret
    )
    .await;
    assert_eq!(different_status, StatusCode::BAD_REQUEST, "B4: {different}");
    assert!(
        !different.to_string().contains("different-secret"),
        "B4 no echo"
    );
    let after_different = sessions.get(session_id).await.unwrap();
    assert_eq!(
        after_different.revision, stranded_revision,
        "B4 Session revision"
    );
    assert_eq!(
        after_different.resources, stranded_resources,
        "B4 Session state"
    );
    assert_eq!(
        credentials.get(&source_id).await.unwrap().version,
        2,
        "B4 Vault"
    );

    let (rotate_status, rotated) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "rotated-secret"})), // awaken-allow: secret
    )
    .await;
    assert_eq!(rotate_status, StatusCode::OK, "B5: {rotated}");
    assert!(
        !rotated.to_string().contains("rotated-secret"),
        "B5 no echo"
    );
    assert_eq!(
        applied.lock().unwrap().len(),
        applied_before,
        "B5 repins without remounting the working tree"
    );
    let durable = sessions.get(session_id).await.unwrap();
    let awaken_session_contract::ResolvedInputSource::Repository { credential, .. } =
        &durable.resources.active.inputs()[0].source
    else {
        panic!("B5 must retain one Repository input")
    };
    assert_eq!(
        credential
            .as_ref()
            .expect("B5 recovered pin")
            .access
            .credential
            .revision,
        2,
        "B5 exact crash replay"
    );
}

#[tokio::test]
async fn profiled_repository_binding_wire_preserves_the_historical_derivation() {
    // Binding migration cause/effect graph: C1 `binding_id` is omitted on the
    // historical private wire; C2 the Session id and Repository index make its
    // former generated identity deterministic; C3 a new caller supplies an
    // explicit non-empty or empty identity. Effects: E1 C1+C2 derives exactly
    // the former binding and preserves create replay; E2 that derived binding
    // selects the same frozen input for terminal publication; E3 a valid
    // explicit identity remains caller-owned (covered by the adjacent release
    // matrix); E4 an explicitly empty identity is rejected and cannot
    // masquerade as omission.
    //
    // | Rule | wire binding | deterministic Session/index | Effect |
    // |---|---|---|---|
    // | B1 | omitted | yes | historical binding + exact replay |
    // | B2 | omitted | yes, then release selects it | one publication |
    // | B3 | explicit non-empty | n/a | preserve caller identity |
    // | B4 | explicit empty | n/a | reject before root creation |
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry()),
    );
    let app = profiled_router(state, "default");
    let historical_create = json!({
        "session_id": "profiled-historical-binding",
        "mode": "work_unit",
        "agent_id": "coder",
        "repositories": [{
            "remote_url": "https://github.com/awaken/historical.git",
            "mount_path": "repository"
        }]
    });
    for replay in ["first", "replay"] {
        let (status, body) = call(
            &app,
            "POST",
            "/v1/awaken/sessions",
            Some(historical_create.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "B1 {replay}: {body}");
    }
    let historical_binding = concat!(
        "profiled:profiled-historical-binding:repository:",
        "profiled:profiled-historical-binding:repository:0"
    );
    let durable = sessions.get("profiled-historical-binding").await.unwrap();
    assert_eq!(
        durable.resources.active.inputs()[0].binding_id.as_str(),
        historical_binding,
        "B1 exact former generated identity"
    );
    let (status, released) = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-historical-binding/release",
        Some(json!({
            "repository_publication": {
                "binding_id": historical_binding,
                "expectation": {
                    "branch": "awf/historical",
                    "commit": "0123456789abcdef0123456789abcdef01234567"
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "B2: {released}");
    assert_eq!(runtime.published.lock().unwrap().len(), 1, "B2");

    let (status, _) = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(json!({
            "session_id": "profiled-empty-binding",
            "mode": "work_unit",
            "agent_id": "coder",
            "repositories": [{
                "binding_id": "",
                "remote_url": "https://github.com/awaken/invalid.git",
                "mount_path": "repository"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "B4");
    assert!(
        matches!(
            sessions.get("profiled-empty-binding").await,
            Err(awaken_session_contract::SessionRepositoryError::NotFound)
        ),
        "B4 no root"
    );
}

#[tokio::test]
async fn profiled_release_projects_one_durable_repository_publication() {
    // Release cause/effect graph: C1 publication is absent/present; C2 the
    // Workspace and frozen binding are exact/foreign; C3 the expectation is
    // first, an exact replay, or a mismatch; C4 the Runtime returns canonical
    // evidence. Effects: E1 legacy release archives without publication; E2 an
    // exact request returns the provisioning receipt only after it is durable in
    // the Session root; E3 exact replay returns the same response and performs no
    // second effect; E4 wrong scope is 404; E5 wrong binding is 400; E6 changed
    // expectation is 409. Rules: P1=!C1=>E1; P2=C1+exact C2+first C3+C4=>E2;
    // P3=C1+exact C2+replay C3=>E3; P4=foreign C2=>E4; P5=wrong binding C2=>E5;
    // P6=mismatch C3=>E6.
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry()),
    );
    let app = profiled_router(state.clone(), "default");

    let create_with_repository = |session_id: &str| {
        json!({
            "session_id": session_id,
            "mode": "work_unit",
            "agent_id": "coder",
            "repositories": [{
                "binding_id": "flow-repository",
                "remote_url": "https://github.com/awaken/publication.git",
                "mount_path": "repository"
            }]
        })
    };
    let (status, created) = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(create_with_repository("profiled-publication")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P2 create: {created}");

    let request = json!({
        "repository_publication": {
            "binding_id": "flow-repository",
            "expectation": {
                "branch": "awf/work-unit-1",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            }
        }
    });
    let (status, first) = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P2/E2: {first}");
    assert_eq!(
        first["repository_publication"]["binding_id"], "flow-repository",
        "P2/E2"
    );
    assert_eq!(
        first["repository_publication"]["receipt"]["commit"],
        "0123456789abcdef0123456789abcdef01234567",
        "P2/E2"
    );
    assert!(
        sessions
            .get("profiled-publication")
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt()
            .is_some(),
        "P2/E2 receipt is durable before response"
    );
    assert_eq!(runtime.published.lock().unwrap().len(), 1, "P2/E2");

    let (status, replay) = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P3/E3: {replay}");
    assert_eq!(replay, first, "P3/E3 exact response");
    assert_eq!(runtime.published.lock().unwrap().len(), 1, "P3/E3");

    let mut conflict = request.clone();
    conflict["repository_publication"]["expectation"]["commit"] =
        json!("abcdef0123456789abcdef0123456789abcdef01");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(conflict),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "P6/E6");

    let foreign = profiled_router(state.clone(), "other-workspace");
    let (status, _) = call(
        &foreign,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(request),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "P4/E4");

    let (status, created) = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(create_with_repository("profiled-publication-wrong-binding")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P5 create: {created}");
    let (status, _) = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-publication-wrong-binding/release",
        Some(json!({
            "repository_publication": {
                "binding_id": "other-repository",
                "expectation": {
                    "branch": "awf/work-unit-1",
                    "commit": "0123456789abcdef0123456789abcdef01234567"
                }
            }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "P5/E5");

    let (status, created) = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(json!({
            "session_id": "profiled-legacy-release",
            "mode": "work_unit",
            "agent_id": "coder"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P1 create: {created}");
    let (status, released) = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-legacy-release/release",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "P1/E1: {released}");
    assert!(released.get("repository_publication").is_none(), "P1/E1");
    assert_eq!(
        runtime.published.lock().unwrap().len(),
        1,
        "all no-op rules"
    );
}
