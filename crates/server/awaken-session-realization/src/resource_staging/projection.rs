// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_resources.explicit_empty_manifest_revokes_runtime_projection
// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
//! Resource transition application of Resource staging: one aggregate-owned
//! Resource transition staged under the resource-projection lock, applied to
//! the Session's Environment, and the desired-only Resources a claimed dispatch
//! installs. Requirement compilation and layout validation are the sibling
//! modules these steps call.
use awaken_provisioning_contract as pc;
use awaken_session_configuration_contract::resource as session_resource;

use awaken_session_types::RunError;

use crate::sandbox_layout::resource_mount_is_create_time;
use crate::staged_resources::EnvironmentResourceReconciliation;
use awaken_session_configuration_contract::skill_delivery::skill_store_run_error;

mod repository_error;
mod transition;
pub(super) use repository_error::repository_binding_run_error;

fn resource_requirement_effect_key(
    transition: &session_resource::SessionResourceTransition,
    claim: Option<&awaken_worker_contract::RunClaim>,
    realization_lease: Option<&session_resource::SessionRealizationLease>,
) -> String {
    let authority = if let Some(claim) = claim {
        format!("run:{}", awaken_agent_contract::stable_fingerprint(claim))
    } else if let Some(lease) = realization_lease {
        format!(
            "realization:{}",
            awaken_agent_contract::stable_fingerprint(lease)
        )
    } else {
        "local".to_string()
    };
    format!("{}:{}", transition.operation_fingerprint(), authority,)
}

impl crate::resource_staging::ResourceStaging {
    /// Install one already-resolved Session resource manifest. This is shared by
    /// managed Session creation and cold durable workers; neither path reads Agent
    /// defaults or selects a newer mutable-resource configuration.
    pub async fn stage_resource_manifest(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        resources: &session_resource::ResolvedSessionResources,
        claim: Option<&awaken_worker_contract::RunClaim>,
    ) -> Result<(), RunError> {
        // Full-manifest preflight must precede even process-local Workspace or
        // Skill projection writes. Per-item validation below remains the final
        // effect-edge fence for defense in depth.
        self.validate_effective_resource_layout(peers, thread, resources)?;

        let desired = session_resource::SessionResourceManifest::at_revision(
            workspace,
            resource_revision,
            resources.clone(),
        );
        // Desired-only ingress can safely compile an exact no-op generation,
        // but it cannot claim knowledge of a physical previous generation. A
        // complete projection uses `stage_prevalidated_resource_transition`
        // below and therefore keys the same compiled requirements by the exact
        // aggregate transition consumed at realization.
        let effect_key = resource_requirement_effect_key(
            &session_resource::SessionResourceTransition::new(desired.clone(), desired.clone())
                .expect("a manifest is always a valid no-op transition"),
            claim,
            None,
        );
        self.stage_resource_requirements(thread, &desired, claim, None, effect_key)
            .await
    }

