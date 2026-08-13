//! HTTP admission/projection for the config authoring plane.
//!
//! Domain mutation and publication remain owned by [`ConfigPlane`]; this module
//! only binds request scope, maps wire JSON, and translates outcomes to HTTP.

use awaken_agent_config::{ConfigWrite, DEFAULT_SCOPE};
use awaken_executable_agent_contract::ExecutableAgentRegistrationError;
use awaken_tenancy::{ExecutionWorkspace, ScopeId};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{Value, json};

use crate::config_plane::ConfigPlane;
use crate::managed_agent::{agent_config_from_managed, managed_from_agent_config};
use crate::publication::{PublishError, ValidationIssue};
use crate::tool_catalog::RESERVED_ADMIN_SCOPE;

/// The config data-plane router: `/v1/config/agents/:id` (author) plus
/// `/validate` and `/publish` (lifecycle).
pub fn config_router(plane: ConfigPlane) -> Router {
    Router::new()
        .route("/v1/config/agents", get(list_configs))
        .route(
            "/v1/config/publications/{fingerprint}",
            get(get_publication),
        )
        .route(
            "/v1/config/publications/{fingerprint}/export",
            get(export_publication),
        )
        .route("/v1/config/agents/{id}/validate", post(validate))
        .route("/v1/config/agents/{id}/publish", post(publish))
        .route("/v1/config/agents/{id}", get(get_config).put(put_config))
        .route(
            "/v1/config/agent-previews/{preview_id}",
            post(create_preview).delete(delete_preview),
        )
        .with_state(plane)
}

fn valid_preview_id(id: &str) -> bool {
    id.strip_prefix("preview-").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix.len() <= 96
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn problem_details(code: &str, title: &str, detail: impl Into<String>) -> Json<Value> {
    let detail = detail.into();
    Json(json!({
        "type": format!("https://awaken.dev/problems/{code}"),
        "code": code,
        "title": title,
        "detail": detail,
    }))
}

#[cfg(test)]
#[test]
fn preview_conflict_preserves_the_structured_root_cause() {
    // Cause/effect rule: an unresolvable preview (model, plugin, credential, or
    // resource cause) returns one RFC-9457-shaped problem whose stable code and
    // exact detail let the Console explain the 409 instead of collapsing it to
    // the HTTP reason phrase. Field-level cause classification stays with the
    // publication resolver that authored the detail.
    let Json(problem) = problem_details(
        "agent_preview_unresolvable",
        "Agent preview cannot be resolved",
        "plugin_config.web_search: config is required",
    );
    assert_eq!(problem["code"], "agent_preview_unresolvable");
    assert_eq!(
        problem["detail"],
        "plugin_config.web_search: config is required"
    );
}

async fn create_preview(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    execution: Option<Extension<ExecutionWorkspace>>,
    Path(preview_id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if !valid_preview_id(&preview_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": "preview id must start with `preview-` and contain only letters, numbers, or dashes" }),
            ),
        );
    }
    let Some(config_body) = body.get("config") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "preview request requires config" })),
        );
    };
    let config = match agent_config_from_managed(preview_id.clone(), config_body) {
        Ok(config) => config,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    };
    let resources = match body
        .get("resources")
        .cloned()
        .map(serde_json::from_value::<awaken_config_resolver::AgentInputConfig>)
    {
        Some(Ok(resources)) => resources,
        Some(Err(error)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid preview resources: {error}") })),
            );
        }
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "preview request requires resources" })),
            );
        }
    };
    let scope = request_scope(scope);
    let Some(workspace) = publication_workspace(&scope, execution.as_ref()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": PublishError::ExecutionWorkspaceRequired.to_string() })),
        );
    };
    match plane
        .preview_for_execution_workspace(&scope, workspace, &preview_id, &config, resources)
        .await
    {
        Ok(snapshot) => (
            StatusCode::OK,
            Json(json!({
                "preview_id": preview_id,
                "fingerprint": snapshot.fingerprint.0,
                "installed": true,
                "persistent": false,
            })),
        ),
        Err(error @ PublishError::Unresolvable(_)) => (
            StatusCode::CONFLICT,
            problem_details(
                "agent_preview_unresolvable",
                "Agent preview cannot be resolved",
                error.to_string(),
            ),
        ),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error.to_string() })),
        ),
    }
}

async fn delete_preview(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    execution: Option<Extension<ExecutionWorkspace>>,
    Path(preview_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if !valid_preview_id(&preview_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid preview id" })),
        );
    }
    let scope = request_scope(scope);
    let Some(workspace) = publication_workspace(&scope, execution.as_ref()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": PublishError::ExecutionWorkspaceRequired.to_string() })),
        );
    };
    match plane.remove_preview(workspace, &preview_id).await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "preview_id": preview_id, "installed": false })),
        ),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))),
    }
}

