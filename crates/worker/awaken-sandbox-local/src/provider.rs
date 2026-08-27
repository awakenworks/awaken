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

mod checkpoint;

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
        let payload = handle.local_payload()?;
        let mut sandbox = self.build(&handle.sandbox_id, &payload.outputs_path);
        sandbox.base_env.clone_from(&payload.base_env);
        sandbox.continuation_excluded_paths = payload
            .continuation_excluded_paths
            .iter()
            .map(|path| sandbox.root.resolve(path).map_err(err))
            .collect::<Result<_, _>>()?;
        Ok(sandbox)
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
        let dir = crate::sandbox_dir(&self.base, id);
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

    fn checkpoint_formats(&self) -> Vec<String> {
        vec!["awaken-fs-tar-v1".into()]
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

    async fn restore(
        &self,
        spec: &pc::SandboxSpec,
        checkpoint: &pc::SandboxCheckpointRef,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(
            self.restore_sandbox(spec, checkpoint, store).await?,
        ))
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
    ) -> Result<pc::RepositoryPublicationReceipt, pc::SandboxError> {
        push_repo_to_at(&self.root, &plan.mount_path, plan, expectation, credential).map_err(err)
    }
}

#[async_trait]
impl pc::Sandbox for LocalSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        let continuation_excluded_paths = self
            .continuation_excluded_paths
            .iter()
            .filter_map(|path| path.strip_prefix(self.root.root()).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        pc::SandboxHandle::local(
            &self.id,
            pc::LocalSandboxHandleV1 {
                outputs_path: self.outputs_path.clone(),
                base_env: self.base_env.clone(),
                continuation_excluded_paths,
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

    async fn checkpoint(
        &self,
        request: &pc::SandboxCheckpointRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError> {
        self.create_checkpoint(request, store).await
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
        if root.exists() {
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
mod shred_tests {
    use super::*;
    use awaken_provisioning_contract::{
        IsolationClass, MountAccess, MountLifetime, MountRequirement, MountSource, NetworkPolicy,
        ResourceLimits, Sandbox, SandboxSpec,
    };

    struct Broker;

    #[async_trait]
    impl pc::SecretBroker for Broker {
        async fn materialize(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            assert_eq!(reference, "broker://k");
            Ok(b"broker-secret".to_vec())
        }

        async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Err(pc::SandboxError::new("process secrets are not supported"))
        }

        async fn write_back(
            &self,
            _reference: &str,
            _bytes: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            unreachable!("the Workdir provider accepts only read-only brokered secrets")
        }
    }

    fn secret_spec(scope: &str) -> SandboxSpec {
        SandboxSpec {
            scope: scope.into(),
            isolation: IsolationClass::Workdir,
            mounts: vec![MountRequirement {
                mount_id: "auth".into(),
                source: MountSource::Secret {
                    reference: "broker://k".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/.auth".into(),
                access: MountAccess::ReadWrite,
                lifetime: MountLifetime::PerRun,
                required: true,
            }],
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: ResourceLimits::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        }
    }

    #[tokio::test]
    async fn dispose_shreds_a_materialized_secret_before_reaping() {
        let tmp = tempfile::tempdir().unwrap();
        let provider =
            LocalProvider::new(tmp.path()).with_blob("broker://k", b"sk-secret".to_vec());
        let sandbox = provider
            .create_sandbox(&secret_spec("t-shred"))
            .await
            .unwrap();

        // The credential is materialized in the sandbox FS and tracked for shredding.
        assert_eq!(sandbox.secret_paths.len(), 1);
        let path = sandbox.secret_paths[0].clone();
        assert_eq!(std::fs::read(&path).unwrap(), b"sk-secret");

        // Shred overwrites the bytes with zeros (runs before the directory is reaped).
        sandbox.shred_secrets();
        assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; 9]);

        // A non-secret mount is NOT tracked (only credentials are shredded).
        sandbox.dispose().await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_read_only_secret_uses_the_dedicated_broker() {
        let source = MountSource::Secret {
            reference: "broker://k".into(),
            content_hash: None,
        };
        let broker: Arc<dyn pc::SecretBroker> = Arc::new(Broker);
        let bytes = resolve_source(&source, &HashMap::new(), &None, Some(&broker))
            .await
            .unwrap();

        assert_eq!(bytes.unwrap(), b"broker-secret");
    }

    #[tokio::test]
    async fn a_brokered_writable_secret_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path()).with_secret_broker(Arc::new(Broker));

        let error = match provider
            .create_sandbox(&secret_spec("t-broker-write"))
            .await
        {
            Ok(_) => panic!("brokered writable Secret must fail closed"),
            Err(error) => error,
        };
        assert!(error.0.contains("cannot write back"));
    }

    #[tokio::test]
    async fn clearing_resource_projection_revokes_stale_mounts_but_keeps_workspace_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut spec = secret_spec("clear-projection");
        spec.mounts.clear();
        let sandbox = LocalProvider::new(tmp.path())
            .create_sandbox(&spec)
            .await
            .unwrap();
        let root = sandbox.root.root();
        std::fs::create_dir_all(root.join(".mnt/private")).unwrap();
        std::fs::write(root.join(".mnt/private/secret.txt"), "secret").unwrap();
        std::fs::write(root.join("work.txt"), "keep").unwrap();

        sandbox.clear_resource_projection().unwrap();

        assert!(!root.join(".mnt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("work.txt")).unwrap(),
            "keep"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_materialized_secret_is_owner_only_on_disk() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let provider =
            LocalProvider::new(tmp.path()).with_blob("broker://k", b"sk-secret".to_vec());
        let sandbox = provider
            .create_sandbox(&secret_spec("t-perms"))
            .await
            .unwrap();

        let path = sandbox.secret_paths[0].clone();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a realized secret must be owner-only, got {mode:o}"
        );
    }

    #[tokio::test]
    async fn a_non_secret_mount_is_not_tracked_for_shredding() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path()).with_blob("file-x", b"data".to_vec());
        let mut spec = secret_spec("t-nosecret");
        spec.mounts[0].source = MountSource::File {
            file_id: "file-x".into(),
            content_hash: None,
        };
        let sandbox = provider.create_sandbox(&spec).await.unwrap();
        assert!(
            sandbox.secret_paths.is_empty(),
            "only Secret mounts are shredded"
        );
    }

    #[tokio::test]
    async fn resolve_source_handles_every_mount_variant() {
        let mut blobs = HashMap::new();
        blobs.insert("f1".to_string(), b"file".to_vec());
        blobs.insert("r1".to_string(), b"res".to_vec());
        let none_store: Option<Arc<dyn pc::BlobSource>> = None;

        let file = MountSource::File {
            file_id: "f1".into(),
            content_hash: None,
        };
        assert_eq!(
            resolve_source(&file, &blobs, &none_store, None)
                .await
                .unwrap(),
            Some(b"file".to_vec())
        );
        let resource = MountSource::Resource {
            resource_id: "r1".into(),
            content_hash: None,
        };
        assert_eq!(
            resolve_source(&resource, &blobs, &none_store, None)
                .await
                .unwrap(),
            Some(b"res".to_vec())
        );
        // Typed inline content short-circuits before any store hit.
        let inline = MountSource::Inline {
            contents: "inline".into(),
        };
        assert_eq!(
            resolve_source(&inline, &blobs, &none_store, None)
                .await
                .unwrap(),
            Some(b"inline".to_vec())
        );
        let binary = vec![0, 0xff, 0x80, b'\n'];
        let carried = MountSource::InlineBytes {
            contents: binary.clone(),
            content_hash: Some(content_fingerprint(&binary)),
        };
        assert_eq!(
            resolve_source(&carried, &blobs, &none_store, None)
                .await
                .unwrap(),
            Some(binary.clone()),
            "binary input must round-trip without UTF-8 coercion"
        );
        assert!(verify(&carried, &binary).is_ok());
        assert!(verify(&carried, b"corrupt").is_err());
        // A memory store is not byte-resolvable through this path.
        let mem = MountSource::MemoryStore {
            store_id: "m".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        };
        assert_eq!(
            resolve_source(&mem, &blobs, &none_store, None)
                .await
                .unwrap(),
            None
        );
        // An unknown id resolves to nothing.
        let missing = MountSource::File {
            file_id: "nope".into(),
            content_hash: None,
        };
        assert_eq!(
            resolve_source(&missing, &blobs, &none_store, None)
                .await
                .unwrap(),
            None
        );
    }

    #[test]
    fn declared_hash_and_verify_are_fail_closed_per_variant() {
        assert_eq!(
            declared_hash(&MountSource::Resource {
                resource_id: "r".into(),
                content_hash: Some("h".into()),
            }),
            Some("h")
        );
        // Non-hashable variants have no declared hash.
        assert_eq!(
            declared_hash(&MountSource::MemoryStore {
                store_id: "m".into(),
                materialization_reference: None,
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            }),
            None
        );
        assert_eq!(
            declared_hash(&MountSource::InlineBytes {
                contents: Vec::new(),
                content_hash: Some("carried-hash".into()),
            }),
            Some("carried-hash")
        );

        let src = MountSource::File {
            file_id: "f".into(),
            content_hash: Some(content_fingerprint(b"right")),
        };
        assert!(verify(&src, b"right").is_ok());
        assert!(verify(&src, b"wrong").is_err());
        // No declared hash → nothing to verify against.
        assert!(
            verify(
                &MountSource::Inline {
                    contents: String::new()
                },
                b"any"
            )
            .is_ok()
        );
    }

    fn bare_spec(scope: &str) -> SandboxSpec {
        let mut s = secret_spec(scope);
        s.mounts = Vec::new();
        s
    }

    #[tokio::test]
    async fn a_nested_resolvable_mount_and_an_optional_unresolvable_one_both_realize() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path()).with_blob("data-x", b"payload".to_vec());
        let mut spec = bare_spec("t-mounts");
        spec.mounts = vec![
            // Resolvable at a nested path → parent dirs are created (Some branch).
            MountRequirement {
                mount_id: "data".into(),
                source: MountSource::File {
                    file_id: "data-x".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/deep/data.bin".into(),
                access: MountAccess::ReadWrite,
                lifetime: MountLifetime::PerRun,
                required: true,
            },
            // Optional + unresolvable → an empty placeholder (None branch).
            MountRequirement {
                mount_id: "opt".into(),
                source: MountSource::File {
                    file_id: "absent".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/deep2/opt.bin".into(),
                access: MountAccess::ReadWrite,
                lifetime: MountLifetime::PerRun,
                required: false,
            },
        ];
        let sandbox = provider.create_sandbox(&spec).await.unwrap();
        let realized = sandbox.realized();
        assert_eq!(realized.len(), 2);
        let opt = realized.iter().find(|m| m.mount_id == "opt").unwrap();
        assert_eq!(opt.content_hash, None, "the placeholder carries no hash");
        let data = realized.iter().find(|m| m.mount_id == "data").unwrap();
        assert!(data.content_hash.is_some());
    }

    #[tokio::test]
    async fn build_command_honors_cwd_and_inline_env_and_rejects_empty_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let sandbox = provider.create_sandbox(&bare_spec("t-cmd")).await.unwrap();
        assert_eq!(
            sandbox.workspace_path(),
            crate::sandbox_dir(tmp.path(), "t-cmd")
        );

        let mut cmd = pc::Command::new(["/bin/true"]);
        cmd.cwd = "/work".into();
        cmd.env = vec![pc::EnvVar {
            name: "K".into(),
            value: pc::EnvValue::Inline { value: "V".into() },
            visibility: pc::EnvVisibility::Process,
        }];
        assert!(sandbox.build_command(cmd).await.is_ok());

        let empty = pc::Command::new(Vec::<String>::new());
        assert!(sandbox.build_command(empty).await.is_err());
    }

    #[tokio::test]
    async fn a_spawned_local_process_reports_its_pid_and_reaps() {
        use awaken_provisioning_contract::ProcessHandle;
        let mut command = successful_command();
        let child = command.spawn().unwrap();
        let proc = LocalProcess::spawned(child);
        assert!(!proc.id().is_empty());
        let status = proc.wait().await.unwrap();
        assert_eq!(status.code, Some(0));
    }

    #[cfg(windows)]
    fn successful_command() -> TokioCommand {
        let mut command = TokioCommand::new("cmd.exe");
        command.args(["/D", "/C", "exit /b 0"]);
        command
    }

    #[cfg(not(windows))]
    fn successful_command() -> TokioCommand {
        TokioCommand::new("true")
    }
}

