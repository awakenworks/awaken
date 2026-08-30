//! Canonical container provider implementation over the runtime port.
//!
//! The crate root retains the public paths through re-exports; this module owns
//! provider creation, adoption, observation, and trait projection as one unit.

use super::*;

// ── Provider + Sandbox over the port ────────────────────────────────────────────

/// Realizes [`pc::Sandbox`]es on a [`ContainerRuntime`].
pub struct ContainerProvider<R: ContainerRuntime> {
    pub(super) runtime: Arc<R>,
    pub(super) package_provisioner: Option<Arc<dyn PackageImageProvisioner>>,
    pub(super) default_image: String,
    /// Optional connectivity proxy for unrestricted traffic. It is never treated
    /// as network-policy enforcement.
    pub(super) forward_proxy: Option<ForwardProxy>,
    /// Capability issuer plus proxy coordinate. Presence is not enough to
    /// advertise support: the runtime must independently attest that direct
    /// workload egress cannot bypass this proxy.
    pub(super) allowlist_proxy: Option<AllowlistProxy>,
    /// In-memory blob seed for `File`/`Resource`/`Secret` mounts (keyed by content id),
    /// consulted before the store — the test/seed path, mirroring `LocalProvider`.
    pub(super) blobs: Arc<std::collections::HashMap<String, Vec<u8>>>,
    /// The injected content-addressed store consulted after the seed. The worker tier
    /// links no durable store (A-G17); the composition root injects an adapter over the
    /// resources-tier content store, so a `File`/`Resource` id resolves to real bytes.
    pub(super) file_store: Option<Arc<dyn pc::BlobSource>>,
    /// Bidirectional broker used only for `MountSource::Secret`; durable writable
    /// mounts are committed through it during aggregate-owned terminal preparation.
    pub(super) secret_broker: std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>,
    /// Neutral MemoryStore projection injected by the composition root. Interior
    /// mutability lets an already-shared provider receive the platform adapter.
    pub(super) memory_mounter: std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>,
    pub(super) resident_hand: Option<ResidentHandConfig>,
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    pub(super) fn runtime_sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        container_capabilities(
            self.runtime.enforces_network_none(),
            allowlist_capability_advertised(
                self.runtime.enforces_network_allowlist(),
                self.allowlist_proxy.is_some(),
            ),
            self.runtime.supports_package_provisioning() || self.package_provisioner.is_some(),
            self.runtime.sandbox_control_services(),
        )
    }

    async fn probe_runtime_ready(&self) -> Result<(), pc::SandboxError> {
        self.runtime.probe_ready().await.map_err(err)
    }

    pub fn new(runtime: Arc<R>, default_image: impl Into<String>) -> Self {
        Self {
            runtime,
            package_provisioner: None,
            default_image: default_image.into(),
            forward_proxy: None,
            allowlist_proxy: None,
            blobs: Arc::new(std::collections::HashMap::new()),
            file_store: None,
            secret_broker: std::sync::RwLock::new(None),
            memory_mounter: std::sync::RwLock::new(None),
            resident_hand: None,
        }
    }

    /// Make the existing Hand the Session container's resident process. This is
    /// mutually exclusive with attached-exec Hand launch at the Runtime Host.
    #[must_use]
    pub fn with_resident_hand(mut self, config: ResidentHandConfig) -> Self {
        self.resident_hand = Some(config);
        self
    }

    pub fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        *self
            .memory_mounter
            .write()
            .expect("container memory mounter lock poisoned") = Some(mounter);
    }

    /// Configure a conventional forward proxy for unrestricted traffic.
    #[must_use]
    pub fn with_forward_proxy(mut self, proxy: ForwardProxy) -> Self {
        self.forward_proxy = Some(proxy);
        self
    }

    #[must_use]
    pub fn with_allowlist_proxy(mut self, proxy: AllowlistProxy) -> Self {
        self.allowlist_proxy = Some(proxy);
        self
    }

    /// Use an independent image builder/publisher. This is required when the
    /// execution runtime cannot build images itself (for example Kubernetes).
    #[must_use]
    pub fn with_package_provisioner(
        mut self,
        provisioner: Arc<dyn PackageImageProvisioner>,
    ) -> Self {
        self.package_provisioner = Some(provisioner);
        self
    }

    /// Register bytes a `File`/`Resource`/`Secret` mount can resolve to by content id
    /// (test/seed helper, mirroring `LocalProvider::with_blob`).
    #[must_use]
    pub fn with_blob(mut self, id: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Arc::make_mut(&mut self.blobs).insert(id.into(), bytes.into());
        self
    }

    /// Inject the content-addressed store consulted after the seed map, so `File` /
    /// `Resource` / `Secret` mounts resolve their bytes by id at `create` (A-G17: the
    /// provider names no durable store; it holds only this `BlobSource` port).
    #[must_use]
    pub fn with_blob_source(mut self, store: Arc<dyn pc::BlobSource>) -> Self {
        self.file_store = Some(store);
        self
    }

    #[must_use]
    pub fn with_secret_broker(self, broker: Arc<dyn pc::SecretBroker>) -> Self {
        self.install_secret_broker(broker);
        self
    }

    pub fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        *self
            .secret_broker
            .write()
            .expect("container secret broker lock poisoned") = Some(broker);
    }

    /// Project the one provider-effective creation/adoption shape without
    /// participant or backend I/O. Creation and recovery both consume this value;
    /// neither may reconstruct command, egress, resident Hand, or runtime
    /// deployment identity on a parallel path.
    fn adoption_plan(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<(ContainerPlan, pc::SandboxRealizationFingerprint), pc::SandboxError> {
        let command = self
            .resident_hand
            .as_ref()
            .map_or_else(environment_keepalive_command, ResidentHandConfig::command);
        let mut plan = container_plan_with_allowlist(
            spec,
            &self.default_image,
            &command,
            self.forward_proxy.as_ref(),
            self.allowlist_proxy.as_ref(),
        )
        .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
        if let Some(hand) = &self.resident_hand {
            process_env::bind_resident_process_environment(&mut plan.env, &spec.outputs_path);
            plan.env
                .push(("AWAKEN_HAND_LEDGER_DIR".into(), hand.ledger_dir.clone()));
            plan.env.push((
                "AWAKEN_HAND_LEDGER_MAX_ENTRIES".into(),
                hand.ledger_max_entries.to_string(),
            ));
            plan.env.push((
                "AWAKEN_HAND_MAX_CONNECTIONS".into(),
                hand.max_connections.to_string(),
            ));
        }
        let runtime = self.runtime.realization_configuration().map_err(err)?;
        let adoption =
            container_adoption_fingerprint(spec, &plan, self.resident_hand.as_ref(), &runtime);
        Ok((plan, adoption))
    }

    /// Realize a Session-owned container environment. The trait `create` boxes this.
    pub async fn create_container(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        self.create_container_for_effect(spec, None, ContainerRealizationIntent::Create)
            .await
    }

    pub(crate) async fn create_container_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: Option<&ContainerEffectFence>,
        intent: ContainerRealizationIntent,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        if let Some(effect_fence) = effect_fence {
            effect_fence.validate_live_at(container_runtime_unix_now_ms()?)?;
        }
        let create_attempt = ContainerCreateAttempt::fresh();
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

        // A Session owns one live environment. The pure adoption plan is the
        // only projection of its provider-effective immutable configuration.
        let (mut plan, adoption_fingerprint) = self.adoption_plan(spec)?;
        // The same stable-scope decision owner first validates the already
        // authorized Session effect without requiring a resolved image. This
        // rejects foreign/newer/duplicate occupants before package build/push or
        // any Resource participant is touched. A second pass below adds the
        // complete immutable fingerprint before workspace mutation.
        let realization_context = ContainerRealizationContext::new(
            &spec.scope,
            &adoption_fingerprint,
            effect_fence,
            &intent,
            &create_attempt,
        );
        self.runtime
            .preflight_create_for_effect(&realization_context, &plan, None)
            .await
            .map_err(err)?;
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
        let realization_fingerprint =
            container_realization_fingerprint(spec, &adoption_fingerprint, &plan.image);
        // This read-only pass rejects foreign, duplicate, newer, or ambiguous
        // scope occupants before BlobSource, SecretBroker, host staging, or
        // MemoryMounter effects. The runtime re-observes through the same pure
        // decision owner immediately before any create/replace mutation.
        self.runtime
            .preflight_create_for_effect(
                &realization_context,
                &plan,
                Some(&realization_fingerprint),
            )
            .await
            .map_err(err)?;
        // Resolve + materialize each mount's bytes: self-contained content (codex config,
        // ADR-0038 resources) ships in the plan; File/Resource/Secret resolve by id through
        // the seed then the injected BlobSource, hash-verified. Bytes are staged to a host
        // dir (bound by docker/podman) and recorded as `content` (projected by the k8s
        // ConfigMap path) — kept alive by the sandbox for the container's lifetime.
        let secret_broker = self
            .secret_broker
            .read()
            .expect("container secret broker lock poisoned")
            .clone();
        let mut staging = resolve_and_stage(
            spec,
            &mut plan.binds,
            &self.blobs,
            &self.file_store,
            &secret_broker,
            self.runtime.uses_persistent_volume_claims(),
            self.runtime.uses_host_bind_materialization(),
        )
        .await?;
        if self.runtime.uses_host_live_input_bind() {
            live_inputs::stage_host_projection(&spec.scope, &mut plan.binds, &mut staging.guard)?;
        }
        let native_memory = self.runtime.has_native_memory_mounts();
        let mounter = self
            .memory_mounter
            .read()
            .expect("container memory mounter lock poisoned")
            .clone();
        if let Err(error) =
            stage_memory_binds(spec, &mut plan, &mut staging, mounter, native_memory).await
        {
            if let Err(cleanup_error) = staging
                .teardown_memory_after_definite_no_backend_effect()
                .await
            {
                staging.retain_participants_without_cleanup();
                return Err(pc::SandboxError::new(format!(
                    "{error}; Memory participant cleanup failed: {cleanup_error}"
                )));
            }
            return Err(error);
        }
        let container_id = match self
            .runtime
            .create_for_effect(&realization_context, &plan, &realization_fingerprint)
            .await
        {
            Ok(id) => id,
            Err(RuntimeError::MayHaveCommitted(message)) => {
                staging.retain_participants_without_cleanup();
                return Err(err(RuntimeError::MayHaveCommitted(message)));
            }
            Err(error) => {
                if let Err(cleanup_error) = staging
                    .teardown_memory_after_definite_no_backend_effect()
                    .await
                {
                    staging.retain_participants_without_cleanup();
                    return Err(pc::SandboxError::new(format!(
                        "{error}; Memory participant cleanup failed: {cleanup_error}"
                    )));
                }
                return Err(err(error));
            }
        };
        let runtime_handle = match self.runtime.handle_extra(&container_id).await {
            Ok(evidence) => evidence,
            // `create` is exact-idempotent and may have recovered a Ready effect
            // left before the Session receipt. Never destroy that exact substrate
            // on a later evidence-read failure; the next root-driven retry must be
            // able to rediscover and converge it.
            Err(error) => {
                staging.retain_participants_without_cleanup();
                return Err(err(error.after_mutation()));
            }
        };
        let control_services = spec.control_services.clone();
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
                    staging.retain_participants_without_cleanup();
                    return Err(err(RuntimeError::MayHaveCommitted(
                        "container runtime omitted a demanded Sandbox control incarnation".into(),
                    )));
                }
                Err(error) => {
                    staging.retain_participants_without_cleanup();
                    return Err(err(error.after_mutation()));
                }
            }
        };
        // Report each mount's realization: a byte mount is a Bind and a Memory
        // store is the canonical mounter's copy projection. Built from spec.mounts
        // directly because native volumes do not align with the byte-bind list.
        let realized = spec
            .mounts
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
            .collect();
        let memory_materializations = staging
            .memory
            .iter()
            .filter_map(|mount| mount.materialization.clone())
            .collect();
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
            continuation_excluded_paths: spec
                .mounts
                .iter()
                .map(|mount| mount.mount_path.clone())
                .collect(),
            adoption_fingerprint: Some(adoption_fingerprint),
            realization_fingerprint: Some(realization_fingerprint),
            memory_materializations,
            owned_paths: std::sync::Mutex::new(
                spec.mounts
                    .iter()
                    .map(|mount| mount.mount_path.clone())
                    .collect(),
            ),
            adopted_handle: None,
            realized,
            recovered: false,
            lifecycle: Arc::new(ContainerCleanupState {
                staging: std::sync::Mutex::new(staging.guard),
                secret_writebacks: staging.secret_writebacks,
                secret_broker,
                memory: tokio::sync::Mutex::new(Some(staging.memory)),
                memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
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
        if handle.restoration().is_some() {
            return Err(pc::SandboxError::new(
                "restored container adoption requires the frozen SandboxSpec",
            ));
        }
        if handle.realization_fingerprint().is_some() {
            return Err(pc::SandboxError::new(
                "current container adoption requires the frozen SandboxSpec and effect-aware provider seam",
            ));
        }
        match self.observe_container_handle(handle).await? {
            pc::SandboxObservation::Ready => {}
            pc::SandboxObservation::Provisioning => {
                return Err(pc::SandboxError::new(
                    "container realization is still provisioning",
                ));
            }
            pc::SandboxObservation::DefinitivelyUnavailable { .. } => {
                return Err(pc::SandboxError::new(
                    "container realization is definitively unavailable",
                ));
            }
            pc::SandboxObservation::Terminal { .. } => {
                return Err(pc::SandboxError::new(
                    "container realization is terminal and requires fenced authorization",
                ));
            }
            pc::SandboxObservation::Disposing { .. } => {
                return Err(pc::SandboxError::new(
                    "container realization is already disposing and cannot be adopted",
                ));
            }
            pc::SandboxObservation::Incompatible { reason } => {
                return Err(pc::SandboxError::new(reason));
            }
        }
        self.adopt_observed_container(handle).await
    }

    async fn adopt_container_with_adoption(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        if adoption.handle.restoration().is_some() {
            return self
                .adopt_restored_container_with_spec(adoption.spec, adoption.handle)
                .await;
        }
        match self.observe_container_adoption(adoption).await? {
            pc::SandboxObservation::Ready => {
                self.adopt_observed_container_with_spec(adoption.handle, adoption.spec)
                    .await
            }
            pc::SandboxObservation::Provisioning => Err(pc::SandboxError::new(
                "container realization is still provisioning",
            )),
            pc::SandboxObservation::DefinitivelyUnavailable { .. } => Err(pc::SandboxError::new(
                "container realization is definitively unavailable",
            )),
            pc::SandboxObservation::Terminal { .. } => Err(pc::SandboxError::new(
                "container realization is terminal and requires fenced authorization",
            )),
            pc::SandboxObservation::Disposing { .. } => Err(pc::SandboxError::new(
                "container realization is already disposing and cannot be adopted",
            )),
            pc::SandboxObservation::Incompatible { reason } => Err(pc::SandboxError::new(reason)),
        }
    }

    async fn adopt_observed_container_with_spec(
        &self,
        handle: &pc::SandboxHandle,
        spec: &pc::SandboxSpec,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        let secret_broker = self
            .secret_broker
            .read()
            .expect("container secret broker lock poisoned")
            .clone();
        let lifecycle = ContainerCleanupState::recovered_native(
            spec,
            handle,
            secret_broker,
            !self.runtime.uses_host_bind_materialization(),
        )?;
        self.adopt_observed_container_with_lifecycle(handle, Some(spec), lifecycle)
            .await
    }

    async fn adopt_observed_container(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        // Compatibility-only handle adoption has no frozen SandboxSpec from
        // which terminal participants can be reconstructed. Managed Session
        // adoption always enters through ContainerEnvironmentAdoption above.
        self.adopt_observed_container_with_lifecycle(
            handle,
            None,
            ContainerCleanupState::physical_cleanup_only(),
        )
        .await
    }

    async fn adopt_observed_container_with_lifecycle(
        &self,
        handle: &pc::SandboxHandle,
        spec: Option<&pc::SandboxSpec>,
        lifecycle: ContainerCleanupState,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        let payload = recovery::decode_handle(handle)?;
        let control_services = pc::validate_adopted_sandbox_control_services(
            spec.map(|spec| &spec.control_services),
            &payload.control_services,
            &self.runtime_sandbox_capabilities(),
        )
        .map_err(|error| err(RuntimeError::Backend(error.to_string())))?;
        if payload.control_services.is_empty() != payload.sandbox_control_incarnation.is_none() {
            return Err(err(RuntimeError::Backend(
                "container handle control topology and incarnation are inconsistent".into(),
            )));
        }
        let container_id = payload.container_id;
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
            adoption_fingerprint: handle.container_adoption_fingerprint()?.cloned(),
            realization_fingerprint: handle.realization_fingerprint().cloned(),
            memory_materializations: handle
                .memory_materializations()?
                .unwrap_or_default()
                .to_vec(),
            owned_paths: std::sync::Mutex::new(handle.owned_paths().unwrap_or_default().to_vec()),
            adopted_handle: None,
            realized: Vec::new(),
            recovered: true,
            lifecycle: Arc::new(lifecycle),
        })
    }

    async fn observe_container_handle(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.runtime
            .observe(ContainerObservationExpectation::from_handle(handle)?)
            .await
            .map_err(err)
    }

    async fn observe_container_adoption(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        if let Some(stored) = adoption.handle.container_adoption_fingerprint()? {
            let (_, expected) = self.adoption_plan(adoption.spec)?;
            if stored != &expected {
                return Ok(pc::SandboxObservation::Incompatible {
                    reason: "container provider-effective adoption configuration differs from its durable handle"
                        .into(),
                });
            }
        }
        self.observe_container_handle(adoption.handle).await
    }

    async fn observe_container_adoption_for_effect(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        effect_fence.validate_live_at(container_runtime_unix_now_ms()?)?;
        if let Some(stored) = adoption.handle.container_adoption_fingerprint()? {
            let (_, expected) = self.adoption_plan(adoption.spec)?;
            if stored != &expected {
                return Ok(pc::SandboxObservation::Incompatible {
                    reason: "container provider-effective adoption configuration differs from its durable handle"
                        .into(),
                });
            }
        }
        self.runtime
            .observe(ContainerObservationExpectation::from_handle_for_effect(
                adoption.handle,
                effect_fence,
            )?)
            .await
            .map_err(err)
    }

    async fn adopt_container_with_adoption_for_effect(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        if adoption.handle.restoration().is_some() {
            effect_fence.validate_live_at(container_runtime_unix_now_ms()?)?;
            return self
                .adopt_restored_container_with_spec(adoption.spec, adoption.handle)
                .await;
        }
        match self
            .observe_container_adoption_for_effect(adoption, effect_fence)
            .await?
        {
            pc::SandboxObservation::Ready => {
                self.adopt_observed_container_with_spec(adoption.handle, adoption.spec)
                    .await
            }
            pc::SandboxObservation::Provisioning => Err(pc::SandboxError::new(
                "container realization is still provisioning",
            )),
            pc::SandboxObservation::DefinitivelyUnavailable { .. } => Err(pc::SandboxError::new(
                "container realization is definitively unavailable",
            )),
            pc::SandboxObservation::Terminal { .. } => Err(pc::SandboxError::new(
                "container realization is terminal and requires fenced authorization",
            )),
            pc::SandboxObservation::Disposing { .. } => Err(pc::SandboxError::new(
                "container realization is already disposing and cannot be adopted",
            )),
            pc::SandboxObservation::Incompatible { reason } => Err(pc::SandboxError::new(reason)),
        }
    }

    async fn prepare_terminal_container_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        _expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Option<ContainerSandbox<R>>, pc::SandboxError> {
        let Some(handle) = handle else {
            return Err(pc::SandboxError::new(
                "container provider cannot dispose an in-flight restore without a durable handle",
            ));
        };
        if handle.restoration().is_some() {
            return Err(pc::SandboxError::new(
                "restored container terminal cleanup requires its exact SandboxRestoreRequest",
            ));
        }
        if self.runtime.uses_host_bind_materialization() {
            return Err(pc::SandboxError::new(
                "host-bind container terminal participants cannot be reconstructed from a prior process-local realization attempt",
            ));
        }
        // Built-in container runtimes do not implement checkpoint restore, so
        // the optional prior fence carries no reconstructible participant when
        // a durable handle is present. Root authorization remains the sole
        // policy owner; the runtime re-observes with the terminal fence below.
        let adoption = ContainerEnvironmentAdoption::new(spec, handle);
        match self
            .observe_container_adoption_for_effect(adoption, terminal_effect_fence)
            .await?
        {
            pc::SandboxObservation::Terminal { .. } | pc::SandboxObservation::Ready => self
                .adopt_observed_container_with_spec(adoption.handle, adoption.spec)
                .await
                .map(Some),
            pc::SandboxObservation::Disposing { .. } => self
                .adopt_observed_container_with_lifecycle(
                    adoption.handle,
                    None,
                    ContainerCleanupState::physical_cleanup_only(),
                )
                .await
                .map(Some),
            pc::SandboxObservation::Provisioning => Err(pc::SandboxError::new(
                "terminal preparation observed a provisioning container realization",
            )),
            pc::SandboxObservation::DefinitivelyUnavailable { .. } => Ok(None),
            pc::SandboxObservation::Incompatible { reason } => Err(pc::SandboxError::new(reason)),
        }
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> ContainerEnvironmentProvider for ContainerProvider<R> {
    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        self.runtime_sandbox_capabilities()
    }

    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.install_memory_mounter(mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        self.install_secret_broker(broker);
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.probe_runtime_ready().await
    }

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(self.create_container(spec).await?))
    }

    async fn create_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: Option<&ContainerEffectFence>,
        intent: ContainerRealizationIntent,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(
            self.create_container_for_effect(spec, effect_fence, intent)
                .await?,
        ))
    }

    async fn observe_environment(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.observe_container_adoption(adoption).await
    }

    async fn observe_environment_for_effect(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.observe_container_adoption_for_effect(adoption, effect_fence)
            .await
    }

    async fn adopt_environment(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(
            self.adopt_container_with_adoption(adoption).await?,
        ))
    }

    async fn adopt_environment_for_effect(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
        effect_fence: Option<&ContainerEffectFence>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        // `adopt_container` re-observes the exact durable incarnation at this
        // effect edge. It never deletes or replaces; a concurrent transition to
        // Gone is surfaced to the Host for a separately authorized Rebuild.
        Ok(Arc::new(match effect_fence {
            Some(effect_fence) => {
                self.adopt_container_with_adoption_for_effect(adoption, effect_fence)
                    .await?
            }
            None => self.adopt_container_with_adoption(adoption).await?,
        }))
    }

    async fn prepare_terminal_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&ContainerEffectFence>,
        terminal_effect_fence: &ContainerEffectFence,
    ) -> Result<Option<Arc<dyn ContainerEnvironment>>, pc::SandboxError> {
        self.prepare_terminal_container_for_effect(
            spec,
            handle,
            expected_effect_fence,
            terminal_effect_fence,
        )
        .await
        .map(|environment| {
            environment.map(|environment| Arc::new(environment) as Arc<dyn ContainerEnvironment>)
        })
    }

    async fn acquire_restore_environment(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<Arc<dyn ContainerEnvironment>>, pc::SandboxError> {
        Ok(self
            .acquire_restore_sandbox(spec, request)
            .await?
            .map_target(|sandbox| Arc::new(sandbox) as Arc<dyn ContainerEnvironment>))
    }

    async fn dispose_restored_environment(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<(), pc::SandboxError> {
        self.dispose_restore_target(spec, request).await
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::SandboxProvider for ContainerProvider<R> {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        self.runtime_sandbox_capabilities()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.probe_runtime_ready().await
    }

    async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.create_container(spec).await?))
    }

    async fn create_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(
            self.create_container_for_effect(
                spec,
                Some(effect_fence),
                ContainerRealizationIntent::Create,
            )
            .await?,
        ))
    }

    async fn observe(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.observe_container_handle(handle).await
    }

    async fn observe_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        self.observe_container_adoption_for_effect(
            ContainerEnvironmentAdoption::new(spec, handle),
            effect_fence,
        )
        .await
    }

    async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.adopt_container(handle).await?))
    }

    async fn adopt_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(
            self.adopt_container_with_adoption_for_effect(
                ContainerEnvironmentAdoption::new(spec, handle),
                effect_fence,
            )
            .await?,
        ))
    }

    async fn prepare_terminal_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Option<Box<dyn pc::Sandbox>>, pc::SandboxError> {
        self.prepare_terminal_container_for_effect(
            spec,
            handle,
            expected_effect_fence,
            terminal_effect_fence,
        )
        .await
        .map(|sandbox| sandbox.map(|sandbox| Box::new(sandbox) as Box<dyn pc::Sandbox>))
    }

    async fn acquire_restore(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<Box<dyn pc::Sandbox>>, pc::SandboxError> {
        Ok(self
            .acquire_restore_sandbox(spec, request)
            .await?
            .map_target(|sandbox| Box::new(sandbox) as Box<dyn pc::Sandbox>))
    }

    async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<(), pc::SandboxError> {
        self.dispose_restore_target(spec, request).await
    }
}
