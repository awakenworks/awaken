//! Managed Deployment HTTP adapter.
//!
//! This module owns only wire admission, filtering/pagination, DTO projection,
//! and HTTP error mapping. Aggregate transitions, persistence, scheduling, and
//! Session launch outcomes belong to `awaken-deployment-application`.

use std::sync::Arc;

use awaken_deployment_application::{
    AgentSelector, CreateDeploymentCommand, DeploymentApplication, DeploymentApplicationError,
    DeploymentOutcomeRubric, DeploymentPauseError, DeploymentPauseReason,
    DeploymentRepositoryCheckout, DeploymentResource, DeploymentRunFailure, DeploymentRunView,
    DeploymentSchedule, DeploymentSeedEvent, DeploymentStatus, DeploymentTrigger, DeploymentView,
    FieldUpdate, MetadataUpdate, UpdateDeploymentCommand,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, FixedOffset};
use chrono_tz::Tz;
use serde::Deserialize;

use crate::common::scope::RequiredWorkspaceScope;
use crate::routes::{ManagedJson, ManagedQuery};
use crate::types::deployment::{
    Deployment, DeploymentCreateParams, DeploymentInitialEvent, DeploymentRun,
    DeploymentUpdateParams, PausedReason, PausedReasonError, RunError, Schedule, TriggerContext,
};
use crate::types::resource::{RepositoryCheckout, ResourceAccess, ResourceInput};
use crate::types::session::{InboundEvent, OutcomeRubric};
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

fn application_schedule(schedule: Schedule) -> DeploymentSchedule {
    match schedule {
        Schedule::Cron {
            expression,
            timezone,
            ..
        } => DeploymentSchedule::Cron {
            expression,
            timezone,
        },
    }
}

fn wire_schedule(
    schedule: DeploymentSchedule,
    last_run_at: Option<String>,
    upcoming_runs_at: Vec<String>,
) -> Schedule {
    match schedule {
        DeploymentSchedule::Cron {
            expression,
            timezone,
        } => Schedule::Cron {
            expression,
            timezone,
            last_run_at,
            upcoming_runs_at,
        },
    }
}

fn application_rubric(rubric: OutcomeRubric) -> DeploymentOutcomeRubric {
    match rubric {
        OutcomeRubric::Text { content } => DeploymentOutcomeRubric::Text { content },
        OutcomeRubric::File { file_id } => DeploymentOutcomeRubric::File { file_id },
    }
}

fn wire_rubric(rubric: DeploymentOutcomeRubric) -> OutcomeRubric {
    match rubric {
        DeploymentOutcomeRubric::Text { content } => OutcomeRubric::Text { content },
        DeploymentOutcomeRubric::File { file_id } => OutcomeRubric::File { file_id },
    }
}

fn application_event(event: DeploymentInitialEvent) -> DeploymentSeedEvent {
    match event {
        DeploymentInitialEvent::UserMessage { content } => {
            DeploymentSeedEvent::UserMessage { content }
        }
        DeploymentInitialEvent::SystemMessage { content } => {
            DeploymentSeedEvent::SystemMessage { content }
        }
        DeploymentInitialEvent::UserDefineOutcome {
            description,
            rubric,
            max_iterations,
        } => DeploymentSeedEvent::DefineOutcome {
            description,
            rubric: application_rubric(rubric),
            max_iterations,
        },
    }
}

fn wire_event(event: DeploymentSeedEvent) -> DeploymentInitialEvent {
    match event {
        DeploymentSeedEvent::UserMessage { content } => {
            DeploymentInitialEvent::UserMessage { content }
        }
        DeploymentSeedEvent::SystemMessage { content } => {
            DeploymentInitialEvent::SystemMessage { content }
        }
        DeploymentSeedEvent::DefineOutcome {
            description,
            rubric,
            max_iterations,
        } => DeploymentInitialEvent::UserDefineOutcome {
            description,
            rubric: wire_rubric(rubric),
            max_iterations,
        },
    }
}

fn inbound_event(event: DeploymentSeedEvent) -> InboundEvent {
    wire_event(event).into()
}

