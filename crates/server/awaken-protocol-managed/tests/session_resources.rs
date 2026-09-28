// @formal evidence session_resources.repository_rotation_requires_original_live_generation
// awaken-file-limit-ceiling: 7631 (legacy oversized test file: split, never grow)
//! Post-creation session-resource CRUD (`/v1/sessions/{id}/resources`) mirrors the
//! Managed Agents contract, while whole-manifest replacement is explicitly owned
//! by `/v1/awaken/sessions/{id}/resources`: only `file` attaches to a live Session;
//! `github_repository` and `memory_store` bind in the create-time snapshot.
//! Repository tokens use the official write-only create/update fields and are
//! sealed into the canonical Vault before a Session snapshot is persisted.
use awaken_session_execution_contract::ThreadReadRuntime;
use awaken_session_types::SessionId;
use std::sync::Arc;
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
use awaken_session_application_contract::StepOutcome;
use awaken_session_configuration_contract::SessionInit;
use awaken_session_error_contract::RunError;
use awaken_session_repository_contract::SessionRepository;
use awaken_session_execution_contract::{
    OutcomeDrive, SessionEnvironmentBindingSink, SessionRuntime, ToolPermissionDecision,
};
use awaken_session_state_contract::{SessionDisposition, SessionExecutionState};
use awaken_session_store::SqliteManagedSessionRepository;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use support::ScheduledConflictRepository;
use tower::ServiceExt;

/// Both surfaces behind the one trusted edge stamp the composition root
/// installs; handlers are fail-closed on an absent Workspace.
fn router(state: std::sync::Arc<ManagedState>) -> Router {
    support::edge_stamped(
        managed_router(state.clone()).merge(
            awaken_protocol_awaken::session_resource_manifest_router(
                awaken_protocol_managed::replace_resource_manifest,
            )
            .with_state(state),
        ),
    )
}

fn profiled_router(state: std::sync::Arc<ManagedState>, workspace: &str) -> Router {
    let extensions = awaken_protocol_awaken::profiled_session_router(
        awaken_protocol_managed::create_profiled_session,
    )
    .merge(awaken_protocol_awaken::profiled_session_run_router(
        awaken_protocol_managed::submit_profiled_session_run,
    ))
    .merge(awaken_protocol_awaken::profiled_session_release_router(
        awaken_protocol_managed::release_profiled_session,
    ))
    .merge(awaken_protocol_awaken::session_execution_evidence_router(
        awaken_protocol_managed::get_session_execution_evidence,
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

async fn resource_registry() -> std::sync::Arc<awaken_resource_application::RegistryApplication> {
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
            .await
            .expect("register session Resource fixture");
    }
    registry
}

/// A runtime that accepts every complete Session projection and records the
/// profiled-Run reservation/activation boundary exercised by this adapter.
#[derive(Clone, Default)]
struct AcceptingFake {
    prepared: std::sync::Arc<std::sync::Mutex<Vec<SessionInit>>>,
    staged: std::sync::Arc<std::sync::Mutex<Vec<awaken_session_mcp_contract::StageMcpAttachment>>>,
    applied: std::sync::Arc<
        std::sync::Mutex<Vec<awaken_session_resource_contract::ResolvedSessionResources>>,
    >,
    stall_projection_install: std::sync::Arc<std::sync::atomic::AtomicBool>,
    recovery_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    reject_recovery_reads: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail_next_apply: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail_apply_remaining: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    published: std::sync::Arc<
        std::sync::Mutex<Vec<awaken_session_state_contract::SessionRepositoryPublicationCommand>>,
    >,
    reserved_runs: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_session_execution_contract::AdmitSessionRun>,
        >,
    >,
    activated_runs: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_session_execution_contract::SessionRunDelivery>,
        >,
    >,
    fail_next_reservation_unavailable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail_next_activation_unavailable: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fail_next_activation_internal: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

/// Build the one complete executable-publication shape used by profile fixtures
/// that declare a positive source revision. The profile is only authoring input;
/// this snapshot is the rebuildable Worker projection selected by that revision.
fn fixture_publication(
    agent_id: &str,
    source_revision: u64,
    account_id: &str,
    model_ref: &str,
    backend_ref: &str,
    inference: awaken_runtime_contract::agent_bindings::InferenceOptions,
) -> awaken_runtime_contract::ExecutableAgentSnapshot {
    let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder(agent_id)
        .model(awaken_runtime_contract::resolved::ModelBinding::new(
            account_id,
            model_ref,
            backend_ref,
        ))
        .inference_options(inference)
        .build();
    snapshot.metadata.source.revision = source_revision;
    snapshot
        .recompute_fingerprint()
        .expect("fixture publication fingerprint");
    snapshot
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
                    target: awaken_session_mcp_contract::McpTarget::parse_http(
                        "https://mcp.example.test",
                    )
                    .unwrap(),
                    credential_source_id: Some("cred:workspace:docs".into()),
                    credential_revision: Some(7),
                    prompts_as_skills: false,
                },
                awaken_executable_agent_contract::ExecutableAgentMcpServer {
                    name: "public-docs".into(),
                    target: awaken_session_mcp_contract::McpTarget::parse_http(
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

    fn executable_snapshot_at_revision_in(
        &self,
        _workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        // C1 positive fixture revisions freeze executable publications; C2 the
        // Session realization projection requests that exact id/revision pair.
        // E1 the fixture supplies one matching Worker projection; a different
        // pair remains unavailable, so this inheritance fixture cannot weaken
        // production's exact-publication gate.
        match (agent_id, source_revision) {
            ("integrated", 7) => Some(fixture_publication(
                "integrated",
                7,
                "integrated-account",
                "integration-model",
                "genai",
                Default::default(),
            )),
            ("researcher", 3) => Some(fixture_publication(
                "researcher",
                3,
                "researcher-account",
                "research-model",
                "genai",
                Default::default(),
            )),
            _ => None,
        }
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
        let created = call(&app, "POST", "/v1/sessions", Some(request))
            .await
            .expect_for(expected, "expected status");
        if expected == StatusCode::BAD_REQUEST {
            assert!(prepared.lock().unwrap().is_empty(), "{rule}");
            assert_eq!(
                repo.get(&SessionId::from("sesn_0")).await,
                Err(awaken_session_state_contract::SessionRepositoryError::NotFound),
                "{rule}"
            );
        } else {
            assert_eq!(prepared.lock().unwrap().len(), 1, "{rule}");
            if rule == "G3" {
                // Cause: an official empty Skill override replaces 500 Agent
                // defaults. Effect: the one durable Resource manifest is empty;
                // the executable publication must project from that manifest.
                // Rule G3, without a second baseline Skill authority.
                let persisted = repo
                    .get(&SessionId::from(created["id"].as_str().unwrap()))
                    .await
                    .unwrap();
                assert!(persisted.resources.active.skills().is_empty());
                assert!(
                    serde_json::to_value(persisted.frozen_baseline().unwrap())
                        .unwrap()
                        .get("skills")
                        .is_none()
                );
            }
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
impl awaken_session_budget_contract::ManagedListPriceProvider for RecordingListPriceProvider {
    async fn resolve_snapshot(
        &self,
        request: awaken_session_budget_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_budget_contract::ManagedListPriceSnapshot,
        awaken_session_budget_contract::ManagedListPriceError,
    > {
        self.model_rosters
            .lock()
            .unwrap()
            .push(request.model_refs.clone());
        let rates = awaken_session_budget_contract::ManagedTokenListRates {
            input_micros_per_million: 1,
            output_micros_per_million: 1,
            cache_read_micros_per_million: 1,
            cache_creation_micros_per_million: 1,
        };
        Ok(awaken_session_budget_contract::ManagedListPriceSnapshot {
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
struct CountingCredentialMaterialIngress {
    writes: std::sync::atomic::AtomicUsize,
    retirements: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_session_application::CredentialMaterialIngress for CountingCredentialMaterialIngress {
    async fn enter_material(
        &self,
        command: awaken_session_application::CredentialMaterialIngressCommand,
    ) -> Result<awaken_session_application::CredentialMaterialIngressReceipt, String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(
            awaken_session_application::CredentialMaterialIngressReceipt {
                credential: awaken_credential_contract::CredentialRef {
                    id: command.source_id.0,
                    revision: 1,
                },
                provenance: awaken_session_application::SessionParticipantProvenance::Applied,
            },
        )
    }

    async fn rotate_material(
        &self,
        command: awaken_session_application::CredentialMaterialRotationCommand,
    ) -> Result<u64, String> {
        self.writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        command
            .expected_revision
            .checked_add(1)
            .ok_or_else(|| "repository credential revision overflow".into())
    }

    async fn retire_material(
        &self,
        _command: awaken_session_application::CredentialMaterialRetirementCommand,
    ) -> Result<(), String> {
        self.retirements
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
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
        awaken_session_configuration_contract::SessionModelPublication,
        awaken_session_configuration_contract::SessionModelResolutionError,
    >,
    calls: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl awaken_session_configuration_contract::SessionModelPublicationResolver
    for FixedSessionModelResolver
{
    async fn resolve_session_model(
        &self,
        workspace_id: &str,
        model_reference: &str,
    ) -> Result<
        awaken_session_configuration_contract::SessionModelPublication,
        awaken_session_configuration_contract::SessionModelResolutionError,
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
            fixture_publication(
                "model-agent",
                7,
                "openai-account",
                "gpt-5-upstream",
                "genai",
                awaken_runtime_contract::agent_bindings::InferenceOptions {
                    speed: Some(awaken_runtime_contract::agent_bindings::InferenceSpeed::Fast),
                    ..Default::default()
                },
            )
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
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
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
        .session(&SessionId::from(equal_created.id))
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
        awaken_runtime_contract::ModelBinding::new(
            "anthropic-account",
            "claude-sonnet-4-5-20250929",
            "acp:claude",
        ),
    );
    let fallback = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::ModelBinding::new(
            "anthropic-fallback",
            "claude-sonnet-4-5-20250929",
            "genai",
        ),
    );
    let calls: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>> = Default::default();
    let resolved_runtime = AcceptingFake::default();
    let resolved_state = ManagedState::new(resolved_runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPublishedModel))
        .with_model_publication_resolver(std::sync::Arc::new(FixedSessionModelResolver {
            result: Ok(
                awaken_session_configuration_contract::SessionModelPublication {
                    primary: primary.clone(),
                    candidates: vec![fallback.clone()],
                },
            ),
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
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .expect("R2");
    assert_eq!(created.agent.model.id, requested, "R2 public echo");
    {
        let prepared = resolved_runtime.prepared.lock().unwrap();
        assert_eq!(
            prepared[0].model.as_deref(),
            Some(primary.binding().identity().1),
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
        .session(&SessionId::from(created.id))
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
        .create_session(
            session_with_model(requested),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
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
                awaken_session_configuration_contract::SessionModelResolutionError::Invalid(
                    "unsupported model combination".into(),
                ),
            ),
            calls: Default::default(),
        }));
    let error = invalid_state
        .create_session(
            session_with_model(requested),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
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
    // | B11 | Invalid("") / absent | absent       | absent               | absent   | unrepresentable |
    //
    // B11 is refused one level down: `ModelBinding::new` applies the wire
    // admission, so a snapshot can never carry an empty backend reference and
    // the gate is never asked about one. The row is kept as the assertion
    // below so the cause stays documented next to its neighbours.
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
    ];
    assert!(
        matches!(
            awaken_runtime_contract::resolved::ModelBinding::parse(
                "test-model-provider",
                "test-model",
                ""
            ),
            Err(awaken_runtime_contract::resolved::InvalidModelBinding::Backend(_))
        ),
        "B11: an empty backend reference never becomes a binding"
    );

    for rule in rules {
        let runtime = AcceptingFake::default();
        let repo = std::sync::Arc::new(
            SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
        );
        let prices = std::sync::Arc::new(RecordingListPriceProvider::default());
        let credential_ingress = std::sync::Arc::new(CountingCredentialMaterialIngress::default());
        let mut state = ManagedState::new(runtime.clone())
            .with_config_source(std::sync::Arc::new(rule.roster))
            .with_session_repo(repo.clone())
            .with_managed_list_price_provider(prices.clone())
            .with_resource_registry(resource_registry().await)
            .with_credential_material_ingress(credential_ingress.clone());
        if let Some(backend_ref) = rule.override_backend {
            state = state.with_model_publication_resolver(std::sync::Arc::new(
                FixedSessionModelResolver {
                    result: Ok(
                        awaken_session_configuration_contract::SessionModelPublication {
                            primary:
                                awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
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
                        },
                    ),
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
        let result = state
            .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
            .await;

        if let Some(expected_models) = rule.expected_models {
            let created = result.unwrap_or_else(|error| panic!("{}: {error}", rule.id));
            assert!(
                repo.get(&SessionId::from(created.id)).await.is_ok(),
                "{}",
                rule.id
            );
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
                awaken_session_error_contract::RunErrorKind::BadRequest,
                "{}",
                rule.id
            );
            assert!(
                error.message.contains("budget_backend_unsupported"),
                "{}: {error}",
                rule.id
            );
            let recovery = repo
                .scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
                .await
                .unwrap();
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

    let _error = call(&app, "POST", "/v1/sessions", Some(request.clone()))
        .await
        .expect_for(StatusCode::BAD_REQUEST, "C1");
    assert!(runtime.prepared.lock().unwrap().is_empty(), "C1");

    policy
        .allow
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let session = call(&app, "POST", "/v1/sessions", Some(request))
        .await
        .expect_for(StatusCode::OK, "C2");
    let session_id = session["id"].as_str().unwrap();

    policy
        .allow
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let _error = call(
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
    .await
    .expect_for(StatusCode::BAD_REQUEST, "C3");
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
        let id = state
            .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
            .await
            .unwrap()
            .id;
        let durable = repo.get(&SessionId::from(id)).await.unwrap();
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
                networking: awaken_environment_contract::EnvironmentNetworking::Unrestricted {},
                packages: Default::default(),
            },
            sandbox_policy: None,
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
    state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap();
    // Cause: a Cloud create has accepted its exact Environment revision but
    // Runtime realization is now asynchronous. Effect: the one existing
    // supervisor installs the same frozen Environment before we inspect it.
    let realized = state
        .session_application()
        .reconcile_session_realizations()
        .await;
    assert!(
        realized.failures.is_empty(),
        "accepted Environment realizes"
    );
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
    let error = stale
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unavailable"));
}

#[async_trait::async_trait]
impl SessionRuntime for AcceptingFake {
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

    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for AcceptingFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for AcceptingFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for AcceptingFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    fn validate_session_sandbox_layout(
        &self,
        _thread: &str,
        _layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        // Managed adapter decision rule: C1 these contract tests install an
        // accepting Runtime; C2 a manifest may include Repository. E1 the fake
        // explicitly supplies the pure provider-layout port; absent that port,
        // the production default rejects C2 before any adapter effect.
        Ok(())
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        if self
            .stall_projection_install
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            std::future::pending::<()>().await;
        }
        if let Some(init) = awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )? {
            self.prepared.lock().unwrap().push(init);
        }
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for AcceptingFake {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        if self
            .fail_next_reservation_unavailable
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::unavailable(
                "injected Session Run reservation outage",
            ));
        }
        let mut reserved = self.reserved_runs.lock().unwrap();
        if let Some(existing) = reserved.get(&command.run_id.0) {
            if existing != &command {
                return Err(RunError::bad_request(
                    "Session Run reservation conflicts with its durable identity",
                ));
            }
            return Ok(awaken_session_execution_contract::SessionRunReservation::AlreadyReserved);
        }
        reserved.insert(command.run_id.0.clone(), command);
        Ok(awaken_session_execution_contract::SessionRunReservation::Reserved)
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        if self
            .fail_next_activation_unavailable
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::unavailable(
                "injected Session Run activation outage",
            ));
        }
        if self
            .fail_next_activation_internal
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::internal(
                "injected Session Run activation failure",
            ));
        }
        let mut activated = self.activated_runs.lock().unwrap();
        if let Some(existing) = activated.get(&delivery.run_id.0) {
            if existing != &delivery {
                return Err(RunError::internal(
                    "Session Run activation changed its durable delivery",
                ));
            }
            return Ok(
                awaken_session_execution_contract::SessionRunActivation::AlreadyActivated {
                    session_activity_epoch: delivery.session_activity_epoch,
                },
            );
        }
        activated.insert(delivery.run_id.0.clone(), delivery);
        Ok(awaken_session_execution_contract::SessionRunActivation::Activated)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::agent::run::RunState>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_error_contract::RunError,
    > {
        self.streaming_runs()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "StreamingRunRuntime is unsupported by this runtime",
                )
            })?
            .interrupt(thread)
            .await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for AcceptingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Replace the complete Session-local tool configuration after an
    /// idle-session update.
    /// Implementations rebuild the disposable runtime context; durable desired
    /// state has already committed before this projection call.
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .replace_session_tools(thread, tools)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Adopt a previously persisted environment before reopening a Session.
    /// Implementations must validate that the binding belongs to `thread` and
    /// fail closed when it is malformed, unavailable, or owned elsewhere.
    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .adopt_session_environment(agent, thread, binding)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Close Runtime admission and prove that no primary, delegated,
    /// shared-background, MCP, tool, Hand, or child-process effect can mutate
    /// this exact environment generation.
    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
        expected_mcp_generations: &[awaken_session_mcp_contract::McpGenerationRef],
    ) -> Result<
        awaken_session_state_contract::QuiescenceReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .quiesce_session_environment(
                thread,
                operation,
                source_effect_id,
                source_binding,
                generation,
                expected_mcp_generations,
            )
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one verified filesystem checkpoint. Replays with the same
    /// operation id must return the same authoritative object receipt.
    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<
        awaken_session_state_contract::CheckpointReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Complete every live source-durability effect without deleting the
    /// source. The aggregate must durably admit the returned receipt before it
    /// invokes physical disposal.
    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<
        awaken_session_state_contract::SourceReleasePreparedReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Physically dispose the source only after the aggregate has durably
    /// committed the exact source-release preparation receipt.
    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<
        awaken_session_state_contract::SourceDisposedReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Restore a distinct environment from the exact durable checkpoint.
    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<
        awaken_session_state_contract::RestoreReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .restore_checkpointed_session_environment(request)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Idempotently delete checkpoint bytes at a terminal edge. It never
    /// restores the Environment merely to clean it up.
    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_state_contract::SandboxCheckpointRef,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    async fn resolve_session_skills(
        &self,
        _workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<awaken_session_resource_contract::ResolvedSkillBinding>, RunError> {
        Ok(skills
            .iter()
            .map(
                |skill| awaken_session_resource_contract::ResolvedSkillBinding {
                    kind: skill.kind,
                    skill_id: skill.skill_id.clone(),
                    version: 1,
                    bundle_sha256: format!("sha256:{}", skill.skill_id),
                },
            )
            .collect())
    }

    async fn apply_session_inputs(
        &self,
        _thread: &str,
        transition: &awaken_session_resource_contract::SessionResourceTransition,
    ) -> Result<(), RunError> {
        self.applied
            .lock()
            .unwrap()
            .push(transition.desired().resources.clone());
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
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for AcceptingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &SessionId,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        // C1 these tests commit no Run; C2 Event/Session observations use the
        // sole recovery port => Ok(None). Resource-only GET must not call it;
        // the dedicated outage test counts calls and rejects them. Unsupported
        // production recovery remains fail-closed, never a split-read snapshot.
        self.recovery_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .reject_recovery_reads
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::unavailable("injected Runtime history outage"));
        }
        Ok(None)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_error_contract::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for AcceptingFake {
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        _lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<awaken_session_state_contract::SessionRepositoryPublicationEffect, RunError> {
        // Cause/effect fixture rule: C1 application has already verified the
        // exact aggregate lease before entering this Runtime port; C2 the
        // command names one Repository. E1 this accepting fake records one
        // receipt on the same lease-fenced edge as production. The trait has no
        // command-only fallback that could hide a missing ownership proof.
        self.record_terminal_repository_publication(command)
            .map(awaken_session_state_contract::SessionRepositoryPublicationEffect::Published)
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        awaken_protocol_managed::test_support::complete_terminal_cleanup_preparation(
            &effect,
            &authorization,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_preparation(effect)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        Ok(awaken_protocol_managed::test_support::complete_terminal_cleanup_disposal(&effect))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_completed_terminal_cleanup(session_id, lease)
            .await
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for AcceptingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for AcceptingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The advertised capability surface echoed in the session's agent object. The
    /// default reports nothing; a real host overrides it with its built-in tools,
    /// custom tools, skills, and delegate roster so the session enumerates what it does.
    fn capabilities(&self) -> awaken_session_configuration_contract::AgentCapabilities {
        awaken_session_execution_contract::NeutralRuntime.capabilities()
    }

    fn capabilities_for(
        &self,
        _thread: &str,
    ) -> awaken_session_configuration_contract::AgentCapabilities {
        awaken_session_configuration_contract::AgentCapabilities {
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
}

impl AcceptingFake {
    fn record_terminal_repository_publication(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
    ) -> Result<awaken_session_state_contract::SessionRepositoryPublicationReceipt, RunError> {
        let awaken_session_resource_contract::ResolvedInputSource::Repository {
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
            repository_id: repository_id.to_string(),
            source_remote_url: config.remote_url.clone(),
            branch: command.intent.expectation.branch.clone(),
            commit: command.intent.expectation.commit.clone(),
        };
        self.published.lock().unwrap().push(command.clone());
        Ok(
            awaken_session_state_contract::SessionRepositoryPublicationReceipt::new(
                &command,
                effect_receipt,
            ),
        )
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for AcceptingFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_mcp_contract::StageMcpAttachment,
    ) -> Result<awaken_session_mcp_contract::McpRealizationReceipt, RunError> {
        self.staged.lock().unwrap().push(request.clone());
        let receipt_fingerprint = request.fingerprint();
        Ok(awaken_session_mcp_contract::McpRealizationReceipt {
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
        _generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    call_with_headers(app, method, uri, body, &[])
        .await
        .map(|(_headers, body)| body)
}

async fn settle_coordinated_user_run_activity(
    application: &awaken_session_application::SessionApplication,
    session_id: &str,
    rule: &str,
) {
    let session = application
        .session(&SessionId::from(session_id))
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
) -> support::Call<(axum::http::HeaderMap, Value)> {
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
    support::Call::new(status, (headers, value))
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

    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "client-tool-agent"})),
    )
    .await
    .expect_for(StatusCode::OK, "expected status");
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
    // | one toolset per server | absent | preserve the exact server/toolset bijection |
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
    let target = awaken_session_mcp_contract::McpTarget::parse_http("https://mcp.example.test")
        .expect("published MCP target");
    let audience = awaken_session_mcp_contract::McpTarget::identity(
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
    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "integrated"})),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(session["agent"]["mcp_servers"][0]["name"], "docs");
    // The immutable transport and policy sets are one causal integration
    // authority. Session inheritance must preserve exactly one typed policy
    // for each published MCP server, never synthesize a permissive default.
    assert_eq!(session["agent"]["tools"].as_array().unwrap().len(), 2);
    assert_eq!(session["agent"]["tools"][0]["type"], "mcp_toolset");
    assert_eq!(session["agent"]["tools"][0]["mcp_server_name"], "docs");
    assert_eq!(
        session["agent"]["tools"][1]["mcp_server_name"],
        "public-docs"
    );
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
        session["agent"]["tools"],
        json!([
            {
                "type": "mcp_toolset",
                "mcp_server_name": "docs",
                "default_config": {
                    "enabled": true,
                    "permission_policy": {"type": "always_ask"}
                },
                "configs": []
            },
            {
                "type": "mcp_toolset",
                "mcp_server_name": "public-docs",
                "default_config": {
                    "enabled": true,
                    "permission_policy": {"type": "always_ask"}
                },
                "configs": []
            }
        ]),
        "each inherited server retains its exact published MCP toolset policy"
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
        .get(&SessionId::from(session["id"].as_str().unwrap()))
        .await
        .expect("created Session is durable");
    assert_eq!(
        durable.mcp.attachments[0].origin,
        awaken_session_mcp_contract::McpAttachmentOrigin::Agent,
        "credential presence does not define origin"
    );
    assert_eq!(
        durable.mcp.attachments[1].origin,
        awaken_session_mcp_contract::McpAttachmentOrigin::Agent,
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
    let session = call(
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
    .await
    .expect(StatusCode::OK);
    assert_eq!(prepared.lock().unwrap().len(), 1);
    assert_eq!(session["agent"]["mcp_servers"].as_array().unwrap().len(), 1);
    assert_eq!(session["agent"]["mcp_servers"][0]["name"], "docs-alias");
    let durable = repo
        .get(&SessionId::from(session["id"].as_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(durable.mcp.attachments.len(), 1);
    assert_eq!(
        durable.mcp.attachments[0].origin,
        awaken_session_mcp_contract::McpAttachmentOrigin::Agent
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
            .with_resource_registry(resource_registry().await),
    ));
    let session = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" })))
        .await
        .expect(StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();
    (app, id)
}

// Test design: create_time_resources_are_backfilled_and_addressable
// Cause/effect graph: create-time resources become durable activations, then list/retrieve projections.
// Decision table: valid manifest=addressable rows; duplicate/conflicting identity=4xx; missing id=404.
#[tokio::test]
async fn create_time_resources_are_backfilled_and_addressable() {
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_resource_registry(resource_registry().await),
    ));

    let session = call(
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
    .await
    .expect(StatusCode::OK);
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
    let listed = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 2);
    let rid = res[0]["id"].as_str().unwrap();
    let got = call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/resources/{rid}"),
        None,
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(got["type"], "file");
}

#[tokio::test]
async fn implicit_memory_mounts_use_catalog_names_and_disambiguate_collisions() {
    // Cause/effect graph: C1 omitted mount_path -> derive from the governed
    // display name; C2 two names sanitize equally -> qualify the later path with
    // its stable store id.
    // Effects: E1 every returned path is frozen and unique; E2 Runtime receives
    // the same paths; E3 no array-order winner or shared `/mnt/memory/store`.
    // Decision table: P1 C1&&!C2=>name slug; P2 C1+C2=>id-qualified slug;
    // Every official MemoryStore input follows the same derived-path rule.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(resource_registry().await),
    ));
    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [
                {"type": "memory_store", "memory_store_id": "mem_4"},
                {"type": "memory_store", "memory_store_id": "mem_same_1"},
                {"type": "memory_store", "memory_store_id": "mem_same_2"},
                {"type": "memory_store", "memory_store_id": "mem_3"}
            ]
        })),
    )
    .await
    .expect_for(StatusCode::OK, "expected status");
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
            "/mnt/memory/project-memory",
            "/mnt/memory/project-memory-mem-same-2",
            "/mnt/memory/mem-3",
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
        ManagedState::new(runtime).with_resource_registry(resource_registry().await),
    ));
    let resources = (1..=8)
        .map(|index| {
            json!({
                "type": "memory_store",
                "memory_store_id": format!("mem_{index}"),
                "instructions": if index == 1 { "界".repeat(4096) } else { String::new() },
            })
        })
        .collect::<Vec<_>>();
    let _accepted = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "resources": resources })),
    )
    .await
    .expect_for(StatusCode::OK, "M1/M3");
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
            })
        })
        .collect::<Vec<_>>();
    let _ = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "resources": nine })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "M2");
    assert_eq!(
        prepared.lock().unwrap().len(),
        1,
        "M2 no Runtime side effect"
    );

    let _ = call(
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
    .await
    .expect_for(StatusCode::BAD_REQUEST, "M4");
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

    let session = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" })))
        .await
        .expect(StatusCode::OK);
    assert_eq!(session["resources"][0]["type"], "file");
    assert_eq!(session["resources"][0]["file_id"], "file-release");
    assert_eq!(session["resources"][0]["mount_path"], "/mnt/release.txt");
}

