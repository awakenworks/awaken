//! Container-provider realization and exact physical restoration lifecycle.

use super::*;

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    /// Physical restore targets start only the provider keepalive and omit every
    /// independently governed mount recorded by the checkpoint driver. The
    /// complete SandboxSpec remains bound by restoration evidence; post-CAS
    /// projection belongs to the existing lifecycle owner.
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
        .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
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

    pub(super) async fn realize_container(
        &self,
        spec: &pc::SandboxSpec,
        restore_request: Option<&pc::SandboxRestoreRequest>,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        // Fail closed against our capabilities before touching the runtime.
        pc::prepare_environment(spec, &self.runtime_sandbox_capabilities())
            .map_err(|e| err(RuntimeError::Backend(e.to_string())))?;
        if spec
            .mounts
            .iter()
            .any(pc::MountRequirement::is_secret_writeback)
            && !self.runtime.supports_secret_writeback()
        {
            return Err(err(RuntimeError::Backend(
                "this container runtime cannot persist a writable credential-file mount"
                    .to_string(),
            )));
        }
        // One authoritative planner owns the immutable runtime shape used by
        // create, exact recovery, adoption, and terminal orphan cleanup.
        if let Some(request) = restore_request {
            request.validate_for_spec(spec)?;
        }
        let restoration = restore_request.map(|request| request.evidence(spec));
        let mut plan = match restore_request {
            Some(request) => {
                self.restoration_environment_plan(spec, &request.checkpoint.excluded_mounts)?
            }
            None => self.environment_plan(spec)?,
        };
        let restoration_plan_fingerprint = restoration
            .as_ref()
            .map(|_| restoration_plan_fingerprint(&plan));
        let restoration_scope = restoration
            .as_ref()
            .map(restoration_runtime_scope)
            .transpose()?;
        // Read the stable physical namespace before touching package, Blob,
        // Secret, or Memory sources. A provider restart after physical Complete
        // but before the aggregate CAS must not require those creation inputs to
        // remain available merely to return the same receipt.
        let recovered_target = match (restoration_scope.as_deref(), restoration.as_ref()) {
            (Some(scope), Some(evidence)) => self
                .runtime
                .recover_restore_target(
                    scope,
                    &plan,
                    restoration_plan_fingerprint
                        .as_deref()
                        .expect("restoration plan accompanies evidence"),
                    evidence,
                )
                .await
                .map_err(err)?,
            _ => None,
        };
        if recovered_target.as_ref().is_some_and(|target| {
            target.disposition != pc::SandboxRestoreTargetDisposition::Recovered
        }) {
            return Err(err(RuntimeError::Backend(
                "restore observation reported a non-recovered physical target".into(),
            )));
        }
        let secret_broker = self
            .secret_broker
            .read()
            .expect("container secret broker lock poisoned")
            .clone();
        let mut staging = StagedMounts::default();
        if recovered_target.is_none() {
            if !plan.packages.is_empty() {
                let base_image = match &plan.rootfs {
                    RootfsPlan::Image(reference) => reference.clone(),
                    RootfsPlan::HostUserland => plan.image.clone(),
                    _ => {
                        return Err(err(RuntimeError::Backend(
                            "package provisioning requires an OCI image rootfs".into(),
                        )));
                    }
                };
                plan.image = if let Some(provisioner) = &self.package_provisioner {
                    provisioner
                        .prepare_package_image(&base_image, &plan.packages, &spec.network)
                        .await
                        .map_err(err)?
                } else {
                    self.runtime
                        .prepare_package_image(&base_image, &plan.packages, &spec.network)
                        .await
                        .map_err(err)?
                };
                plan.rootfs = RootfsPlan::Image(plan.image.clone());
            }
            if restore_request.is_none() {
                // Ordinary creation resolves independently governed mount
                // sources through their existing authorities.
                staging = resolve_and_stage(
                    spec,
                    &mut plan.binds,
                    &self.blobs,
                    &self.file_store,
                    &secret_broker,
                    self.runtime.uses_persistent_volume_claims(),
                )
                .await?;
                if self.runtime.uses_host_live_input_bind() {
                    live_inputs::stage_host_projection(
                        &spec.scope,
                        &mut plan.binds,
                        &mut staging.guard,
                    )?;
                }
                let native_memory = self.runtime.has_native_memory_mounts();
                let mounter = self
                    .memory_mounter
                    .read()
                    .expect("container memory mounter lock poisoned")
                    .clone();
                stage_memory_binds(spec, &mut plan, &mut staging, mounter, native_memory).await?;
            } else if self.runtime.uses_host_live_input_bind() {
                // The physical Docker/Podman target needs one empty host-owned
                // locator for exact recovery and cleanup. No File, Secret,
                // MemoryStore, or CacheVolume source is opened before root CAS.
                live_inputs::stage_host_projection(
                    &spec.scope,
                    &mut plan.binds,
                    &mut staging.guard,
                )?;
            }
        }
        let (container_id, restore_disposition) = match recovered_target {
            Some(target) => (target.container_id, Some(target.disposition)),
            None => match restoration.as_ref() {
                Some(evidence) => match self
                    .runtime
                    .restore_or_adopt(
                        restoration_scope
                            .as_deref()
                            .expect("restoration scope accompanies evidence"),
                        &plan,
                        restoration_plan_fingerprint
                            .as_deref()
                            .expect("restoration plan accompanies evidence"),
                        evidence,
                    )
                    .await
                {
                    Ok(target) => (target.container_id, Some(target.disposition)),
                    Err(error) => {
                        for mount in staging.memory.drain(..) {
                            mount.handle.teardown().await;
                        }
                        return Err(err(error));
                    }
                },
                None => match self.runtime.create(&spec.scope, &plan).await {
                    Ok(id) => (id, None),
                    Err(error) => {
                        for mount in staging.memory.drain(..) {
                            mount.handle.teardown().await;
                        }
                        return Err(err(error));
                    }
                },
            },
        };
        // Once the runtime has created or recovered an exact restore target,
        // later provider failures are retryable observations, not authority to
        // replace that physical effect. Ordinary create retains its historical
        // rollback behavior.
        let preserve_restore_target = restoration.is_some();
        if restoration.is_some()
            && self.runtime.uses_host_live_input_bind()
            && restore_disposition == Some(pc::SandboxRestoreTargetDisposition::Created)
            && let Some(staging) = staging.guard.as_mut()
        {
            // From this point the runtime owns this path through its live bind.
            // No failed receipt observation may let wrapper Drop unlink it.
            staging.retain_for_restoration();
        }
        let runtime_handle = match self.runtime.handle_extra(&container_id).await {
            Ok(evidence) => evidence,
            Err(error) => {
                if !preserve_restore_target {
                    let _ = self.runtime.remove(&container_id).await;
                }
                for mount in staging.memory.drain(..) {
                    mount.handle.teardown().await;
                }
                return Err(err(error));
            }
        };
        if restoration.is_some() && self.runtime.uses_host_live_input_bind() {
            let physical_staging = match retained_host_staging(runtime_handle.as_ref()) {
                Ok(staging) => staging,
                Err(error) => {
                    if restore_disposition == Some(pc::SandboxRestoreTargetDisposition::Created)
                        && let Some(staging) = staging.guard.as_mut()
                    {
                        staging.retain_for_restoration();
                    }
                    return Err(err(error));
                }
            };
            let Some(physical_staging) = physical_staging else {
                if restore_disposition == Some(pc::SandboxRestoreTargetDisposition::Created)
                    && let Some(staging) = staging.guard.as_mut()
                {
                    staging.retain_for_restoration();
                }
                return Err(err(RuntimeError::Backend(
                    "host-bind restore target omitted its physical staging locator".into(),
                )));
            };
            match restore_disposition {
                Some(pc::SandboxRestoreTargetDisposition::Created) => {
                    let Some(staging) = staging.guard.as_mut() else {
                        return Err(err(RuntimeError::Backend(
                            "created host-bind restore target has no staging directory".into(),
                        )));
                    };
                    if staging.path() != physical_staging.path() {
                        staging.retain_for_restoration();
                        return Err(err(RuntimeError::Backend(
                            "created host-bind restore target reported another staging directory"
                                .into(),
                        )));
                    }
                    staging.retain_for_restoration();
                }
                Some(pc::SandboxRestoreTargetDisposition::Recovered) => {
                    // The newly resolved staging tree was never attached to the
                    // recovered target. When an ambiguous Podman `run` actually
                    // created the target, both guards name the same tree: retain
                    // the original before the replacement could drop it.
                    match staging.guard.as_mut() {
                        Some(current) if current.path() == physical_staging.path() => {
                            current.retain_for_restoration();
                        }
                        _ => staging.guard = Some(physical_staging),
                    }
                    staging.secret_writebacks.clear();
                    for mount in staging.memory.drain(..) {
                        mount.handle.teardown().await;
                    }
                }
                None => unreachable!("restoration always has a target disposition"),
            }
        }
        // Verify completion only after retaining the daemon-observed physical
        // host locator. If create reached the substrate but start/readiness did
        // not, a retry must keep the same bind instead of dropping it while
        // returning the incomplete receipt error.
        if let Some(expected) = restoration.as_ref() {
            let observed = match self.runtime.restoration_evidence(&container_id).await {
                Ok(observed) => observed,
                Err(error) => {
                    for mount in staging.memory.drain(..) {
                        mount.handle.teardown().await;
                    }
                    return Err(err(error));
                }
            };
            if observed.as_ref() != Some(expected) {
                for mount in staging.memory.drain(..) {
                    mount.handle.teardown().await;
                }
                return Err(err(RuntimeError::Backend(
                    "container restore target omitted or changed its exact physical evidence"
                        .into(),
                )));
            }
        }
        if restoration.is_some() {
            // Restore acquisition must never claim that excluded MemoryStore
            // state was restored. Any process-local handles are torn down before
            // the aggregate CAS; the existing post-CAS projection owner remounts
            // independently governed resources.
            for mount in staging.memory.drain(..) {
                mount.handle.teardown().await;
            }
        }
        let control_services = if restoration.is_some() {
            Default::default()
        } else {
            spec.control_services.clone()
        };
        let sandbox_control_incarnation = if control_services.is_empty() {
            None
        } else {
            match self
                .runtime
                .sandbox_control_binding(
                    &container_id,
                    SandboxControlBindingRequest::New {
                        required: &control_services,
                    },
                )
                .await
            {
                Ok(Some(binding)) => Some(binding),
                Ok(None) => {
                    if !preserve_restore_target {
                        let _ = self.runtime.remove(&container_id).await;
                    }
                    return Err(err(RuntimeError::Backend(
                        "container runtime omitted a demanded Sandbox control incarnation".into(),
                    )));
                }
                Err(error) => {
                    if !preserve_restore_target {
                        let _ = self.runtime.remove(&container_id).await;
                    }
                    return Err(err(error));
                }
            }
        };
        // Report each mount's realization: a byte mount is a Bind and a Memory
        // store is the canonical mounter's copy projection. Built from spec.mounts
        // directly because native volumes do not align with the byte-bind list.
        let realized = if restoration.is_some() {
            Vec::new()
        } else {
            spec.mounts
                .iter()
                .map(|m| pc::RealizedMount {
                    mount_id: m.mount_id.clone(),
                    mount_path: m.mount_path.clone(),
                    access: m.access,
                    realization: match m.source {
                        // Remote container memory is seeded and harvested as a bounded
                        // copy through the canonical MemoryMounter.
                        pc::MountSource::MemoryStore { .. } => pc::Realization::Copy,
                        _ => pc::Realization::Bind,
                    },
                    content_hash: None,
                })
                .collect()
        };
        let continuation_excluded_paths = restore_request.map_or_else(
            || {
                spec.mounts
                    .iter()
                    .map(|mount| mount.mount_path.clone())
                    .collect()
            },
            |request| request.checkpoint.excluded_mounts.clone(),
        );
        let adopted_handle = restore_request
            .map(|request| {
                request.bind_handle(
                    spec,
                    pc::SandboxHandle::container(
                        &spec.scope,
                        pc::ContainerSandboxHandleV1 {
                            container_id: container_id.clone(),
                            outputs_path: spec.outputs_path.clone(),
                            base_env: spec.env.clone(),
                            live_input_projection: self.runtime.supports_live_input_projection(),
                            continuation_excluded_paths: continuation_excluded_paths.clone(),
                            runtime_handle: runtime_handle.clone(),
                            sandbox_control_incarnation: sandbox_control_incarnation.clone(),
                            control_services: control_services.clone(),
                        },
                    ),
                )
            })
            .transpose()?;
        Ok(ContainerSandbox {
            runtime: self.runtime.clone(),
            id: spec.scope.clone(),
            container_id,
            outputs_path: spec.outputs_path.clone(),
            base_env: spec.env.clone(),
            control_services,
            sandbox_control_incarnation,
            control_publication: Arc::new(ContainerControlPublicationRegistry::default()),
            blobs: self.blobs.clone(),
            file_store: self.file_store.clone(),
            live_input_projection: self.runtime.supports_live_input_projection(),
            runtime_handle,
            continuation_excluded_paths,
            adopted_handle,
            realized,
            recovered: restore_disposition == Some(pc::SandboxRestoreTargetDisposition::Recovered),
            lifecycle: Arc::new(ContainerCleanupState {
                staging: std::sync::Mutex::new(staging.guard),
                secret_writebacks: staging.secret_writebacks,
                secret_broker,
                memory: tokio::sync::Mutex::new(Some(staging.memory)),
                writeback_done: tokio::sync::Mutex::new(false),
                remove_done: tokio::sync::Mutex::new(false),
            }),
        })
    }

    /// Re-adopt a concrete long-lived environment from its durable handle.
    pub async fn adopt_container(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        self.adopt_container_with_spec(None, handle).await
    }

    pub(super) async fn adopt_container_with_spec(
        &self,
        spec: Option<&pc::SandboxSpec>,
        handle: &pc::SandboxHandle,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        let payload = recovery::decode_handle(handle)?;
        let capabilities = self.runtime_sandbox_capabilities();
        if let Some(spec) = spec {
            pc::prepare_environment(spec, &capabilities)
                .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
        }
        let restoration = handle.restoration().cloned();
        if let Some(evidence) = restoration.as_ref() {
            let spec = spec.ok_or_else(|| {
                err(RuntimeError::Backend(
                    "restored container adoption requires the frozen SandboxSpec".into(),
                ))
            })?;
            if evidence.sandbox_spec_fingerprint() != pc::sandbox_spec_security_fingerprint(spec)
                || evidence.checkpoint_exclusions_fingerprint()
                    != pc::checkpoint_exclusions_fingerprint(&payload.continuation_excluded_paths)
                || pc::validate_checkpoint_exclusions_for_spec(
                    &payload.continuation_excluded_paths,
                    spec,
                )
                .is_err()
            {
                return Err(err(RuntimeError::Backend(
                    "restored container handle differs from the exact SandboxSpec or checkpoint exclusions"
                        .into(),
                )));
            }
        }
        let control_services = pc::validate_adopted_sandbox_control_services(
            spec.map(|spec| &spec.control_services),
            &payload.control_services,
            &capabilities,
        )
        .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
        if payload.control_services.is_empty() != payload.sandbox_control_incarnation.is_none() {
            return Err(err(RuntimeError::Backend(
                "container handle control topology and incarnation are inconsistent".into(),
            )));
        }
        let container_id = payload.container_id;
        if self.runtime.inspect(&container_id).await.map_err(err)? == ContainerState::Gone {
            return Err(err(RuntimeError::NotFound(container_id)));
        }
        let observed_restoration = self
            .runtime
            .restoration_evidence(&container_id)
            .await
            .map_err(err)?;
        if observed_restoration.as_ref() != restoration.as_ref() {
            return Err(err(RuntimeError::Backend(
                "container restore evidence differs between physical target and durable handle"
                    .into(),
            )));
        }
        if restoration.is_some() {
            let spec = spec.expect("restored adoption requires the frozen SandboxSpec");
            let expected_plan = restoration_plan_fingerprint(
                &self.restoration_environment_plan(spec, &payload.continuation_excluded_paths)?,
            );
            if self
                .runtime
                .restoration_plan_fingerprint(&container_id)
                .await
                .map_err(err)?
                .as_deref()
                != Some(expected_plan.as_str())
            {
                return Err(err(RuntimeError::Backend(
                    "container physical realization differs from the exact restore plan".into(),
                )));
            }
        }
        let observed_runtime_handle = self
            .runtime
            .handle_extra(&container_id)
            .await
            .map_err(err)?;
        if observed_runtime_handle.as_ref() != payload.runtime_handle.as_ref() {
            return Err(err(RuntimeError::Backend(
                "container continuation handle differs between physical target and durable binding"
                    .into(),
            )));
        }
        let recovered_staging =
            retained_host_staging(payload.runtime_handle.as_ref()).map_err(err)?;
        let sandbox_control_incarnation = if control_services.is_empty() {
            None
        } else {
            Some(
                self.runtime
                    .sandbox_control_binding(
                        &container_id,
                        SandboxControlBindingRequest::Adopt {
                            required: &control_services,
                            expected: payload.sandbox_control_incarnation.as_ref(),
                        },
                    )
                    .await
                    .map_err(err)?
                    .ok_or_else(|| {
                        err(RuntimeError::Backend(
                            "adopted container omitted a demanded Sandbox control incarnation"
                                .into(),
                        ))
                    })?,
            )
        };
        Ok(ContainerSandbox {
            runtime: self.runtime.clone(),
            id: handle.sandbox_id.clone(),
            container_id,
            outputs_path: payload.outputs_path,
            base_env: payload.base_env,
            control_services,
            sandbox_control_incarnation,
            control_publication: Arc::new(ContainerControlPublicationRegistry::default()),
            blobs: self.blobs.clone(),
            file_store: self.file_store.clone(),
            live_input_projection: payload.live_input_projection,
            runtime_handle: payload.runtime_handle,
            continuation_excluded_paths: payload.continuation_excluded_paths,
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
        })
    }

    pub(super) async fn acquire_restore_sandbox(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<ContainerSandbox<R>>, pc::SandboxError> {
        request.validate_for_spec(spec)?;
        let sandbox = self.realize_container(spec, Some(request)).await?;
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
    ) -> Result<(), pc::SandboxError> {
        request.validate_for_spec(spec)?;
        pc::prepare_environment(spec, &self.runtime_sandbox_capabilities())
            .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
        let plan = self.restoration_environment_plan(spec, &request.checkpoint.excluded_mounts)?;
        let plan_fingerprint = restoration_plan_fingerprint(&plan);
        let evidence = request.evidence(spec);
        let scope = restoration_runtime_scope(&evidence)?;
        self.runtime
            .dispose_restore_target(&scope, &plan, &plan_fingerprint, &evidence)
            .await
            .map_err(err)
    }
}