fn application_resource(resource: ResourceInput) -> Result<DeploymentResource, WireError> {
    Ok(match resource {
        ResourceInput::File {
            file_id,
            mount_path,
        } => DeploymentResource::File {
            file_id,
            mount_path,
        },
        ResourceInput::MemoryStore {
            memory_store_id,
            mount_path,
            instructions,
            access,
        } => DeploymentResource::MemoryStore {
            memory_store_id,
            mount_path,
            instructions,
            access: access.map(|access| match access {
                ResourceAccess::ReadOnly => awaken_resource_contract::ResourceAccess::ReadOnly,
                ResourceAccess::ReadWrite => awaken_resource_contract::ResourceAccess::ReadWrite,
            }),
        },
        ResourceInput::GithubRepository {
            url,
            authorization_token,
            mount_path,
            checkout,
        } => {
            if authorization_token.is_some() {
                return Err(invalid(
                    "Deployment repository authorization_token cannot be stored durably; bind a vault credential instead",
                ));
            }
            DeploymentResource::GithubRepository {
                url,
                mount_path,
                checkout: checkout.map(|checkout| match checkout {
                    RepositoryCheckout::Branch { name } => {
                        DeploymentRepositoryCheckout::Branch { name }
                    }
                    RepositoryCheckout::Commit { sha } => {
                        DeploymentRepositoryCheckout::Commit { sha }
                    }
                }),
            }
        }
    })
}

fn wire_resource(resource: DeploymentResource) -> ResourceInput {
    match resource {
        DeploymentResource::File {
            file_id,
            mount_path,
        } => ResourceInput::File {
            file_id,
            mount_path,
        },
        DeploymentResource::MemoryStore {
            memory_store_id,
            mount_path,
            instructions,
            access,
        } => ResourceInput::MemoryStore {
            memory_store_id,
            mount_path,
            instructions,
            access: access.map(|access| match access {
                awaken_resource_contract::ResourceAccess::ReadOnly => ResourceAccess::ReadOnly,
                awaken_resource_contract::ResourceAccess::ReadWrite => ResourceAccess::ReadWrite,
            }),
        },
        DeploymentResource::GithubRepository {
            url,
            mount_path,
            checkout,
        } => ResourceInput::GithubRepository {
            url,
            authorization_token: None,
            mount_path,
            checkout: checkout.map(|checkout| match checkout {
                DeploymentRepositoryCheckout::Branch { name } => {
                    RepositoryCheckout::Branch { name }
                }
                DeploymentRepositoryCheckout::Commit { sha } => RepositoryCheckout::Commit { sha },
            }),
        },
    }
}

fn wire_pause_error(error: DeploymentPauseError) -> PausedReasonError {
    match error {
        DeploymentPauseError::EnvironmentArchivedError => {
            PausedReasonError::EnvironmentArchivedError
        }
        DeploymentPauseError::AgentArchivedError => PausedReasonError::AgentArchivedError,
        DeploymentPauseError::EnvironmentNotFoundError => {
            PausedReasonError::EnvironmentNotFoundError
        }
        DeploymentPauseError::VaultNotFoundError => PausedReasonError::VaultNotFoundError,
        DeploymentPauseError::FileNotFoundError => PausedReasonError::FileNotFoundError,
        DeploymentPauseError::SessionResourceNotFoundError => {
            PausedReasonError::SessionResourceNotFoundError
        }
        DeploymentPauseError::WorkspaceArchivedError => PausedReasonError::WorkspaceArchivedError,
        DeploymentPauseError::OrganizationDisabledError => {
            PausedReasonError::OrganizationDisabledError
        }
        DeploymentPauseError::MemoryStoreArchivedError => {
            PausedReasonError::MemoryStoreArchivedError
        }
        DeploymentPauseError::SkillNotFoundError => PausedReasonError::SkillNotFoundError,
        DeploymentPauseError::VaultArchivedError => PausedReasonError::VaultArchivedError,
        DeploymentPauseError::UnknownError => PausedReasonError::UnknownError,
        DeploymentPauseError::SelfHostedResourcesUnsupportedError => {
            PausedReasonError::SelfHostedResourcesUnsupportedError
        }
        DeploymentPauseError::McpEgressBlockedError => PausedReasonError::McpEgressBlockedError,
    }
}

fn wire_pause_reason(reason: DeploymentPauseReason) -> PausedReason {
    match reason {
        DeploymentPauseReason::Manual => PausedReason::Manual,
        DeploymentPauseReason::Error { error } => PausedReason::Error {
            error: wire_pause_error(error),
        },
    }
}

