// @formal entrypoint session_resources.install_replacement_requires_newer_or_unattempted_authority_amendment
// @formal entrypoint session_run.reservation_never_executes
// @formal evidence session_run.execution_requires_dispatch_work_and_realization_owner
//! Process-local composition of the Managed adapter around one shared Host.

use super::*;

impl ManagedHost {
    /// Read side of this Host's sole durable dispatch authority. Composition
    /// supplies it to the Session application without exposing a second queue.
    pub fn dispatch_observation_source(
        &self,
    ) -> Result<
        Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>,
        AgentCoordinationInstallError,
    > {
        self.host
            .dispatch_store()
            .map(|source| source as Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>)
            .map_err(|_| AgentCoordinationInstallError::DispatchObservationUnavailable)
    }

    pub fn new(host: Arc<SharedHost>) -> Self {
        Self {
            host,
            credentials: None,
            credential_refresh_factory: None,
            platform_mcp_egress: None,
            resource_validator: None,
            repository_binding_verifier: None,
            mcp_realizer: None,
        }
    }

    /// Install the fully configured Managed adapter used by durable dispatch.
    ///
    /// Call this once at the process startup after all `with_*` configuration
    /// has been applied. Construction and configuration are deliberately free
    /// of shared-host side effects, so a partially configured adapter can never
    /// become visible to a concurrently claimed Run.
    #[must_use]
    pub fn install_dispatch_session_runtime(self) -> Self {
        *self
            .host
            .dispatch
            .dispatch_session_runtime
            .write()
            .expect("dispatch Session Runtime lock poisoned") = Some(DispatchSessionRuntime {
            host: Arc::downgrade(&self.host),
            credentials: self.credentials.clone(),
            credential_refresh_factory: self.credential_refresh_factory.clone(),
            platform_mcp_egress: self.platform_mcp_egress.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            mcp_realizer: self.mcp_realizer.clone(),
        });
        self
    }

    /// Decide whether a complete projection needs execution preparation before
    /// any projection field is mutated. The caller holds the Session lifecycle
    /// mutex across this preflight, projection installation, and completion.
    pub(super) fn session_preparation_needed(&self, thread: &str) -> Result<bool, RunError> {
        let active_projection = self
            .host
            .session_slots
            .read(thread, |slot| {
                (
                    slot.runtime.as_ref().and_then(|context| {
                        context
                            .active_run
                            .lock()
                            .expect("active run mutex poisoned")
                            .clone()
                    }),
                    slot.baseline.is_some() || slot.session_dispatch,
                    slot.runtime
                        .as_ref()
                        .is_some_and(|context| context.is_coordination_envelope()),
                )
            })
            .unwrap_or((None, false, false));
        match active_projection {
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
    pub(super) fn complete_session_preparation(
        &self,
        thread: &str,
        environment: &awaken_session_configuration_contract::EnvironmentSnapshot,
    ) {
        self.host.session_slots.update(thread, |slot| {
            slot.runtime = None;
            slot.session_dispatch = true;
        });
        if environment.sandbox_provisioning
            == awaken_session_configuration_contract::SandboxProvisioning::OnToolUse
        {
            let executor: Arc<dyn awaken_tool_contract::tool::ToolExecutor> =
                Arc::new(crate::lazy_sandbox::DeferredSandboxExecutor::new(
                    Arc::downgrade(&self.host),
                    thread,
                ));
            self.host
                .session_slots
                .update(thread, |slot| slot.deferred_executor = Some(executor));
        }
    }

    /// Install all immutable facts selected by one typed mode through the sole
    /// complete projection owner.
    pub(super) async fn install_projection_facts(
        &self,
        thread: &str,
        projection: &awaken_session_realization_contract::FrozenSessionProjection,
        mode: &awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        match mode {
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch => {
                self.host
                    .install_dispatch_frozen_session_projection(thread, projection.clone())
                    .await
            }
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                prepare_session,
                lease,
            } => {
                self.host
                    .install_frozen_session_projection(
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
    pub(super) async fn install_test_session_init(
        &self,
        thread: &str,
        init: awaken_session_configuration_contract::SessionInit,
    ) -> Result<(), RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        if !self.session_preparation_needed(thread)? {
            return Ok(());
        }
        let resource_projection = self
            .host
            .session_slots
            .update(thread, |slot| slot.resource_projection.clone());
        let _resource_projection = resource_projection.lock().await;
        self.host
            .project_session_init(thread, &init)
            .map_err(to_run_error)?;
        self.stage_resource_manifest(
            thread,
            &init.workspace_id,
            init.resource_revision,
            &init.resources,
            None,
        )
        .await?;
        self.complete_session_preparation(thread, &init.environment);
        Ok(())
    }
}