// Test design: session_get_projects_the_durable_activity_lifecycle
// Cause/effect graph: durable activity facts select each official Session status across replacement.
// Decision table: one valid lifecycle=one SDK projection; unknown=404; contradictory facts=fail closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_get_projects_the_durable_activity_lifecycle() {
    // Causes: the fixtures below establish `session get` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 the accepted User Event is the global FIFO head,
    // so the request-local canonical Event driver reserves it and commits its
    // durable activity epoch before activation; C2 the one-shot test gate holds
    // that exact canonical activation inside the request; C3 the next
    // sequential supervisor scan observes the released Run's committed terminal
    // truth; C4 the in-process fake's missing Runtime Host callback is
    // represented by the application-owned activity-settlement port used by
    // that real callback.
    // Effects: E1 the Event receipt is returned once the canonical activation
    // port returns (the production Host only publishes the Dispatch row there);
    // E2 GET projects Running while C2 holds; E3 the same aggregate commits
    // Idle and GET projects it. Decision rules: A1=C1+C2=>E2,
    // A2=C1+C2+C3+C4=>E1+E3. Constraint: the gate wraps the shared
    // `CoordinatedRuntimeFake`; the request drive and the one supervisor scan
    // never overlap, and no dispatch lookup or protocol-only status owner
    // participates.
    let runtime = CoordinatedRuntimeFake::default();
    runtime.hold_next_user_run_activation();
    let state = std::sync::Arc::new(ManagedState::new(runtime.clone()));
    let app = router(state.clone());
    let session = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" })))
        .await
        .expect(StatusCode::OK);
    let session_id = session["id"].as_str().unwrap().to_owned();

    let submission = tokio::spawn({
        let app = app.clone();
        let uri = format!("/v1/sessions/{session_id}/events");
        async move {
            call(
                &app,
                "POST",
                &uri,
                Some(json!({
                    "events": [{
                        "type": "user.message",
                        "content": [{"type": "text", "text": "hold"}]
                    }]
                })),
            )
            .await
        }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.wait_for_user_run_activation(),
    )
    .await
    .expect("A1 canonical activation was not reached");

    let running = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None)
        .await
        .expect_for(StatusCode::OK, "A1");
    assert_eq!(running["status"], "running", "A1/E2");

    runtime.release_user_run_activation();
    let receipt = submission
        .await
        .expect("A2 Event submission task")
        .expect_for(StatusCode::OK, "A2/E1");
    assert_eq!(receipt["data"][0]["type"], "user.message", "A2/E1");
    let application = state.session_application();
    Box::pin(application.drive_session_event_batches(&SessionId::from(&session_id), None))
        .await
        .expect("A2 terminal recovery scan");
    settle_coordinated_user_run_activity(&application, &session_id, "A2/C4").await;
    let idle = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None)
        .await
        .expect_for(StatusCode::OK, "A2/E3");
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
    // C1 Published -> E1 a Session may be admitted; C2 the accepted User Event
    // is the FIFO head, so the request-local canonical Event driver commits a
    // Run activity receipt and the canonical one-shot activation gate holds it
    // inside that request; C3 lifecycle changes to Disabled; C4 the next
    // sequential supervisor scan observes terminal truth and the canonical
    // Runtime Host settlement port closes its activity -> E2 that
    // already-admitted Run still settles and its receipt is returned, while E3
    // a new Session and a new Event on the existing Session both fail before
    // any second activation. Constraint: the shared `CoordinatedRuntimeFake`
    // remains the sole reservation/activation implementation; its ordinary
    // `researcher` child is frozen in the fixture roster, and the request drive
    // and supervisor scan never overlap.
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

    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "lifecycle" })),
    )
    .await
    .expect_for(StatusCode::OK, "L1");
    let session_id = session["id"].as_str().unwrap();

    let submission = tokio::spawn({
        let app = app.clone();
        let uri = format!("/v1/sessions/{session_id}/events");
        async move {
            call(
                &app,
                "POST",
                &uri,
                Some(json!({
                    "events": [{
                        "type": "user.message",
                        "content": [{"type": "text", "text": "already admitted"}]
                    }]
                })),
            )
            .await
        }
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
    let receipt = submission
        .await
        .expect("L2 Event submission task")
        .expect_for(StatusCode::OK, "L2 admitted receipt");
    assert_eq!(
        receipt["data"][0]["type"], "user.message",
        "L2 admitted receipt"
    );
    let application = state.session_application();
    Box::pin(application.drive_session_event_batches(&SessionId::from(session_id), None))
        .await
        .expect("L2 terminal recovery scan");
    settle_coordinated_user_run_activity(&application, session_id, "L2/C4").await;
    let settled = application
        .session(&SessionId::from(session_id))
        .await
        .expect("L2/E2 Session");
    assert_eq!(
        settled.execution,
        awaken_session_state_contract::SessionExecutionState::Idle,
        "L2/E2"
    );

    let _ = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "lifecycle" })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "L3");

    let _body = call(
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
    .await
    .expect_for(StatusCode::BAD_REQUEST, "L4");
}

