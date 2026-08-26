//! Cross-module E2E for the official Managed Deployment lifecycle.

mod support;

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
use awaken_protocol_managed::{ManagedDeploymentSessionLauncher, deployments_router};
use awaken_runtime_contract::snapshot::AgentId;
use awaken_runtime_contract::{
    AgentConfigRevisionRef, AgentPublicationVersion, AgentSnapshotFingerprint,
    AgentSnapshotMetadata, ExecutableAgentSnapshot, ModelBinding,
};
use awaken_scenario_host::{
    EchoModel, build_unmounted_host, build_unmounted_host_with_agent_publications,
};
use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use support::wait_for_session_events;

fn resource_registry() -> Arc<dyn awaken_resource_contract::ResourceRegistry> {
    awaken_resource_persistence::ephemeral()
        .expect("open the Deployment test Resource application")
        .authorities()
        .resource_registry()
}

struct DeploymentPriceProvider;

#[async_trait::async_trait]
impl awaken_session_contract::ManagedListPriceProvider for DeploymentPriceProvider {
    async fn resolve_snapshot(
        &self,
        request: awaken_session_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_contract::ManagedListPriceSnapshot,
        awaken_session_contract::ManagedListPriceError,
    > {
        let rate = awaken_session_contract::ManagedTokenListRates {
            input_micros_per_million: 10_000_000,
            output_micros_per_million: 10_000_000,
            cache_read_micros_per_million: 10_000_000,
            cache_creation_micros_per_million: 10_000_000,
        };
        Ok(awaken_session_contract::ManagedListPriceSnapshot {
            snapshot_id: "deployment-budget-test".into(),
            version: 1,
            effective_at_unix_ms: request.occurred_at_unix_ms,
            arithmetic_version: 1,
            model_rates: request
                .model_refs
                .into_iter()
                .map(|model| (model, rate))
                .collect(),
            runtime_rates: Default::default(),
            fingerprint: "deployment-budget-test-v1".into(),
        })
    }
}

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
async fn initial_event_failure_preserves_the_committed_deployment_session() {
    // FMECA/cause-effect graph: C1 a valid frozen Agent and user-then-system
    // initial Event plan commit one Session root; C2 the Runtime publication
    // source is unavailable when the lifecycle owner later executes that plan; C3 the same
    // DeploymentRun is retried after response loss. Effects: E1 launch returns
    // the committed Session, E2 C2 leaves that Session visible and retryable,
    // E3 C3 returns the same Session. K1 DeploymentRun records Session creation,
    // while the Session root is the sole Event/lifecycle authority; no follow-up
    // launch executor or compensating delete exists. Rules: D1 C1 -> E1;
    // D2 C1+C2 -> E2; D3 C1+C2+C3 -> E3.
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let host = build_unmounted_host(Arc::new(EchoModel), "claude-sonnet-5");
    let workspace_id = host.local_workspace().to_string();
    publish_assistant(catalog.clone(), &workspace_id, "claude-sonnet-5").await;
    let managed = awaken_coordinator::local_managed_state_with_agent_source(
        host,
        resource_registry(),
        catalog,
    );
    let launcher = ManagedDeploymentSessionLauncher::new(managed.clone());
    let request = DeploymentLaunch {
        deployment_id: "depl_failed_initial".into(),
        deployment_run_id: "drun_failed_initial".into(),
        workspace_id,
        agent: DeploymentAgent::new("assistant", 1),
        environment_id: "env_local".into(),
        metadata: Default::default(),
        initial_events: vec![
            DeploymentSeedEvent::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "initialize the frozen publication".into(),
                }],
            },
            DeploymentSeedEvent::SystemMessage {
                content: vec![ContentBlock::Text {
                    text: "must use the frozen publication".into(),
                }],
            },
        ],
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
        matches!(
            first,
            DeploymentLaunchOutcome::Created {
                session_id: ref created
            } if created == &session_id
        ),
        "D1/E1: {first:?}"
    );

    let error = Box::pin(
        managed
            .session_application()
            .drive_session_event_batches(&session_id, None),
    )
    .await
    .expect_err("D2 lifecycle execution must observe the unavailable publication");
    assert!(
        error.to_string().contains("publication"),
        "D2 expected publication failure: {error}"
    );
    assert_eq!(
        managed.get_session(&session_id).unwrap().id,
        session_id,
        "D2/E2"
    );
    assert!(
        matches!(
            launcher.launch(request).await,
            DeploymentLaunchOutcome::Created {
                session_id: replayed
            } if replayed == session_id
        ),
        "D3/E3"
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

#[tokio::test]
async fn deployment_run_identity_replays_one_session_and_rejects_payload_reuse() {
    // Cause/effect decision table:
    // R1 new stable DeploymentRun + valid launch -> create one deterministic Session;
    // R2 same run + byte-equivalent launch -> return that Session, enqueue no second
    // initial Event batch; R3 same run + changed payload -> fail closed and preserve
    // the R1 Session. This covers the response-loss retry before an HTTP adapter exists.
    // Constraints/invariants: DeploymentRun identity and launch fingerprint are
    // the sole replay fence; the receipt-aware observer never drives execution.
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let host = build_unmounted_host_with_agent_publications(
        Arc::new(EchoModel),
        "claude-sonnet-5",
        catalog.clone(),
    );
    let workspace_id = host.local_workspace().to_string();
    publish_assistant(catalog.clone(), &workspace_id, "claude-sonnet-5").await;
    let resources = resource_registry();
    let managed = awaken_coordinator::local_managed_state_with_agent_source(
        host.clone(),
        resources.clone(),
        catalog,
    );
    // The public Session scope guard must observe the same trusted Workspace as
    // the internal Deployment launcher. Omitting this edge stamp proves only a
    // cross-tenant 404, not the launch or initial-Event contract.
    let app = awaken_coordinator::mount_with_managed_and_resource_registry(
        host,
        managed.clone(),
        resources,
    )
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
    let events = wait_for_session_events(
        &app,
        &first_id,
        None,
        "the deployment's one initial user.message",
        |events| events.iter().any(|event| event["type"] == "user.message"),
    )
    .await;
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
    // Official-docs cause/effect graph and FMECA: C1 valid Agent/environment
    // plus one authoritative Managed list-price snapshot;
    // C2 Deployment carries its wider official initial-event union, including a
    // user.message immediately followed by the final system.message; C3 manual
    // trigger; C4 due cron;
    // C5 pause; C6 create/update/null Deployment budget across successive
    // manual runs. Effects: E1 active Deployment; E2 exactly one ordinary Session;
    // E3 both Events commit in the Session root and execute through its sole
    // lifecycle state machine; E4 a schedule-triggered Session; E5 no launch
    // while paused; E6 every new Session freezes the current budget independently,
    // while prior Sessions keep their original cap and null removes it only for
    // later runs. If the Deployment batch bypasses the shared validator, or is
    // revalidated as the narrower public Session-create input, authoring and
    // execution can diverge (terminal run, severity high). The mitigation is one
    // Deployment validator followed by one atomic Session creation command.
    // Constraints/invariants: at most one system.message is admitted, it is final
    // and immediately follows its user.message, and Session/Run committed facts
    // remain the only execution authority.
    // Decision table:
    // D1 C1+C2 -> E1;
    // D2 manual run -> DeploymentRun XOR terminal branch with a Session id;
    // D3 C1+C2+C3 -> E2+E3; D4 C1+C2+C4 -> E4;
    // D5 C5 -> E5, then unpause advances the future-only cursor;
    // D6 C1+C6 -> E6.
    let catalog = Arc::new(ExecutableAgentCatalog::new());
    let host = build_unmounted_host_with_agent_publications(
        Arc::new(EchoModel),
        "claude-sonnet-5",
        catalog.clone(),
    );
    let workspace_id = host.local_workspace().to_string();
    publish_assistant(catalog.clone(), &workspace_id, "claude-sonnet-5").await;
    let resources = resource_registry();
    let managed = awaken_coordinator::local_managed_state_with_agent_source_and_list_prices(
        host.clone(),
        resources.clone(),
        catalog,
        Arc::new(DeploymentPriceProvider),
    );
    let deployments = Arc::new(DeploymentApplication::new());
    deployments.bind_launcher(Arc::new(ManagedDeploymentSessionLauncher::new(
        managed.clone(),
    )));
    let deployment_api = deployments_router(deployments.clone())
        .layer(axum::Extension(WorkspaceScope(workspace_id.clone())));
    let app = awaken_coordinator::mount_with_managed_and_resource_registry(
        host,
        managed.clone(),
        resources,
    )
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
                    "type":"user.message",
                    "content":[{"type":"text","text":"deployment seed event"}]
                },
                {
                    "type":"system.message",
                    "content":[{"type":"text","text":"deployment system directive"}]
                }
            ],
            "schedule":{"type":"cron","expression":"*/15 * * * *","timezone":"UTC"},
            "budget":{"type":"limit","max_list_cost":{"amount":"2000","currency":"USD"}}
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
    let events = wait_for_session_events(
        &app,
        session_id,
        None,
        "the deployment system directive and seed Event",
        |events| {
            let rendered = serde_json::to_string(events).expect("render Event projection");
            rendered.contains("deployment system directive")
                && rendered.contains("deployment seed event")
        },
    )
    .await;
    let initial_types = events["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|event| event["type"].as_str())
        .filter(|kind| matches!(*kind, "system.message" | "user.message"))
        .collect::<Vec<_>>();
    assert_eq!(initial_types, vec!["user.message", "system.message"], "D3");

    let (_, first_session) = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(
        first_session["budget"]["max_list_cost"]["amount"], "2000",
        "D6/E6 create budget copied"
    );
    let (status, updated) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}"),
        Some(json!({
            "budget":{"type":"limit","max_list_cost":{"amount":"500","currency":"USD"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D6 budget update: {updated}");
    let (_, second_run) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/run"),
        None,
    )
    .await;
    let second_session_id = second_run["session_id"]
        .as_str()
        .expect("D6 second Session");
    let (_, second_session) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{second_session_id}"),
        None,
    )
    .await;
    assert_ne!(second_session_id, session_id, "D6 independent Sessions");
    assert_eq!(
        second_session["budget"]["max_list_cost"]["amount"], "500",
        "D6/E6"
    );
    let (_, first_after_update) =
        call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(
        first_after_update["budget"]["max_list_cost"]["amount"], "2000",
        "D6/E6 old Session cap is immutable"
    );
    let (status, cleared) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}"),
        Some(json!({"budget": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D6 clear budget: {cleared}");
    assert!(cleared["budget"].is_null(), "D6 clear projection");
    let (_, third_run) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/run"),
        None,
    )
    .await;
    let third_session_id = third_run["session_id"].as_str().expect("D6 third Session");
    let (_, third_session) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{third_session_id}"),
        None,
    )
    .await;
    assert!(
        third_session["budget"].is_null(),
        "D6/E6 null affects future only"
    );

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
