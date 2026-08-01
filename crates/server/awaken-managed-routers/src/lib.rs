//! `awaken-managed-routers` — thin per-plane HTTP router adapters over the host.
//!
//! Driving adapters that build an axum `Router` using only the host's public API:
//! `files_router` over the Resources-owned File application service,
//! and the fully self-contained `models_router`. They live outside
//! `awaken-runtime-host` so the host stays the substrate and this wire surface stays
//! a thin adapter — the composition layer (`awaken-server`) mounts them onto the app.
//!
//! Memory content and Skill routers consume their aggregate ports directly. Memory
//! identity/lifecycle commands go through the Resources application service, so an
//! HTTP adapter cannot recreate lifecycle coordination.

mod files;
mod memory_stores;
mod models;
mod resource_scope;
mod resources;
mod skills;

pub use files::files_router;
pub use memory_stores::memory_stores_router;
pub use models::{
    ModelDirectory, ModelDirectoryFuture, ModelEntry, default_models, models_router,
    models_router_with_directory,
};
pub use resources::{ResourcesRouterInput, resources_router};
pub use skills::skills_router;
