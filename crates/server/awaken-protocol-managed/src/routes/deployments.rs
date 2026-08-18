//! Managed Deployment HTTP adapter.
//!
//! This module owns only wire admission, filtering/pagination, DTO projection,
//! and HTTP error mapping. Aggregate transitions, persistence, scheduling, and
//! Session launch outcomes belong to `awaken-deployment-application`.

use std::sync::Arc;

use awaken_deployment_application::{
    AgentSelector, CreateDeploymentCommand, DeploymentApplication, DeploymentApplicationError,
    DeploymentRunView, DeploymentSchedule, DeploymentStatus, DeploymentTrigger, DeploymentView,
    UpdateDeploymentCommand,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, FixedOffset};
use chrono_tz::Tz;
use serde::Deserialize;

use crate::common::scope::RequiredWorkspaceScope;
use crate::routes::ManagedJson;
use crate::types::deployment::{
    Deployment, DeploymentCreateParams, DeploymentInitialEvent, DeploymentRun,
    DeploymentUpdateParams, RunError, Schedule, TriggerContext,
};
use crate::types::resource::ResourceInput;
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};

#[path = "deployments/launcher.rs"]
mod launcher;
pub use launcher::ManagedDeploymentSessionLauncher;

type WireError = (StatusCode, Json<ErrorResponse>);

pub fn deployments_router(application: Arc<DeploymentApplication>) -> Router {
    Router::new()
        .route(
            "/v1/deployments",
            post(create_deployment).get(list_deployments),
        )
        .route(
            "/v1/deployments/{id}",
            get(retrieve_deployment).post(update_deployment),
        )
        .route("/v1/deployments/{id}/archive", post(archive_deployment))
        .route("/v1/deployments/{id}/pause", post(pause_deployment))
        .route("/v1/deployments/{id}/unpause", post(unpause_deployment))
        .route("/v1/deployments/{id}/run", post(run_deployment))
        .route("/v1/deployment_runs", get(list_runs))
        .route("/v1/deployment_runs/{id}", get(retrieve_run))
        .with_state(application)
}

fn invalid(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

fn wire_error(error: DeploymentApplicationError) -> WireError {
    match error {
        DeploymentApplicationError::NotFound(what) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "not_found_error",
                format!("{what} not found"),
            )),
        ),
        DeploymentApplicationError::Terminal | DeploymentApplicationError::Conflict(_) => (
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "invalid_request_error",
                error.to_string(),
            )),
        ),
        DeploymentApplicationError::Invalid(_) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                error.to_string(),
            )),
        ),
        DeploymentApplicationError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new("api_error", error.to_string())),
        ),
    }
}

fn selector(input: &crate::types::AgentRef) -> Result<AgentSelector, WireError> {
    if matches!(
        input,
        crate::types::AgentRef::Object(reference)
            if matches!(reference.as_ref(), crate::types::AgentRefObject::AgentWithOverrides { .. })
    ) {
        return Err(invalid(
            "deployment Agent object must be an unmodified `agent` reference",
        ));
    }
    Ok(AgentSelector {
        id: input.id().to_string(),
        version: input.version(),
    })
}

fn application_schedule(schedule: Schedule) -> Result<DeploymentSchedule, WireError> {
    serde_json::from_value(
        serde_json::to_value(schedule).map_err(|error| invalid(error.to_string()))?,
    )
    .map_err(|error| invalid(error.to_string()))
}

fn wire_schedule(schedule: DeploymentSchedule) -> Result<Schedule, WireError> {
    serde_json::from_value(
        serde_json::to_value(schedule).map_err(|error| wire_projection(error.to_string()))?,
    )
    .map_err(|error| wire_projection(error.to_string()))
}

fn wire_projection(message: impl Into<String>) -> WireError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse::new("api_error", message)),
    )
}

fn values<T: serde::Serialize>(items: Vec<T>) -> Result<Vec<serde_json::Value>, WireError> {
    items
        .into_iter()
        .map(|item| serde_json::to_value(item).map_err(|error| invalid(error.to_string())))
        .collect()
}

