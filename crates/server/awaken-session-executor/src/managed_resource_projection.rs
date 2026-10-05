// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal entrypoint session_resources.explicit_empty_manifest_revokes_runtime_projection
// @formal entrypoint session_resources.external_io_requires_durable_prepared_state
//! Pure projection from frozen Managed resource inputs to Agent-visible paths and
//! prompt fragments. Keeping this boundary separate prevents protocol path rules
//! from being reimplemented by individual Sandbox adapters.
use std::sync::Arc;

use awaken_credential_materializer::{CredentialRefreshFactory, PinnedCredentialMaterializer};
use awaken_resource_contract::RepositoryBindingVerifier;
use awaken_session_error_contract::RunError;

use crate::managed_input_layout::{
    ManagedLayoutProjection, ManagedLayoutValidationScope, ResourcePromptProjection,
    binding_layout_projection, managed_mount_access, resolved_input_layout_projection,
};
use crate::skill_catalog::skill_store_run_error;

mod repository_error;
mod transition;
use repository_error::repository_binding_run_error;

/// One compiled projection from the frozen Session manifest. Standard mounts
/// and optional automatic-memory candidates travel together so installation
/// cannot publish one generation with bindings from another.
pub(crate) struct CompiledEffectiveInputs {
    pub(crate) staged: crate::staged_resources::StagedResources,
    pub(crate) memory_bindings:
        std::collections::HashMap<String, std::sync::Arc<crate::memory::BoundMemory>>,
}
fn validate_preparation_publication(
    slot: &crate::session_slot::SessionRuntimeSlot,
    asserted: Option<&awaken_session_resource_contract::SessionRealizationLease>,
) -> Result<(), RunError> {
    if slot.resource_preparation.is_some()
        && !asserted.is_some_and(|lease| slot.resource_preparation_authorized(lease))
    {
        return Err(RunError::unavailable_classified(
            "session_realization_authority_lost",
            "Resource preparation cannot publish after realization authority ended",
        ));
    }
    Ok(())
}