#[tokio::test]
async fn session_resolves_scoped_defaults_and_attachments_once_before_runtime() {
    // Cause/effect graph: C1 the published Agent owns a MemoryStore and File;
    // C2 the Session attaches another MemoryStore without the removed wire
    // mount_path; C3 its derived path differs from the published default.
    // Effects: E1 all three effective bindings cross the one Runtime boundary;
    // E2 the Session attachment remains read-only; E3 neither input replaces
    // another solely because their former client-authored paths matched.
    // Decision rule: C1+C2+C3 => E1+E2+E3.
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let state = ManagedState::new(runtime)
        .with_config_source(std::sync::Arc::new(WorkspaceScopedAgent))
        .with_resource_registry(resource_registry().await);
    let app = router(std::sync::Arc::new(state));

    let session = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "scoped",
            "resources": [{
                "type": "memory_store",
                "memory_store_id": "session-memory",
                "access": "read_only"
            }]
        })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(session["resources"].as_array().unwrap().len(), 3);
    assert_eq!(session["resources"][0]["memory_store_id"], "session-memory");
    assert_eq!(session["resources"][0]["access"], "read_only");

    let calls = prepared.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].workspace_id, "default");
    assert_eq!(calls[0].resources.inputs().len(), 3);
    assert_eq!(
        calls[0].resources.inputs()[0].access,
        ResourceAccess::ReadOnly
    );
    assert!(calls[0].resources.inputs().iter().any(|resource| matches!(
        &resource.source,
        awaken_session_resource_contract::ResolvedInputSource::MemoryStore { memory_store_id, .. }
            if memory_store_id.as_str() == "agent-memory"
    )));
    assert!(calls[0].resources.inputs().iter().any(|resource| matches!(
        &resource.source,
        awaken_session_resource_contract::ResolvedInputSource::File { file_id }
            if file_id.as_str() == "agent-file"
    )));
}

