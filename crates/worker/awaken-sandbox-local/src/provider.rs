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
use awaken_local_process::{LocalProcess, configure_process_group};
use awaken_provisioning_contract as pc;
use tokio::process::Command as TokioCommand;

use std::sync::Arc;

use awaken_runtime_contract::tool::RawTool;

use crate::read_only_tree::materialize_read_only_tree_at;
use crate::{
    DiscoveredSkillFile, IsolatedRoot, content_fingerprint, jailed_at, list_files_at,
    provision_repo_at, push_repo_to_at, rooted_raw_tools, scan_skill_dir_at,
};

mod restore_target;

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

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
            return Err(err(format!(
                "mount content hash mismatch: declared {expected}, realized {got}"
            )));
        }
    }
    Ok(())
}

/// Tighten a materialized secret file to owner-only (`0600`) so other host users
/// cannot read the credential bytes in the window before dispose shreds them.
///
/// This is the *local* provider's ceiling: it realizes onto the host filesystem,
/// so it can restrict permissions but cannot keep the bytes off swap. True
/// anti-swap isolation (a `tmpfs` with `noswap`) is the namespace/container
/// provider's job — see [`SandboxCapabilities`](pc::SandboxCapabilities). The
/// Secret bytes are resolved only at this provider boundary through the dedicated
/// [`SecretBroker`](pc::SecretBroker), restricted immediately, and never carried in
/// the declarative sandbox specification.
#[cfg(unix)]
pub(crate) fn restrict_to_owner(path: &std::path::Path) -> Result<(), pc::SandboxError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(err)
}

#[cfg(not(unix))]
pub(crate) fn restrict_to_owner(_path: &std::path::Path) -> Result<(), pc::SandboxError> {
    Ok(())
}

/// Realizes [`LocalSandbox`] environments as directories under a base path.
pub struct LocalProvider {
    base: PathBuf,
    inherit_agent_stderr: bool,
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
}

