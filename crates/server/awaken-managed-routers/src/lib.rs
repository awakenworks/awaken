//! `awaken-managed-routers` — thin per-plane HTTP router adapters over the host.
//!
//! Driving adapters that build an axum `Router` using only the host's public API:
//! `files_router` over [`awaken_runtime_host::SharedHost`]'s file/harvest methods,
//! and the fully self-contained `models_router`. They live outside
//! `awaken-runtime-host` so the host stays the substrate and this wire surface stays
//! a thin adapter — the composition layer (`awaken-server`) mounts them onto the app.
//!
//! Not every per-plane router qualifies: `memory_stores_router` and `skills_router`
//! reach into the host's internal `MemoryStores` / `SkillCatalog` subsystems (private
//! fields, `awaken_skill_store`), so they are the HTTP face of those subsystems and
//! stay in `awaken-runtime-host` rather than leak the subsystems' method surface.

mod files;
mod models;

pub use files::files_router;
pub use models::{ModelEntry, default_models, models_router};
