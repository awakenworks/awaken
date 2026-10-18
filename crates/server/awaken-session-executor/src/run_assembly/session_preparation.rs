// @formal entrypoint session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal entrypoint session_run.reservation_never_executes
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
//! Installing a frozen Session projection into the process: the preparation
//! preflight, the typed installation of its facts, and completing preparation.

use awaken_session_types::RunError;

use super::RunAssembly;
use crate::managed_adapter_error::to_run_error;

impl RunAssembly {
    /// Unit-test receipt failure injection; product composition selects this
    /// ordinary port in HostDependencies before constructing the Host.
    #[cfg(test)]
    pub(crate) fn install_test_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        self.realization_executor
            .environments()
            .install_environment_binding_sink(sink);
    }

    /// Decide whether a complete projection needs execution preparation before
    /// any projection field is mutated. The caller holds the Session lifecycle
    /// mutex across this preflight, projection installation, and completion.
    pub(crate) fn session_preparation_needed(&self, thread: &str) -> Result<bool, RunError> {
        let active_projection = self
            .installed_sessions
            .read(thread, |slot| {
                (
                    slot.run
                        .context
                        .as_ref()
                        .map(|context| {
                            context
                                .foreground
                                .observe()
                                .map(|foreground| foreground.run)
                        })
                        .transpose()
                        .map(Option::flatten),
                    slot.inputs.baseline.is_some() || slot.run.session_dispatch,
                    slot.run
                        .context
                        .as_ref()
                        .is_some_and(|context| context.is_coordination_envelope()),
                )
            })
            .map_err(|error| {
                RunError::from(crate::realization_error::RealizationError::from(error))
            })?
            .unwrap_or((Ok(None), false, false));
        let (active_run, installed, coordination) = active_projection;
        let active_run = active_run
            .map_err(|poisoned| crate::managed_adapter_error::to_run_error(poisoned.into()))?;
        match (active_run, installed, coordination) {
            // A coordination envelope owns no execution projection. Even when
            // it wins the cache race before a Worker claim, the Worker must run
            // the canonical realization driver and install its lease before
            // rebuilding an executable context.
            (_, _, true) => Ok(true),
            (Some(_), true, false) => Ok(false),
            (Some(_), false, false) => Err(RunError::internal(
                "cannot install a frozen Session projection while its Runtime is active",
            )),
            (None, _, false) => Ok(true),
        }
    }

    /// Publish only execution-preparation effects that are not already part of
    /// the complete frozen projection.
    pub(crate) fn complete_session_preparation(&self, thread: &str) -> Result<(), RunError> {
        let refused =
            |error| RunError::from(crate::realization_error::RealizationError::from(error));
        self.installed_sessions
            .install_with(thread, |slot| {
                slot.run.context = None;
                slot.run.session_dispatch = true;
            })
            .map_err(refused)?;
        crate::lazy_sandbox::bind_deferred_executor(self, thread).map_err(refused)
    }

    /// Install all immutable facts selected by one typed mode through the sole
    /// complete projection owner.
    pub(crate) async fn install_projection_facts(
        &self,
        thread: &str,
        projection: &awaken_session_realization_contract::FrozenSessionProjection,
        mode: &awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        match mode {
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch => {
                self.install_dispatch_frozen_session_projection(thread, projection.clone())
                    .await
            }
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                prepare_session,
                lease,
            } => {
                self.install_frozen_session_projection(
                    thread,
                    projection.clone(),
                    None,
                    *prepare_session,
                    Some(lease.clone()),
                )
                .await
            }
        }
        .map_err(to_run_error)
    }
    /// Unit-test fixture for low-level Runtime behavior that does not construct
    /// a persisted Session aggregate. Production paths use the complete port.
    #[cfg(test)]
    pub(crate) async fn install_test_session_init(
        &self,
        thread: &str,
        init: awaken_session_configuration_contract::SessionInit,
    ) -> Result<(), RunError> {
        let realizations = self.installed_sessions.realizations();
        self.installed_sessions
            .install(thread)
            .expect("installed Sessions");
        let _lifecycle = realizations.lock_lifecycle(thread).await?;
        if !self.session_preparation_needed(thread)? {
            return Ok(());
        }
        let _resource_projection = realizations.lock_resource_projection(thread).await?;
        self.project_session_init(thread, &init)
            .map_err(to_run_error)?;
        self.realization_executor
            .staging()
            .stage_resource_manifest(
                self.realization_executor.peers(),
                thread,
                &init.workspace_id,
                init.resource_revision,
                &init.resources,
                None,
            )
            .await?;
        self.complete_session_preparation(thread)?;
        Ok(())
    }
}