    /// Stage cold provider requirements after the complete projection owner has
    /// already validated the prospective baseline/Environment/provider layout.
    /// The transition supplies the only exact previous generation and its
    /// fingerprint lets later physical realization reuse this compilation.
    #[allow(clippy::too_many_arguments)]
    pub async fn stage_prevalidated_resource_transition_under_resource_projection(
        &self,
        resource_projection: crate::session_realizations::ResourceProjectionHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        transition: &session_resource::SessionResourceTransition,
        claim: Option<&awaken_worker_contract::RunClaim>,
        realization_lease: Option<&session_resource::SessionRealizationLease>,
        prospective_environment_binding: bool,
    ) -> Result<(), RunError> {
        debug_assert_eq!(
            resource_projection.session(),
            thread,
            "resource projection held for another Session"
        );
        let environments = peers.environments;
        let staging = self;
        let realizations = &staging.sessions;
        let installed = staging.thread_resource_manifest(thread)?;
        if installed.as_ref().is_some_and(|installed| {
            installed != transition.previous() && installed != transition.desired()
        }) {
            return Err(RunError::unavailable_classified(
                "session_resource_transition_conflict",
                "the resident active Resource generation is outside the aggregate transition",
            ));
        }
        let live_environment = environments.session_environment(thread).await?;
        let awaiting_adoption = live_environment.is_none()
            && (prospective_environment_binding
                || realizations
                    .read_realization(thread, |r| {
                        r.environment().owner.durable_binding().is_some()
                    })?
                    .unwrap_or(false));
        if awaiting_adoption {
            // The bound provider substrate is the only physical prior
            // generation. Adoption must publish it before File/Vault reads or
            // owned-path mutation can safely run. Immutable Skill bytes are a
            // dispatch-semantic input as well as a later filesystem input,
            // however: a cold Coordinator must freeze slash-command/context
            // selection before reserving the Run. Reuse the one pinned Skill
            // loader here, but deliberately leave the physical effect key and
            // all provider requirements absent so adoption still owns the sole
            // complete compilation edge.
            let versions = staging
                .load_pinned(
                    transition.desired().workspace_id.as_str(),
                    transition.desired().resources.skills(),
                    claim,
                    realization_lease.map(|lease| (thread, transition.desired().revision, lease)),
                )
                .await
                .map_err(skill_store_run_error)?;
            realizations.update_realization(thread, |r| {
                r.validate_preparation_publication(realization_lease)?;
                r.staging_mut().skills = Some(versions);
                Ok::<_, awaken_session_types::RunError>(())
            })??;
            return Ok(());
        }
        let effect_key = resource_requirement_effect_key(transition, claim, realization_lease);
        staging
            .stage_resource_requirements(
                thread,
                transition.desired(),
                claim,
                realization_lease.map(|lease| (thread, transition.desired().revision, lease)),
                effect_key,
            )
            .await
    }