impl LocalProvider {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            inherit_agent_stderr: false,
            blobs: HashMap::new(),
            file_store: None,
            secret_broker: Arc::new(std::sync::RwLock::new(None)),
            memory_mounter: Arc::new(std::sync::RwLock::new(None)),
        }
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

    /// Realize a sandbox and return the concrete [`LocalSandbox`], so a caller can
    /// use the tool-transparent [`LocalSandbox::spawn_agent`] capability. The trait
    /// `create` delegates here and boxes the result.
    pub async fn create_sandbox(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        // Fail closed against our capabilities (isolation, ro, egress secrets, …).
        pc::prepare_environment(spec, &Self::capabilities()).map_err(err)?;

        let mut sandbox = self.build(&spec.scope, &spec.outputs_path);
        // Best-effort egress denial for rooted Workdir tools. This is not
        // admission-gated whole-sandbox isolation (`NetworkPolicy`).
        sandbox.deny_egress = spec.deny_tool_egress;
        std::fs::create_dir_all(sandbox.root.root()).map_err(|error| {
            err(format!(
                "create sandbox root `{}`: {error}",
                sandbox.root.root().display()
            ))
        })?;
        // Outputs directory (sandbox-absolute → rejailed host path).
        let host_outputs = sandbox.root.resolve(&spec.outputs_path).map_err(err)?;
        std::fs::create_dir_all(&host_outputs).map_err(|error| {
            err(format!(
                "create sandbox outputs `{}`: {error}",
                host_outputs.display()
            ))
        })?;

        // Keep only references/literals in the Session environment. Process
        // secrets are opened afresh by `build_command`, never retained as base
        // plaintext on the sandbox object.
        sandbox.base_env.clone_from(&spec.env);
        // All-or-nothing: a failed mount reaps the whole environment (no partial dir).
        for req in &spec.mounts {
            match self.realize_mount(&sandbox.root, req).await {
                Ok((m, guard)) => {
                    // Track a realized secret's host path so it can be shredded on
                    // teardown (the credential bytes are materialized in the sandbox FS).
                    if matches!(req.source, pc::MountSource::Secret { .. })
                        && let Ok(path) = sandbox.root.resolve(&req.mount_path)
                    {
                        sandbox.secret_paths.push(path);
                    }
                    if matches!(
                        req.source,
                        pc::MountSource::Secret { .. }
                            | pc::MountSource::MemoryStore { .. }
                            | pc::MountSource::CacheVolume { .. }
                    ) && let Ok(path) = sandbox.root.resolve(&req.mount_path)
                    {
                        sandbox.continuation_excluded_paths.push(path);
                    }
                    sandbox.realized.push(m);
                    if let Some(guard) = guard {
                        sandbox.memory_mounts.lock().unwrap().push(guard);
                    }
                }
                Err(e) => {
                    // Tear down any memory mounts already realized before reaping.
                    sandbox.release_memory_mounts().await;
                    let _ = std::fs::remove_dir_all(sandbox.root.root());
                    return Err(e);
                }
            }
        }
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
        // Phase-A reader compatibility remains an ordinary-root pass-through:
        // observing future evidence must not activate physical restoration.
        // The spec-bound composition path below is the sole Phase-B adoption
        // authority for an acquired exact target.
        let root = crate::sandbox_dir(&self.base, &handle.sandbox_id);
        self.adopt_sandbox_at(handle, root)
    }

    fn adopt_sandbox_at(
        &self,
        handle: &pc::SandboxHandle,
        root: PathBuf,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        let payload = handle.local_payload()?;
        let mut sandbox = self.build_at(
            &handle.sandbox_id,
            &payload.outputs_path,
            root,
            Some(handle.clone()),
        );
        sandbox.base_env.clone_from(&payload.base_env);
        sandbox.deny_egress = payload.deny_tool_egress;
        sandbox.continuation_excluded_paths = payload
            .continuation_excluded_paths
            .iter()
            .map(|path| sandbox.root.resolve(path).map_err(err))
            .collect::<Result<_, _>>()?;
        sandbox.adopted_handle = Some(handle.clone());
        Ok(sandbox)
    }

    /// Adopt through the frozen security authority. Restored handles are never
    /// sufficient on their own because their compact locator intentionally does
    /// not duplicate the full SandboxSpec.
    pub async fn adopt_sandbox_with_spec(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<LocalSandbox, pc::SandboxError> {
        pc::prepare_environment(spec, &Self::capabilities()).map_err(err)?;
        if handle.sandbox_id != spec.scope {
            return Err(err("local Sandbox handle does not match SandboxSpec scope"));
        }
        if let Some(evidence) = handle.restoration() {
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
            {
                return Err(err(
                    "restored local handle differs from the exact SandboxSpec or checkpoint exclusions",
                ));
            }
        }
        let root = match handle.restoration() {
            Some(evidence) => {
                let root = restore_target::restoration_root(&self.base, evidence)?;
                restore_target::verify_binding(&root, evidence)?;
                root
            }
            None => crate::sandbox_dir(&self.base, &handle.sandbox_id),
        };
        self.adopt_sandbox_at(handle, root)
    }

    /// Static capability evidence shared by provider admission and owners of an
    /// already-created local environment.
    pub fn capabilities() -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Workdir,
            tool_transparent: false,
            path_fidelity: false,
            enforced_readonly: false,
            network_isolation: false,
            enforced_network_allowlist: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
            package_provisioning: false,
            control_services: Default::default(),
        }
    }

    async fn realize_mount(
        &self,
        root: &IsolatedRoot,
        req: &pc::MountRequirement,
    ) -> Result<(pc::RealizedMount, Option<Box<dyn pc::MemoryMount>>), pc::SandboxError> {
        let host = root.resolve(&req.mount_path).map_err(err)?;
        // A memory_store is a genuine keyed store (ADR-0038/0053), not a byte blob:
        // realize it through the injected mounter (FUSE where the kernel supports it,
        // else a harvested copy). Without a mounter, fail loud rather than fake it
        // with an empty file that misleads the agent into thinking it has a store.
        if let pc::MountSource::MemoryStore {
            store_id,
            materialization_reference,
            write_consistency,
        } = &req.source
        {
            let mounter = self
                .memory_mounter
                .read()
                .expect("memory mounter lock poisoned")
                .clone();
            let Some(mounter) = mounter else {
                return Err(err(format!(
                    "mount {:?}: memory_store is not realizable on this provider (no memory mounter wired)",
                    req.mount_id
                )));
            };
            let guard = mounter
                .mount(
                    materialization_reference.as_deref().unwrap_or(store_id),
                    &host,
                    req.access,
                )
                .await?;
            if *write_consistency == pc::MemoryWriteConsistency::WriteThroughRequired
                && guard.realization() != pc::Realization::Fuse
            {
                guard.teardown().await;
                return Err(err(format!(
                    "mount {:?}: memory_store requires write-through FUSE realization",
                    req.mount_id
                )));
            }
            let realized = pc::RealizedMount {
                mount_id: req.mount_id.clone(),
                mount_path: req.mount_path.clone(),
                access: req.access,
                realization: guard.realization(),
                content_hash: None,
            };
            return Ok((realized, Some(guard)));
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
            return Err(err(
                "the Workdir provider cannot write back a brokered writable Secret mount",
            ));
        }
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
                if let Some(parent) = host.parent() {
                    std::fs::create_dir_all(parent).map_err(err)?;
                }
                std::fs::write(&host, &bytes).map_err(err)?;
                // A realized secret is owner-only on disk (0600); the bytes are
                // still shredded on dispose (see `secret_paths`/`shred_secrets`).
                if matches!(req.source, pc::MountSource::Secret { .. }) {
                    restrict_to_owner(&host)?;
                }
                pc::RealizedMount {
                    mount_id: req.mount_id.clone(),
                    mount_path: req.mount_path.clone(),
                    access: req.access,
                    realization: pc::Realization::Copy,
                    content_hash: Some(content_fingerprint(&bytes)),
                }
            }
            None if req.required => {
                return Err(err(format!(
                    "required mount {:?} has no resolvable source on the local provider",
                    req.mount_id
                )));
            }
            None => {
                // Optional + unresolvable: create an empty placeholder so the path exists.
                if let Some(parent) = host.parent() {
                    std::fs::create_dir_all(parent).map_err(err)?;
                }
                std::fs::write(&host, b"").map_err(err)?;
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
        LocalSandbox {
            id: id.to_string(),
            root: IsolatedRoot::new(dir),
            outputs_path: outputs_path.to_string(),
            deny_egress: false,
            inherit_agent_stderr: self.inherit_agent_stderr,
            base_env: Vec::new(),
            secret_broker: self.secret_broker.clone(),
            realized: Vec::new(),
            secret_paths: Vec::new(),
            memory_mounts: std::sync::Mutex::new(Vec::new()),
            continuation_excluded_paths: Vec::new(),
            adopted_handle,
        }
    }
}

