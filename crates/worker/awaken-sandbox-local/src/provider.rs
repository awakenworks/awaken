//! `LocalProvider` — the `awaken-provisioning-contract` seam realized on the local
//! machine (ADR-0041, first slice / Workdir tier).
//!
//! It launches real processes via [`tokio::process`], jailing *paths it resolves*
//! under an [`IsolatedRoot`] and exposing the outputs directory to a spawned
//! process through the reserved `AWAKEN_OUTPUTS_DIR` / `AWAKEN_PROJECT_DIR` env
//! vars. It reports `tool_transparent = false`: a launched process is **not**
//! OS-confined (a lexical jail cannot confine an opaque agent), so
//! [`prepare_environment`](awaken_provisioning_contract::prepare_environment)
//! refuses to place a `Namespace`/`Container` workload here — that is the Bwrap /
//! container tier's job. This provider is for trusted, single-machine execution
//! (CI, dev, the runtime's own in-process tools) plus the full artifact/reattach/
//! lease lifecycle the contract requires. The Workdir [`LocalSandbox`] also carries
//! the host-tier helpers (rooted tools, repo clone/write-back, artifact + skill
//! scanning) the runtime host composes into each session.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio as ProcStdio;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_local_process::LocalProcess;
use awaken_provisioning_contract as pc;
use tokio::process::Command as TokioCommand;

use std::sync::Arc;

use awaken_runtime_contract::tool::RawTool;

use crate::FilesystemHandleCodec as _;
use crate::read_only_tree::materialize_read_only_tree_at;
use crate::{
    DiscoveredSkillFile, IsolatedRoot, content_fingerprint, list_files_at, provision_repo_at,
    push_repo_to_at, rooted_raw_tools, scan_skill_dir_at,
};

mod checkpoint;
#[cfg(target_os = "linux")]
pub(crate) mod execution;
mod restore_target;
pub(crate) mod terminal_disposal;

/// Resolve a mount's bytes: an in-memory seed map first, then an optional
/// content-addressed [`BlobSource`](pc::BlobSource), then typed inline content.
pub(crate) async fn resolve_source(
    source: &pc::MountSource,
    blobs: &HashMap<String, Vec<u8>>,
    store: &Option<Arc<dyn pc::BlobSource>>,
    secret_broker: Option<&Arc<dyn pc::SecretBroker>>,
) -> Result<Option<Vec<u8>>, pc::SandboxError> {
    // Inline content and unresolvable memory stores short-circuit before
    // any store hit; the rest resolve by id (seed map first, then the store).
    let id = match source {
        pc::MountSource::File { file_id, .. } => file_id.as_str(),
        pc::MountSource::Resource { resource_id, .. } => resource_id.as_str(),
        pc::MountSource::Secret { reference, .. } => {
            if let Some(broker) = secret_broker {
                return broker.materialize(reference).await.map(Some);
            }
            reference.as_str()
        }
        // Inline ephemeral content ships in the spec — no store hit, no id.
        pc::MountSource::Inline { contents } => {
            return Ok(Some(contents.clone().into_bytes()));
        }
        pc::MountSource::InlineBytes { contents, .. } => return Ok(Some(contents.clone())),
        pc::MountSource::MemoryStore { .. } => return Ok(None),
        // A Cache Volume has no seedable content — it is mounted in place from its
        // host path and its bytes have no authority (ADR-0056), so there is nothing to
        // fingerprint or seed here.
        pc::MountSource::CacheVolume { .. } => return Ok(None),
    };
    if let Some(bytes) = blobs.get(id) {
        return Ok(Some(bytes.clone()));
    }
    if let Some(store) = store
        && let Some(bytes) = store.get(id).await
    {
        return Ok(Some(bytes));
    }
    Ok(None)
}

/// The declared content hash of a mount source, if any (verified fail-closed).
pub(crate) fn declared_hash(source: &pc::MountSource) -> Option<&str> {
    match source {
        pc::MountSource::File { content_hash, .. } => content_hash.as_deref(),
        pc::MountSource::Resource { content_hash, .. } => content_hash.as_deref(),
        pc::MountSource::Secret { content_hash, .. } => content_hash.as_deref(),
        pc::MountSource::InlineBytes { content_hash, .. } => content_hash.as_deref(),
        _ => None,
    }
}

/// Verify resolved bytes against a declared hash; fail closed on mismatch.
pub(crate) fn verify(source: &pc::MountSource, bytes: &[u8]) -> Result<(), pc::SandboxError> {
    if let Some(expected) = declared_hash(source) {
        let got = content_fingerprint(bytes);
        if got != expected {
            return Err(pc::SandboxError::conflict(format!(
                "mount content hash mismatch: declared {expected}, realized {got}"
            )));
        }
    }
    Ok(())
}

/// Realizes [`LocalSandbox`] environments as directories under a base path.
pub struct LocalProvider {
    base: PathBuf,
    inherit_agent_stderr: bool,
    trusted_host_access: bool,
    #[cfg(target_os = "linux")]
    execution_deployment: Option<execution::LocalExecutionDeployment>,
    /// In-memory blob seed for `File`/`Resource` mounts (keyed by id).
    blobs: HashMap<String, Vec<u8>>,
    /// Optional content-addressed store consulted after the seed map (Slice 4).
    file_store: Option<Arc<dyn pc::BlobSource>>,
    /// Credential-file materialization port. Kept separate from `BlobSource` so
    /// secret references cannot accidentally resolve through the ordinary
    /// resource-content path.
    secret_broker: Arc<std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>>,
    /// Optional memory-store realizer (FUSE / copy). Absent → a `MemoryStore` mount
    /// fails loud rather than being faked as an empty file (ADR-0053 item 1).
    memory_mounter: Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    memory_mounts: crate::MemoryMountRegistry,
    /// Roots named by a creation claim that was dropped before it settled.
    abandoned: Arc<crate::AbandonedRealizations>,
}