macro_rules! map_run_failure {
    ($error:expr, $target:ident) => {
        match $error {
            DeploymentRunFailure::EnvironmentArchivedError { message } => {
                $target::EnvironmentArchivedError { message }
            }
            DeploymentRunFailure::AgentArchivedError { message } => {
                $target::AgentArchivedError { message }
            }
            DeploymentRunFailure::EnvironmentNotFoundError { message } => {
                $target::EnvironmentNotFoundError { message }
            }
            DeploymentRunFailure::VaultNotFoundError { message } => {
                $target::VaultNotFoundError { message }
            }
            DeploymentRunFailure::VaultArchivedError { message } => {
                $target::VaultArchivedError { message }
            }
            DeploymentRunFailure::FileNotFoundError { message } => {
                $target::FileNotFoundError { message }
            }
            DeploymentRunFailure::MemoryStoreArchivedError { message } => {
                $target::MemoryStoreArchivedError { message }
            }
            DeploymentRunFailure::SkillNotFoundError { message } => {
                $target::SkillNotFoundError { message }
            }
            DeploymentRunFailure::SessionResourceNotFoundError { message } => {
                $target::SessionResourceNotFoundError { message }
            }
            DeploymentRunFailure::WorkspaceArchivedError { message } => {
                $target::WorkspaceArchivedError { message }
            }
            DeploymentRunFailure::OrganizationDisabledError { message } => {
                $target::OrganizationDisabledError { message }
            }
            DeploymentRunFailure::SessionRateLimitedError { message } => {
                $target::SessionRateLimitedError { message }
            }
            DeploymentRunFailure::SessionCreationRejectedError { message } => {
                $target::SessionCreationRejectedError { message }
            }
            DeploymentRunFailure::UnknownError { message } => $target::UnknownError { message },
            DeploymentRunFailure::SelfHostedResourcesUnsupportedError { message } => {
                $target::SelfHostedResourcesUnsupportedError { message }
            }
            DeploymentRunFailure::McpEgressBlockedError { message } => {
                $target::McpEgressBlockedError { message }
            }
        }
    };
}

fn wire_run_failure(error: DeploymentRunFailure) -> RunError {
    map_run_failure!(error, RunError)
}

