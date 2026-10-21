//! Shared Session application assembly with the required real Web compiler.
use super::*;

pub(crate) fn application(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    environments: impl Into<SessionEnvironments>,
) -> SessionApplication {
    application_with_configuration(repo, environments, Default::default())
}

pub(super) fn application_with_runtime(
    runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    environments: impl Into<SessionEnvironments>,
) -> SessionApplication {
    configured_application(
        runtime,
        SessionPersistence::new(repo, system_clock()),
        environments,
        Default::default(),
        None,
    )
}

pub(super) fn configured_application(
    runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
    persistence: SessionPersistence,
    environments: impl Into<SessionEnvironments>,
    configuration: SessionApplicationConfiguration,
    resources: Option<SessionResources>,
) -> SessionApplication {
    let builder = SessionApplicationBuilder::fixture(
        runtime,
        Arc::new(NoopMcpRealizer),
        persistence,
        environments,
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(configuration);
    match resources {
        Some(resources) => builder.resources(resources),
        None => builder,
    }
    .build()
    .expect("valid fixture Session application")
}

pub(super) fn application_with_configuration(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    environments: impl Into<SessionEnvironments>,
    configuration: SessionApplicationConfiguration,
) -> SessionApplication {
    configured_application(
        Arc::new(NoopRuntime),
        SessionPersistence::new(repo, system_clock()),
        environments,
        configuration,
        None,
    )
}

pub(super) fn application_with_persistence(
    runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
    persistence: SessionPersistence,
) -> SessionApplication {
    configured_application(
        runtime,
        persistence,
        Arc::new(RecordingEnvironmentSource::default()),
        Default::default(),
        None,
    )
}

pub(super) fn configured_admission_application(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    runtime: Arc<RecordingAgentAdmissionRuntime>,
) -> SessionApplication {
    let mut app = application_with_runtime(
        runtime,
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    app.set_config_source(Arc::new(CoordinatedAgentSource));
    app
}