impl LocalProvider {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            inherit_agent_stderr: false,
            trusted_host_access: false,
            #[cfg(target_os = "linux")]
            execution_deployment: None,
            blobs: HashMap::new(),
            file_store: None,
            secret_broker: Arc::new(std::sync::RwLock::new(None)),
            memory_mounter: Arc::new(std::sync::RwLock::new(None)),
            memory_mounts: crate::MemoryMountRegistry::default(),
            abandoned: Arc::default(),
        }
    }

    /// Every sandbox root a cancelled creation left behind on this provider,
    /// for the reconciler to read the durable marker of.
    #[must_use]
    pub fn abandoned_realizations(&self) -> Arc<crate::AbandonedRealizations> {
        Arc::clone(&self.abandoned)
    }

    /// Construct the exact operator-authorized host-identity adapter. It uses
    /// the same lifecycle and materialization implementation as Workdir; only
    /// read-only mount admission becomes advisory.
    pub fn trusted_host(base: impl Into<PathBuf>) -> Self {
        Self {
            trusted_host_access: true,
            ..Self::new(base)
        }
    }

    #[must_use]
    pub fn has_trusted_host_access(&self) -> bool {
        self.trusted_host_access
    }

    /// Install immutable optional execution hardening before provider use.
    /// This does not qualify `process_tree_supervision`: distributed lease and
    /// selected-provider failure qualification remain required. Only fenced realizations
    /// are eligible, and their launches deny until the Session owner binds them.
    #[cfg(target_os = "linux")]
    pub fn install_execution_deployment(
        &mut self,
        deployment: execution::LocalExecutionDeployment,
    ) -> Result<(), pc::SandboxError> {
        if !self.trusted_host_access {
            return Err(pc::SandboxError::unsupported(
                "execution deployment requires TrustedHost",
            ));
        }
        if self.execution_deployment.is_some() {
            return Err(pc::SandboxError::conflict(
                "execution deployment is already installed",
            ));
        }
        self.execution_deployment = Some(deployment);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn execution_deployment(&self) -> Option<&execution::LocalExecutionDeployment> {
        self.execution_deployment.as_ref()
    }

    fn select_execution_boundary(&self, sandbox: &LocalSandbox) -> Result<(), pc::SandboxError> {
        #[cfg(target_os = "linux")]
        if let Some(deployment) = &self.execution_deployment {
            if sandbox.realization.current().is_none() {
                return Err(pc::SandboxError::fence_rejected(
                    "execution deployment requires a current fenced realization",
                ));
            }
            return sandbox.select_execution_boundary(deployment);
        }
        let _ = sandbox;
        Ok(())
    }

    pub(super) fn prepare(&self, spec: &pc::SandboxSpec) -> Result<(), pc::SandboxError> {
        if self.trusted_host_access {
            pc::prepare_trusted_host_environment(spec, &Self::capabilities())
        } else {
            pc::prepare_environment(spec, &Self::capabilities())
        }
        .map(|_| ())
        .map_err(pc::SandboxError::from)
    }

    /// Select whether an opaque agent child inherits the host stderr.
    #[must_use]
    pub fn with_agent_stderr(mut self, inherit: bool) -> Self {
        self.inherit_agent_stderr = inherit;
        self
    }

    #[must_use]
    pub fn inherits_agent_stderr(&self) -> bool {
        self.inherit_agent_stderr
    }

    /// Register bytes a `File`/`Resource` mount can resolve to (test/seed helper).
    #[must_use]
    pub fn with_blob(mut self, id: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        self.blobs.insert(id.into(), bytes.into());
        self
    }

    /// Resolve `File`/`Resource` mounts from a content-addressed store.
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
            .expect("secret broker lock poisoned") = Some(broker);
    }

    /// Realize `MemoryStore` mounts via an injected mounter (FUSE where available,
    /// else a harvested copy). Without one, a `MemoryStore` mount fails loud.
    #[must_use]
    pub fn with_memory_mounter(self, mounter: Arc<dyn pc::MemoryMounter>) -> Self {
        self.install_memory_mounter(mounter);
        self
    }

    /// Install/replace the mounter at the outer composition seam. Interior
    /// mutability lets a server assemble the host first and wire worker adapters
    /// before serving, without putting the implementation dependency in the host.
    pub fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        *self
            .memory_mounter
            .write()
            .expect("memory mounter lock poisoned") = Some(mounter);
    }

    /// Project the handle-visible fields from the frozen creation spec. This is
    /// the single owner for new Local handle payloads; create and handle-less
    /// terminal recovery must not derive these fields independently.
    fn project_spec_handle_fields(
        sandbox: &mut LocalSandbox,
        spec: &pc::SandboxSpec,
    ) -> Result<(), pc::SandboxError> {
        sandbox.deny_egress = spec.deny_tool_egress;
        sandbox.base_env.clone_from(&spec.env);
        sandbox.continuation_excluded_paths = spec
            .mounts
            .iter()
            .filter(|mount| {
                matches!(
                    mount.source,
                    pc::MountSource::Secret { .. }
                        | pc::MountSource::MemoryStore { .. }
                        | pc::MountSource::CacheVolume { .. }
                )
            })
            .map(|mount| {
                sandbox
                    .root
                    .resolve(&mount.mount_path)
                    .map_err(pc::SandboxError::from)
            })
            .collect::<Result<_, _>>()?;
        Ok(())
    }

    /// Restore every handle-visible field from durable provider evidence. All
    /// Local reopen paths use this projection so an exact handle cannot drift
    /// merely because the in-memory wrapper was reconstructed.
    fn project_durable_handle_fields(
        sandbox: &mut LocalSandbox,
        payload: &crate::LocalSandboxHandleV1,
    ) -> Result<(), pc::SandboxError> {
        sandbox.base_env.clone_from(&payload.base_env);
        sandbox.deny_egress = payload.deny_tool_egress;
        sandbox.continuation_excluded_paths = payload
            .continuation_excluded_paths
            .iter()
            .map(|path| sandbox.root.resolve(path).map_err(pc::SandboxError::from))
            .collect::<Result<_, _>>()?;
        Ok(())
    }

    /// Realize a sandbox and return the concrete [`LocalSandbox`], so a caller can
    /// use the tool-transparent [`LocalSandbox::spawn_agent`] capability. The trait
    /// `create` delegates here and boxes the result.
    pub async fn create_sandbox(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        self.create_sandbox_inner(spec, None, None).await
    }

    /// Realize a current durable Local sandbox under one aggregate-owned fence.
    /// A rebuild must additionally supply the exact V2 source handle; a plain
    /// create may not infer takeover authority from an absent same-spec root.
    pub async fn create_sandbox_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: &pc::SandboxEffectFence,
        source_handle: Option<&pc::SandboxHandle>,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        if let Some(handle) = source_handle {
            handle.local_payload()?;
        }
        self.create_sandbox_inner(spec, Some((effect_fence, authority)), source_handle)
            .await
    }

    async fn create_sandbox_inner(
        &self,
        spec: &pc::SandboxSpec,
        effect: Option<(&pc::SandboxEffectFence, &dyn pc::CurrentExecutionAuthority)>,
        source_handle: Option<&pc::SandboxHandle>,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        // Fail closed against our capabilities (isolation, ro, egress secrets, …).
        self.prepare(spec)?;

        #[cfg(target_os = "linux")]
        if self.execution_deployment.is_some() && effect.is_none() {
            return Err(pc::SandboxError::fence_rejected(
                "execution deployment requires fenced creation",
            ));
        }

        let mut sandbox = self.build(&spec.scope, &spec.outputs_path);
        let realization_fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
        let mut realization_guard = match effect {
            Some((effect_fence, authority)) => {
                crate::realization_marker::ProviderCreationGuard::current(
                    sandbox.root.root(),
                    &realization_fingerprint,
                    effect_fence,
                    source_handle
                        .map(crate::realization_marker::rebuild_source)
                        .transpose()?,
                    authority,
                )?
            }
            None if source_handle.is_none() => {
                crate::realization_marker::ProviderCreationGuard::Legacy(
                    crate::realization_marker::begin_legacy(sandbox.root.root())?,
                )
            }
            None => {
                return Err(pc::SandboxError::fence_rejected(
                    "sandbox rebuild requires an aggregate effect fence",
                ));
            }
        };
        sandbox.secret_paths = spec
            .mounts
            .iter()
            .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
            .map(|mount| {
                sandbox
                    .root
                    .resolve(&mount.mount_path)
                    .map_err(pc::SandboxError::from)
            })
            .collect::<Result<_, _>>()?;
        Self::project_spec_handle_fields(&mut sandbox, spec)?;
        *sandbox
            .owned_paths
            .lock()
            .expect("owned paths lock poisoned") = spec
            .mounts
            .iter()
            .map(|mount| mount.mount_path.clone())
            .collect();
        // Ready is a completed physical attempt, not permission to reconcile
        // provider-owned paths again. Rebuild the response only from the exact
        // durable receipt while the retained-parent lock proves the same root.
        if let Some(receipt) = realization_guard.completed_receipt()?.cloned() {
            let (realized, materializations) =
                crate::replay_completion_receipt(spec, &receipt, pc::Realization::Copy)?;
            let host_paths = spec
                .mounts
                .iter()
                .map(|mount| {
                    sandbox
                        .root
                        .resolve(&mount.mount_path)
                        .map_err(pc::SandboxError::from)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let memory_mounts = crate::replay_memory_mount_guards(
                &self.memory_mounter,
                sandbox.memory_mounts.clone(),
                spec,
                &realized,
                &host_paths,
                || realization_guard.validate_memory_replay(),
            )
            .await?;
            sandbox.realized = realized;
            sandbox.memory_materializations = materializations;
            sandbox.realization = match realization_guard.complete(&receipt) {
                Ok(realization) => realization,
                Err(cause) => {
                    let teardown = memory_mounts
                        .lock()
                        .await
                        .release_unconfirmed(|| realization_guard.validate_memory_replay())
                        .await;
                    return Err(match teardown {
                        Ok(()) => cause,
                        Err(teardown) => cause.map_detail(|detail| format!("Local Ready replay publication failed: {detail}; Memory replay teardown failed: {teardown}"
                        )),
                    });
                }
            };
            sandbox.memory_mounts = memory_mounts;
            self.select_execution_boundary(&sandbox)?;
            return Ok(sandbox);
        }
        // A retry must shred credential bytes left by the previous interrupted
        // attempt before the marker guard clears that exact root. Missing paths
        // are idempotent; any shred fault retains the Creating/Recreating evidence.
        crate::memory_mount::prepare_memory_before_root(&sandbox.memory_mounts, &realization_guard)
            .await?;
        if realization_guard.is_incomplete() {
            realization_guard.with_authority(|| {
                crate::shred_secret_paths_at(
                    &sandbox.root,
                    realization_guard.root_identity()?,
                    &sandbox.secret_paths,
                )
            })?;
        }
        realization_guard.prepare_root()?;
        let root_identity = realization_guard.root_identity()?.ok_or_else(|| {
            pc::SandboxError::invalid("prepared Local sandbox has no root identity")
        })?;
        let realization = async {
            // Outputs directory (sandbox-absolute → rejailed host path).
            let host_outputs = sandbox
                .root
                .resolve(&spec.outputs_path)
                .map_err(pc::SandboxError::from)?;
            realization_guard.with_mutation(|| {
                awaken_sandbox_fs::create_relative_directory_all(
                    sandbox.root.root(),
                    root_identity,
                    std::path::Path::new(spec.outputs_path.trim_start_matches('/')),
                )
                .map_err(|error| {
                    pc::SandboxError::backend(
                        "create_sandbox_inner",
                        format!(
                            "create sandbox outputs `{}`: {error}",
                            host_outputs.display()
                        ),
                    )
                })
            })?;

            // Only incomplete creation reaches materialization. Ready returned
            // above by projecting its receipt without rewriting Agent bytes.
            for req in &spec.mounts {
                let (mount, materialization) = self
                    .realize_mount(
                        &sandbox.root,
                        root_identity,
                        req,
                        &sandbox.memory_mounts,
                        &realization_guard,
                    )
                    .await?;
                sandbox.realized.push(mount);
                if let Some(materialization) = materialization {
                    sandbox.memory_materializations.push(materialization);
                }
            }
            let receipt = crate::realization_marker::RealizationCompletionReceipt::new(
                &sandbox.realized,
                sandbox.memory_materializations.clone(),
            )?;
            let (realized, materializations) =
                crate::replay_completion_receipt(spec, &receipt, pc::Realization::Copy)?;
            sandbox.realized = realized;
            sandbox.memory_materializations = materializations;
            Ok::<_, pc::SandboxError>(receipt)
        }
        .await;
        let receipt = match realization {
            Ok(receipt) => receipt,
            Err(cause) => {
                return Err(crate::compensate_creation_failure(
                    realization_guard,
                    &mut *sandbox.memory_mounts.lock().await,
                    &sandbox.root,
                    &sandbox.secret_paths,
                    "sandbox realization",
                    cause,
                )
                .await);
            }
        };
        sandbox.realization = match realization_guard.complete(&receipt) {
            Ok(realization) => realization,
            Err(cause) => {
                return Err(crate::compensate_creation_failure(
                    realization_guard,
                    &mut *sandbox.memory_mounts.lock().await,
                    &sandbox.root,
                    &sandbox.secret_paths,
                    "sandbox Ready publication",
                    cause,
                )
                .await);
            }
        };
        let mut memory_state = sandbox.memory_mounts.lock().await;
        realization_guard.with_authority(|| {
            memory_state.confirm_all();
            Ok(())
        })?;
        drop(memory_state);
        self.select_execution_boundary(&sandbox)?;
        realization_guard.with_authority(|| Ok(()))?;
        Ok(sandbox)
    }

    /// Re-open a local sandbox from its durable handle without erasing any files.
    ///
    /// This is the concrete counterpart of [`pc::SandboxProvider::adopt`], exposed
    /// for hosts that need the local-tier helpers on [`LocalSandbox`]. A handle is
    /// provider-scoped: accepting a container/k8s handle here would silently place
    /// work on the wrong isolation tier, so the boundary fails closed.
    pub async fn adopt_sandbox(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        self.adopt_sandbox_inner(None, handle, None).await
    }

    pub async fn adopt_sandbox_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        self.adopt_sandbox_inner(Some(spec), handle, Some((effect_fence, authority)))
            .await
    }

    /// Adopt through the frozen security authority. Exact restoration evidence
    /// selects the provider-owned restore root; ordinary handles continue
    /// through the existing marker verifier.
    pub async fn adopt_sandbox_with_spec(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        if handle.restoration().is_none() {
            return self.adopt_sandbox_inner(Some(spec), handle, None).await;
        }
        #[cfg(target_os = "linux")]
        if self.execution_deployment.is_some() {
            return Err(pc::SandboxError::unsupported(
                "scoped restored realization is not qualified",
            ));
        }
        self.prepare(spec)?;
        if handle.sandbox_id != spec.scope {
            return Err(pc::SandboxError::conflict(
                "local Sandbox handle does not match SandboxSpec scope",
            ));
        }
        let evidence = handle.restoration().ok_or_else(|| {
            pc::SandboxError::not_found("restored local handle has no exact evidence")
        })?;
        let payload = handle.local_payload()?;
        if evidence.sandbox_spec_fingerprint() != pc::sandbox_spec_security_fingerprint(spec)
            || pc::validate_checkpoint_exclusions_for_spec(
                &payload.continuation_excluded_paths,
                spec,
            )
            .is_err()
            || evidence.checkpoint_exclusions_fingerprint()
                != pc::checkpoint_exclusions_fingerprint(&payload.continuation_excluded_paths)
            || payload.deny_tool_egress != spec.deny_tool_egress
            || payload.outputs_path != spec.outputs_path
            || payload.base_env != spec.env
        {
            return Err(pc::SandboxError::conflict(
                "restored local handle differs from the exact SandboxSpec or checkpoint exclusions",
            ));
        }
        let root = restore_target::restoration_root(&self.base, evidence)?;
        restore_target::verify_binding(&root, evidence)?;
        let mut sandbox = self.build_at(
            &handle.sandbox_id,
            &payload.outputs_path,
            root,
            Some(handle.clone()),
        );
        Self::project_durable_handle_fields(&mut sandbox, &payload)?;
        Ok(sandbox)
    }

    /// Enter the terminal edge of the same marker lifecycle and reconstruct at
    /// most the exact physical participant that still requires disposal. No
    /// ordinary adoption, process, or Hand effect occurs here. An exact Ready
    /// FUSE participant is reacquired through the canonical MemoryMounter.
    pub async fn prepare_terminal_sandbox_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<LocalSandbox>, pc::SandboxError> {
        let fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
        let (id, outputs_path, durable_payload, owned_paths) = if let Some(handle) = handle {
            let payload = handle.local_payload()?;
            if handle.realization_fingerprint() != Some(&fingerprint)
                || payload.outputs_path != spec.outputs_path
                || payload.base_env != spec.env
            {
                return Err(pc::SandboxError::conflict(
                    "Local terminal handle does not match the effective authorization spec",
                ));
            }
            (
                handle.sandbox_id.clone(),
                payload.outputs_path.clone(),
                Some(payload.clone()),
                handle.owned_paths().unwrap_or_default().to_vec(),
            )
        } else {
            (
                spec.scope.clone(),
                spec.outputs_path.clone(),
                None,
                spec.mounts
                    .iter()
                    .map(|mount| mount.mount_path.clone())
                    .collect(),
            )
        };
        let mut sandbox = self.build(&id, &outputs_path);
        sandbox.secret_paths = spec
            .mounts
            .iter()
            .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
            .map(|mount| {
                sandbox
                    .root
                    .resolve(&mount.mount_path)
                    .map_err(pc::SandboxError::from)
            })
            .collect::<Result<_, _>>()?;
        let host_paths = spec
            .mounts
            .iter()
            .map(|mount| {
                sandbox
                    .root
                    .resolve(&mount.mount_path)
                    .map_err(pc::SandboxError::from)
            })
            .collect::<Result<Vec<_>, _>>()?;
        // The provider-neutral terminal Memory owner performs the one typed
        // preflight->takeover->receipt/FUSE replay sequence. Local only supplies
        // its root/path projection and wraps the returned physical participant.
        let terminal = crate::prepare_terminal_memory_replay(
            &self.memory_mounter,
            sandbox.memory_mounts.clone(),
            sandbox.root.root(),
            &fingerprint,
            spec,
            handle,
            expected_effect_fence,
            terminal_effect_fence,
            authority,
            pc::Realization::Copy,
            &host_paths,
        )
        .await?;
        let Some(crate::TerminalMemoryReplay {
            realization,
            removal,
            owned_root_present: _,
            realized,
            materializations,
            memory_mounts,
        }) = terminal
        else {
            return Ok(None);
        };
        sandbox.realized = realized;
        sandbox.realization = crate::realization_marker::LiveRealization::Current(realization);
        self.select_execution_boundary(&sandbox)?;
        *sandbox.terminal_removal.get_mut() = Some(removal);
        if let Some(payload) = durable_payload.as_ref() {
            Self::project_durable_handle_fields(&mut sandbox, payload)?;
        } else {
            Self::project_spec_handle_fields(&mut sandbox, spec)?;
        }
        *sandbox
            .owned_paths
            .lock()
            .expect("owned paths lock poisoned") = owned_paths;
        sandbox.memory_materializations = materializations;
        sandbox.memory_mounts = memory_mounts;
        Ok(Some(sandbox))
    }

    async fn adopt_sandbox_inner(
        &self,
        spec: Option<&pc::SandboxSpec>,
        handle: &pc::SandboxHandle,
        effect: Option<(&pc::SandboxEffectFence, &dyn pc::CurrentExecutionAuthority)>,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        let effect_fence = effect.map(|(fence, _)| fence);
        let admit = || {
            effect.map_or(Ok(()), |(fence, authority)| {
                pc::with_current_sandbox_effect(fence, authority, || Ok(()))
            })
        };
        admit()?;
        let payload = handle.local_payload()?;
        #[cfg(target_os = "linux")]
        if self.execution_deployment.is_some() && spec.is_none() {
            return Err(pc::SandboxError::invalid(
                "execution deployment requires the frozen adoption spec",
            ));
        }
        if let Some(spec) = spec {
            self.prepare(spec)?;
        }
        if let Some(spec) = spec
            && (handle.realization_fingerprint()
                != Some(&pc::SandboxRealizationFingerprint::from_spec(spec))
                || payload.outputs_path != spec.outputs_path
                || payload.base_env != spec.env)
        {
            return Err(pc::SandboxError::conflict(
                "Local sandbox handle does not match the effective adoption spec",
            ));
        }
        let mut sandbox = self.build_at(
            &handle.sandbox_id,
            &payload.outputs_path,
            crate::sandbox_dir(&self.base, &handle.sandbox_id),
            handle.restoration().map(|_| handle.clone()),
        );
        let verified = crate::realization_marker::verify_adoption(
            sandbox.root.root(),
            handle.realization_fingerprint(),
            handle.filesystem_effect_fence()?,
            handle.filesystem_physical_incarnation()?.as_deref(),
            effect_fence,
        )?;
        let completion = verified.as_ref().map(|(_, receipt)| receipt.clone());
        let current_evidence = verified.as_ref().map(|(evidence, _)| evidence.clone());
        sandbox.realization = match verified {
            Some((evidence, _)) => crate::realization_marker::LiveRealization::Current(evidence),
            None => crate::realization_marker::LiveRealization::LegacyAdopted,
        };
        self.select_execution_boundary(&sandbox)?;
        Self::project_durable_handle_fields(&mut sandbox, &payload)?;
        if let Some(spec) = spec {
            sandbox.secret_paths = spec
                .mounts
                .iter()
                .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
                .map(|mount| {
                    sandbox
                        .root
                        .resolve(&mount.mount_path)
                        .map_err(pc::SandboxError::from)
                })
                .collect::<Result<_, _>>()?;
        }
        *sandbox
            .owned_paths
            .lock()
            .expect("owned paths lock poisoned") =
            handle.owned_paths().unwrap_or_default().to_vec();
        if let Some(receipt) = completion.as_ref() {
            let host_paths = spec
                .map(|spec| {
                    spec.mounts
                        .iter()
                        .map(|mount| {
                            sandbox
                                .root
                                .resolve(&mount.mount_path)
                                .map_err(pc::SandboxError::from)
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?;
            let crate::AdoptionMemoryReplay {
                realized,
                materializations,
                memory_mounts,
            } = crate::prepare_adoption_memory_replay(
                &self.memory_mounter,
                sandbox.memory_mounts.clone(),
                sandbox.root.root(),
                current_evidence.as_ref(),
                effect_fence,
                effect.map(|(_, authority)| authority),
                spec,
                receipt,
                handle,
                pc::Realization::Copy,
                host_paths.as_deref(),
            )
            .await?;
            sandbox.realized = realized;
            sandbox.memory_materializations = materializations;
            sandbox.memory_mounts = memory_mounts;
        } else {
            sandbox.memory_materializations = handle
                .memory_materializations()?
                .unwrap_or_default()
                .to_vec();
        }
        admit()?;
        Ok(sandbox)
    }

    /// Static capability evidence shared by provider admission and owners of an
    /// already-created local environment.
    pub fn capabilities() -> pc::SandboxCapabilities {
        pc::SandboxCapabilities::default()
    }

    async fn realize_mount(
        &self,
        root: &IsolatedRoot,
        root_identity: awaken_sandbox_fs::DirectoryIdentity,
        req: &pc::MountRequirement,
        retained_mounts: &tokio::sync::Mutex<crate::MemoryMountState>,
        mutation_guard: &crate::realization_marker::ProviderCreationGuard<'_>,
    ) -> Result<(pc::RealizedMount, Option<pc::MemoryMaterializationEvidence>), pc::SandboxError>
    {
        let host = root
            .resolve(&req.mount_path)
            .map_err(pc::SandboxError::from)?;
        // A memory_store is a genuine keyed store (ADR-0038/0053), not a byte blob:
        // realize it through the injected mounter (FUSE where the kernel supports it,
        // else a harvested copy). Without a mounter, fail loud rather than fake it
        // with an empty file that misleads the agent into thinking it has a store.
        if matches!(&req.source, pc::MountSource::MemoryStore { .. }) {
            mutation_guard.validate_before_mutation()?;
            let mut retained = retained_mounts.lock().await;
            mutation_guard.validate_before_mutation()?;
            let mounted =
                crate::mount_memory_requirement(&self.memory_mounter, req, &host, &mut retained)
                    .await?;
            mutation_guard.validate_before_mutation()?;
            return mounted.ok_or_else(|| {
                pc::SandboxError::invalid(
                    "Memory requirement did not reach the canonical Memory mount adapter",
                )
            });
        }
        let secret_broker = self
            .secret_broker
            .read()
            .expect("secret broker lock poisoned")
            .clone();
        if secret_broker.is_some()
            && matches!(req.source, pc::MountSource::Secret { .. })
            && req.access == pc::MountAccess::ReadWrite
        {
            return Err(pc::SandboxError::unsupported(
                "the Workdir provider cannot write back a brokered writable Secret mount",
            ));
        }
        mutation_guard.validate_before_mutation()?;
        let bytes = resolve_source(
            &req.source,
            &self.blobs,
            &self.file_store,
            secret_broker.as_ref(),
        )
        .await?;
        let realized = match bytes {
            Some(bytes) => {
                verify(&req.source, &bytes)?; // fail closed on content-hash mismatch
                let relative = host.strip_prefix(root.root()).map_err(|_| {
                    pc::SandboxError::invalid("mount path escaped its exact sandbox root")
                })?;
                mutation_guard.with_mutation(|| {
                    awaken_sandbox_fs::write_relative_file_atomic(
                        root.root(),
                        root_identity,
                        relative,
                        &bytes,
                        0o600,
                    )
                    .map_err(|error| pc::SandboxError::backend("realize_mount", error))
                })?;
                pc::RealizedMount {
                    mount_id: req.mount_id.clone(),
                    mount_path: req.mount_path.clone(),
                    access: req.access,
                    realization: pc::Realization::Copy,
                    content_hash: Some(content_fingerprint(&bytes)),
                }
            }
            None if req.required => {
                return Err(pc::SandboxError::not_found(format!(
                    "required mount {:?} has no resolvable source on the local provider",
                    req.mount_id
                )));
            }
            None => {
                // Optional + unresolvable: create an empty placeholder so the path exists.
                let relative = host.strip_prefix(root.root()).map_err(|_| {
                    pc::SandboxError::invalid("mount path escaped its exact sandbox root")
                })?;
                mutation_guard.with_mutation(|| {
                    awaken_sandbox_fs::write_relative_file_atomic(
                        root.root(),
                        root_identity,
                        relative,
                        b"",
                        0o600,
                    )
                    .map_err(|error| pc::SandboxError::backend("realize_mount", error))
                })?;
                pc::RealizedMount {
                    mount_id: req.mount_id.clone(),
                    mount_path: req.mount_path.clone(),
                    access: req.access,
                    realization: pc::Realization::Copy,
                    content_hash: None,
                }
            }
        };
        Ok((realized, None))
    }

    fn build(&self, id: &str, outputs_path: &str) -> LocalSandbox {
        self.build_at(id, outputs_path, crate::sandbox_dir(&self.base, id), None)
    }

    fn build_at(
        &self,
        id: &str,
        outputs_path: &str,
        dir: PathBuf,
        adopted_handle: Option<pc::SandboxHandle>,
    ) -> LocalSandbox {
        let memory_mounts = self.memory_mounts.owner(&dir);
        LocalSandbox {
            id: id.to_string(),
            root: IsolatedRoot::new(dir),
            outputs_path: outputs_path.to_string(),
            deny_egress: false,
            inherit_agent_stderr: self.inherit_agent_stderr,
            trusted_host_access: self.trusted_host_access,
            base_env: Vec::new(),
            secret_broker: self.secret_broker.clone(),
            realized: Vec::new(),
            secret_paths: Vec::new(),
            memory_mounts,
            memory_materializations: Vec::new(),
            memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
            continuation_excluded_paths: Vec::new(),
            realization: crate::realization_marker::LiveRealization::LegacyAdopted,
            terminal_removal: tokio::sync::Mutex::new(None),
            owned_paths: std::sync::Mutex::new(Vec::new()),
            adopted_handle,
            #[cfg(target_os = "linux")]
            execution: std::sync::OnceLock::new(),
        }
    }
}

#[async_trait]
impl pc::SandboxProvider for LocalProvider {
    /// Ordinary Workdir owns its filesystem root; a configured execution
    /// boundary additionally attests the actual installed physical mechanism.
    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        #[cfg(target_os = "linux")]
        if let Some(execution) = &self.execution_deployment {
            return execution.probe_ready().await;
        }
        Ok(())
    }

    fn capabilities(&self) -> pc::SandboxCapabilities {
        Self::capabilities()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        vec!["awaken-fs-tar-v1".into()]
    }

    /// The durable record is the realization marker this creation's own
    /// synchronous prologue writes; `observe_adoption` is the scan that reads
    /// it back. The retention covers the one window the marker cannot: a
    /// caller dropped before that write, which would otherwise leave the root
    /// discoverable only by walking the whole provider base.
    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a pc::SandboxSpec,
        effect_fence: &'a pc::SandboxEffectFence,
        authority: &'a dyn pc::CurrentExecutionAuthority,
    ) -> pc::ClaimedEffect<'a, Box<dyn pc::Sandbox>> {
        let claim = pc::EffectClaim::durable(
            crate::abandoned_realization::REALIZATION_RECONCILER,
            self.abandoned
                .retain(crate::sandbox_dir(&self.base, &spec.scope)),
        );
        pc::ClaimedEffect::new(claim, async move {
            Ok(Box::new(
                self.create_sandbox_for_effect(spec, effect_fence, None, authority)
                    .await?,
            ) as Box<dyn pc::Sandbox>)
        })
    }

    async fn observe(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        handle.local_payload()?;
        crate::realization_marker::observe_adoption(
            &crate::sandbox_dir(&self.base, &handle.sandbox_id),
            handle.realization_fingerprint(),
            handle.filesystem_effect_fence()?,
            handle.filesystem_physical_incarnation()?.as_deref(),
            None,
        )
    }

    async fn observe_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        handle.local_payload()?;
        if handle.realization_fingerprint()
            != Some(&pc::SandboxRealizationFingerprint::from_spec(spec))
        {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: "Local sandbox handle does not match the effective observation spec".into(),
            });
        }
        crate::realization_marker::observe_adoption(
            &crate::sandbox_dir(&self.base, &handle.sandbox_id),
            handle.realization_fingerprint(),
            handle.filesystem_effect_fence()?,
            handle.filesystem_physical_incarnation()?.as_deref(),
            Some(effect_fence),
        )
    }

    async fn adopt_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(
            self.adopt_sandbox_for_effect(spec, handle, effect_fence, authority)
                .await?,
        ))
    }

    async fn prepare_terminal_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<Box<dyn pc::Sandbox>>, pc::SandboxError> {
        Ok(self
            .prepare_terminal_sandbox_for_effect(
                spec,
                handle,
                expected_effect_fence,
                terminal_effect_fence,
                authority,
            )
            .await?
            .map(|sandbox| Box::new(sandbox) as Box<dyn pc::Sandbox>))
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

    async fn restore(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxRestoreResult<Box<dyn pc::Sandbox>>, pc::SandboxError> {
        Ok(self
            .restore_exact_sandbox(spec, request, store)
            .await?
            .map_target(|sandbox| Box::new(sandbox) as Box<dyn pc::Sandbox>))
    }

    async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        self.dispose_restore_sandbox(spec, request, authorization, authority)
            .await
    }
}

