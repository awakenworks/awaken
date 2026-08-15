//! Post-creation session-resource CRUD (`/v1/sessions/{id}/resources`) mirrors the
//! Managed Agents contract, while whole-manifest replacement is explicitly owned
//! by `/v1/awaken/sessions/{id}/resources`: only `file` attaches to a live Session;
//! `github_repository` and `memory_store` bind in the create-time snapshot.
//! Repository tokens use the official write-only create/update fields and are
//! sealed into the canonical Vault before a Session snapshot is persisted.

mod support;

use awaken_agent_contract::ClientToolDescriptor;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::run::EndCause;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_executable_agent_contract::{
    ExecutableAgentProfileSource, ExecutableAgentSessionProfile,
};
use awaken_protocol_managed::{ManagedState, router as managed_router};
use awaken_resource_contract::{
    BindingId, ClonePolicy, ConfigVersion, FileId, InputBinding, InputResourceId,
    MemoryStoreConfigVersion, MemoryStoreDefinition, MemoryStoreId, RepositoryConfigVersion,
    RepositoryDefinition, RepositoryId, ResourceAccess, ResourceCatalog, ResourceState,
    RetentionPolicy,
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

fn resource_catalog() -> std::sync::Arc<awaken_resource_store::SqliteResourceStore> {
    let catalog = std::sync::Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Catalog"),
    );
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
        catalog
            .create_memory_store(
                MemoryStoreDefinition {
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
                MemoryStoreConfigVersion {
                    memory_store_id: id.into(),
                    version: ConfigVersion::INITIAL,
                    retention_policy: RetentionPolicy::default(),
                },
            )
            .unwrap();
    }
    catalog
}

/// A runtime that accepts every `prepare_session` — the session record exists, so
/// the resource routes can be exercised. Turn methods are unused here.
#[derive(Clone, Default)]
struct AcceptingFake {
    prepared: std::sync::Arc<std::sync::Mutex<Vec<SessionInit>>>,
    staged: std::sync::Arc<std::sync::Mutex<Vec<awaken_session_contract::StageMcpAttachment>>>,
    applied:
        std::sync::Arc<std::sync::Mutex<Vec<awaken_session_contract::ResolvedSessionResources>>>,
    fail_next_apply: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail_apply_remaining: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    settle_run: std::sync::Arc<std::sync::atomic::AtomicBool>,
    run_started: std::sync::Arc<tokio::sync::Notify>,
    run_release: std::sync::Arc<tokio::sync::Notify>,
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
        (agent_id == "lifecycle" && !self.unavailable.load(std::sync::atomic::Ordering::SeqCst))
            .then(|| empty_agent_view("genai"))
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

struct AgentWithPublishedModel;

impl ExecutableAgentProfileSource for AgentWithPublishedModel {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
    ) -> Option<ExecutableAgentSessionProfile> {
        (agent_id == "model-agent").then(|| ExecutableAgentSessionProfile {
            model: Some("gpt-5;provider=openai;api=open_ai_responses;endpoint=edge".into()),
            execution_model_ref: Some("gpt-5-upstream".into()),
            ..empty_agent_view("genai")
        })
    }
}

