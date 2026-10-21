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
    configured_application(runtime, repo, environments, Default::default())
}

pub(super) fn configured_application(
    runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    environments: impl Into<SessionEnvironments>,
    configuration: SessionApplicationConfiguration,
) -> SessionApplication {
    SessionApplicationBuilder::fixture(
        runtime,
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repo, system_clock()),
        environments,
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(configuration)
    .build()
    .expect("valid fixture Session application")
}

pub(super) fn application_with_configuration(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    environments: impl Into<SessionEnvironments>,
    configuration: SessionApplicationConfiguration,
) -> SessionApplication {
    SessionApplicationBuilder::fixture(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repo, system_clock()),
        environments,
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(configuration)
    .build()
    .expect("valid fixture Session application")
}

pub(super) fn application_with_persistence(
    runtime: Arc<dyn awaken_session_execution_contract::CompleteSessionRuntime>,
    persistence: SessionPersistence,
) -> SessionApplication {
    SessionApplicationBuilder::fixture(
        runtime,
        Arc::new(NoopMcpRealizer),
        persistence,
        Arc::new(RecordingEnvironmentSource::default()),
        Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .expect("valid fixture Store/Clock composition")
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