#[async_trait]
impl pc::SandboxProvider for LocalProvider {
    /// Workdir isolation has no external daemon or kernel feature to attest;
    /// construction already owns and validates its filesystem root.
    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    fn capabilities(&self) -> pc::SandboxCapabilities {
        Self::capabilities()
    }

    async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.create_sandbox(spec).await?))
    }

    async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.adopt_sandbox(handle).await?))
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
        self.dispose_restore_sandbox(spec, request).await
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
    base_env: Vec<pc::EnvVar>,
    /// Shared broker installation; only opaque refs are retained in `base_env`.
    secret_broker: Arc<std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>>,
    realized: Vec<pc::RealizedMount>,
    /// Host paths of realized `Secret` mounts, **shredded** (overwritten) at
    /// [`dispose`](pc::Sandbox::dispose) before the directory is reaped so a
    /// materialized credential does not linger in freed disk blocks (ADR-0023).
    secret_paths: Vec<PathBuf>,
    /// Live memory-store mounts (FUSE / copy), torn down (unmount / harvest) at
    /// [`dispose`](pc::Sandbox::dispose) before the sandbox directory is reaped.
    memory_mounts: std::sync::Mutex<Vec<Box<dyn pc::MemoryMount>>>,
    /// Host paths whose contents have an independent durable authority or carry
    /// credentials. They are rematerialized from that authority after restore.
    continuation_excluded_paths: Vec<PathBuf>,
    /// Exact reader-owned wire handle retained only across adoption. Create and
    /// ordinary creation emit the current legacy-None constructor shape; exact
    /// physical restore acquisition installs the request-bound handle.
    adopted_handle: Option<pc::SandboxHandle>,
}

