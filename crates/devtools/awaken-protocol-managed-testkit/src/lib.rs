//! Volatile store fixtures for Managed protocol tests.
//!
//! Consumers: the Managed adapter's own tests (as a dev-dependency), its
//! integration tests, the Coordinator's tests and the scenario host. The
//! adapter itself depends on no concrete store: every fixture here reaches it
//! behind the port it consumes, so its `test-support` feature no longer drags
//! four stores and a catalog into every crate that enables it.

use std::sync::Arc;

use awaken_environment_application::EnvironmentApplication;
use awaken_environment_execution_application::{
    CoordinatorEnvironmentRegistrar, EnvironmentExecutionApplication,
};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;
use awaken_protocol_managed::EnvironmentAuthoringState;
/// Re-exported so fixture users import the state together with the
/// [`ManagedStateFixture`] constructors that build it.
pub use awaken_protocol_managed::ManagedState;
use awaken_provisioning_contract::SandboxExecutionPolicyStore;
use awaken_session_repository_contract::SessionStore;
use awaken_session_execution_contract::{CompleteSessionRuntime, McpAttachmentRealizer};
use awaken_session_work_contract::WorkQueue;

/// A fresh in-memory durable Session repository.
#[must_use]
pub async fn ephemeral_session_repository() -> Arc<dyn SessionStore> {
    Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("open ephemeral managed Session repository"),
    )
}

/// The split Environment owners over one volatile WorkQueue and catalog: the
/// authoring application with its sandbox policies, and the execution
/// projection sharing the same queue.
pub struct EnvironmentFixture {
    pub application: Arc<EnvironmentApplication>,
    pub sandbox_policies: Arc<dyn SandboxExecutionPolicyStore>,
    pub execution: Arc<EnvironmentExecutionApplication>,
}

/// Build the Environment owners with the built-in local Environment seeded.
#[must_use]
///
/// `clock` is the test's wall clock; the Work queue and the Environment
/// registry both stamp with it.
pub fn environment_fixture(clock: Arc<dyn awaken_time::WallClock>) -> EnvironmentFixture {
    let work: Arc<dyn WorkQueue> =
        Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock.clone()));
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
    let sandbox_policies: Arc<dyn SandboxExecutionPolicyStore> =
        Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
    let application = Arc::new(EnvironmentApplication::new(
        Arc::new(awaken_env_store::InMemoryEnvRegistry::new(clock)),
        registrar,
        Some(sandbox_policies.clone()),
    ));
    EnvironmentFixture {
        application,
        sandbox_policies,
        execution: Arc::new(EnvironmentExecutionApplication::new(work, catalog)),
    }
}

/// The Managed Environment authoring surface and execution projection.
#[must_use]
pub fn environment_components(
    clock: Arc<dyn awaken_time::WallClock>,
) -> (
    Arc<EnvironmentAuthoringState>,
    Arc<EnvironmentExecutionApplication>,
) {
    let fixture = environment_fixture(clock);
    (
        Arc::new(EnvironmentAuthoringState::new(
            fixture.application,
            fixture.sandbox_policies,
        )),
        fixture.execution,
    )
}

/// Volatile `ManagedState` constructors for tests outside the adapter crate.
/// Bring it into scope (`use ... as _`) and `ManagedState::new(runtime).await` reads
/// as it did when the adapter built its own stores.
// Test fixtures await these directly in their own test body; no caller needs
// a `Send` bound on the returned future.
#[allow(async_fn_in_trait)]
pub trait ManagedStateFixture: Sized {
    /// A Managed state over `runtime` with no MCP realization.
    async fn new(runtime: impl CompleteSessionRuntime + 'static) -> Self;

    /// A Managed state whose one fixture object is both the Runtime and the
    /// MCP attachment realizer.
    async fn new_with_mcp<R>(runtime: R) -> Self
    where
        R: CompleteSessionRuntime + McpAttachmentRealizer + 'static;
}

impl ManagedStateFixture for ManagedState {
    async fn new(runtime: impl CompleteSessionRuntime + 'static) -> Self {
        // A test's fixture graph: it runs on the host clock throughout.
        let clock = awaken_time::system_clock();
        ManagedState::from_fixture_parts(
            Arc::new(runtime),
            None,
            ephemeral_session_repository().await,
            environment_fixture(clock.clone()).execution,
            clock,
        )
    }

    async fn new_with_mcp<R>(runtime: R) -> Self
    where
        R: CompleteSessionRuntime + McpAttachmentRealizer + 'static,
    {
        let runtime = Arc::new(runtime);
        let clock = awaken_time::system_clock();
        ManagedState::from_fixture_parts(
            runtime.clone(),
            Some(runtime),
            ephemeral_session_repository().await,
            environment_fixture(clock.clone()).execution,
            clock,
        )
    }
}
