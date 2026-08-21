//! Cross-module E2E for the official Managed Deployment lifecycle.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_deployment_application::{
    DeploymentAgent, DeploymentApplication, DeploymentLaunch, DeploymentLaunchOutcome,
    DeploymentSeedEvent, DeploymentSessionLauncher,
};
use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
use awaken_executable_agent_contract::{
    ExecutableAgentRegistrar, ExecutableAgentRegistration, ExecutableAgentSessionProfile,
};
use awaken_protocol_managed::{ManagedDeploymentSessionLauncher, ManagedState, deployments_router};
use awaken_runtime_contract::snapshot::AgentId;
use awaken_runtime_contract::{
    AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
    AgentSnapshotMetadata, ExecutableAgentSnapshot, ModelBinding,
};
use awaken_runtime_host::ManagedHost;
use awaken_scenario_host::{
    EchoModel, build_router_and_host, build_router_and_host_with_agent_publications,
};
use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn publish_assistant(
    catalog: Arc<ExecutableAgentCatalog>,
    workspace_id: &str,
    model_ref: &str,
) {
    let fingerprint = "scenario-assistant-v1";
    let mut snapshot = ExecutableAgentSnapshot::builder("assistant")
        .model(ModelBinding::new("scenario", model_ref, "default"))
        .fingerprint(fingerprint)
        .build();
    snapshot.metadata = AgentSnapshotMetadata {
        source: AgentConfigRevisionRef {
            agent_id: AgentId("assistant".into()),
            revision: 1,
        },
        publication_version: AgentPublicationVersion("assistant-v1".into()),
        resolution: Default::default(),
        fingerprint: AgentSnapshotFingerprint(fingerprint.into()),
    };
    LocalExecutableAgentRegistrar::new(catalog)
        .register(ExecutableAgentRegistration {
            workspace_id: workspace_id.into(),
            agent_id: "assistant".into(),
            source_revision: 1,
            snapshot,
            session_profile: ExecutableAgentSessionProfile {
                source_revision: 1,
                model: Some(model_ref.into()),
                execution_model_ref: Some(model_ref.into()),
                backend_ref: "default".into(),
                ..Default::default()
            },
        })
        .await
        .expect("register the Deployment's exact Agent publication");
}