pub(crate) fn request_scope(ext: Option<Extension<awaken_tenancy::WorkspaceScope>>) -> ScopeId {
    ext.map(|Extension(w)| ScopeId::from(w.0))
        .unwrap_or_else(|| ScopeId::from(DEFAULT_SCOPE))
}

/// Absence is scoped, so another Workspace's fingerprint is never disclosed.
async fn get_publication(
    State(plane): State<ConfigPlane>,
    Path(fingerprint): Path<String>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
) -> (StatusCode, Json<Value>) {
    if fingerprint.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "publication fingerprint is required" })),
        );
    }
    match plane.publication(&request_scope(scope), &fingerprint).await {
        Ok(Some(publication)) => (
            StatusCode::OK,
            Json(serde_json::to_value(publication).expect("StoredPublication serializes")),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "publication not found" })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

/// Export the canonical executable snapshot itself. The embedded SDK accepts no
/// alternate bundle/spec and rejects publications containing ACP/A2A branches.
async fn export_publication(
    State(plane): State<ConfigPlane>,
    Path(fingerprint): Path<String>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
) -> (StatusCode, Json<Value>) {
    if fingerprint.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "publication fingerprint is required" })),
        );
    }
    match plane.publication(&request_scope(scope), &fingerprint).await {
        Ok(Some(publication)) => match publication.snapshot.validate_embedded_native() {
            Ok(()) => (
                StatusCode::OK,
                Json(
                    serde_json::to_value(publication.snapshot)
                        .expect("ExecutableAgentSnapshot serializes"),
                ),
            ),
            Err(error) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": error })),
            ),
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "publication not found" })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

async fn list_configs(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    execution: Option<Extension<ExecutionWorkspace>>,
) -> (StatusCode, Json<Value>) {
    let scope = request_scope(scope);
    let execution = publication_workspace(&scope, execution.as_ref());
    match plane.list(&scope).await {
        Ok(configs) => {
            let _execution_workspace = execution.unwrap_or(scope.as_str());
            let mut data = Vec::with_capacity(configs.len());
            for config in configs {
                let versioned = match plane.get_versioned(&scope, &config.id).await {
                    Ok(Some(versioned)) => versioned,
                    Ok(None) => continue,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": error })),
                        );
                    }
                };
                let published = match plane
                    .has_published_revision(&scope, &config.id, versioned.revision)
                    .await
                {
                    Ok(published) => published,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": error })),
                        );
                    }
                };
                data.push(managed_from_agent_config(&config, published));
            }
            (StatusCode::OK, Json(json!({ "data": data })))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

pub(crate) async fn get_config(
    State(plane): State<ConfigPlane>,
    Path(id): Path<String>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    execution: Option<Extension<ExecutionWorkspace>>,
) -> (StatusCode, Json<Value>) {
    let scope = request_scope(scope);
    match plane.get_versioned(&scope, &id).await {
        Ok(Some(versioned)) => {
            let _execution_workspace = publication_workspace(&scope, execution.as_ref());
            let published = match plane
                .has_published_revision(&scope, &id, versioned.revision)
                .await
            {
                Ok(published) => published,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": error })),
                    );
                }
            };
            let mut body = managed_from_agent_config(&versioned.config, published);
            body.as_object_mut()
                .expect("managed config is an object")
                .insert("generation".to_string(), json!(versioned.revision));
            (StatusCode::OK, Json(body))
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no config stored for agent `{id}`") })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

pub(crate) async fn validate(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    execution: Option<Extension<ExecutionWorkspace>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let config = match agent_config_from_managed(id, &body) {
        Ok(config) => config,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "valid": false,
                    "issues": [{ "path": "", "message": error, "severity": "error" }],
                })),
            );
        }
    };
    let scope = request_scope(scope);
    let result = match publication_workspace(&scope, execution.as_ref()) {
        Some(workspace) => {
            plane
                .validate_for_execution_workspace(&scope, workspace, &config)
                .await
        }
        None => Err(ValidationIssue {
            path: "model".into(),
            message: PublishError::ExecutionWorkspaceRequired.to_string(),
        }),
    };
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({ "valid": true, "issues": [] }))),
        Err(issue) => (
            StatusCode::OK,
            Json(json!({
                "valid": false,
                "issues": [{ "path": issue.path, "message": issue.message, "severity": "error" }],
            })),
        ),
    }
}

