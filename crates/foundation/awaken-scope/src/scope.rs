//! The scope tree — Org, Workspace, Project — as plain persistent entities.
//!
//! These are tenancy/addressing config, not aggregates whose history or ordering
//! is load-bearing, so they are **not** fact-sourced: each is a plain row read
//! and written directly, `update` mutates it in place, and a delete sets an
//! `archived` flag. Persistence is a [`ScopeRepo`] port (this crate ships the
//! in-memory reference; the SQL backends live in the adapter layer). The domain
//! services here own the business rules — slug validity, parent liveness, and
//! mapping a repo's slug conflict to a typed rejection.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// A URL-safe handle: lowercase alphanumeric with internal hyphens, 1–50 chars,
/// no leading/trailing hyphen.
pub fn slug_is_valid(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.is_empty() || bytes.len() > 50 {
        return false;
    }
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes.iter().all(|&c| alnum(c) || c == b'-')
}

/// Live or archived — derived from the `archived` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Archived,
}

/// The three tiers of the scope tree (ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    Org,
    Workspace,
    Project,
}

impl Tier {
    /// The tier a member of this tier roots under (`None` for `Org`).
    pub fn parent_tier(self) -> Option<Tier> {
        match self {
            Tier::Org => None,
            Tier::Workspace => Some(Tier::Org),
            Tier::Project => Some(Tier::Workspace),
        }
    }

    /// Stable table-less name of the tier (used by the SQL adapter for tables).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Org => "org",
            Tier::Workspace => "workspace",
            Tier::Project => "project",
        }
    }
}

/// One scope entity row.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: String,
    pub tier: Tier,
    pub parent: Option<String>,
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    pub archived: bool,
}

impl Entity {
    pub fn status(&self) -> Status {
        if self.archived {
            Status::Archived
        } else {
            Status::Active
        }
    }
}

/// Why a command was rejected by domain rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    InvalidSlug { slug: String },
    EmptyName,
    SlugTaken { slug: String },
    NotFound,
    ParentMissing,
    ParentArchived,
    Backend(String),
}

/// Persistence failures a [`ScopeRepo`] surfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoError {
    /// A live entity already holds this `(tier, parent, slug)`.
    SlugConflict,
    Backend(String),
}

