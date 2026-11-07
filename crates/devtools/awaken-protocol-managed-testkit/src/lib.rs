//! Volatile store fixtures and protocol test doubles for Managed protocol tests.
//!
//! Consumers: the Managed adapter's own tests (as a dev-dependency), its
//! integration tests, the Coordinator's tests and the scenario host. The
//! adapter itself depends on no concrete store: every fixture here reaches it
//! behind the port it consumes, so its `test-support` feature no longer drags
//! four stores and a catalog into every crate that enables it.

use std::sync::Arc;

mod committed_audit;
mod hosted_doubles;
mod managed_event_observer;
mod projection_completion;
mod protocol_doubles;
mod read_control;
mod rehydrate_fake;
mod rehydrate_fake_roles;
mod session_fixtures;

pub use committed_audit::{
    AwaitTargets, StateLog, audited_lifecycle_records, fixture_state_log, fixture_thread_audit,
    fixture_usage_command, lifecycle_audit_pausing_on, lifecycle_audit_records,
    remember_ticket_await_target, tool_call_target,
};
pub use hosted_doubles::EmptyManagedTunnelApplication;
pub use managed_event_observer::{
    DurableFact, wait_for_delivered_lifecycle_facts, wait_for_session_events,
};
pub use projection_completion::{
    complete_session_projection_init, complete_terminal_cleanup_disposal,
    complete_terminal_cleanup_preparation, observe_realization_request,
};
pub use protocol_doubles::CoordinatedRuntimeFake;
pub use read_control::ReadControl;
pub use rehydrate_fake::{
    ChildCommitAfterHistorySnapshot, RehydrateFake, RestoredRuntime, create_session_fixture,
    ephemeral_session_repo,
};
pub use session_fixtures::{
    RecordingProjectionRefresh, RolloutCredentialSource, paired_projection_refresh,
    persisted_with_mcp_source, preparing_session_with_mcp_source, sample_inputs, sample_persisted,
};

use awaken_environment_application::EnvironmentApplication;
use awaken_environment_application::execution::{
    CoordinatorEnvironmentRegistrar, EnvironmentExecutionApplication,
};
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrar;
/// Re-exported so fixture users import the state together with the
/// [`ManagedStateFixture`] constructors that build it.
pub use awaken_protocol_managed::ManagedState;
use awaken_provisioning_contract::SandboxExecutionPolicyStore;
use awaken_session_application::SessionStore;
use awaken_session_execution_contract::{CompleteSessionRuntime, McpAttachmentRealizer};

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
    let work = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock.clone()));
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
        sandbox_policies.clone(),
    ));
    EnvironmentFixture {
        application,
        sandbox_policies,
        execution: Arc::new(EnvironmentExecutionApplication::new(work, catalog)),
    }
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

    /// A Managed state over executor roles selected from separate owners.
    async fn with_roles(
        roles: awaken_session_execution_contract::SessionExecutionRoleSelection,
    ) -> Self;
}

impl ManagedStateFixture for ManagedState {
    async fn new(runtime: impl CompleteSessionRuntime + 'static) -> Self {
        // A test's fixture graph: it runs on the host clock throughout.
        let clock = awaken_time::system_clock();
        managed_state_from_parts(
            Arc::new(runtime),
            None,
            ephemeral_session_repository().await,
            environment_fixture(clock.clone()).execution,
            clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    }

    async fn new_with_mcp<R>(runtime: R) -> Self
    where
        R: CompleteSessionRuntime + McpAttachmentRealizer + 'static,
    {
        let runtime = Arc::new(runtime);
        let clock = awaken_time::system_clock();
        managed_state_from_parts(
            runtime.clone(),
            Some(runtime),
            ephemeral_session_repository().await,
            environment_fixture(clock.clone()).execution,
            clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    }

    async fn with_roles(
        roles: awaken_session_execution_contract::SessionExecutionRoleSelection,
    ) -> Self {
        let clock = awaken_time::system_clock();
        ManagedState::from_application(
            session_application_from_parts(
                roles,
                ephemeral_session_repository().await,
                environment_fixture(clock.clone()).execution,
                clock.clone(),
                {
                    let probe = std::sync::Arc::new(
                        awaken_session_application::ExecutionObservationProbe::default(),
                    );
                    awaken_session_application::SessionExecutionAdmission::new(
                        std::sync::Arc::new(awaken_dispatch::MemoryDispatchStore::new(
                            clock.clone(),
                        )),
                        probe.clone(),
                        probe,
                    )
                },
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            ),
            awaken_data_governance_contract::CapturePolicy::default(),
        )
    }
}

/// Explicit bootstrap of a test Session application over supplied ports.
pub fn managed_state_from_parts(
    runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
    mcp: Option<Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>>,
    sessions: Arc<dyn awaken_session_application::SessionStore>,
    environments: Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    clock: Arc<dyn awaken_time::WallClock>,
    compiler: Arc<dyn awaken_tool_contract::ToolConfigurationCompiler>,
) -> ManagedState {
    ManagedState::from_application(
        session_application_from_parts(
            awaken_session_execution_contract::SessionExecutionRoles::colocated(
                runtime,
                mcp.unwrap_or_else(|| Arc::new(awaken_session_execution_contract::NeutralRuntime)),
            ),
            sessions,
            environments.clone(),
            clock.clone(),
            {
                let probe = std::sync::Arc::new(
                    awaken_session_application::ExecutionObservationProbe::default(),
                );
                awaken_session_application::SessionExecutionAdmission::new(
                    std::sync::Arc::new(awaken_dispatch::MemoryDispatchStore::new(clock.clone())),
                    probe.clone(),
                    probe,
                )
            },
            compiler,
        ),
        awaken_data_governance_contract::CapturePolicy::default(),
    )
}

/// Build only the application so a protocol unit-test crate retains its own
/// ManagedState type rather than importing the non-test copy through testkit.
/// The caller explicitly selects its complete execution observations before
/// publishing this App; this fixture supplies no late-install or Registry fallback.
pub fn session_application_from_parts(
    runtime: impl Into<awaken_session_execution_contract::SessionExecutionRoles>,
    sessions: Arc<dyn awaken_session_application::SessionStore>,
    environments: Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    clock: Arc<dyn awaken_time::WallClock>,
    execution: awaken_session_application::SessionExecutionAdmission,
    compiler: Arc<dyn awaken_tool_contract::ToolConfigurationCompiler>,
) -> Arc<awaken_session_application::SessionApplication> {
    Arc::new(
        awaken_session_application::SessionApplicationBuilder::new(
            runtime,
            awaken_session_application::SessionPersistence::new(sessions, clock),
            environments,
            execution,
            compiler,
        )
        .build()
        .expect("valid fixture Session application"),
    )
}
