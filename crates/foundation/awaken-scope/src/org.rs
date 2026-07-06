//! Org — the tenant / billing / partition root of the scope tree (ADR-0006).

use serde::{Deserialize, Serialize};

use crate::scope::{self, Entity, Rejection, ScopeRepo, Status, Tier};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrgView {
    pub id: String,
    pub name: String,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: Status,
}

#[derive(Debug, Clone)]
pub struct CreateOrg {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateOrg {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
}

fn view(entity: Entity) -> OrgView {
    let status = entity.status();
    OrgView {
        id: entity.id,
        name: entity.name,
        slug: entity.slug,
        description: entity.description,
        status,
    }
}

pub fn create_org(repo: &dyn ScopeRepo, cmd: CreateOrg) -> Result<OrgView, Rejection> {
    scope::create_entity(
        repo,
        Tier::Org,
        None,
        cmd.id,
        cmd.name,
        cmd.slug,
        cmd.description,
    )
    .map(view)
}

pub fn get_org(repo: &dyn ScopeRepo, id: &str) -> Result<Option<OrgView>, Rejection> {
    Ok(scope::get_entity(repo, Tier::Org, id)?.map(view))
}

pub fn list_orgs(repo: &dyn ScopeRepo) -> Result<Vec<OrgView>, Rejection> {
    Ok(scope::list_entities(repo, Tier::Org, None)?
        .into_iter()
        .map(view)
        .collect())
}

pub fn update_org(repo: &dyn ScopeRepo, cmd: UpdateOrg) -> Result<OrgView, Rejection> {
    scope::update_entity(
        repo,
        Tier::Org,
        &cmd.id,
        cmd.name.as_deref(),
        cmd.description.as_deref(),
    )
    .map(view)
}

pub fn archive_org(repo: &dyn ScopeRepo, id: &str) -> Result<(), Rejection> {
    scope::archive_entity(repo, Tier::Org, id)
}