impl LocalProvider {
    // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
    // are no longer a trait obligation or a reachable escape hatch. They stay
    // as this type's own implementation detail, used by its fenced edges.
    pub async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.create_sandbox(spec).await?))
    }
    pub async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.adopt_sandbox(handle).await?))
    }
}

/// A realized local environment: an [`IsolatedRoot`] directory plus its outputs
/// path and base env. Path-jailed but not OS-confined (Workdir tier).
pub struct LocalSandbox {
    id: String,
    root: IsolatedRoot,
    outputs_path: String,
    /// Egress denied for this sandbox's rooted in-process tools (derived from the
    /// spec's [`NetworkPolicy`](pc::NetworkPolicy)); threaded into [`rooted_tools`].
    deny_egress: bool,
    inherit_agent_stderr: bool,
    trusted_host_access: bool,
    base_env: Vec<pc::EnvVar>,
    /// Shared broker installation; only opaque refs are retained in `base_env`.
    secret_broker: Arc<std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>>,
    realized: Vec<pc::RealizedMount>,
    /// Host paths of realized `Secret` mounts, **shredded** (overwritten) at
    /// [teardown](pc::Sandbox::dispose_for_effect) before the directory is reaped so a
    /// materialized credential does not linger in freed disk blocks (ADR-0023).
    secret_paths: Vec<PathBuf>,
    /// Live memory-store mounts (FUSE / copy), torn down (unmount / harvest) at
    /// [teardown](pc::Sandbox::dispose_for_effect) before the sandbox directory is reaped.
    memory_mounts: crate::MemoryMountOwner,
    /// Canonical durable heads captured from copy-backed mounts at create, or
    /// recovered verbatim from a validated current handle at adoption.
    memory_materializations: Vec<pc::MemoryMaterializationEvidence>,
    /// One process-local exact-evidence/effect acknowledgement. Durable Memory
    /// heads remain owned by the handle and store; this only gates guard drain
    /// against physical disposal.
    memory_reconciliation_ack: pc::MemoryReconciliationAck,
    /// Host paths whose contents have an independent durable authority or carry
    /// credentials. They are rematerialized from that authority after restore.
    continuation_excluded_paths: Vec<PathBuf>,
    /// One coherent marker/legacy realization authority; current evidence
    /// cannot drift across parallel optional fingerprint/fence/incarnation fields.
    realization: crate::realization_marker::LiveRealization,
    /// The exact prepared participant remains here across cancelled waits in
    /// checkpoint cleanup and physical disposal, until Removed is committed.
    terminal_removal: tokio::sync::Mutex<Option<crate::realization_marker::RemovalGuard>>,
    /// Complete sandbox-visible trees frozen into the current V2 handle.
    owned_paths: std::sync::Mutex<Vec<String>>,
    /// Exact restore-bound handle retained only for the unpublished/restored
    /// physical target. Ordinary P/V2 handles remain derived from the marker.
    adopted_handle: Option<pc::SandboxHandle>,
    /// Selected before the first spawn. `Unsupervised` freezes the explicitly weaker
    /// path, so installing supervision later cannot overlook earlier children.
    #[cfg(target_os = "linux")]
    execution: std::sync::OnceLock<execution::ExecutionBinding>,
}