    pub async fn validate_thread_resource_bindings(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
    ) -> Result<(), RunError> {
        let environments = peers.environments;
        let realizations = self.sessions.clone();
        use crate::staged_resources::ResourceBindingCheck;

        // This method is entered only through the SessionRuns application
        // port. Preserve that neutral identity before dispatch so a claiming
        // Worker enters the frozen Session realization path.
        realizations.enter_session_dispatch(thread)?;
        let checks = realizations
            .read_realization(thread, |r| r.staging().resources.binding_checks.clone())?
            .unwrap_or_default();
        if checks.is_empty() {
            return Ok(());
        }
        let workspace = environments.publication().workspace(thread)?;
        for check in checks {
            match check {
                ResourceBindingCheck::MemoryStore {
                    memory_store_id,
                    config_version,
                } => {
                    self.verify_memory_binding(&workspace, &memory_store_id, config_version)
                        .await?
                }
                ResourceBindingCheck::Repository {
                    repository_id,
                    config_version,
                    authority,
                    ..
                } => self
                    .repository_binding_verifier
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "repository resources require a configured binding verifier",
                        )
                    })?
                    .verify(
                        &workspace,
                        &repository_id,
                        config_version,
                        authority.as_deref(),
                    )
                    .await
                    .map(|_| ())
                    .map_err(repository_binding_run_error)?,
            }
        }
        Ok(())
    }

    pub async fn apply_session_inputs_with_context(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        transition: &session_resource::SessionResourceTransition,
        claim: Option<&awaken_worker_contract::RunClaim>,
        realization_lease: Option<&session_resource::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        let realizations = self.sessions.clone();
        let lifecycle = realizations.lock_lifecycle(thread).await?;
        self.apply_session_inputs_under_lifecycle(
            lifecycle.held(),
            peers,
            thread,
            transition,
            claim,
            realization_lease,
        )
        .await
    }

    /// Execute the one Resource transition while the caller holds the Session
    /// lifecycle fence. Fresh Environment creation uses this exact body after
    /// its handle is durably published; ordinary reconciliation enters through
    /// `apply_session_inputs_with_context`, so no second physical apply path is
    /// introduced.
    pub async fn apply_session_inputs_under_lifecycle(
        &self,
        lifecycle: crate::session_realizations::LifecycleHeld<'_>,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        transition: &session_resource::SessionResourceTransition,
        claim: Option<&awaken_worker_contract::RunClaim>,
        realization_lease: Option<&session_resource::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        debug_assert_eq!(
            lifecycle.session(),
            thread,
            "lifecycle held for another Session"
        );
        let realizations = self.sessions.clone();
        let _resource_projection = realizations.lock_resource_projection(thread).await?;
        self.apply_session_inputs_body(peers, thread, transition, claim, realization_lease)
            .await
    }

    /// Sole physical Resource transition body. Lock acquisition is deliberately
    /// absent here: ordinary callers enter through lifecycle -> resource, while
    /// Environment creation/adoption already holding lifecycle enters through
    /// the resource-locking suffix above.
    /// Publish the completed active generation. Callers must have completed the
    /// exact aggregate-authored physical transition before entering this edge;
    /// desired-only staging is deliberately unable to reach it.
    async fn apply_session_inputs_body(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        transition: &session_resource::SessionResourceTransition,
        claim: Option<&awaken_worker_contract::RunClaim>,
        realization_lease: Option<&session_resource::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        let environments = peers.environments;
        let staging = self;
        let sessions = &staging.sessions;
        let previous_manifest = transition.previous();
        let desired_manifest = transition.desired();
        let inputs = &desired_manifest.resources;
        // Read baseline/environment/provider projection while the same lifecycle
        // fence excludes a concurrent projection install. Validation still
        // precedes every Workspace, Skill, File, Repository, cache, or provider
        // effect below.
        self.validate_effective_resource_layout(peers, thread, inputs)?;
        sessions.update_realization(thread, |r| {
            r.staging_mut().transition = Some(transition.clone());
        })?;
        let live_environment = environments.session_environment(thread).await?;
        let awaiting_adoption = live_environment.is_none()
            && sessions
                .read_realization(thread, |r| {
                    r.environment().owner.durable_binding().is_some()
                })?
                .unwrap_or(false);
        let (baseline_is_staged, session_dispatch, reconciliation, environment_projection) =
            sessions
                .modify_session(thread, |installed| {
                    (
                        installed.inputs.baseline.is_some(),
                        installed.run.session_dispatch(),
                        installed.realization.staging().reconciliation,
                        installed.inputs.environment_projection.clone(),
                    )
                })?
                .unwrap_or_default();
        // Direct protocol Sessions do not own a frozen Managed baseline, but
        // their Environment still enters through this one aggregate transition
        // authority. Admit only the exact empty legacy generation synthesized
        // by `ExecutorComponents::run`; any claimed, Managed, non-empty, advancing, or
        // replacement transition without its frozen projection remains closed.
        let direct_empty_revision_zero_noop = claim.is_none()
            && !baseline_is_staged
            && !session_dispatch
            && transition.previous() == transition.desired()
            && desired_manifest.revision == 0
            && inputs.inputs().is_empty()
            && inputs.skills().is_empty();
        // An ordinary claimed Run may carry the same frozen Environment
        // projection without acquiring Session-command identity. That exact
        // projection is sufficient runtime authority for its carried Resource
        // transition; absence of both projections remains fail-closed.
        let projection_is_staged = baseline_is_staged
            || session_dispatch
            || environment_projection.is_some()
            || direct_empty_revision_zero_noop;
        if live_environment.is_none() && !projection_is_staged {
            return Err(RunError::unavailable_classified(
                "session_resource_projection_not_staged",
                "Session Resource reconciliation requires its frozen Runtime projection",
            ));
        }
        if awaiting_adoption {
            // The durable binding is the only physical prior generation. Keep
            // this transition as a pure command projection until that exact
            // handle is adopted and published; compiling File/Skill/Vault input
            // here would run before its owned-path reservation can be renewed.
            return Ok(());
        }
        let desired_manifest = desired_manifest.clone();
        let projection_change = transition::live_resource_projection_change(
            transition.previous() != transition.desired(),
            !desired_manifest.resources.inputs().is_empty(),
            !desired_manifest.resources.skills().is_empty(),
        );
        // This is the completed physical projection, not the desired-only
        // requirements cache. A lost aggregate acknowledgement may resend A→B
        // after this process completed B. Memory must compare against B while
        // the remaining Resource effects still replay the authored A→B command.
        let installed_manifest = staging.thread_resource_manifest(thread)?;
        let old_memory: Vec<_> = installed_manifest
            .as_ref()
            .unwrap_or(previous_manifest)
            .resources
            .inputs()
            .iter()
            .filter(|input| {
                matches!(
                    input.source,
                    session_resource::ResolvedInputSource::MemoryStore { .. }
                )
            })
            .cloned()
            .collect();
        let desired_memory: Vec<_> = desired_manifest
            .resources
            .inputs()
            .iter()
            .filter(|input| {
                matches!(
                    input.source,
                    session_resource::ResolvedInputSource::MemoryStore { .. }
                )
            })
            .cloned()
            .collect();
        // Only a completed, explicit no-op may skip the entire transition.
        // A→B still replays non-Memory effects; staging desired requirements
        // alone is never completion evidence.
        if live_environment.is_some()
            && transition.previous() == transition.desired()
            && installed_manifest.as_ref() == Some(&desired_manifest)
            && reconciliation == EnvironmentResourceReconciliation::None
        {
            return Ok(());
        }
        // A durable sandbox binding can be adopted before this process has any
        // Resource projection. `None` therefore means cold recovery: install the
        // authority's active generation. Only an already-installed, different
        // manifest is evidence of a forbidden live Memory mutation.
        if live_environment.is_some()
            && installed_manifest.is_some()
            && old_memory != desired_memory
        {
            tracing::warn!(
                session_id = thread,
                installed_manifest = ?installed_manifest,
                old_memory = ?old_memory,
                desired_memory = ?desired_memory,
                "rejecting a live Session Memory projection change"
            );
            return Err(RunError::bad_request(
                "memory_store inputs are create-time only for a live Session",
            ));
        }
        if live_environment.is_some()
            && transition.previous() == transition.desired()
            && reconciliation == EnvironmentResourceReconciliation::None
        {
            // Preparation already staged the exact desired requirements. A
            // same-generation adoption is continuity, not a replacement: the
            // provider adoption hook reattaches Namespace mounts and no Resource
            // path may be removed before terminal publication/cleanup.
            staging.register_thread_resource_manifest(thread, desired_manifest)?;
            return Ok(());
        }
        let mut previous_mounts = previous_manifest
            .resources
            .inputs()
            .iter()
            .filter(|input| {
                !matches!(
                    input.source,
                    session_resource::ResolvedInputSource::MemoryStore { .. }
                )
            })
            .chain(old_memory.iter())
            .filter_map(|input| {
                crate::managed_layout::resolved_input_validation_mount(
                    input,
                    environment_projection.as_ref(),
                )
            })
            .collect::<Vec<_>>();
        let desired_mounts = desired_manifest
            .resources
            .inputs()
            .iter()
            .filter_map(|input| {
                crate::managed_layout::resolved_input_validation_mount(
                    input,
                    environment_projection.as_ref(),
                )
            })
            .collect::<Vec<_>>();
        if reconciliation == EnvironmentResourceReconciliation::Fresh {
            previous_mounts.extend(
                desired_mounts
                    .iter()
                    .filter(|mount| resource_mount_is_create_time(mount))
                    .cloned(),
            );
        }
        if let Some(environment) = &live_environment {
            environment
                .projection()
                .validate_live_mount_replacement(&previous_mounts, &desired_mounts)
                .map_err(|error| RunError::bad_request(error.to_string()))?;

            // The provider-visible path set is known without reading a File,
            // Skill, credential, Vault, cache, or Repository. Persist that
            // conservative reservation first, under the exact aggregate
            // transition identity; every later effect is therefore replayable.
            let source_binding = serde_json::to_string(&environment.lifecycle().handle())
                .map_err(crate::realization_error::RealizationError::from)?;
            let reserved_paths = if transition.previous() != transition.desired() {
                desired_manifest
                    .resources
                    .inputs()
                    .iter()
                    .filter(|input| {
                        !previous_manifest
                            .resources
                            .inputs()
                            .iter()
                            .any(|candidate| candidate == *input)
                    })
                    .map(crate::managed_layout::resolved_input_effect_path)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if !reserved_paths.is_empty() {
                let effect = environments.authorize_environment_effect_before_io(peers,
                        thread,
                        &peers.authority.new_environment_effect_intent(thread, awaken_session_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                            transition_fingerprint: transition.operation_fingerprint(),
                        },
                        Some(&source_binding))?,
                    )
                    .await
                    .map_err(awaken_session_types::RunError::from)?;
                match &effect.authorization {
                    awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                        binding,
                        ..
                    } if binding == &source_binding => {}
                    awaken_session_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                        ..
                    } => {
                        return Err(RunError::internal(
                            "root-committed Resource reservation does not match the live Environment",
                        ));
                    }
                    awaken_session_contract::SessionEnvironmentEffectAuthorization::Authorized { .. }
                    | awaken_session_contract::SessionEnvironmentEffectAuthorization::Unowned { .. } => {
                        for path in reserved_paths {
                            environment
                                .projection().reserve_owned_path(&path)
                                .map_err(crate::realization_error::RealizationError::from)?;
                        }
                        let binding = serde_json::to_string(&environment.lifecycle().handle())
                            .map_err(crate::realization_error::RealizationError::from)?;
                        environments.persist_authorized_environment_binding(peers, thread, binding, &effect)
                            .await
                            .map_err(|failure| {
                                awaken_session_types::RunError::from(failure.error)
                            })?;
                    }
                }
            }
        }
        let effect_key = resource_requirement_effect_key(transition, claim, realization_lease);
        let compiled = staging
            .retained_resource_requirements(
                thread,
                &desired_manifest,
                claim,
                realization_lease,
                &effect_key,
            )
            .await?;
        if live_environment.is_none() {
            // Cold requirements are now available for provider create-time
            // mount selection. The desired manifest becomes visible only after
            // the durably bound Environment re-enters this transition and its
            // physical effects succeed.
            return Ok(());
        }
        let new = &compiled.staged;
        let projection_update = match &live_environment {
            Some(environment) => environment
                .projection()
                .begin_live_projection_update()
                .await
                .map_err(crate::realization_error::RealizationError::from)?,
            None => None,
        };
        if let Some(environment) = &live_environment {
            // A Resource generation can advance without changing its Skill
            // pins. Preserve that completed tree; replay actual replacements
            // and cold/fresh/adopted realization through the same installer.
            let reconcile_skills = previous_manifest.resources.skills()
                != desired_manifest.resources.skills()
                || installed_manifest.is_none()
                || reconciliation != EnvironmentResourceReconciliation::None;
            // Transition replay cause/effect table:
            // C1 desired path differs from the aggregate's previous generation;
            // C2 reservation CAS succeeds; C3 physical removal/attach/Git
            // succeeds; C4 logical manifest publishes. R1 !C1 => no receipt or
            // physical mutation. R2 C1+!C2 => zero physical effect. R3 C1+C2+!C3
            // => pending remains and the exact previous→desired transition
            // replays. R4 C1+C2+C3+!C4 => the same idempotent transition replays.
            // R5 all => desired may become active. The existing Environment
            // binding is the WAL; the Session Resource aggregate remains the
            // sole completion authority.
            // Realize the desired live projection before committing its logical
            // manifest. Every operation is idempotent, so a failed attempt leaves
            // the prior manifest authoritative and the persisted pending generation
            // can safely retry without mistaking an unrealized mount for success.
            if reconcile_skills {
                environment
                    .projection()
                    .remove_projection_path(awaken_session_configuration_contract::skill_delivery::DELIVERED_SKILLS_SUBDIR)
                    .await
                    .map_err(crate::realization_error::RealizationError::from)?;
            }
            if projection_change.removes_previous() {
                for input in previous_manifest.resources.inputs() {
                    if !desired_manifest
                        .resources
                        .inputs()
                        .iter()
                        .any(|candidate| candidate == input)
                    {
                        let path = crate::managed_layout::resolved_input_effect_path(input);
                        environment
                            .projection()
                            .remove_projection_path(&path)
                            .await
                            .map_err(crate::realization_error::RealizationError::from)?;
                    }
                }
            }
            if reconciliation == EnvironmentResourceReconciliation::Adopted {
                environment
                    .projection()
                    .reconcile_adopted_mounts(&self.thread_session_mounts(thread)?)
                    .await
                    .map_err(crate::realization_error::RealizationError::from)?;
            }
            if projection_change.installs_desired()
                || reconciliation != EnvironmentResourceReconciliation::None
            {
                // Physical Skill installation belongs to this same Resource
                // completion edge for eager, lazy and adopted Environments.
                // Context construction only derives the Managed registry; it
                // must not reinstall a second projection after publication.
                if reconcile_skills {
                    let versions = sessions
                        .read_realization(thread, |r| r.staging().skills.clone())?
                        .flatten()
                        .ok_or_else(|| {
                            RunError::internal("compiled pinned Skill bytes are missing")
                        })?;
                    for version in &versions {
                        crate::skill_delivery::materialize_delivered_skill(environment, version)
                            .await
                            .map_err(RunError::internal)?;
                    }
                }
                for input in desired_manifest.resources.inputs() {
                    if (installed_manifest.is_some()
                        && matches!(
                            input.source,
                            session_resource::ResolvedInputSource::MemoryStore { .. }
                        ))
                        || (reconciliation == EnvironmentResourceReconciliation::None
                            && previous_manifest
                                .resources
                                .inputs()
                                .iter()
                                .any(|candidate| candidate == input))
                    {
                        // Both proved rows skip physical replay: installed
                        // editable Memory retains its bytes/heads; unchanged
                        // inputs with no adoption/freshness cause remain active.
                        // Adoption separately reconciles provider participants.
                        continue;
                    }
                    let path = crate::managed_layout::resolved_input_effect_path(input);
                    match &input.source {
                        session_resource::ResolvedInputSource::File { .. }
                        | session_resource::ResolvedInputSource::MemoryStore { .. } => {
                            let mount = new
                                .mounts
                                .iter()
                                .find(|mount| mount.mount_path == path)
                                .ok_or_else(|| {
                                    RunError::internal(format!(
                                        "compiled Resource mount `{path}` is missing"
                                    ))
                                })?;
                            if reconciliation == EnvironmentResourceReconciliation::Adopted
                                || (reconciliation == EnvironmentResourceReconciliation::Fresh
                                    && resource_mount_is_create_time(mount))
                            {
                                continue;
                            }
                            environment
                                .projection()
                                .attach_mount(mount.clone())
                                .await
                                .map_err(crate::realization_error::RealizationError::from)?;
                        }
                        session_resource::ResolvedInputSource::Repository { .. } => {
                            let retain = reconciliation
                                == EnvironmentResourceReconciliation::Adopted
                                && previous_manifest
                                    .resources
                                    .inputs()
                                    .iter()
                                    .any(|previous| previous == input);
                            let repository = new
                                .repository_at_path(&path)
                                .map_err(crate::realization_error::RealizationError::from)?;
                            staging
                                .realize_repository_activation(
                                    thread,
                                    environments.publication().local_workspace(),
                                    repository,
                                    &new.binding_checks,
                                    environment.repositories().as_ref(),
                                    if retain {
                                        pc::RepositoryRealizationIntent::Retain
                                    } else {
                                        pc::RepositoryRealizationIntent::Initialize
                                    },
                                )
                                .await?;
                        }
                    }
                }
            }
        }
        staging.register_thread_resource_manifest(thread, desired_manifest)?;
        if let Some(update) = projection_update {
            update.commit();
        }
        sessions.update_realization(thread, |r| {
            r.staging_mut().reconciliation = EnvironmentResourceReconciliation::None;
        })?;
        environments.evict_session_for_rebuild(thread).await?;
        Ok(())
    }
}

