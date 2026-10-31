// @formal entrypoint resource_reclamation.continuation_claim_deletion_requires_exact_incarnation_fence
//! Canonical container provider implementation over the runtime port.
//!
//! The crate root retains the public paths through re-exports; this module owns
//! provider creation, adoption, observation, and trait projection. Terminal
//! takeover and retained-handle reconstruction live with the existing recovery owner.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_container_contract::RuntimeError;
use awaken_provisioning_contract::{
    SandboxAdoption, SandboxEffectFence, SandboxObservationPurpose, SessionSandbox,
};

// All three ordinary adoption seams consume this one observation admission.
// A retention fact is deliberately insufficient for execution or cleanup.
fn admit_ready_observation(observed: pc::SandboxObservation) -> Result<(), pc::SandboxError> {
    match observed {
        pc::SandboxObservation::Ready => Ok(()),
        pc::SandboxObservation::Provisioning { .. } => Err(pc::SandboxError::not_ready(
            "container realization is still provisioning",
        )),
        pc::SandboxObservation::DefinitivelyUnavailable { .. } => Err(pc::SandboxError::gone(
            "container realization is definitively unavailable",
        )),
        pc::SandboxObservation::Terminal { .. } => Err(pc::SandboxError::conflict(
            "container realization is terminal and requires fenced authorization",
        )),
        pc::SandboxObservation::RetainedOriginal { .. } => Err(pc::SandboxError::conflict(
            "original source retention is not container adoption authority",
        )),
        pc::SandboxObservation::Disposing { .. } => Err(pc::SandboxError::gone(
            "container realization is already disposing and cannot be adopted",
        )),
        pc::SandboxObservation::Incompatible { reason } => Err(pc::SandboxError::conflict(reason)),
    }
}

fn rebase_staged_paths(
    plan: &mut ContainerPlan,
    staging: &mut StagedMounts,
    source: &std::path::Path,
    destination: &std::path::Path,
) {
    for bind in &mut plan.binds {
        let path = std::path::Path::new(&bind.source_ref);
        if let Ok(relative) = path.strip_prefix(source) {
            bind.source_ref = destination.join(relative).to_string_lossy().into_owned();
        }
    }
    for writeback in &mut staging.secret_writebacks {
        if let Some(path) = writeback.staged_path.as_mut()
            && let Ok(relative) = path.strip_prefix(source)
        {
            *path = destination.join(relative);
        }
    }
}

fn refresh_effect_fence(
    asserted: &SandboxEffectFence,
    source: SandboxEffect<'_>,
    now_ms: u64,
) -> Result<SandboxEffectFence, pc::SandboxError> {
    let SandboxEffect::Fenced {
        authority: source, ..
    } = source
    else {
        return Err(pc::SandboxError::fence_rejected(
            "container effect fence expired without a renewable Session authority",
        ));
    };
    let current = source.current_effect_fence(asserted)?;
    if !asserted.same_effect_identity(&current)
        || current.expires_at_unix_ms() <= asserted.expires_at_unix_ms()
    {
        return Err(pc::SandboxError::fence_rejected(
            "renewed container effect fence is not a strict successor of the admitted effect",
        ));
    }
    current.validate_live_at(now_ms)?;
    Ok(current)
}

// ── Provider + Sandbox over the port ────────────────────────────────────────────

