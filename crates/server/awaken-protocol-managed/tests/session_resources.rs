//! Post-creation session-resource CRUD (`/v1/sessions/{id}/resources`) mirrors the
//! Managed Agents contract: `file` and `github_repository` attach to a live
//! session, but a `memory_store` binds at session-create time only — attaching one
//! to a running session fails closed with a 400 (`invalid_request_error`).

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_config_resolver::InMemoryResourceCatalog;
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
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
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

fn resource_catalog() -> std::sync::Arc<InMemoryResourceCatalog> {
    let catalog = std::sync::Arc::new(InMemoryResourceCatalog::new());
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
            mcp_servers: vec![awaken_protocol_managed::AgentMcpServerView {
                name: "docs".into(),
                url: "https://mcp.example.test".into(),
            }],
            skill_ids: vec!["skill_release".into()],
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
    let state = ManagedState::new(AcceptingFake::default())
        .with_config_source(std::sync::Arc::new(AgentWithIntegrations));
    let app = router(std::sync::Arc::new(state));
    let (status, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "integrated" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["agent"]["mcp_servers"][0]["name"], "docs");
    assert_eq!(session["agent"]["skills"][0]["id"], "skill_release");
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
async fn repository_token_is_sealed_before_the_effective_manifest() {
    let runtime = AcceptingFake::default();
    let prepared = runtime.prepared.clone();
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
    ));
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime)
            .with_vaults(vaults)
            .with_resource_catalog(resource_catalog()),
    ));

    let secret = "ghp_manifest_must_not_contain_this"; // awaken-allow: secret
    let (status, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [{
                "type": "github_repository",
                "url": "https://github.com/awaken/example.git",
                "authorization_token": secret
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let calls = prepared.lock().unwrap();
    let encoded = serde_json::to_string(&calls[0].resources).unwrap();
    assert!(!encoded.contains(secret));
    match &calls[0].resources.inputs[0].source {
        awaken_session_contract::ResolvedInputSource::Repository { config, .. } => {
            assert!(config.credential_binding.is_some());
        }
        other => panic!("expected repository input, got {other:?}"),
    }
}

#[tokio::test]
async fn terminal_session_retires_only_its_compatibility_repository_definition() {
    let catalog = resource_catalog();
    let repo = std::sync::Arc::new(awaken_session_store::InMemorySessionRepository::default());
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
            .state,
        ResourceState::Active
    );

    state.archive_session(&id).await.unwrap();
    assert_eq!(
        catalog
            .repository("default", repository_id.as_str())
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
    let repo = std::sync::Arc::new(awaken_session_store::InMemorySessionRepository::default());
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

#[tokio::test]
async fn github_repository_attaches_to_a_live_session() {
    let (app, id) = app_with_session().await;

    let (s, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "github_repository",
            "url": "https://github.com/owner/repo",
            "authorization_token": "ghp_x", // awaken-allow: secret
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
async fn live_repository_credentials_are_references_and_config_versions_are_frozen() {
    let runtime = AcceptingFake::default();
    let applied = runtime.applied.clone();
    let vaults = std::sync::Arc::new(awaken_protocol_managed::VaultState::new(
        std::sync::Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        std::sync::Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
    ));
    let app = router(std::sync::Arc::new(
        ManagedState::new(runtime)
            .with_vaults(vaults)
            .with_resource_catalog(resource_catalog()),
    ));
    let (_, session) = call(&app, "POST", "/v1/sessions", Some(json!({"agent": "a"}))).await;
    let session_id = session["id"].as_str().unwrap();
    let first_secret = "ghp_live_first_must_not_persist"; // awaken-allow: secret
    let (status, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources"),
        Some(json!({
            "type": "github_repository",
            "url": "https://github.com/owner/repo",
            "authorization_token": first_secret
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let resource_id = resource["id"].as_str().unwrap();

    let second_secret = "ghp_live_second_must_not_persist"; // awaken-allow: secret
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session_id}/resources/{resource_id}"),
        Some(json!({"authorization_token": second_secret})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let calls = applied.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let awaken_session_contract::ResolvedInputSource::Repository {
        config: first_config,
        ..
    } = &calls[0].inputs[0].source
    else {
        panic!("expected first repository input")
    };
    let awaken_session_contract::ResolvedInputSource::Repository {
        config: second_config,
        ..
    } = &calls[1].inputs[0].source
    else {
        panic!("expected second repository input")
    };
    assert_eq!(first_config.version, ConfigVersion::INITIAL);
    assert_eq!(second_config.version, ConfigVersion(2));
    assert_ne!(
        first_config.credential_binding,
        second_config.credential_binding
    );
    let encoded = serde_json::to_string(&*calls).unwrap();
    assert!(!encoded.contains(first_secret));
    assert!(!encoded.contains(second_secret));
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
