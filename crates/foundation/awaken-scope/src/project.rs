//! Project — the innermost work container inside a Workspace (ADR-0006).
//!
//! A Project is the container for managed agents: the managed-agent protocol
//! surface is addressable under `/projects/{id|slug}/…`, and a Project is the
//! authorization anchor those requests resolve to (`ScopeRef::Project`).

use serde::{Deserialize, Serialize};

use crate::scope::{self, Entity, Rejection, ScopeRepo, Status, Tier};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: String,
    pub workspace: String,
    pub name: String,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct CreateProject {
    pub id: String,
    pub workspace: String,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateProject {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

fn view(entity: Entity) -> ProjectView {
    let status = entity.status();
    ProjectView {
        id: entity.id,
        workspace: entity.parent.unwrap_or_default(),
        name: entity.name,
        slug: entity.slug,
        description: entity.description,
        status,
    }
}

pub fn create_project(repo: &dyn ScopeRepo, cmd: CreateProject) -> Result<ProjectView, Rejection> {
    scope::create_entity(
        repo,
        Tier::Project,
        Some(cmd.workspace),
        cmd.id,
        cmd.name,
        cmd.slug,
        cmd.description,
    )
    .map(view)
}

pub fn get_project(repo: &dyn ScopeRepo, id: &str) -> Result<Option<ProjectView>, Rejection> {
    Ok(scope::get_entity(repo, Tier::Project, id)?.map(view))
}

pub fn list_projects(repo: &dyn ScopeRepo, workspace: &str) -> Result<Vec<ProjectView>, Rejection> {
    Ok(scope::list_entities(repo, Tier::Project, Some(workspace))?
        .into_iter()
        .map(view)
        .collect())
}

pub fn update_project(repo: &dyn ScopeRepo, cmd: UpdateProject) -> Result<ProjectView, Rejection> {
    scope::update_entity(
        repo,
        Tier::Project,
        &cmd.id,
        cmd.name.as_deref(),
        cmd.description.as_deref(),
    )
    .map(view)
}

pub fn archive_project(repo: &dyn ScopeRepo, id: &str) -> Result<(), Rejection> {
    scope::archive_entity(repo, Tier::Project, id)
}
