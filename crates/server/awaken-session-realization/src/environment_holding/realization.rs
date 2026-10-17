// @formal entrypoint runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal evidence session_environment.terminal_fence_admits_only_inflight_binding_receipts
//! Ordinary Session Environment selection, effect authorization, and publication.

use crate::realization_error::RealizationError;
use crate::session_environment::{
    AuthorizedSessionEnvironmentEffect, SessionEnvironmentAdoptionDisposition,
    SessionEnvironmentUnavailablePolicy,
};
use awaken_session_configuration_contract::resource as session_resource;
use std::sync::Arc;

impl crate::environment_holding::EnvironmentHolding {
    /// Original unfenced fixture entry; production callers use the authorized effect.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn create_session_environment(
        &self,
        peers: crate::peers::Peers<'_>,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<crate::session_environment::SessionEnvironment, RealizationError> {
        self.create_session_environment_with_provider_effect(
            peers,
            provider,
            spec,
            None,
            None,
            None,
            awaken_session_environment_contract::EnvironmentRealizationKind::Create,
        )
        .await
    }

    pub fn session_environment_provider(
        &self,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, RealizationError> {
        match provisioning {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. } => self
                .providers
                .backend_owned_session_provider
                .as_ref()
                .ok_or_else(|| {
                    RealizationError::invariant(
                        "BackendOwned provisioning requires a trusted-host Session provider",
                    )
                }),
            awaken_runtime_contract::resolved::ModelProvisioning::Provider { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::Remote { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor => {
                Ok(&self.providers.session_provider)
            }
        }
    }

    /// Select one provider from the frozen Environment trust decision and the
    /// immutable model provisioning. `trusted_host` is exact rather than a
    /// minimum capability: a stronger Namespace/Container provider would lose
    /// the local machine identity the author explicitly selected.
    pub fn session_environment_provider_for_thread(
        &self,
        thread: &str,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, RealizationError> {
        let sandbox = self
            .sessions
            .frozen(thread)?
            .environment_projection
            .as_ref()
            .and_then(|environment| environment.sandbox.as_ref())
            .cloned();
        if sandbox.as_ref().is_some_and(|sandbox| {
            sandbox.host_access == awaken_provisioning_contract::HostAccessPolicy::TrustedHost
        }) {
            if self.providers.trusted_host
                != crate::environment_providers::TrustedHostPolicy::Admitted
            {
                return Err(RealizationError::invalid_request(
                    "trusted_host Environment is disabled on this Worker",
                ));
            }
            let provider = self
                .providers
                .backend_owned_session_provider
                .as_ref()
                .ok_or_else(|| {
                    RealizationError::invalid_request(
                        "trusted_host Environment requires a local Workdir provider",
                    )
                })?;
            if !provider.realizer().supports_host_identity() {
                return Err(RealizationError::invariant(
                    "trusted_host provider does not preserve host identity",
                ));
            }
            return Ok(provider);
        }
        if matches!(
            provisioning,
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. }
        ) && sandbox.as_ref().and_then(|sandbox| sandbox.isolation)
            >= Some(awaken_provisioning_contract::IsolationClass::Namespace)
        {
            return Err(RealizationError::invalid_request(
                "backend-owned local Agent credentials require a trusted_host Workdir Environment",
            ));
        }
        self.session_environment_provider(provisioning)
    }

    async fn create_session_environment_for_effect(
        &self,
        peers: crate::peers::Peers<'_>,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<crate::session_environment::SessionEnvironment, RealizationError> {
        let source_handle = effect
            .authorization
            .intent()
            .source_binding()
            .map(|source| {
                serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(source)
                    .map_err(crate::realization_error::EvidenceFault::InvalidSourceBinding)
            })
            .transpose()?;
        // The provider decides what a Rebuild means for its physical owner: a
        // container replaces the incarnation the source binding names, while
        // Workdir and Namespace rebuilds are owned by their realization marker.
        let intent = if matches!(
            effect.authorization.intent().kind(),
            awaken_session_state_contract::SessionEnvironmentEffectKind::Rebuild { .. }
        ) {
            awaken_session_environment_contract::EnvironmentRealizationKind::Rebuild
        } else {
            awaken_session_environment_contract::EnvironmentRealizationKind::Create
        };
        self.create_session_environment_with_provider_effect(
            peers,
            provider,
            spec,
            effect.provider_fence.as_ref(),
            effect.provider_authority.as_deref(),
            source_handle.as_ref(),
            intent,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create_session_environment_with_provider_effect(
        &self,
        peers: crate::peers::Peers<'_>,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        authority: Option<&dyn awaken_provisioning_contract::CurrentExecutionAuthority>,
        source_handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        intent: awaken_session_environment_contract::EnvironmentRealizationKind,
    ) -> Result<crate::session_environment::SessionEnvironment, RealizationError> {
        let spec = self.validate_session_environment_capabilities(peers, provider, spec)?;
        // Capture before any preparation await. A same-byte replacement installed Session
        // must not lend its proof to this in-flight creation or its retries.
        let current_fence = match (effect_fence, authority) {
            (Some(_), Some(authority)) => Some(authority),
            (None, None) => None,
            _ => {
                return Err(RealizationError::invariant(
                    "Environment create lacks captured authority",
                ));
            }
        };
        let effect =
            crate::session_environment::ProviderEffect::from_parts(effect_fence, current_fence)
                .map_err(RealizationError::from)?;
        effect.admit().map_err(RealizationError::from)?;
        self.providers
            .cache_volume_prewarmer
            .prepare_mounts(&spec.mounts)
            .await
            .map_err(RealizationError::from)?;
        // Cache preparation is asynchronous: re-enter the same captured effect.
        effect.admit().map_err(RealizationError::from)?;
        provider
            .realizer()
            .create_effective_for_effect(&spec, effect, source_handle, intent)
            .await
            .map_err(|error| {
                RealizationError::Run(
                    awaken_session_realization_contract::environment_effect_error(&error),
                )
            })
    }

    /// Validate one exact physical projection without creating it. Reservation
    /// uses this shared classifier when durable admission must remain
    /// side-effect free but the eventual execution cannot be deferred; actual
    /// creation reuses it immediately before effects.
    /// Reject a deterministic substrate mismatch between the Session's frozen
    /// mounts and its provider before any Environment exists.
    pub fn validate_session_environment_substrate(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<(), RealizationError> {
        let provider = self.session_environment_provider_for_thread(thread, provisioning)?;
        let spec = peers.staging.sandbox_spec_for_provider(thread, provider)?;
        self.validate_session_environment_capabilities(peers, provider, &spec)
            .map(|_| ())
    }

    pub fn validate_session_environment_capabilities(
        &self,
        peers: crate::peers::Peers<'_>,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, RealizationError> {
        let resources = peers.staging.thread_resources_snapshot(&spec.scope)?;
        let repository_paths = resources
            .repositories
            .iter()
            .map(|repository| repository.plan.mount_path.clone())
            .collect::<Vec<_>>();
        let spec = provider
            .realizer()
            .effective_spec(spec)
            .map_err(RealizationError::from)?;
        self.validate_effective_session_environment_capabilities(
            provider,
            spec,
            &repository_paths,
            &[],
        )
    }

    /// Validate the exact typed durable-handle layout before any provider
    /// adoption or lease-renewal I/O. This is deliberately distinct from
    /// validating the current create spec: historical handles own the output
    /// and base-environment coordinates that built-in providers reopen.
    pub fn validate_session_environment_adoption(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, RealizationError> {
        let repository_paths = self
            .sessions
            .read_realization(&spec.scope, |r| {
                r.staging()
                    .transition
                    .as_ref()
                    .map(|transition| {
                        transition
                            .desired()
                            .resources
                            .inputs()
                            .iter()
                            .filter(|input| {
                                matches!(
                                    input.source,
                                    session_resource::ResolvedInputSource::Repository { .. }
                                )
                            })
                            .map(crate::managed_input_layout::resolved_input_effect_path)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| {
                        r.staging()
                            .resources
                            .repositories
                            .iter()
                            .map(|repository| repository.plan.mount_path.clone())
                            .collect()
                    })
            })?
            .unwrap_or_default();
        let layout = provider
            .adoption()
            .effective_adoption_layout(spec, handle)
            .map_err(RealizationError::from)?;
        if !repository_paths.is_empty() && layout.historical_owned_paths.is_none() {
            return Err(RealizationError::invariant(
                "legacy Session sandbox handle lacks complete owned-path evidence required for Repository adoption",
            ));
        }
        let historical_owned_paths = layout
            .historical_owned_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        self.validate_effective_session_environment_capabilities(
            provider,
            layout.spec,
            &repository_paths,
            &historical_owned_paths,
        )
    }

    fn validate_effective_session_environment_capabilities(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: awaken_provisioning_contract::SandboxSpec,
        repository_paths: &[String],
        historical_owned_paths: &[&str],
    ) -> Result<awaken_provisioning_contract::SandboxSpec, RealizationError> {
        let repository_paths = repository_paths
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        awaken_provisioning_contract::validate_repository_sandbox_adoption_layout(
            &repository_paths,
            &spec,
            historical_owned_paths,
        )
        .map_err(crate::realization_error::InvalidRequest::EnvironmentLayout)?;
        let capabilities = provider.realizer().capabilities();
        if provider.realizer().supports_host_identity() {
            awaken_provisioning_contract::prepare_trusted_host_environment(&spec, &capabilities)
                .map_err(crate::realization_error::InvalidRequest::from)?;
            return Ok(spec);
        }
        let base = awaken_provisioning_contract::SandboxRequirements::from_spec(&spec, false);
        // The retained immutable publication is the exact candidate-set fact
        // installed by Session activation and claimed-worker replay. Never
        // re-resolve a mutable catalog head while choosing physical isolation.
        let opaque_process = self
            .sessions
            .modify_session(&spec.scope, |installed| {
                awaken_session_realization_contract::frozen_session_requires_opaque_process(
                    installed.inputs.published_snapshot.as_ref(),
                    installed.inputs.baseline.as_ref(),
                )
            })?
            .unwrap_or(false);
        let (spec, required) = awaken_provisioning_contract::session_sandbox_projection(
            &spec,
            !repository_paths.is_empty(),
            opaque_process,
        );
        if capabilities.satisfies_requirements(&base)
            && !capabilities.satisfies_requirements(&required)
        {
            return Err(RealizationError::invalid_request(format!(
                "Session environment cannot preserve one sandbox-absolute workspace path across Hand, Bash, Git, and Agent processes: required={required:?}, provider={capabilities:?}",
            )));
        }
        awaken_provisioning_contract::prepare_environment(&spec, &capabilities)
            .map_err(crate::realization_error::InvalidRequest::from)?;
        Ok(spec)
    }

    /// Evict only the rebuildable runtime context while retaining the
    /// independently-owned Session environment and its live resource projection.
    /// Terminal cleanup remains the single two-stage responsibility of
    /// [`Self::prepare_terminal_cleanup_effect`] and [`Self::dispose_terminal_cleanup_effect`].
    pub async fn evict_session_for_rebuild(&self, thread: &str) -> Result<(), RealizationError> {
        self.sessions
            .modify_session(thread, |installed| installed.run.drop_context())?;
        Ok(())
    }
}

impl crate::environment_holding::EnvironmentHolding {
    /// Atomically publish one created/adopted Environment into the existing
    /// durable binding authority and the process-local installed Session. Every lifecycle
    /// entry point uses this owner so the reconciliation flag cannot drift from
    /// the handle that was persisted.
    pub async fn publish_session_environment_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        effect: &AuthorizedSessionEnvironmentEffect,
        reconciliation: crate::staged_resources::EnvironmentResourceReconciliation,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        debug_assert_eq!(
            lifecycle.session(),
            thread,
            "lifecycle held for another Session"
        );
        let realizations = self.sessions.clone();
        let exec = &self;
        let candidate = match effect.authorization.intent().kind() {
            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt => {
                exec.begin_session_environment_adoption(peers, thread, environment.clone(), Some(effect.authorization.intent()))?
            }
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create
            | awaken_session_state_contract::SessionEnvironmentEffectKind::Rebuild { .. } => {
                exec.begin_session_environment_preparation(peers, thread, environment.clone(), Some(effect.authorization.intent()))?
            }
            awaken_session_state_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                ..
            } => {
                return Err(RealizationError::invariant(
                    "Resource reservation cannot publish a Session Environment owner",
                ));
            }
        };
        let identity = match self
            .persist_authorized_environment_before_publish(peers, thread, &candidate, effect)
            .await
        {
            Ok(identity) => identity,
            Err(failure) => {
                if let Some(identity) = failure.committed_identity {
                    exec.retain_committed_session_environment_candidate(
                        thread, &candidate, identity,
                    )?;
                }
                // Provider realization may have committed even when root CAS or
                // its response is unavailable. Retain Store-read identity but
                // never publish the stale caller; exact-fenced recovery owns
                // cleanup of the same Candidate.
                return Err(failure.error);
            }
        };
        let environment =
            exec.publish_prepared_session_environment(thread, &candidate, identity)?;
        realizations.update_realization(thread, |r| {
            r.set_resource_reconciliation(reconciliation);
        })?;
        Ok(environment)
    }

    /// Retry the one retained Candidate. Same-generation preparation retries
    /// only publication with its original Arc and effect kind. Revoked Unbound
    /// ownership instead delegates to ordinary authorized adoption, preserving
    /// its physical target while replacing closed execution. In both cases only
    /// the existing Store readback supplies durable publication identity.
    pub async fn retry_session_environment_candidate_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
    ) -> Result<Option<Arc<crate::session_environment::SessionEnvironment>>, RealizationError> {
        let realizations = self.sessions.clone();
        let exec = &self;
        let revoked_binding = realizations.read_realization(thread, |r| match &r.environment().owner {
            crate::environment_owner::SessionEnvironmentOwner::Retiring(retiring)
                if retiring.cause == crate::environment_owner::SessionEnvironmentRetirementCause::RealizationRevocation =>
                match &retiring.owned {
                    crate::environment_owner::RetiringEnvironmentOwner::Unbound(candidate) => candidate.ordinary_intent().map(|_| Some(candidate.binding.clone())),
                    _ => Ok(None),
                },
            _ => Ok(None),
        })?.unwrap_or(Ok(None))?;
        if let Some(binding) = revoked_binding {
            // A Candidate can own physical effects without a returned root
            // receipt. Reuse ordinary authorized adoption, including for an
            // Unmaterialized root; raw local binding is not durable authority.
            let provider = self.projected_session_environment_provider(thread, None)?;
            self.adopt_bound_session_environment_under_lifecycle(
                lifecycle,
                peers,
                thread,
                &binding,
                provider,
                None,
                SessionEnvironmentUnavailablePolicy::Reject,
                None,
            )
            .await?;
            return exec
                .session_environment(thread)
                .await?
                .map(Some)
                .ok_or_else(|| {
                    RealizationError::invariant(
                        "unacknowledged Candidate adoption published no owner",
                    )
                });
        }
        let candidate = realizations
            .read_realization(thread, |r| {
                r.environment().owner.unpublished_candidate().cloned()
            })?
            .flatten();
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let effect = self
            .authorize_environment_effect_before_io(peers, thread, candidate.ordinary_intent()?)
            .await?;
        let identity = match self
            .persist_authorized_environment_before_publish(peers, thread, &candidate, &effect)
            .await
        {
            Ok(identity) => identity,
            Err(failure) => {
                if let Some(identity) = failure.committed_identity {
                    exec.retain_committed_session_environment_candidate(
                        thread, &candidate, identity,
                    )?;
                }
                return Err(failure.error);
            }
        };
        let reconciliation = match &candidate.origin {
            crate::environment_owner::UnboundSessionEnvironmentOrigin::New
            | crate::environment_owner::UnboundSessionEnvironmentOrigin::DurableCreate(_) => {
                crate::staged_resources::EnvironmentResourceReconciliation::Fresh
            }
            crate::environment_owner::UnboundSessionEnvironmentOrigin::Adoption
            | crate::environment_owner::UnboundSessionEnvironmentOrigin::AdoptionFrom(_)
            | crate::environment_owner::UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                crate::staged_resources::EnvironmentResourceReconciliation::Adopted
            }
        };
        let environment =
            exec.publish_prepared_session_environment(thread, &candidate, identity)?;
        realizations.update_realization(thread, |r| {
            r.set_resource_reconciliation(reconciliation);
        })?;
        Ok(Some(environment))
    }

    /// Create the one baseline-only substrate, reserve every desired Resource
    /// path in its V2 handle, durably publish that handle, and only then execute
    /// the canonical physical Resource transition. Callers must hold the
    /// Session lifecycle fence.
    pub async fn create_reserved_session_environment_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        let realizations = self.sessions.clone();
        let staging = peers.staging;
        let transition = realizations
            .read_realization(thread, |r| r.staging().transition.clone())?
            .flatten()
            .ok_or_else(|| {
                RealizationError::invariant(
                    "Session Environment creation requires an exact Resource transition",
                )
            })?;
        let (claim, realization_lease) = realizations
            .modify_session(thread, |installed| {
                (
                    installed.run.dispatch_claim().cloned(),
                    installed.realization.execution().authority.lease().cloned(),
                )
            })?
            .unwrap_or_default();
        let rebuild_source = realizations
            .read_realization(thread, |r| r.environment().rebuild_source.clone())?
            .flatten();
        let (kind, source_binding) = match &rebuild_source {
            Some((source_binding, source_generation_id)) => (
                awaken_session_state_contract::SessionEnvironmentEffectKind::Rebuild {
                    source_generation_id: source_generation_id.clone(),
                },
                Some(source_binding.as_str()),
            ),
            None => (
                awaken_session_state_contract::SessionEnvironmentEffectKind::Create,
                None,
            ),
        };
        let effect = self
            .authorize_environment_effect_before_io(
                peers,
                thread,
                &peers
                    .authority
                    .new_environment_effect_intent(thread, kind, source_binding)?,
            )
            .await?;
        if let awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding,
            ..
        } = &effect.authorization
        {
            return self
                .adopt_already_applied_environment_under_lifecycle(lifecycle, peers,
                    thread, provider, binding, &effect,
                )
                .await;
        }
        // Compile/read/verify the immutable desired inputs once under the
        // aggregate transition before choosing the provider's create-time mount
        // subset. With no live Environment this performs no workspace mutation;
        // the second call after publication consumes the keyed staged result.
        peers
            .staging
            .apply_session_inputs_under_lifecycle(
                lifecycle,
                peers,
                thread,
                &transition,
                claim.as_ref(),
                realization_lease.as_ref(),
            )
            .await
            .map_err(RealizationError::from)?;
        let spec = staging.sandbox_substrate_spec_for_provider(thread, provider)?;
        let environment = Arc::new(
            self.create_session_environment_for_effect(peers, provider, &spec, &effect)
                .await?,
        );
        for input in transition.desired().resources.inputs() {
            let path = crate::managed_input_layout::resolved_input_effect_path(input);
            if let Err(error) = environment.projection().reserve_owned_path(&path) {
                let _ = environment.lifecycle().dispose().await;
                return Err(RealizationError::from(error));
            }
        }
        let environment = self
            .publish_session_environment_under_lifecycle(
                lifecycle,
                peers,
                thread,
                environment,
                &effect,
                crate::staged_resources::EnvironmentResourceReconciliation::Fresh,
            )
            .await?;
        realizations.update_realization(thread, |r| {
            r.environment_mut().rebuild_source = None;
        })?;
        self.ensure_published_environment_reconciled_under_lifecycle(
            lifecycle,
            peers,
            thread,
            environment,
        )
        .await
    }

    /// Recover a root-committed create/rebuild after its response or delivery
    /// cache update was lost. The durable binding is adopted as-is; re-entering
    /// provider creation here could replace the very substrate the root names.
    async fn adopt_already_applied_environment_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        binding: &str,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        let disposition = self
            .adopt_bound_session_environment_under_lifecycle(
                lifecycle,
                peers,
                thread,
                binding,
                provider,
                None,
                SessionEnvironmentUnavailablePolicy::Reject,
                Some(effect),
            )
            .await?;
        if disposition != SessionEnvironmentAdoptionDisposition::Ready {
            return Err(RealizationError::invariant(
                "root-committed Session Environment was not ready during receipt replay",
            ));
        }
        self.session_environment(thread).await?.ok_or_else(|| {
            RealizationError::invariant(
                "root-committed Session Environment was not published after adoption",
            )
        })
    }

    /// Gate every Environment consumer on completion of the exact transition
    /// that accompanied its Create/Adopt publication. A persisted substrate is
    /// intentionally retained after a transient effect failure; this method
    /// makes the next tool, child, or context retry the same command instead of
    /// exposing a partially projected workspace.
    pub async fn ensure_published_environment_reconciled_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        let realizations = self.sessions.clone();
        let reconciliation = realizations
            .read_realization(thread, |r| r.staging().reconciliation)?
            .unwrap_or_default();
        let transition = realizations
            .read_realization(thread, |r| r.staging().transition.clone())?
            .flatten()
            .ok_or_else(|| {
                RealizationError::invariant(
                    "published Session Environment lacks its Resource transition",
                )
            })?;
        let installed = peers.staging.thread_resource_manifest(thread)?;
        if reconciliation == crate::staged_resources::EnvironmentResourceReconciliation::None
            && installed.as_ref() == Some(transition.desired())
        {
            return Ok(environment);
        }
        let (claim, realization_lease) = realizations
            .modify_session(thread, |installed| {
                (
                    installed.run.dispatch_claim().cloned(),
                    installed.realization.execution().authority.lease().cloned(),
                )
            })?
            .unwrap_or_default();
        peers
            .staging
            .apply_session_inputs_under_lifecycle(
                lifecycle,
                peers,
                thread,
                &transition,
                claim.as_ref(),
                realization_lease.as_ref(),
            )
            .await
            .map_err(RealizationError::from)?;
        Ok(environment)
    }

    /// Materialize the deferred environment at the first Sandbox-target tool.
    /// The same lifecycle mutex used by context construction guarantees one
    /// creator, and publication follows resource realization + durable binding.
    pub async fn ensure_session_environment_for_tool(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, RealizationError> {
        let realizations = self.sessions.clone();
        let lifecycle = realizations.lock_lifecycle(thread).await?;
        if let Some(environment) = self
            .retry_session_environment_candidate_under_lifecycle(lifecycle.held(), peers, thread)
            .await?
        {
            return self
                .ensure_published_environment_reconciled_under_lifecycle(
                    lifecycle.held(),
                    peers,
                    thread,
                    environment,
                )
                .await;
        }
        if let Some(environment) = realizations
            .read_realization(thread, |r| r.environment().owner.resident())?
            .flatten()
        {
            return self
                .ensure_published_environment_reconciled_under_lifecycle(
                    lifecycle.held(),
                    peers,
                    thread,
                    environment,
                )
                .await;
        }
        let provider = self.projected_session_environment_provider(thread, None)?;
        if let Some(binding) = self.durable_environment_binding(thread)? {
            let disposition = self
                .adopt_bound_session_environment_under_lifecycle(
                    lifecycle.held(),
                    peers,
                    thread,
                    &binding,
                    provider,
                    None,
                    SessionEnvironmentUnavailablePolicy::Reject,
                    None,
                )
                .await?;
            if disposition != SessionEnvironmentAdoptionDisposition::Ready {
                return Err(RealizationError::invariant(
                    "durable Session Environment did not become Resident",
                ));
            }
            return self.session_environment(thread).await?.ok_or_else(|| {
                RealizationError::invariant(
                    "durable Session Environment adoption published no owner",
                )
            });
        }
        self.create_reserved_session_environment_under_lifecycle(
            lifecycle.held(),
            peers,
            thread,
            provider,
        )
        .await
    }
}