pub(crate) async fn put_config(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let config = match agent_config_from_managed(id.clone(), &body) {
        Ok(config) => config,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    };
    let scope = request_scope(scope);
    if let Some(expected) = body.get("generation").and_then(Value::as_u64) {
        return match plane.put_if_revision(&scope, &config, expected).await {
            Ok(ConfigWrite::Applied { revision }) => (
                StatusCode::OK,
                Json(json!({ "id": id, "generation": revision })),
            ),
            Ok(ConfigWrite::Conflict { current_revision }) => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "config generation conflict",
                    "current_revision": current_revision,
                })),
            ),
            Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
        };
    }
    match plane.put(&scope, &config).await {
        Ok(()) => match plane.get_versioned(&scope, &id).await {
            Ok(Some(current)) => (
                StatusCode::OK,
                Json(json!({ "id": id, "generation": current.revision })),
            ),
            _ => (StatusCode::OK, Json(json!({ "id": id }))),
        },
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    }
}

pub(crate) async fn publish(
    State(plane): State<ConfigPlane>,
    scope: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    execution: Option<Extension<ExecutionWorkspace>>,
    Path(id): Path<String>,
    request: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    let scope = request_scope(scope);
    let request = match request
        .map(|Json(request)| PublishRequest::try_from(request))
        .transpose()
    {
        Ok(request) => request.unwrap_or_default(),
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    };
    let result = match publication_workspace(&scope, execution.as_ref()) {
        Some(workspace) => {
            plane
                .publish_for_execution_workspace_at_revisions(
                    &scope,
                    workspace,
                    &id,
                    request.source_revision,
                    request.resource_revision,
                )
                .await
        }
        None => Err(PublishError::ExecutionWorkspaceRequired),
    };
    match result {
        Ok(publication) => (
            StatusCode::OK,
            Json(json!({
                "publication_id": publication.publication_id,
                "fingerprint": publication.fingerprint,
                "agent_id": publication.agent_id,
                "installed": true,
            })),
        ),
        Err(error) => publish_error_response(error),
    }
}

fn publish_error_response(error: PublishError) -> (StatusCode, Json<Value>) {
    let (status, code, title) = match &error {
        PublishError::Unresolvable(_) => (
            StatusCode::CONFLICT,
            "agent_publication_unresolvable",
            "Agent publication cannot be resolved",
        ),
        PublishError::StaleRevision(_) => (
            StatusCode::CONFLICT,
            "agent_source_revision_conflict",
            "Agent source revision changed",
        ),
        PublishError::StaleResourceRevision(_) => (
            StatusCode::CONFLICT,
            "agent_resource_revision_conflict",
            "Agent resource revision changed",
        ),
        PublishError::Registration(
            ExecutableAgentRegistrationError::Unavailable(_)
            | ExecutableAgentRegistrationError::Storage(_),
        ) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_registration_unavailable",
            "Executable Agent registration is unavailable",
        ),
        PublishError::Registration(
            ExecutableAgentRegistrationError::Invalid(_)
            | ExecutableAgentRegistrationError::Conflict(_),
        ) => (
            StatusCode::CONFLICT,
            "agent_registration_conflict",
            "Executable Agent registration conflicts with durable state",
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            "agent_publication_invalid",
            "Agent publication request is invalid",
        ),
    };
    (status, problem_details(code, title, error.to_string()))
}

#[cfg(test)]
#[test]
fn publication_failures_keep_distinct_operator_actions() {
    // Cause/effect decision table (FMECA generic 409 S7/O8/D8=448):
    // R1 stale author source -> refresh source; R2 stale Resource -> refresh
    // bindings; R3 unresolvable dependency -> repair the named dependency;
    // R4 registration storage unavailable -> 503 retry; R5 identity conflict ->
    // 409 inspect/quarantine. Every effect is RFC-9457-shaped and retains the
    // exact resolver/registrar detail for correlation with server logs.
    let cases = [
        (
            PublishError::StaleRevision(Some(7)),
            StatusCode::CONFLICT,
            "agent_source_revision_conflict",
        ),
        (
            PublishError::StaleResourceRevision(4),
            StatusCode::CONFLICT,
            "agent_resource_revision_conflict",
        ),
        (
            PublishError::Unresolvable("plugin config missing".into()),
            StatusCode::CONFLICT,
            "agent_publication_unresolvable",
        ),
        (
            PublishError::Registration(ExecutableAgentRegistrationError::Storage(
                "database timeout".into(),
            )),
            StatusCode::SERVICE_UNAVAILABLE,
            "agent_registration_unavailable",
        ),
        (
            PublishError::Registration(ExecutableAgentRegistrationError::Conflict(
                "fingerprint mismatch".into(),
            )),
            StatusCode::CONFLICT,
            "agent_registration_conflict",
        ),
    ];
    for (error, expected_status, expected_code) in cases {
        let (status, Json(problem)) = publish_error_response(error);
        assert_eq!(status, expected_status, "{expected_code}");
        assert_eq!(problem["code"], expected_code, "{expected_code}");
        assert!(
            problem["detail"]
                .as_str()
                .is_some_and(|detail| !detail.is_empty())
        );
    }
}

