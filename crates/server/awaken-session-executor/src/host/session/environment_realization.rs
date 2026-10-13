// @formal entrypoint runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal evidence session_environment.terminal_fence_admits_only_inflight_binding_receipts
//! Ordinary Session Environment selection, effect authorization, and publication.

use super::*;
use awaken_session_configuration_contract::resource as session_resource;

#[cfg(test)]
mod fence_source_tests;

impl SharedHost {
    /// Resolve one Session publication and its exact model candidate from
    /// explicit frozen coordinates. This is the sole selector used by cold
    /// install and resident execution: a complete Session override wins even
    /// when no Agent publication exists, the exact Agent publication is next,
    /// and only a genuinely unpinned legacy Session has no candidate.
    pub(crate) fn resolve_canonical_session_projection(
        &self,
        workspace: &str,
        coordinates: CanonicalSessionProjection<'_>,
        delivered: Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
        skill_pins: Option<&[session_resource::ResolvedSkillBinding]>,
    ) -> Result<
        (
            Option<awaken_runtime_contract::ExecutableAgentSnapshot>,
            Option<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
        ),
        HostError,
    > {
        let agent = coordinates.agent();
        let frozen_revision = coordinates.agent_revision();
        let installed = delivered.or_else(|| {
            self.agent.agent_publications.as_ref().and_then(|source| {
                let agent_id = awaken_runtime_contract::snapshot::AgentId(agent.to_string());
                match frozen_revision {
                    Some(revision) => source.at_revision(workspace, &agent_id, revision),
                    None => source.current(workspace, &agent_id),
                }
            })
        });
        let model_override = coordinates.model_override();
        if let Some(publication) =
            model_override.and_then(|override_| override_.publication.as_ref())
        {
            publication
                .validate_for_workspace(workspace)
                .map_err(|error| {
                    HostError::internal(format!(
                        "frozen Session model override publication is invalid: {error}"
                    ))
                })?;
        }
        match coordinates.publication_decision(installed.as_ref()) {
            awaken_session_realization_contract::FrozenAgentPublicationDecision::Unpinned
            | awaken_session_realization_contract::FrozenAgentPublicationDecision::OptionalMissing
            | awaken_session_realization_contract::FrozenAgentPublicationDecision::Exact => {}
            awaken_session_realization_contract::FrozenAgentPublicationDecision::MissingRequired => {
                return Err(HostError::internal(
                    "frozen Session Agent publication is unavailable",
                ));
            }
            awaken_session_realization_contract::FrozenAgentPublicationDecision::Mismatch => {
                let actual = installed.as_ref().map(|publication| {
                    format!(
                        "agent={}, revision={}, backend={}",
                        publication.root_agent_id.0,
                        publication.metadata.source.revision,
                        publication.resolved_spec.model_binding.identity().2,
                    )
                });
                return Err(HostError::internal(format!(
                    "Session Agent/model publication does not match its frozen coordinates \
                     (expected agent={}, revision={:?}; actual={})",
                    coordinates.agent(),
                    coordinates.agent_revision(),
                    actual.as_deref().unwrap_or("missing"),
                )));
            }
        }
        let installed = installed
            .map(|snapshot| {
                let projected = match &coordinates {
                    CanonicalSessionProjection::Baseline(baseline) => {
                        awaken_session_realization_contract::project_effective_agent_publication(
                            baseline.model_override.as_ref(),
                            &baseline.system_prompt,
                            skill_pins,
                            workspace,
                            snapshot,
                        )
                    }
                    CanonicalSessionProjection::Layout(layout) => {
                        awaken_session_realization_contract::project_effective_agent_publication(
                            layout.model_override.as_ref(),
                            &awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit,
                            None,
                            workspace,
                            snapshot,
                        )
                    }
                    CanonicalSessionProjection::LegacyAgent(_) => Ok(snapshot),
                };
                projected.map_err(|error| HostError::internal(error.to_string()))
            })
            .transpose()?;
        let candidate = model_override
            .and_then(|override_| override_.publication.as_ref())
            .map(|publication| publication.primary.clone())
            .or_else(|| {
                installed
                    .as_ref()
                    .map(|snapshot| snapshot.resolved_spec.model_binding.clone())
            });
        Ok((installed, candidate))
    }

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
            Option<awaken_runtime_contract::resolved::ResolvedModelCandidate>,
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
        // A provider-driven re-entry may carry neither the original request
        // argument nor a resident Thread projection. The already delivered
        // immutable publication is authoritative in that case; defaulting to
        // `assistant` would manufacture a second Agent identity and make the
        // exact-publication fence reject its own retained snapshot.
        let delivered_agent = published_snapshot
            .as_ref()
            .map(|snapshot| snapshot.root_agent_id.0.clone());
        let selected_agent = agent
            .or(projected_agent.as_deref())
            .or(delivered_agent.as_deref())
            .unwrap_or("assistant");
        let baseline = self.installed_sessions.frozen(thread).baseline.clone();
        let skill_pins = self
            .installed_sessions
            .read_realization(thread, |r| {
                r.resource_transition
                    .as_ref()
                    .map(|transition| transition.desired().resources.skills().to_vec())
            })
            .flatten();
        let coordinates = baseline.as_ref().map_or(
            CanonicalSessionProjection::LegacyAgent(selected_agent),
            CanonicalSessionProjection::Baseline,
        );
        let (installed, model_candidate) = self.resolve_canonical_session_projection(
            &workspace,
            coordinates,
            published_snapshot,
            skill_pins.as_deref(),
        )?;
        let published_backend_ref = model_candidate
            .as_ref()
            .map(|candidate| candidate.binding().identity().2.to_owned());
        let projected_backend_ref = self.installed_sessions.frozen(thread).backend_ref.clone();
        if let (Some(published), Some(projected)) = (&published_backend_ref, &projected_backend_ref)
            && published != projected
        {
            return Err(HostError::internal(format!(
                "session backend projection `{projected}` does not match publication `{published}`"
            )));
        }
        if model_candidate.is_none() && projected_backend_ref.is_some() {
            return Err(HostError::internal(
                "session backend projection has no immutable model publication",
            ));
        }
        Ok((
            workspace,
            selected_agent.to_string(),
            installed,
            model_candidate,
        ))
    }

    /// Freeze one exact publication into the existing Session runtime slot.
    /// The slot is already the projection used by claimed-worker replay; direct
    /// and deferred execution reuse it instead of adding a provider-selection
    /// store or resolving a later catalog head.
    pub(crate) fn retain_session_publication(
        &self,
        thread: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), HostError> {
        self.installed_sessions.update(thread, |slot| {
            slot.inputs.validate_publication(publication)?;
            if let Some(publication) = publication {
                slot.replace_frozen_with(|next| {
                    next.published_snapshot = Some(publication.clone())
                });
            }
            Ok(())
        })
    }

    /// Select the environment provider from the immutable publication already
    /// frozen for this Session. Callers without an Agent parameter (deferred
    /// tool realization and terminal cleanup) must use this path rather than a
    /// process-wide default.
    pub(crate) fn projected_session_environment_provider(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, HostError> {
        let provisioning = self.projected_session_model_provisioning(thread, agent)?;
        self.realizer
            .session_environment_provider_for_thread(thread, &provisioning)
    }

    pub(crate) fn projected_session_model_provisioning(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<awaken_runtime_contract::resolved::ModelProvisioning, HostError> {
        let delivered = self.installed_sessions.published_snapshot(thread);
        let (_, _, _, model_candidate) =
            self.resolve_session_publication(thread, agent, delivered)?;
        Ok(model_candidate
            .as_ref()
            .map(awaken_runtime_contract::resolved::ResolvedModelCandidate::provisioning)
            .cloned()
            .unwrap_or(awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor))
    }

    pub(crate) async fn authorize_environment_effect_before_io(
        &self,
        thread: &str,
        intent: &awaken_session_state_contract::SessionEnvironmentEffectIntent,
    ) -> Result<AuthorizedSessionEnvironmentEffect, HostError> {
        intent
            .verify()
            .map_err(|error| HostError::internal(error.to_string()))?;
        if intent.session_id().as_str() != thread {
            return Err(HostError::internal(
                "Environment request names another Session",
            ));
        }
        let (claimed_ordinary_run, managed_projection, closed) = self
            .installed_sessions
            .modify_session(thread, |slot| {
                let managed = slot.inputs.baseline.is_some() || slot.run.session_dispatch();
                let ordinary = slot.realization.authority.lease().is_none()
                    && slot.run.dispatch_claim().is_some()
                    && !managed;
                (ordinary, managed, slot.realization.authority.is_closed())
            })
            .unwrap_or_default();
        if closed {
            return Err(HostError::unavailable_classified(
                "session_environment_realization_closed",
                "closed Session realization cannot authorize an Environment effect",
            ));
        }
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone();
        // A claimed ordinary Run and a Managed Session already have distinct
        // durable owners. The former fences its local Environment through the
        // Run claim and `DispatchQueue::bind_sandbox`; only the latter may ask
        // the Session root to authorize/persist an aggregate Environment effect.
        // Sending an ordinary Run to a remote Session sink cannot be authorized
        // (it has no realization lease) and duplicates the queue-owned binding.
        let authorization = match sink {
            Some(_) if claimed_ordinary_run => {
                awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned {
                    intent: Box::new(intent.clone()),
                }
            }
            Some(sink) => sink
                .authorize(intent)
                .await
                .map_err(crate::host_error::from_run_error)?,
            None if !managed_projection && intent.realization().is_none() => {
                awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned {
                    intent: Box::new(intent.clone()),
                }
            }
            None => {
                return Err(HostError::unavailable_classified(
                    "session_environment_binding_authority_missing",
                    "durable Session Environment effect has no binding authority",
                ));
            }
        };
        authorization
            .verify_for(intent)
            .map_err(|error| HostError::internal(error.to_string()))?;
        let original = authorization.intent();
        if matches!(
            authorization,
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) && (managed_projection || original.realization().is_some())
        {
            return Err(HostError::unavailable_classified(
                "session_environment_owned_root_missing",
                "a Managed Session cannot fall back to an unowned Environment effect",
            ));
        }
        let provider_fence = if !matches!(
            authorization,
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) {
            original
                .realization()
                .map(|realization| {
                    realization
                        .sandbox_effect_fence(original.effect_id())
                        .map_err(|error| HostError::internal(error.to_string()))
                })
                .transpose()?
        } else {
            None
        };
        let provider_authority = provider_fence
            .as_ref()
            .map(|fence| {
                self.realizer
                    .execution
                    .provider_effect_authority(thread, fence)
                    .map(|source| {
                        Arc::new(source)
                            as Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>
                    })
            })
            .transpose()?;
        Ok(AuthorizedSessionEnvironmentEffect {
            authorization,
            provider_fence,
            provider_authority,
        })
    }

    pub(super) async fn persist_authorized_environment_before_publish(
        &self,
        thread: &str,
        candidate: &crate::environment_owner::UnboundSessionEnvironment,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<
        crate::environment_owner::BoundSessionEnvironmentIdentity,
        EnvironmentBindingPersistenceError,
    > {
        let captured = candidate.ordinary_intent()?;
        if captured != effect.authorization.intent() {
            return Err(HostError::internal(
                "candidate changed its original Environment effect intent",
            )
            .into());
        }
        if matches!(
            effect.authorization,
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned { .. }
        ) && let Some(identity) = candidate.durable_identity()
        {
            return Ok(identity.clone());
        }
        self.persist_authorized_environment_binding(thread, candidate.binding.clone(), effect)
            .await
    }

    pub(crate) async fn persist_authorized_environment_binding(
        &self,
        thread: &str,
        binding: String,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<
        crate::environment_owner::BoundSessionEnvironmentIdentity,
        EnvironmentBindingPersistenceError,
    > {
        let receipt = self
            .realizer
            .current_environment_receipt(binding.clone(), effect)?;
        // A Resource reservation extends the already-published Environment
        // handle under the Session root. It is not a Run placement and must not
        // rewrite `RunDispatch.sandbox`: the slot can legitimately retain the
        // preceding settled attempt's claim while an idle Session accepts a
        // live Resource generation. Create/Adopt/Rebuild remain the only raw
        // dispatch-cache writers and retain their exact claim fence below.
        let binds_deferred_dispatch = !matches!(
            effect.authorization.intent().kind(),
            awaken_session_state_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }
        );
        match &effect.authorization {
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned { .. } => {
                if binds_deferred_dispatch {
                    self.bind_deferred_dispatch_before_publish(thread, &binding)
                        .await?;
                }
                return Ok(
                    crate::environment_owner::BoundSessionEnvironmentIdentity::LegacyDirect(
                        crate::environment_owner::LegacyDirectEnvironmentProvenance::Direct(receipt),
                    ),
                );
            }
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                binding: committed,
                ..
            } => {
                if committed != &binding {
                    return Err(HostError::internal(
                            "Session Environment effect already committed another binding",
                        ).into());
                }
            }
            awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized { .. } => {}
        }
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone()
            .ok_or_else(|| {
                HostError::internal(
                    "authorized Session Environment effect has no durable binding sink",
                )
            })?;
        // Publishing needs Store-read generated identity, not merely a positive
        // authorization readback. If the response is lost, retain the exact
        // Candidate and retry this idempotent persist instead of inventing a
        // process-local generation.
        let committed = sink
            .persist(receipt.clone())
            .await
            .map_err(crate::host_error::from_run_error)?;
        let committed_identity =
            super::environment_lifecycle::committed_identity(&receipt, committed)?;
        if binds_deferred_dispatch {
            self.bind_deferred_dispatch_before_publish(thread, &binding)
                .await
                .map_err(|error| EnvironmentBindingPersistenceError {
                    error,
                    committed_identity: Some(committed_identity.clone()),
                })?;
        }
        Ok(committed_identity)
    }

    /// Atomically publish one created/adopted Environment into the existing
    /// durable binding authority and the process-local slot. Every lifecycle
    /// entry point uses this owner so the reconciliation flag cannot drift from
    /// the handle that was persisted.
    pub(super) async fn publish_session_environment_under_lifecycle(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
        effect: &AuthorizedSessionEnvironmentEffect,
        reconciliation: crate::staged_resources::EnvironmentResourceReconciliation,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let exec = &self.realizer;
        let candidate = match effect.authorization.intent().kind() {
            awaken_session_state_contract::SessionEnvironmentEffectKind::Adopt => {
                exec.begin_session_environment_adoption(thread, environment.clone(), Some(effect.authorization.intent()))?
            }
            awaken_session_state_contract::SessionEnvironmentEffectKind::Create
            | awaken_session_state_contract::SessionEnvironmentEffectKind::Rebuild { .. } => {
                exec.begin_session_environment_preparation(thread, environment.clone(), Some(effect.authorization.intent()))?
            }
            awaken_session_state_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                ..
            } => {
                return Err(HostError::internal(
                    "Resource reservation cannot publish a Session Environment owner",
                ));
            }
        };
        let identity = match self
            .persist_authorized_environment_before_publish(thread, &candidate, effect)
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
        self.installed_sessions.update_realization(thread, |r| {
            r.environment_resource_reconciliation = reconciliation;
        });
        Ok(environment)
    }

    /// Retry the one retained Candidate. Same-generation preparation retries
    /// only publication with its original Arc and effect kind. Revoked Unbound
    /// ownership instead delegates to ordinary authorized adoption, preserving
    /// its physical target while replacing closed execution. In both cases only
    /// the existing Store readback supplies durable publication identity.
    pub(super) async fn retry_session_environment_candidate_under_lifecycle(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<crate::session_environment::SessionEnvironment>>, HostError> {
        let exec = &self.realizer;
        let revoked_binding = self.installed_sessions.read_realization(thread, |r| match &r.environment_owner {
            crate::environment_owner::SessionEnvironmentOwner::Retiring(retiring)
                if retiring.cause == crate::environment_owner::SessionEnvironmentRetirementCause::RealizationRevocation =>
                match &retiring.owned {
                    crate::environment_owner::RetiringEnvironmentOwner::Unbound(candidate) => candidate.ordinary_intent().map(|_| Some(candidate.binding.clone())),
                    _ => Ok(None),
                },
            _ => Ok(None),
        }).unwrap_or(Ok(None))?;
        if let Some(binding) = revoked_binding {
            // A Candidate can own physical effects without a returned root
            // receipt. Reuse ordinary authorized adoption, including for an
            // Unmaterialized root; raw local binding is not durable authority.
            let provider = self.projected_session_environment_provider(thread, None)?;
            self.adopt_bound_session_environment_under_lifecycle(
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
                .await
                .map(Some)
                .ok_or_else(|| {
                    HostError::internal("unacknowledged Candidate adoption published no owner")
                });
        }
        let candidate = self
            .installed_sessions
            .read_realization(thread, |r| {
                r.environment_owner.unpublished_candidate().cloned()
            })
            .flatten();
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let effect = self
            .authorize_environment_effect_before_io(thread, candidate.ordinary_intent()?)
            .await?;
        let identity = match self
            .persist_authorized_environment_before_publish(thread, &candidate, &effect)
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
        self.installed_sessions.update_realization(thread, |r| {
            r.environment_resource_reconciliation = reconciliation;
        });
        Ok(Some(environment))
    }

    /// Create the one baseline-only substrate, reserve every desired Resource
    /// path in its V2 handle, durably publish that handle, and only then execute
    /// the canonical physical Resource transition. Callers must hold the
    /// Session lifecycle fence.
    pub(super) async fn create_reserved_session_environment_under_lifecycle(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let staging = &self.realizer.staging;
        let transition = self
            .installed_sessions
            .read_realization(thread, |r| r.resource_transition.clone())
            .flatten()
            .ok_or_else(|| {
                HostError::internal(
                    "Session Environment creation requires an exact Resource transition",
                )
            })?;
        let (claim, realization_lease) = self
            .installed_sessions
            .modify_session(thread, |slot| {
                (
                    slot.run.dispatch_claim().cloned(),
                    slot.realization.authority.lease().cloned(),
                )
            })
            .unwrap_or_default();
        let rebuild_source = self
            .installed_sessions
            .read_realization(thread, |r| r.environment_rebuild_source.clone())
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
                thread,
                &self
                    .realizer
                    .new_environment_effect_intent(thread, kind, source_binding),
            )
            .await?;
        if let awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding,
            ..
        } = &effect.authorization
        {
            return self
                .adopt_already_applied_environment_under_lifecycle(
                    thread, provider, binding, &effect,
                )
                .await;
        }
        // Compile/read/verify the immutable desired inputs once under the
        // aggregate transition before choosing the provider's create-time mount
        // subset. With no live Environment this performs no workspace mutation;
        // the second call after publication consumes the keyed staged result.
        self.apply_dispatched_resource_transition_under_lifecycle(
            thread,
            &transition,
            claim.as_ref(),
            realization_lease.as_ref(),
        )
        .await
        .map_err(|error| HostError::internal(error.to_string()))?;
        let spec = staging.sandbox_substrate_spec_for_provider(thread, provider);
        let environment = Arc::new(
            self.realizer
                .create_session_environment_for_effect(provider, &spec, &effect)
                .await?,
        );
        for input in transition.desired().resources.inputs() {
            let path = crate::managed_input_layout::resolved_input_effect_path(input);
            if let Err(error) = environment.projection().reserve_owned_path(&path) {
                let _ = environment.lifecycle().dispose().await;
                return Err(HostError::internal(error.to_string()));
            }
        }
        let environment = self
            .publish_session_environment_under_lifecycle(
                thread,
                environment,
                &effect,
                crate::staged_resources::EnvironmentResourceReconciliation::Fresh,
            )
            .await?;
        self.installed_sessions.update_realization(thread, |r| {
            r.environment_rebuild_source = None;
        });
        self.ensure_published_environment_reconciled_under_lifecycle(thread, environment)
            .await
    }

    /// Recover a root-committed create/rebuild after its response or delivery
    /// cache update was lost. The durable binding is adopted as-is; re-entering
    /// provider creation here could replace the very substrate the root names.
    async fn adopt_already_applied_environment_under_lifecycle(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        binding: &str,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let disposition = self
            .adopt_bound_session_environment_under_lifecycle(
                thread,
                binding,
                provider,
                None,
                SessionEnvironmentUnavailablePolicy::Reject,
                Some(effect),
            )
            .await?;
        if disposition != SessionEnvironmentAdoptionDisposition::Ready {
            return Err(HostError::internal(
                "root-committed Session Environment was not ready during receipt replay",
            ));
        }
        self.realizer
            .session_environment(thread)
            .await
            .ok_or_else(|| {
                HostError::internal(
                    "root-committed Session Environment was not published after adoption",
                )
            })
    }

    /// Gate every Environment consumer on completion of the exact transition
    /// that accompanied its Create/Adopt publication. A persisted substrate is
    /// intentionally retained after a transient effect failure; this method
    /// makes the next tool, child, or context retry the same command instead of
    /// exposing a partially projected workspace.
    pub(super) async fn ensure_published_environment_reconciled_under_lifecycle(
        &self,
        thread: &str,
        environment: Arc<crate::session_environment::SessionEnvironment>,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let reconciliation = self
            .installed_sessions
            .read_realization(thread, |r| r.environment_resource_reconciliation)
            .unwrap_or_default();
        let transition = self
            .installed_sessions
            .read_realization(thread, |r| r.resource_transition.clone())
            .flatten()
            .ok_or_else(|| {
                HostError::internal("published Session Environment lacks its Resource transition")
            })?;
        let installed = self.realizer.staging.thread_resource_manifest(thread);
        if reconciliation == crate::staged_resources::EnvironmentResourceReconciliation::None
            && installed.as_ref() == Some(transition.desired())
        {
            return Ok(environment);
        }
        let (claim, realization_lease) = self
            .installed_sessions
            .modify_session(thread, |slot| {
                (
                    slot.run.dispatch_claim().cloned(),
                    slot.realization.authority.lease().cloned(),
                )
            })
            .unwrap_or_default();
        self.apply_dispatched_resource_transition_under_lifecycle(
            thread,
            &transition,
            claim.as_ref(),
            realization_lease.as_ref(),
        )
        .await
        .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(environment)
    }

    async fn bind_deferred_dispatch_before_publish(
        &self,
        thread: &str,
        binding: &str,
    ) -> Result<bool, HostError> {
        let claim = self
            .installed_sessions
            .read_run(thread, |run| run.dispatch_claim.clone())
            .flatten();
        let Some(claim) = claim else {
            return Ok(false);
        };
        let outcome = self
            .dispatch_store()?
            .bind_sandbox(&claim, binding)
            .await
            .map_err(|error| {
                HostError::unavailable_classified(
                    "dispatch_sandbox_binding_unavailable",
                    error.to_string(),
                )
            })?;
        if !outcome.applied() {
            return Err(HostError::unavailable_classified(
                "dispatch_claim_stale",
                "deferred sandbox binding was fenced by a replacement claim",
            ));
        }
        Ok(true)
    }

    /// Test-only unfenced entry into the one physical Session-environment
    /// creation path. Production creation always carries the aggregate-owned
    /// effect authorization through `create_session_environment_for_effect`.
    #[cfg(test)]
    pub(crate) async fn create_session_environment(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<crate::session_environment::SessionEnvironment, HostError> {
        self.realizer
            .create_session_environment_with_provider_effect(
                provider,
                spec,
                None,
                None,
                None,
                awaken_session_environment_contract::EnvironmentRealizationKind::Create,
            )
            .await
    }

    /// Materialize the deferred environment at the first Sandbox-target tool.
    /// The same lifecycle mutex used by context construction guarantees one
    /// creator, and publication follows resource realization + durable binding.
    pub(crate) async fn ensure_session_environment_for_tool(
        &self,
        thread: &str,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let _lifecycle = self.installed_sessions.lock_lifecycle(thread).await;
        if let Some(environment) = self
            .retry_session_environment_candidate_under_lifecycle(thread)
            .await?
        {
            return self
                .ensure_published_environment_reconciled_under_lifecycle(thread, environment)
                .await;
        }
        if let Some(environment) = self
            .installed_sessions
            .read_realization(thread, |r| r.environment_owner.resident())
            .flatten()
        {
            return self
                .ensure_published_environment_reconciled_under_lifecycle(thread, environment)
                .await;
        }
        let provider = self.projected_session_environment_provider(thread, None)?;
        if let Some(binding) = self.realizer.durable_environment_binding(thread) {
            let disposition = self
                .adopt_bound_session_environment_under_lifecycle(
                    thread,
                    &binding,
                    provider,
                    None,
                    SessionEnvironmentUnavailablePolicy::Reject,
                    None,
                )
                .await?;
            if disposition != SessionEnvironmentAdoptionDisposition::Ready {
                return Err(HostError::internal(
                    "durable Session Environment did not become Resident",
                ));
            }
            return self
                .realizer
                .session_environment(thread)
                .await
                .ok_or_else(|| {
                    HostError::internal("durable Session Environment adoption published no owner")
                });
        }
        self.create_reserved_session_environment_under_lifecycle(thread, provider)
            .await
    }
}

impl crate::environment_holding::EnvironmentHolding {
    pub(crate) fn session_environment_provider(
        &self,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, HostError> {
        match provisioning {
            awaken_runtime_contract::resolved::ModelProvisioning::BackendOwned { .. } => self
                .env
                .backend_owned_session_provider
                .as_ref()
                .ok_or_else(|| {
                    HostError::internal(
                        "BackendOwned provisioning requires a trusted-host Session provider",
                    )
                }),
            awaken_runtime_contract::resolved::ModelProvisioning::Provider { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::Remote { .. }
            | awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor => {
                Ok(&self.env.session_provider)
            }
        }
    }

    /// Select one provider from the frozen Environment trust decision and the
    /// immutable model provisioning. `trusted_host` is exact rather than a
    /// minimum capability: a stronger Namespace/Container provider would lose
    /// the local machine identity the author explicitly selected.
    pub(crate) fn session_environment_provider_for_thread(
        &self,
        thread: &str,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<&crate::session_environment::SessionEnvironmentProvider, HostError> {
        let sandbox = self
            .sessions
            .frozen(thread)
            .environment_projection
            .as_ref()
            .and_then(|environment| environment.sandbox.as_ref())
            .cloned();
        if sandbox.as_ref().is_some_and(|sandbox| {
            sandbox.host_access == awaken_provisioning_contract::HostAccessPolicy::TrustedHost
        }) {
            if self.env.trusted_host != crate::environment_providers::TrustedHostPolicy::Admitted {
                return Err(HostError::bad_request(
                    "trusted_host Environment is disabled on this Worker",
                ));
            }
            let provider = self
                .env
                .backend_owned_session_provider
                .as_ref()
                .ok_or_else(|| {
                    HostError::bad_request(
                        "trusted_host Environment requires a local Workdir provider",
                    )
                })?;
            if !provider.realizer().supports_host_identity() {
                return Err(HostError::internal(
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
            return Err(HostError::bad_request(
                "backend-owned local Agent credentials require a trusted_host Workdir Environment",
            ));
        }
        self.session_environment_provider(provisioning)
    }

    /// Re-enter current authority after I/O while preserving the Root's exact
    /// original intent. Renewal is a delivery grant, not a new receipt identity.
    fn current_environment_receipt(
        &self,
        binding: String,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<awaken_session_state_contract::SessionEnvironmentReceipt, HostError> {
        match (&effect.provider_fence, &effect.provider_authority) {
            (Some(initial), Some(authority)) => {
                awaken_provisioning_contract::with_current_sandbox_effect(
                    initial,
                    authority.as_ref(),
                    |_| Ok(()),
                )
                .map_err(|error| HostError::internal(error.to_string()))?;
            }
            (None, None) => {}
            _ => {
                return Err(HostError::internal(
                    "Environment effect lacks its captured authority",
                ));
            }
        }
        awaken_session_state_contract::SessionEnvironmentReceipt::from_intent(
            effect.authorization.intent(),
            binding,
        )
        .map_err(|error| HostError::internal(error.to_string()))
    }

    async fn create_session_environment_for_effect(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<crate::session_environment::SessionEnvironment, HostError> {
        let source_handle = effect
            .authorization
            .intent()
            .source_binding()
            .map(|source| {
                serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(source).map_err(
                    |error| {
                        HostError::internal(format!(
                            "Session Environment effect has an invalid source binding: {error}"
                        ))
                    },
                )
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
            provider,
            spec,
            effect.provider_fence.as_ref(),
            effect.provider_authority.as_deref(),
            source_handle.as_ref(),
            intent,
        )
        .await
    }

    async fn create_session_environment_with_provider_effect(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        authority: Option<&dyn awaken_provisioning_contract::CurrentExecutionAuthority>,
        source_handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        intent: awaken_session_environment_contract::EnvironmentRealizationKind,
    ) -> Result<crate::session_environment::SessionEnvironment, HostError> {
        let spec = self.validate_session_environment_capabilities(provider, spec)?;
        // Capture before any preparation await. A same-byte replacement slot
        // must not lend its proof to this in-flight creation or its retries.
        let current_fence = match (effect_fence, authority) {
            (Some(_), Some(authority)) => Some(authority),
            (None, None) => None,
            _ => {
                return Err(HostError::internal(
                    "Environment create lacks captured authority",
                ));
            }
        };
        let effect =
            crate::session_environment::ProviderEffect::from_parts(effect_fence, current_fence)
                .map_err(|error| HostError::internal(error.to_string()))?;
        effect
            .admit()
            .map_err(|error| HostError::internal(error.to_string()))?;
        self.env
            .cache_volume_prewarmer
            .prepare_mounts(&spec.mounts)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        // Cache preparation is asynchronous: re-enter the same captured effect.
        effect
            .admit()
            .map_err(|error| HostError::internal(error.to_string()))?;
        provider
            .realizer()
            .create_effective_for_effect(&spec, effect, source_handle, intent)
            .await
            .map_err(|error| {
                crate::host_error::from_run_error(
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
    pub(crate) fn validate_session_environment_substrate(
        &self,
        thread: &str,
        provisioning: &awaken_runtime_contract::resolved::ModelProvisioning,
    ) -> Result<(), HostError> {
        let provider = self.session_environment_provider_for_thread(thread, provisioning)?;
        let spec = self.staging.sandbox_spec_for_provider(thread, provider);
        self.validate_session_environment_capabilities(provider, &spec)
            .map(|_| ())
    }

    pub(crate) fn validate_session_environment_capabilities(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, HostError> {
        let resources = self.staging.thread_resources_snapshot(&spec.scope);
        let repository_paths = resources
            .repositories
            .iter()
            .map(|repository| repository.plan.mount_path.clone())
            .collect::<Vec<_>>();
        let spec = provider
            .realizer()
            .effective_spec(spec)
            .map_err(|error| HostError::internal(error.to_string()))?;
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
    pub(crate) fn validate_session_environment_adoption(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, HostError> {
        let repository_paths = self
            .sessions
            .read_realization(&spec.scope, |r| {
                r.resource_transition
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
                        r.resources
                            .repositories
                            .iter()
                            .map(|repository| repository.plan.mount_path.clone())
                            .collect()
                    })
            })
            .unwrap_or_default();
        let layout = provider
            .adoption()
            .effective_adoption_layout(spec, handle)
            .map_err(|error| HostError::internal(error.to_string()))?;
        if !repository_paths.is_empty() && layout.historical_owned_paths.is_none() {
            return Err(HostError::internal(
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
    ) -> Result<awaken_provisioning_contract::SandboxSpec, HostError> {
        let repository_paths = repository_paths
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        awaken_provisioning_contract::validate_repository_sandbox_adoption_layout(
            &repository_paths,
            &spec,
            historical_owned_paths,
        )
        .map_err(|error| HostError::bad_request(error.to_string()))?;
        let capabilities = provider.realizer().capabilities();
        if provider.realizer().supports_host_identity() {
            awaken_provisioning_contract::prepare_trusted_host_environment(&spec, &capabilities)
                .map_err(|error| HostError::bad_request(error.to_string()))?;
            return Ok(spec);
        }
        let base = awaken_provisioning_contract::SandboxRequirements::from_spec(&spec, false);
        // The retained immutable publication is the exact candidate-set fact
        // installed by Session activation and claimed-worker replay. Never
        // re-resolve a mutable catalog head while choosing physical isolation.
        let opaque_process = self
            .sessions
            .modify_session(&spec.scope, |slot| {
                crate::provisioning::frozen_session_requires_opaque_process(
                    slot.inputs.published_snapshot.as_ref(),
                    slot.inputs.baseline.as_ref(),
                )
            })
            .unwrap_or(false);
        let (spec, required) = crate::provisioning::session_sandbox_projection(
            &spec,
            !repository_paths.is_empty(),
            opaque_process,
        );
        if capabilities.satisfies_requirements(&base)
            && !capabilities.satisfies_requirements(&required)
        {
            return Err(HostError::bad_request(format!(
                "Session environment cannot preserve one sandbox-absolute workspace path across Hand, Bash, Git, and Agent processes: required={required:?}, provider={capabilities:?}",
            )));
        }
        awaken_provisioning_contract::prepare_environment(&spec, &capabilities)
            .map_err(|error| HostError::bad_request(error.to_string()))?;
        Ok(spec)
    }

    /// Evict only the rebuildable runtime context while retaining the
    /// independently-owned Session environment and its live resource projection.
    /// Terminal cleanup remains the single two-stage responsibility of
    /// [`Self::prepare_terminal_cleanup_effect`] and [`Self::dispose_terminal_cleanup_effect`].
    pub(crate) async fn evict_session_for_rebuild(&self, thread: &str) {
        self.sessions
            .modify_session(thread, |slot| slot.run.drop_context());
    }
}

use awaken_dispatch_contract::DispatchExecution as _;