/// Desired-only Resource installation of a claimed dispatch.
impl crate::resource_staging::ResourceStaging {
    pub async fn install_dispatched_resources(
        &self,
        peers: crate::peers::Peers<'_>,
        thread: &str,
        manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest,
        claim: Option<&awaken_worker_contract::RunClaim>,
    ) -> Result<(), RunError> {
        let realization_view = self.sessions.clone();
        // Installing a claim's dispatched Resources is the install path.
        realization_view.install_session(thread)?;
        // Resource generation is the innermost Session transition. A complete
        // projection may already hold `lifecycle`, while claimed realization
        // also holds `realization`; this dedicated suffix lock avoids re-entry
        // and keeps compare/realize/publish atomic for every caller.
        let resource_projection = realization_view.lock_resource_projection(thread).await?;
        let staging = self;
        let active = staging.thread_resource_manifest(thread)?;
        let live_environment = peers.environments.session_environment(thread).await?;
        let (expected_binding, projected_transition) = realization_view
            .read_realization(thread, |r| {
                (
                    r.environment().owner.durable_binding().is_some(),
                    r.staging().transition.clone(),
                )
            })?
            .unwrap_or_default();
        if let Some(projected_transition) = projected_transition {
            if projected_transition.desired() != manifest {
                return Err(RunError::unavailable_classified(
                    "session_resource_transition_conflict",
                    "the cold Session already carries a different exact Resource transition",
                ));
            }
            // The complete Session projection already owns the exact physical
            // previous generation. Reuse that command for requirement staging
            // so desired-only dispatch cannot replace it with an inferred
            // Empty→desired transition or a no-op effect identity.
            return self
                .stage_prevalidated_resource_transition_under_resource_projection(
                    resource_projection.held(),
                    peers,
                    thread,
                    &projected_transition,
                    claim,
                    None,
                    expected_binding,
                )
                .await;
        }
        if let Some(active) = &active {
            if active != manifest {
                // Without an aggregate transition, a desired-only envelope
                // cannot prove the physical previous generation. Every
                // replacement must carry that exact command instead.
                return Err(RunError::unavailable_classified(
                    "session_resource_transition_required",
                    "a desired-only Resource manifest cannot replace the active generation",
                ));
            }
            // Re-stage even when the active manifest is unchanged: immutable
            // File bytes, config-version integrity, and credential revocation
            // are live-deny checks at every claimed operation. Staging never
            // republishes completion.
            return self
                .stage_resource_manifest(
                    peers,
                    thread,
                    &manifest.workspace_id,
                    manifest.revision,
                    &manifest.resources,
                    claim,
                )
                .await;
        }
        if live_environment.is_some() || expected_binding {
            return Err(RunError::unavailable_classified(
                "session_resource_transition_required",
                "a desired-only Resource manifest cannot identify the bound Environment's active generation",
            ));
        }
        let empty =
            awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                manifest.workspace_id.clone(),
                0,
                awaken_session_configuration_contract::resource::ResolvedSessionResources::default(
                ),
            );
        let proven_transition =
            awaken_session_configuration_contract::resource::SessionResourceTransition::new(
                empty,
                manifest.clone(),
            )
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        self.stage_prevalidated_resource_transition_under_resource_projection(
            resource_projection.held(),
            peers,
            thread,
            &proven_transition,
            claim,
            None,
            false,
        )
        .await?;
        // No resident or durable binding exists, so Empty→desired is proven
        // rather than inferred from a cache. Persist only the command projection;
        // the active manifest remains absent until physical realization succeeds.
        realization_view.update_realization(thread, |r| {
            if r.staging().transition.is_none() {
                r.staging_mut().transition = Some(proven_transition);
            }
        })?;
        Ok(())
    }
}