fn resource_requirement_effect_key(
    transition: &awaken_session_resource_contract::SessionResourceTransition,
    claim: Option<&awaken_dispatch_contract::RunClaim>,
    realization_lease: Option<&awaken_session_resource_contract::SessionRealizationLease>,
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

impl crate::ManagedHost {
    /// Wire the live resource-invariant port used at activation and Memory use.
    /// Configuration was already selected by the Session control plane; this port
    /// only validates trusted Workspace ownership, lifecycle state, and the frozen
    /// config version. It does not make an authorization decision.
    #[must_use]
    pub fn with_resource_validator(
        mut self,
        validator: Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>,
    ) -> Self {
        self.resource_validator = Some(validator);
        self
    }

    /// Install the Repository-specific live binding guard used by a distributed
    /// Worker without granting it Resource Registry database access.
    #[must_use]
    pub fn with_repository_binding_verifier(
        mut self,
        verifier: Arc<
            dyn RepositoryBindingVerifier<awaken_dispatch_contract::ResourceOperationFence>,
        >,
    ) -> Self {
        self.repository_binding_verifier = Some(verifier);
        self
    }

    /// Realize an already-resolved, secret-free manifest. The pinned Memory/
    /// Repository configuration in `inputs` remains authoritative; the per-item
    /// validation in `stage_resolved_input` checks only current ownership/state
    /// and the frozen version's integrity. No Agent binding or current config is
    /// configured here.
    async fn compile_effective_inputs(
        &self,
        thread: &str,
        workspace: &str,
        inputs: &awaken_session_resource_contract::ResolvedSessionResources,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(
            &str,
            u64,
            &awaken_session_resource_contract::SessionRealizationLease,
        )>,
    ) -> Result<CompiledEffectiveInputs, RunError> {
        let mut all = crate::staged_resources::StagedResources::default();
        let mut memory_bindings = std::collections::HashMap::new();
        let environment = self
            .host
            .session_slots
            .read(thread, |slot| slot.environment_projection.clone())
            .flatten();
        for input in inputs.inputs() {
            let one = self
                .stage_resolved_input(workspace, input, claim, realization, environment.as_ref())
                .await?;
            // Read this exact projection before merging it. Two bindings may
            // legally reference the same store with different access, and a
            // prior mount must never become the authority for the later one.
            let materialization_reference = one.mounts.iter().find_map(|mount| {
                if let awaken_provisioning_contract::MountSource::MemoryStore {
                    materialization_reference,
                    ..
                } = &mount.source
                {
                    materialization_reference.clone()
                } else {
                    None
                }
            });
            if let Some((binding_id, memory)) = self
                .compile_memory_binding(thread, workspace, input, materialization_reference)
                .await?
            {
                memory_bindings.insert(binding_id, memory);
            }
            all.mounts.extend(one.mounts);
            all.prompts.extend(one.prompts);
            all.resource_prompts.extend(one.resource_prompts);
            all.binding_checks.extend(one.binding_checks);
            all.repositories.extend(one.repositories);
        }

        Ok(CompiledEffectiveInputs {
            staged: all,
            memory_bindings,
        })
    }

    /// Compile the one Memory-specific leaf shared by ordinary Session staging
    /// and post-commit recovery from a frozen dispatch. The caller owns Resource
    /// selection and may install the result into a resident Session slot; this
    /// leaf only binds one already-resolved input and never opens an Environment.
    pub(crate) async fn compile_memory_binding(
        &self,
        session_thread: &str,
        workspace: &str,
        input: &awaken_session_resource_contract::ResolvedInput,
        materialization_reference: Option<String>,
    ) -> Result<Option<(String, Arc<crate::memory::BoundMemory>)>, RunError> {
        let awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
            memory_store_id,
            config,
        } = &input.source
        else {
            return Ok(None);
        };
        let writable = input.access == awaken_resource_contract::ResourceAccess::ReadWrite;
        if let Some(reference) = &materialization_reference {
            // Remote Memory claim decision table: active + exact config =>
            // snapshot preflight succeeds; archived/config-changed/stale claim
            // fails before the mounter reuses a prior projection.
            self.host
                .memory_repository()
                .snapshot_heads(reference)
                .await
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        let handle = self.host.platform_memory_handle(
            materialization_reference
                .clone()
                .unwrap_or_else(|| memory_store_id.to_string()),
            writable,
        );
        let resource_validator = if materialization_reference.is_some() {
            None
        } else {
            Some(
                self.resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "Memory extraction requires a configured resource binding validator",
                        )
                    })?
                    .clone(),
            )
        };
        Ok(Some((
            input.binding_id.to_string(),
            Arc::new(self.host.resources.memory.bind(
                session_thread,
                workspace,
                handle,
                resource_validator,
                config,
                writable,
            )),
        )))
    }

    /// Publish the completed active generation. Callers must have completed the
    /// exact aggregate-authored physical transition before entering this edge;
    /// desired-only staging is deliberately unable to reach it.
    fn publish_active_resource_manifest(
        &self,
        thread: &str,
        manifest: awaken_session_resource_contract::SessionResourceManifest,
    ) {
        self.host
            .register_thread_resource_manifest(thread, manifest);
    }

    /// Install one already-resolved Session resource manifest. This is shared by
    /// managed Session creation and cold durable workers; neither path reads Agent
    /// defaults or selects a newer mutable-resource configuration.
    pub(crate) async fn stage_resource_manifest(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        resources: &awaken_session_resource_contract::ResolvedSessionResources,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
    ) -> Result<(), RunError> {
        // Full-manifest preflight must precede even process-local Workspace or
        // Skill projection writes. Per-item validation below remains the final
        // effect-edge fence for defense in depth.
        self.validate_effective_resource_layout(thread, resources)?;
        self.stage_resource_manifest_after_preflight(
            thread,
            workspace,
            resource_revision,
            resources,
            claim,
        )
        .await
    }

    async fn stage_resource_manifest_after_preflight(
        &self,
        thread: &str,
        workspace: &str,
        resource_revision: u64,
        resources: &awaken_session_resource_contract::ResolvedSessionResources,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
    ) -> Result<(), RunError> {
        let desired = awaken_session_resource_contract::SessionResourceManifest::at_revision(
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
            &awaken_session_resource_contract::SessionResourceTransition::new(
                desired.clone(),
                desired.clone(),
            )
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
    pub(crate) async fn stage_prevalidated_resource_transition_under_resource_projection(
        &self,
        thread: &str,
        transition: &awaken_session_resource_contract::SessionResourceTransition,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization_lease: Option<&awaken_session_resource_contract::SessionRealizationLease>,
        prospective_environment_binding: bool,
    ) -> Result<(), RunError> {
        let installed = self.host.thread_resource_manifest(thread);
        if installed.as_ref().is_some_and(|installed| {
            installed != transition.previous() && installed != transition.desired()
        }) {
            return Err(RunError::unavailable_classified(
                "session_resource_transition_conflict",
                "the resident active Resource generation is outside the aggregate transition",
            ));
        }
        let live_environment = self.host.session_environment(thread).await;
        let awaiting_adoption = live_environment.is_none()
            && (prospective_environment_binding
                || self
                    .host
                    .session_slots
                    .read(thread, |slot| {
                        slot.environment_owner.durable_binding().is_some()
                    })
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
            let versions = self
                .load_pinned_skill_requirements(
                    transition.desired(),
                    claim,
                    realization_lease.map(|lease| (thread, transition.desired().revision, lease)),
                )
                .await?;
            self.host.session_slots.update(thread, |slot| {
                validate_preparation_publication(slot, realization_lease)?;
                slot.skills = Some(versions);
                Ok(())
            })?;
            return Ok(());
        }
        let effect_key = resource_requirement_effect_key(transition, claim, realization_lease);
        self.stage_resource_requirements(
            thread,
            transition.desired(),
            claim,
            realization_lease.map(|lease| (thread, transition.desired().revision, lease)),
            effect_key,
        )
        .await
    }

    async fn stage_resource_requirements(
        &self,
        thread: &str,
        desired: &awaken_session_resource_contract::SessionResourceManifest,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(
            &str,
            u64,
            &awaken_session_resource_contract::SessionRealizationLease,
        )>,
        effect_key: String,
    ) -> Result<(), RunError> {
        // Unclaimed replay may reuse the exact already-compiled cold
        // requirements. Claimed staging intentionally revalidates immutable
        // bytes/configuration on every ingress operation before refreshing the
        // same cache entry.
        if claim.is_none()
            && self
                .host
                .session_slots
                .read(thread, |slot| {
                    slot.staged_resource_effect_key.as_deref() == Some(effect_key.as_str())
                })
                .unwrap_or(false)
        {
            return Ok(());
        }
        let workspace = desired.workspace_id.as_str();
        let resources = &desired.resources;
        let versions = self
            .load_pinned_skill_requirements(desired, claim, realization)
            .await?;
        let compiled = self
            .compile_effective_inputs(thread, workspace, resources, claim, realization)
            .await?;
        // Publish only after every fallible Resource/Skill read has succeeded.
        // The slot then exposes one complete set of cold requirements; failed
        // compilation leaves no Workspace, prompt, binding, or active-manifest
        // residue. Only the exact physical transition publishes the manifest.
        self.host.session_slots.update(thread, |slot| {
            // The lease check and correlated cache publication share the slot
            // lock with renewal rejection. Neither a late read nor cancellation
            // may publish requirements after that rejection has linearized.
            validate_preparation_publication(slot, realization.map(|(_, _, lease)| lease))?;
            slot.workspace = Some(workspace.to_owned());
            slot.resources = compiled.staged;
            slot.memory_bindings = compiled.memory_bindings;
            slot.memory = None;
            slot.skills = Some(versions);
            slot.staged_resource_effect_key = Some(effect_key);
            Ok(())
        })?;
        Ok(())
    }

    /// Resolve the exact Skill bytes selected by one aggregate-authored
    /// Resource generation. Both dispatch-only semantic recovery and complete
    /// physical requirement compilation enter here, so a cold replica cannot
    /// invent a second catalog or hash-validation path.
    async fn load_pinned_skill_requirements(
        &self,
        desired: &awaken_session_resource_contract::SessionResourceManifest,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(
            &str,
            u64,
            &awaken_session_resource_contract::SessionRealizationLease,
        )>,
    ) -> Result<Vec<awaken_resource_contract::SkillVersion>, RunError> {
        let skill_realization = if self.host.is_database_less_worker() {
            realization
        } else {
            None
        };
        self.host
            .tools
            .skills
            .load_pinned(
                desired.workspace_id.as_str(),
                desired.resources.skills(),
                claim,
                skill_realization,
            )
            .await
            .map_err(skill_store_run_error)
    }

    pub(crate) async fn validate_thread_resource_bindings(
        &self,
        thread: &str,
    ) -> Result<(), RunError> {
        use crate::staged_resources::ResourceBindingCheck;

        // This method is entered only through the SessionRuntime application
        // port. Preserve that neutral identity before dispatch so a claiming
        // Worker enters the frozen Session realization path.
        self.host
            .session_slots
            .update(thread, |slot| slot.session_dispatch = true);
        let checks = self
            .host
            .session_slots
            .read(thread, |slot| slot.resources.binding_checks.clone())
            .unwrap_or_default();
        if checks.is_empty() {
            return Ok(());
        }
        let workspace = self.host.thread_workspace(thread);
        for check in checks {
            match check {
                ResourceBindingCheck::MemoryStore {
                    memory_store_id,
                    config_version,
                } => self
                    .resource_validator
                    .as_ref()
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "memory resources require a configured resource binding validator",
                        )
                    })?
                    .verify_memory_binding(&workspace, &memory_store_id, config_version)
                    .await
                    .map_err(|error| RunError::bad_request(error.to_string()))?,
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

    /// Wire runtime credential injection for the already-frozen Session bindings
    /// and Repository realization, in this crate's own unit tests. Other
    /// compositions install the materializer they built from their Vault.
    #[cfg(test)]
    #[must_use]
    pub fn with_credentials(
        self,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    ) -> Self {
        self.with_credential_materializer(PinnedCredentialMaterializer::new(
            Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                credentials,
                secrets,
                awaken_time::system_clock(),
            )),
            awaken_time::system_clock(),
        ))
    }

    /// Reuse the process startup's canonical exact materializer for MCP and
    /// Repository realization instead of constructing a peer over the same stores.
    #[must_use]
    pub fn with_credential_materializer(
        mut self,
        materializer: PinnedCredentialMaterializer,
    ) -> Self {
        self.credentials = Some(materializer);
        self
    }

    /// Install the credential adapter's exact OAuth refresh port. The Host
    /// retains only this factory and never receives Credential/Secret Store
    /// handles.
    #[must_use]
    pub fn with_credential_refresh_factory(
        mut self,
        factory: Arc<dyn CredentialRefreshFactory>,
    ) -> Self {
        self.credential_refresh_factory = Some(factory);
        self
    }

    /// Install the hosted deployment's platform MCP egress mediator.
    ///
    /// Without it, a Platform-held MCP credential is refused: the Host never
    /// falls back to resolving upstream material it is not authorized to hold.
    #[must_use]
    pub fn with_platform_mcp_egress_provisioner(
        mut self,
        provisioner: Arc<dyn awaken_credential_materializer::PlatformMcpEgressProvisioner>,
    ) -> Self {
        self.platform_mcp_egress = Some(provisioner);
        self
    }

    /// Replace the local Host MCP realization adapter with one downstream
    /// implementation of the same exact-generation Session port. This is the
    /// sole injection seam used by durable Worker commands; desired state and
    /// credential selection remain outside the implementation.
    #[must_use]
    pub fn with_mcp_attachment_realizer(
        mut self,
        realizer: Arc<dyn awaken_session_execution_contract::McpAttachmentRealizer>,
    ) -> Self {
        self.mcp_realizer = Some(realizer);
        self
    }

    pub(crate) async fn apply_session_inputs_with_context(
        &self,
        thread: &str,
        transition: &awaken_session_resource_contract::SessionResourceTransition,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization_lease: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        let lifecycle = self
            .host
            .session_slots
            .update(thread, |slot| slot.lifecycle.clone());
        let _lifecycle = lifecycle.lock().await;
        self.apply_session_inputs_under_lifecycle(thread, transition, claim, realization_lease)
            .await
    }

    /// Execute the one Resource transition while the caller holds the Session
    /// lifecycle fence. Fresh Environment creation uses this exact body after
    /// its handle is durably published; ordinary reconciliation enters through
    /// `apply_session_inputs_with_context`, so no second physical apply path is
    /// introduced.
    pub(crate) async fn apply_session_inputs_under_lifecycle(
        &self,
        thread: &str,
        transition: &awaken_session_resource_contract::SessionResourceTransition,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization_lease: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        let resource_projection = self
            .host
            .session_slots
            .update(thread, |slot| slot.resource_projection.clone());
        let _resource_projection = resource_projection.lock().await;
        self.apply_session_inputs_body(thread, transition, claim, realization_lease)
            .await
    }

    /// Sole physical Resource transition body. Lock acquisition is deliberately
    /// absent here: ordinary callers enter through lifecycle -> resource, while
    /// Environment creation/adoption already holding lifecycle enters through
    /// the resource-locking suffix above.
    async fn apply_session_inputs_body(
        &self,
        thread: &str,
        transition: &awaken_session_resource_contract::SessionResourceTransition,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization_lease: Option<&awaken_session_resource_contract::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        let previous_manifest = transition.previous();
        let desired_manifest = transition.desired();
        let inputs = &desired_manifest.resources;
        // Read baseline/environment/provider projection while the same lifecycle
        // fence excludes a concurrent projection install. Validation still
        // precedes every Workspace, Skill, File, Repository, cache, or provider
        // effect below.
        self.validate_effective_resource_layout(thread, inputs)?;
        self.host.session_slots.update(thread, |slot| {
            slot.resource_transition = Some(transition.clone());
        });
        let live_environment = self.host.session_environment(thread).await;
        let awaiting_adoption = live_environment.is_none()
            && self
                .host
                .session_slots
                .read(thread, |slot| {
                    slot.environment_owner.durable_binding().is_some()
                })
                .unwrap_or(false);
        let (baseline_is_staged, session_dispatch, reconciliation, environment_projection) = self
            .host
            .session_slots
            .read(thread, |slot| {
                (
                    slot.baseline.is_some(),
                    slot.session_dispatch,
                    slot.environment_resource_reconciliation,
                    slot.environment_projection.clone(),
                )
            })
            .unwrap_or_default();
        // Direct protocol Sessions do not own a frozen Managed baseline, but
        // their Environment still enters through this one aggregate transition
        // authority. Admit only the exact empty legacy generation synthesized
        // by `SharedHost::run`; any claimed, Managed, non-empty, advancing, or
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
        let installed_manifest = self.host.thread_resource_manifest(thread);
        let old_memory: Vec<_> = installed_manifest
            .as_ref()
            .unwrap_or(previous_manifest)
            .resources
            .inputs()
            .iter()
            .filter(|input| {
                matches!(
                    input.source,
                    awaken_session_resource_contract::ResolvedInputSource::MemoryStore { .. }
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
                    awaken_session_resource_contract::ResolvedInputSource::MemoryStore { .. }
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
            && reconciliation == crate::staged_resources::EnvironmentResourceReconciliation::None
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
            && reconciliation == crate::staged_resources::EnvironmentResourceReconciliation::None
        {
            // Preparation already staged the exact desired requirements. A
            // same-generation adoption is continuity, not a replacement: the
            // provider adoption hook reattaches Namespace mounts and no Resource
            // path may be removed before terminal publication/cleanup.
            self.host
                .register_thread_resource_manifest(thread, desired_manifest);
            return Ok(());
        }
        let mut previous_mounts = previous_manifest
            .resources
            .inputs()
            .iter()
            .filter(|input| {
                !matches!(
                    input.source,
                    awaken_session_resource_contract::ResolvedInputSource::MemoryStore { .. }
                )
            })
            .chain(old_memory.iter())
            .filter_map(|input| {
                crate::managed_input_layout::resolved_input_validation_mount(
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
                crate::managed_input_layout::resolved_input_validation_mount(
                    input,
                    environment_projection.as_ref(),
                )
            })
            .collect::<Vec<_>>();
        if reconciliation == crate::staged_resources::EnvironmentResourceReconciliation::Fresh {
            previous_mounts.extend(
                desired_mounts
                    .iter()
                    .filter(|mount| crate::provisioning::resource_mount_is_create_time(mount))
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
                .map_err(|error| RunError::internal(error.to_string()))?;
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
                    .map(crate::managed_input_layout::resolved_input_effect_path)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if !reserved_paths.is_empty() {
                let effect = self
                    .host
                    .authorize_environment_effect_before_io(
                        thread,
                        awaken_session_state_contract::SessionEnvironmentEffectKind::ResourceProjectionReservation {
                            transition_fingerprint: transition.operation_fingerprint(),
                        },
                        Some(&source_binding),
                    )
                    .await
                    .map_err(crate::managed_adapter_error::to_run_error)?;
                match &effect.authorization {
                    awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                        binding,
                    } if binding == &source_binding => {}
                    awaken_session_state_contract::SessionEnvironmentEffectAuthorization::AlreadyApplied {
                        ..
                    } => {
                        return Err(RunError::internal(
                            "root-committed Resource reservation does not match the live Environment",
                        ));
                    }
                    awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Authorized
                    | awaken_session_state_contract::SessionEnvironmentEffectAuthorization::Unowned => {
                        for path in reserved_paths {
                            environment
                                .projection().reserve_owned_path(&path)
                                .map_err(|error| RunError::internal(error.to_string()))?;
                        }
                        let binding = serde_json::to_string(&environment.lifecycle().handle())
                            .map_err(|error| RunError::internal(error.to_string()))?;
                        self.host
                            .persist_authorized_environment_binding(thread, binding, &effect)
                            .await
                            .map_err(|failure| {
                                crate::managed_adapter_error::to_run_error(failure.error)
                            })?;
                    }
                }
            }
        }
        let effect_key = resource_requirement_effect_key(transition, claim, realization_lease);
        let cache_matches = self
            .host
            .session_slots
            .read(thread, |slot| {
                slot.staged_resource_effect_key.as_deref() == Some(effect_key.as_str())
            })
            .unwrap_or(false);
        if !cache_matches {
            // Cache misses converge on the same cold-requirements compiler used
            // by complete projection staging. This is the only Skill/Resource
            // read and requirements-publication owner; the physical body below
            // consumes its result and publishes only completion.
            self.stage_resource_requirements(
                thread,
                &desired_manifest,
                claim,
                realization_lease.map(|lease| (thread, desired_manifest.revision, lease)),
                effect_key.clone(),
            )
            .await?;
        }
        let compiled = self
            .host
            .session_slots
            .read(thread, |slot| {
                (slot.staged_resource_effect_key.as_deref() == Some(effect_key.as_str())).then(
                    || CompiledEffectiveInputs {
                        staged: slot.resources.clone(),
                        memory_bindings: slot.memory_bindings.clone(),
                    },
                )
            })
            .flatten()
            .ok_or_else(|| {
                RunError::internal(
                    "Session Resource requirements were not retained under the exact transition",
                )
            })?;
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
                .map_err(|error| RunError::internal(error.to_string()))?,
            None => None,
        };
        if let Some(environment) = &live_environment {
            // A Resource generation can advance without changing its Skill
            // pins. Preserve that completed tree; replay actual replacements
            // and cold/fresh/adopted realization through the same installer.
            let reconcile_skills = previous_manifest.resources.skills()
                != desired_manifest.resources.skills()
                || installed_manifest.is_none()
                || reconciliation
                    != crate::staged_resources::EnvironmentResourceReconciliation::None;
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
                    .remove_projection_path(crate::skills::DELIVERED_SKILLS_SUBDIR)
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?;
            }
            if projection_change.removes_previous() {
                for input in previous_manifest.resources.inputs() {
                    if !desired_manifest
                        .resources
                        .inputs()
                        .iter()
                        .any(|candidate| candidate == input)
                    {
                        let path = crate::managed_input_layout::resolved_input_effect_path(input);
                        environment
                            .projection()
                            .remove_projection_path(&path)
                            .await
                            .map_err(|error| RunError::internal(error.to_string()))?;
                    }
                }
            }
            if reconciliation == crate::staged_resources::EnvironmentResourceReconciliation::Adopted
            {
                environment
                    .projection()
                    .reconcile_adopted_mounts(&self.host.thread_session_mounts(thread))
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?;
            }
            if projection_change.installs_desired()
                || reconciliation
                    != crate::staged_resources::EnvironmentResourceReconciliation::None
            {
                // Physical Skill installation belongs to this same Resource
                // completion edge for eager, lazy and adopted Environments.
                // Context construction only derives the Managed registry; it
                // must not reinstall a second projection after publication.
                if reconcile_skills {
                    let versions = self
                        .host
                        .session_slots
                        .read(thread, |slot| slot.skills.clone())
                        .flatten()
                        .ok_or_else(|| {
                            RunError::internal("compiled pinned Skill bytes are missing")
                        })?;
                    for version in &versions {
                        crate::skills::materialize_delivered_skill(environment, version)
                            .await
                            .map_err(RunError::internal)?;
                    }
                }
                for input in desired_manifest.resources.inputs() {
                    if installed_manifest.is_some()
                        && matches!(
                            input.source,
                            awaken_session_resource_contract::ResolvedInputSource::MemoryStore { .. }
                        )
                    {
                        // Admission above proved exact installed Memory. Never
                        // rematerialize its editable Copy or original heads on
                        // an unacknowledged A→B replay. Adoption separately
                        // reconciles provider-owned participants above.
                        continue;
                    }
                    if reconciliation
                        == crate::staged_resources::EnvironmentResourceReconciliation::None
                        && previous_manifest
                            .resources
                            .inputs()
                            .iter()
                            .any(|candidate| candidate == input)
                    {
                        continue;
                    }
                    let path = crate::managed_input_layout::resolved_input_effect_path(input);
                    match &input.source {
                        awaken_session_resource_contract::ResolvedInputSource::File { .. }
                        | awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
                            ..
                        } => {
                            let mount = new
                                .mounts
                                .iter()
                                .find(|mount| mount.mount_path == path)
                                .ok_or_else(|| {
                                    RunError::internal(format!(
                                        "compiled Resource mount `{path}` is missing"
                                    ))
                                })?;
                            if reconciliation
                                == crate::staged_resources::EnvironmentResourceReconciliation::Adopted
                                || (reconciliation
                                    == crate::staged_resources::EnvironmentResourceReconciliation::Fresh
                                    && crate::provisioning::resource_mount_is_create_time(mount))
                            {
                                continue;
                            }
                            environment
                                .projection()
                                .attach_mount(mount.clone())
                                .await
                                .map_err(|error| RunError::internal(error.to_string()))?;
                        }
                        awaken_session_resource_contract::ResolvedInputSource::Repository {
                            ..
                        } => {
                            let repository = new
                                .repositories
                                .iter()
                                .find(|repository| repository.plan.mount_path == path)
                                .ok_or_else(|| {
                                    RunError::internal(format!(
                                        "compiled Repository activation `{path}` is missing"
                                    ))
                                })?;
                            self.host
                                .realize_repository_activation(
                                    thread,
                                    repository,
                                    &new.binding_checks,
                                    environment.repositories().as_ref(),
                                )
                                .await
                                .map_err(|error| RunError::internal(error.to_string()))?;
                        }
                    }
                }
            }
        }
        self.publish_active_resource_manifest(thread, desired_manifest);
        if let Some(update) = projection_update {
            update.commit();
        }
        self.host.session_slots.update(thread, |slot| {
            slot.environment_resource_reconciliation =
                crate::staged_resources::EnvironmentResourceReconciliation::None;
        });
        self.host.evict_session_for_rebuild(thread).await;
        Ok(())
    }
}