#[tokio::test]
async fn failed_initial_events_are_compensated_before_deployment_acknowledgement() {
    // FMECA/cause-effect graph: C1 a valid frozen Agent creates a Session; C2
    // its Runtime publication source is unavailable at the first system Event;
    // C3 the same DeploymentRun is retried after response loss. Required
    // effects: E1 the first launch is failed, E2 no partially initialized
    // Session remains visible, E3 retry can never reinterpret that Session as a
    // successful replay. Rules:
    // D1 C1+C2 -> E1+E2; D2 C1+C2+C3 -> E3. This mutation-kills both the former
    // detached executor and a synchronous implementation without compensation.
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let (_, host) = build_router_and_host(Arc::new(EchoModel), "claude-sonnet-5");
    let workspace_id = host.local_workspace().to_string();
    publish_assistant(catalog.clone(), &workspace_id, "claude-sonnet-5").await;
    let managed = Arc::new(ManagedState::new(ManagedHost::new(host)).with_config_source(catalog));
    let launcher = ManagedDeploymentSessionLauncher::new(managed.clone());
    let request = DeploymentLaunch {
        deployment_id: "depl_failed_initial".into(),
        deployment_run_id: "drun_failed_initial".into(),
        workspace_id,
        agent: DeploymentAgent::new("assistant", 1),
        environment_id: "env_local".into(),
        metadata: Default::default(),
        initial_events: vec![DeploymentSeedEvent::SystemMessage {
            content: vec![ContentBlock::Text {
                text: "must initialize the frozen publication".into(),
            }],
        }],
        resources: Vec::new(),
        vault_ids: Vec::new(),
        budget_max_list_cost_minor: None,
    };
    let session_id = format!(
        "sesn_{}",
        awaken_session_contract::stable_fingerprint(&(
            "deployment-run",
            request.deployment_run_id.as_str()
        ))
    );

    let first = launcher.launch(request.clone()).await;
    assert!(
        matches!(first, DeploymentLaunchOutcome::Failed { .. }),
        "D1/E1: {first:?}"
    );
    assert!(
        matches!(
            managed.get_session(&session_id),
            Err(awaken_protocol_managed::StateError::NotFound)
        ),
        "D1/E2"
    );
    assert!(
        !matches!(
            launcher.launch(request).await,
            DeploymentLaunchOutcome::Created { .. }
        ),
        "D2/E3"
    );
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("anthropic-beta", "managed-agents-2026-04-01");
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn wait_for_session_events(
    app: &Router,
    session_id: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Option<(StatusCode, Value)> {
    // Cause/effect decision table: W1 the detached ordinary Event command commits
    // before the monotonic deadline -> return its public projection; W2 the
    // projection is not ready yet -> retry without driving product state; W3 the
    // deadline expires -> return no evidence and let the calling rule fail with
    // its domain context. A fixed iteration/millisecond loop duplicated in both
    // tests was not a valid deployment realization deadline.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let response = call(
            app,
            "GET",
            &format!("/v1/sessions/{session_id}/events"),
            None,
        )
        .await;
        if response.0 == StatusCode::OK && predicate(&response.1) {
            return Some(response);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn deployment_run_identity_replays_one_session_and_rejects_payload_reuse() {
    // Cause/effect decision table:
    // R1 new stable DeploymentRun + valid launch -> create one deterministic Session;
    // R2 same run + byte-equivalent launch -> return that Session, enqueue no second
    // initial Event batch; R3 same run + changed payload -> fail closed and preserve
    // the R1 Session. This covers the response-loss retry before an HTTP adapter exists.
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let (_, host) = build_router_and_host_with_agent_publications(
        Arc::new(EchoModel),
        "claude-sonnet-5",
        catalog.clone(),
    );
    let workspace_id = host.local_workspace().to_string();
    publish_assistant(catalog.clone(), &workspace_id, "claude-sonnet-5").await;
    let managed =
        Arc::new(ManagedState::new(ManagedHost::new(host.clone())).with_config_source(catalog));
    // The public Session scope guard must observe the same trusted Workspace as
    // the internal Deployment launcher. Omitting this edge stamp proves only a
    // cross-tenant 404, not the launch or initial-Event contract.
    let app = awaken_coordinator::mount_with_managed(host, managed.clone())
        .layer(axum::Extension(WorkspaceScope(workspace_id.clone())));
    let launcher = ManagedDeploymentSessionLauncher::new(managed.clone());
    let request = DeploymentLaunch {
        deployment_id: "depl_retry".into(),
        deployment_run_id: "drun_retry".into(),
        workspace_id,
        agent: DeploymentAgent::new("assistant", 1),
        environment_id: "env_local".into(),
        metadata: Default::default(),
        initial_events: vec![DeploymentSeedEvent::UserMessage {
            content: vec![ContentBlock::text("one launch only")],
        }],
        resources: Vec::new(),
        vault_ids: Vec::new(),
        budget_max_list_cost_minor: None,
    };

    let first = launcher.launch(request.clone()).await;
    let second = launcher.launch(request.clone()).await;
    let (first_id, second_id) = match (first, second) {
        (
            DeploymentLaunchOutcome::Created { session_id: first },
            DeploymentLaunchOutcome::Created { session_id: second },
        ) => (first, second),
        outcomes => panic!("R1/R2 unexpected outcomes: {outcomes:?}"),
    };
    assert_eq!(first_id, second_id, "R1/R2");
    let events = wait_for_session_events(&app, &first_id, |events| {
        events["data"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|event| event["type"] == "user.message")
    })
    .await
    .expect("R2 initial Event batch commits before the deployment deadline")
    .1;
    let user_messages = events["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["type"] == "user.message")
        .count();
    assert_eq!(user_messages, 1, "R2 initial Event batch");

    let mut changed = request;
    changed.metadata.insert("changed".into(), "true".into());
    assert!(
        matches!(
            launcher.launch(changed).await,
            DeploymentLaunchOutcome::Failed { .. }
        ),
        "R3"
    );
    assert_eq!(managed.get_session(&first_id).unwrap().id, first_id, "R3");
}

#[tokio::test]
async fn deployment_manual_and_cron_runs_create_ordinary_sessions_with_initial_events() {
    // Official-docs cause/effect graph and FMECA: C1 valid Agent/environment;
    // C2 Deployment carries its wider official initial-event union, including a
    // system.message followed by user.message; C3 manual trigger; C4 due cron;
    // C5 pause. Effects: E1 active Deployment; E2 exactly one ordinary Session;
    // E3 both Events commit through the canonical Session event command; E4 a
    // schedule-triggered Session; E5 no launch while paused. If the Deployment
    // batch is revalidated as public Session-create or mid-conversation input,
    // system.message is accepted at authoring but rejected at execution
    // (terminal run, severity high). The mitigation is to admit the empty
    // Session once and deliver the already Deployment-validated batch through
    // the sole source-aware event command.
    // Decision table:
    // D1 C1+C2 -> E1;
    // D2 manual run -> DeploymentRun XOR terminal branch with a Session id;
    // D3 C1+C2+C3 -> E2+E3; D4 C1+C2+C4 -> E4;
    // D5 C5 -> E5, then unpause advances the future-only cursor.
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let (_, host) = build_router_and_host_with_agent_publications(
        Arc::new(EchoModel),
        "claude-sonnet-5",
        catalog.clone(),
    );
    let workspace_id = host.local_workspace().to_string();
    publish_assistant(catalog.clone(), &workspace_id, "claude-sonnet-5").await;
    let managed =
        Arc::new(ManagedState::new(ManagedHost::new(host.clone())).with_config_source(catalog));
    let deployments = Arc::new(DeploymentApplication::new());
    deployments.bind_launcher(Arc::new(ManagedDeploymentSessionLauncher::new(
        managed.clone(),
    )));
    let deployment_api = deployments_router(deployments.clone())
        .layer(axum::Extension(WorkspaceScope(workspace_id.clone())));
    let app = awaken_coordinator::mount_with_managed(host, managed.clone())
        .merge(deployment_api)
        .layer(axum::Extension(WorkspaceScope(workspace_id)));

    let (status, deployment) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent":"assistant",
            "environment_id":"env_local",
            "name":"Dream maintenance",
            "initial_events":[
                {
                    "type":"system.message",
                    "content":[{"type":"text","text":"deployment system directive"}]
                },
                {
                    "type":"user.message",
                    "content":[{"type":"text","text":"deployment seed event"}]
                }
            ],
            "schedule":{"type":"cron","expression":"*/15 * * * *","timezone":"UTC"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D1: {deployment}");
    let deployment_id = deployment["id"].as_str().unwrap();

    let (status, manual) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/run"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D2: {manual}");
    assert!(manual["error"].is_null(), "D2 XOR: {manual}");
    let session_id = manual["session_id"].as_str().unwrap();
    let observed = wait_for_session_events(&app, session_id, |events| {
        let rendered = events.to_string();
        rendered.contains("deployment system directive")
            && rendered.contains("deployment seed event")
    })
    .await;
    let (status, events) = match observed {
        Some(observed) => observed,
        None => panic!(
            "D3 initial Event did not commit; direct session={:?}, owner={:?}",
            managed.get_session(session_id),
            managed.resolve_owner(session_id).await
        ),
    };
    assert_eq!(status, StatusCode::OK, "D3: {events}");
    let initial_types = events["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|event| event["type"].as_str())
        .filter(|kind| matches!(*kind, "system.message" | "user.message"))
        .collect::<Vec<_>>();
    assert_eq!(initial_types, vec!["system.message", "user.message"], "D3");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let scheduled = deployments
        .tick_and_launch(now + 20 * 60_000)
        .await
        .unwrap();
    assert!(!scheduled.is_empty(), "D4");
    assert!(
        scheduled.iter().all(|run| {
            run.record.error.is_none()
                && run.record.session_id.is_some()
                && serde_json::to_value(&run.record.trigger).unwrap()["type"] == "schedule"
        }),
        "D4: {scheduled:?}"
    );

    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D5");
    assert!(
        deployments
            .tick_and_launch(now + 40 * 60_000)
            .await
            .unwrap()
            .is_empty(),
        "D5"
    );
}
