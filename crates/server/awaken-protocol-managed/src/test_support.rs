//! Explicit test-only composition for the split Environment owners.

use std::sync::Arc;

use awaken_environment_execution_application::{
    CoordinatorEnvironmentRegistrar, EnvironmentExecutionApplication,
};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;

use crate::EnvironmentAuthoringState;

#[must_use]
pub fn environment_components() -> (
    Arc<EnvironmentAuthoringState>,
    Arc<EnvironmentExecutionApplication>,
) {
    let work: Arc<dyn awaken_session_contract::work_queue::WorkQueue> =
        Arc::new(awaken_work_store::InMemoryWorkQueue::new());
    let catalog =
        Arc::new(awaken_executable_environment_catalog::ExecutableEnvironmentCatalog::new());
    catalog
        .install_seed(awaken_environment_application::default_environment_registration())
        .expect("install built-in local Environment");
    let registrar: Arc<dyn ExecutableEnvironmentRegistrar> =
        Arc::new(CoordinatorEnvironmentRegistrar::new(
            Arc::new(
                awaken_executable_environment_catalog::LocalExecutableEnvironmentRegistrar::new(
                    catalog.clone(),
                ),
            ),
            work.clone(),
        ));
    (
        Arc::new(EnvironmentAuthoringState::new(
            Arc::new(awaken_env_store::InMemoryEnvRegistry::new()),
            Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default()),
            registrar,
        )),
        Arc::new(EnvironmentExecutionApplication::new(work, catalog)),
    )
}
