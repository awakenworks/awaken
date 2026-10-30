//! Guard the Environment roles absent from this private Root fixture.
//!
//! The fixture commits a frozen Session declaration itself. It must not select
//! Environment revisions, enqueue Work, claim leases, or report invented queue
//! results. The existing ScenarioHost owns those real composition scenarios.

use awaken_executable_environment_contract::{
    EnvironmentImageBuildError, ExecutableEnvironmentRegistrationError, ResolvedSessionEnvironment,
    SessionEnvironmentProjectionSource,
};
use awaken_session_application_contract::{SessionWorkerAssignment, SessionWorkerLeases};
use awaken_session_types::{RunError, SessionId, SessionWorkLease};

pub(super) struct UnusedEnvironmentPorts;

#[async_trait::async_trait]
impl SessionEnvironmentProjectionSource for UnusedEnvironmentPorts {
    type Snapshot = awaken_session_configuration_contract::EnvironmentSnapshot;

    async fn get(
        &self,
        _environment_id: &str,
    ) -> Result<Option<awaken_environment_contract::EnvItem>, ExecutableEnvironmentRegistrationError>
    {
        panic!("private Root fixture must not read an Environment registration")
    }

    async fn resolve_current_for_session(
        &self,
        _environment_id: &str,
        _runtime: Option<&str>,
        _mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment<Self::Snapshot>>, EnvironmentImageBuildError>
    {
        panic!("private Root fixture must not resolve an Environment revision")
    }

    async fn resolve_exact_for_session(
        &self,
        _environment_id: &str,
        _revision: u64,
        _runtime: Option<&str>,
        _mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment<Self::Snapshot>>, EnvironmentImageBuildError>
    {
        panic!("private Root fixture must not resolve an Environment revision")
    }
}

#[async_trait::async_trait]
impl SessionWorkerAssignment for UnusedEnvironmentPorts {
    async fn enqueue_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
    ) -> Result<String, RunError> {
        panic!("private Root fixture must not enqueue Work")
    }

    async fn wake_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
    ) -> Result<String, RunError> {
        panic!("private Root fixture must not wake Work")
    }

    async fn retire_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
    ) -> Result<(), RunError> {
        panic!("private Root fixture must not retire Work")
    }
}

#[async_trait::async_trait]
impl SessionWorkerLeases for UnusedEnvironmentPorts {
    async fn acquire_session(
        &self,
        _environment_id: &str,
        _session_id: &SessionId,
        _worker_owner: &str,
        _now_ms: u64,
    ) -> Result<Option<SessionWorkLease>, RunError> {
        panic!("private Root fixture must not acquire a Work lease")
    }

    async fn release_session(&self, _lease: &SessionWorkLease) -> Result<bool, RunError> {
        panic!("private Root fixture must not release a Work lease")
    }

    async fn release_owner(&self, _worker_owner: &str) -> Result<usize, RunError> {
        panic!("private Root fixture must not release a Work owner")
    }
}
