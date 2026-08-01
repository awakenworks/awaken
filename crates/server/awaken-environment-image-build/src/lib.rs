//! Coordinator application for durable Environment image realization.

mod application;
mod durable;
mod in_memory;
mod postgres;
mod schema;
mod sqlite;

pub use application::{
    BuildAwareExecutableEnvironmentRegistrar, EnvironmentImageBuildCoordinator,
    EnvironmentImageBuildPolicy,
};
pub use in_memory::InMemoryEnvironmentImageBuildStore;
pub use postgres::{
    connect_existing_postgres_environment_image_build_store,
    connect_postgres_environment_image_build_store,
};
pub use schema::environment_image_build_bundle;
pub use sqlite::{
    open_in_memory_environment_image_build_store, open_sqlite_environment_image_build_store,
};

#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
