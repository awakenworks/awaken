//! Post-creation session-resource CRUD (`/v1/sessions/{id}/resources`) mirrors the
//! Managed Agents contract: `file` and `github_repository` attach to a live
//! session, but a `memory_store` binds at session-create time only — attaching one
//! to a running session fails closed with a 400 (`invalid_request_error`).

mod support;

use awaken_admin_config_api::SqliteAdminStore;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_protocol_managed::{
    AgentConfigSource, AgentConfigView, ManagedState, OutcomeReport, RunError, SessionInit,
    SessionRuntime, StepOutcome, ToolPermissionDecision, router,
};
use awaken_resource_contract::{
    BindingId, ClonePolicy, ConfigVersion, ExtractionPolicy, FileId, InputBinding, InputResourceId,
    MemoryStoreConfigVersion, MemoryStoreDefinition, MemoryStoreId, RecallPolicy,
    RepositoryConfigVersion, RepositoryDefinition, RepositoryId, ResourceAccess, ResourceCatalog,
    ResourceState, RetentionPolicy,
};
use awaken_session_contract::ManagedSessionRepository;
use awaken_session_store::SqliteManagedSessionRepository;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use support::ScheduledConflictRepository;
use tower::ServiceExt;

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

fn resource_catalog() -> std::sync::Arc<SqliteAdminStore> {
    let catalog = std::sync::Arc::new(
        SqliteAdminStore::open_in_memory().expect("open ephemeral Resource Catalog"),
    );
    for id in ["mem_1", "agent-memory", "session-memory"] {
        catalog
            .create_memory_store(
                MemoryStoreDefinition {
                    id: id.into(),
                    workspace_id: "default".into(),
                    name: id.into(),
                    description: String::new(),
                    metadata: Default::default(),
                    state: ResourceState::Active,
                    current_config_version: ConfigVersion::INITIAL,
                    timestamps: Default::default(),
                },
                MemoryStoreConfigVersion {
                    memory_store_id: id.into(),
                    version: ConfigVersion::INITIAL,
                    recall_policy: RecallPolicy::default(),
                    extraction_policy: ExtractionPolicy::default(),
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
}

struct AgentWithResources;

impl AgentConfigSource for AgentWithResources {
    fn agent_view_in(&self, _workspace_id: &str, agent_id: &str) -> Option<AgentConfigView> {
        (agent_id == "a").then(|| AgentConfigView {
            model: None,
            system: None,
            tool_ids: Vec::new(),
            mcp_servers: Vec::new(),
            skill_ids: Vec::new(),
            delegate_ids: Vec::new(),
            resources: vec![input(
                "release-notes",
                InputResourceId::File(FileId::from("file-release")),
                "/mnt/release.txt",
                ResourceAccess::ReadOnly,
            )],
        })
    }
}

struct AgentWithIntegrations;

impl AgentConfigSource for AgentWithIntegrations {
    fn agent_view_in(&self, _workspace_id: &str, agent_id: &str) -> Option<AgentConfigView> {
        (agent_id == "integrated").then(|| AgentConfigView {
            model: None,
            system: None,
            tool_ids: Vec::new(),
            mcp_servers: vec![
                awaken_protocol_managed::AgentMcpServerView {
                    name: "docs".into(),
                    url: "https://mcp.example.test".into(),
                    credential_source_id: Some("cred:workspace:docs".into()),
                    credential_revision: Some(7),
                },
                awaken_protocol_managed::AgentMcpServerView {
                    name: "public-docs".into(),
                    url: "https://public.example.test".into(),
                    credential_source_id: None,
                    credential_revision: None,
                },
            ],
            skill_ids: vec!["skill_release".into()],
            delegate_ids: vec!["researcher".into()],
            resources: Vec::new(),
        })
    }
}

struct AgentWithPlatformRepository;

impl AgentConfigSource for AgentWithPlatformRepository {
    fn agent_view_in(&self, _workspace_id: &str, agent_id: &str) -> Option<AgentConfigView> {
        (agent_id == "repo-agent").then(|| AgentConfigView {
            model: None,
            system: None,
            tool_ids: Vec::new(),
            mcp_servers: Vec::new(),
            skill_ids: Vec::new(),
            delegate_ids: Vec::new(),
            resources: vec![input(
                "platform-repository",
                InputResourceId::Repository(RepositoryId::from("platform-repository")),
                "/workspace/repository",
                ResourceAccess::ReadWrite,
            )],
        })
    }
}

struct WorkspaceScopedAgent;

impl AgentConfigSource for WorkspaceScopedAgent {
    fn agent_view_in(&self, workspace_id: &str, agent_id: &str) -> Option<AgentConfigView> {
        (workspace_id == "default" && agent_id == "scoped").then(|| AgentConfigView {
            model: None,
            system: None,
            tool_ids: Vec::new(),
            mcp_servers: Vec::new(),
            skill_ids: Vec::new(),
            delegate_ids: Vec::new(),
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
        })
    }
}

#[async_trait::async_trait]
impl SessionRuntime for AcceptingFake {
    async fn prepare_session(&self, _thread: &str, init: SessionInit) -> Result<(), RunError> {
        self.prepared.lock().unwrap().push(init);
        Ok(())
    }
    fn capabilities_for(&self, _thread: &str) -> awaken_protocol_managed::AgentCapabilities {
        awaken_protocol_managed::AgentCapabilities {
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
        _c: &str,
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
        skill_ids: &[String],
    ) -> Result<Vec<awaken_session_contract::ResolvedSkillBinding>, RunError> {
        Ok(skill_ids
            .iter()
            .map(|skill_id| awaken_session_contract::ResolvedSkillBinding {
                skill_id: skill_id.clone(),
                version: 1,
                bundle_sha256: format!("sha256:{skill_id}"),
            })
            .collect())
    }
    async fn apply_session_inputs(
        &self,
        _thread: &str,
        _workspace_id: &str,
        inputs: &awaken_session_contract::ResolvedSessionResources,
    ) -> Result<(), RunError> {
        self.applied.lock().unwrap().push(inputs.clone());
        if self
            .fail_next_apply
            .swap(false, std::sync::atomic::Ordering::SeqCst)
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[async_trait::async_trait]
impl awaken_protocol_managed::McpAttachmentRealizer for AcceptingFake {
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
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn session_inherits_published_agent_integrations_and_echoes_the_effective_set() {
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
                "name": "public-docs",
                "url": "https://session-public.example.test"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
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
    assert_eq!(session["agent"]["skills"][0]["id"], "skill_release");
    assert_eq!(session["agent"]["multiagent"]["agents"][0], "researcher");
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
                "name": "docs-alias",
                "url": "HTTPS://MCP.EXAMPLE.TEST:443/"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(prepared.lock().unwrap().is_empty());
    assert!(repo.get("sesn_0").await.is_none());
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

    // They are addressable via list/get, uniformly with any later-attached ones.
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
                recall_policy: RecallPolicy {
                    enabled: true,
                    max_results: 2,
                },
                extraction_policy: ExtractionPolicy::default(),
                retention_policy: RetentionPolicy::default(),
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
            awaken_credential_vault::CredentialSourceId("missing".into())
        };
        let holder = awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
            .resource_holder;
        let vaults = awaken_protocol_managed::VaultState::new(secrets, credentials);
        let actual = vaults
            .credential_access_for_source(
                &source_id,
                "workspace-a",
                awaken_session_contract::repository_transport_credential_usage(),
                awaken_credential_contract::CredentialExecutionPolicy::exact(
                    holder.clone(),
                    awaken_credential_contract::ModelExposurePolicy::Forbidden,
                ),
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
            json!({
                "type": "file",
                "file_id": "file-rollback",
                "mount_path": "/rollback.txt"
            }),
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

#[derive(Clone, Copy)]
enum ResourceCasRule {
    NoConflict,
    PrepareConflictOnce,
    SettlementConflictOnce,
    SettlementConflictsExhausted,
    RollbackSettlementConflictOnce,
    ConcurrentResourceChange,
}

/// Resource commands use the repository root CAS as their only serializer.
/// The cases are generated from this cause graph:
///
/// unchanged Resource state + prepare CAS conflict -> reload/rebase before I/O;
/// durable Prepared + runtime effect + root-only conflict -> settle the same
/// Resource revision on the latest aggregate; a changed/exhausted fence leaves
/// durable pending work for recovery. Runtime failure follows the same settlement
/// path, but records rollback rather than Active.
///
/// | Rule | Runtime | Prepare CAS | Settlement CAS | Result | Runtime applies | Durable Resource |
/// |------|---------|-------------|----------------|--------|-----------------|------------------|
/// | C1 | success | apply | apply | success | 1 | Active |
/// | C2 | success | conflict once | apply | success | 1 | Active |
/// | C3 | success | apply | conflict once | success | 1 | Active |
/// | C4 | success | apply | conflict x3 | conflict | 1 | Prepared/recoverable |
/// | C5 | fail then rollback | apply | conflict once | runtime error | 2 | Failed/no pending |
/// | C6 | success | another Resource wins | - | conflict | 0 | other intent preserved |
#[tokio::test]
async fn resource_root_cas_cases_follow_the_decision_table_without_a_process_lock() {
    for (index, rule) in [
        ResourceCasRule::NoConflict,
        ResourceCasRule::PrepareConflictOnce,
        ResourceCasRule::SettlementConflictOnce,
        ResourceCasRule::SettlementConflictsExhausted,
        ResourceCasRule::RollbackSettlementConflictOnce,
        ResourceCasRule::ConcurrentResourceChange,
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
            ResourceCasRule::SettlementConflictOnce => repo.conflict_on_next(2),
            ResourceCasRule::SettlementConflictsExhausted => {
                repo.conflicts_on_next(&[2, 3, 4]);
            }
            ResourceCasRule::RollbackSettlementConflictOnce => {
                fail_next.store(true, std::sync::atomic::Ordering::SeqCst);
                repo.conflict_on_next(2);
            }
            ResourceCasRule::ConcurrentResourceChange => repo.resource_change_on_next(1),
        }

        let result = state
            .create_resource(
                &id,
                json!({
                    "type": "file",
                    "file_id": format!("file-cas-{index}"),
                    "mount_path": format!("/cas-{index}.txt")
                }),
            )
            .await;
        let durable = repo.get(&id).await.unwrap();
        let apply_count = applied.lock().unwrap().len();

        match rule {
            ResourceCasRule::NoConflict
            | ResourceCasRule::PrepareConflictOnce
            | ResourceCasRule::SettlementConflictOnce => {
                assert!(result.is_ok(), "C{}: {result:?}", index + 1);
                assert_eq!(apply_count, 1, "C{}", index + 1);
                assert!(durable.resources.pending.is_none(), "C{}", index + 1);
                assert_eq!(durable.resources.active.inputs.len(), 1, "C{}", index + 1);
            }
            ResourceCasRule::SettlementConflictsExhausted => {
                assert!(
                    matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
                    "C4: {result:?}"
                );
                assert_eq!(apply_count, 1, "C4");
                assert!(durable.resources.pending.is_some(), "C4");
                assert!(durable.resources.needs_reconciliation(), "C4");
            }
            ResourceCasRule::RollbackSettlementConflictOnce => {
                assert!(
                    format!("{result:?}").contains("injected activation failure"),
                    "C5"
                );
                assert_eq!(apply_count, 2, "C5");
                assert!(durable.resources.pending.is_none(), "C5");
                assert_eq!(
                    durable.resources.activations.last().unwrap().state,
                    awaken_session_contract::ActivationState::Failed,
                    "C5"
                );
            }
            ResourceCasRule::ConcurrentResourceChange => {
                assert!(
                    matches!(result, Err(awaken_protocol_managed::StateError::Conflict)),
                    "C6: {result:?}"
                );
                assert_eq!(apply_count, 0, "C6");
                assert!(durable.resources.pending.is_some(), "C6");
                assert_eq!(durable.resources.revision, 2, "C6");
            }
        }
    }
}

#[tokio::test]
async fn github_repository_attaches_to_a_live_session() {
    let (app, id) = app_with_session().await;

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
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resource["type"], "github_repository");
    assert!(resource["id"].is_string());

    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
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
async fn repository_raw_credentials_are_rejected_on_create_and_update() {
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

    let (_, session) = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"}))).await;
    let session_id = session["id"].as_str().unwrap();
    let (status, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources"),
        Some(json!({
            "type": "github_repository",
            "url": "https://github.com/awaken/example.git"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let resource_id = resource["id"].as_str().unwrap();
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
