//! Ordinary Session Environment selection, effect authorization, and publication.

use super::*;

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
            self.agent_publications.as_ref().and_then(|source| {
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
            awaken_session_contract::FrozenAgentPublicationDecision::Unpinned
            | awaken_session_contract::FrozenAgentPublicationDecision::OptionalMissing
            | awaken_session_contract::FrozenAgentPublicationDecision::Exact => {}
            awaken_session_contract::FrozenAgentPublicationDecision::MissingRequired => {
                return Err(HostError::internal(
                    "frozen Session Agent publication is unavailable",
                ));
            }
            awaken_session_contract::FrozenAgentPublicationDecision::Mismatch => {
                return Err(HostError::internal(
                    "Session Agent/model publication does not match its frozen coordinates",
                ));
            }
        }
        let installed = installed
            .map(|snapshot| {
                let projected = match &coordinates {
                    CanonicalSessionProjection::Baseline(baseline) => {
                        awaken_session_contract::project_effective_agent_publication(
                            baseline.model_override.as_ref(),
                            &baseline.system_prompt,
                            workspace,
                            snapshot,
                        )
                    }
                    CanonicalSessionProjection::Layout(layout) => {
                        awaken_session_contract::project_effective_agent_publication(
                            layout.model_override.as_ref(),
                            &awaken_session_contract::SessionSystemPromptSelection::Inherit,
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
        let selected_agent = agent.or(projected_agent.as_deref()).unwrap_or("assistant");
        let baseline = self
            .session_slots
            .read(thread, |slot| slot.baseline.clone())
            .flatten();
        let coordinates = baseline.as_ref().map_or(
            CanonicalSessionProjection::LegacyAgent(selected_agent),
            CanonicalSessionProjection::Baseline,
        );
        let (installed, model_candidate) =
            self.resolve_canonical_session_projection(&workspace, coordinates, published_snapshot)?;
        let published_backend_ref = model_candidate
            .as_ref()
            .map(|candidate| candidate.binding().backend_ref.clone());
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

    /// Freeze one exact publication into the existing Session runtime slot.
    /// The slot is already the projection used by claimed-worker replay; direct
    /// and deferred execution reuse it instead of adding a provider-selection
    /// store or resolving a later catalog head.
    pub(crate) fn retain_session_publication(
        &self,
        thread: &str,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> Result<(), HostError> {
        let Some(publication) = publication else {
            return Ok(());
        };
        let conflict = self.session_slots.update(thread, |slot| {
            if slot
                .published_snapshot
                .as_ref()
                .is_some_and(|retained| retained != publication)
            {
                true
            } else {
                slot.published_snapshot = Some(publication.clone());
                false
            }
        });
        if conflict {
            Err(HostError::internal(
                "Session cannot replace its immutable Agent publication",
            ))
        } else {
            Ok(())
        }
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
        self.session_environment_provider(&provisioning)
    }

    pub(crate) fn projected_session_model_provisioning(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<awaken_runtime_contract::resolved::ModelProvisioning, HostError> {
        let delivered = self
            .session_slots
            .read(thread, |slot| slot.published_snapshot.clone())
            .flatten();
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
        kind: awaken_session_contract::SessionEnvironmentEffectKind,
        source_binding: Option<&str>,
    ) -> Result<AuthorizedSessionEnvironmentEffect, HostError> {
        let realization = self
            .session_slots
            .read(thread, |slot| slot.realization_lease.clone())
            .flatten();
        let environment_fingerprint = self
            .session_slots
            .read(thread, |slot| {
                slot.baseline
                    .as_ref()
                    .map(|baseline| baseline.environment.config_fingerprint.0.clone())
            })
            .flatten();
        let mut intent =
            awaken_session_contract::SessionEnvironmentEffectIntent::new(thread, kind, realization);
        if let Some(environment_fingerprint) = environment_fingerprint {
            intent = intent.for_environment(environment_fingerprint);
        }
        if let Some(source_binding) = source_binding {
            intent = intent.from_binding(source_binding);
        }
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone();
        let authorization = match sink {
            Some(sink) => sink
                .authorize(&intent)
                .await
                .map_err(crate::managed_adapter_error::from_run_error)?,
            None if intent.realization().is_none() => {
                awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned
            }
            None => {
                return Err(HostError::internal(
                    "durable Session Environment effect has no binding authority",
                ));
            }
        };
        let provider_fence = if !matches!(
            authorization,
            awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned
        ) {
            intent
                .realization()
                .map(|realization| {
                    realization
                        .sandbox_effect_fence(intent.effect_id())
                        .map_err(|error| HostError::internal(error.to_string()))
                })
                .transpose()?
        } else {
            None
        };
        Ok(AuthorizedSessionEnvironmentEffect {
            intent,
            authorization,
            provider_fence,
        })
    }

    pub(super) async fn persist_authorized_environment_before_publish(
        &self,
        thread: &str,
        candidate: &crate::session_slot::UnboundSessionEnvironment,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<
        crate::session_slot::BoundSessionEnvironmentIdentity,
        EnvironmentBindingPersistenceError,
    > {
        if matches!(
            effect.authorization,
            awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned
        ) && let crate::session_slot::UnboundSessionEnvironmentOrigin::DurableAdoption(identity) =
            &candidate.origin
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
        crate::session_slot::BoundSessionEnvironmentIdentity,
        EnvironmentBindingPersistenceError,
    > {
        let receipt = awaken_session_contract::SessionEnvironmentReceipt::from_intent(
            &effect.intent,
            binding.clone(),
        )
        .map_err(|error| EnvironmentBindingPersistenceError {
            error: HostError::internal(error.to_string()),
        })?;
        // A Resource reservation extends the already-published Environment
        // handle under the Session root. It is not a Run placement and must not
        // rewrite `RunDispatch.sandbox`: the slot can legitimately retain the
        // preceding settled attempt's claim while an idle Session accepts a
        // live Resource generation. Create/Adopt/Rebuild remain the only raw
        // dispatch-cache writers and retain their exact claim fence below.
        let binds_deferred_dispatch = !matches!(
            effect.intent.kind(),
            awaken_session_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation { .. }
        );
        match &effect.authorization {
            awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned => {
                if binds_deferred_dispatch {
                    self.bind_deferred_dispatch_before_publish(thread, &binding)
                        .await
                        .map_err(|error| EnvironmentBindingPersistenceError { error })?;
                }
                return Ok(
                    crate::session_slot::BoundSessionEnvironmentIdentity::LegacyDirect(
                        crate::session_slot::LegacyDirectEnvironmentProvenance::Direct(receipt),
                    ),
                );
            }
            awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                binding: committed,
            } => {
                if committed != &binding {
                    return Err(EnvironmentBindingPersistenceError {
                        error: HostError::internal(
                            "Session Environment effect already committed another binding",
                        ),
                    });
                }
            }
            awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized => {}
        }
        let sink = self
            .environment_binding_sink
            .read()
            .expect("environment binding sink lock poisoned")
            .clone()
            .ok_or_else(|| EnvironmentBindingPersistenceError {
                error: HostError::internal(
                    "authorized Session Environment effect has no durable binding sink",
                ),
            })?;
        // Publishing needs Store-read generated identity, not merely a positive
        // authorization readback. If the response is lost, retain the exact
        // Candidate and retry this idempotent persist instead of inventing a
        // process-local generation.
        let committed = sink
            .persist(receipt.clone())
            .await
            .map_err(crate::managed_adapter_error::from_run_error)
            .map_err(|error| EnvironmentBindingPersistenceError { error })?;
        if binds_deferred_dispatch {
            self.bind_deferred_dispatch_before_publish(thread, &binding)
                .await
                .map_err(|error| EnvironmentBindingPersistenceError { error })?;
        }
        super::environment_lifecycle::committed_identity(&receipt, committed)
            .map_err(|error| EnvironmentBindingPersistenceError { error })
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
        reconciliation: crate::session_slot::EnvironmentResourceReconciliation,
    ) -> Result<Arc<crate::session_environment::SessionEnvironment>, HostError> {
        let candidate = match effect.intent.kind() {
            awaken_session_contract::SessionEnvironmentEffectKind::Adopt => {
                self.begin_session_environment_adoption(thread, environment.clone())?
            }
            awaken_session_contract::SessionEnvironmentEffectKind::Create
            | awaken_session_contract::SessionEnvironmentEffectKind::Rebuild { .. } => {
                self.begin_session_environment_preparation(thread, environment.clone())?
            }
            awaken_session_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation {
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
                // Provider realization may have committed even when root CAS or
                // its response is unavailable. Never let this stale caller run a
                // destructive compensation; exact-fenced recovery owns cleanup.
                return Err(failure.error);
            }
        };
        let environment =
            self.publish_prepared_session_environment(thread, &candidate, identity)?;
        self.session_slots.update(thread, |slot| {
            slot.environment_resource_reconciliation = reconciliation;
        });
        Ok(environment)
    }

    /// Resume only the publication edge of an exact hidden Candidate. A prior
    /// persistence call may have failed definitively or committed while losing
    /// its response; both retries reuse the same Arc, binding and effect kind,
    /// and only the existing Store port can return the generated durable
    /// identity. No provider create/adopt effect is repeated here.
    pub(super) async fn retry_prepared_session_environment_publication_under_lifecycle(
        &self,
        thread: &str,
    ) -> Result<Option<Arc<crate::session_environment::SessionEnvironment>>, HostError> {
        let candidate = self
            .session_slots
            .read(thread, |slot| {
                slot.environment_owner.unpublished_candidate().cloned()
            })
            .flatten();
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let source_binding = match &candidate.origin {
            crate::session_slot::UnboundSessionEnvironmentOrigin::New => None,
            crate::session_slot::UnboundSessionEnvironmentOrigin::Adoption
            | crate::session_slot::UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                Some(candidate.binding.as_str())
            }
        };
        let effect = self
            .authorize_environment_effect_before_io(thread, candidate.effect_kind(), source_binding)
            .await?;
        let identity = self
            .persist_authorized_environment_before_publish(thread, &candidate, &effect)
            .await
            .map_err(|failure| failure.error)?;
        let reconciliation = match &candidate.origin {
            crate::session_slot::UnboundSessionEnvironmentOrigin::New => {
                crate::session_slot::EnvironmentResourceReconciliation::Fresh
            }
            crate::session_slot::UnboundSessionEnvironmentOrigin::Adoption
            | crate::session_slot::UnboundSessionEnvironmentOrigin::DurableAdoption(_) => {
                crate::session_slot::EnvironmentResourceReconciliation::Adopted
            }
        };
        let environment =
            self.publish_prepared_session_environment(thread, &candidate, identity)?;
        self.session_slots.update(thread, |slot| {
            slot.environment_resource_reconciliation = reconciliation;
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
        let transition = self
            .session_slots
            .read(thread, |slot| slot.resource_transition.clone())
            .flatten()
            .ok_or_else(|| {
                HostError::internal(
                    "Session Environment creation requires an exact Resource transition",
                )
            })?;
        let claim = self
            .session_slots
            .read(thread, |slot| slot.dispatch_claim.clone())
            .flatten();
        let rebuild_source = self
            .session_slots
            .read(thread, |slot| slot.environment_rebuild_source.clone())
            .flatten();
        let (kind, source_binding) = match &rebuild_source {
            Some((source_binding, source_generation_id)) => (
                awaken_session_contract::SessionEnvironmentEffectKind::Rebuild {
                    source_generation_id: source_generation_id.clone(),
                },
                Some(source_binding.as_str()),
            ),
            None => (
                awaken_session_contract::SessionEnvironmentEffectKind::Create,
                None,
            ),
        };
        let effect = self
            .authorize_environment_effect_before_io(thread, kind, source_binding)
            .await?;
        if let awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
            binding,
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
        )
        .await
        .map_err(|error| HostError::internal(error.to_string()))?;
        let spec = self.sandbox_substrate_spec_for_provider(thread, provider);
        let environment = Arc::new(
            self.create_session_environment_for_effect(thread, provider, &spec, &effect)
                .await?,
        );
        for input in transition.desired().resources.inputs() {
            let path = crate::managed_resource_projection::resolved_input_effect_path(input);
            if let Err(error) = environment.reserve_owned_path(&path) {
                let _ = environment.dispose().await;
                return Err(HostError::internal(error.to_string()));
            }
        }
        let environment = self
            .publish_session_environment_under_lifecycle(
                thread,
                environment,
                &effect,
                crate::session_slot::EnvironmentResourceReconciliation::Fresh,
            )
            .await?;
        self.session_slots.update(thread, |slot| {
            slot.environment_rebuild_source = None;
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
        self.session_environment(thread).await.ok_or_else(|| {
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
            .session_slots
            .read(thread, |slot| slot.environment_resource_reconciliation)
            .unwrap_or_default();
        let transition = self
            .session_slots
            .read(thread, |slot| slot.resource_transition.clone())
            .flatten()
            .ok_or_else(|| {
                HostError::internal("published Session Environment lacks its Resource transition")
            })?;
        let installed = self.thread_resource_manifest(thread);
        if reconciliation == crate::session_slot::EnvironmentResourceReconciliation::None
            && installed.as_ref() == Some(transition.desired())
        {
            return Ok(environment);
        }
        let claim = self
            .session_slots
            .read(thread, |slot| slot.dispatch_claim.clone())
            .flatten();
        self.apply_dispatched_resource_transition_under_lifecycle(
            thread,
            &transition,
            claim.as_ref(),
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
            .session_slots
            .read(thread, |slot| slot.dispatch_claim.clone())
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
        self.create_session_environment_with_provider_effect(
            &spec.scope,
            provider,
            spec,
            None,
            None,
            awaken_sandbox_container::ContainerRealizationIntent::Create,
        )
        .await
    }

    async fn create_session_environment_for_effect(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        effect: &AuthorizedSessionEnvironmentEffect,
    ) -> Result<crate::session_environment::SessionEnvironment, HostError> {
        let source_handle = effect
            .intent
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
        let intent = if matches!(
            effect.intent.kind(),
            awaken_session_contract::SessionEnvironmentEffectKind::Rebuild { .. }
        ) && matches!(
            provider,
            crate::session_environment::SessionEnvironmentProvider::Container { .. }
        ) {
            let handle = source_handle.as_ref().ok_or_else(|| {
                HostError::internal("Container Environment rebuild lacks its source binding")
            })?;
            awaken_sandbox_container::ContainerRealizationIntent::Rebuild {
                source_incarnation: handle
                    .container_physical_incarnation()
                    .map_err(|error| HostError::internal(error.to_string()))?
                    .to_string(),
                source_runtime_handle: handle
                    .container_payload()
                    .map_err(|error| HostError::internal(error.to_string()))?
                    .runtime_handle
                    .clone(),
            }
        } else {
            // This fact is consumed only by the Container provider. Workdir and
            // Namespace rebuilds remain owned by their realization-marker path.
            awaken_sandbox_container::ContainerRealizationIntent::Create
        };
        self.create_session_environment_with_provider_effect(
            thread,
            provider,
            spec,
            effect.provider_fence.as_ref(),
            source_handle.as_ref(),
            intent,
        )
        .await
    }

    async fn create_session_environment_with_provider_effect(
        &self,
        thread: &str,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
        effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        source_handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        intent: awaken_sandbox_container::ContainerRealizationIntent,
    ) -> Result<crate::session_environment::SessionEnvironment, HostError> {
        let spec = self.validate_session_environment_capabilities(provider, spec)?;
        self.cache_volume_prewarmer
            .prepare_mounts(&spec.mounts)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let current_fence = |asserted: &awaken_provisioning_contract::SandboxEffectFence| {
            let lease = self
                .session_slots
                .read(thread, |slot| slot.realization_lease.clone())
                .flatten()
                .ok_or_else(|| {
                    awaken_provisioning_contract::SandboxError::new(
                        "Session realization authority is no longer installed",
                    )
                })?;
            lease
                .sandbox_effect_fence(asserted.operation_id.clone())
                .map_err(|error| awaken_provisioning_contract::SandboxError::new(error.to_string()))
        };
        provider
            .create_effective_for_effect(
                &spec,
                effect_fence,
                effect_fence.map(|_| {
                    &current_fence as &dyn awaken_sandbox_container::ContainerEffectFenceSource
                }),
                source_handle,
                intent,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }

    /// Validate one exact physical projection without creating it. Reservation
    /// uses this shared classifier when durable admission must remain
    /// side-effect free but the eventual execution cannot be deferred; actual
    /// creation reuses it immediately before effects.
    pub(crate) fn validate_session_environment_capabilities(
        &self,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<awaken_provisioning_contract::SandboxSpec, HostError> {
        let resources = self.thread_resources_snapshot(&spec.scope);
        let repository_paths = resources
            .repositories
            .iter()
            .map(|repository| repository.plan.mount_path.clone())
            .collect::<Vec<_>>();
        let spec = provider
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
            .session_slots
            .read(&spec.scope, |slot| {
                slot.resource_transition
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
                                    awaken_session_contract::ResolvedInputSource::Repository { .. }
                                )
                            })
                            .map(crate::managed_resource_projection::resolved_input_effect_path)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| {
                        slot.resources
                            .repositories
                            .iter()
                            .map(|repository| repository.plan.mount_path.clone())
                            .collect()
                    })
            })
            .unwrap_or_default();
        let layout = provider
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
        .map_err(|error| HostError::internal(error.to_string()))?;
        let capabilities = provider.capabilities();
        let base = awaken_provisioning_contract::SandboxRequirements::from_spec(&spec, false);
        // The retained immutable publication is the exact candidate-set fact
        // installed by Session activation and claimed-worker replay. Never
        // re-resolve a mutable catalog head while choosing physical isolation.
        let opaque_process = self
            .session_slots
            .read(&spec.scope, |slot| {
                crate::provisioning::frozen_session_requires_opaque_process(
                    slot.published_snapshot.as_ref(),
                    slot.baseline.as_ref(),
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
            return Err(HostError::internal(format!(
                "Session environment cannot preserve one sandbox-absolute workspace path across Hand, Bash, Git, and Agent processes: required={required:?}, provider={capabilities:?}",
            )));
        }
        awaken_provisioning_contract::prepare_environment(&spec, &capabilities)
            .map_err(|error| HostError::internal(error.to_string()))?;
        Ok(spec)
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
        if let Some(environment) = self
            .retry_prepared_session_environment_publication_under_lifecycle(thread)
            .await?
        {
            return self
                .ensure_published_environment_reconciled_under_lifecycle(thread, environment)
                .await;
        }
        if let Some(environment) = self
            .session_slots
            .read(thread, |slot| slot.environment_owner.resident())
            .flatten()
        {
            return self
                .ensure_published_environment_reconciled_under_lifecycle(thread, environment)
                .await;
        }
        let provider = self.projected_session_environment_provider(thread, None)?;
        if let Some(binding) = self.durable_session_environment_binding(thread) {
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
            return self.session_environment(thread).await.ok_or_else(|| {
                HostError::internal("durable Session Environment adoption published no owner")
            });
        }
        self.create_reserved_session_environment_under_lifecycle(thread, provider)
            .await
    }

    /// Evict only the rebuildable runtime context while retaining the
    /// independently-owned Session environment and its live resource projection.
    /// Terminal cleanup remains the single two-stage responsibility of
    /// [`Self::prepare_terminal_cleanup_effect`] and [`Self::dispose_terminal_cleanup_effect`].
    pub(crate) async fn evict_session_for_rebuild(&self, thread: &str) {
        self.session_slots
            .modify(thread, |slot| slot.runtime = None);
    }
}