#[tokio::test]
async fn resource_config_publication_only_affects_later_sessions() {
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let catalog = resource_registry().await;
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(catalog.clone()),
    ));
    let request = || {
        json!({
            "agent": "a",
            "resources": [{
                "type": "memory_store",
                "memory_store_id": "mem_1"
            }]
        })
    };

    assert_eq!(
        call(&app, "POST", "/v1/sessions", Some(request()))
            .await
            .status(),
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
        .await
        .expect("publish MemoryStore config V2");
    assert_eq!(
        call(&app, "POST", "/v1/sessions", Some(request()))
            .await
            .status(),
        StatusCode::OK
    );

    let calls = prepared.lock().unwrap();
    let version = |call: &SessionInit| match &call.resources.inputs()[0].source {
        awaken_session_resource_contract::ResolvedInputSource::MemoryStore { config, .. } => {
            config.version
        }
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
        let declared_usage =
            awaken_session_resource_contract::repository_transport_credential_usage();
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
                    awaken_session_resource_contract::repository_transport_credential_usage(),
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
    let catalog = resource_registry().await;
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
    let id = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .id;
    let persisted = repo.get(&SessionId::from(&id)).await.unwrap();
    let awaken_session_resource_contract::ResolvedInputSource::Repository { repository_id, .. } =
        &persisted.resources.active.inputs()[0].source
    else {
        panic!("expected compatibility Repository")
    };
    let definition = catalog
        .find_repository("default", repository_id.as_str())
        .await
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
            .await
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
    let catalog = resource_registry().await;
    let repository = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let state = ManagedState::new(AcceptingFake::default())
        .with_session_repo(repository)
        .with_resource_registry(catalog.clone());
    let application = state.session_application();
    let session_id = "profiled-terminal-repository";
    let repository_id = format!("profiled:{session_id}:repository:0");
    application
        .create_profiled_session(awaken_session_application::CreateProfiledSessionCommand {
            owner_scope: "default".into(),
            session_id: session_id.into(),
            mutation_policy: awaken_session_resource_contract::SessionMutationPolicy::Frozen,
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
                    credential_material: None,
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
        .await
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
            awaken_session_state_contract::ManagedLifecycleFact {
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
            .await
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
    let catalog = resource_registry().await;
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
    let session = call(
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
    .await
    .expect_for(StatusCode::OK, "T1 create");
    let session_id = session["id"].as_str().unwrap();
    let persisted = repository.get(&SessionId::from(session_id)).await.unwrap();
    let awaken_session_resource_contract::ResolvedInputSource::Repository {
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
            .await
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
async fn platform_repository_default_is_shared_and_replaceable_without_a_false_path_conflict() {
    // Agent-default replacement decision table: P1 no Session attachment ->
    // inherit the platform Repository and terminal cleanup leaves it Active;
    // P2 an explicit Repository uses the same authored mount -> the existing
    // SessionInputResolver replacement rule selects the Session-owned Repository
    // rather than treating the replaced default as a second overlapping tree.
    let catalog = resource_registry().await;
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
        .await
        .expect("register platform Repository");
    let state = ManagedState::new(AcceptingFake::default())
        .with_resource_registry(catalog.clone())
        .with_config_source(std::sync::Arc::new(AgentWithPlatformRepository));
    let request = serde_json::from_value(with_session_environment(json!({
        "agent": "repo-agent"
    })))
    .unwrap();
    let id = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .id;
    state.archive_session(&id).await.unwrap();

    assert_eq!(
        catalog
            .find_repository("default", "platform-repository")
            .await
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Active
    );

    let replacement = serde_json::from_value(with_session_environment(json!({
        "agent": "repo-agent",
        "resources": [{
            "type": "github_repository",
            "url": "https://github.com/awaken/replacement.git",
            "mount_path": "/workspace/repository"
        }]
    })))
    .unwrap();
    let replacement = state
        .create_session(replacement, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .expect("P2 same-mount Repository replaces the published default");
    let replacement = state
        .session_application()
        .session(&SessionId::from(replacement.id))
        .await
        .expect("P2 durable replacement Session");
    assert_eq!(replacement.resources.active.inputs().len(), 1, "P2");
    assert_eq!(
        replacement.resources.active.inputs()[0].binding_id.as_str(),
        "platform-repository",
        "P2 retains the replaced logical binding identity"
    );
    assert!(
        matches!(
            &replacement.resources.active.inputs()[0].source,
            awaken_session_resource_contract::ResolvedInputSource::Repository { repository_id, .. }
                if repository_id.as_str() != "platform-repository"
        ),
        "P2 freezes the Session-owned compatibility Repository"
    );
}

#[tokio::test]
async fn duplicate_session_mount_paths_fail_closed_before_runtime() {
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let app = router(std::sync::Arc::new(ManagedState::new(runtime)));

    let _error = call(
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
    .await
    .expect_for(StatusCode::BAD_REQUEST, "expected status");
    assert!(prepared.lock().unwrap().is_empty());
}

// Test design: file_resource_attaches_to_a_live_session
// Cause/effect graph: add on a live Session activates one File mount and durable resource projection.
// Decision table: live+valid=attach; duplicate/conflict follows identity rule; terminal/missing=4xx/404.
#[tokio::test]
async fn invalid_repository_path_preflights_the_whole_create_before_resource_effects() {
    // Whole-create cause/effect table: C1 valid File, Memory, and published Skill
    // inputs precede C2 an invalid explicit Repository path carrying a write-only
    // token. C1+C2 must produce E1 400 with the original path, E2 no Repository
    // Registry/Vault participant, E3 no Session root, and E4 no Runtime staging.
    // The order is intentional: a per-item loop would already have crossed one
    // of those effect boundaries before discovering C2.
    //
    // | Rule | prior inputs | Repository path | token | Effect |
    // |---|---|---|---|---|
    // | R1 | File + Memory + Skill | `/repo` | present | E1+E2+E3+E4 |
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let credentials = std::sync::Arc::new(CountingCredentialMaterialIngress::default());
    let state = ManagedState::new(runtime.clone())
        .with_config_source(std::sync::Arc::new(AgentWithIntegrations))
        .with_session_repo(sessions.clone())
        .with_resource_registry(resource_registry().await)
        .with_credential_material_ingress(credentials.clone());
    let app = router(std::sync::Arc::new(state));

    let error = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "integrated",
            "resources": [
                {"type": "file", "file_id": "file-before-invalid", "mount_path": "/workspace/shared"},
                {"type": "memory_store", "memory_store_id": "mem_1"},
                {
                    "type": "github_repository",
                    "url": "https://github.com/awaken/invalid-path.git",
                    "mount_path": "/repo",
                    "authorization_token": "must-never-enter-vault" // awaken-allow: secret
                }
            ]
        })),
    )
    .await.expect_for(StatusCode::BAD_REQUEST, "R1/E1");
    assert!(error.to_string().contains("/repo"), "R1/E1: {error}");
    assert_eq!(
        credentials.writes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "R1/E2 no Vault ingress"
    );
    assert_eq!(
        credentials
            .retirements
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "R1/E2 no compensation is needed"
    );
    assert!(
        sessions
            .scan(awaken_session_state_contract::SessionScanQuery::Workspace {
                workspace_id: "default"
            })
            .await
            .map(awaken_session_state_contract::SessionScan::into_sessions)
            .unwrap()
            .is_empty(),
        "R1/E3"
    );
    assert!(runtime.prepared.lock().unwrap().is_empty(), "R1/E4");
    assert!(runtime.applied.lock().unwrap().is_empty(), "R1/E4");
    assert!(runtime.staged.lock().unwrap().is_empty(), "R1/E4");
}

#[tokio::test]
async fn file_resource_attaches_to_a_live_session() {
    let (app, id) = app_with_session().await;

    let resource = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "file", "file_id": "file_1", "mount_path": "/workspace/data.csv" })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(resource["type"], "file");
    assert!(resource["id"].is_string(), "the mount is assigned an id");

    // It now shows up in the session's resource listing.
    let listed = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None)
        .await
        .expect(StatusCode::OK);
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
        .with_resource_registry(resource_registry().await);
    let request =
        serde_json::from_value(with_session_environment(json!({ "agent": "a" }))).unwrap();
    let id = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .id;

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
    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty());
    assert!(durable.resources.pending.is_none());
    assert_eq!(
        durable.resources.activations[0].state,
        awaken_session_state_contract::ActivationState::Failed
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
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
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
    let (_, resources) = state.list_resources(&id).await.unwrap();
    assert_eq!(resources.len(), 1, "accepted desired resource is queryable");
    assert_eq!(
        serde_json::to_value(&resources[0]).unwrap()["mount_path"],
        "/retryable.txt"
    );
    let durable = repo.get(&SessionId::from(&id)).await.unwrap();
    assert!(durable.resources.pending.is_some());
    assert!(durable.resources.needs_reconciliation());
    assert_eq!(
        durable.resources.activations.last().unwrap().state,
        awaken_session_state_contract::ActivationState::Prepared
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
    let settled = repo.get(&SessionId::from(id)).await.unwrap();
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
    PrepareConflictsExhausted,
}

/// Resource commands use the repository root CAS as their only serializer.
/// The cases are generated from this cause graph:
///
/// item command + unrelated prepare CAS conflict -> retry against the same
/// Resource generation;
/// durable Prepared + attempt fence + runtime effect + root-only conflict ->
/// settle the same Resource revision on the latest aggregate; a changed or
/// exhausted fence leaves durable pending work for recovery. Runtime failure
/// follows the same settlement path, but records rollback rather than Active.
///
/// | Rule | Runtime | Intent CAS | Attempt CAS | Settlement CAS | Result | Runtime applies | Durable Resource |
/// |------|---------|------------|-------------|----------------|--------|-----------------|------------------|
/// | C1 | success | apply | apply | apply | success | 1 | Active |
/// | C2 | success | conflict once then apply | apply | apply | success | 1 | Active |
/// | C3 | success | apply | conflict once | apply | success | 1 | Active |
/// | C4 | success | apply | apply | conflict once | success | 1 | Active |
/// | C5 | success | apply | conflict x3 | - | conflict | 0 | unattempted pending |
/// | C6 | success | apply | apply | conflict x3 | conflict | 1 | attempted pending |
/// | C7 | fail then rollback | apply | apply | conflict once | runtime error | 2 | Failed/no pending |
/// | C8 | success | another unattempted intent wins | - | - | conflict | 0 | winner pending |
/// | C9 | success | conflict x3 | - | - | conflict | 0 | unchanged |
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
        ResourceCasRule::PrepareConflictsExhausted,
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
        .with_resource_registry(resource_registry().await);
    let request =
        serde_json::from_value(with_session_environment(json!({ "agent": "a" }))).unwrap();
    let id = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .id;

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
        ResourceCasRule::PrepareConflictsExhausted => repo.conflicts_on_next(&[1, 2, 3]),
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
    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    let apply_count = applied.lock().unwrap().len();

    match rule {
        ResourceCasRule::NoConflict
        | ResourceCasRule::PrepareConflictOnce
        | ResourceCasRule::AttemptConflictOnce
        | ResourceCasRule::SettlementConflictOnce => {
            assert!(result.is_ok(), "C{}: {result:?}", index + 1);
            assert_eq!(apply_count, 1, "C{}", index + 1);
            assert!(durable.resources.pending.is_none(), "C{}", index + 1);
            assert_eq!(durable.resources.active.inputs().len(), 1, "C{}", index + 1);
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
                awaken_session_state_contract::ActivationState::Failed,
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
        ResourceCasRule::PrepareConflictsExhausted => {
            assert!(
                matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
                "C9: {result:?}"
            );
            assert_eq!(apply_count, 0, "C9");
            assert!(durable.resources.pending.is_none(), "C9");
            assert!(durable.resources.active.inputs().is_empty(), "C9");
        }
    }
}

#[tokio::test]
async fn file_item_resource_fence_rejects_resource_races_and_rebases_root_races() {
    // Cause/effect graph: C1 create/delete reads Resource generation G; C2 a
    // concurrent Resource mutation commits G+1, or an unrelated root mutation
    // advances only the Session revision; C3 the item verb carries G into the
    // canonical Manifest CAS. E1 a true Resource loser returns 409 without
    // overwrite; E2 an unrelated root loser retries and succeeds; E3 every
    // concurrent fact remains durable.
    //
    // | Rule | Item | Concurrent winner | Item result | Durable File |
    // | F1 | create | Resource generation | 409 | absent |
    // | F2 | delete | metadata/lease root fact | success | deleted |
    let runtime = AcceptingFake::default();
    let inner = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let repo = std::sync::Arc::new(ScheduledConflictRepository::new(inner));
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(repo.clone())
            .with_resource_registry(resource_registry().await),
    );
    let app = router(state.clone());
    let session = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"})))
        .await
        .expect(StatusCode::OK);
    let id = session["id"].as_str().unwrap();

    repo.resource_change_on_next(1);
    let _error = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "file",
            "file_id": "file-race",
            "mount_path": "/race.txt"
        })),
    )
    .await
    .expect_for(StatusCode::CONFLICT, "F1");
    assert!(
        repo.get(&SessionId::from(id))
            .await
            .unwrap()
            .resources
            .desired()
            .inputs()
            .iter()
            .all(|input| !matches!(
                &input.source,
                awaken_session_resource_contract::ResolvedInputSource::File { file_id }
                    if file_id.as_str() == "file-race"
            )),
        "F1 stale File create is absent"
    );

    state.reconcile_resource_activations().await;
    let file = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "file",
            "file_id": "file-delete-race",
            "mount_path": "/delete-race.txt"
        })),
    )
    .await
    .expect_for(StatusCode::OK, "prepare F2");
    let resource_id = file["id"].as_str().unwrap();

    repo.metadata_change_on_next(1, "concurrent", "winner");
    let _deleted = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{id}/resources/{resource_id}"),
        None,
    )
    .await
    .expect_for(StatusCode::OK, "F2/E2");
    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(
        durable.metadata.get("concurrent").map(String::as_str),
        Some("winner")
    );
    assert!(
        durable.resources.desired().inputs().iter().all(|input| {
            !matches!(
                &input.source,
                awaken_session_resource_contract::ResolvedInputSource::File { file_id }
                    if file_id.as_str() == "file-delete-race"
            )
        }),
        "F2/E2 Resource delete rebases over the unrelated root fact"
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
        ManagedState::new(runtime).with_resource_registry(resource_registry().await),
    ));
    let session = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"})))
        .await
        .expect(StatusCode::OK);
    let id = session["id"].as_str().unwrap();
    let before = applied.lock().unwrap().len();

    let _resource = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "github_repository",
            "url": "https://github.com/owner/repo"
        })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "expected status");
    assert_eq!(applied.lock().unwrap().len(), before);

    let listed = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None)
        .await
        .expect(StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

// Test design: a_file_resource_can_be_detached_from_a_live_session
// Cause/effect graph: delete retires the exact File activation without changing unrelated resources.
// Decision table: live+attached=detach; missing=404; terminal=reject without mutation.
#[tokio::test]
async fn a_file_resource_can_be_detached_from_a_live_session() {
    let (app, id) = app_with_session().await;

    let resource = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "file", "file_id": "file_1" })),
    )
    .await
    .expect(StatusCode::OK);
    let rid = resource["id"].as_str().unwrap().to_string();

    let _ = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{id}/resources/{rid}"),
        None,
    )
    .await
    .expect(StatusCode::OK);

    let listed = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None)
        .await
        .expect(StatusCode::OK);
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
    let catalog = resource_registry().await;
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(sessions.clone())
            .with_resource_registry(catalog.clone()),
    ));
    let session = call(
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
    .await
    .expect(StatusCode::OK);
    let session_id = session["id"].as_str().unwrap();
    let resource_id = session["resources"][0]["id"].as_str().unwrap();
    let repository_id = match &sessions
        .get(&SessionId::from(session_id))
        .await
        .unwrap()
        .resources
        .active
        .inputs()[0]
        .source
    {
        awaken_session_resource_contract::ResolvedInputSource::Repository {
            repository_id, ..
        } => repository_id.clone(),
        _ => panic!("U1 Repository fixture"),
    };
    let _body = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        None,
    )
    .await
    .expect_for(StatusCode::OK, "U1/E1");
    let durable = sessions.get(&SessionId::from(session_id)).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty(), "U1/E1");
    assert!(
        durable.resources.repository_retirements().is_empty(),
        "U1/E2"
    );
    assert_eq!(
        catalog
            .find_repository("default", repository_id.as_str())
            .await
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
    let catalog = resource_registry().await;
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let app = router(std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(sessions.clone())
            .with_resource_registry(catalog.clone()),
    ));
    let session = call(
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
    .await
    .expect(StatusCode::OK);
    let session_id = session["id"].as_str().unwrap();
    let repository_id = match &sessions
        .get(&SessionId::from(session_id))
        .await
        .unwrap()
        .resources
        .active
        .inputs()[0]
        .source
    {
        awaken_session_resource_contract::ResolvedInputSource::Repository {
            repository_id, ..
        } => repository_id.clone(),
        _ => panic!("U2 Repository fixture"),
    };
    let _body = call_with_headers(
        &app,
        "PUT",
        &format!("/v1/awaken/sessions/{session_id}/resources"),
        Some(json!({ "resources": [] })),
        &[("idempotency-key", "repository-omission")],
    )
    .await
    .expect_for(StatusCode::OK, "U2/E1");
    let durable = sessions.get(&SessionId::from(session_id)).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty(), "U2/E1");
    assert!(
        durable.resources.repository_retirements().is_empty(),
        "U2/E2"
    );
    assert_eq!(
        catalog
            .find_repository("default", repository_id.as_str())
            .await
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
    // absent/same/different payload; C3 Resource If-Match current/stale; C4
    // Runtime realization succeeds. Effects: E1 GET and PUT expose the one
    // Resource generation ETag; E2 one desired generation is persisted and
    // realized; E3 exact replay wins before a stale precondition and returns
    // current truth without another generation; E4 key mismatch or a new
    // command based on a stale Resource generation is 409; E5 invalid input
    // fails before Runtime. Session-root revisions are intentionally absent
    // from this table: lease/metadata changes are covered by W2 below.
    //
    // | Rule | Manifest | Key | Resource ETag | Result | Effect |
    // |---|---|---|---|---|---|
    // | M0 | valid | new | current | wrong protocol URI | 405 |
    // | M1 | valid | new | current | accepted | E1 + E2 |
    // | M2 | same | same | stale | replay | E3 |
    // | M3 | changed | same | any | reject | E4 mismatch |
    // | M4 | invalid | new | any | reject | E5 |
    // | M5 | valid | new | stale | reject | E4 precondition |
    // FMECA: sequenced delete/add could expose partial sets or duplicate mounts
    // (severity 8, occurrence 6, detection 5); one root CAS plus whole-manifest
    // validation eliminates both intermediate states.
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime).with_resource_registry(resource_registry().await),
    ));
    let session = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" })))
        .await
        .expect(StatusCode::OK);
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
    let (initial_headers, _listed) = call_with_headers(&app, "GET", &compatibility_uri, None, &[])
        .await
        .expect_for(StatusCode::OK, "initial Resource manifest");
    let initial_etag = initial_headers["etag"].to_str().unwrap().to_string();
    let initial_revision = initial_etag
        .trim_matches('"')
        .parse::<u64>()
        .expect("M1 current Resource ETag");
    let _rejected = call_with_headers(
        &app,
        "PUT",
        &compatibility_uri,
        Some(manifest.clone()),
        &[("idempotency-key", "wrong-protocol-owner")],
    )
    .await
    .expect_for(StatusCode::METHOD_NOT_ALLOWED, "M0");
    let (first_headers, first) = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(manifest.clone()),
        &[
            ("idempotency-key", "manifest-1"),
            ("if-match", initial_etag.as_str()),
        ],
    )
    .await
    .expect_for(StatusCode::OK, "M1");
    assert_eq!(first["phase"], "active", "M1/E2");
    assert_eq!(first["resources"].as_array().unwrap().len(), 2, "M1/E2");
    let first_etag = first_headers["etag"].to_str().unwrap().to_string();
    assert_eq!(
        first_etag,
        format!("\"{}\"", initial_revision + 1),
        "M1/E1 one Resource generation"
    );
    assert_eq!(applied.lock().unwrap().len(), before + 1, "M1/E2");

    let (replay_headers, _replay) = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(manifest),
        &[
            ("idempotency-key", "manifest-1"),
            ("if-match", initial_etag.as_str()),
        ],
    )
    .await
    .expect_for(StatusCode::OK, "M2");
    assert_eq!(replay_headers["etag"], first_etag, "M2/E3");
    assert_eq!(applied.lock().unwrap().len(), before + 1, "M2/E3");

    let _mismatch = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(json!({ "resources": [] })),
        &[("idempotency-key", "manifest-1")],
    )
    .await
    .expect_for(StatusCode::CONFLICT, "M3/E4");

    let _stale = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(json!({ "resources": [] })),
        &[
            ("idempotency-key", "manifest-stale-resource-generation"),
            ("if-match", initial_etag.as_str()),
        ],
    )
    .await
    .expect_for(StatusCode::CONFLICT, "M5/E4");

    let _collision = call_with_headers(
        &app,
        "PUT",
        &uri,
        Some(json!({ "resources": [
            { "type": "file", "file_id": "x", "mount_path": "/same" },
            { "type": "file", "file_id": "y", "mount_path": "same" }
        ] })),
        &[],
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "M4/E5");
    assert_eq!(applied.lock().unwrap().len(), before + 1, "M4/E5");

    let (final_headers, listed) = call_with_headers(&app, "GET", &compatibility_uri, None, &[])
        .await
        .expect(StatusCode::OK);
    assert_eq!(final_headers["etag"], first_etag, "M1/E1");
    assert_eq!(listed["data"].as_array().unwrap().len(), 2, "M1/E1");
}

#[tokio::test]
async fn whole_manifest_resource_loser_compensates_its_applied_repository() {
    // Cause/effect graph: C1 whole-manifest lowering creates an Applied
    // Session-owned Repository; C2 a concurrent Resource generation wins after
    // the read; C3 expected_resource_revision prevents rebase/overwrite; C4 the
    // winner does not reference the candidate Repository. Effects: E1 Conflict
    // is preserved; E2 winner Resource intent remains; E3 the Applied candidate
    // Repository is tombstoned.
    //
    // | Rule | Participant | Root race | Root reference | Result | Repository |
    // | W1 | Applied | Resource G+1 | absent | Conflict | Deleted |
    let runtime = AcceptingFake::default();
    let inner = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let repo = std::sync::Arc::new(ScheduledConflictRepository::new(inner));
    let catalog = resource_registry().await;
    let state = ManagedState::new(runtime)
        .with_session_repo(repo.clone())
        .with_resource_registry(catalog.clone());
    let request =
        serde_json::from_value(with_session_environment(json!({ "agent": "a" }))).unwrap();
    let id = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .id;
    let read_revision = repo
        .get(&SessionId::from(&id))
        .await
        .unwrap()
        .resources
        .revision;
    let remote_url = "https://github.com/awaken/manifest-race.git";
    let normalized_mount = "workspace/manifest-race";
    let initial_branch = Some("main".to_string());
    let initial_commit: Option<String> = None;
    let repository_id = format!(
        "managed:{id}:repository:{}",
        awaken_agent_contract::stable_fingerprint(&(
            normalized_mount,
            remote_url,
            &initial_branch,
            &initial_commit,
        ))
    );
    repo.resource_change_on_next(1);
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
    assert!(
        repo.get(&SessionId::from(id))
            .await
            .unwrap()
            .resources
            .pending
            .is_some(),
        "W1/E2"
    );
    assert_eq!(
        catalog
            .find_repository("default", &repository_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ResourceState::Deleted,
        "W1/E3"
    );
}

#[tokio::test]
async fn whole_manifest_rebases_over_unrelated_root_revision_changes() {
    // Cause/effect graph: C1 the caller fences the Resource generation; C2 a
    // concurrent Runtime-lease/metadata mutation advances only the Session root;
    // C3 no Resource generation changed; C4 bounded root CAS retry succeeds.
    // Effects: E1 the Resource command is accepted exactly once; E2 the
    // unrelated root fact is preserved; E3 the Resource generation advances
    // once. This is the production renewal race: root churn must not starve a
    // semantically independent Resource write.
    //
    // | Rule | Resource base | Concurrent root fact | Root CAS | Result |
    // |---|---|---|---|---|
    // | W2 | current G | metadata/lease only | loses once | rebase + E1-E3 |
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let inner = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("open ephemeral Session store"),
    );
    let repo = std::sync::Arc::new(ScheduledConflictRepository::new(inner));
    let state = ManagedState::new(runtime)
        .with_session_repo(repo.clone())
        .with_resource_registry(resource_registry().await);
    let request =
        serde_json::from_value(with_session_environment(json!({ "agent": "a" }))).unwrap();
    let id = state
        .create_session(request, awaken_tenancy::DEFAULT_WORKSPACE_ID)
        .await
        .unwrap()
        .id;
    let resource_revision = repo
        .get(&SessionId::from(&id))
        .await
        .unwrap()
        .resources
        .revision;
    repo.metadata_change_on_next(1, "realization-lease", "renewed");

    // The HTTP adapter owns this same Box boundary because the complete
    // application future is intentionally too large to embed in an Axum or
    // Tokio caller future. Keep the direct integration test faithful to that
    // production boundary while exercising the root-CAS rules above.
    let result = Box::pin(
        state.replace_resource_manifest(
            &id,
            serde_json::from_value(json!({
                "resources": [{
                    "type": "file",
                    "file_id": "file-context",
                    "mount_path": "/context.txt"
                }]
            }))
            .unwrap(),
            Some("manifest-root-rebase".into()),
            Some(resource_revision),
            "manifest-root-rebase-request".into(),
        ),
    )
    .await
    .expect("W2 unrelated root mutation is a retryable implementation race");

    assert_eq!(result.desired_revision, resource_revision + 1, "W2/E3");
    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(
        durable
            .metadata
            .get("realization-lease")
            .map(String::as_str),
        Some("renewed"),
        "W2/E2"
    );
    assert_eq!(durable.resources.active.inputs().len(), 1, "W2/E1");
    assert_eq!(applied.lock().unwrap().len(), 1, "W2/E1 exactly once");
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
            .with_resource_registry(resource_registry().await),
    ));
    let session = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" })))
        .await
        .expect(StatusCode::OK);
    let id = session["id"].as_str().unwrap();
    let uri = format!("/v1/awaken/sessions/{id}/resources");
    let _error = call(
        &app,
        "PUT",
        &uri,
        Some(json!({ "resources": [
            { "type": "file", "file_id": "accepted", "mount_path": "/accepted" }
        ] })),
    )
    .await
    .expect_for(StatusCode::INTERNAL_SERVER_ERROR, "F2");
    let durable = repo.get(&SessionId::from(id)).await.unwrap();
    assert!(durable.resources.active.inputs().is_empty(), "F2 active");
    assert!(durable.resources.pending.is_some(), "F2 desired");

    let listed = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None)
        .await
        .expect(StatusCode::OK);
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
            .with_resource_registry(resource_registry().await),
    ));
    let session = call(
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
    .await
    .expect(StatusCode::OK);
    let id = session["id"].as_str().unwrap();
    let before = repo.get(&SessionId::from(id)).await.unwrap();
    let old = before.resources.desired().inputs()[0].clone();
    let response = call(
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
    .await
    .expect_for(StatusCode::OK, "K1");
    let after = repo.get(&SessionId::from(id)).await.unwrap();
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

    let err = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "memory_store", "memory_store_id": "memstore_1" })),
    )
    .await
    // Managed Agents contract: memory stores are create-time only.
    .expect(StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["type"], "invalid_request_error");

    // Nothing was mounted — the listing stays empty.
    let listed = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None)
        .await
        .expect(StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn getting_or_deleting_an_unknown_session_resource_is_404() {
    let (app, id) = app_with_session().await;
    let uri = format!("/v1/sessions/{id}/resources/res_does_not_exist");
    let _ = call(&app, "GET", &uri, None)
        .await
        .expect(StatusCode::NOT_FOUND);
    let _ = call(&app, "DELETE", &uri, None)
        .await
        .expect(StatusCode::NOT_FOUND);
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
        ManagedState::new(runtime).with_resource_registry(resource_registry().await),
    ));

    let _ = call(
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
    .await
    .expect(StatusCode::BAD_REQUEST);
    assert!(prepared.lock().unwrap().is_empty());

    let session = call(
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
    .await
    .expect(StatusCode::OK);
    let session_id = session["id"].as_str().unwrap();
    let resource_id = session["resources"][0]["id"].as_str().unwrap();
    let applied_before = applied.lock().unwrap().len();
    let _ = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "must-not-enter"})), // awaken-allow: secret
    )
    .await
    .expect(StatusCode::BAD_REQUEST);
    assert_eq!(applied.lock().unwrap().len(), applied_before);
}