impl crate::SharedHost {
    fn validate_managed_layout_inputs(
        &self,
        thread: &str,
        inputs: impl IntoIterator<Item = ManagedLayoutProjection>,
        scope: ManagedLayoutValidationScope<'_>,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> Result<(), crate::host::HostError> {
        let (resident_baseline, content_delivery, resident_environment, live_environment) = self
            .session_slots
            .read(thread, |slot| {
                (
                    slot.baseline.clone(),
                    slot.content_delivery,
                    slot.environment_projection.clone(),
                    slot.environment_owner.resident(),
                )
            })
            .unwrap_or_default();
        let environment = environment.or(resident_environment.as_ref());
        let mut mounts = Vec::new();
        let mut repository_paths = Vec::new();
        for input in inputs {
            match input {
                ManagedLayoutProjection::Mount(mount) => {
                    mounts.push(mount.validation_requirement(environment));
                }
                ManagedLayoutProjection::Repository { mount_path, .. } => {
                    repository_paths.push(mount_path)
                }
            }
        }
        let spec = self.sandbox_spec_for_projected_layout(
            thread,
            crate::provisioning::ProjectedSandboxLayout {
                resource_mounts: mounts,
                has_repositories: !repository_paths.is_empty(),
                baseline_mounts: baseline_mounts.or_else(|| {
                    resident_baseline
                        .as_ref()
                        .map(|baseline| baseline.mounts.as_slice())
                }),
                baseline_env: baseline_env.or_else(|| {
                    resident_baseline
                        .as_ref()
                        .map(|baseline| baseline.env.as_slice())
                }),
                content_delivery,
                environment,
                network_isolation: scope.network_isolation(),
            },
        );
        let historical_owned_paths = live_environment.as_ref().and_then(|environment| {
            environment
                .lifecycle()
                .handle()
                .owned_paths()
                .map(<[String]>::to_vec)
        });
        if live_environment.is_some()
            && !repository_paths.is_empty()
            && historical_owned_paths.is_none()
        {
            return Err(crate::host::HostError::internal(
                "legacy Session sandbox handle lacks complete owned-path evidence required for live Repository replacement",
            ));
        }
        let historical_owned_paths = historical_owned_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let repository_paths = repository_paths
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        match scope {
            ManagedLayoutValidationScope::Structural => {
                awaken_provisioning_contract::validate_repository_sandbox_adoption_layout(
                    &repository_paths,
                    &spec,
                    &historical_owned_paths,
                )
                .map_err(|error| crate::host::HostError::internal(error.to_string()))
            }
            ManagedLayoutValidationScope::Exact(provider) => self
                .validate_repository_environment_adoption_paths(
                    provider,
                    &spec,
                    &repository_paths,
                    &historical_owned_paths,
                )
                .map(|_| ()),
        }
    }