fn typed_values<T: serde::de::DeserializeOwned>(
    items: Vec<serde_json::Value>,
) -> Result<Vec<T>, WireError> {
    items
        .into_iter()
        .map(|item| {
            serde_json::from_value(item).map_err(|error| wire_projection(error.to_string()))
        })
        .collect()
}

fn upcoming_occurrences(schedule: &DeploymentSchedule, after_ms: u64) -> Vec<String> {
    let Ok(cron) = awaken_deployment_contract::Cron::parse(schedule.expression()) else {
        return Vec::new();
    };
    let Ok(timezone) = schedule.timezone().parse::<Tz>() else {
        return Vec::new();
    };
    let mut cursor = after_ms;
    (0..5)
        .filter_map(|_| {
            cursor = cron.next_after_in(cursor, timezone)?;
            Some(awaken_session_contract::epoch_millis_to_rfc3339(cursor))
        })
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn project_deployment(view: DeploymentView) -> Result<Deployment, WireError> {
    let record = view.record;
    let schedule = record
        .schedule
        .map(|schedule| {
            let upcoming = if record.archived_at.is_some() {
                Vec::new()
            } else {
                upcoming_occurrences(&schedule, now_ms())
            };
            wire_schedule(schedule.with_runtime(record.last_run_at.clone(), upcoming))
        })
        .transpose()?;
    let paused_reason = record
        .paused_reason
        .map(|reason| {
            serde_json::from_value(
                serde_json::to_value(reason).map_err(|error| wire_projection(error.to_string()))?,
            )
            .map_err(|error| wire_projection(error.to_string()))
        })
        .transpose()?;
    Ok(Deployment {
        id: view.id,
        object_type: "deployment",
        agent: crate::types::agent::AgentReference::new(record.agent.id, record.agent.version),
        budget: record
            .budget_max_list_cost_minor
            .map(crate::types::BudgetLimit::from_minor),
        archived_at: record.archived_at,
        created_at: record.created_at,
        updated_at: record.updated_at,
        description: record.description,
        environment_id: record.environment_id,
        initial_events: typed_values(record.initial_events)?,
        metadata: record.metadata,
        name: record.name,
        paused_reason,
        resources: typed_values(record.resources)?,
        schedule,
        status: match record.status {
            DeploymentStatus::Active => "active",
            DeploymentStatus::Paused => "paused",
        },
        vault_ids: record.vault_ids,
    })
}

fn project_run(view: DeploymentRunView) -> Result<DeploymentRun, WireError> {
    let record = view.record;
    let error: Option<RunError> = record
        .error
        .map(|error| {
            serde_json::from_value(
                serde_json::to_value(error).map_err(|error| wire_projection(error.to_string()))?,
            )
            .map_err(|error| wire_projection(error.to_string()))
        })
        .transpose()?;
    let trigger_context: TriggerContext = serde_json::from_value(
        serde_json::to_value(record.trigger).map_err(|error| wire_projection(error.to_string()))?,
    )
    .map_err(|error| wire_projection(error.to_string()))?;
    Ok(DeploymentRun {
        id: view.id,
        object_type: "deployment_run",
        agent: crate::types::agent::AgentReference::new(record.agent.id, record.agent.version),
        created_at: record.created_at,
        deployment_id: record.deployment_id,
        error,
        session_id: record.session_id,
        trigger_context,
    })
}

fn validate_initial_events(events: &[DeploymentInitialEvent]) -> Result<(), WireError> {
    crate::types::initial_event::validate_initial_events(
        events,
        &crate::types::initial_event::InitialEventPolicy {
            min_count: 1,
            max_count: 50,
            allow_system_message: true,
            max_outcomes: None,
            outcome_iterations: Some(1..=20),
        },
    )
    .map_err(invalid)
}

fn validate_durable_resources(resources: &[ResourceInput]) -> Result<(), WireError> {
    if resources.iter().any(|resource| {
        matches!(
            resource,
            ResourceInput::GithubRepository {
                authorization_token: Some(_),
                ..
            }
        )
    }) {
        return Err(invalid(
            "Deployment repository authorization_token cannot be stored durably; bind a vault credential instead",
        ));
    }
    Ok(())
}

async fn create_deployment(
    State(application): State<Arc<DeploymentApplication>>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedJson(params): ManagedJson<DeploymentCreateParams>,
) -> Result<Json<Deployment>, WireError> {
    validate_initial_events(&params.initial_events)?;
    validate_durable_resources(&params.resources)?;
    let command = CreateDeploymentCommand {
        workspace_id: scope,
        agent: selector(&params.agent)?,
        environment_id: params.environment_id,
        name: params.name,
        description: params.description,
        metadata: params.metadata,
        initial_events: values(params.initial_events)?,
        resources: values(params.resources)?,
        schedule: params.schedule.map(application_schedule).transpose()?,
        vault_ids: params.vault_ids,
        budget_max_list_cost_minor: params
            .budget
            .map(|budget| budget.max_list_cost_minor())
            .transpose()
            .map_err(invalid)?,
    };
    application
        .create(command)
        .await
        .map_err(wire_error)
        .and_then(project_deployment)
        .map(Json)
}

async fn retrieve_deployment(
    State(application): State<Arc<DeploymentApplication>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
) -> Result<Json<Deployment>, WireError> {
    application
        .get(&scope, &id)
        .await
        .map_err(wire_error)
        .and_then(project_deployment)
        .map(Json)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentListParams {
    #[serde(flatten)]
    page: PageQuery,
    #[serde(default, rename = "beta")]
    _beta: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default, rename = "created_at[gte]")]
    created_at_gte: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[lte]")]
    created_at_lte: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    include_archived: bool,
    #[serde(default)]
    status: Option<DeploymentStatusFilter>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeploymentStatusFilter {
    Active,
    Paused,
}

fn page_query(query: &PageQuery) -> Result<PageQuery, WireError> {
    let limit = query.limit.unwrap_or(20);
    if !(1..=100).contains(&limit) {
        return Err(invalid("limit must be between 1 and 100"));
    }
    Ok(PageQuery {
        limit: Some(limit),
        page: query.page.clone(),
    })
}

async fn list_deployments(
    State(application): State<Arc<DeploymentApplication>>,
    Query(query): Query<DeploymentListParams>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
) -> Result<Json<PageCursor<Deployment>>, WireError> {
    if query.include_archived && query.status.is_some() {
        return Err(invalid(
            "include_archived and status filters cannot be combined",
        ));
    }
    let page = page_query(&query.page)?;
    let data = application
        .list(&scope)
        .await
        .map_err(wire_error)?
        .into_iter()
        .filter(|view| query.include_archived || view.record.archived_at.is_none())
        .filter(|view| {
            query
                .agent_id
                .as_ref()
                .is_none_or(|agent| &view.record.agent.id == agent)
        })
        .filter(|view| {
            query.status.is_none_or(|status| {
                matches!(
                    (view.record.status, status),
                    (DeploymentStatus::Active, DeploymentStatusFilter::Active)
                        | (DeploymentStatus::Paused, DeploymentStatusFilter::Paused)
                )
            })
        })
        .map(project_deployment)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|deployment| {
            let Ok(created) = DateTime::parse_from_rfc3339(&deployment.created_at) else {
                return false;
            };
            query.created_at_gte.is_none_or(|bound| created >= bound)
                && query.created_at_lte.is_none_or(|bound| created <= bound)
        })
        .collect();
    Ok(Json(paginate(data, &page, |deployment| {
        deployment.id.as_str()
    })))
}