impl LocalSandbox {
    /// Absolute host path used as the Workdir tier's agent workspace.
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
        materialize_read_only_tree_at(&self.root, subdir, files)
    }

    /// Tear down every live memory mount: unmount a FUSE mount, or harvest a writable
    /// copy back to its store. Drains the guard list so a later dispose is a no-op.
    pub async fn release_memory_mounts(&self) {
        let mounts: Vec<Box<dyn pc::MemoryMount>> =
            std::mem::take(&mut self.memory_mounts.lock().unwrap());
        for mount in mounts {
            mount.teardown().await;
        }
    }

    /// Remove only the runtime-owned resource projection, preserving the rest of
    /// the Session workspace. Used when a live input manifest is replaced: a
    /// detached resource must not remain reachable as stale bytes under `.mnt`.
    pub fn clear_resource_projection(&self) -> Result<(), pc::SandboxError> {
        let projection = self.root.resolve(".mnt").map_err(err)?;
        let Ok(metadata) = std::fs::symlink_metadata(&projection) else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() || metadata.is_file() {
            std::fs::remove_file(&projection).map_err(err)
        } else {
            std::fs::remove_dir_all(&projection).map_err(err)
        }
    }

    fn host_outputs(&self) -> Result<PathBuf, pc::SandboxError> {
        self.root.resolve(&self.outputs_path).map_err(err)
    }

    /// Outputs scan → `(artifact, host_path)` pairs (shared with other tiers).
    fn scan(&self) -> Result<Vec<(pc::Artifact, PathBuf)>, pc::SandboxError> {
        crate::artifacts::scan_outputs(&self.host_outputs()?, &self.outputs_path)
    }

    /// Build the child command (program, args, jailed cwd, reserved + declared env),
    /// leaving stdio for the caller to configure. Shared by `spawn`/`spawn_agent`.
    async fn build_command(&self, command: pc::Command) -> Result<TokioCommand, pc::SandboxError> {
        let broker = self
            .secret_broker
            .read()
            .expect("secret broker lock poisoned")
            .clone();
        let command =
            pc::materialize_process_command(&self.base_env, command, broker.as_ref()).await?;
        let program = command
            .argv
            .first()
            .ok_or_else(|| err("command argv is empty"))?;
        let cwd_logical = if command.cwd.is_empty() {
            "/"
        } else {
            &command.cwd
        };
        let host_cwd = self.root.resolve(cwd_logical).map_err(err)?;
        std::fs::create_dir_all(&host_cwd).map_err(err)?;
        let host_outputs = self.host_outputs()?;
        std::fs::create_dir_all(&host_outputs).map_err(err)?;

        let mut cmd = TokioCommand::new(program);
        configure_process_group(&mut cmd);
        cmd.args(&command.argv[1..]);
        cmd.current_dir(&host_cwd);
        cmd.env_clear();
        // Reserved, runtime-owned env: where the agent writes/works (the local tier
        // has no path fidelity, so a process finds the outputs dir via this var).
        crate::RuntimePathEnv::new(
            host_cwd.to_string_lossy().into_owned(),
            host_outputs.to_string_lossy().into_owned(),
        )
        .apply(&mut cmd);
        for var in &command.env {
            cmd.env(&var.name, var.value.expose());
        }
        Ok(cmd)
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
        let mut cmd = self.build_command(command).await?;
        let stderr = if self.inherit_agent_stderr {
            ProcStdio::inherit()
        } else {
            ProcStdio::null()
        };
        cmd.stdin(ProcStdio::piped())
            .stdout(ProcStdio::piped())
            .stderr(stderr);
        let mut child = cmd.spawn().map_err(err)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| err("agent stdin was not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| err("agent stdout was not piped"))?;
        let channel: Box<dyn AgentChannel> = Box::new(SplitChannel::new(stdout, stdin));
        Ok((Box::new(LocalProcess::spawned(child)), channel))
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
    pub fn list_files(&self, subdir: &str) -> Vec<(String, Vec<u8>)> {
        list_files_at(&self.root, subdir)
    }

    /// Scan `<root>/<subdir>/*/SKILL.md` live, returning neutral file data (the host
    /// parses the skill model). Re-scanned each call so a run-authored skill is seen.
    pub fn scan_skill_dir(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        scan_skill_dir_at(&self.root, subdir)
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
        let path = jailed_at(&self.root, logical).map_err(err)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        std::fs::write(&path, contents).map_err(err)?;
        restrict_to_owner(&path)?;
        Ok(())
    }

    /// Remove one dynamically projected workspace path without tearing down the
    /// Session environment. The same lexical jail used by materialization rejects
    /// escape attempts; missing paths are an idempotent success.
    pub fn remove_inline(&self, logical: &str) -> Result<(), pc::SandboxError> {
        let path = self.root.resolve(logical).map_err(err)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path).map_err(err),
            Ok(_) => std::fs::remove_file(path).map_err(err),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(err(error)),
        }
    }
}

