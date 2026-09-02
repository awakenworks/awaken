//! Coordinator application for durable Environment image realization.

mod application;
mod durable;
#[cfg(any(test, feature = "test-support"))]
mod in_memory;
mod postgres;
mod schema;
mod sqlite;

pub use application::{
    BuildAwareExecutableEnvironmentRegistrar, EnvironmentImageBuildCoordinator,
    EnvironmentImageBuildPolicy,
};
#[cfg(any(test, feature = "test-support"))]
pub use in_memory::InMemoryEnvironmentImageBuildStore;
pub use postgres::{
    connect_existing_postgres_environment_image_build_store,
    connect_postgres_environment_image_build_store,
};
pub use schema::environment_image_build_bundle;
#[cfg(any(test, feature = "test-support"))]
pub use sqlite::open_in_memory_environment_image_build_store;
pub use sqlite::open_sqlite_environment_image_build_store;

#[cfg(test)]
pub(crate) fn test_demand() -> awaken_environment_realization_contract::EnvironmentImageBuildDemand
{
    use awaken_environment_contract::{
        EnvItem, EnvironmentConfig, EnvironmentPackages, EnvironmentRevision,
    };
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistration;

    let registration = ExecutableEnvironmentRegistration::new(
        EnvItem {
            id: "env-browser".into(),
            revision: EnvironmentRevision(1),
            name: "browser".into(),
            description: None,
            metadata: Default::default(),
            scope: None,
            config: EnvironmentConfig::Cloud {
                networking: Default::default(),
                packages: EnvironmentPackages {
                    npm: vec!["@playwright/mcp@latest".into()],
                    ..Default::default()
                },
            },
            sandbox_policy: None,
            archived_at: None,
        },
        None,
    );
    awaken_environment_realization_contract::EnvironmentImageBuildDemand::from_registration(
        &registration,
        "registry/base:1",
    )
    .expect("test Environment requires an immutable image build")
}

#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