    pub(crate) fn validate_managed_resource_layout(
        &self,
        thread: &str,
        resources: &awaken_session_resource_contract::ResolvedSessionResources,
        provider: &crate::session_environment::SessionEnvironmentProvider,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> Result<(), crate::host::HostError> {
        self.validate_managed_layout_inputs(
            thread,
            resources
                .inputs()
                .iter()
                .map(resolved_input_layout_projection),
            ManagedLayoutValidationScope::Exact(provider),
            baseline_mounts,
            baseline_env,
            environment,
        )
    }

    pub(crate) fn validate_structural_managed_resource_layout(
        &self,
        thread: &str,
        resources: &awaken_session_resource_contract::ResolvedSessionResources,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> Result<(), crate::host::HostError> {
        self.validate_managed_layout_inputs(
            thread,
            resources
                .inputs()
                .iter()
                .map(resolved_input_layout_projection),
            ManagedLayoutValidationScope::Structural,
            baseline_mounts,
            baseline_env,
            environment,
        )
    }

    pub(crate) fn validate_managed_binding_layout(
        &self,
        thread: &str,
        resources: &[awaken_resource_contract::InputBinding],
        provider: &crate::session_environment::SessionEnvironmentProvider,
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> Result<(), crate::host::HostError> {
        self.validate_managed_layout_inputs(
            thread,
            resources.iter().map(binding_layout_projection),
            ManagedLayoutValidationScope::Exact(provider),
            baseline_mounts,
            baseline_env,
            environment,
        )
    }

    pub(crate) fn validate_structural_managed_binding_layout(
        &self,
        thread: &str,
        resources: &[awaken_resource_contract::InputBinding],
        baseline_mounts: Option<&[awaken_provisioning_contract::MountRequirement]>,
        baseline_env: Option<&[awaken_provisioning_contract::EnvVar]>,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> Result<(), crate::host::HostError> {
        self.validate_managed_layout_inputs(
            thread,
            resources.iter().map(binding_layout_projection),
            ManagedLayoutValidationScope::Structural,
            baseline_mounts,
            baseline_env,
            environment,
        )
    }
}

impl crate::ManagedHost {
    pub(super) fn validate_prospective_session_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        let environment = crate::provisioning::project_environment(&layout.environment);
        let (_, candidate) = self
            .host
            .resolve_canonical_session_projection(
                &layout.workspace_id,
                crate::host::CanonicalSessionProjection::Layout(layout),
                None,
                None,
            )
            .map_err(|error| {
                awaken_session_error_contract::RunError::bad_request(error.to_string())
            })?;
        let result = if layout.runtime_placement
            == awaken_session_configuration_contract::SessionRuntimePlacement::Worker
        {
            self.host.validate_structural_managed_binding_layout(
                thread,
                &layout.resources,
                Some(&layout.mounts),
                Some(&layout.env),
                Some(&environment),
            )
        } else {
            let provider = if let Some(candidate) = candidate.as_ref() {
                self.host
                    .session_environment_provider_for_thread(thread, candidate.provisioning())
            } else {
                self.host.session_environment_provider_for_thread(
                    thread,
                    &awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor,
                )
            }
            .map_err(|error| {
                awaken_session_error_contract::RunError::bad_request(error.to_string())
            })?;
            self.host.validate_managed_binding_layout(
                thread,
                &layout.resources,
                provider,
                Some(&layout.mounts),
                Some(&layout.env),
                Some(&environment),
            )
        };
        result.map_err(|error| {
            awaken_session_error_contract::RunError::bad_request(error.to_string())
        })
    }

    pub(super) fn validate_effective_resource_layout(
        &self,
        thread: &str,
        resources: &awaken_session_resource_contract::ResolvedSessionResources,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        let provider = self
            .host
            .projected_session_environment_provider(thread, None)
            .map_err(|error| {
                awaken_session_error_contract::RunError::bad_request(error.to_string())
            })?;
        self.host
            .validate_managed_resource_layout(thread, resources, provider, None, None, None)
            .map_err(|error| {
                awaken_session_error_contract::RunError::bad_request(error.to_string())
            })
    }

    /// Compile one frozen Managed input into the provisioning vocabulary. This
    /// projection owns protocol paths and secret-free credential pins; the
    /// Repository effect edge owns the one typed material translation. Sandbox
    /// adapters consume only the resulting neutral requirements.
    pub(super) async fn stage_resolved_input(
        &self,
        workspace: &str,
        input: &awaken_session_resource_contract::ResolvedInput,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
        realization: Option<(
            &str,
            u64,
            &awaken_session_resource_contract::SessionRealizationLease,
        )>,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_error_contract::RunError>
    {
        let authority = if let (Some((session_id, resource_revision, lease)), true) =
            (realization, self.host.is_database_less_worker())
        {
            Some(
                awaken_dispatch_contract::ResourceOperationFence::SessionMaterialization(
                    awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
                        session_id,
                        resource_revision,
                        input.clone(),
                        lease.clone(),
                    )
                    .map_err(awaken_session_error_contract::RunError::bad_request)?,
                ),
            )
        } else {
            claim.map(|claim| {
                awaken_dispatch_contract::ResourceOperationFence::Run(claim.clone())
            })
        };
        self.stage_resolved_input_with_resource_authority(
            workspace,
            input,
            authority.as_ref(),
            realization,
            environment,
        )
        .await
    }

    /// Compile the exact Repository input carried by a terminal publication
    /// command through the same path as ordinary Session staging. A remote lease
    /// selects the HTTP authority verifier; a local command needs no mutable
    /// catalog re-read. Path, access, credential pin, transport, and
    /// realization-plan projection stay canonical here.
    pub(super) async fn stage_terminal_repository_publication_input(
        &self,
        workspace: &str,
        command: &awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_error_contract::RunError>
    {
        command.intent.validate().map_err(|error| {
            awaken_session_error_contract::RunError::bad_request(error.to_string())
        })?;
        let input = &command.intent.input;
        if !matches!(
            input.source,
            awaken_session_resource_contract::ResolvedInputSource::Repository { .. }
        ) {
            return Err(awaken_session_error_contract::RunError::bad_request(
                "terminal Repository publication input is not a Repository",
            ));
        }
        let fence = awaken_dispatch_contract::ResourceOperationFence::RepositoryPublication {
            command: Box::new(command.clone()),
            lease: lease.clone(),
        };
        self.stage_resolved_input_with_resource_authority(
            workspace,
            input,
            Some(&fence),
            None,
            None,
        )
        .await
    }

    async fn stage_resolved_input_with_resource_authority(
        &self,
        workspace: &str,
        input: &awaken_session_resource_contract::ResolvedInput,
        resource_authority: Option<&awaken_dispatch_contract::ResourceOperationFence>,
        realization: Option<(
            &str,
            u64,
            &awaken_session_resource_contract::SessionRealizationLease,
        )>,
        environment: Option<&crate::frozen_runtime_projection::FrozenEnvironmentRuntimeProjection>,
    ) -> Result<crate::staged_resources::StagedResources, awaken_session_error_contract::RunError>
    {
        use awaken_session_error_contract::RunError;
        use awaken_session_resource_contract::ResolvedInputSource;

        let claim = resource_authority.and_then(|authority| match authority {
            awaken_dispatch_contract::ResourceOperationFence::Run(claim) => Some(claim),
            _ => None,
        });
        let mut staged = crate::staged_resources::StagedResources::default();
        staged
            .resource_prompts
            .push(ResourcePromptProjection::try_from_input(input)?);
        let (mut projected_mount, projected_repository_access) =
            match resolved_input_layout_projection(input) {
                ManagedLayoutProjection::Mount(mount) => (Some(mount), None),
                ManagedLayoutProjection::Repository { access, .. } => (None, Some(access)),
            };

        match &input.source {
            ResolvedInputSource::File { file_id } => {
                // Ordinary staging already chose the exact Run or Session fence.
                // Terminal publication admits only Repository inputs above.
                let content = self
                    .host
                    .resources
                    .file_content_source
                    .read(
                        workspace,
                        file_id.as_str(),
                        &awaken_resource_contract::FileReadPurpose::SessionResource,
                        resource_authority,
                    )
                    .await
                    .map_err(|error| RunError::internal(error.to_string()))?
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "file resource `{file_id}` not found in this workspace"
                        ))
                    })?;
                let actual = awaken_resource_contract::content_id(&content.bytes);
                if actual != content.content_id {
                    return Err(RunError::bad_request(format!(
                        "file resource `{file_id}` content hash mismatch (realized `{actual}`)"
                    )));
                }
                staged.mounts.push(
                    projected_mount
                        .take()
                        .expect("File input has projected mount metadata")
                        .requirement(awaken_provisioning_contract::MountSource::InlineBytes {
                            contents: content.bytes,
                            content_hash: Some(content.content_id),
                        }),
                );
            }
            ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } => {
                let materialization_reference = match (
                    self.host.is_database_less_worker(),
                    claim,
                    realization,
                ) {
                    (true, _, Some(_)) => {
                        let Some(
                            awaken_dispatch_contract::ResourceOperationFence::SessionMaterialization(
                                intent,
                            ),
                        ) = resource_authority
                        else {
                            return Err(RunError::internal(
                                "remote Memory realization lost its admitted Resource fence",
                            ));
                        };
                        Some(
                            awaken_resource_contract::MemoryMaterializationReferenceEncoder::<
                                awaken_session_realization_contract::SessionResourceMaterializationIntent,
                            >::encode(
                                self.host
                                    .worker.memory_reference_encoder
                                    .as_ref()
                                    .ok_or_else(|| {
                                        RunError::internal(
                                            "remote Memory materialization encoder is not configured",
                                        )
                                    })?
                                    .as_ref(),
                                workspace,
                                memory_store_id.as_str(),
                                config.version,
                                input.access,
                                intent,
                            )
                            .map_err(|error| RunError::internal(error.to_string()))?,
                        )
                    }
                    (true, Some(claim), None) => Some(
                        awaken_resource_contract::MemoryMaterializationReferenceEncoder::<
                            awaken_dispatch_contract::RunClaim,
                        >::encode(
                            self.host
                                .worker
                                .memory_reference_encoder
                                .as_ref()
                                .ok_or_else(|| {
                                    RunError::internal(
                                        "remote Memory materialization encoder is not configured",
                                    )
                                })?
                                .as_ref(),
                            workspace,
                            memory_store_id.as_str(),
                            config.version,
                            input.access,
                            claim,
                        )
                        .map_err(|error| RunError::internal(error.to_string()))?,
                    ),
                    (true, None, None) => {
                        return Err(RunError::bad_request(
                            "remote Memory materialization requires a dispatch claim or Session realization lease",
                        ));
                    }
                    (false, _, _) => None,
                };
                if materialization_reference.is_none() {
                    let validator = self.resource_validator.as_ref().ok_or_else(|| {
                        RunError::bad_request(
                            "memory resources require a configured resource binding validator",
                        )
                    })?;
                    validator
                        .verify_memory_binding(workspace, memory_store_id.as_str(), config.version)
                        .await
                        .map_err(|error| RunError::bad_request(error.to_string()))?;
                    staged.binding_checks.push(
                        crate::staged_resources::ResourceBindingCheck::MemoryStore {
                            memory_store_id: memory_store_id.to_string(),
                            config_version: config.version,
                        },
                    );
                }
                // The worker realizes one governed store directory through its
                // MemoryMounter. Resources never receives a principal,
                // role, API key, or policy: the outer authorization/ACL seam has
                // already selected workspace, store, and maximum access.
                staged.mounts.push(
                    projected_mount
                        .take()
                        .expect("Memory input has projected mount metadata")
                        .requirement(awaken_provisioning_contract::MountSource::MemoryStore {
                            store_id: memory_store_id.to_string(),
                            materialization_reference,
                            write_consistency:
                                crate::managed_input_layout::projected_memory_write_consistency(
                                    managed_mount_access(input.access),
                                    environment.map(|projection| &projection.idle_retention),
                                ),
                        }),
                );
            }
            ResolvedInputSource::Repository {
                repository_id,
                config,
                credential: credential_pin,
            } => {
                let mount_access = projected_repository_access
                    .expect("Repository input has Repository layout metadata");
                // Historical aggregates may carry a path that predates the
                // current provider profile. Reject the exact durable value
                // before verifier, credential, transport, or Git work.
                awaken_provisioning_contract::validate_repository_mount_path(&input.mount_path)
                    .map_err(|error| {
                        RunError::bad_request(format!(
                            "repository `{repository_id}` cannot be realized: {error}"
                        ))
                    })?;
                let transport = match (resource_authority, self.host.is_database_less_worker()) {
                    (
                        Some(awaken_dispatch_contract::ResourceOperationFence::RepositoryPublication {
                            ..
                        }),
                        false,
                    ) => Ok(awaken_resource_contract::RepositoryTransport::Direct),
                    (authority, _) => {
                        self.repository_binding_verifier
                            .as_ref()
                            .ok_or_else(|| {
                                RunError::bad_request(
                                    "repository resources require a configured binding verifier",
                                )
                            })?
                            .verify(workspace, repository_id.as_str(), config.version, authority)
                            .await
                    }
                }
                .map_err(repository_binding_run_error)?;
                staged.binding_checks.push(
                    crate::staged_resources::ResourceBindingCheck::Repository {
                        repository_id: repository_id.to_string(),
                        config_version: config.version,
                        remote_url: config.remote_url.clone(),
                        credential_binding: config.credential_binding.clone(),
                        authority: resource_authority.cloned().map(Box::new),
                    },
                );
                // Coordinator staging has no Run claim and cannot authorize a
                // Platform-held Git transport. Retain the exact binding check
                // and immutable Session input; the claiming Worker recompiles
                // it through the existing Gateway authorizer. No executable
                // direct Repository activation may escape this stage.
                if resource_authority.is_none()
                    && let (Some(binding), Some(pin)) = (&config.credential_binding, credential_pin)
                    && pin.selected_plaintext_holder.boundary
                        == awaken_runtime_contract::PlaintextBoundary::Platform
                {
                    pin.validate_for_repository(binding, &config.remote_url)
                        .map_err(|error| {
                            RunError::bad_request(format!(
                                "repository `{repository_id}` credential: {error}"
                            ))
                        })?;
                    return Ok(staged);
                }
                let (transport_url, credential_pin) = match (
                    &config.credential_binding,
                    credential_pin,
                ) {
                    (Some(binding), Some(pin)) => {
                        pin.validate_for_repository(binding, &config.remote_url)
                            .map_err(|error| {
                                RunError::bad_request(format!(
                                    "repository `{repository_id}` credential: {error}"
                                ))
                            })?;
                        match pin.selected_plaintext_holder.boundary {
                            awaken_runtime_contract::PlaintextBoundary::Worker => {
                                return Err(RunError::bad_request(format!(
                                    "repository `{repository_id}` credential cannot be held by the Runtime Host or Worker"
                                )));
                            }
                            awaken_runtime_contract::PlaintextBoundary::Platform => match transport {
                                awaken_resource_contract::RepositoryTransport::GatewayMediated {
                                    remote_url,
                                    ..
                                } => (remote_url, Some(pin.as_ref().clone())),
                                awaken_resource_contract::RepositoryTransport::Direct => {
                                    return Err(RunError::bad_request(format!(
                                        "repository `{repository_id}` Platform credential requires Gateway mediation"
                                    )));
                                }
                            },
                            awaken_runtime_contract::PlaintextBoundary::Workload => {
                                if !matches!(transport, awaken_resource_contract::RepositoryTransport::Direct) {
                                    return Err(RunError::bad_request(format!(
                                        "repository `{repository_id}` Sandbox credential cannot use a mediated transport"
                                    )));
                                }
                                (config.remote_url.clone(), Some(pin.as_ref().clone()))
                            }
                        }
                    }
                    (None, None) => match transport {
                        awaken_resource_contract::RepositoryTransport::Direct => {
                            (config.remote_url.clone(), None)
                        }
                        awaken_resource_contract::RepositoryTransport::GatewayMediated {
                            ..
                        } => {
                            return Err(RunError::bad_request(format!(
                                "repository `{repository_id}` has a mediated transport without a credential pin"
                            )));
                        }
                    },
                    (Some(_), None) => {
                        return Err(RunError::bad_request(format!(
                            "repository `{repository_id}` credential binding has no exact Session pin"
                        )));
                    }
                    (None, Some(_)) => {
                        return Err(RunError::bad_request(format!(
                            "repository `{repository_id}` has a credential pin without a binding"
                        )));
                    }
                };
                let plan = awaken_provisioning_contract::RepositoryRealizationPlan {
                    repository_id: repository_id.to_string(),
                    // `ResolvedInput.mount_path` is the durable Managed-wire truth.
                    // Never turn `/repo` into `/workspace/repo` in an ephemeral
                    // adapter projection: either every provider supports the exact
                    // path or the shared realization contract rejects it.
                    mount_path: input.mount_path.clone(),
                    source_remote_url: config.remote_url.clone(),
                    transport_url,
                    initial_branch: config.initial_branch.clone(),
                    initial_commit: config.initial_commit.clone(),
                    access: mount_access,
                };
                plan.validate_mount_path().map_err(|error| {
                    RunError::bad_request(format!(
                        "repository `{repository_id}` cannot be realized: {error}"
                    ))
                })?;
                staged
                    .repositories
                    .push(crate::staged_resources::RepositoryActivation {
                        plan,
                        credential_pin,
                    });
            }
        }
        Ok(staged)
    }
}