// Test design: repository_authorization_is_sealed_pinned_and_rotated_without_echo
// Cause/effect graph: update seals credentials, pins/rotates one revision, and exposes only metadata.
// Decision table: fresh+valid=rotate; stale=409; invalid=4xx; every response/recovery path=no secret.
#[tokio::test]
async fn repository_authorization_is_sealed_pinned_and_rotated_without_echo() {
    // The same real Vault/CAS crash rules cover both desired slots. B6 an
    // already-attempted pending generation, B7 a Repository removed from
    // desired and B10 a terminal root refuse before any Vault write. B8 an
    // unrelated root winner is preserved; B9 an attempted newer Resource
    // generation cannot absorb the completed old rotation.
    // These are logical requested/attempted fixtures, never forged completion
    // receipts. The product management_surface case independently produces the
    // initially pending state through actual AllInOne creation.
    for slot in [
        "active",
        "pending",
        "attempted",
        "removed",
        "terminal",
        "archived",
        "deleting",
        "deleted",
        "metadata-race",
        "generation-race",
        "archive-race",
    ] {
        Box::pin(repository_rotation_slot_case(slot)).await;
    }
}

async fn repository_rotation_slot_case(slot: &str) {
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
        secrets.clone(),
        credentials.clone(),
    ));
    let storage = tempfile::tempdir().unwrap();
    let database = storage.path().join("credential-root.sqlite");
    let session_store = std::sync::Arc::new(
        SqliteManagedSessionRepository::open(database.to_str().unwrap())
            .expect("file Session repository"),
    );
    let sessions = std::sync::Arc::new(ScheduledConflictRepository::new(session_store));
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let applied = runtime.applied.clone();
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime)
            .with_vaults(vaults)
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry().await),
    ));

    let session = call(
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
    .await
    .expect_for(StatusCode::OK, "expected status");
    let session_id = session["id"].as_str().unwrap();
    assert_eq!(
        prepared.lock().unwrap().len(),
        1,
        "B2 Runtime prepared once"
    );
    let serialized = session.to_string();
    assert!(!serialized.contains("credential_binding"));
    assert!(!serialized.contains("never-project-this-secret"));
    let mut durable = sessions.get(&SessionId::from(session_id)).await.unwrap();
    if matches!(slot, "pending" | "attempted" | "removed") {
        let original = durable.resources.active.clone();
        if slot != "removed" {
            durable.resources = Default::default();
        }
        durable
            .resources
            .prepare(
                &durable.session_id,
                if slot == "removed" {
                    Default::default()
                } else {
                    original
                },
            )
            .unwrap();
        if slot == "attempted" {
            durable.resources.start_attempt().unwrap();
        }
        durable = support::replace_session_fixture(
            sessions.as_ref(),
            "default",
            durable,
            &format!("qualification-slot:{session_id}:{slot}"),
        )
        .await;
    }
    if matches!(slot, "terminal" | "archived" | "deleting" | "deleted") {
        // B10 the same aggregate terminality includes non-Active disposition,
        // independently from execution. The archived observation is copied
        // from the actual public creation receipt, not a fabricated clock.
        match slot {
            "terminal" => durable.execution = SessionExecutionState::Terminated,
            "archived" => {
                durable.disposition = SessionDisposition::Archived {
                    archived_at: session["updated_at"].as_str().unwrap().to_owned(),
                }
            }
            "deleting" => durable.disposition = SessionDisposition::Deleting,
            "deleted" => durable.disposition = SessionDisposition::Deleted,
            _ => unreachable!(),
        }
        durable = support::replace_session_fixture(
            sessions.as_ref(),
            "default",
            durable,
            &format!("terminal-rotation:{session_id}"),
        )
        .await;
    }
    let resource_id = session["resources"][0]["id"].as_str().unwrap();
    if matches!(
        slot,
        "attempted" | "removed" | "terminal" | "archived" | "deleting" | "deleted"
    ) {
        let inventory = (
            serde_json::to_value(credentials.list("default").await.unwrap()).unwrap(),
            secrets.inventory().await.unwrap(),
        );
        let response = call(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
            Some(json!({"authorization_token": "must-not-rotate"})), // awaken-allow: secret
        )
        .await;
        response.expect_for(
            if slot == "removed" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::CONFLICT
            },
            "B6/B7/B10 refuse before Vault",
        );
        assert_eq!(
            sessions.get(&SessionId::from(session_id)).await.unwrap(),
            durable,
            "B6/B7 whole root"
        );
        assert_eq!(
            (
                serde_json::to_value(credentials.list("default").await.unwrap()).unwrap(),
                secrets.inventory().await.unwrap()
            ),
            inventory,
            "B6/B7 zero Vault changes",
        );
        return;
    }
    let active_before = durable.resources.active.clone();
    let resource_revision = durable.resources.revision;
    let awaken_session_resource_contract::ResolvedInputSource::Repository {
        config,
        credential,
        ..
    } = &durable.resources.desired().inputs()[0].source
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
                &awaken_session_resource_contract::repository_transport_credential_usage(),
            )
            .is_ok(),
        "B2 exact target/usage"
    );

    let applied_before = applied.lock().unwrap().len();
    if matches!(slot, "generation-race" | "archive-race") {
        // B9 the existing fault owner commits a real newer attempted Resource
        // generation at the first rotation CAS. Vault 2 is a completed old
        // effect, but neither retained manifest may be silently repinned.
        if slot == "generation-race" {
            sessions.resource_change_on_next(1);
        } else {
            // B11 execution remains Idle, but retention denies authoring after
            // the Vault effect. The original generation alone cannot authorize
            // a retry into this archived root.
            sessions.disposition_change_on_next(
                1,
                SessionDisposition::Archived {
                    archived_at: session["updated_at"].as_str().unwrap().to_owned(),
                },
            );
        }
        call(
            &app,
            "POST",
            &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
            Some(json!({"authorization_token": "rotated-secret"})), // awaken-allow: secret
        )
        .await
        .expect_for(StatusCode::CONFLICT, "B9 competing Resource winner");
        let winner = sessions.get(&SessionId::from(session_id)).await.unwrap();
        assert_eq!(
            winner.resources.revision,
            resource_revision + u64::from(slot == "generation-race"),
            "B9/B11 winner"
        );
        assert_eq!(winner.resources.active, active_before, "B9 old active pin");
        assert_eq!(
            winner.resources.desired().inputs()[0].source,
            durable.resources.desired().inputs()[0].source,
            "B9 pending pin unchanged"
        );
        if slot == "generation-race" {
            assert!(
                winner.resources.activations.iter().any(|a| a.attempts == 1),
                "B9 real attempt fence"
            );
        } else {
            assert_eq!(
                winner.execution,
                SessionExecutionState::Idle,
                "B11 independent axes"
            );
            assert!(winner.is_terminal(), "B11 aggregate terminality");
        }
        assert_eq!(
            credentials
                .get(&awaken_credential_contract::CredentialSourceId(
                    credential.access.credential.id.clone()
                ))
                .await
                .unwrap()
                .version,
            2,
            "B9 one Vault rotation"
        );
        assert_eq!(
            applied.lock().unwrap().len(),
            applied_before,
            "B9 no remount"
        );
        return;
    }
    sessions.unavailable_on_next(1);
    let crash = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "rotated-secret"})), // awaken-allow: secret
    )
    .await
    .expect_for(StatusCode::INTERNAL_SERVER_ERROR, "B3");
    assert!(!crash.to_string().contains("rotated-secret"), "B3 no echo");
    assert_eq!(
        applied.lock().unwrap().len(),
        applied_before,
        "B3 changes no Runtime effect"
    );
    let stranded = sessions.get(&SessionId::from(session_id)).await.unwrap();
    let awaken_session_resource_contract::ResolvedInputSource::Repository { credential, .. } =
        &stranded.resources.desired().inputs()[0].source
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
    let different = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "different-secret"})), // awaken-allow: secret
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "B4");
    assert!(
        !different.to_string().contains("different-secret"),
        "B4 no echo"
    );
    let after_different = sessions.get(&SessionId::from(session_id)).await.unwrap();
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

    if slot == "metadata-race" {
        sessions.metadata_change_on_next(1, "credential-independent", "preserved");
    }
    let rotated = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": "rotated-secret"})), // awaken-allow: secret
    )
    .await
    .expect_for(StatusCode::OK, "B5");
    assert!(
        !rotated.to_string().contains("rotated-secret"),
        "B5 no echo"
    );
    assert_eq!(
        applied.lock().unwrap().len(),
        applied_before,
        "B5 repins without remounting the working tree"
    );
    let durable = sessions.get(&SessionId::from(session_id)).await.unwrap();
    if slot == "metadata-race" {
        assert_eq!(
            durable
                .metadata
                .get("credential-independent")
                .map(String::as_str),
            Some("preserved"),
            "B8 rebase only this pin over the other root winner"
        );
    }
    let awaken_session_resource_contract::ResolvedInputSource::Repository { credential, .. } =
        &durable.resources.desired().inputs()[0].source
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
    assert_eq!(
        durable.resources.revision, resource_revision,
        "B5 no new Resource generation"
    );
    if slot == "pending" {
        assert_eq!(
            durable.resources.active, active_before,
            "B5 do not install pending implicitly"
        );
        assert!(
            durable.resources.pending.is_some(),
            "B5 exact pending retained"
        );
        assert!(
            durable
                .resources
                .activations
                .iter()
                .all(|a| a.attempts == 0),
            "B5 remains unattempted"
        );
    }
    let reopened = SqliteManagedSessionRepository::open(database.to_str().unwrap()).unwrap();
    assert_eq!(
        reopened.get(&SessionId::from(session_id)).await.unwrap(),
        durable,
        "B5 exact pin/slot survives reopen"
    );
}

async fn settle_profiled_session(
    state: &std::sync::Arc<ManagedState>,
    workspace_id: &str,
    session_id: &str,
) -> awaken_session_state_contract::PersistedSession {
    // Fixture boundary rule: C1 an accepted profiled root is still Preparing;
    // C2 the exact owner is supplied. Effect E1=C1+C2 enters the canonical
    // execution-admission recovery once and returns the same root after
    // Resource/realization convergence. This wrapper owns no retry, state, or
    // physical effect; foreign-owner rejection remains covered at the existing
    // application boundary rather than being claimed by this exact-owner helper.
    let recovered = state
        .session_application()
        .recover_session_projection(session_id, Some(workspace_id))
        .await
        .expect("profiled Session projection recovers")
        .expect("profiled Session root exists");
    assert_eq!(recovered.owner_scope, workspace_id, "E1 exact owner");
    assert_eq!(
        recovered.session.execution,
        awaken_session_state_contract::SessionExecutionState::Idle,
        "E1 canonical realization converges"
    );
    recovered.session
}

async fn persist_profiled_publication_source(
    state: &std::sync::Arc<ManagedState>,
    sessions: &std::sync::Arc<SqliteManagedSessionRepository>,
    workspace_id: &str,
    session_id: &str,
) {
    let before = settle_profiled_session(state, workspace_id, session_id).await;
    let realization = before
        .realization
        .clone()
        .expect("profiled Session has a current realization lease");
    let environment_fingerprint = before
        .frozen_baseline()
        .expect("profiled Session has a frozen baseline")
        .environment
        .config_fingerprint
        .0
        .clone();
    let intent = awaken_session_state_contract::SessionEnvironmentEffectIntent::new(
        session_id,
        awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
        Some(realization),
    )
    .for_environment(environment_fingerprint);
    let sink = awaken_session_application::RepositoryEnvironmentBindingSink::new(sessions.clone());
    assert_eq!(
        sink.authorize(&intent).await.unwrap(),
        awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized,
        "profiled publication fixture uses the aggregate-authorized live source path"
    );
    let binding = format!("profiled-test-live-source:{session_id}");
    sink.persist(
        awaken_session_state_contract::SessionEnvironmentReceipt::from_intent(
            &intent,
            binding.clone(),
        )
        .expect("authorized Environment receipt"),
    )
    .await
    .expect("persist profiled live Environment source");
    let durable = sessions
        .get(&SessionId::from(session_id))
        .await
        .expect("profiled Session with live source");
    assert_eq!(
        durable.environment.binding(),
        Some(binding.as_str()),
        "live publication source is durable before release"
    );
    assert!(
        durable.revision > before.revision,
        "Environment receipt advances the Session root"
    );
    assert_eq!(
        durable.resources.revision, before.resources.revision,
        "Environment receipt does not invent a Resource generation"
    );
}