struct PreparedCommand {
    command: pc::MaterializedCommand,
    cwd: PathBuf,
}

impl PreparedCommand {
    fn into_tokio(self) -> TokioCommand {
        let mut cmd = TokioCommand::new(&self.command.argv[0]);
        cmd.args(&self.command.argv[1..])
            .current_dir(self.cwd)
            .env_clear();
        for var in &self.command.env {
            cmd.env(&var.name, var.value.expose());
        }
        cmd
    }
}

impl LocalSandbox {
    #[must_use]
    pub fn has_trusted_host_access(&self) -> bool {
        self.trusted_host_access
    }

    fn root_identity_for_access(
        &self,
    ) -> Result<Option<awaken_sandbox_fs::DirectoryIdentity>, pc::SandboxError> {
        self.realization.root_identity_for_access(self.root.root())
    }

    fn require_root_identity(
        &self,
    ) -> Result<awaken_sandbox_fs::DirectoryIdentity, pc::SandboxError> {
        self.realization.require_root_identity(self.root.root())
    }

    /// Reserve one provider-visible path in the durable V2 handle before a
    /// live projection effect starts. Reservations are monotonic: retaining a
    /// disjoint historical path is safer than losing evidence across a crash.
    pub fn reserve_owned_path(&self, path: &str) {
        let mut owned = self.owned_paths.lock().expect("owned paths lock poisoned");
        if !owned.iter().any(|current| current == path) {
            owned.push(path.to_string());
        }
    }

