//! Process-local composition of the Managed adapter around one shared Host.

use super::*;

impl ManagedHost {
    pub fn new(host: Arc<SharedHost>) -> Self {
        Self {
            host,
            credentials: None,
            credential_refresh_factory: None,
            resource_validator: None,
            repository_binding_verifier: None,
            repository_publication_binding_verifier: None,
            mcp_realizer: None,
        }
    }

    /// Connect the fixed Runtime coordination tools to the canonical Session
    /// application after composition has wrapped that application in an `Arc`.
    /// The Host retains only a weak application port, avoiding an ownership
    /// cycle (`SessionApplication -> ManagedHost -> SharedHost`).
    pub fn install_agent_coordination_application(
        &self,
        application: std::sync::Weak<dyn awaken_session_contract::SessionAgentCoordination>,
    ) -> Result<(), AgentCoordinationInstallError> {
        let mut installed = self
            .host
            .agent_coordination
            .write()
            .expect("Agent coordination application lock poisoned");
        if installed.is_some() {
            return Err(AgentCoordinationInstallError::AlreadyInstalled);
        }
        *installed = Some(application);
        Ok(())
    }

    /// Connect BackgroundTask completion to the canonical Session Run
    /// application after composition has wrapped that application in an `Arc`.
    ///
    /// This is an executable weak edge only. It owns no notification record,
    /// BackgroundTask state, Run identity mapping, or retry ledger.
    pub fn install_session_background_run_application(
        &self,
        application: std::sync::Weak<dyn awaken_session_contract::SessionRunBackgroundApplication>,
    ) -> Result<(), SessionRunBackgroundInstallError> {
        let mut installed = self
            .host
            .session_background_runs
            .write()
            .expect("Session background Run application lock poisoned");
        if installed.is_some() {
            return Err(SessionRunBackgroundInstallError::AlreadyInstalled);
        }
        *installed = Some(application);
        Ok(())
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
            .dispatch_session_runtime
            .write()
            .expect("dispatch Session Runtime lock poisoned") = Some(DispatchSessionRuntime {
            host: Arc::downgrade(&self.host),
            credentials: self.credentials.clone(),
            credential_refresh_factory: self.credential_refresh_factory.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            repository_publication_binding_verifier: self
                .repository_publication_binding_verifier
                .clone(),
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
                )
            })
            .unwrap_or((None, false));
        match active_projection {
            (Some(_), true) => Ok(false),
            (Some(_), false) => Err(RunError::internal(
                "cannot install a frozen Session projection while its Runtime is active",
            )),
            (None, _) => Ok(true),
        }
    }

    /// Publish only execution-preparation effects that are not already part of
    /// the complete frozen projection.
    pub(super) fn complete_session_preparation(
        &self,
        thread: &str,
        environment: &awaken_session_contract::EnvironmentSnapshot,
    ) {
        self.host.session_slots.update(thread, |slot| {
            slot.runtime = None;
            slot.session_dispatch = true;
        });
        if environment.sandbox_provisioning
            == awaken_session_contract::SandboxProvisioning::OnToolUse
        {
            let executor: Arc<dyn awaken_runtime_contract::tool::ToolExecutor> =
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
        projection: &awaken_session_contract::FrozenSessionProjection,
        mode: &awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        let realization_lease = mode.realization_lease().cloned();
        match mode {
            awaken_session_contract::SessionProjectionInstallMode::Dispatch => {
                self.host
                    .install_dispatch_frozen_session_projection(thread, projection.clone())
                    .await
            }
            awaken_session_contract::SessionProjectionInstallMode::Realization {
                prepare_session,
                ..
            } => {
                self.host
                    .install_frozen_session_projection(
                        thread,
                        projection.clone(),
                        None,
                        *prepare_session,
                        realization_lease,
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
        init: awaken_session_contract::SessionInit,
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