#[tokio::test]
async fn profiled_create_accepts_the_durable_root_without_waiting_for_realization() {
    // Profiled-create cause/effect graph: C1 the typed request and complete
    // preflight are valid; C2 the atomic Session root/receipt is absent or an
    // exact replay; C3 physical Runtime projection cannot complete. Effects:
    // E1 a new request returns the stable Session id after the complete root is
    // durable; E2 the root remains Preparing and the Runtime effect is still
    // unentered; E3 exact replay returns the same operation identity without a
    // second physical effect. The sole lifecycle supervisor owns later retry.
    //
    // | Rule | request | receipt | realization | Effect |
    // |---|---|---|---|---|
    // | Q1 | valid | absent | stalled | 202 + E1+E2 |
    // | Q2 | valid | exact | stalled | 202 + E1+E2+E3 |
    // | Q3 | invalid preflight | absent | n/a | reject and no root (adjacent admission tests) |
    //
    // Constraint: the Session id/root is the asynchronous operation authority;
    // the HTTP adapter must not add a Job table or keep physical realization in
    // its request future. This deterministic stall detects synchronous waiting
    // without relying on a slow filesystem, network, or provider.
    let runtime = AcceptingFake::default();
    runtime
        .stall_projection_install
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let app = profiled_router(state.clone(), "default");
    let request = json!({
        "session_id": "profiled-durable-accept",
        "mode": "work_unit",
        "agent_id": "coder"
    });

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        call(&app, "POST", "/v1/awaken/sessions", Some(request.clone())),
    )
    .await;

    let durable = sessions
        .get(&SessionId::from("profiled-durable-accept"))
        .await
        .unwrap();
    assert_eq!(
        durable.execution,
        awaken_session_state_contract::SessionExecutionState::Preparing,
        "Q1/E2"
    );
    assert!(runtime.prepared.lock().unwrap().is_empty(), "Q1/E2");

    let body = response
        .unwrap_or_else(|_| {
            panic!("Q1/E1 profiled create waited on the stalled physical realization")
        })
        .expect_for(StatusCode::ACCEPTED, "Q1/E1");
    assert_eq!(body["id"], "profiled-durable-accept", "Q1/E1");

    let durable_before_replay = sessions
        .get(&SessionId::from("profiled-durable-accept"))
        .await
        .unwrap();
    let replay = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        call(&app, "POST", "/v1/awaken/sessions", Some(request)),
    )
    .await
    .unwrap_or_else(|_| panic!("Q2/E3 exact receipt replay waited on realization"));
    assert_eq!(
        replay.expect(StatusCode::ACCEPTED),
        body,
        "Q2/E3 same operation identity/projection"
    );
    assert_eq!(
        sessions
            .get(&SessionId::from("profiled-durable-accept"))
            .await
            .unwrap(),
        durable_before_replay,
        "Q2/E3 replay does not mutate durable operation truth"
    );
    assert!(runtime.prepared.lock().unwrap().is_empty(), "Q2/E3");
}

#[tokio::test]
async fn execution_evidence_http_reads_the_owned_frozen_root_without_mutation() {
    // Cause/effect decision table:
    // H1 an owned frozen Session with no committed Thread => 200 with the exact
    // baseline and zero Runs; H2 the same id under another Workspace => 404;
    // H3 either read => no Session-root mutation. The adapter must not invent a
    // Run/model call or add a second ownership rule. Rules H1=>E1, H2=>E2,
    // H1+H2=>E3.
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime)
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let owned = profiled_router(state.clone(), "workspace-a");
    let request = json!({
        "session_id": "profiled-execution-evidence",
        "mode": "work_unit",
        "agent_id": "coder"
    });
    let _created = call(&owned, "POST", "/v1/awaken/sessions", Some(request))
        .await
        .expect_for(StatusCode::ACCEPTED, "H1 create");
    let before = sessions
        .get(&SessionId::from("profiled-execution-evidence"))
        .await
        .expect("durable Session root");
    let baseline = before.frozen_baseline().expect("frozen baseline");

    let evidence = call(
        &owned,
        "GET",
        "/v1/awaken/sessions/profiled-execution-evidence/execution-evidence",
        None,
    )
    .await
    .expect_for(StatusCode::OK, "H1/E1");
    assert_eq!(evidence["session_id"], before.session_id.as_str(), "H1/E1");
    assert_eq!(evidence["session_revision"], before.revision.0, "H1/E1");
    assert_eq!(evidence["agent_id"], baseline.agent_id, "H1/E1");
    assert_eq!(evidence["model"], baseline.model, "H1/E1");
    assert_eq!(
        evidence["execution_model_ref"], baseline.execution_model_ref,
        "H1/E1"
    );
    assert_eq!(evidence["runs"], json!([]), "H1/E1");

    let foreign = profiled_router(state, "workspace-b");
    let _ = call(
        &foreign,
        "GET",
        "/v1/awaken/sessions/profiled-execution-evidence/execution-evidence",
        None,
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "H2/E2");
    assert_eq!(
        sessions
            .get(&SessionId::from("profiled-execution-evidence"))
            .await
            .expect("Session after reads"),
        before,
        "H3/E3"
    );
}

#[tokio::test]
async fn profiled_create_replays_and_releases_activation_failed_root_without_restarting_effects() {
    // Failed-replay cause/effect graph: C1 one atomic profiled-create receipt
    // exists; C2 its Session root has reached ActivationFailed with a durable
    // reason; C3 the private request fingerprint and owner are exact; C4 the
    // target Runtime would stall if entered; C5 the durable root is terminal
    // or live; C6 verified cleanup is available. Effects: E1 POST returns 202 with
    // the same Session id; E2 failure state/reason remain byte-identical; E3 no
    // Runtime preparation, replacement operation, or second durable mutation.
    //
    // | rule | receipt | owner/hash | lifecycle | effect |
    // E4 public idle archive rejects the terminal root; E5 existing profiled
    // release joins verified cleanup and preserves the failure state; E6 exact
    // retry is idempotent; E7 live execution remains rejected and unchanged.
    // | F1 | failed root | exact | ActivationFailed | 202 + E1+E2+E3 |
    // | F2 | failed root | mismatch | any | conflict, no effect (adjacent receipt tests) |
    // | F3 | terminal root | exact | ActivationFailed | public archive E4, profiled E5+E6 |
    // | F4 | live root | exact | Running | E7 |
    // | F5 | terminal root | exact | Terminated | E5 |
    let request = json!({
        "session_id": "profiled-failed-replay",
        "mode": "work_unit",
        "agent_id": "coder",
        "metadata": { "case": "failed-replay" }
    });

    let seed_sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("seed Session repository"),
    );
    let seed_state = std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(seed_sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let seed_app = profiled_router(seed_state, "default");
    let _body = call(
        &seed_app,
        "POST",
        "/v1/awaken/sessions",
        Some(request.clone()),
    )
    .await
    .expect_for(StatusCode::ACCEPTED, "F1 seed");
    let mut failed = seed_sessions
        .get(&SessionId::from("profiled-failed-replay"))
        .await
        .expect("seed Session root");
    failed.execution = awaken_session_state_contract::SessionExecutionState::ActivationFailed;
    failed.realization_progress.last_error = Some("sandbox realization failed".into());
    failed.revision = Default::default();

    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("target Session repository"),
    );
    let typed: awaken_protocol_awaken::ProfiledSessionCreate =
        serde_json::from_value(request.clone()).expect("typed private request");
    let receipt = awaken_session_state_contract::IdempotencyRecord {
        key: "profiled:create:profiled-failed-replay".into(),
        payload_hash: awaken_agent_contract::stable_fingerprint(&typed),
    };
    sessions
        .create("default", failed.clone(), receipt.clone(), Vec::new())
        .await
        .expect("seed failed root and atomic receipt");
    // The repository, not the pre-admission candidate, owns the canonical
    // revision used by an exact replay oracle.
    let failed = sessions
        .get(&SessionId::from("profiled-failed-replay"))
        .await
        .expect("durable failed root");
    let runtime = AcceptingFake::default();
    runtime
        .stall_projection_install
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let app = profiled_router(state.clone(), "default");
    let replay = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        call(&app, "POST", "/v1/awaken/sessions", Some(request)),
    )
    .await
    .unwrap_or_else(|_| panic!("F1 replay entered stalled physical realization"))
    .expect_for(StatusCode::ACCEPTED, "F1/E1");
    assert_eq!(replay["id"], "profiled-failed-replay", "F1/E1");
    assert_eq!(
        sessions
            .get(&SessionId::from("profiled-failed-replay"))
            .await
            .unwrap(),
        failed,
        "F1/E2 failure state/reason and operation root are retained"
    );
    assert!(runtime.prepared.lock().unwrap().is_empty(), "F1/E3");

    assert!(
        state
            .archive_session("profiled-failed-replay")
            .await
            .is_err(),
        "F3/E4"
    );
    runtime
        .stall_projection_install
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let released = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-failed-replay/release",
        Some(json!({})),
    )
    .await
    .expect_for(StatusCode::OK, "F3/E5");
    assert_eq!(released["id"], "profiled-failed-replay", "F3/E5");
    assert!(released.get("repository_publication").is_none(), "F3/E5");
    let completed = sessions
        .get(&SessionId::from("profiled-failed-replay"))
        .await
        .unwrap();
    assert_eq!(
        completed.execution,
        SessionExecutionState::ActivationFailed,
        "F3/E5"
    );
    assert!(
        completed
            .verified_terminal_cleanup()
            .unwrap()
            .is_completed(),
        "F3/E5"
    );
    assert!(
        matches!(completed.disposition, SessionDisposition::Archived { .. }),
        "F3/E5"
    );
    let retry = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-failed-replay/release",
        Some(json!({})),
    )
    .await
    .expect_for(StatusCode::OK, "F3/E6");
    assert_eq!(retry, released, "F3/E6");
    assert_eq!(
        sessions
            .get(&SessionId::from("profiled-failed-replay"))
            .await
            .unwrap(),
        completed,
        "F3/E6"
    );

    let mut running = failed.clone();
    running.execution = SessionExecutionState::Running;
    running.realization_progress.last_error = None;
    running.revision = Default::default();
    let running_sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("running Session repository"),
    );
    running_sessions
        .create("default", running, receipt.clone(), Vec::new())
        .await
        .unwrap();
    let before = running_sessions
        .get(&SessionId::from("profiled-failed-replay"))
        .await
        .unwrap();
    let running_state = std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(running_sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let running_app = profiled_router(running_state, "default");
    call(
        &running_app,
        "POST",
        "/v1/awaken/sessions/profiled-failed-replay/release",
        Some(json!({})),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "F4/E7");
    assert_eq!(
        running_sessions
            .get(&SessionId::from("profiled-failed-replay"))
            .await
            .unwrap(),
        before,
        "F4/E7"
    );

    let mut terminated = failed;
    terminated.execution = SessionExecutionState::Terminated;
    terminated.revision = Default::default();
    let terminal_sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("terminal Session repository"),
    );
    terminal_sessions
        .create("default", terminated, receipt, Vec::new())
        .await
        .unwrap();
    let terminal_state = std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(terminal_sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let terminal_app = profiled_router(terminal_state, "default");
    call(
        &terminal_app,
        "POST",
        "/v1/awaken/sessions/profiled-failed-replay/release",
        Some(json!({})),
    )
    .await
    .expect_for(StatusCode::OK, "F5/E5");
    let completed_terminal = terminal_sessions
        .get(&SessionId::from("profiled-failed-replay"))
        .await
        .unwrap();
    assert_eq!(
        completed_terminal.execution,
        SessionExecutionState::Terminated,
        "F5/E5"
    );
    assert!(
        completed_terminal
            .verified_terminal_cleanup()
            .unwrap()
            .is_completed(),
        "F5/E5"
    );
}

#[tokio::test]
async fn profiled_repository_binding_wire_preserves_the_historical_derivation() {
    // Binding migration cause/effect graph: C1 `binding_id` is omitted on the
    // historical private wire; C2 the Session id and Repository index make its
    // former generated identity deterministic; C3 a new caller supplies an
    // explicit non-empty or empty identity; C4 the canonical Environment
    // receipt has durably established one live source without changing the
    // Resource generation. Effects: E1 C1+C2 derives exactly the former binding
    // and preserves create replay; E2 C4 lets that derived binding select the
    // same frozen input for terminal publication; E3 a valid explicit identity
    // remains caller-owned (covered by the adjacent release matrix); E4 an
    // explicitly empty identity is rejected and cannot masquerade as omission.
    //
    // | Rule | wire binding | deterministic Session/index | Effect |
    // |---|---|---|---|
    // | B1 | omitted | yes | historical binding + exact replay |
    // | B2 | omitted | yes, then release selects it from a live source | one publication |
    // | B3 | explicit non-empty | n/a | preserve caller identity |
    // | B4 | explicit empty | n/a | reject before root creation |
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let app = profiled_router(state.clone(), "default");
    let historical_create = json!({
        "session_id": "profiled-historical-binding",
        "mode": "work_unit",
        "agent_id": "coder",
        "repositories": [{
            "remote_url": "https://github.com/awaken/historical.git",
            "mount_path": "/workspace/repository"
        }]
    });
    for _replay in ["first", "replay"] {
        let _body = call(
            &app,
            "POST",
            "/v1/awaken/sessions",
            Some(historical_create.clone()),
        )
        .await
        .expect_for(StatusCode::ACCEPTED, "B1");
    }
    let historical_binding = concat!(
        "profiled:profiled-historical-binding:repository:",
        "profiled:profiled-historical-binding:repository:0"
    );
    persist_profiled_publication_source(
        &state,
        &sessions,
        "default",
        "profiled-historical-binding",
    )
    .await;
    let durable = sessions
        .get(&SessionId::from("profiled-historical-binding"))
        .await
        .unwrap();
    assert_eq!(
        durable.resources.active.inputs()[0].binding_id.as_str(),
        historical_binding,
        "B1 exact former generated identity"
    );
    let _released = call(
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
    .await
    .expect_for(StatusCode::OK, "B2");
    assert_eq!(runtime.published.lock().unwrap().len(), 1, "B2");

    let _ = call(
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
                "mount_path": "/workspace/repository"
            }]
        })),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "B4");
    assert!(
        matches!(
            sessions
                .get(&SessionId::from("profiled-empty-binding"))
                .await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "B4 no root"
    );
}

