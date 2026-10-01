// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: This source implements the repository-controlled agent profile product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! HTTP admission/projection for the config authoring plane.
//!
//! Domain mutation and publication remain owned by [`ConfigPlane`]; this module
//! only binds request scope, maps wire JSON, and translates outcomes to HTTP.

use awaken_agent_config::ConfigWrite;
use awaken_executable_agent_contract::ExecutableAgentRegistrationError;
use awaken_session_configuration_contract::preserve_runtime_agent_overrides;
use awaken_tenancy::{ExecutionWorkspace, ScopeId, WorkspaceScope};
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
    id.strip_prefix(awaken_executable_agent_contract::PREVIEW_AGENT_PREFIX)
        .is_some_and(|suffix| {
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
    RequiredScope(scope): RequiredScope,
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
    RequiredScope(scope): RequiredScope,
    execution: Option<Extension<ExecutionWorkspace>>,
    Path(preview_id): Path<String>,
) -> (StatusCode, Json<Value>) {
    if !valid_preview_id(&preview_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid preview id" })),
        );
    }
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

fn workspace_not_found() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "workspace not found" })),
    )
}

/// The tenant scope the trusted ingress edge already selected.
///
/// Absence is refused here, in the extractor, so no handler below has a `None`
/// arm that could be given a compiled-in meaning. A single-machine deployment
/// receives its scope from the one composition-root stamp
/// (`awaken_coordinator::workspace_path::with_platform_workspace`).
pub(crate) struct RequiredScope(pub(crate) ScopeId);

impl<S> axum::extract::FromRequestParts<S> for RequiredScope
where
    S: Send + Sync,
{
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        match parts
            .extensions
            .get::<WorkspaceScope>()
            .and_then(WorkspaceScope::non_empty)
        {
            Some(scope) => Ok(Self(ScopeId::from(scope))),
            None => Err(axum::response::IntoResponse::into_response(
                workspace_not_found(),
            )),
        }
    }
}

