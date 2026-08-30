use super::*;



impl SharedHost {
    /// Resolve the one immutable publication selected for a Session and enforce
    /// its projected Agent/backend fences. Context construction and cold
    /// environment adoption share this boundary so provider selection cannot
    /// drift from execution selection.
    pub(crate) fn resolve_session_publication(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<
        (
            String,
            String,
            Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
        ),
        HostError,
    > {
        let workspace = self.thread_workspace(thread);
        let projected_agent = self.thread_agent_projection(thread);
        if let (Some(asserted), Some(projected)) = (agent, projected_agent.as_deref())
            && asserted != projected
        {
            return Err(HostError::internal(format!(
                "session Agent projection `{projected}` does not match requested Agent `{asserted}`"
            )));
        }
        let selected_agent = agent.or(projected_agent.as_deref()).unwrap_or("assistant");
        let baseline = self
            .session_slots
            .read(thread, |slot| slot.baseline.clone())
            .flatten();
        let frozen_revision = baseline
            .as_ref()
            .and_then(|baseline| baseline.agent_revision);
        let installed = published_snapshot.or_else(|| {
            self.agent_publications.as_ref().and_then(|source| {
                let agent_id =
                    awaken_runtime_contract::snapshot::AgentId(selected_agent.to_string());
                match frozen_revision {
                    Some(revision) => source.at_revision(&workspace, &agent_id, revision),
                    None => source.current(&workspace, &agent_id),
                }
            })
        });
        if installed.is_none() && frozen_revision.is_some() {
            return Err(HostError::internal(
                "frozen Session Agent publication is unavailable",
            ));
        }
        let installed = installed
            .map(|snapshot| {
                baseline.as_ref().map_or(Ok(snapshot.clone()), |baseline| {
                    awaken_session_contract::project_effective_agent_publication(
                        baseline.model_override.as_ref(),
                        &baseline.system_prompt,
                        &workspace,
                        snapshot,
                    )
                    .map_err(|error| HostError::internal(error.to_string()))
                })
            })
            .transpose()?;
        let published_backend_ref = installed
            .as_ref()
            .map(|snapshot| snapshot.resolved_spec.model_binding.backend_ref.clone());
        let projected_backend_ref = self
            .session_slots
            .read(thread, |slot| slot.backend_ref.clone())
            .flatten();
        if let (Some(published), Some(projected)) = (&published_backend_ref, &projected_backend_ref)
            && published != projected
        {
            return Err(HostError::internal(format!(
                "session backend projection `{projected}` does not match publication `{published}`"
            )));
        }
        if installed.is_none() && projected_backend_ref.is_some() {
            return Err(HostError::internal(
                "session backend projection has no immutable Agent publication",
            ));
        }
        Ok((workspace, selected_agent.to_string(), installed))
    }

    pub(crate) fn session_environment_provider(
        &self,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, HostError> {
        match provisioning {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. } => {
                self.backend_owned_session_provider.as_ref().ok_or_else(|| {
                    HostError::internal(
                        "BackendOwned provisioning requires a trusted-host Session provider",
                    )
                })
            }
            awaken_runtime_contract::resolved::ModelProvisioning::Provider { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::Remote { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor => {
                Ok(&self.session_provider)
            }
        }
    }

    pub(super) async fn bind_deferred_dispatch_before_publish(
        &self,
        thread: &str,
        binding: &str,
    ) -> Result<bool, HostError> {
        let claim = self
            .session_slots
            .read(thread, |slot| slot.dispatch_claim.clone())
            .flatten();
        let Some(claim) = claim else {
            return Ok(false);
        };
        let outcome = self
            .dispatch_store()
            .map_err(|error| HostError::internal(error.to_string()))?
            .bind_sandbox(&claim, binding)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        if !outcome.applied() {
            return Err(HostError::internal(
                "deferred sandbox binding was fenced by a replacement claim",
            ));
        }
        Ok(true)
    }

    /// One physical Session-environment creation path. CacheVolume preparation
    /// happens before either the authoritative or BackendOwned provider binds the
    /// mount, so eager warmup and first-use realization cannot diverge.
    pub(crate) async fn create_session_environment(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<crate::session_environment::SessionEnvironment, HostError> {
        self.validate_session_environment_capabilities(provider, spec)?;
        self.cache_volume_prewarmer
            .prepare_mounts(&spec.mounts)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        provider
            .create(spec)
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Validate one exact physical projection without creating it. Reservation
    /// uses this shared classifier when durable admission must remain
    /// side-effect free but the eventual execution cannot be deferred; actual
    /// creation reuses it immediately before effects.
    pub(super) fn validate_session_environment_capabilities(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<(), HostError> {
        let capabilities = provider.capabilities();
        if spec.isolation >= awaken_provisioning_contract::IsolationClass::Namespace
            && (!capabilities.tool_transparent || !capabilities.path_fidelity)
        {
            let required = awaken_provisioning_contract::SandboxRequirements::from_spec(spec, true);
            return Err(HostError::internal(format!(
                "Session environment cannot preserve one sandbox-absolute workspace path across Hand, Bash, Git, and Agent processes: required={required:?}, provider={capabilities:?}",
            )));
        }
        awaken_provisioning_contract::prepare_environment(spec, &capabilities)
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(())
    }

    /// Materialize the deferred environment at the first Sandbox-target tool.
    /// The same lifecycle mutex used by context construction guarantees one
    /// creator, and publication follows resource realization + durable binding.
    pub(crate) async fn ensure_session_environment_for_tool(
        &self,
        thread: &str,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let lifecycle = self
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.retry_unpublished_session_environment_cleanup(thread)
            .await?;
        if let Some(environment) = self
            .session_slots
            .read(thread, |slot| slot.environment_owner.resident())
            .flatten()
        {
            return Ok(environment);
        }
        let candidate = match self.prepared_session_environment(thread) {
            Some(candidate) => candidate,
            None => {
                if !self.session_environment_owner_is_vacant(thread) {
                    return Err(HostError::internal(format!(
                        "Session {thread} Environment transition must be recovered before tool use"
                    )));
                }
                let environment = Arc::new(
                    self.create_session_environment(
                        &self.session_provider,
                        &self.sandbox_spec(thread),
                    )
                    .await?,
                );
                self.begin_session_environment_preparation(thread, environment)?
            }
        };
        self.complete_prepared_session_environment(thread, &candidate, true)
            .await
    }

    /// Evict only the rebuildable runtime context while retaining the
    /// independently-owned Session environment and its live resource projection.
    /// Terminal cleanup remains the single responsibility of [`Self::end_session`].
    pub(crate) async fn evict_session_for_rebuild(&self, thread: &str) {
        self.session_slots
            .modify(thread, |slot| slot.runtime = None);
    }}