#[tokio::test]
async fn profiled_create_receipt_replays_a_historical_repository_path_without_reauthoring() {
    // Historical-replay cause/effect graph: C1 a durable root and its create
    // receipt predate strict Repository-path admission; C2 the replay carries
    // the byte-identical former request (`repo`); C3 a new request with
    // that path would now fail admission. Effects: E1 C1+C2 returns the existing
    // Session before lowering; E2 the stored `/repo` value is unchanged;
    // E3 Registry, Vault, and Runtime creation effects remain zero. New-create
    // rejection is covered by the path-admission matrix adjacent to this route.
    //
    // | Rule | durable receipt | request hash | new admission | Effect |
    // |---|---|---|---|---|
    // | H1 | historical exact | exact | bypassed | E1 + E2 + E3 |
    let legacy_wire = json!({
        "session_id": "profiled-legacy-path-replay",
        "mode": "work_unit",
        "agent_id": "coder",
        "repositories": [{
            "remote_url": "https://github.com/awaken/historical-path.git",
            "mount_path": "repository"
        }]
    });

    // Compile one otherwise-current aggregate in an isolated authority, then
    // seed the target store with the exact shape an older compiler persisted:
    // authored `repo` was normalized to `/repo` before storage.
    let seed_sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("seed Session repository"),
    );
    let seed_state = std::sync::Arc::new(
        ManagedState::new(AcceptingFake::default())
            .with_session_repo(seed_sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let seed_app = profiled_router(seed_state.clone(), "default");
    let mut current_wire = legacy_wire.clone();
    current_wire["repositories"][0]["mount_path"] = json!("/workspace/repository");
    let _body = call(&seed_app, "POST", "/v1/awaken/sessions", Some(current_wire))
        .await
        .expect_for(StatusCode::ACCEPTED, "H1 seed");
    let mut historical =
        settle_profiled_session(&seed_state, "default", "profiled-legacy-path-replay").await;
    let (mut inputs, skills) = historical.resources.active.clone().into_parts();
    inputs[0].mount_path = "/repo".into();
    // Replay must use the storage-only decoder: public construction deliberately
    // rejects `/repo`, while durable serde keeps this one historical wire value
    // byte-exact and prevents it from becoming a new-create admission path.
    historical.resources.active = serde_json::from_value(json!({
        "inputs": inputs,
        "skills": skills,
    }))
    .expect("historical aggregate remains durably decodable");
    historical.revision = Default::default();
    let historical_repository_id = match &historical.resources.active.inputs()[0].source {
        awaken_session_resource_contract::ResolvedInputSource::Repository {
            repository_id, ..
        } => repository_id.clone(),
        _ => panic!("historical fixture must retain its Repository input"),
    };

    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("target Session repository"),
    );
    let typed_legacy: awaken_protocol_awaken::ProfiledSessionCreate =
        serde_json::from_value(legacy_wire.clone()).expect("legacy private wire");
    let receipt = awaken_session_state_contract::IdempotencyRecord {
        key: "profiled:create:profiled-legacy-path-replay".into(),
        payload_hash: awaken_agent_contract::stable_fingerprint(&typed_legacy),
    };
    sessions
        .create("default", historical, receipt, Vec::new())
        .await
        .expect("seed historical root and receipt");

    let runtime = AcceptingFake::default();
    let target_registry = resource_registry().await;
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(target_registry.clone()),
    );
    let app = profiled_router(state, "default");
    let _replayed = call(&app, "POST", "/v1/awaken/sessions", Some(legacy_wire))
        .await
        .expect_for(StatusCode::ACCEPTED, "H1/E1");
    assert_eq!(
        sessions
            .get(&SessionId::from("profiled-legacy-path-replay"))
            .await
            .unwrap()
            .resources
            .active
            .inputs()[0]
            .mount_path,
        "/repo",
        "H1/E2 durable path is neither rejected nor rewritten during receipt replay"
    );
    assert_eq!(
        target_registry
            .find_repository("default", historical_repository_id.as_str())
            .await
            .expect("H1/E3 target Registry read"),
        None,
        "H1/E3 receipt replay creates no Repository participant"
    );
    assert!(runtime.prepared.lock().unwrap().is_empty(), "H1/E3");
    assert!(runtime.applied.lock().unwrap().is_empty(), "H1/E3");
    assert!(runtime.staged.lock().unwrap().is_empty(), "H1/E3");
}

#[tokio::test]
async fn profiled_release_projects_one_durable_repository_publication() {
    // Release cause/effect graph: C1 publication is absent/present; C2 the
    // Workspace and frozen binding are exact/foreign; C3 the expectation is
    // first, an exact replay, or a mismatch; C4 the Runtime returns canonical
    // evidence; C5 the canonical Environment receipt has durably established
    // one live source without changing the Resource generation; C6 the same
    // canonical recovery has made the accepted root exactly Idle. Effects: E1
    // ordinary release archives and verifies cleanup without publication; E2 an exact request returns
    // the provisioning receipt only after it is durable in the Session root;
    // E3 exact replay returns the same response and performs no second effect;
    // E4 wrong scope is 404; E5 wrong binding is 400; E6 changed expectation is
    // 409. Rules: P1=!C1+C6=>E1;
    // P2=C1+exact C2+first C3+C4+C5+C6=>E2;
    // P3=C1+exact C2+replay C3=>E3; P4=foreign C2=>E4; P5=wrong binding C2=>E5;
    // P6=mismatch C3=>E6.
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry().await),
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
                "mount_path": "/workspace/repository"
            }]
        })
    };
    let _created = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(create_with_repository("profiled-publication")),
    )
    .await
    .expect_for(StatusCode::ACCEPTED, "P2 create");
    persist_profiled_publication_source(&state, &sessions, "default", "profiled-publication").await;

    let request = json!({
        "repository_publication": {
            "binding_id": "flow-repository",
            "expectation": {
                "branch": "awf/work-unit-1",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            }
        }
    });
    let first = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(request.clone()),
    )
    .await
    .expect_for(StatusCode::OK, "P2/E2");
    // P7 archive with publication records one real timestamp in the durable
    // root; exact HTTP replay below preserves it with the publication receipt.
    let archived = sessions
        .get(&SessionId::from("profiled-publication"))
        .await
        .unwrap()
        .archived_at()
        .map(str::to_owned);
    assert!(
        archived
            .as_deref()
            .and_then(awaken_session_application_contract::rfc3339_to_epoch_millis)
            .is_some_and(|millis| millis > 1_767_225_600_000),
        "P7 actual archive observation"
    );
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
            .get(&SessionId::from("profiled-publication"))
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt(&awaken_session_types::SessionId::from(
                "profiled-publication"
            ))
            .unwrap()
            .is_some(),
        "P2/E2 receipt is durable before response"
    );
    assert_eq!(runtime.published.lock().unwrap().len(), 1, "P2/E2");

    let replay = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(request.clone()),
    )
    .await
    .expect_for(StatusCode::OK, "P3/E3");
    assert_eq!(replay, first, "P3/E3 exact response");
    assert_eq!(runtime.published.lock().unwrap().len(), 1, "P3/E3");

    let mut conflict = request.clone();
    conflict["repository_publication"]["expectation"]["commit"] =
        json!("abcdef0123456789abcdef0123456789abcdef01");
    let _ = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(conflict),
    )
    .await
    .expect_for(StatusCode::CONFLICT, "P6/E6");

    let foreign = profiled_router(state.clone(), "other-workspace");
    let _ = call(
        &foreign,
        "POST",
        "/v1/awaken/sessions/profiled-publication/release",
        Some(request),
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "P4/E4");

    let _created = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(create_with_repository("profiled-publication-wrong-binding")),
    )
    .await
    .expect_for(StatusCode::ACCEPTED, "P5 create");
    let _ = call(
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
    .await
    .expect_for(StatusCode::BAD_REQUEST, "P5/E5");

    let _created = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(json!({
            "session_id": "profiled-legacy-release",
            "mode": "work_unit",
            "agent_id": "coder"
        })),
    )
    .await
    .expect_for(StatusCode::ACCEPTED, "P1 create");
    settle_profiled_session(&state, "default", "profiled-legacy-release").await;
    let released = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-legacy-release/release",
        Some(json!({})),
    )
    .await
    .expect_for(StatusCode::OK, "P1/E1");
    assert!(released.get("repository_publication").is_none(), "P1/E1");
    assert!(
        sessions
            .get(&SessionId::from("profiled-legacy-release"))
            .await
            .unwrap()
            .verified_terminal_cleanup()
            .unwrap()
            .is_completed(),
        "P1/E1 verified cleanup"
    );
    assert_eq!(
        runtime.published.lock().unwrap().len(),
        1,
        "all no-op rules"
    );
}

fn profiled_run_body(operation_id: &str, run_id: &str, narrowing: &str) -> Value {
    json!({
        "agent_id": "coder",
        "operation_id": operation_id,
        "run_id": run_id,
        "messages": [
            {
                "id": format!("flow-execution-envelope:{operation_id}"),
                "role": "System",
                "content": [{"type": "text", "text": "exact execution envelope"}]
            },
            {
                "id": format!("flow-system:{operation_id}"),
                "role": "System",
                "content": [{"type": "text", "text": "Return the required output"}]
            },
            {
                "id": format!("flow-task:{operation_id}"),
                "role": "User",
                "content": [{"type": "text", "text": "Implement the accepted Issue"}]
            }
        ],
        "execution_requirements": {
            "tool_capability_narrowing": narrowing,
            "required_worker_capabilities": ["awaken.flow.coding.v1"]
        }
    })
}