/// Absence is scoped, so another Workspace's fingerprint is never disclosed.
async fn get_publication(
    State(plane): State<ConfigPlane>,
    Path(fingerprint): Path<String>,
    RequiredScope(scope): RequiredScope,
) -> (StatusCode, Json<Value>) {
    if fingerprint.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "publication fingerprint is required" })),
        );
    }
    match plane.publication(&scope, &fingerprint).await {
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
    RequiredScope(scope): RequiredScope,
) -> (StatusCode, Json<Value>) {
    if fingerprint.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "publication fingerprint is required" })),
        );
    }
    match plane.publication(&scope, &fingerprint).await {
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

pub(crate) async fn list_configs(
    State(plane): State<ConfigPlane>,
    RequiredScope(scope): RequiredScope,
    execution: Option<Extension<ExecutionWorkspace>>,
) -> (StatusCode, Json<Value>) {
    let execution = publication_workspace(&scope, execution.as_ref());
    match plane.list(&scope).await {
        Ok(configs) => {
            let _execution_workspace = execution.unwrap_or(scope.as_str());
            let published_revisions = match plane.published_revisions(&scope).await {
                Ok(revisions) => revisions,
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": error })),
                    );
                }
            };
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
                let published = published_revisions.contains(&(config.id, versioned.revision));
                data.push(managed_from_agent_config(&versioned.config, published));
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
    RequiredScope(scope): RequiredScope,
    execution: Option<Extension<ExecutionWorkspace>>,
) -> (StatusCode, Json<Value>) {
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

fn take_agent_permission_preset(
    body: &mut Value,
) -> Result<Option<awaken_session_configuration_contract::AgentPermissionPreset>, String> {
    body.as_object_mut()
        .and_then(|body| body.remove("permission_preset"))
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| format!("invalid permission_preset: {error}"))
}

fn apply_authoring_permission_preset(
    config: &mut awaken_agent_config::AgentConfig,
    preset: Option<awaken_session_configuration_contract::AgentPermissionPreset>,
    allowed_runtime_override_ids: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
    let Some(preset) = preset else {
        return Ok(());
    };
    awaken_session_configuration_contract::apply_agent_permission_preset(
        awaken_session_configuration_contract::AgentPermissionPresetTarget {
            tool_ids: &mut config.tool_ids,
            toolsets: &mut config.toolsets,
            mcp_servers: &config.mcp_servers,
            plugin_ids: &mut config.plugin_ids,
            plugin_config: &mut config.plugin_config,
            allowed_runtime_override_ids,
        },
        preset,
    )
}

pub(crate) async fn validate(
    State(plane): State<ConfigPlane>,
    RequiredScope(scope): RequiredScope,
    execution: Option<Extension<ExecutionWorkspace>>,
    Path(id): Path<String>,
    Json(mut body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let permission_preset = match take_agent_permission_preset(&mut body) {
        Ok(preset) => preset,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "valid": false,
                    "issues": [{ "path": "permission_preset", "message": error, "severity": "error" }],
                })),
            );
        }
    };
    let mut config = match agent_config_from_managed(id, &body) {
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
    let allowed_runtime_override_ids = plane.runtime_agent_override_ids(&scope, &config);
    if let Err(error) = apply_authoring_permission_preset(
        &mut config,
        permission_preset,
        &allowed_runtime_override_ids,
    ) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "valid": false,
                "issues": [{ "path": "permission_preset", "message": error, "severity": "error" }],
            })),
        );
    }
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
    RequiredScope(scope): RequiredScope,
    Path(id): Path<String>,
    Json(mut body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let permission_preset = match take_agent_permission_preset(&mut body) {
        Ok(preset) => preset,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
        }
    };
    let mut config = match agent_config_from_managed(id.clone(), &body) {
        Ok(config) => config,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    };
    let current = match plane.get_versioned(&scope, &id).await {
        Ok(current) => current,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error })),
            );
        }
    };
    let observed_revision = current.as_ref().map_or(0, |revision| revision.revision);
    // An authored `generation` is the caller's optimistic-concurrency fence.
    // Reading it with `as_u64` alone made a wrongly typed one — a string `"3"`,
    // a float `3.0` — indistinguishable from an absent one, so a client that
    // believed it was fencing had its write applied unfenced over whatever
    // revision happened to be current. A present `generation` must be a
    // revision number or the write is refused.
    let expected_generation = match body.get("generation") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(expected) => Some(expected),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "generation must be a non-negative integer revision number",
                    })),
                );
            }
        },
    };
    if let Some(expected) = expected_generation
        && expected != observed_revision
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "config generation conflict",
                "current_revision": (observed_revision != 0).then_some(observed_revision),
            })),
        );
    }
    let allowed_runtime_override_ids = plane.runtime_agent_override_ids(&scope, &config);
    if let Some(current) = &current {
        preserve_runtime_agent_overrides(
            &current.config.toolsets,
            &mut config.toolsets,
            &allowed_runtime_override_ids,
        );
    }
    if let Err(error) = apply_authoring_permission_preset(
        &mut config,
        permission_preset,
        &allowed_runtime_override_ids,
    ) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
    }
    match plane
        .put_if_revision(&scope, &config, observed_revision)
        .await
    {
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
    }
}

