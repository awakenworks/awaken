//! [`InstalledSessionLowering`]: the frozen inputs of this executor's
//! installed Sessions, as coordinated Thread admission lowers over them.

use std::sync::Arc;

use awaken_dispatch_execution::session_dispatch::DispatchPreparation;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation};
use awaken_session_run_delivery::{ChildRunInputs, SessionLowering};
use awaken_session_types::RunError;

use crate::SessionExecutionError;
use crate::session_execution_error::to_run_error;

/// The installed Sessions of one executor and the contexts it builds over them.
pub(crate) struct InstalledSessionLowering {
    pub(crate) run_assembly: Arc<crate::RunAssembly>,
}

#[async_trait::async_trait]
impl SessionLowering for InstalledSessionLowering {
    fn child_run_inputs(&self, session: &str) -> Result<ChildRunInputs, RunError> {
        let (baseline, tools, manifest, coordinator_publication) = self
            .run_assembly
            .installed_sessions
            .read(session, |installed| {
                (
                    installed.inputs.baseline.clone(),
                    installed.inputs.tools.clone(),
                    // Coordinated children share the exact generation selected
                    // for root and extension dispatch. Selection stays inside
                    // this one installed Session snapshot; child admission neither publishes
                    // nor reconstructs physical active truth.
                    installed
                        .realization
                        .staging()
                        .run_manifest(installed.run.session_dispatch),
                    installed.inputs.published_snapshot.clone(),
                )
            })
            .map_err(|error| to_run_error(SessionExecutionError::from(error)))?
            .ok_or_else(|| RunError::bad_request("Session runtime projection is unavailable"))?;
        Ok(ChildRunInputs {
            baseline: baseline.ok_or_else(|| {
                RunError::bad_request("Session frozen baseline is unavailable for coordination")
            })?,
            tools,
            manifest: manifest.ok_or_else(|| {
                RunError::bad_request("Session resource manifest is unavailable for coordination")
            })?,
            coordinator_publication,
        })
    }

    async fn continuation_snapshot(
        &self,
        session: &str,
    ) -> Result<ExecutableAgentSnapshot, RunError> {
        Ok(self
            .run_assembly
            .ctx_for_session_coordination(session, None, None)
            .await
            .map_err(to_run_error)?
            .config
            .clone())
    }

    async fn resume_snapshot(&self, session: &str) -> Result<ExecutableAgentSnapshot, RunError> {
        Ok(self
            .run_assembly
            .ctx_for(session, None)
            .await
            .map_err(to_run_error)?
            .config
            .clone())
    }

    fn model_ref_override(&self, session: &str) -> Result<Option<String>, RunError> {
        self.run_assembly
            .capabilities
            .agent
            .inference_routing
            .override_for(session)
            .map_err(RunError::from)
    }

    fn prepare(&self, activation: RunActivation) -> DispatchPreparation<'_> {
        self.run_assembly.prepare_dispatch(activation)
    }
}
