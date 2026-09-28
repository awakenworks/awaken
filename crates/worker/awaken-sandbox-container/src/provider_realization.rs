// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Exact physical restoration lifecycle for the canonical container provider.
//!
//! Ordinary create/adopt/rebuild remains owned by `provider.rs`. This module
//! accepts only a canonical `SandboxRestoreRequest`, so it cannot become a
//! parallel environment-realization path.

use super::*;

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    /// Physical restore targets run only the provider keepalive and omit every
    /// independently governed mount recorded by the checkpoint driver.
    pub(super) fn restoration_environment_plan(
        &self,
        spec: &pc::SandboxSpec,
        exclusions: &[String],
    ) -> Result<ContainerPlan, pc::SandboxError> {
        pc::validate_checkpoint_exclusions_for_spec(exclusions, spec)?;
        let mut plan = container_plan_with_allowlist(
            spec,
            &self.default_image,
            &environment_keepalive_command(),
            self.forward_proxy.as_ref(),
            self.allowlist_proxy.as_ref(),
        )
        .map_err(pc::SandboxError::from)?;
        plan.binds.retain(|bind| {
            !exclusions
                .iter()
                .any(|path| path.trim_end_matches('/') == bind.mount_path.trim_end_matches('/'))
        });
        plan.memory_mounts.retain(|mount| {
            !exclusions
                .iter()
                .any(|path| path.trim_end_matches('/') == mount.mount_path.trim_end_matches('/'))
        });
        plan.control_services.clear();
        Ok(plan)
    }

    async fn realize_restore_container(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        request.validate_for_spec(spec)?;
        pc::prepare_environment(spec, &self.runtime_sandbox_capabilities())
            .map_err(pc::SandboxError::from)?;

        let evidence = request.evidence(spec);
        let mut plan =
            self.restoration_environment_plan(spec, &request.checkpoint.excluded_mounts)?;
        let plan_fingerprint = restoration_plan_fingerprint(&plan);
        let runtime_scope = restoration_runtime_scope(&evidence)?;

        // Read before any package or host-staging effect. A replacement process
        // can therefore recover a completed target without reopening sources.
        let recovered = self
            .runtime
            .recover_restore_target(&runtime_scope, &plan, &plan_fingerprint, &evidence)
            .await
            .map_err(pc::SandboxError::from)?;
        if recovered.as_ref().is_some_and(|target| {
            target.disposition != pc::SandboxRestoreTargetDisposition::Recovered
        }) {
            return Err(pc::SandboxError::conflict(
                "restore observation reported a non-recovered physical target",
            ));
        }

        let mut staging = StagedMounts::default();
        if recovered.is_none() {
            if !plan.packages.is_empty() {
                let base_image = match &plan.rootfs {
                    RootfsPlan::Image(reference) => reference.clone(),
                    RootfsPlan::HostUserland => plan.image.clone(),
                    _ => {
                        return Err(pc::SandboxError::unsupported(
                            "package provisioning requires an OCI image rootfs",
                        ));
                    }
                };
                plan.image = if let Some(provisioner) = &self.package_provisioner {
                    provisioner
                        .prepare_package_image(&base_image, &plan.packages, &spec.network)
                        .await
                        .map_err(pc::SandboxError::from)?
                } else {
                    self.runtime
                        .prepare_package_image(&base_image, &plan.packages, &spec.network)
                        .await
                        .map_err(pc::SandboxError::from)?
                };
                plan.rootfs = RootfsPlan::Image(plan.image.clone());
            }
            if self.runtime.uses_host_live_input_bind() {
                live_inputs::stage_host_projection(
                    &spec.scope,
                    &mut plan.binds,
                    &mut staging.guard,
                )?;
            }
        }

        let target = match recovered {
            Some(target) => target,
            None => match self
                .runtime
                .restore_or_adopt(&runtime_scope, &plan, &plan_fingerprint, &evidence)
                .await
            {
                Ok(target) => target,
                Err(error) => {
                    // The runtime contract may have crossed a mutation boundary.
                    // A host locator already exposed to that object must not be
                    // unlinked by this process-local wrapper.
                    if let Some(staging) = staging.guard.as_mut() {
                        staging.retain_for_restoration();
                    }
                    return Err(pc::SandboxError::from(error));
                }
            },
        };
        let container_id = target.container_id;
        let disposition = target.disposition;

        if self.runtime.uses_host_live_input_bind()
            && disposition == pc::SandboxRestoreTargetDisposition::Created
            && let Some(staging) = staging.guard.as_mut()
        {
            staging.retain_for_restoration();
        }

        let runtime_handle = match self.runtime.handle_extra(&container_id).await {
            Ok(handle) => handle,
            Err(error) => {
                if let Some(staging) = staging.guard.as_mut() {
                    staging.retain_for_restoration();
                }
                return Err(pc::SandboxError::from(error.after_mutation()));
            }
        };

        if self.runtime.uses_host_live_input_bind() {
            let physical_staging =
                retained_host_staging(runtime_handle.as_ref()).map_err(pc::SandboxError::from)?;
            let physical_staging = physical_staging.ok_or_else(|| {
                pc::SandboxError::invalid(
                    "host-bind restore target omitted its physical staging locator",
                )
            })?;
            match disposition {
                pc::SandboxRestoreTargetDisposition::Created => {
                    let current = staging.guard.as_mut().ok_or_else(|| {
                        pc::SandboxError::not_found(
                            "created host-bind restore target has no staging directory",
                        )
                    })?;
                    if current.path() != physical_staging.path() {
                        current.retain_for_restoration();
                        return Err(pc::SandboxError::conflict(
                            "created host-bind restore target reported another staging directory",
                        ));
                    }
                    current.retain_for_restoration();
                }
                pc::SandboxRestoreTargetDisposition::Recovered => match staging.guard.as_mut() {
                    Some(current) if current.path() == physical_staging.path() => {
                        current.retain_for_restoration();
                    }
                    _ => staging.guard = Some(physical_staging),
                },
            }
        }

        let observed = self
            .runtime
            .restoration_evidence(&container_id)
            .await
            .map_err(pc::SandboxError::from)?;
        if observed.as_ref() != Some(&evidence) {
            return Err(pc::SandboxError::conflict(
                "container restore target omitted or changed its exact physical evidence",
            ));
        }
        if self
            .runtime
            .restoration_plan_fingerprint(&container_id)
            .await
            .map_err(pc::SandboxError::from)?
            .as_deref()
            != Some(plan_fingerprint.as_str())
        {
            return Err(pc::SandboxError::conflict(
                "container restore target differs from its exact immutable plan",
            ));
        }

        let continuation_excluded_paths = request.checkpoint.excluded_mounts.clone();
        let handle = request.bind_handle(
            spec,
            pc::SandboxHandle::container(
                &spec.scope,
                crate::ContainerSandboxHandleV1 {
                    container_id: container_id.clone(),
                    outputs_path: spec.outputs_path.clone(),
                    base_env: spec.env.clone(),
                    live_input_projection: self.runtime.supports_live_input_projection(),
                    continuation_excluded_paths: continuation_excluded_paths.clone(),
                    runtime_handle: runtime_handle.clone(),
                    sandbox_control_incarnation: None,
                    control_services: Default::default(),
                },
            ),
        )?;
        Ok(ContainerSandbox {
            retained_attempt: None,
            runtime: self.runtime.clone(),
            id: spec.scope.clone(),
            container_id,
            outputs_path: spec.outputs_path.clone(),
            base_env: spec.env.clone(),
            control_services: Default::default(),
            sandbox_control_incarnation: None,
            control_publication: Arc::new(ContainerControlPublicationRegistry::default()),
            blobs: self.blobs.clone(),
            file_store: self.file_store.clone(),
            live_input_projection: self.runtime.supports_live_input_projection(),
            runtime_handle,
            continuation_excluded_paths,
            adoption_fingerprint: None,
            realization_fingerprint: None,
            memory_materializations: Vec::new(),
            owned_paths: std::sync::Mutex::new(Vec::new()),
            adopted_handle: Some(handle),
            realized: Vec::new(),
            recovered: disposition == pc::SandboxRestoreTargetDisposition::Recovered,
            lifecycle: Arc::new(ContainerCleanupState::recovered(
                self.secret_broker
                    .read()
                    .expect("container secret broker lock poisoned")
                    .clone(),
                staging.guard.take(),
            )),
            execs: Arc::default(),
        })
    }

    pub(super) async fn adopt_restored_container_with_spec(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        admit: &(impl Fn() -> Result<(), pc::SandboxError> + Sync),
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        admit()?;
        pc::prepare_environment(spec, &self.runtime_sandbox_capabilities())
            .map_err(pc::SandboxError::from)?;
        let evidence = handle.restoration().cloned().ok_or_else(|| {
            pc::SandboxError::not_found("restored container handle has no exact evidence")
        })?;
        let payload = recovery::decode_handle(handle)?;
        if handle.sandbox_id != spec.scope
            || evidence.sandbox_spec_fingerprint() != pc::sandbox_spec_security_fingerprint(spec)
            || evidence.checkpoint_exclusions_fingerprint()
                != pc::checkpoint_exclusions_fingerprint(&payload.continuation_excluded_paths)
            || pc::validate_checkpoint_exclusions_for_spec(
                &payload.continuation_excluded_paths,
                spec,
            )
            .is_err()
        {
            return Err(pc::SandboxError::conflict(
                "restored container handle differs from the exact SandboxSpec or checkpoint exclusions",
            ));
        }
        if !payload.control_services.is_empty() || payload.sandbox_control_incarnation.is_some() {
            return Err(pc::SandboxError::invalid(
                "restore target must not publish independently governed control services before root CAS",
            ));
        }

        let container_id = payload.container_id;
        if self
            .runtime
            .inspect(&container_id)
            .await
            .map_err(pc::SandboxError::from)?
            == ContainerState::Gone
        {
            return Err(pc::SandboxError::from(RuntimeError::NotFound(container_id)));
        }
        admit()?;
        if self
            .runtime
            .restoration_evidence(&container_id)
            .await
            .map_err(pc::SandboxError::from)?
            .as_ref()
            != Some(&evidence)
        {
            return Err(pc::SandboxError::conflict(
                "container restore evidence differs between physical target and durable handle",
            ));
        }
        admit()?;
        let expected_plan = restoration_plan_fingerprint(
            &self.restoration_environment_plan(spec, &payload.continuation_excluded_paths)?,
        );
        if self
            .runtime
            .restoration_plan_fingerprint(&container_id)
            .await
            .map_err(pc::SandboxError::from)?
            .as_deref()
            != Some(expected_plan.as_str())
        {
            return Err(pc::SandboxError::conflict(
                "container physical realization differs from the exact restore plan",
            ));
        }
        admit()?;
        let observed_runtime_handle = self
            .runtime
            .handle_extra(&container_id)
            .await
            .map_err(pc::SandboxError::from)?;
        if observed_runtime_handle.as_ref() != payload.runtime_handle.as_ref() {
            return Err(pc::SandboxError::conflict(
                "container continuation handle differs between physical target and durable binding",
            ));
        }
        admit()?;
        let recovered_staging = retained_host_staging(payload.runtime_handle.as_ref())
            .map_err(pc::SandboxError::from)?;

        Ok(ContainerSandbox {
            retained_attempt: None,
            runtime: self.runtime.clone(),
            id: handle.sandbox_id.clone(),
            container_id,
            outputs_path: payload.outputs_path,
            base_env: payload.base_env,
            control_services: Default::default(),
            sandbox_control_incarnation: None,
            control_publication: Arc::new(ContainerControlPublicationRegistry::default()),
            blobs: self.blobs.clone(),
            file_store: self.file_store.clone(),
            live_input_projection: payload.live_input_projection,
            runtime_handle: payload.runtime_handle,
            continuation_excluded_paths: payload.continuation_excluded_paths,
            adoption_fingerprint: None,
            realization_fingerprint: None,
            memory_materializations: Vec::new(),
            owned_paths: std::sync::Mutex::new(Vec::new()),
            adopted_handle: Some(handle.clone()),
            realized: Vec::new(),
            recovered: true,
            lifecycle: Arc::new(ContainerCleanupState::recovered(
                self.secret_broker
                    .read()
                    .expect("container secret broker lock poisoned")
                    .clone(),
                recovered_staging,
            )),
            execs: Arc::default(),
        })
    }

    pub(super) async fn acquire_restore_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<ContainerSandbox<R>>, pc::SandboxError> {
        let sandbox = self.realize_restore_container(spec, request).await?;
        let disposition = if sandbox.recovered {
            pc::SandboxRestoreTargetDisposition::Recovered
        } else {
            pc::SandboxRestoreTargetDisposition::Created
        };
        let handle = pc::Sandbox::handle(&sandbox);
        pc::SandboxRestoreTarget::exact(request, spec, sandbox, &handle, disposition)
    }

    pub(super) async fn dispose_restore_target(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        authorization.validate()?;
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))?;
        request.validate_for_spec(spec)?;
        pc::prepare_environment(spec, &self.runtime_sandbox_capabilities())
            .map_err(pc::SandboxError::from)?;
        let plan = self.restoration_environment_plan(spec, &request.checkpoint.excluded_mounts)?;
        let plan_fingerprint = restoration_plan_fingerprint(&plan);
        let evidence = request.evidence(spec);
        let scope = restoration_runtime_scope(&evidence)?;
        self.runtime
            .dispose_restore_target(
                &scope,
                &plan,
                &plan_fingerprint,
                &evidence,
                authorization,
                authority,
            )
            .await
            .map_err(pc::SandboxError::from)?;
        pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))
    }
}