pub(crate) async fn publish(
    State(plane): State<ConfigPlane>,
    RequiredScope(scope): RequiredScope,
    execution: Option<Extension<ExecutionWorkspace>>,
    Path(id): Path<String>,
    request: Option<Json<Value>>,
) -> (StatusCode, Json<Value>) {
    let request = match request
        .map(|Json(request)| PublishRequest::try_from(request))
        .transpose()
    {
        Ok(request) => request.unwrap_or_default(),
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    };
    let result = match publication_workspace(&scope, execution.as_ref()) {
        Some(workspace) => {
            let published = plane
                .publish_for_execution_workspace_at_revisions(
                    &scope,
                    workspace,
                    &id,
                    request.source_revision,
                    request.resource_revision,
                )
                .await;
            if publish_conflict_requires_dependency_reconciliation(
                published.as_ref().err(),
                request.source_revision,
                request.resource_revision,
            ) {
                match plane
                    .reconcile_for_execution_workspace(&scope, workspace, &id)
                    .await
                {
                    Ok(true) => match plane.latest_publication(&scope, &id).await {
                        Ok(Some(publication)) => Ok(publication),
                        Ok(None) => Err(PublishError::Store(
                            "Agent reconciliation produced no durable publication".into(),
                        )),
                        Err(error) => Err(PublishError::Store(error)),
                    },
                    Ok(false) => published,
                    Err(error) => Err(PublishError::Store(error)),
                }
            } else {
                published
            }
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
                "source_revision": publication.source_revision,
                "installed": true,
            })),
        ),
        Err(error) => publish_error_response(error),
    }
}

fn publish_conflict_requires_dependency_reconciliation(
    error: Option<&PublishError>,
    requested_source_revision: Option<u64>,
    requested_resource_revision: Option<i64>,
) -> bool {
    matches!(
        (error, requested_source_revision, requested_resource_revision),
        (Some(PublishError::StaleRevision(Some(current))), Some(requested), None)
            if current == &requested
    )
}

