//! Narrow read/snapshot fixtures use the original Session and Resource owners.
use std::sync::Arc;

pub(crate) struct SessionReadFixture {
    pub sessions: Arc<awaken_session_application::SessionApplication>,
    pub environments:
        Arc<awaken_environment_application::execution::EnvironmentExecutionApplication>,
    pub dispatch: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
}

pub(crate) async fn session_read_fixture(
    catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> SessionReadFixture {
    let runtime = Arc::new(awaken_protocol_managed_testkit::RehydrateFake::default());
    let environments =
        awaken_protocol_managed_testkit::environment_fixture(clock.clone()).execution;
    let dispatch = Arc::new(
        awaken_dispatch_erasure::AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
            awaken_dispatch::MemoryDispatchStore::new(clock.clone()),
        )),
    );
    let sessions = Arc::new(
        awaken_session_application::SessionApplicationBuilder::new(
            awaken_session_execution_contract::SessionExecutionRoles::colocated(
                runtime.clone(),
                runtime,
            ),
            awaken_session_application::SessionPersistence::new(
                awaken_protocol_managed_testkit::ephemeral_session_repository().await,
                clock,
            ),
            environments.clone(),
            {
                let probe = std::sync::Arc::new(
                    awaken_session_application::ExecutionObservationProbe::default(),
                );
                awaken_session_application::SessionExecutionAdmission::new(
                    dispatch.clone(),
                    probe.clone(),
                    probe,
                )
            },
            Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .resources(awaken_session_application::SessionResources::catalog(
            catalog,
        ))
        .build()
        .expect("valid read-only Session fixture"),
    );
    SessionReadFixture {
        sessions,
        environments,
        dispatch,
    }
}