#[tokio::test]
async fn session_model_override_cannot_change_a_published_execution_route() {
    // Causes: C1 published model id; C2 official override absent/equal/different.
    // Effects: E1 preserve the public id but prepare the exact publication model
    // coordinate; E2 accept the same immutable route; E3 reject before Session
    // persistence.
    let runtime = AcceptingFake::default();
    let state = ManagedState::new(runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPublishedModel));
    let equal = serde_json::from_value(json!({
        "agent": {
            "id": "model-agent",
            "type": "agent_with_overrides",
            "model": "gpt-5;provider=openai;api=open_ai_responses;endpoint=edge"
        }
    }))
    .unwrap();
    assert!(state.create_session(equal, None).await.is_ok(), "E2");
    assert_eq!(
        runtime.prepared.lock().unwrap()[0].model.as_deref(),
        Some("gpt-5-upstream"),
        "E1"
    );

    let different = serde_json::from_value(json!({
        "agent": {
            "id": "model-agent",
            "type": "agent_with_overrides",
            "model": "gpt-5;provider=openai;api=open_ai_responses;endpoint=gateway"
        }
    }))
    .unwrap();
    let error = state.create_session(different, None).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("agent_model_override_unpublished"),
        "E3: {error}"
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
        let request = serde_json::from_value(json!({
            "agent": agent,
            "metadata": {"awaken.runtime": metadata}
        }))
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
            description: String::new(),
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
    let request = serde_json::from_value(json!({"agent": "environment-agent"})).unwrap();
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
            environment_id,
            revision: revision + 1,
        }));
    let request = serde_json::from_value(json!({"agent": "environment-agent"})).unwrap();
    let error = stale.create_session(request, None).await.unwrap_err();
    assert!(error.to_string().contains("unavailable"));
}