    /// Absolute host path of the Workdir tier's complete Session filesystem.
    ///
    /// ACP carries a `cwd` independently from the child process cwd. A bound ACP
    /// adapter must send this exact path in `session/new`; otherwise the protocol
    /// default `/` makes the CLI's own file tools operate outside the realized
    /// Session tree even though the process itself was launched here.
    pub fn workspace_path(&self) -> &std::path::Path {
        self.root.root()
    }

    /// Materialize a runtime-owned, read-only file tree below the sandbox root.
    /// Every relative path is revalidated by [`IsolatedRoot`], existing symlinks are
    /// rejected, and permissions are narrowed only after the complete tree is
    /// written. This is a generic provisioning primitive; it knows no Skill,
    /// Workspace, principal, or authorization policy.
    pub fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>, bool)],
    ) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        materialize_read_only_tree_at(&self.root, root_identity, subdir, files)
    }

    /// Tear down every live memory mount: unmount a FUSE mount, or harvest a writable
    /// copy back to its store. Drains the guard list so a later dispose is a no-op.
    pub async fn release_memory_mounts(&self) -> Result<(), pc::SandboxError> {
        crate::release_memory_mounts(&self.memory_mounts, || Ok(())).await
    }

    /// Remove only the runtime-owned resource projection, preserving the rest of
    /// the Session workspace. Used when a live input manifest is replaced: a
    /// detached resource must not remain reachable as stale bytes under `.mnt`.
    pub fn clear_resource_projection(&self) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        awaken_sandbox_fs::remove_relative_entry_exact(
            self.root.root(),
            root_identity,
            std::path::Path::new(pc::WorkspaceLayout::RESOURCE_PROJECTION_SUBDIR),
        )
        .map_err(|error| pc::SandboxError::backend("clear_resource_projection", error))
    }

    fn host_outputs(&self) -> Result<PathBuf, pc::SandboxError> {
        self.root
            .resolve(&self.outputs_path)
            .map_err(pc::SandboxError::from)
    }

    /// Outputs scan → one descriptor-captured `(artifact, bytes)` snapshot.
    fn scan(&self) -> Result<Vec<(pc::Artifact, Vec<u8>)>, pc::SandboxError> {
        let Some(root_identity) = self.root_identity_for_access()? else {
            return Ok(Vec::new());
        };
        crate::artifacts::scan_outputs(&self.root, root_identity, &self.outputs_path)
    }

    /// Build the child command (program, args, jailed cwd, reserved + declared env),
    /// leaving stdio for the caller to configure. Shared by `spawn`/`spawn_agent`.
    async fn build_command(
        &self,
        command: pc::Command,
    ) -> Result<PreparedCommand, pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        let broker = self
            .secret_broker
            .read()
            .expect("secret broker lock poisoned")
            .clone();
        let mut command =
            pc::materialize_process_command(&self.base_env, command, broker.as_ref()).await?;
        if command.argv.is_empty() {
            return Err(pc::SandboxError::invalid("command argv is empty"));
        }
        let cwd_logical = if command.cwd.is_empty() {
            "/"
        } else {
            &command.cwd
        };
        let host_cwd = self
            .root
            .resolve(cwd_logical)
            .map_err(pc::SandboxError::from)?;
        awaken_sandbox_fs::create_relative_directory_all(
            self.root.root(),
            root_identity,
            host_cwd.strip_prefix(self.root.root()).map_err(|_| {
                pc::SandboxError::invalid("command cwd escaped its exact sandbox root")
            })?,
        )
        .map_err(|error| pc::SandboxError::backend("build_command", error))?;
        let host_outputs = self.host_outputs()?;
        awaken_sandbox_fs::create_relative_directory_all(
            self.root.root(),
            root_identity,
            host_outputs.strip_prefix(self.root.root()).map_err(|_| {
                pc::SandboxError::invalid("outputs path escaped its exact sandbox root")
            })?,
        )
        .map_err(|error| pc::SandboxError::backend("build_command", error))?;

        // Reserved, runtime-owned env: where the agent writes/works (the local tier
        // has no path fidelity, so a process finds the outputs dir via this var).
        let paths = crate::RuntimePathEnv::new(
            host_cwd.to_string_lossy().into_owned(),
            host_outputs.to_string_lossy().into_owned(),
        );
        for (name, value) in paths.entries() {
            if !command.env.iter().any(|var| var.name == name) {
                command.env.push(pc::MaterializedEnvVar {
                    name: name.into(),
                    value: pc::MaterializedEnvValue::Inline(value.into()),
                });
            }
        }
        Ok(PreparedCommand {
            command,
            cwd: host_cwd,
        })
    }

    /// The tool-transparent capability (ADR-0041 amendment): launch an opaque agent
    /// with piped stdio and hand back its [`pc::ProcessHandle`] plus a duplex
    /// [`AgentChannel`] (its stdout+stdin). The bridge drives ACP over the channel
    /// while the supervisor polls the handle. The Workdir tier realizes the same
    /// channel plumbing the transparent tiers use; production agents still require a
    /// `tool_transparent` tier (gated by `prepare_environment`).
    pub async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannel>), pc::SandboxError> {
        #[cfg(target_os = "linux")]
        if let execution::ExecutionBinding::Scoped(execution) = self
            .execution
            .get_or_init(|| execution::ExecutionBinding::Unsupervised)
        {
            let mut process = execution
                .spawn(
                    self,
                    command,
                    awaken_local_process::scoped::ScopedStdio::agent(self.inherit_agent_stderr),
                )
                .await?;
            let stdin = process.take_stdin().ok_or_else(|| {
                pc::SandboxError::backend("spawn_agent", "agent stdin was not piped")
            })?;
            let stdout = process.take_stdout().ok_or_else(|| {
                pc::SandboxError::backend("spawn_agent", "agent stdout was not piped")
            })?;
            return Ok((
                execution.track(process)?,
                Box::new(SplitChannel::new(stdout, stdin)),
            ));
        }
        let mut cmd = self.build_command(command).await?.into_tokio();
        let stderr = if self.inherit_agent_stderr {
            ProcStdio::inherit()
        } else {
            ProcStdio::null()
        };
        cmd.stdin(ProcStdio::piped())
            .stdout(ProcStdio::piped())
            .stderr(stderr);
        let mut process = LocalProcess::spawn(&mut cmd)
            .map_err(|error| pc::SandboxError::backend("spawn_agent", error))?;
        let stdin = process
            .take_stdin()
            .ok_or_else(|| pc::SandboxError::backend("spawn_agent", "agent stdin was not piped"))?;
        let stdout = process.take_stdout().ok_or_else(|| {
            pc::SandboxError::backend("spawn_agent", "agent stdout was not piped")
        })?;
        let channel: Box<dyn AgentChannel> = Box::new(SplitChannel::new(stdout, stdin));
        Ok((Box::new(process), channel))
    }

    // --- Host-tier helpers (Workdir tier) -----------------------------------
    // The trusted, single-machine capability surface the host composes into a run.
    // These operate on the sandbox's realized root — path fidelity is a lexical
    // convenience here (`tool_transparent = false`), not an isolation boundary. They
    // replace the legacy `Environment` methods; OS enforcement is the container tier's.

    /// The full provisioned capability surface (ADR-0035 D8): the sandbox's rooted
    /// in-process tools as `RawTool`s, jailed to the root with egress per the spec.
    /// This is what the host registers on the runtime (the pc-model `Environment::tools`).
    pub fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        let outputs = self
            .root
            .resolve(&self.outputs_path)
            .expect("validated Sandbox outputs path");
        rooted_raw_tools(
            self.root.clone(),
            outputs.clone(),
            crate::RuntimePathEnv::new(
                self.root.root().to_string_lossy().into_owned(),
                outputs.to_string_lossy().into_owned(),
            ),
            self.deny_egress,
        )
    }

    /// List regular files under `<root>/<subdir>` as `(logical_path, bytes)` — a
    /// session's output artifacts or a memory-harvest read. Paths are logical (G3).
    pub fn list_files(&self, subdir: &str) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        let Some(root_identity) = self.root_identity_for_access()? else {
            if !self.memory_materializations.is_empty() {
                return Err(pc::SandboxError::gone(
                    "copy-backed Memory lost its exact sandbox root before reconciliation",
                ));
            }
            return Ok(Vec::new());
        };
        let files = list_files_at(&self.root, root_identity, subdir.trim_start_matches('/'))?;
        // The neutral scanner treats a concurrently missing physical root as
        // empty. Recovered Memory reconciliation cannot: an empty candidate
        // after root loss would authorize deletion of durable heads. Recheck
        // the exact incarnation after the descriptor-captured snapshot.
        if !self.memory_materializations.is_empty() {
            self.require_root_identity()?;
        }
        Ok(files)
    }

    /// Scan `<root>/<subdir>/*/SKILL.md` live, returning neutral file data (the host
    /// parses the skill model). Re-scanned each call so a run-authored skill is seen.
    pub fn scan_skill_dir(
        &self,
        subdir: &str,
    ) -> Result<Vec<DiscoveredSkillFile>, pc::SandboxError> {
        let Some(root_identity) = self.root_identity_for_access()? else {
            return Ok(Vec::new());
        };
        scan_skill_dir_at(
            &self.root,
            root_identity,
            subdir.trim_start_matches('/'),
            subdir.trim_start_matches('/'),
        )
    }

    /// Materialize host-projected, non-secret configuration inside this existing
    /// sandbox. This is the late-bound counterpart of an inline mount: ACP config
    /// depends on the resolved Run snapshot, but writing it must not require a
    /// second sandbox. Paths are resolved by the same jail used by every helper.
    pub fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        awaken_sandbox_fs::write_relative_file_atomic(
            self.root.root(),
            root_identity,
            std::path::Path::new(logical.trim_start_matches('/')),
            contents,
            0o600,
        )
        .map_err(|error| pc::SandboxError::backend("materialize_inline", error))
    }

    /// Remove one dynamically projected workspace path without tearing down the
    /// Session environment. The same lexical jail used by materialization rejects
    /// escape attempts; missing paths are an idempotent success.
    pub fn remove_inline(&self, logical: &str) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        awaken_sandbox_fs::remove_relative_entry_exact(
            self.root.root(),
            root_identity,
            std::path::Path::new(logical.trim_start_matches('/')),
        )
        .map_err(|error| pc::SandboxError::backend("remove_inline", error))
    }
}