/// The persistence port for the scope tree. Implementations enforce
/// per-parent slug uniqueness among **live** entities (a `SlugConflict`); they
/// carry no business rules of their own.
pub trait ScopeRepo: Send + Sync {
    fn insert(&self, entity: &Entity) -> Result<(), RepoError>;
    fn get(&self, tier: Tier, id: &str) -> Result<Option<Entity>, RepoError>;
    fn list(&self, tier: Tier, parent: Option<&str>) -> Result<Vec<Entity>, RepoError>;
    fn update(
        &self,
        tier: Tier,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<Option<Entity>, RepoError>;
    fn archive(&self, tier: Tier, id: &str) -> Result<bool, RepoError>;
}

fn map_repo(error: RepoError) -> Rejection {
    match error {
        RepoError::SlugConflict => Rejection::SlugTaken {
            slug: String::new(),
        },
        RepoError::Backend(detail) => Rejection::Backend(detail),
    }
}

// --- Domain services (business rules over the repo) ------------------------

/// Create an entity: validate the slug and name, verify the parent is live,
/// then insert (the repo enforces slug uniqueness).
pub fn create_entity(
    repo: &dyn ScopeRepo,
    tier: Tier,
    parent: Option<String>,
    id: String,
    name: String,
    slug: String,
    description: Option<String>,
) -> Result<Entity, Rejection> {
    if name.trim().is_empty() {
        return Err(Rejection::EmptyName);
    }
    if !slug_is_valid(&slug) {
        return Err(Rejection::InvalidSlug { slug });
    }
    if let Some(parent_tier) = tier.parent_tier() {
        let parent_id = parent.as_deref().ok_or(Rejection::ParentMissing)?;
        match repo.get(parent_tier, parent_id).map_err(map_repo)? {
            Some(p) if p.archived => return Err(Rejection::ParentArchived),
            Some(_) => {}
            None => return Err(Rejection::ParentMissing),
        }
    }
    let entity = Entity {
        id,
        tier,
        parent,
        name,
        slug,
        description,
        archived: false,
    };
    match repo.insert(&entity) {
        Ok(()) => Ok(entity),
        Err(RepoError::SlugConflict) => Err(Rejection::SlugTaken { slug: entity.slug }),
        Err(RepoError::Backend(detail)) => Err(Rejection::Backend(detail)),
    }
}

pub fn get_entity(repo: &dyn ScopeRepo, tier: Tier, id: &str) -> Result<Option<Entity>, Rejection> {
    repo.get(tier, id).map_err(map_repo)
}

pub fn list_entities(
    repo: &dyn ScopeRepo,
    tier: Tier,
    parent: Option<&str>,
) -> Result<Vec<Entity>, Rejection> {
    repo.list(tier, parent).map_err(map_repo)
}

pub fn update_entity(
    repo: &dyn ScopeRepo,
    tier: Tier,
    id: &str,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<Entity, Rejection> {
    if let Some(name) = name
        && name.trim().is_empty()
    {
        return Err(Rejection::EmptyName);
    }
    repo.update(tier, id, name, description)
        .map_err(map_repo)?
        .ok_or(Rejection::NotFound)
}

pub fn archive_entity(repo: &dyn ScopeRepo, tier: Tier, id: &str) -> Result<(), Rejection> {
    if repo.archive(tier, id).map_err(map_repo)? {
        Ok(())
    } else {
        Err(Rejection::NotFound)
    }
}

// --- In-memory reference implementation (test double) ----------------------

/// The in-memory reference `ScopeRepo`: the executable definition of the port
/// semantics, used by domain tests with zero adapter dependencies.
#[derive(Default)]
pub struct InMemoryScopeRepo {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_id: HashMap<String, Entity>,
    /// `(tier, parent, slug) -> id` for the live members.
    live_slugs: HashMap<(Tier, Option<String>, String), String>,
}

impl InMemoryScopeRepo {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ScopeRepo for InMemoryScopeRepo {
    fn insert(&self, entity: &Entity) -> Result<(), RepoError> {
        let mut inner = self.inner.lock().expect("scope repo poisoned");
        let key = (entity.tier, entity.parent.clone(), entity.slug.clone());
        if inner.live_slugs.contains_key(&key) {
            return Err(RepoError::SlugConflict);
        }
        inner.live_slugs.insert(key, entity.id.clone());
        inner.by_id.insert(entity.id.clone(), entity.clone());
        Ok(())
    }

    fn get(&self, tier: Tier, id: &str) -> Result<Option<Entity>, RepoError> {
        Ok(self
            .inner
            .lock()
            .expect("scope repo poisoned")
            .by_id
            .get(id)
            .filter(|e| e.tier == tier)
            .cloned())
    }

    fn list(&self, tier: Tier, parent: Option<&str>) -> Result<Vec<Entity>, RepoError> {
        let inner = self.inner.lock().expect("scope repo poisoned");
        let mut out: Vec<Entity> = inner
            .by_id
            .values()
            .filter(|e| e.tier == tier && !e.archived && e.parent.as_deref() == parent)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(out)
    }

    fn update(
        &self,
        tier: Tier,
        id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<Option<Entity>, RepoError> {
        let mut inner = self.inner.lock().expect("scope repo poisoned");
        match inner
            .by_id
            .get_mut(id)
            .filter(|e| e.tier == tier && !e.archived)
        {
            Some(entity) => {
                if let Some(name) = name {
                    entity.name = name.to_string();
                }
                if let Some(description) = description {
                    entity.description = Some(description.to_string());
                }
                Ok(Some(entity.clone()))
            }
            None => Ok(None),
        }
    }

    fn archive(&self, tier: Tier, id: &str) -> Result<bool, RepoError> {
        let mut inner = self.inner.lock().expect("scope repo poisoned");
        let key = match inner
            .by_id
            .get(id)
            .filter(|e| e.tier == tier && !e.archived)
        {
            Some(e) => (e.tier, e.parent.clone(), e.slug.clone()),
            None => return Ok(false),
        };
        inner.by_id.get_mut(id).expect("just checked").archived = true;
        inner.live_slugs.remove(&key);
        Ok(true)
    }
}
