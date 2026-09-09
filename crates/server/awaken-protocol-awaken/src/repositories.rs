//! Awaken-owned reusable Repository paths and wire types over injected methods.
//! Resource application services own validation and durable registry behavior.

use awaken_resource_contract::{
    ClonePolicy, RepositoryConfigVersion, RepositoryDefinition, RepositoryDetails,
};
use axum::{Router, handler::Handler, routing::get};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRepository {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub remote_url: String,
    pub credential_binding: Option<String>,
    pub initial_branch: Option<String>,
    pub initial_commit: Option<String>,
    #[serde(default)]
    pub clone_policy: ClonePolicy,
}

#[derive(Serialize)]
pub struct RepositoryObject {
    #[serde(flatten)]
    definition: RepositoryDefinition,
    config: RepositoryConfigVersion,
}

impl From<RepositoryDetails> for RepositoryObject {
    fn from(value: RepositoryDetails) -> Self {
        Self {
            definition: value.definition,
            config: value.config,
        }
    }
}

/// The existing Resource adapter supplies trusted scope/header extraction and
/// application calls; this module alone owns Repository extension paths/DTOs.
pub fn repositories_router<List, ListArgs, Create, CreateArgs, Retrieve, RetrieveArgs, S>(
    list: List,
    create: Create,
    retrieve: Retrieve,
) -> Router<S>
where
    List: Handler<ListArgs, S>,
    ListArgs: 'static,
    Create: Handler<CreateArgs, S>,
    CreateArgs: 'static,
    Retrieve: Handler<RetrieveArgs, S>,
    RetrieveArgs: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/v1/awaken/repositories", get(list).post(create))
        .route("/v1/awaken/repositories/{id}", get(retrieve))
}