/// Realizes [`pc::Sandbox`]es on a [`ContainerRuntime`].
pub struct ContainerProvider<R: ContainerRuntime> {
    pub(super) runtime: Arc<R>,
    pub(super) package_provisioner:
        Option<Arc<dyn awaken_container_contract::PackageImageProvisioner>>,
    pub(super) default_image: String,
    /// Optional connectivity proxy for unrestricted traffic. It is never treated
    /// as network-policy enforcement.
    pub(super) forward_proxy: Option<awaken_container_contract::ForwardProxy>,
    /// Capability issuer plus proxy coordinate. Presence is not enough to
    /// advertise support: the runtime must independently attest that direct
    /// workload egress cannot bypass this proxy.
    pub(super) allowlist_proxy: Option<AllowlistProxy>,
    /// Deployment selected the existing Worker MCP relay and Gateway route.
    /// Capability publication still requires independently attested runtime
    /// no-bypass policy and the installed signed proxy issuer.
    pub(super) worker_mcp_relay_substitution: bool,
    /// In-memory blob seed for `File`/`Resource`/`Secret` mounts (keyed by content id),
    /// consulted before the store — the test/seed path, mirroring `LocalProvider`.
    pub(super) blobs: Arc<std::collections::HashMap<String, Vec<u8>>>,
    /// The injected content-addressed store consulted after the seed. The worker tier
    /// links no durable store (A-G17); the composition root injects an adapter over the
    /// resources-tier content store, so a `File`/`Resource` id resolves to real bytes.
    pub(super) file_store: Option<Arc<dyn pc::BlobSource>>,
    /// Bidirectional broker used only for `MountSource::Secret`; durable writable
    /// mounts are committed through it during aggregate-owned terminal preparation.
    pub(super) secret_broker: awaken_sync::LeafRwLock<Option<Arc<dyn pc::SecretBroker>>>,
    /// Neutral MemoryStore projection injected by the composition root. Interior
    /// mutability lets an already-shared provider receive the platform adapter.
    pub(super) memory_mounter: awaken_sync::LeafRwLock<Option<Arc<dyn pc::MemoryMounter>>>,
    pub(super) resident_hand: Option<ResidentHandConfig>,
    /// The composing process's wall clock: effect fences are judged and
    /// allowlist capabilities issued on it. A provider composed without one
    /// judges no fence and issues no capability (fail closed).
    pub(super) clock: Option<Arc<dyn awaken_time::WallClock>>,
    /// What the engine proved at start-up ([`Self::attest_startup`]). Until
    /// then the adapter's static answers stand; the execution composition
    /// attests before the Worker derives its manifest.
    pub(super) startup_evidence:
        std::sync::OnceLock<crate::host_evidence::ContainerStartupEvidence>,
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    pub(super) fn runtime_sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        let allowlist = allowlist_capability_advertised(
            self.runtime.enforces_network_allowlist(),
            self.allowlist_proxy.is_some(),
        );
        let evidence = self.startup_evidence.get();
        let builder_proven = evidence.is_none_or(|evidence| evidence.package_builder);
        container_capabilities(
            self.runtime.enforces_network_none(),
            allowlist,
            self.worker_mcp_relay_substitution,
            self.package_provisioner.is_some() && builder_proven,
            evidence.is_none_or(|evidence| evidence.resource_limits),
            self.runtime.sandbox_control_services(),
        )
    }

    pub(super) fn package_image_provisioner(
        &self,
    ) -> Result<&Arc<dyn awaken_container_contract::PackageImageProvisioner>, pc::SandboxError>
    {
        self.package_provisioner.as_ref().ok_or_else(|| {
            pc::SandboxError::from(RuntimeError::Backend(
                "container runtime cannot provision package requirements".into(),
            ))
        })
    }

    async fn probe_runtime_ready(&self) -> Result<(), pc::SandboxError> {
        self.runtime
            .probe_ready()
            .await
            .map_err(pc::SandboxError::from)
    }

    pub fn new(runtime: Arc<R>, default_image: impl Into<String>) -> Self {
        Self {
            runtime,
            package_provisioner: None,
            default_image: default_image.into(),
            forward_proxy: None,
            allowlist_proxy: None,
            worker_mcp_relay_substitution: false,
            blobs: Arc::new(std::collections::HashMap::new()),
            file_store: None,
            secret_broker: awaken_sync::LeafRwLock::new(None),
            memory_mounter: awaken_sync::LeafRwLock::new(None),
            resident_hand: None,
            clock: None,
            startup_evidence: std::sync::OnceLock::new(),
        }
    }

    /// Prove, before registration, each capability this provider advertises
    /// (ADR-0092 decision 5): the engine answers and its default image
    /// resolves (else `Err`: no isolation tier would remain); CPU/memory/pid
    /// limits and the configured package builder are withheld when unproven.
    pub async fn attest_startup(
        &self,
    ) -> Result<crate::host_evidence::ContainerStartupEvidence, pc::SandboxError> {
        self.runtime.probe_ready().await?;
        let resource_limits = self
            .runtime
            .startup_resource_limits(&self.default_image)
            .await?;
        let package_builder = match &self.package_provisioner {
            Some(builder) => builder.probe_builder().await.is_ok(),
            None => true,
        };
        let evidence = crate::host_evidence::ContainerStartupEvidence {
            resource_limits,
            package_builder,
        };
        Ok(*self.startup_evidence.get_or_init(|| evidence))
    }

    /// A unit-test provider composed on the host clock.
    #[cfg(test)]
    pub(crate) fn for_test(runtime: Arc<R>, default_image: impl Into<String>) -> Self {
        Self::new(runtime, default_image).with_clock(awaken_time::system_clock())
    }

    /// Compose the provider with the process's wall clock (see the `clock`
    /// field). The execution composition always installs it.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn awaken_time::WallClock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// One reading of the composed clock for an effect-fence judgement.
    pub(super) fn effect_clock_ms(&self) -> Result<u64, pc::SandboxError> {
        crate::effect_clock_reading_ms(self.clock.as_ref())
    }

    /// Make the existing Hand the Session container's resident process. This is
    /// mutually exclusive with attached-exec Hand launch at the Runtime Host.
    #[must_use]
    pub fn with_resident_hand(mut self, config: ResidentHandConfig) -> Self {
        self.resident_hand = Some(config);
        self
    }

    pub fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        *self.memory_mounter.write() = Some(mounter);
    }

    /// Configure a conventional forward proxy for unrestricted traffic.
    #[must_use]
    pub fn with_forward_proxy(mut self, proxy: awaken_container_contract::ForwardProxy) -> Self {
        self.forward_proxy = Some(proxy);
        self
    }

    #[must_use]
    pub fn with_allowlist_proxy(mut self, proxy: AllowlistProxy) -> Self {
        self.allowlist_proxy = Some(proxy);
        self
    }

    #[must_use]
    pub fn with_worker_mcp_relay_substitution(mut self) -> Self {
        self.worker_mcp_relay_substitution = true;
        self
    }

    /// Build package images through this builder. No container engine adapter
    /// builds images itself; without a builder, package requirements fail
    /// closed before a workload exists.
    #[must_use]
    pub fn with_package_provisioner(
        mut self,
        provisioner: Arc<dyn awaken_container_contract::PackageImageProvisioner>,
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
        *self.secret_broker.write() = Some(broker);
    }

    /// Project the one provider-effective creation/adoption shape without
    /// participant or backend I/O. Creation and recovery both consume this value;
    /// neither may reconstruct command, egress, resident Hand, or runtime
    /// deployment identity on a parallel path.
    pub(super) fn adoption_plan(
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
            self.clock.as_ref(),
        )
        .map_err(pc::SandboxError::from)?;
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
        let runtime = self
            .runtime
            .realization_configuration()
            .map_err(pc::SandboxError::from)?;
        let adoption =
            container_adoption_fingerprint(spec, &plan, self.resident_hand.as_ref(), &runtime);
        Ok((plan, adoption))
    }

    /// Dispose never-used capacity that a cancelled creator left at `scope`.
    ///
    /// There is deliberately no handle, fence or spec here: the caller was
    /// dropped (or its process died) before any of them could exist, and
    /// demanding one is exactly why the container became unreachable. The
    /// runtime addresses the object by the scope name alone, which only a warm
    /// pool mints, and a reclaimer reaches it only through a claim on an
    /// expired, never-checked-out lease record (`pool::lease`), so this port
    /// cannot reach a Session-owned environment.
    #[cfg(all(unix, any(feature = "connection", test)))]
    pub(crate) async fn dispose_unreceipted_capacity(
        &self,
        scope: &str,
    ) -> Result<(), pc::SandboxError> {
        self.runtime
            .remove_unreceipted_scope(scope)
            .await
            .map_err(pc::SandboxError::from)
    }

    /// Realize a Session-owned container environment. The trait `create` boxes this.
    pub async fn create_container(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        self.create_container_for_effect(
            spec,
            SandboxEffect::Unattached,
            ContainerRealizationIntent::Create,
        )
        .await
    }

    pub(crate) async fn create_container_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: SandboxEffect<'_>,
        intent: ContainerRealizationIntent,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        let effect_fence = effect.fence();
        if let Some(effect_fence) = effect_fence {
            effect_fence.validate_live_at(self.effect_clock_ms()?)?;
        }
        // The copied durable fence is identity, not current admission. The
        // fenced creation port carries the original captured authority here;
        // explicitly unattached creation continues through this same path.
        let admit = || effect.admit();
        admit()?;
        let create_attempt = crate::fresh_create_attempt();
        // Fail closed against our capabilities before touching the runtime.
        pc::prepare_environment(spec, &self.runtime_sandbox_capabilities())?;
        if spec
            .mounts
            .iter()
            .any(pc::MountRequirement::is_secret_writeback)
            && !self.runtime.supports_secret_writeback()
        {
            return Err(pc::SandboxError::unsupported(
                "this container runtime cannot persist a writable credential-file mount",
            ));
        }
        if memory_projections(spec).iter().any(|projection| {
            projection.write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
        }) {
            return Err(pc::SandboxError::unsupported(
                "container Memory cannot satisfy restart-safe write-through ownership; use a copy-compatible requirement",
            ));
        }

        // A Session owns one live environment. The pure adoption plan is the
        // only projection of its provider-effective immutable configuration.
        let (mut plan, adoption_fingerprint) = self.adoption_plan(spec)?;
        // The same stable-scope decision owner first validates the already
        // authorized Session effect without requiring a resolved image. This
        // rejects foreign/newer/duplicate occupants before package build/push or
        // any Resource participant is touched. A second pass below adds the
        // complete immutable fingerprint before workspace mutation.
        let initial_realization_context = ContainerRealizationContext::new(
            &spec.scope,
            &adoption_fingerprint,
            effect_fence,
            &intent,
            &create_attempt,
        );
        let initial_preflight = self
            .runtime
            .preflight_create_for_effect(&initial_realization_context, &plan, None)
            .await;
        admit()?;
        initial_preflight.map_err(pc::SandboxError::from)?;
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
            let prepared_image = self
                .package_image_provisioner()?
                .prepare_package_image(&base_image, &plan.packages, &spec.network)
                .await;
            admit()?;
            plan.image = prepared_image.map_err(pc::SandboxError::from)?;
            plan.rootfs = RootfsPlan::Image(plan.image.clone());
        }
        admit()?;
        let realization_fingerprint =
            container_realization_fingerprint(spec, &adoption_fingerprint, &plan.image);
        // This read-only pass rejects foreign, duplicate, newer, or ambiguous
        // scope occupants before BlobSource, SecretBroker, host staging, or
        // MemoryMounter effects. The runtime re-observes through the same pure
        // decision owner immediately before any create/replace mutation.
        let realization_preflight = self
            .runtime
            .preflight_create_for_effect(
                &initial_realization_context,
                &plan,
                Some(&realization_fingerprint),
            )
            .await;
        admit()?;
        realization_preflight.map_err(pc::SandboxError::from)?;
        // Resolve + materialize each mount's bytes: self-contained content (codex config,
        // ADR-0038 resources) ships in the plan; File/Resource/Secret resolve by id through
        // the seed then the injected BlobSource, hash-verified. Bytes are staged to a host
        // dir (bound by docker/podman) and recorded as `content` (projected by the k8s
        // ConfigMap path) — kept alive by the sandbox for the container's lifetime.
        let secret_broker = self.secret_broker.read().clone();
        let staged = resolve_and_stage(
            spec,
            &mut plan.binds,
            &self.blobs,
            &self.file_store,
            &secret_broker,
            self.runtime.uses_persistent_volume_claims(),
            self.runtime.uses_host_bind_materialization(),
        )
        .await;
        admit()?;
        let mut staging = staged?;
        if self.runtime.uses_host_live_input_bind() {
            live_inputs::stage_host_projection(&spec.scope, &mut plan.binds, &mut staging.guard)?;
        }
        let native_memory = self.runtime.has_native_memory_mounts();
        let has_memory = !memory_projections(spec).is_empty();
        reserve_memory_binds(spec, &mut plan, &mut staging, native_memory)?;
        if !self.runtime.uses_host_bind_materialization() && has_memory {
            // Native backends derive their process-surviving staging identity
            // from the same attempt stamped on the physical reservation. Create
            // it first so a lost prepare response cannot leave a Pod whose
            // Memory responsibility was never addressable by its successor.
            staging_dir_for_backend_attempt(
                &mut staging.guard,
                &spec.scope,
                create_attempt.as_str(),
                PreparedContainerDisposition::FreshReservation,
            )?;
        }
        let mounter = self.memory_mounter.read().clone();
        let mut current_effect_fence = effect_fence.cloned();
        let prepared = loop {
            admit()?;
            let realization_context = ContainerRealizationContext::new(
                &spec.scope,
                &adoption_fingerprint,
                current_effect_fence.as_ref(),
                &intent,
                &create_attempt,
            );
            let preparation = self
                .runtime
                .prepare_for_effect(&realization_context, &plan, &realization_fingerprint)
                .await;
            match preparation {
                Ok(prepared) => {
                    // Any successful reservation may already refer to the
                    // candidate host path. Transfer that path to restoration
                    // ownership before checking anything that can return from
                    // this call; exact-attempt validation follows below.
                    if let Some(guard) = staging.guard.as_mut() {
                        guard.retain_for_restoration();
                    }
                    admit()?;
                    break prepared;
                }
                Err(RuntimeError::EffectFenceExpired) => {
                    let Some(asserted) = current_effect_fence.as_ref() else {
                        return Err(pc::SandboxError::fence_rejected(
                            "unfenced container realization reported an expired effect fence",
                        ));
                    };
                    let refreshed = refresh_effect_fence(asserted, effect, self.effect_clock_ms()?);
                    match refreshed {
                        Ok(refreshed) => current_effect_fence = Some(refreshed),
                        Err(error) => return Err(error),
                    }
                }
                Err(RuntimeError::MayHaveCommitted(message)) => {
                    if let Some(guard) = staging.guard.as_mut() {
                        guard.retain_for_restoration();
                    }
                    let current = admit().err().map(|error| error.to_string());
                    return Err(pc::SandboxError::from(RuntimeError::MayHaveCommitted(
                        current.map_or(message.clone(), |authority| {
                            format!("{message}; current authority changed: {authority}")
                        }),
                    )));
                }
                Err(error) => {
                    admit()?;
                    return Err(pc::SandboxError::from(error));
                }
            }
        };

        // A same-attempt reservation may already bind the candidate root. Make
        // that ownership transfer synchronously before any evidence-read await;
        // losing the next response must never let Drop unlink live bind input.
        let same_attempt = prepared.authority_attempt_id == create_attempt.as_str();
        if prepared.disposition == PreparedContainerDisposition::FreshReservation && !same_attempt {
            if let Some(guard) = staging.guard.as_mut() {
                guard.retain_for_restoration();
            }
            return Err(pc::SandboxError::from(RuntimeError::MayHaveCommitted(
                "fresh container reservation returned a foreign attempt identity".into(),
            )));
        }
        debug_assert!(
            !same_attempt
                || staging
                    .guard
                    .as_ref()
                    .is_none_or(|guard| !guard.remove_on_drop)
        );

        if self.runtime.uses_host_bind_materialization() && self.runtime.uses_host_live_input_bind()
        {
            let recovered_root = self
                .runtime
                .recovered_host_staging_root(&prepared.locator)
                .await;
            admit()?;
            let recovered_root = recovered_root
                .map_err(|error| pc::SandboxError::from(error.after_mutation()))?
                .ok_or_else(|| {
                    pc::SandboxError::not_found(
                        "prepared host-bind container has no exact staging-root evidence",
                    )
                })?;
            let current_root = staging
                .guard
                .as_ref()
                .map(StagingGuard::path)
                .map(ToOwned::to_owned);
            if current_root.as_deref() != Some(recovered_root.as_path()) {
                if prepared.disposition == PreparedContainerDisposition::FreshReservation {
                    return Err(pc::SandboxError::from(RuntimeError::MayHaveCommitted(
                        "fresh container reservation reported a different staging root".into(),
                    )));
                }
                if let Some(current_root) = current_root.as_deref() {
                    rebase_staged_paths(&mut plan, &mut staging, current_root, &recovered_root);
                }
                drop(staging.guard.take());
                let recovered = crate::restore_target::retained_host_staging_path(recovered_root)
                    .map_err(pc::SandboxError::from)?;
                staging.guard = Some(recovered);
            }
        } else if self.runtime.uses_host_bind_materialization()
            && prepared.disposition != PreparedContainerDisposition::FreshReservation
        {
            return Err(pc::SandboxError::gone(
                "host-bind reservation cannot recover its staging root",
            ));
        } else if !self.runtime.uses_host_bind_materialization() && has_memory {
            if !same_attempt {
                // The candidate belongs to this call and no Memory effect has
                // started yet. A recovered backend reservation selects its old
                // exact attempt root; never copy fresh bytes into that root.
                drop(staging.guard.take());
            }
            staging_dir_for_backend_attempt(
                &mut staging.guard,
                &spec.scope,
                &prepared.authority_attempt_id,
                prepared.disposition,
            )?;
        }
        // The physical reservation is now the recovery root. Process-local Drop
        // cannot remove a path that the exact container/Pod may still reference.
        if let Some(guard) = staging.guard.as_mut() {
            guard.retain_for_restoration();
        }
        admit()?;
        let recovering =
            prepared.disposition == PreparedContainerDisposition::RecoveringReservation;
        let already_active = prepared.disposition == PreparedContainerDisposition::AlreadyActive;
        let memory_materializations = if already_active {
            if memory_projections(spec).is_empty() {
                Vec::new()
            } else {
                staging.read_memory_pre_activation_receipt(spec)?
            }
        } else {
            if let Err(error) = materialize_memory_binds(
                spec,
                &mut plan,
                &mut staging,
                mounter,
                native_memory,
                recovering,
                &admit,
            )
            .await
            {
                let cleanup = staging.recover_memory_before_activation(&admit).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => error.map_detail(|detail| {
                        format!("{detail}; Memory participant cleanup failed: {cleanup_error}")
                    }),
                });
            }
            if let Err(error) = admit() {
                let cleanup = staging.recover_memory_before_activation(&admit).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => error.map_detail(|detail| {
                        format!("{detail}; Memory participant cleanup failed: {cleanup_error}")
                    }),
                });
            }
            if let Err(error) = staging.write_memory_pre_activation_receipt() {
                let cleanup = staging.recover_memory_before_activation(&admit).await;
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => error.map_detail(|detail| {
                        format!("{detail}; Memory participant cleanup failed: {cleanup_error}")
                    }),
                });
            }
            let container_id = loop {
                if let Err(error) = admit() {
                    let cleanup = staging.recover_memory_before_activation(&admit).await;
                    return Err(match cleanup {
                        Ok(()) => error,
                        Err(cleanup_error) => error.map_detail(|detail| {
                            format!("{detail}; Memory participant cleanup failed: {cleanup_error}")
                        }),
                    });
                }
                let realization_context = ContainerRealizationContext::new(
                    &spec.scope,
                    &adoption_fingerprint,
                    current_effect_fence.as_ref(),
                    &intent,
                    &create_attempt,
                )
                .with_effect(effect);
                let activation = self
                    .runtime
                    .activate_for_effect(
                        &realization_context,
                        &plan,
                        &realization_fingerprint,
                        &prepared.locator,
                    )
                    .await;
                let current_authority = admit();
                match activation {
                    Ok(container_id) => {
                        current_authority?;
                        break container_id;
                    }
                    Err(RuntimeError::EffectFenceExpired) => {
                        current_authority?;
                        let asserted = current_effect_fence.as_ref().ok_or_else(|| {
                            pc::SandboxError::fence_rejected(
                                "unfenced container activation reported an expired effect fence",
                            )
                        })?;
                        current_effect_fence = Some(refresh_effect_fence(
                            asserted,
                            effect,
                            self.effect_clock_ms()?,
                        )?);
                    }
                    Err(RuntimeError::MayHaveCommitted(message)) => {
                        return Err(pc::SandboxError::from(RuntimeError::MayHaveCommitted(
                            current_authority
                                .err()
                                .map_or(message.clone(), |authority| {
                                    format!("{message}; current authority changed: {authority}")
                                }),
                        )));
                    }
                    Err(error) => {
                        if let Err(authority_error) = current_authority {
                            return Err(pc::SandboxError::from(error).map_detail(|detail| format!("{detail}; Memory participant cleanup was not authorized: {authority_error}"
                            )));
                        }
                        let cleanup = staging.recover_memory_before_activation(&admit).await;
                        return Err(match cleanup {
                            Ok(()) => pc::SandboxError::from(error),
                            Err(cleanup_error) => pc::SandboxError::from(error).map_detail(|detail| format!("{detail}; Memory participant cleanup failed: {cleanup_error}"
                            )),
                        });
                    }
                }
            };
            if container_id != prepared.locator {
                return Err(pc::SandboxError::from(RuntimeError::MayHaveCommitted(
                    "container activation returned a different physical locator".into(),
                )));
            }
            admit()?;
            staging
                .memory
                .iter()
                .filter_map(|mount| mount.materialization.clone())
                .collect()
        };
        let container_id = prepared.locator;
        let runtime_handle_result = self.runtime.handle_extra(&container_id).await;
        admit()?;
        // `create` may have recovered a Ready effect before the Session
        // receipt. Preserve that substrate after failed evidence readback so
        // the next root-driven retry can rediscover the same incarnation.
        let runtime_handle = runtime_handle_result
            .map_err(|error| pc::SandboxError::from(error.after_mutation()))?;
        let control_services = spec.control_services.clone();
        let sandbox_control_incarnation = if control_services.is_empty() {
            None
        } else {
            let binding = self
                .runtime
                .sandbox_control_binding(
                    &container_id,
                    SandboxControlBindingRequest::New {
                        required: &control_services,
                    },
                )
                .await;
            admit()?;
            match binding {
                Ok(Some(binding)) => Some(binding),
                Ok(None) => {
                    return Err(pc::SandboxError::from(RuntimeError::MayHaveCommitted(
                        "container runtime omitted a demanded Sandbox control incarnation".into(),
                    )));
                }
                Err(error) => {
                    return Err(pc::SandboxError::from(error.after_mutation()));
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
        let parts = staging.into_parts()?;
        Ok(ContainerSandbox {
            retained_attempt: Some(create_attempt),
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
            continuation_excluded_paths: recovery::source_mount_paths(spec),
            adoption_fingerprint: Some(adoption_fingerprint),
            realization_fingerprint: Some(realization_fingerprint),
            memory_materializations,
            owned_paths: awaken_sync::LeafLock::new(recovery::source_mount_paths(spec)),
            adopted_handle: None,
            realized,
            recovered: already_active,
            lifecycle: Arc::new(ContainerCleanupState {
                staging: awaken_sync::InvariantLock::new(parts.guard),
                secret_writebacks: parts.secret_writebacks,
                secret_broker,
                memory: tokio::sync::Mutex::new((!already_active).then_some(parts.memory)),
                memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
                writeback_done: tokio::sync::Mutex::new(false),
                remove_done: tokio::sync::Mutex::new(false),
            }),
            execs: Arc::default(),
        })
    }

    /// Re-adopt a concrete long-lived environment from its durable handle.
    pub async fn adopt_container(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        if handle.restoration().is_some() {
            return Err(pc::SandboxError::invalid(
                "restored container adoption requires the frozen SandboxSpec",
            ));
        }
        if handle.realization_fingerprint().is_some() {
            return Err(pc::SandboxError::invalid(
                "current container adoption requires the frozen SandboxSpec and effect-aware provider seam",
            ));
        }
        admit_ready_observation(self.observe_container_handle(handle).await?)?;
        self.adopt_observed_container(handle).await
    }

    async fn adopt_container_with_adoption(
        &self,
        adoption: SandboxAdoption<'_>,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        if adoption.handle.restoration().is_some() {
            return self
                .adopt_restored_container_with_spec(adoption.spec, adoption.handle, &|| Ok(()))
                .await;
        }
        admit_ready_observation(
            self.observe_container_adoption(adoption, None, SandboxObservationPurpose::Recovery)
                .await?,
        )?;
        self.adopt_observed_container_with_spec(
            adoption.handle,
            adoption.spec,
            SandboxObservationPurpose::Recovery,
            &|| Ok(()),
        )
        .await
    }

    pub(super) async fn adopt_observed_container_with_spec(
        &self,
        handle: &pc::SandboxHandle,
        spec: &pc::SandboxSpec,
        purpose: SandboxObservationPurpose,
        admit: &(impl Fn() -> Result<(), pc::SandboxError> + Sync),
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        admit()?;
        let payload = recovery::decode_handle(handle)?;
        let recovered_staging = if self.runtime.uses_host_live_input_bind() {
            let path = self
                .runtime
                .recovered_host_staging_root(&payload.container_id)
                .await
                .map_err(pc::SandboxError::from)?
                .ok_or_else(|| {
                    pc::SandboxError::not_found(
                        "host-bind container adoption has no exact staging-root evidence",
                    )
                })?;
            admit()?;
            Some(
                crate::restore_target::retained_host_staging_path(path)
                    .map_err(pc::SandboxError::from)?,
            )
        } else {
            None
        };
        let secret_broker = self.secret_broker.read().clone();
        let lifecycle = ContainerCleanupState::recovered_native(
            spec,
            handle,
            secret_broker,
            !self.runtime.uses_host_bind_materialization() || recovered_staging.is_some(),
            recovered_staging,
        )?;
        self.adopt_observed_container_with_lifecycle(handle, Some(spec), lifecycle, purpose, admit)
            .await
    }

    async fn adopt_observed_container(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        // Compatibility-only handle adoption has no frozen SandboxSpec from
        // which terminal participants can be reconstructed. Managed Session
        // adoption always enters through SandboxAdoption above.
        self.adopt_observed_container_with_lifecycle(
            handle,
            None,
            ContainerCleanupState::physical_cleanup_only(),
            SandboxObservationPurpose::Recovery,
            &|| Ok(()),
        )
        .await
    }

    pub(super) async fn adopt_observed_container_with_lifecycle(
        &self,
        handle: &pc::SandboxHandle,
        spec: Option<&pc::SandboxSpec>,
        lifecycle: ContainerCleanupState,
        purpose: SandboxObservationPurpose,
        admit: &(impl Fn() -> Result<(), pc::SandboxError> + Sync),
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        admit()?;
        let payload = recovery::decode_handle(handle)?;
        if let Some(runtime_handle) = payload.runtime_handle.as_ref()
            && runtime_handle.terminal_only()
            && purpose != SandboxObservationPurpose::TerminalCleanup
        {
            return Err(pc::SandboxError::unsupported(
                "joined-empty evidence is terminal cleanup only",
            ));
        }
        let control_services = pc::validate_adopted_sandbox_control_services(
            spec.map(|spec| &spec.control_services),
            &payload.control_services,
            &self.runtime_sandbox_capabilities(),
        )
        .map_err(pc::SandboxError::from)?;
        if payload.control_services.is_empty() != payload.sandbox_control_incarnation.is_none() {
            return Err(pc::SandboxError::invalid(
                "container handle control topology and incarnation are inconsistent",
            ));
        }
        let container_id = payload.container_id;
        let sandbox_control_incarnation = if control_services.is_empty() {
            None
        } else if purpose == SandboxObservationPurpose::TerminalCleanup {
            // The caller already observed the exact Terminal/Disposing physical
            // owner under its aggregate fence. Preserve its immutable topology
            // without reopening a control listener or transferring live owner.
            // This identity is evidence for cleanup, never a live-I/O grant.
            payload.sandbox_control_incarnation.clone()
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
                    .map_err(pc::SandboxError::from)?
                    .ok_or_else(|| {
                        pc::SandboxError::conflict(
                            "adopted container omitted a demanded Sandbox control incarnation",
                        )
                    })?,
            )
        };
        let control_publication = ContainerControlPublicationRegistry::for_adoption(purpose).await;
        admit()?;
        Ok(ContainerSandbox {
            retained_attempt: None,
            runtime: self.runtime.clone(),
            id: handle.sandbox_id.clone(),
            container_id,
            outputs_path: payload.outputs_path,
            base_env: payload.base_env,
            control_services,
            sandbox_control_incarnation,
            control_publication: Arc::new(control_publication),
            blobs: self.blobs.clone(),
            file_store: self.file_store.clone(),
            live_input_projection: payload.live_input_projection,
            runtime_handle: payload.runtime_handle,
            continuation_excluded_paths: payload.continuation_excluded_paths,
            adoption_fingerprint: handle.container_adoption_fingerprint()?,
            realization_fingerprint: handle.realization_fingerprint().cloned(),
            memory_materializations: handle
                .memory_materializations()?
                .unwrap_or_default()
                .to_vec(),
            owned_paths: awaken_sync::LeafLock::new(
                handle.owned_paths().unwrap_or_default().to_vec(),
            ),
            adopted_handle: None,
            realized: Vec::new(),
            recovered: true,
            lifecycle: Arc::new(lifecycle),
            execs: Arc::default(),
        })
    }

    async fn observe_container_handle(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        let decoded = crate::DecodedContainerHandle::decode(handle)?;
        self.runtime
            .observe(ContainerObservationExpectation::from_decoded(&decoded))
            .await
            .map_err(pc::SandboxError::from)
    }

    pub(super) async fn observe_container_adoption(
        &self,
        adoption: SandboxAdoption<'_>,
        effect_fence: Option<&pc::SandboxEffectFence>,
        purpose: SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        let retirement = matches!(purpose, SandboxObservationPurpose::WorkerRetirement { .. });
        let normalized = if retirement {
            self.normalized_terminal_handle(adoption.spec, Some(adoption.handle))?
        } else {
            None
        };
        let adoption = SandboxAdoption {
            handle: normalized.as_ref().unwrap_or(adoption.handle),
            ..adoption
        };
        let decoded = crate::DecodedContainerHandle::decode(adoption.handle)?;
        if retirement
            && (effect_fence.is_none()
                || decoded.adoption_fingerprint.is_none()
                || decoded.realization_fingerprint.is_none()
                || adoption.handle.sandbox_id != adoption.spec.scope)
        {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: "Worker retirement requires the original effect fence and complete same-Session durable identity".into(),
            });
        }
        // Retirement observes the original source, not a new execution under
        // this process's installation/Hand/volume configuration. The runtime
        // still verifies the durable fingerprints, fence and physical UIDs.
        // Execution recovery and terminal preparation retain their strict plan.
        if !retirement && let Some(stored) = decoded.adoption_fingerprint.as_ref() {
            let (plan, expected) = self.adoption_plan(adoption.spec)?;
            let historical_match = if *stored != expected
                && purpose == SandboxObservationPurpose::TerminalCleanup
                && effect_fence.is_some()
            {
                self.runtime
                    .historical_terminal_configuration(
                        adoption.handle.container_payload()?.runtime_handle.as_ref(),
                    )
                    .map_err(pc::SandboxError::from)?
                    .is_some_and(|configuration| {
                        container_adoption_fingerprint(
                            adoption.spec,
                            &plan,
                            self.resident_hand.as_ref(),
                            &configuration,
                        ) == *stored
                    })
            } else {
                false
            };
            if *stored != expected && !historical_match {
                return Ok(pc::SandboxObservation::Incompatible {
                    reason: format!(
                        "container provider-effective adoption configuration differs from its durable handle (expected={expected}, stored={stored})"
                    ),
                });
            }
        }
        let mut expectation = ContainerObservationExpectation::from_decoded(&decoded);
        expectation.effect_fence = effect_fence;
        expectation.retained_attempt = adoption.retained_attempt;
        expectation.purpose = purpose;
        self.runtime
            .observe(expectation)
            .await
            .map_err(pc::SandboxError::from)
    }

    async fn adopt_container_with_adoption_for_effect(
        &self,
        adoption: SandboxAdoption<'_>,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<ContainerSandbox<R>, pc::SandboxError> {
        let admit = || pc::with_current_sandbox_effect(effect_fence, authority, |_| Ok(()));
        admit()?;
        if adoption.handle.restoration().is_some() {
            return self
                .adopt_restored_container_with_spec(adoption.spec, adoption.handle, &admit)
                .await;
        }
        admit_ready_observation(
            self.observe_container_adoption(
                adoption,
                Some(effect_fence),
                SandboxObservationPurpose::Recovery,
            )
            .await?,
        )?;
        self.adopt_observed_container_with_spec(
            adoption.handle,
            adoption.spec,
            SandboxObservationPurpose::Recovery,
            &admit,
        )
        .await
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::SandboxReadiness for ContainerProvider<R> {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        self.runtime_sandbox_capabilities()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        self.probe_runtime_ready().await
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::SandboxProvider for ContainerProvider<R> {
    type Environment = dyn SessionSandbox;

    async fn prewarm_images(&self, specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        let mut images = std::collections::BTreeSet::new();
        for spec in specs {
            let (plan, _) = self.adoption_plan(spec)?;
            // Coordinator must prepare package images first. Cache warmup never
            // becomes another package builder or materializes customer mounts.
            if plan.packages.is_empty() && spec.mounts.is_empty() {
                images.insert(plan.image);
            }
        }
        self.runtime
            .prewarm_images(&images.into_iter().collect::<Vec<_>>())
            .await
            .map_err(pc::SandboxError::from)
    }

    fn adoption_layout(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxAdoptionLayout, pc::SandboxError> {
        crate::container_adoption_layout(handle)
    }

    fn validate_closed_observation(
        &self,
        handle: &pc::SandboxHandle,
        observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError> {
        crate::validate_container_closed_observation(handle, observation)
    }

    // The same physical labels retain responsibility before the first poll.
    // Both unattached capacity and fenced realization use this one provider edge.
    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a pc::SandboxSpec,
        effect: SandboxEffect<'a>,
        intent: pc::SandboxRealizationIntent,
    ) -> pc::ClaimedEffect<'a, Arc<dyn SessionSandbox>> {
        let claim = pc::EffectClaim::self_recording(CONTAINER_REALIZATION_RECONCILER);
        pc::ClaimedEffect::new(claim, async move {
            intent.admit_under(effect)?;
            let intent = ContainerRealizationIntent::from_environment(intent)?;
            Ok(Arc::new(
                self.create_container_for_effect(spec, effect, intent)
                    .await?,
            ) as Arc<dyn SessionSandbox>)
        })
    }

    async fn observe(
        &self,
        adoption: SandboxAdoption<'_>,
        basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        let effect_fence = adoption.observation_fence(basis)?;
        self.observe_container_adoption(adoption, effect_fence, adoption.observation_purpose)
            .await
    }

    async fn adopt_for_effect(
        &self,
        adoption: SandboxAdoption<'_>,
        effect: SandboxEffect<'_>,
    ) -> Result<Arc<dyn SessionSandbox>, pc::SandboxError> {
        let environment = match effect {
            SandboxEffect::Unattached => self.adopt_container_with_adoption(adoption).await?,
            SandboxEffect::Fenced { fence, authority } => {
                self.adopt_container_with_adoption_for_effect(adoption, fence, authority)
                    .await?
            }
        };
        Ok(Arc::new(environment))
    }

    async fn prepare_terminal_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&SandboxEffectFence>,
        terminal_effect_fence: &SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<Arc<dyn SessionSandbox>>, pc::SandboxError> {
        self.prepare_terminal_container_for_effect(
            spec,
            handle,
            expected_effect_fence,
            terminal_effect_fence,
            authority,
        )
        .await
        .map(|environment| {
            environment.map(|environment| Arc::new(environment) as Arc<dyn SessionSandbox>)
        })
    }

    fn restoration(&self) -> pc::SandboxRestoration<'_, Arc<dyn SessionSandbox>> {
        pc::SandboxRestoration::Targets(self)
    }
}

impl<R: ContainerRuntime + 'static> awaken_provisioning_contract::SandboxInstallation
    for ContainerProvider<R>
{
    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        self.install_memory_mounter(mounter);
    }

    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>) {
        self.install_secret_broker(broker);
    }
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    /// The container-environment restore target. This provider serves both
    /// restore rungs (Sandbox and container environment), so a caller holding
    /// the concrete provider names the rung it means here.
    pub async fn acquire_restore_environment(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<Arc<dyn SessionSandbox>>, pc::SandboxError> {
        Ok(self
            .acquire_restore_sandbox(spec, request)
            .await?
            .map_target(|sandbox| Arc::new(sandbox) as Arc<dyn SessionSandbox>))
    }

    /// Dispose the unpublished container-environment restore target.
    pub async fn dispose_restored_environment(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        self.dispose_restore_target(spec, request, authorization, authority)
            .await
    }
}

#[async_trait]
impl<R: ContainerRuntime + 'static> pc::RestoreTargetProvider<Arc<dyn SessionSandbox>>
    for ContainerProvider<R>
{
    async fn acquire_restore(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<Arc<dyn SessionSandbox>>, pc::SandboxError> {
        self.acquire_restore_environment(spec, request).await
    }

    async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        self.dispose_restored_environment(spec, request, authorization, authority)
            .await
    }
}

impl<R: ContainerRuntime + 'static> ContainerProvider<R> {
    // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
    // are no longer a trait obligation or a reachable escape hatch. They stay
    // as this type's own implementation detail, used by its fenced edges.
    pub async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.create_container(spec).await?))
    }
    pub async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.adopt_container(handle).await?))
    }
}