#[tokio::test]
async fn profiled_run_submission_preserves_one_canonical_command_across_replay_and_repair() {
    // Causes: C1 exact durably accepted and recovered profiled Session/owner
    // plus Primary command; C2 exact replay after activation/HTTP response loss;
    // C3 same Run with a changed Agent, operation, Message, requirements, or
    // Session; C4 a distinct OutputRepair command; C5 the Flow Worker capability
    // is later evaluated by canonical placement. Effects: E1 C1
    // reserves/activates every exact field;
    // E2 C2 returns the same receipt without duplicate effects; E3 C3 is 400 and
    // cannot overwrite the first command; E4 C4 preserves Session/Agent, uses
    // DenyAll and PreservePrior; E5 C5 remains frozen in durable admission.
    // Rules R1=C1=>E1, R2=C2=>E2, R3=C3=>E3, R4=C4=>E4, R9=C5=>E5. Runtime Host
    // and Dispatch conformance tests own capability selection/claim behavior;
    // this adapter test owns only exact delivery into that existing authority.
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions)
            .with_resource_registry(resource_registry().await),
    );
    let app = profiled_router(state.clone(), "default");
    for session_id in ["profiled-run-command", "profiled-run-other"] {
        let _created = call(
            &app,
            "POST",
            "/v1/awaken/sessions",
            Some(json!({
                "session_id": session_id,
                "mode": "work_unit",
                "agent_id": "coder"
            })),
        )
        .await
        .expect_for(StatusCode::ACCEPTED, "seed");
        settle_profiled_session(&state, "default", session_id).await;
    }

    let primary = profiled_run_body("work-unit-42", "work-unit-42", "configured");
    let run_uri = "/v1/awaken/sessions/profiled-run-command/runs";
    let first = call(&app, "POST", run_uri, Some(primary.clone()))
        .await
        .expect_for(StatusCode::OK, "R1");
    assert_eq!(
        first,
        json!({"session_id": "profiled-run-command", "run_id": "work-unit-42"}),
        "R1/E1 exact receipt"
    );
    let primary_command = runtime
        .reserved_runs
        .lock()
        .unwrap()
        .get("work-unit-42")
        .cloned()
        .expect("R1 durable reservation");
    assert_eq!(primary_command.session_id, "profiled-run-command", "R1");
    assert_eq!(primary_command.agent_id, "coder", "R1");
    assert_eq!(primary_command.operation_id, "work-unit-42", "R1");
    assert_eq!(primary_command.run_id.0, "work-unit-42", "R1");
    assert_eq!(
        primary_command
            .messages
            .iter()
            .map(|message| (message.id.0.as_str(), message.role, message.text_content()))
            .collect::<Vec<_>>(),
        [
            (
                "flow-execution-envelope:work-unit-42",
                awaken_agent_contract::agent::message::Role::System,
                "exact execution envelope".into(),
            ),
            (
                "flow-system:work-unit-42",
                awaken_agent_contract::agent::message::Role::System,
                "Return the required output".into(),
            ),
            (
                "flow-task:work-unit-42",
                awaken_agent_contract::agent::message::Role::User,
                "Implement the accepted Issue".into(),
            ),
        ],
        "R1 exact Message identity, role, and content"
    );
    assert!(primary_command.data_subject_id.is_none(), "R1");
    assert!(primary_command.traceparent.is_none(), "R1");
    assert_eq!(
        primary_command
            .execution_requirements
            .tool_capability_narrowing,
        awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured,
        "R1"
    );
    assert_eq!(
        primary_command
            .execution_requirements
            .required_worker_capabilities,
        ["awaken.flow.coding.v1".to_string()].into_iter().collect(),
        "R9/E5"
    );
    assert_eq!(
        primary_command.replacement,
        awaken_session_execution_contract::SessionRunReplacement::PreservePrior,
        "R1"
    );

    let replay = call(&app, "POST", run_uri, Some(primary.clone()))
        .await
        .expect_for(StatusCode::OK, "R2");
    assert_eq!(replay, first, "R2/E2 stable receipt");
    assert_eq!(runtime.reserved_runs.lock().unwrap().len(), 1, "R2/E2");
    assert_eq!(runtime.activated_runs.lock().unwrap().len(), 1, "R2/E2");

    let mut changed_requirements = primary.clone();
    changed_requirements["execution_requirements"]["tool_capability_narrowing"] = json!("deny_all");
    let _ = call(&app, "POST", run_uri, Some(changed_requirements))
        .await
        .expect_for(StatusCode::BAD_REQUEST, "R3 requirements");
    let mut changed_agent = primary.clone();
    changed_agent["agent_id"] = json!("other-agent");
    let _ = call(&app, "POST", run_uri, Some(changed_agent))
        .await
        .expect_for(StatusCode::BAD_REQUEST, "R3 Agent");
    let mut changed_operation = primary.clone();
    changed_operation["operation_id"] = json!("another-operation");
    let _ = call(&app, "POST", run_uri, Some(changed_operation))
        .await
        .expect_for(StatusCode::BAD_REQUEST, "R3 operation");
    let mut changed_message = primary.clone();
    changed_message["messages"][2]["content"][0]["text"] = json!("different task");
    let _ = call(&app, "POST", run_uri, Some(changed_message))
        .await
        .expect_for(StatusCode::BAD_REQUEST, "R3 Message");
    let _ = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-run-other/runs",
        Some(primary),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "R3 Session");
    assert_eq!(runtime.reserved_runs.lock().unwrap().len(), 1, "R3/E3");

    let repair = profiled_run_body(
        "work-unit-42-output-repair-1",
        "work-unit-42-output-repair-1",
        "deny_all",
    );
    let _repair_receipt = call(&app, "POST", run_uri, Some(repair))
        .await
        .expect_for(StatusCode::OK, "R4");
    let repair_command = runtime
        .reserved_runs
        .lock()
        .unwrap()
        .get("work-unit-42-output-repair-1")
        .cloned()
        .expect("R4 distinct repair reservation");
    assert_eq!(repair_command.session_id, primary_command.session_id, "R4");
    assert_eq!(repair_command.agent_id, primary_command.agent_id, "R4");
    assert_eq!(
        repair_command
            .execution_requirements
            .tool_capability_narrowing,
        awaken_runtime_contract::permission::ToolCapabilityNarrowing::DenyAll,
        "R4"
    );
    assert_eq!(
        repair_command.replacement,
        awaken_session_execution_contract::SessionRunReplacement::PreservePrior,
        "R4"
    );
    assert_eq!(runtime.reserved_runs.lock().unwrap().len(), 2, "R4/E4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profiled_run_commits_into_the_existing_managed_read_projection() {
    // Causes: C1 an exact profiled Session is durably accepted/recovered and its
    // Run is admitted while canonical activation is held after the Session
    // activity receipt; C2 Runtime then commits the Run lifecycle, output, and
    // usage facts; C3 the application settles that exact activity; C4 the
    // product retries after completion.
    // C5 that Run commits a batch-local tool call projected through the Managed
    // Event codec. Effects: E1 ordinary Managed GET projects Running during C1;
    // E2 the same GET projects Idle after C2+C3; E3 ordinary Managed SSE
    // backfills Running, output, usage, then terminal truth in commit order; E4
    // C4 returns the same receipt without another activity; E5 the Awaken
    // execution-evidence endpoint names that tool use with the exact same
    // qualified public identity as ordinary Managed Events. Decision rules
    // R10a=C1=>E1, R10b=C1+C2+C3=>E2+E3, R2c=C4=>E4, and
    // R10c=C2+C5=>E5. The shared Runtime fake implements the existing
    // SessionRuntime commit ports; this route adds no Event fallback, projector,
    // transcript, identity codec, or status owner.
    let runtime = CoordinatedRuntimeFake::default();
    runtime.hold_next_user_run_activation();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions)
            .with_resource_registry(resource_registry().await),
    );
    let app = router(state.clone()).merge(profiled_router(state.clone(), "default"));
    let session_id = "profiled-run-projection";
    let _created = call(
        &app,
        "POST",
        "/v1/awaken/sessions",
        Some(json!({
            "session_id": session_id,
            "mode": "work_unit",
            "agent_id": "coder"
        })),
    )
    .await
    .expect_for(StatusCode::ACCEPTED, "R10 seed");
    settle_profiled_session(&state, "default", session_id).await;

    let run_uri = format!("/v1/awaken/sessions/{session_id}/runs");
    let request = profiled_run_body("work-unit-45", "work-unit-45", "configured");
    let run_app = app.clone();
    let run_uri_for_task = run_uri.clone();
    let request_for_task = request.clone();
    let submission = tokio::spawn(async move {
        call(&run_app, "POST", &run_uri_for_task, Some(request_for_task)).await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime.wait_for_user_run_activation(),
    )
    .await
    .expect("R10a canonical activation was not reached");

    let running = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None)
        .await
        .expect_for(StatusCode::OK, "R10a/E1");
    assert_eq!(running["status"], "running", "R10a/E1");

    runtime.release_user_run_activation();
    let receipt = submission
        .await
        .expect("R10 Run submission task")
        .expect_for(StatusCode::OK, "R10b receipt");
    assert_eq!(
        receipt,
        json!({"session_id": session_id, "run_id": "work-unit-45"}),
        "R10b exact receipt"
    );
    let application = state.session_application();
    settle_coordinated_user_run_activity(&application, session_id, "R10b/C3").await;

    let idle = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None)
        .await
        .expect_for(StatusCode::OK, "R10b/E2");
    assert_eq!(idle["status"], "idle", "R10b/E2");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/sessions/{session_id}/events/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "R10b/E3");
    let sse = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        response.into_body().collect(),
    )
    .await
    .expect("R10b terminal SSE did not close")
    .unwrap()
    .to_bytes();
    let sse = String::from_utf8(sse.to_vec()).unwrap();
    let running_at = sse
        .find("event: session.status_running")
        .expect("R10b Running");
    let output_at = sse.find("event: agent.message").expect("R10b output");
    let usage_at = sse.find("event: session.usage").expect("R10b usage");
    let idle_at = sse
        .find("event: session.status_idle")
        .expect("R10b terminal");
    assert!(
        running_at < output_at && output_at < usage_at && usage_at < idle_at,
        "R10b/E3 committed order:\n{sse}"
    );

    let replay = call(&app, "POST", &run_uri, Some(request))
        .await
        .expect_for(StatusCode::OK, "R2c/E4");
    assert_eq!(replay, receipt, "R2c/E4 stable completed receipt");
    assert!(
        application
            .session(&SessionId::from(session_id))
            .await
            .expect("R2c durable Session")
            .active_activity_epochs
            .is_empty(),
        "R2c/E4 completed replay starts no activity"
    );

    let events = call(
        &app,
        "GET",
        &format!("/v1/sessions/{session_id}/events"),
        None,
    )
    .await
    .expect_for(StatusCode::OK, "R10c events");
    let public_tool_ids = events["data"]
        .as_array()
        .expect("R10c Managed Event page")
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("agent.tool_use" | "agent.custom_tool_use" | "agent.mcp_tool_use")
            )
        })
        .map(|event| {
            event["id"]
                .as_str()
                .expect("R10c Managed tool-use event identity")
                .to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(!public_tool_ids.is_empty(), "R10c/C5");

    let evidence = call(
        &app,
        "GET",
        &format!("/v1/awaken/sessions/{session_id}/execution-evidence"),
        None,
    )
    .await
    .expect_for(StatusCode::OK, "R10c evidence");
    let evidence_tool_ids = evidence["runs"]
        .as_array()
        .expect("R10c execution-evidence Runs")
        .iter()
        .flat_map(|run| {
            run["tool_use_ids"]
                .as_array()
                .expect("R10c Run tool-use identities")
        })
        .map(|id| {
            id.as_str()
                .expect("R10c execution-evidence tool-use identity")
                .to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(evidence_tool_ids, public_tool_ids, "R10c/E5");
    assert!(
        evidence_tool_ids
            .iter()
            .all(|id| id.starts_with("mtool_v1_")),
        "R10c/E5 raw Runtime-local call ids must not escape"
    );
}

#[tokio::test]
async fn profiled_run_submission_fails_closed_by_target_and_error_class() {
    // Causes: C1 target missing/foreign/ordinary Managed; C2
    // reservation or activation unavailable; C3 unknown field or missing
    // Workspace; C4 activation invariant failure; C5 exact retry after the
    // post-reservation failure. Effects: E1 C1=>404/no implicit create; E2
    // C2=>503; E3 malformed=>400 and internal=>500; E4 C5 converges on one
    // reservation/activation and the same receipt. Rules R5=C1=>E1,
    // R6=C2=>E2, R7=C3|C4=>E3, R2=C5=>E4. No Event or background fallback is
    // mounted in this harness.
    let runtime = AcceptingFake::default();
    let sessions = std::sync::Arc::new(
        SqliteManagedSessionRepository::open_in_memory().expect("Session repository"),
    );
    let state = std::sync::Arc::new(
        ManagedState::new(runtime.clone())
            .with_session_repo(sessions.clone())
            .with_resource_registry(resource_registry().await),
    );
    let app = profiled_router(state.clone(), "default");
    for session_id in [
        "profiled-run-errors",
        "profiled-run-reservation-unavailable",
        "profiled-run-activation-unavailable",
    ] {
        let _body = call(
            &app,
            "POST",
            "/v1/awaken/sessions",
            Some(json!({
                "session_id": session_id,
                "mode": "work_unit",
                "agent_id": "coder"
            })),
        )
        .await
        .expect_for(StatusCode::ACCEPTED, "seed");
        settle_profiled_session(&state, "default", session_id).await;
    }

    let request = profiled_run_body("work-unit-43", "work-unit-43", "configured");
    let _ = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-run-missing/runs",
        Some(request.clone()),
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "R5 missing");
    assert!(
        matches!(
            sessions.get(&SessionId::from("profiled-run-missing")).await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "R5 no implicit create"
    );

    let foreign = profiled_router(state.clone(), "other-workspace");
    let _ = call(
        &foreign,
        "POST",
        "/v1/awaken/sessions/profiled-run-errors/runs",
        Some(request.clone()),
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "R5 foreign");

    let managed_app = router(state.clone());
    let managed = call(
        &managed_app,
        "POST",
        "/v1/sessions",
        Some(with_session_environment(json!({"agent": "coder"}))),
    )
    .await
    .expect_for(StatusCode::OK, "seed Managed target");
    let managed_id = managed["id"].as_str().expect("Managed Session id");
    let _ = call(
        &app,
        "POST",
        &format!("/v1/awaken/sessions/{managed_id}/runs"),
        Some(request.clone()),
    )
    .await
    .expect_for(StatusCode::NOT_FOUND, "R5 Managed target");

    runtime
        .fail_next_reservation_unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-run-reservation-unavailable/runs",
        Some(request.clone()),
    )
    .await
    .expect_for(StatusCode::SERVICE_UNAVAILABLE, "R6 reservation");

    let activation_request = profiled_run_body("work-unit-44", "work-unit-44", "configured");
    runtime
        .fail_next_activation_unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let activation_uri = "/v1/awaken/sessions/profiled-run-activation-unavailable/runs";
    let _ = call(
        &app,
        "POST",
        activation_uri,
        Some(activation_request.clone()),
    )
    .await
    .expect_for(StatusCode::SERVICE_UNAVAILABLE, "R6 activation");
    let recovered = call(&app, "POST", activation_uri, Some(activation_request))
        .await
        .expect_for(StatusCode::OK, "R2 retry unavailable activation");
    assert_eq!(recovered["run_id"], "work-unit-44", "R2");

    let mut unknown = request.clone();
    unknown["fallback_to_events"] = json!(true);
    let _ = call(
        &app,
        "POST",
        "/v1/awaken/sessions/profiled-run-errors/runs",
        Some(unknown),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "R7 unknown field");

    let unscoped = awaken_protocol_awaken::profiled_session_run_router(
        awaken_protocol_managed::submit_profiled_session_run,
    )
    .with_state(state);
    let _ = call(
        &unscoped,
        "POST",
        "/v1/awaken/sessions/profiled-run-errors/runs",
        Some(request.clone()),
    )
    .await
    // The Workspace is now refused by the extractor, before the handler body,
    // with the same non-disclosing 404 every other tenant-fenced route uses —
    // not a 400 describing the deployment's missing tenancy wiring.
    .expect_for(StatusCode::NOT_FOUND, "R7 missing Workspace");

    runtime
        .fail_next_activation_internal
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error_uri = "/v1/awaken/sessions/profiled-run-errors/runs";
    let _status = call(&app, "POST", error_uri, Some(request.clone()))
        .await
        .expect_for(StatusCode::INTERNAL_SERVER_ERROR, "R7 internal");
    let recovered = call(&app, "POST", error_uri, Some(request))
        .await
        .expect_for(StatusCode::OK, "R2 internal response-loss retry");
    assert_eq!(recovered["run_id"], "work-unit-43", "R2");
    assert_eq!(runtime.reserved_runs.lock().unwrap().len(), 2, "R2/R6");
    assert_eq!(runtime.activated_runs.lock().unwrap().len(), 2, "R2/R6");
}

#[tokio::test]
async fn resource_reads_use_durable_root_without_runtime_history_or_cache_rehydration() {
    // Causes: C1 a readable durable root owns a desired resource; C2 Runtime
    // history reads fail; C3 Managed cache is warm/cold; C4 a peer has committed
    // a newer desired generation; C5 Session/resource identity is absent/wrong.
    // Effects: E1 resource list/detail and resource ETag come from that root;
    // E2 zero Runtime recovery reads/effects; E3 no stale cache hides C4;
    // E4 C5 => 404; E5 Event history still fails through its original health seam.
    // Rules R1=C1+C2+warm => E1/E2; R2=C1+C2+cold => E1/E2;
    // R3=R1+C4 => E3; R4=C5 => E4; R5=C2+Events GET => E5.
    // Same HTTP routes/DTO mapper/Session read-visibility authority in all rows.
    use std::sync::{Arc, atomic::Ordering};
    let runtime = AcceptingFake::default();
    let reads = runtime.recovery_reads.clone();
    let reject = runtime.reject_recovery_reads.clone();
    let applied = runtime.applied.clone();
    let repo = Arc::new(SqliteManagedSessionRepository::open_in_memory().unwrap());
    let warm = Arc::new(ManagedState::new(runtime.clone()).with_session_repo(repo.clone()));
    let session = warm
        .create_session(
            serde_json::from_value(with_session_environment(json!({"agent":"a"}))).unwrap(),
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .unwrap();
    let id = session.id;
    let resource = warm
        .create_resource(
            &id,
            serde_json::from_value(json!({
                "type":"file", "file_id":"file-first", "mount_path":"/first.txt"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let expected = serde_json::to_value(resource).unwrap();
    let resource_id = expected["id"].as_str().unwrap();
    let cold = Arc::new(ManagedState::new(runtime).with_session_repo(repo.clone()));
    let warm_app = router(warm.clone());
    let cold_app = router(cold.clone());
    reject.store(true, Ordering::SeqCst);
    reads.store(0, Ordering::SeqCst);
    let effects = applied.lock().unwrap().len();
    for (rule, app) in [("R1 warm", &warm_app), ("R2 cold", &cold_app)] {
        let (headers, body) = call_with_headers(
            app,
            "GET",
            &format!("/v1/sessions/{id}/resources"),
            None,
            &[],
        )
        .await
        .expect_for(StatusCode::OK, rule);
        assert_eq!(body["data"], json!([expected.clone()]), "{rule}");
        assert_eq!(
            headers["etag"].to_str().unwrap(),
            format!(
                "\"{}\"",
                repo.get(&SessionId::from(&id))
                    .await
                    .unwrap()
                    .resources
                    .revision
            )
        );
        let detail = call(
            app,
            "GET",
            &format!("/v1/sessions/{id}/resources/{resource_id}"),
            None,
        )
        .await
        .expect_for(StatusCode::OK, rule);
        assert_eq!(detail, expected, "{rule}");
        assert_eq!(reads.load(Ordering::SeqCst), 0, "{rule}: no history query");
        assert_eq!(
            applied.lock().unwrap().len(),
            effects,
            "{rule}: read has no effects"
        );
    }
    // Peer command advances durable desired truth while warm's wire record stays
    // older. Only the existing command owns mutation; the reader never repairs it.
    reject.store(false, Ordering::SeqCst);
    cold.create_resource(
        &id,
        serde_json::from_value(json!({
            "type":"file", "file_id":"file-second", "mount_path":"/second.txt"
        }))
        .unwrap(),
    )
    .await
    .unwrap();
    reject.store(true, Ordering::SeqCst);
    reads.store(0, Ordering::SeqCst);
    let (headers, body) = call_with_headers(
        &warm_app,
        "GET",
        &format!("/v1/sessions/{id}/resources"),
        None,
        &[],
    )
    .await
    .expect_for(StatusCode::OK, "R3 peer generation");
    assert_eq!(body["data"].as_array().unwrap().len(), 2);
    assert_eq!(
        headers["etag"].to_str().unwrap(),
        format!(
            "\"{}\"",
            repo.get(&SessionId::from(&id))
                .await
                .unwrap()
                .resources
                .revision
        )
    );
    for uri in [
        "/v1/sessions/missing/resources".to_string(),
        format!("/v1/sessions/{id}/resources/foreign:resource:id"),
    ] {
        call(&warm_app, "GET", &uri, None)
            .await
            .expect_for(StatusCode::NOT_FOUND, "R4 identity");
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0, "R3/R4 no history query");
    call(&warm_app, "GET", &format!("/v1/sessions/{id}/events"), None)
        .await
        .expect_for(
            StatusCode::SERVICE_UNAVAILABLE,
            "R5 original history health",
        );
    assert!(
        reads.load(Ordering::SeqCst) > 0,
        "R5 actually exercised the failing dependency"
    );
}