fn application_run_failure(error: RunError) -> DeploymentRunFailure {
    match error {
        RunError::EnvironmentArchivedError { message } => {
            DeploymentRunFailure::EnvironmentArchivedError { message }
        }
        RunError::AgentArchivedError { message } => {
            DeploymentRunFailure::AgentArchivedError { message }
        }
        RunError::EnvironmentNotFoundError { message } => {
            DeploymentRunFailure::EnvironmentNotFoundError { message }
        }
        RunError::VaultNotFoundError { message } => {
            DeploymentRunFailure::VaultNotFoundError { message }
        }
        RunError::VaultArchivedError { message } => {
            DeploymentRunFailure::VaultArchivedError { message }
        }
        RunError::FileNotFoundError { message } => {
            DeploymentRunFailure::FileNotFoundError { message }
        }
        RunError::MemoryStoreArchivedError { message } => {
            DeploymentRunFailure::MemoryStoreArchivedError { message }
        }
        RunError::SkillNotFoundError { message } => {
            DeploymentRunFailure::SkillNotFoundError { message }
        }
        RunError::SessionResourceNotFoundError { message } => {
            DeploymentRunFailure::SessionResourceNotFoundError { message }
        }
        RunError::WorkspaceArchivedError { message } => {
            DeploymentRunFailure::WorkspaceArchivedError { message }
        }
        RunError::OrganizationDisabledError { message } => {
            DeploymentRunFailure::OrganizationDisabledError { message }
        }
        RunError::SessionRateLimitedError { message } => {
            DeploymentRunFailure::SessionRateLimitedError { message }
        }
        RunError::SessionCreationRejectedError { message } => {
            DeploymentRunFailure::SessionCreationRejectedError { message }
        }
        RunError::UnknownError { message } => DeploymentRunFailure::UnknownError { message },
        RunError::SelfHostedResourcesUnsupportedError { message } => {
            DeploymentRunFailure::SelfHostedResourcesUnsupportedError { message }
        }
        RunError::McpEgressBlockedError { message } => {
            DeploymentRunFailure::McpEgressBlockedError { message }
        }
    }
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
    let archived = record.archived_at.is_some();
    let last_run_at = record.last_run_at;
    let schedule = record.schedule.map(|schedule| {
        let upcoming = if archived {
            Vec::new()
        } else {
            upcoming_occurrences(&schedule, now_ms())
        };
        wire_schedule(schedule, last_run_at, upcoming)
    });
    let paused_reason = record.paused_reason.map(wire_pause_reason);
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
        initial_events: record.initial_events.into_iter().map(wire_event).collect(),
        metadata: record.metadata,
        name: record.name,
        paused_reason,
        resources: record.resources.into_iter().map(wire_resource).collect(),
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
    let error = record.error.map(wire_run_failure);
    let trigger_context = match record.trigger {
        DeploymentTrigger::Manual => TriggerContext::Manual,
        DeploymentTrigger::Schedule { scheduled_at } => TriggerContext::Schedule { scheduled_at },
    };
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
    crate::types::initial_event::validate_deployment_initial_events(events).map_err(invalid)
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
        initial_events: params
            .initial_events
            .into_iter()
            .map(application_event)
            .collect(),
        resources: params
            .resources
            .into_iter()
            .map(application_resource)
            .collect::<Result<_, _>>()?,
        schedule: params.schedule.map(application_schedule),
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
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    agent_id: Option<String>,
    #[serde(
        default,
        rename = "created_at[gte]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    created_at_gte: Option<DateTime<FixedOffset>>,
    #[serde(
        default,
        rename = "created_at[lte]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
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
    ManagedQuery(query): ManagedQuery<DeploymentListParams>,
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
        description: params.description.map(|value| match value {
            Some(value) => FieldUpdate::Replace(value),
            None => FieldUpdate::Clear,
        }),
        metadata: params.metadata.map(|value| match value {
            Some(value) => MetadataUpdate::Patch(value),
            None => MetadataUpdate::Clear,
        }),
        initial_events: params
            .initial_events
            .map(|events| events.into_iter().map(application_event).collect()),
        resources: params
            .resources
            .map(|value| match value {
                Some(resources) => resources
                    .into_iter()
                    .map(application_resource)
                    .collect::<Result<Vec<_>, _>>()
                    .map(FieldUpdate::Replace),
                None => Ok(FieldUpdate::Clear),
            })
            .transpose()?,
        schedule: params.schedule.map(|value| match value {
            Some(schedule) => FieldUpdate::Replace(application_schedule(schedule)),
            None => FieldUpdate::Clear,
        }),
        vault_ids: params.vault_ids.map(|value| match value {
            Some(value) => FieldUpdate::Replace(value),
            None => FieldUpdate::Clear,
        }),
        budget_max_list_cost_minor: match params.budget {
            None => None,
            Some(None) => Some(FieldUpdate::Clear),
            Some(Some(budget)) => Some(FieldUpdate::Replace(
                budget.max_list_cost_minor().map_err(invalid)?,
            )),
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
    #[serde(
        default,
        rename = "created_at[gt]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    created_at_gt: Option<DateTime<FixedOffset>>,
    #[serde(
        default,
        rename = "created_at[gte]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    created_at_gte: Option<DateTime<FixedOffset>>,
    #[serde(
        default,
        rename = "created_at[lt]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    created_at_lt: Option<DateTime<FixedOffset>>,
    #[serde(
        default,
        rename = "created_at[lte]",
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    created_at_lte: Option<DateTime<FixedOffset>>,
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
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
    ManagedQuery(query): ManagedQuery<DeploymentRunListParams>,
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

#[cfg(test)]
mod query_tests {
    use super::{DeploymentListParams, DeploymentRunListParams};

    #[test]
    fn empty_deployment_filters_match_python_omission() {
        // Causal matrix: both official SDKs expose the same optional filter
        // states, but TS emits empty query pairs and Python omits them. Cross
        // deployments/runs and every timestamp/id branch; populated RFC-3339
        // values prove the adapter does not collapse valid filters.
        let deployments: DeploymentListParams = serde_urlencoded::from_str(concat!(
            "agent_id=&page=&created_at%5Bgte%5D=&created_at%5Blte%5D="
        ))
        .unwrap();
        assert!(deployments.agent_id.is_none());
        assert!(deployments.page.page.is_none());
        assert!(deployments.created_at_gte.is_none());
        assert!(deployments.created_at_lte.is_none());

        let runs: DeploymentRunListParams = serde_urlencoded::from_str(concat!(
            "deployment_id=&page=&created_at%5Bgt%5D=&created_at%5Bgte%5D=",
            "&created_at%5Blt%5D=&created_at%5Blte%5D="
        ))
        .unwrap();
        assert!(runs.deployment_id.is_none());
        assert!(runs.page.page.is_none());
        assert!(runs.created_at_gt.is_none());
        assert!(runs.created_at_gte.is_none());
        assert!(runs.created_at_lt.is_none());
        assert!(runs.created_at_lte.is_none());

        let populated: DeploymentRunListParams = serde_urlencoded::from_str(
            "deployment_id=deploy_1&created_at%5Bgte%5D=2026-01-01T00%3A00%3A00Z",
        )
        .unwrap();
        assert_eq!(populated.deployment_id.as_deref(), Some("deploy_1"));
        assert!(populated.created_at_gte.is_some());
    }
}