#[async_trait]
impl pc::RepositoryRealizer for LocalSandbox {
    async fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        provision_repo_at(
            &self.root,
            &plan.mount_path,
            &plan.transport_url,
            plan.initial_branch.as_deref(),
            plan.initial_commit.as_deref(),
            credential,
        )
        .map_err(err)
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        push_repo_to_at(&self.root, &plan.mount_path, plan, expectation, credential)
    }
}

#[async_trait]
impl pc::Sandbox for LocalSandbox {
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
            .map(|path| format!("/{}", path.to_string_lossy().trim_start_matches('/')))
            .collect();
        pc::SandboxHandle::local(
            &self.id,
            pc::LocalSandboxHandleV1 {
                outputs_path: self.outputs_path.clone(),
                base_env: self.base_env.clone(),
                continuation_excluded_paths,
                deny_tool_egress: self.deny_egress,
            },
        )
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let stdio = command.stdio;
        let mut cmd = self.build_command(command).await?;
        let (out, e) = match stdio {
            pc::Stdio::Inherit => (ProcStdio::inherit(), ProcStdio::inherit()),
            pc::Stdio::Piped => (ProcStdio::piped(), ProcStdio::piped()),
            pc::Stdio::Null => (ProcStdio::null(), ProcStdio::null()),
        };
        cmd.stdout(out).stderr(e);

        let child = cmd.spawn().map_err(err)?;
        Ok(Box::new(LocalProcess::spawned(child)))
    }

    async fn attach(
        &self,
        _req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        // Runtime attach after create realizes to disk but is not reflected in
        // `realized()` (which reports the create-time set); the caller owns the
        // returned ref. Provider re-borrows `&self`, so we resolve without state.
        Err(err(
            "runtime attach is realized by the provider that owns the blob store (Slice 4)",
        ))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        Ok(self.scan()?.into_iter().map(|(a, _)| a).collect())
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        for (artifact, host) in self.scan()? {
            if artifact.id == id {
                return std::fs::read(&host).map_err(err);
            }
        }
        Err(err(format!("no artifact with id {id:?}")))
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
        Err(err(
            "local provider cannot reattach to a process across owners",
        ))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        Ok(if self.root.root().exists() {
            pc::SandboxStatus::Ready
        } else {
            pc::SandboxStatus::Terminated
        })
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        Ok(()) // no lease: a local sandbox dies with its owner.
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        // Order: harvest memory (reads edits back, unmounts FUSE) → shred secrets
        // (overwrite credential bytes) → reap the directory. Memory harvest must run
        // before shred so a promised write-back is never lost; both run before reap.
        self.release_memory_mounts().await;
        self.shred_secrets();
        let root = self.root.root();
        if let Some(evidence) = self
            .adopted_handle
            .as_ref()
            .and_then(pc::SandboxHandle::restoration)
        {
            restore_target::dispose_bound_target(root, evidence).await?;
        } else if root.exists() {
            std::fs::remove_dir_all(root).map_err(err)?;
        }
        Ok(())
    }
}

impl LocalSandbox {
    /// Overwrite each realized secret file's bytes with zeros before the directory is
    /// reaped, so a materialized credential does not survive in freed disk blocks
    /// (ADR-0023 shred-on-teardown). Best-effort: a missing/short file is skipped.
    fn shred_secrets(&self) {
        for path in &self.secret_paths {
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = std::fs::write(path, vec![0u8; meta.len() as usize]);
            }
        }
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
mod workdir_helper_tests;