#[async_trait::async_trait]
impl SessionRuntime for AcceptingFake {
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
        if self.settle_run.load(std::sync::atomic::Ordering::SeqCst) {
            self.run_started.notify_one();
            self.run_release.notified().await;
            return Ok(StepOutcome::ended(
                Vec::new(),
                EndCause::NaturalEnd,
                false,
                false,
            ));
        }
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
    async fn add_system(&self, _t: &str, _x: &str) -> Result<(), RunError> {
        Ok(())
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

async fn call_with_headers(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Value) {
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
    // published Agent bindings + Session MCP override
    //   -> normalize both sources -> Session wins on equal name without credential
    //   -> prepare Runtime with exact delegate and credential revision
    //   -> persist one authoritative Resource/MCP realization.
    //
    // Decision table:
    // | Agent binding | Session override | Expected behavior |
    // | exact credential@7 | absent | stage credential@7 with Agent origin |
    // | public same-name URL | present | use Session URL with Session origin |
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
    let state = ManagedState::new_with_mcp(runtime)
        .with_config_source(std::sync::Arc::new(AgentWithIntegrations))
        .with_session_repo(repo.clone());
    let app = router(std::sync::Arc::new(state));
    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "integrated",
            "mcp_servers": [{
                "type": "url",
                "name": "public-docs",
                "url": "https://session-public.example.test"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    assert_eq!(session["agent"]["mcp_servers"][0]["name"], "docs");
    assert_eq!(
        session["agent"]["mcp_servers"][1],
        json!({
            "name": "public-docs",
            "type": "url",
            "url": "https://session-public.example.test"
        }),
        "Session origin wins over an uncredentialed Agent source with the same name"
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
        awaken_session_contract::McpAttachmentOrigin::Session,
        "Session precedence is decided after both sources are normalized"
    );
    assert_eq!(
        durable
            .resources
            .active
            .skills
            .as_ref()
            .and_then(|skills| skills.first())
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
async fn create_rejects_different_names_for_one_canonical_mcp_target_before_insert() {
    // Composed decision-table rule C5 + P4: source collection retains both
    // candidates, canonical precedence cannot select between different names,
    // and the create fails before a durable preparation row or Runtime effect.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let state = ManagedState::new_with_mcp(runtime)
        .with_config_source(std::sync::Arc::new(AgentWithIntegrations))
        .with_session_repo(repo.clone());
    let app = router(std::sync::Arc::new(state));
    let (status, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "integrated",
            "mcp_servers": [{
                "type": "url",
                "name": "docs-alias",
                "url": "HTTPS://MCP.EXAMPLE.TEST:443/"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(prepared.lock().unwrap().is_empty());
    assert_eq!(
        repo.get("sesn_0").await,
        Err(awaken_session_contract::SessionRepositoryError::NotFound)
    );
}

async fn app_with_session() -> (Router, String) {
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
    ));
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_vaults(vaults)
            .with_resource_catalog(resource_catalog()),
    ));
    let (s, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(s, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();
    (app, id)
}

#[tokio::test]
async fn create_time_resources_are_backfilled_and_addressable() {
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default()).with_resource_catalog(resource_catalog()),
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
        ManagedState::new(runtime).with_resource_catalog(resource_catalog()),
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
        .inputs
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
        ManagedState::new(runtime).with_resource_catalog(resource_catalog()),
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

#[tokio::test]
async fn session_get_projects_the_durable_activity_lifecycle() {
    // Cause/effect graph: C1 a driving event crosses durable activity admission;
    // C2 its Runtime execution remains in flight; C3 the same activity epoch
    // settles. Effects: E1 the canonical Session snapshot is running; E2 GET
    // projects running while C2 holds; E3 settlement commits idle and GET
    // projects idle. Decision rules: A1=C1+C2=>E1+E2, A2=C1+C2+C3=>E3.
    // This exercises the authoritative aggregate-to-wire projection seam; a
    // dispatch lookup or protocol-only status owner would be a duplicate path.
    let runtime = AcceptingFake::default();
    runtime
        .settle_run
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let app = router(std::sync::Arc::new(ManagedState::new(runtime.clone())));
    let (status, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(status, StatusCode::OK);
    let session_id = session["id"].as_str().unwrap().to_owned();

    let run_app = app.clone();
    let run_session_id = session_id.clone();
    let in_flight = tokio::spawn(async move {
        call(
            &run_app,
            "POST",
            &format!("/v1/sessions/{run_session_id}/events"),
            Some(json!({
                "events": [{
                    "type": "user.message",
                    "content": [{"type": "text", "text": "hold"}]
                }]
            })),
        )
        .await
    });
    runtime.run_started.notified().await;

    let (status, running) = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "A1");
    assert_eq!(running["status"], "running", "A1/E2");

    runtime.run_release.notify_one();
    let (status, body) = in_flight.await.unwrap();
    assert_eq!(status, StatusCode::OK, "A2: {body}");
    let (status, idle) = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "A2");
    assert_eq!(idle["status"], "idle", "A2/E3");
}

#[tokio::test]
async fn disabling_an_agent_fences_new_sessions_and_new_runs() {
    // Cause/effect graph:
    // C1 Published -> E1 a Session may be admitted; C2 lifecycle changes to
    // Disabled after that Session exists -> E2 a new Session and a new event
    // on the existing Session both fail before Runtime::run. A run that already
    // crossed this admission fence has no later lifecycle check and can settle.
    //
    // Decision table:
    // | rule | lifecycle at admission | target           | outcome |
    // | L1   | Published              | new Session      | admit   |
    // | L2   | Published then Disabled| admitted Run     | settle  |
    // | L3   | Disabled               | new Session      | 400     |
    // | L4   | Disabled               | existing Session | 400     |
    let source = std::sync::Arc::new(LifecycleAgent {
        unavailable: std::sync::atomic::AtomicBool::new(false),
    });
    let runtime = AcceptingFake::default();
    runtime
        .settle_run
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let state = ManagedState::new(runtime.clone()).with_config_source(source.clone());
    let app = router(std::sync::Arc::new(state));

    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "lifecycle" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "L1");
    let session_id = session["id"].as_str().unwrap();

    let first_app = app.clone();
    let first_session_id = session_id.to_owned();
    let in_flight = tokio::spawn(async move {
        call(
            &first_app,
            "POST",
            &format!("/v1/sessions/{first_session_id}/events"),
            Some(json!({
                "events": [{
                    "type": "user.message",
                    "content": [{"type": "text", "text": "already admitted"}]
                }]
            })),
        )
        .await
    });
    runtime.run_started.notified().await;
    source
        .unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    runtime.run_release.notify_one();
    let (status, body) = in_flight.await.unwrap();
    assert_eq!(status, StatusCode::OK, "L2: {body}");

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
        .with_resource_catalog(resource_catalog());
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
    assert_eq!(calls[0].resources.inputs.len(), 2);
    assert_eq!(
        calls[0].resources.inputs[0].binding_id.as_str(),
        "agent-memory",
        "a replacement retains the published logical binding identity"
    );
    assert_eq!(
        calls[0].resources.inputs[0].access,
        ResourceAccess::ReadOnly
    );
    assert!(
        calls[0].resources.inputs.iter().all(|resource| !matches!(
            &resource.source,
                awaken_session_contract::ResolvedInputSource::MemoryStore {
                memory_store_id,
                ..
            } if memory_store_id.as_str() == "agent-memory"
        )),
        "the replaced Agent default must not cross the runtime boundary"
    );
    assert!(calls[0].resources.inputs.iter().any(|resource| matches!(
        &resource.source,
        awaken_session_contract::ResolvedInputSource::File { file_id }
            if file_id.as_str() == "agent-file"
    )));
}

#[tokio::test]
async fn resource_config_publication_only_affects_later_sessions() {
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let catalog = resource_catalog();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_catalog(catalog.clone()),
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
        .publish_memory_config(
            "default",
            ConfigVersion::INITIAL,
            MemoryStoreConfigVersion {
                memory_store_id: "mem_1".into(),
                version: ConfigVersion(2),
                retention_policy: RetentionPolicy {
                    retention_days: Some(2),
                },
            },
        )
        .unwrap();
    assert_eq!(
        call(&app, "POST", "/v1/sessions", Some(request())).await.0,
        StatusCode::OK
    );

    let calls = prepared.lock().unwrap();
    let version = |call: &SessionInit| match &call.resources.inputs[0].source {
        awaken_session_contract::ResolvedInputSource::MemoryStore { config, .. } => config.version,
        other => panic!("expected memory input, got {other:?}"),
    };
    assert_eq!(version(&calls[0]), ConfigVersion::INITIAL);
    assert_eq!(version(&calls[1]), ConfigVersion(2));
}

#[tokio::test]
/// Access-compiler cause graph:
/// C1 source exists -> C2 source active -> C3 Workspace exact -> E1 immutable
/// id/revision/usage/policy access pin. The first failed cause terminates without
/// opening material.
///
/// | Rule | C1 | C2 | C3 | Result |
/// |---|---|---|---|---|
/// | A1 | T | T | T | exact access |
/// | A2 | F | - | - | source not found |
/// | A3 | T | F | - | not active |
/// | A4 | T | T | F | cross-Workspace rejected |
async fn repository_access_compiler_follows_the_decision_table() {
    #[derive(Clone)]
    struct Rule {
        id: &'static str,
        exists: bool,
        active: bool,
        workspace_exact: bool,
        expected: Result<(), awaken_credential_vault::CredentialError>,
    }
    let valid = Rule {
        id: "A1",
        exists: true,
        active: true,
        workspace_exact: true,
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
            ..valid
        },
    ];

    for rule in rules {
        let secrets = std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let credentials =
            std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let source_id = if rule.exists {
            let mut source = awaken_credential_vault::repo::enter_credential(
                awaken_credential_vault::CredentialCreateParams {
                    workspace_id: if rule.workspace_exact {
                        "workspace-a".into()
                    } else {
                        "workspace-b".into()
                    },
                    kind: awaken_credential_vault::CredentialKind::Vault,
                    provider_id: Some("git".into()),
                    env_key: None,
                    secret: Some(awaken_agent_contract::RedactedString::from(
                        "compiler-secret".to_string(),
                    )),
                    oauth_command: None,
                },
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
        let usage = awaken_session_contract::repository_transport_credential_usage();
        let binding = awaken_credential_contract::CredentialMaterialBinding::for_target(
            "workspace-a",
            &("repository-a", 1_u64),
            &usage,
        );
        let vaults = awaken_protocol_managed::VaultState::new(secrets, credentials);
        let actual = vaults
            .credential_access_for_source(
                &source_id,
                "workspace-a",
                usage,
                awaken_credential_contract::CredentialExecutionPolicy::exact(
                    holder.clone(),
                    awaken_credential_contract::ModelExposurePolicy::Forbidden,
                ),
                &holder,
                &binding,
            )
            .await;
        match (rule.expected, actual) {
            (Ok(()), Ok(access)) => {
                assert_eq!(access.credential.id, source_id.0, "{}", rule.id);
                assert_eq!(access.credential.revision, 1, "{}", rule.id);
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
    let catalog = resource_catalog();
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let state = ManagedState::new(AcceptingFake::default())
        .with_resource_catalog(catalog.clone())
        .with_session_repo(repo.clone());
    let request = serde_json::from_value(json!({
        "agent": "a",
        "resources": [{
            "type": "github_repository",
            "url": "https://github.com/awaken/example.git"
        }]
    }))
    .unwrap();
    let id = state.create_session(request, None).await.unwrap().id;
    let persisted = repo.get(&id).await.unwrap();
    let awaken_session_contract::ResolvedInputSource::Repository { repository_id, .. } =
        &persisted.resources.active.inputs[0].source
    else {
        panic!("expected compatibility Repository")
    };
    assert_eq!(
        catalog
            .repository("default", repository_id.as_str())
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Active
    );

    state.archive_session(&id).await.unwrap();
    assert_eq!(
        catalog
            .repository("default", repository_id.as_str())
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "Session cleanup tombstones only the generated compatibility definition"
    );
}

#[tokio::test]
async fn terminal_session_never_deletes_a_platform_repository_definition() {
    let catalog = resource_catalog();
    catalog
        .create_repository(
            RepositoryDefinition {
                id: "platform-repository".into(),
                workspace_id: "default".into(),
                name: "Platform Repository".into(),
                description: String::new(),
                metadata: Default::default(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            RepositoryConfigVersion {
                repository_id: "platform-repository".into(),
                version: ConfigVersion::INITIAL,
                remote_url: "https://github.com/awaken/platform.git".into(),
                credential_binding: None,
                initial_branch: None,
                initial_commit: None,
                clone_policy: ClonePolicy::default(),
            },
        )
        .unwrap();
    let state = ManagedState::new(AcceptingFake::default())
        .with_resource_catalog(catalog.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPlatformRepository));
    let request = serde_json::from_value(json!({ "agent": "repo-agent" })).unwrap();
    let id = state.create_session(request, None).await.unwrap().id;
    state.archive_session(&id).await.unwrap();

    assert_eq!(
        catalog
            .repository("default", "platform-repository")
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Active
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
        .with_resource_catalog(resource_catalog());
    let request = serde_json::from_value(json!({ "agent": "a" })).unwrap();
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
        assert_eq!(calls[0].inputs.len(), 1);
        assert!(calls[1].inputs.is_empty());
    }
    let durable = repo.get(&id).await.unwrap();
    assert!(durable.resources.active.inputs.is_empty());
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
        .create_session(serde_json::from_value(json!({"agent": "a"})).unwrap(), None)
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
        settled.resources.active.inputs.len(),
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
/// unchanged Resource state + prepare CAS conflict -> reload/rebase before I/O;
/// durable Prepared + attempt fence + runtime effect + root-only conflict ->
/// settle the same Resource revision on the latest aggregate; a changed or
/// exhausted fence leaves durable pending work for recovery. Runtime failure
/// follows the same settlement path, but records rollback rather than Active.
///
/// | Rule | Runtime | Intent CAS | Attempt CAS | Settlement CAS | Result | Runtime applies | Durable Resource |
/// |------|---------|------------|-------------|----------------|--------|-----------------|------------------|
/// | C1 | success | apply | apply | apply | success | 1 | Active |
/// | C2 | success | conflict once | apply | apply | success | 1 | Active |
/// | C3 | success | apply | conflict once | apply | success | 1 | Active |
/// | C4 | success | apply | apply | conflict once | success | 1 | Active |
/// | C5 | success | apply | conflict x3 | - | conflict | 0 | unattempted pending |
/// | C6 | success | apply | apply | conflict x3 | conflict | 1 | attempted pending |
/// | C7 | fail then rollback | apply | apply | conflict once | runtime error | 2 | Failed/no pending |
/// | C8 | success | another unattempted intent wins | revise same generation | apply | success | 1 | requested Active |
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
        let runtime = AcceptingFake::default();
        let applied = runtime.applied.clone();
        let fail_next = runtime.fail_next_apply.clone();
        let inner = std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
        );
        let repo = std::sync::Arc::new(ScheduledConflictRepository::new(inner));
        let state = ManagedState::new(runtime)
            .with_session_repo(repo.clone())
            .with_resource_catalog(resource_catalog());
        let request = serde_json::from_value(json!({ "agent": "a" })).unwrap();
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
            | ResourceCasRule::PrepareConflictOnce
            | ResourceCasRule::AttemptConflictOnce
            | ResourceCasRule::SettlementConflictOnce => {
                assert!(result.is_ok(), "C{}: {result:?}", index + 1);
                assert_eq!(apply_count, 1, "C{}", index + 1);
                assert!(durable.resources.pending.is_none(), "C{}", index + 1);
                assert_eq!(durable.resources.active.inputs.len(), 1, "C{}", index + 1);
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
                assert!(result.is_ok(), "C8: {result:?}");
                assert_eq!(apply_count, 1, "C8");
                assert!(durable.resources.pending.is_none(), "C8");
                assert_eq!(durable.resources.revision, 2, "C8 same generation");
                assert_eq!(durable.resources.active.inputs.len(), 1, "C8");
            }
        }
    }
}

#[tokio::test]
async fn github_repository_live_attach_is_rejected_without_runtime_effect() {
    // Official subresource admission is file-only. Repository attachment is a
    // create-time snapshot, so accepting it here would create a second mutable
    // ownership path beside Session creation.
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_catalog(resource_catalog()),
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
        ManagedState::new(runtime).with_resource_catalog(resource_catalog()),
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
            .with_resource_catalog(resource_catalog()),
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
    assert!(durable.resources.active.inputs.is_empty(), "F2 active");
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
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
    ));
    let repo = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(repo.clone())
            .with_vaults(vaults)
            .with_resource_catalog(resource_catalog()),
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
    let old = before.resources.desired().inputs[0].clone();
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
    let retained = &after.resources.desired().inputs[0];
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
        ManagedState::new(runtime).with_resource_catalog(resource_catalog()),
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
    // update token -> canonical material rotation -> Session pin CAS. Raw material
    // and the internal binding identifier never enter the wire projection.
    //
    // Decision table:
    // | Rule | Binding | Workspace/status | Result | Runtime | Durable pin |
    // | B1 | absent | n/a | public Repository | once | none |
    // | B2 | supplied | exact/active | seal/create | once | revision 1 |
    // | B3 | replacement | exact/active | rotate/update | unchanged | revision 2 |
    let secrets = std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let credentials =
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        secrets,
        credentials,
    ));
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("session repository"),
    );
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let applied = runtime.applied.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime)
            .with_vaults(vaults)
            .with_session_repo(sessions.clone())
            .with_resource_catalog(resource_catalog()),
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
    } = &durable.resources.active.inputs[0].source
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

    let resource_id = session["resources"][0]["id"].as_str().unwrap();
    let applied_before = applied.lock().unwrap().len();
    let (rotate_status, rotated) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "rotated-secret"})), // awaken-allow: secret
    )
    .await;
    assert_eq!(rotate_status, StatusCode::OK, "{rotated}");
    assert!(!rotated.to_string().contains("rotated-secret"));
    assert_eq!(
        applied.lock().unwrap().len(),
        applied_before,
        "B3 changes the execution pin without remounting the working tree"
    );
    let durable = sessions.get(session_id).await.unwrap();
    let awaken_session_contract::ResolvedInputSource::Repository { credential, .. } =
        &durable.resources.active.inputs[0].source
    else {
        panic!("B3 must retain one Repository input")
    };
    assert_eq!(
        credential
            .as_ref()
            .expect("B3 rotated pin")
            .access
            .credential
            .revision,
        2
    );
}
