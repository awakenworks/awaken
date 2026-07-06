//! Workspace — a work-organization grouping inside an Org (ADR-0006).

use serde::{Deserialize, Serialize};

use crate::scope::{self, Entity, Rejection, ScopeRepo, Status, Tier};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: String,
    pub org: String,
    pub name: String,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct CreateWorkspace {
    pub id: String,
    pub org: String,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateWorkspace {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

fn view(entity: Entity) -> WorkspaceView {
    let status = entity.status();
    WorkspaceView {
        id: entity.id,
        org: entity.parent.unwrap_or_default(),
        name: entity.name,
        slug: entity.slug,
        description: entity.description,
        status,
    }
}

pub fn create_workspace(
    repo: &dyn ScopeRepo,
    cmd: CreateWorkspace,
) -> Result<WorkspaceView, Rejection> {
    scope::create_entity(
        repo,
        Tier::Workspace,
        Some(cmd.org),
        cmd.id,
        cmd.name,
        cmd.slug,
        cmd.description,
    )
    .map(view)
}

pub fn get_workspace(repo: &dyn ScopeRepo, id: &str) -> Result<Option<WorkspaceView>, Rejection> {
    Ok(scope::get_entity(repo, Tier::Workspace, id)?.map(view))
}

pub fn list_workspaces(repo: &dyn ScopeRepo, org: &str) -> Result<Vec<WorkspaceView>, Rejection> {
    Ok(scope::list_entities(repo, Tier::Workspace, Some(org))?
        .into_iter()
        .map(view)
        .collect())
}

pub fn update_workspace(
    repo: &dyn ScopeRepo,
    cmd: UpdateWorkspace,
) -> Result<WorkspaceView, Rejection> {
    scope::update_entity(
        repo,
        Tier::Workspace,
        &cmd.id,
        cmd.name.as_deref(),
        cmd.description.as_deref(),
    )
    .map(view)
}

pub fn archive_workspace(repo: &dyn ScopeRepo, id: &str) -> Result<(), Rejection> {
    scope::archive_entity(repo, Tier::Workspace, id)
}
