//! Coordinator application for durable Environment image realization.

mod application;
mod in_memory;

pub use application::{
    BuildAwareExecutableEnvironmentRegistrar, EnvironmentImageBuildCoordinator,
    EnvironmentImageBuildPolicy,
};
pub use in_memory::InMemoryEnvironmentImageBuildStore;

#[must_use]
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}