async fn update_deployment(
    State(application): State<Arc<DeploymentApplication>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
    ManagedJson(params): ManagedJson<DeploymentUpdateParams>,
) -> Result<Json<Deployment>, WireError> {
    if let Some(events) = &params.initial_events {
        validate_initial_events(events)?;
    }
    if let Some(Some(resources)) = &params.resources {
        validate_durable_resources(resources)?;
    }
    let command = UpdateDeploymentCommand {
        agent: params.agent.as_ref().map(selector).transpose()?,
        environment_id: params.environment_id,
        name: params.name,
        description: params.description,
        metadata: params.metadata,
        initial_events: params.initial_events.map(values).transpose()?,
        resources: params
            .resources
            .map(|resources| resources.map(values).transpose())
            .transpose()?,
        schedule: params
            .schedule
            .map(|schedule| schedule.map(application_schedule).transpose())
            .transpose()?,
        vault_ids: params.vault_ids,
        budget_max_list_cost_minor: match params.budget {
            None => None,
            Some(None) => Some(None),
            Some(Some(budget)) => Some(Some(budget.max_list_cost_minor().map_err(invalid)?)),
        },
    };
    application
        .update(&scope, &id, command)
        .await
        .map_err(wire_error)
        .and_then(project_deployment)
        .map(Json)
}