#[async_trait]
impl pc::RepositoryRealizer for LocalSandbox {
    async fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        self.require_root_identity()?;
        plan.validate_mount_path()?;
        #[cfg(target_os = "linux")]
        if let Some(outcome) = self.scoped_repository(plan, None, credential).await? {
            match outcome.into_result()? {
                crate::repository_effect::Outcome::Realized => {
                    self.reserve_owned_path(&plan.mount_path);
                    return Ok(());
                }
                _ => {
                    return Err(pc::SandboxError::conflict(
                        "repository realization returned another operation's receipt",
                    ));
                }
            }
        }
        provision_repo_at(
            &self.root,
            &plan.mount_path,
            &plan.transport_url,
            plan.initial_branch.as_deref(),
            plan.initial_commit.as_deref(),
            credential,
        )?;
        self.reserve_owned_path(&plan.mount_path);
        Ok(())
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        self.require_root_identity()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        plan.validate_mount_path()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        #[cfg(target_os = "linux")]
        if let Some(outcome) = self
            .scoped_repository(plan, Some(expectation), credential)
            .await
            .map_err(pc::RepositoryPublicationError::Unavailable)?
        {
            use crate::repository_effect::Outcome;
            return match outcome
                .into_result()
                .map_err(pc::RepositoryPublicationError::Unavailable)?
            {
                Outcome::Published(receipt)
                    if receipt == pc::RepositoryPublicationReceipt::new(plan, expectation) =>
                {
                    Ok(receipt)
                }
                Outcome::Rejected(rejection) => {
                    rejection.verify(expectation)?;
                    Err(pc::RepositoryPublicationError::Rejected(rejection))
                }
                _ => Err(pc::RepositoryPublicationError::Unavailable(
                    pc::SandboxError::conflict(
                        "repository publication returned another operation's receipt",
                    ),
                )),
            };
        }
        push_repo_to_at(&self.root, &plan.mount_path, plan, expectation, credential)
    }
}

