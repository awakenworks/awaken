//! Guard the Environment roles absent from this private Root fixture.
//!
//! The fixture commits a frozen Session declaration itself. It must not select
//! Environment revisions, wake or retire Work, claim leases, or report invented
//! queue results. The existing ScenarioHost owns those real composition
//! scenarios. A Worker-placed Session re-projects its Work when its Resource
//! manifest changes, so the fixture records each enqueue it is asked for and
//! grants nothing else: no queue, claim or lease follows from a record.

use awaken_executable_environment_contract::{
    EnvironmentImageBuildError, ExecutableEnvironmentRegistrationError, ResolvedSessionEnvironment,
    SessionEnvironmentProjectionSource,
};
use awaken_runtime_contract::runs::RunError;
use awaken_session_application_contract::{SessionWorkerAssignment, SessionWorkerLeases};
use awaken_session_types::{SessionId, SessionWorkLease};

#[derive(Default)]
pub(super) struct UnusedEnvironmentPorts {
    enqueued: std::sync::Mutex<Vec<SessionId>>,
}

impl UnusedEnvironmentPorts {
    /// Sessions whose Work projection was requested, in request order.
    pub(super) fn enqueued(&self) -> Vec<SessionId> {
        self.enqueued.lock().unwrap().clone()
    }
}

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
        session_id: &SessionId,
    ) -> Result<String, RunError> {
        self.enqueued.lock().unwrap().push(session_id.clone());
        Ok(format!("fixture-work:{session_id}"))
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