#[cfg(test)]
#[test]
fn only_same_source_dependency_drift_enters_reconciliation() {
    // Causes: D1 publish reports current source revision; D2 request names the
    // same, a stale, or no source revision; D3 reviewed Resource fence is absent
    // or present. Effect E1 only D1+same(D2)+absent(D3) may invoke the existing
    // policy-bound reconciler; every true stale/reviewed request retains 409.
    let same = PublishError::StaleRevision(Some(7));
    assert!(publish_conflict_requires_dependency_reconciliation(
        Some(&same),
        Some(7),
        None
    ));
    for (source, resource) in [(Some(6), None), (None, None), (Some(7), Some(3))] {
        assert!(!publish_conflict_requires_dependency_reconciliation(
            Some(&same),
            source,
            resource
        ));
    }
    assert!(!publish_conflict_requires_dependency_reconciliation(
        Some(&PublishError::Unresolvable("missing".into())),
        Some(7),
        None
    ));
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

    #[tokio::test]
    async fn config_get_controlled_put_preserves_runtime_only_policy_under_one_revision_fence() {
        // Cause/effect table for the Console GET -> controlled PUT round trip:
        // | rule | current opaque/collision | catalog + role | effect |
        // | O1   | agent_run ask             | delegation + roster | retain exact |
        // | O2   | delete + custom_dynamic   | client/regular       | prune both |
        // | O3   | O1/O2                     | stale revision       | 409; zero write |
        // The closed wire never gains another member table; the versioned current
        // config supplies bytes, but only the catalog's AgentDelegation semantic
        // role may cross the projection. A same-name custom/dynamic descriptor
        // cannot inherit historical permission. The observed revision fences CAS.
        use awaken_runtime_contract::agent_bindings::{
            ToolExecutionPolicy, ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy,
            ToolsetSource,
        };
        use awaken_runtime_contract::resolved::ToolDescriptor;

        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![
                ToolDescriptor::pinned(
                    "managed",
                    "agent_run",
                    "Run an exact roster Agent",
                    json!({"type": "object"}),
                )
                .with_kind(awaken_runtime_contract::resolved::ToolKind::AgentDelegation),
                ToolDescriptor::pinned(
                    "dynamic",
                    "custom_dynamic",
                    "Current dynamic tool",
                    json!({"type": "object"}),
                ),
            ])),
        );
        let scope = ScopeId::from("workspace-console");
        let mut seeded = agent_config("opaque-agent");
        let runtime_only = ToolPolicyOverride::new(
            "agent_run",
            ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
        );
        seeded.toolsets = vec![ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: false,
                permission: ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: vec![
                runtime_only.clone(),
                ToolPolicyOverride::new("delete", ToolExecutionPolicy::default()),
                ToolPolicyOverride::new("custom_dynamic", ToolExecutionPolicy::default()),
            ],
        }];
        seeded.multiagent = Some(awaken_agent_config::MultiagentConfig {
            agents: vec![awaken_agent_config::MultiagentTarget::SelfReference],
        });
        plane.put(&scope, &seeded).await.unwrap();

        let (status, Json(mut wire)) = get_config(
            State(plane.clone()),
            Path(seeded.id.clone()),
            RequiredScope(ScopeId::from(scope.as_str())),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "O1 GET");
        assert_eq!(wire["generation"], 1, "O1");
        assert!(
            !wire["tools"].to_string().contains("agent_run"),
            "O1 closed wire"
        );
        wire["tools"] = json!([
            {
                "type": "agent_toolset_20260401",
                "default_config": {
                    "enabled": false,
                    "permission_policy": { "type": "always_allow" }
                },
                "configs": [{
                    "name": "write",
                    "enabled": true,
                    "permission_policy": { "type": "always_ask" }
                }]
            },
            {
                "type": "custom",
                "name": "delete",
                "description": "Client-owned collision",
                "input_schema": { "type": "object" }
            }
        ]);
        let (status, Json(saved)) = put_config(
            State(plane.clone()),
            RequiredScope(ScopeId::from(scope.as_str())),
            Path(seeded.id.clone()),
            Json(wire.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "O1 PUT: {saved}");
        assert_eq!(saved["generation"], 2, "O1");
        let current = plane
            .get_versioned(&scope, &seeded.id)
            .await
            .unwrap()
            .unwrap();
        let agent = current
            .config
            .toolsets
            .iter()
            .find(|toolset| toolset.source == ToolsetSource::Agent)
            .unwrap();
        assert_eq!(
            agent
                .overrides
                .iter()
                .find(|entry| entry.name == "agent_run")
                .unwrap(),
            &runtime_only,
            "O1 exact opaque bytes"
        );
        assert_eq!(
            agent.policy_for("write").permission,
            ToolPermissionRequirement::AlwaysAsk,
            "O1 controlled policy"
        );
        assert!(
            agent
                .overrides
                .iter()
                .all(|entry| !matches!(entry.name.as_str(), "delete" | "custom_dynamic")),
            "O2 retired/client/dynamic collisions are pruned"
        );
        let publication = plane.publish(&scope, &seeded.id).await.unwrap();
        let verdict = publication
            .snapshot
            .resolved_spec
            .plugin_config
            .agent
            .tool_policy("agent_run")
            .expect("O1 Runtime policy");
        assert!(verdict.enabled, "O1 Runtime enabled");
        assert_eq!(
            verdict.permission,
            ToolPermissionRequirement::AlwaysAsk,
            "O1 Runtime ask"
        );

        wire["generation"] = json!(1);
        let (status, _) = put_config(
            State(plane.clone()),
            RequiredScope(ScopeId::from(scope.as_str())),
            Path(seeded.id.clone()),
            Json(wire.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "O3");
        assert_eq!(
            plane
                .get_versioned(&scope, &seeded.id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            2,
            "O2 zero write"
        );

        // O4: a `generation` the fence cannot read is refused, never treated as
        // an absent fence. `as_u64` alone made a stringly-typed or fractional
        // revision indistinguishable from no fence at all, so a caller who
        // believed it was guarding against concurrent edits had its write
        // applied unfenced. The stored revision is the consumer-side witness.
        for unreadable in [json!("1"), json!(1.5), json!(true), json!([1])] {
            let mut fenced = wire.clone();
            fenced["generation"] = unreadable.clone();
            let (status, body) = put_config(
                State(plane.clone()),
                RequiredScope(ScopeId::from(scope.as_str())),
                Path(seeded.id.clone()),
                Json(fenced),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "O4 {unreadable}");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("generation"),
                "O4 names the field"
            );
            assert_eq!(
                plane
                    .get_versioned(&scope, &seeded.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .revision,
                2,
                "O4 zero write"
            );
        }
    }

    #[tokio::test]
    async fn controlled_preset_is_transient_and_server_projected_once() {
        // Cause/effect graph: C1 Web submits one transient preset; C2 its wire
        // draft selected read/write/bash; C3 an unknown preset is submitted.
        // Effects: E1 the server-owned domain
        // transform keeps Bash enabled but requires confirmation; E2 read is
        // allow and write is ask; E3 no preset marker or legacy permission
        // section is stored; E4 C3 is rejected without another write.
        // R1=C1+C2=>E1-E3; R2=C3=>400+E4.
        use awaken_runtime_contract::agent_bindings::{ToolPermissionRequirement, ToolsetSource};

        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from("workspace-preset");
        let seeded = agent_config("preset-agent");
        plane.put(&scope, &seeded).await.unwrap();
        let (status, Json(mut wire)) = get_config(
            State(plane.clone()),
            Path(seeded.id.clone()),
            RequiredScope(ScopeId::from(scope.as_str())),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        wire["tools"] = json!(["read", "write", "bash"]);
        wire["permission_preset"] = json!("controlled_modifications");
        let (status, Json(result)) = put_config(
            State(plane.clone()),
            RequiredScope(ScopeId::from(scope.as_str())),
            Path(seeded.id.clone()),
            Json(wire),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "R1: {result}");
        let stored = plane
            .get_versioned(&scope, &seeded.id)
            .await
            .unwrap()
            .unwrap()
            .config;
        let agent = stored
            .toolsets
            .iter()
            .find(|toolset| toolset.source == ToolsetSource::Agent)
            .unwrap();
        for (name, enabled, permission) in [
            ("bash", true, ToolPermissionRequirement::AlwaysAsk),
            ("read", true, ToolPermissionRequirement::AlwaysAllow),
            ("write", true, ToolPermissionRequirement::AlwaysAsk),
        ] {
            let policy = agent.policy_for(name);
            assert_eq!(
                (policy.enabled, policy.permission),
                (enabled, permission),
                "{name}"
            );
        }
        let serialized = serde_json::to_value(&stored).unwrap().to_string();
        assert!(!serialized.contains("permission_preset"), "E3 transient");
        assert!(
            !stored.plugin_config.contains_key("permission"),
            "E3 legacy"
        );

        let (status, Json(mut invalid)) = get_config(
            State(plane.clone()),
            Path(seeded.id.clone()),
            RequiredScope(ScopeId::from(scope.as_str())),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "R2 setup");
        invalid["permission_preset"] = json!("read_only");
        let (status, Json(error)) = put_config(
            State(plane.clone()),
            RequiredScope(ScopeId::from(scope.as_str())),
            Path(seeded.id.clone()),
            Json(invalid),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "R2/E4: {error}");
        assert_eq!(
            plane
                .get_versioned(&scope, &seeded.id)
                .await
                .unwrap()
                .unwrap()
                .config,
            stored,
            "R2/E4 no write"
        );
    }

    /// The full-replace PUT over the real route: an unaddressable key must be
    /// refused, not applied as "that section was absent".
    ///
    /// | Rule | authored key                    | status | stored system prompt |
    /// | U1   | `system` (the wire name)         | 200    | replaced as authored |
    /// | U2   | `instructions` (the domain name) | 400    | unchanged            |
    /// | U3   | `id`/`type`/`published`/lifecycle | 200    | replaced as authored |
    ///
    /// U2 is the live defect. `put_config` builds a **fresh** `AgentConfig` with
    /// no merge, so a key the reader does not model does not leave its section
    /// alone — the section is rebuilt from its default. Authoring `instructions`
    /// returned 200 and stored an empty system prompt, destroying it.
    ///
    /// U3 pins the other half: the keys the GET projection itself emits stay
    /// accepted, so the console's ordinary GET -> edit -> PUT is not broken by
    /// the guard.
    #[tokio::test]
    async fn a_put_refuses_an_unaddressable_key_instead_of_blanking_its_section() {
        let plane = ConfigPlane::new(
            Arc::new(test_service()),
            Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
            Arc::new(crate::tool_catalog::StaticToolCatalog(vec![])),
        );
        let scope = ScopeId::from("workspace-unknown-key");
        let mut seeded = agent_config("prompted-agent");
        seeded.instructions = "the authored system prompt".into();
        plane.put(&scope, &seeded).await.unwrap();

        let wire_now = |plane: ConfigPlane, scope: ScopeId, id: String| async move {
            let (status, Json(wire)) =
                get_config(State(plane), Path(id), RequiredScope(scope), None).await;
            assert_eq!(status, StatusCode::OK);
            wire
        };
        let base = wire_now(plane.clone(), scope.clone(), seeded.id.clone()).await;
        assert_eq!(base["system"], "the authored system prompt", "setup");

        // U2: the same edit, addressed by the domain field name.
        let mut renamed = base.clone();
        renamed.as_object_mut().unwrap().remove("system");
        renamed["instructions"] = json!("an edit the author expects to land");
        let (status, Json(error)) = put_config(
            State(plane.clone()),
            RequiredScope(scope.clone()),
            Path(seeded.id.clone()),
            Json(renamed),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "U2: {error}");
        assert!(
            error["error"]
                .as_str()
                .unwrap_or_default()
                .contains("instructions"),
            "U2 names the offending key: {error}"
        );
        let stored = plane
            .get_versioned(&scope, &seeded.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.config.instructions, "the authored system prompt",
            "U2: the authored prompt survives a refused write"
        );
        assert_eq!(stored.revision, 1, "U2: no revision is written");

        // U1 + U3: the projected round trip, edited through the wire name, lands.
        let mut edited = base;
        edited["system"] = json!("an edit the author expects to land");
        for (key, value) in [
            ("id", json!(seeded.id.clone())),
            ("type", json!("agent")),
            ("published", json!(false)),
            ("disabled_at", Value::Null),
            ("archived_at", Value::Null),
        ] {
            edited[key] = value;
        }
        let (status, Json(saved)) = put_config(
            State(plane.clone()),
            RequiredScope(scope.clone()),
            Path(seeded.id.clone()),
            Json(edited),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "U1/U3: {saved}");
        assert_eq!(
            plane
                .get_versioned(&scope, &seeded.id)
                .await
                .unwrap()
                .unwrap()
                .config
                .instructions,
            "an edit the author expects to land",
            "U1/U3"
        );
    }

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
            RequiredScope(ScopeId::from(owner.as_str().to_owned())),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["fingerprint"], published.fingerprint);

        let (status, _) = get_publication(
            State(plane),
            Path(published.fingerprint),
            RequiredScope(ScopeId::from("intruder")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            get_publication(
                State(failing_scoped_plane()),
                Path("fp".into()),
                RequiredScope(ScopeId::from("workspace-owner")),
            )
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
            RequiredScope(ScopeId::from(owner.as_str().to_owned())),
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
                RequiredScope(ScopeId::from("intruder")),
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            export_publication(
                State(failing_scoped_plane()),
                Path("fp".into()),
                RequiredScope(ScopeId::from("workspace-owner")),
            )
            .await
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            export_publication(
                State(plane.clone()),
                Path("   ".into()),
                RequiredScope(ScopeId::from("workspace-owner")),
            )
            .await
            .0,
            StatusCode::BAD_REQUEST,
            "R4"
        );

        let mut non_native = agent_config("agent-acp");
        non_native.model_binding =
            awaken_agent_config::ModelSelection::pinned("p", "m", "acp:claude")
                .expect("literal coordinates");
        plane.put(&owner, &non_native).await.unwrap();
        let non_native = plane.publish(&owner, "agent-acp").await.unwrap();
        assert_eq!(
            export_publication(
                State(plane),
                Path(non_native.fingerprint),
                RequiredScope(ScopeId::from(owner.as_str().to_owned())),
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "R5"
        );
    }
}