#[async_trait]
impl pc::Sandbox for LocalSandbox {
    async fn dispose_unattached(&self) -> Result<(), pc::SandboxError> {
        self.dispose().await
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        if let Some(handle) = &self.adopted_handle {
            return handle.clone();
        }
        let continuation_excluded_paths = self
            .continuation_excluded_paths
            .iter()
            .filter_map(|path| path.strip_prefix(self.root.root()).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let previous = crate::LocalSandboxHandleV1 {
            outputs_path: self.outputs_path.clone(),
            base_env: self.base_env.clone(),
            continuation_excluded_paths,
            deny_tool_egress: self.deny_egress,
        };
        match self.realization.current() {
            Some(realization) => pc::SandboxHandle::local_v2(
                &self.id,
                crate::LocalSandboxHandleV2 {
                    previous,
                    realization_fingerprint: realization.fingerprint().clone(),
                    effect_fence: realization.effect_fence().clone(),
                    physical_incarnation: realization.physical_incarnation().to_owned(),
                    owned_paths: self
                        .owned_paths
                        .lock()
                        .expect("owned paths lock poisoned")
                        .clone(),
                },
            )
            .with_memory_materializations(self.memory_materializations.clone())
            .expect("Local sandbox cached canonical Memory materialization evidence"),
            // V1 adoption is a decode-only compatibility path. Re-emitting V1
            // preserves that boundary and cannot fabricate a marker or path WAL.
            None => pc::SandboxHandle::local(&self.id, previous),
        }
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let stdio = command.stdio;
        #[cfg(target_os = "linux")]
        if let execution::ExecutionBinding::Scoped(execution) = self
            .execution
            .get_or_init(|| execution::ExecutionBinding::Unsupervised)
        {
            let process = execution.spawn(self, command, stdio.into()).await?;
            return execution.track(process);
        }
        let mut cmd = self.build_command(command).await?.into_tokio();
        let (out, e) = match stdio {
            pc::Stdio::Inherit => (ProcStdio::inherit(), ProcStdio::inherit()),
            pc::Stdio::Piped => (ProcStdio::piped(), ProcStdio::piped()),
            pc::Stdio::Null => (ProcStdio::null(), ProcStdio::null()),
        };
        cmd.stdout(out).stderr(e);

        Ok(Box::new(LocalProcess::spawn(&mut cmd).map_err(
            |error| pc::SandboxError::backend("spawn", error),
        )?))
    }

    async fn checkpoint(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        self.create_checkpoint(request, store).await
    }

    async fn checkpoint_for_effect(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        self.create_checkpoint_for_effect(request, store, effect_fence, authority)
            .await
    }

    async fn cleanup_checkpoint_for_terminal(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
        expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        self.cleanup_checkpoint_for_terminal(
            request,
            store,
            expected_effect_fence,
            terminal_effect_fence,
            authority,
        )
        .await
    }

    async fn attach(
        &self,
        req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        if self.trusted_host_access {
            pc::validate_trusted_host_mount_requirements(
                std::slice::from_ref(&req),
                &LocalProvider::capabilities(),
            )
            .map_err(pc::SandboxError::from)?;
        } else {
            pc::validate_mount_requirements(
                std::slice::from_ref(&req),
                &LocalProvider::capabilities(),
            )
            .map_err(pc::SandboxError::from)?;
        }
        // Create-time participants keep their original lifecycle authority.
        // Only already-resolved ordinary File bytes use this late attach port.
        let destination = self
            .root
            .resolve(&req.mount_path)
            .map_err(pc::SandboxError::from)?;
        for retained in &self.realized {
            let retained_path = self
                .root
                .resolve(&retained.mount_path)
                .map_err(pc::SandboxError::from)?;
            if destination.starts_with(&retained_path) || retained_path.starts_with(&destination) {
                return Err(pc::SandboxError::conflict(
                    "runtime attachment cannot replace a create-time mount participant",
                ));
            }
        }
        let contents = match &req.source {
            pc::MountSource::Inline { contents } => contents.as_bytes(),
            pc::MountSource::InlineBytes { contents, .. } => contents.as_slice(),
            _ => {
                return Err(pc::SandboxError::invalid(
                    "runtime attachment requires resolved inline File bytes",
                ));
            }
        };
        verify(&req.source, contents)?;
        self.materialize_inline(&req.mount_path, contents)?;
        Ok(pc::RealizedMount {
            mount_id: req.mount_id,
            mount_path: req.mount_path,
            access: req.access,
            realization: pc::Realization::Copy,
            content_hash: Some(content_fingerprint(contents)),
        })
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        Ok(self.scan()?.into_iter().map(|(a, _)| a).collect())
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        for (artifact, bytes) in self.scan()? {
            if artifact.id == id {
                return Ok(bytes);
            }
        }
        Err(pc::SandboxError::not_found(format!(
            "no artifact with id {id:?}"
        )))
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &self.realized
    }

    async fn process(
        &self,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        // A local child is owned by its spawner's process; once that process is
        // gone there is nothing to re-open (no OS-level process registry here).
        Err(pc::SandboxError::unsupported(
            "local provider cannot reattach to a process across owners",
        ))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        let entry = awaken_sandbox_fs::classify_nofollow(self.root.root())
            .map_err(|error| pc::SandboxError::backend("status", error))?;
        match (&self.realization, entry) {
            (_, awaken_sandbox_fs::PathEntry::Absent) => Ok(pc::SandboxStatus::Terminated),
            (
                crate::realization_marker::LiveRealization::Current(evidence),
                awaken_sandbox_fs::PathEntry::Directory(observed),
            ) if observed == evidence.root_identity() => Ok(pc::SandboxStatus::Ready),
            (
                crate::realization_marker::LiveRealization::LegacyCreated(evidence),
                awaken_sandbox_fs::PathEntry::Directory(observed),
            ) if observed == evidence.root_identity() => Ok(pc::SandboxStatus::Ready),
            (
                crate::realization_marker::LiveRealization::LegacyAdopted,
                awaken_sandbox_fs::PathEntry::Directory(_),
            ) => Ok(pc::SandboxStatus::Ready),
            _ => Err(pc::SandboxError::conflict(
                "sandbox root has a foreign file type or physical identity",
            )),
        }
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        // No provider lease. Worker death does not imply local descendants have
        // stopped; this no-op must not be used as a quiescence acknowledgement.
        Ok(())
    }

    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        crate::acknowledge_memory_reconciliation(
            &self.memory_reconciliation_ack,
            &self.memory_mounts,
            effect_fence,
            authority,
            complete_materializations,
            || {
                let handle = pc::Sandbox::handle(self);
                Ok(handle
                    .memory_materializations()?
                    .unwrap_or_default()
                    .to_vec())
            },
            || {
                crate::authorize_terminal_disposal(
                    self.root.root(),
                    &self.realization,
                    &self.terminal_removal,
                    effect_fence,
                )
                .map(|_| ())
            },
        )
        .await
    }

    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        let handle = pc::Sandbox::handle(self);
        let complete_materializations = handle.memory_materializations()?.unwrap_or_default();
        crate::prepare_terminal_disposal(
            &self.memory_reconciliation_ack,
            complete_materializations,
            self.root.root(),
            &self.realization,
            &self.terminal_removal,
            effect_fence,
            authority,
        )
    }

    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError> {
        crate::dispose_terminal_realization(
            &self.memory_mounts,
            self.root.root(),
            &self.secret_paths,
            &self.terminal_removal,
            authorization,
            authority,
            std::future::ready(()),
        )
        .await
    }
}