#[derive(Default)]
pub(crate) struct PublishRequest {
    source_revision: Option<u64>,
    resource_revision: Option<i64>,
}

impl TryFrom<Value> for PublishRequest {
    type Error = String;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        let source_revision =
            serde_json::from_value(value.get("source_revision").cloned().unwrap_or(Value::Null))
                .map_err(|error| format!("invalid source_revision: {error}"))?;
        let resource_revision = serde_json::from_value(
            value
                .get("resource_revision")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map_err(|error| format!("invalid resource_revision: {error}"))?;
        Ok(Self {
            source_revision,
            resource_revision,
        })
    }
}

fn publication_workspace<'a>(
    scope: &'a ScopeId,
    execution: Option<&'a Extension<ExecutionWorkspace>>,
) -> Option<&'a str> {
    (scope.as_str() != RESERVED_ADMIN_SCOPE)
        .then(|| scope.as_str())
        .or_else(|| execution.map(|Extension(workspace)| workspace.0.as_str()))
}

#[cfg(test)]
mod publication_projection_tests {
    use std::sync::Arc;

    use awaken_config_store::SqliteConfigStore;

    use super::*;
    use crate::config_service::resource_prompt_tests::{
        agent_config, failing_scoped_plane, test_service,
    };

    // Causal decision table: present in trusted scope => exact value;
    // absent/cross-scope => 404; repository failure => 500.
    #[tokio::test]
    async fn exact_scoped_and_fail_closed() {
        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let owner = ScopeId::from("workspace-owner");
        plane.put(&owner, &agent_config("agent-a")).await.unwrap();
        let published = plane.publish(&owner, "agent-a").await.unwrap();

        let (status, body) = get_publication(
            State(plane.clone()),
            Path(published.fingerprint.clone()),
            Some(Extension(awaken_tenancy::WorkspaceScope(
                owner.as_str().to_owned(),
            ))),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["fingerprint"], published.fingerprint);

        let (status, _) = get_publication(
            State(plane),
            Path(published.fingerprint),
            Some(Extension(awaken_tenancy::WorkspaceScope("intruder".into()))),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            get_publication(State(failing_scoped_plane()), Path("fp".into()), None)
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// Cause/effect decision table for online export:
    /// R1 owned native publication => exact canonical snapshot; R2 cross-scope
    /// fingerprint => 404; R3 repository fault => 500; R4 blank fingerprint =>
    /// 400; R5 a non-native publication => 422 before any snapshot is returned.
    #[tokio::test]
    async fn export_is_scoped_and_returns_the_existing_snapshot_contract() {
        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let owner = ScopeId::from("workspace-owner");
        plane.put(&owner, &agent_config("agent-a")).await.unwrap();
        let published = plane.publish(&owner, "agent-a").await.unwrap();

        let (status, body) = export_publication(
            State(plane.clone()),
            Path(published.fingerprint.clone()),
            Some(Extension(awaken_tenancy::WorkspaceScope(
                owner.as_str().to_owned(),
            ))),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let snapshot: awaken_runtime_contract::ExecutableAgentSnapshot =
            serde_json::from_value(body.0).unwrap();
        assert_eq!(snapshot.fingerprint.0, published.fingerprint);

        assert_eq!(
            export_publication(
                State(plane.clone()),
                Path(published.fingerprint),
                Some(Extension(awaken_tenancy::WorkspaceScope("intruder".into()))),
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            export_publication(State(failing_scoped_plane()), Path("fp".into()), None)
                .await
                .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            export_publication(State(plane.clone()), Path("   ".into()), None)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "R4"
        );

        let mut non_native = agent_config("agent-acp");
        non_native.model_binding =
            awaken_agent_config::ModelSelection::pinned("p", "m", "acp:claude");
        plane.put(&owner, &non_native).await.unwrap();
        let non_native = plane.publish(&owner, "agent-acp").await.unwrap();
        assert_eq!(
            export_publication(
                State(plane),
                Path(non_native.fingerprint),
                Some(Extension(awaken_tenancy::WorkspaceScope(
                    owner.as_str().to_owned(),
                ))),
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "R5"
        );
    }
}