/// The Workdir-tier host helpers that replace the legacy `Environment` methods —
/// rooted tools, repo provisioning/write-back, artifact/skill scanning — exercised
/// on a `LocalSandbox` built through the pc `SandboxProvider` path.
#[cfg(test)]
mod workdir_helper_tests {
    use super::*;
    use crate::git_transport::git_stdout;
    use awaken_provisioning_contract::{
        IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxProvider, SandboxSpec,
    };

    fn workdir_spec(scope: &str, deny_egress: bool) -> SandboxSpec {
        SandboxSpec {
            scope: scope.into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            // The Workdir tier admits only unrestricted network (it cannot enforce
            // isolation); egress denial for the rooted bash tool rides `extra`.
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            requests: Default::default(),
            limits: ResourceLimits::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: deny_egress,
        }
    }

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[tokio::test]
    async fn rooted_tools_are_nonempty_and_egress_tracks_the_network_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());

        let open = provider
            .create_sandbox(&workdir_spec("t-open", false))
            .await
            .unwrap();
        assert!(!open.deny_egress);
        // The full built-in capability surface is composed as RawTools.
        assert!(!open.rooted_tools().is_empty());

        let closed = provider
            .create_sandbox(&workdir_spec("t-closed", true))
            .await
            .unwrap();
        assert!(closed.deny_egress);
        // Egress denial changes the tool wrapper, not the tool set's size.
        assert_eq!(open.rooted_tools().len(), closed.rooted_tools().len());
    }

    #[tokio::test]
    async fn list_files_and_scan_skill_dir_read_the_realized_root() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let sandbox = provider
            .create_sandbox(&workdir_spec("t-scan", false))
            .await
            .unwrap();
        let root = sandbox.root.root().to_path_buf();

        // Output artifacts are collected as (logical_path, bytes), sorted, recursively.
        std::fs::create_dir_all(root.join("outputs/sub")).unwrap();
        std::fs::write(root.join("outputs/a.txt"), b"A").unwrap();
        std::fs::write(root.join("outputs/sub/b.txt"), b"B").unwrap();
        assert_eq!(
            sandbox.list_files("outputs"),
            vec![
                ("a.txt".to_string(), b"A".to_vec()),
                ("sub/b.txt".to_string(), b"B".to_vec()),
            ]
        );

        // Managed Skill discovery decision table. C1 file is under the exact
        // `.claude/skills` root; C2 it has exactly one Skill directory level;
        // C3 its filename is `SKILL.md`. D1 C1+C2+C3 => discover one logical
        // Skill. D2 wrong root, D3 root-level file, D4 extra nesting, and D5
        // missing canonical filename => ignore. This owns Anthropic's repository
        // discovery shape while the host remains the Skill semantic owner.
        std::fs::create_dir_all(root.join(".claude/skills/greet")).unwrap();
        std::fs::write(root.join(".claude/skills/greet/SKILL.md"), "# greet").unwrap();
        std::fs::write(root.join(".claude/skills/SKILL.md"), "# root").unwrap();
        std::fs::create_dir_all(root.join(".claude/skills/nested/too-deep")).unwrap();
        std::fs::write(
            root.join(".claude/skills/nested/too-deep/SKILL.md"),
            "# nested",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".claude/skills/missing")).unwrap();
        std::fs::write(root.join(".claude/skills/missing/skill.md"), "# wrong name").unwrap();
        std::fs::create_dir_all(root.join("skills/outside")).unwrap();
        std::fs::write(root.join("skills/outside/SKILL.md"), "# outside").unwrap();

        let skills = sandbox.scan_skill_dir(".claude/skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "greet");
        assert_eq!(skills[0].dir, ".claude/skills/greet");
        assert_eq!(skills[0].content, "# greet");
    }

    /// Repository realization cause/effect decision table:
    /// | Rule | Destination / Agent Git config | Frozen plan | Effect |
    /// |---|---|---|---|
    /// | R1 | absent | valid | clone the exact repository |
    /// | R2 | already realized | exact replay | succeed without replacing Agent state |
    /// | R3 | occupied | different remote/checkout | reject without modifying the tree |
    /// | R4 | origin and `url.*.insteadOf` target attacker | exact authored remote plus branch/commit | push only the absent exact ref; attacker unchanged |
    /// | R5 | R4 after successful push | same plan/coordinate | return the identical canonical receipt |
    #[tokio::test]
    async fn provision_clones_then_the_host_pushes_the_agents_own_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        // Seed a bare "remote" with one commit.
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q"]);
        git(&seed, &["checkout", "-q", "-b", "main"]);
        git(&seed, &["config", "user.email", "seed@t"]);
        git(&seed, &["config", "user.name", "seed"]);
        std::fs::write(seed.join("README.md"), "hello").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", "seed"]);
        let bare = base.join("remote.git");
        git(
            base,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let attacker = base.join("attacker.git");
        git(
            base,
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                attacker.to_str().unwrap(),
            ],
        );
        let seed_head = std::process::Command::new("git")
            .current_dir(&seed)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let seed_head = String::from_utf8(seed_head.stdout)
            .unwrap()
            .trim()
            .to_owned();

        let provider = LocalProvider::new(base.join("envs"));
        let sandbox = provider
            .create_sandbox(&workdir_spec("t-repo", false))
            .await
            .unwrap();
        let root = sandbox.root.root().to_path_buf();
        let plan = pc::RepositoryRealizationPlan {
            repository_id: "repo-1".into(),
            mount_path: "workspace/repo".into(),
            source_remote_url: bare.to_string_lossy().into_owned(),
            transport_url: bare.to_string_lossy().into_owned(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        };

        pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
            .await
            .unwrap();
        let repo_dir = root.join("workspace/repo");
        assert_eq!(
            std::fs::read_to_string(repo_dir.join("README.md")).unwrap(),
            "hello"
        );
        std::fs::write(repo_dir.join("PRESERVED"), "agent state").unwrap();
        pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
            .await
            .expect("R2 exact replay is idempotent");
        assert_eq!(
            std::fs::read_to_string(repo_dir.join("PRESERVED")).unwrap(),
            "agent state",
            "R2 preserves the existing working tree"
        );
        let conflicting = pc::RepositoryRealizationPlan {
            source_remote_url: base.join("different.git").to_string_lossy().into_owned(),
            transport_url: base.join("different.git").to_string_lossy().into_owned(),
            ..plan.clone()
        };
        assert!(
            pc::RepositoryRealizer::realize_repository(&sandbox, &conflicting, None)
                .await
                .is_err(),
            "R3 conflicting realization fails closed"
        );
        assert_eq!(
            std::fs::read_to_string(repo_dir.join("README.md")).unwrap(),
            "hello",
            "R3 does not replace the authoritative tree"
        );
        // Provision sets NO committer identity — that is the agent's to own.
        let cfg = std::process::Command::new("git")
            .current_dir(&repo_dir)
            .args(["config", "--local", "user.name"])
            .output()
            .unwrap();
        assert!(
            cfg.stdout.is_empty(),
            "provision must not set a committer identity"
        );

        // Nothing authored yet: an explicit exact seed coordinate observes the
        // already-current remote and returns its canonical receipt.
        let initial_branch = git_stdout(Some(&repo_dir), &["symbolic-ref", "--short", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let seed_expectation = pc::RepositoryPublicationExpectation {
            branch: initial_branch,
            commit: seed_head.clone(),
        };
        let seed_receipt =
            pc::RepositoryRealizer::publish_repository(&sandbox, &plan, &seed_expectation, None)
                .await
                .unwrap();
        seed_receipt.verify(&plan, &seed_expectation).unwrap();

        // The AGENT configures its own identity and authors a commit in the jail — a clean
        // working tree afterwards (it committed everything), which the OLD harvest would have
        // wrongly skipped. The host then only pushes.
        git(&repo_dir, &["config", "user.email", "hermes@agent.local"]);
        git(&repo_dir, &["config", "user.name", "Hermes"]);
        git(&repo_dir, &["checkout", "-b", "awf/work"]);
        std::fs::write(repo_dir.join("NEW.txt"), "agent").unwrap();
        git(&repo_dir, &["add", "-A"]);
        git(&repo_dir, &["commit", "-q", "-m", "agent: add NEW.txt"]);
        let agent_commit = git_stdout(Some(&repo_dir), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let expectation = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: agent_commit,
        };

        // The Agent owns this config and may point both the named remote and an
        // `insteadOf` rewrite at an attacker. Publication must ignore both and
        // consume only the immutable plan URL passed by the host.
        let attacker_url = attacker.to_string_lossy().into_owned();
        let frozen_url = plan.transport_url.clone();
        git(&repo_dir, &["remote", "set-url", "origin", &attacker_url]);
        let rewrite_key = format!("url.{attacker_url}.insteadOf");
        git(&repo_dir, &["config", &rewrite_key, &frozen_url]);

        // R4 pushes the frozen target even though both Agent-authored mechanisms
        // select the attacker. R5 compares the exact target and becomes a no-op.
        let first = pc::RepositoryRealizer::publish_repository(&sandbox, &plan, &expectation, None)
            .await
            .expect("R4");
        let replay =
            pc::RepositoryRealizer::publish_repository(&sandbox, &plan, &expectation, None)
                .await
                .expect("R5");
        assert_eq!(first, replay, "R4/R5");

        let attacker_head = std::process::Command::new("git")
            .args([
                "--git-dir",
                attacker.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(attacker_head.stdout).unwrap().trim(),
            seed_head,
            "R4 attacker ref must remain unchanged"
        );

        // The bare remote carries the AGENT's commit — its own message and author, not a
        // canned harvest commit by a fake user.
        let log = std::process::Command::new("git")
            .current_dir(&bare)
            .args(["log", "-1", "refs/heads/awf/work", "--pretty=%an|%ae|%s"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("agent: add NEW.txt"), "agent's message: {log}");
        assert!(log.contains("Hermes"), "agent's author name: {log}");
        assert!(
            log.contains("hermes@agent.local"),
            "agent's author email: {log}"
        );
    }

    #[tokio::test]
    async fn repository_realizer_checks_out_the_exact_commit_pin() {
        // Cause graph: commit checkout in frozen config -> realization plan
        // -> clone tokenless origin -> detached checkout -> exact tree/HEAD.
        //
        // Decision table:
        // | Rule | Branch | Commit | Expected behavior |
        // | G1 | none | valid reachable SHA | detached exact SHA/tree |
        // | G2 | none | invalid SHA | fail realization, no fallback to HEAD |
        let tmp = tempfile::tempdir().unwrap();
        let seed = tmp.path().join("seed-commit");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q"]);
        git(&seed, &["config", "user.email", "seed@t"]);
        git(&seed, &["config", "user.name", "seed"]);
        std::fs::write(seed.join("VERSION"), "one").unwrap();
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", "one"]);
        let first = std::process::Command::new("git")
            .current_dir(&seed)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let first = String::from_utf8(first.stdout).unwrap().trim().to_string();
        std::fs::write(seed.join("VERSION"), "two").unwrap();
        git(&seed, &["commit", "-q", "-am", "two"]);

        let provider = LocalProvider::new(tmp.path().join("envs"));
        let sandbox = provider
            .create_sandbox(&workdir_spec("exact-commit", false))
            .await
            .unwrap();
        let plan = pc::RepositoryRealizationPlan {
            repository_id: "repo-commit".into(),
            mount_path: "workspace/repo".into(),
            source_remote_url: seed.to_string_lossy().into_owned(),
            transport_url: seed.to_string_lossy().into_owned(),
            initial_branch: None,
            initial_commit: Some(first.clone()),
            access: pc::MountAccess::ReadOnly,
        };
        pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
            .await
            .unwrap();
        let realized = sandbox.root.root().join("workspace/repo");
        assert_eq!(
            std::fs::read_to_string(realized.join("VERSION")).unwrap(),
            "one"
        );
        let head = std::process::Command::new("git")
            .current_dir(&realized)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(head.stdout).unwrap().trim(), first);

        let invalid = pc::RepositoryRealizationPlan {
            mount_path: "workspace/invalid".into(),
            initial_commit: Some("0000000000000000000000000000000000000000".into()),
            ..plan
        };
        assert!(
            pc::RepositoryRealizer::realize_repository(&sandbox, &invalid, None)
                .await
                .is_err(),
            "G2 invalid commit fails instead of using the remote default HEAD"
        );
    }

    #[tokio::test]
    async fn repository_realizer_rejects_a_jail_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let sandbox = provider
            .create_sandbox(&workdir_spec("t-escape", false))
            .await
            .unwrap();
        let plan = pc::RepositoryRealizationPlan {
            repository_id: "repo-escape".into(),
            mount_path: "../escape".into(),
            source_remote_url: "http://x".into(),
            transport_url: "http://x".into(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadOnly,
        };
        assert!(
            pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dynamic_inline_projection_handles_file_directory_and_missing_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = LocalProvider::new(tmp.path())
            .create_sandbox(&workdir_spec("inline-lifecycle", false))
            .await
            .unwrap();

        sandbox
            .materialize_inline("nested/value", b"value")
            .unwrap();
        sandbox.remove_inline("nested/value").unwrap();
        sandbox
            .materialize_inline("nested/value", b"value")
            .unwrap();
        sandbox.remove_inline("nested").unwrap();
        sandbox.remove_inline("nested").unwrap();
    }

    #[derive(Default)]
    struct CheckpointStore {
        objects: std::sync::Mutex<HashMap<String, Vec<u8>>>,
    }

    #[async_trait]
    impl pc::SandboxCheckpointStore for CheckpointStore {
        async fn put(
            &self,
            metadata: &pc::CheckpointObjectMetadata,
            bytes: Vec<u8>,
        ) -> Result<pc::StoredCheckpointObject, pc::SandboxError> {
            let id = format!("{}/{}", metadata.generation_id, metadata.suspend_effect_id);
            let digest = content_fingerprint(&bytes);
            let size_bytes = bytes.len() as u64;
            self.objects.lock().unwrap().insert(id.clone(), bytes);
            Ok(pc::StoredCheckpointObject {
                id,
                digest,
                size_bytes,
            })
        }

        async fn get(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
            self.objects
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| err("checkpoint object not found"))
        }

        async fn delete(&self, id: &str) -> Result<(), pc::SandboxError> {
            self.objects.lock().unwrap().remove(id);
            Ok(())
        }
    }

    fn checkpoint_request() -> pc::SandboxCheckpointRequest {
        let generation = awaken_session_contract::SandboxGeneration::new(
            "checkpoint-session",
            10,
            10_000,
            "environment",
            "base-image",
        );
        pc::SandboxCheckpointRequest {
            workspace_id: "workspace-a".into(),
            session_id: "checkpoint-session".into(),
            generation_id: generation.id,
            environment_fingerprint: generation.environment_fingerprint,
            base_image_fingerprint: generation.base_image_fingerprint,
            effect_id: "suspend".into(),
            format: "awaken-fs-tar-v1".into(),
            created_at_unix_ms: 20,
            expires_at_unix_ms: 10_000,
            max_bytes: 1024 * 1024,
        }
    }

    // Cause/effect design: C1=mutable nested file+mode, C5=durable store write,
    // C6=source disposed, C7=valid object; provider-conformance rule P1 => a
    // distinct live Sandbox contains identical bytes and executable metadata.
    #[tokio::test]
    async fn checkpoint_dispose_restore_preserves_mutable_filesystem() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let spec = workdir_spec("checkpoint-session", false);
        let sandbox = provider.create_sandbox(&spec).await.unwrap();
        let file = sandbox.workspace_path().join("workspace/bin/tool");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"mutable state").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o750)).unwrap();
        let store = CheckpointStore::default();
        let receipt = sandbox
            .checkpoint(&checkpoint_request(), &store)
            .await
            .unwrap();
        sandbox.dispose().await.unwrap();
        assert!(!sandbox.workspace_path().exists(), "source terminated");

        let restored = provider.restore(&spec, &receipt, &store).await.unwrap();
        let restored_root = crate::sandbox_dir(tmp.path(), "checkpoint-session");
        let restored_file = restored_root.join("workspace/bin/tool");
        assert_eq!(std::fs::read(&restored_file).unwrap(), b"mutable state");
        assert_eq!(
            std::fs::metadata(restored_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o750
        );
        restored.dispose().await.unwrap();
    }

    // Cause/effect design: C7=object bytes differ from the committed digest.
    // FMECA corruption rule => fail closed, create no usable restored Sandbox,
    // and preserve the durable checkpoint reference for operator recovery.
    #[tokio::test]
    async fn corrupt_checkpoint_is_rejected_before_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = LocalProvider::new(tmp.path());
        let spec = workdir_spec("checkpoint-session", false);
        let sandbox = provider.create_sandbox(&spec).await.unwrap();
        std::fs::write(sandbox.workspace_path().join("value"), b"original").unwrap();
        let store = CheckpointStore::default();
        let receipt = sandbox
            .checkpoint(&checkpoint_request(), &store)
            .await
            .unwrap();
        sandbox.dispose().await.unwrap();
        store
            .objects
            .lock()
            .unwrap()
            .insert(receipt.id.clone(), b"corrupt".to_vec());
        assert!(provider.restore(&spec, &receipt, &store).await.is_err());
    }
}