macro_rules! deployment_action {
    ($name:ident, $method:ident) => {
        async fn $name(
            State(application): State<Arc<DeploymentApplication>>,
            Path(id): Path<String>,
            RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
        ) -> Result<Json<Deployment>, WireError> {
            application
                .$method(&scope, &id)
                .await
                .map_err(wire_error)
                .and_then(project_deployment)
                .map(Json)
        }
    };
}

deployment_action!(archive_deployment, archive);
deployment_action!(pause_deployment, pause);
deployment_action!(unpause_deployment, unpause);

async fn run_deployment(
    State(application): State<Arc<DeploymentApplication>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
) -> Result<Json<DeploymentRun>, WireError> {
    application
        .run(&scope, &id)
        .await
        .map_err(wire_error)
        .and_then(project_run)
        .map(Json)
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentRunListParams {
    #[serde(flatten)]
    page: PageQuery,
    #[serde(default, rename = "beta")]
    _beta: Option<String>,
    #[serde(default, rename = "created_at[gt]")]
    created_at_gt: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[gte]")]
    created_at_gte: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[lt]")]
    created_at_lt: Option<DateTime<FixedOffset>>,
    #[serde(default, rename = "created_at[lte]")]
    created_at_lte: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    deployment_id: Option<String>,
    #[serde(default)]
    has_error: Option<bool>,
    #[serde(default)]
    trigger_type: Option<TriggerType>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TriggerType {
    Schedule,
    Manual,
}

async fn retrieve_run(
    State(application): State<Arc<DeploymentApplication>>,
    Path(id): Path<String>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
) -> Result<Json<DeploymentRun>, WireError> {
    application
        .get_run(&scope, &id)
        .await
        .map_err(wire_error)
        .and_then(project_run)
        .map(Json)
}

async fn list_runs(
    State(application): State<Arc<DeploymentApplication>>,
    Query(query): Query<DeploymentRunListParams>,
    RequiredWorkspaceScope(scope): RequiredWorkspaceScope,
) -> Result<Json<PageCursor<DeploymentRun>>, WireError> {
    let page = page_query(&query.page)?;
    let data = application
        .list_runs(&scope)
        .await
        .map_err(wire_error)?
        .into_iter()
        .filter(|view| {
            query
                .deployment_id
                .as_ref()
                .is_none_or(|id| &view.record.deployment_id == id)
        })
        .filter(|view| {
            query
                .has_error
                .is_none_or(|expected| view.record.error.is_some() == expected)
        })
        .filter(|view| {
            query.trigger_type.is_none_or(|expected| {
                matches!(
                    (&view.record.trigger, expected),
                    (DeploymentTrigger::Manual, TriggerType::Manual)
                        | (DeploymentTrigger::Schedule { .. }, TriggerType::Schedule)
                )
            })
        })
        .map(project_run)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|run| {
            let Ok(created) = DateTime::parse_from_rfc3339(&run.created_at) else {
                return false;
            };
            query.created_at_gt.is_none_or(|bound| created > bound)
                && query.created_at_gte.is_none_or(|bound| created >= bound)
                && query.created_at_lt.is_none_or(|bound| created < bound)
                && query.created_at_lte.is_none_or(|bound| created <= bound)
        })
        .collect();
    Ok(Json(paginate(data, &page, |run| run.id.as_str())))
}