impl LocalSandbox {
    // Relocated from the Sandbox/SandboxProvider trait: the unfenced ports
    // are no longer a trait obligation or a reachable escape hatch. They stay
    // as this type's own implementation detail, used by its fenced edges.
    pub async fn dispose(&self) -> Result<(), pc::SandboxError> {
        if self
            .adopted_handle
            .as_ref()
            .and_then(pc::SandboxHandle::restoration)
            .is_some()
        {
            return Err(pc::SandboxError::fence_rejected(
                "restored target disposal requires the exact provider request and current authority",
            ));
        }
        if self.realization.current().is_some() {
            return Err(pc::SandboxError::fence_rejected(
                "current durable sandbox disposal requires an aggregate effect fence",
            ));
        }
        self.release_memory_mounts().await?;
        self.shred_secrets()?;
        crate::realization_marker::dispose_legacy(
            self.root.root(),
            self.realization.legacy_live_identity(),
        )
    }
}

impl LocalSandbox {
    /// Overwrite each realized secret file's bytes with zeros before the directory is
    /// reaped, so a materialized credential does not survive in freed disk blocks
    /// (ADR-0023 shred-on-teardown). Every nofollow overwrite must succeed before
    /// root deletion; failure retains the marker and exact root for retry.
    fn shred_secrets(&self) -> Result<(), pc::SandboxError> {
        crate::shred_secret_paths_at(
            &self.root,
            self.root_identity_for_access()?,
            &self.secret_paths,
        )
    }
}

#[cfg(test)]
#[path = "provider/shred_tests.rs"]
mod shred_tests;
/// The Workdir-tier host helpers that replace the legacy `Environment` methods —
/// rooted tools, repo provisioning/write-back, artifact/skill scanning — exercised
/// on a `LocalSandbox` built through the pc `SandboxProvider` path.
#[cfg(test)]
#[path = "provider/workdir_helper_tests.rs"]
pub(crate) mod workdir_helper_tests;
