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
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio as ProcStdio;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_provisioning_contract as pc;
use serde_json::json;
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::Mutex as AsyncMutex;

use std::sync::Arc;

use awaken_runtime_contract::tool::RawTool;

use crate::{
    DiscoveredSkillFile, IsolatedRoot, content_fingerprint, list_files_at, provision_repo_at,
    push_repo_at, rooted_raw_tools, scan_skill_dir_at,
};

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

static READ_ONLY_TREE_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Resolve a mount's bytes: an in-memory seed map first, then an optional
/// content-addressed [`BlobSource`](pc::BlobSource), then inline `Other({content})`.
pub(crate) async fn resolve_source(
    source: &pc::MountSource,
    blobs: &HashMap<String, Vec<u8>>,
    store: &Option<Arc<dyn pc::BlobSource>>,
) -> Option<Vec<u8>> {
    // Inline `Other({content})` and unresolvable memory stores short-circuit before
    // any store hit; the rest resolve by id (seed map first, then the store).
    let id = match source {
        pc::MountSource::File { file_id, .. } => file_id.as_str(),
        pc::MountSource::Resource { resource_id, .. } => resource_id.as_str(),
        // The broker is faked locally by the seed map / store keyed on the reference.
        pc::MountSource::Secret { reference, .. } => reference.as_str(),
        // Inline ephemeral content ships in the spec — no store hit, no id.
        pc::MountSource::Inline { contents } => return Some(contents.clone().into_bytes()),
        pc::MountSource::InlineBytes { contents, .. } => return Some(contents.clone()),
        pc::MountSource::Other(v) => {
            return v
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.as_bytes().to_vec());
        }
        pc::MountSource::MemoryStore { .. } => return None,
        // A Cache Volume has no seedable content — it is mounted in place from its
        // host path and its bytes have no authority (ADR-0056), so there is nothing to
        // fingerprint or seed here.
        pc::MountSource::CacheVolume { .. } => return None,
    };
    if let Some(bytes) = blobs.get(id) {
        return Some(bytes.clone());
    }
    if let Some(store) = store
        && let Some(bytes) = store.get(id).await
    {
        return Some(bytes);
    }
    None
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
/// secret still never enters our process as a value: it is resolved by reference
/// through the injected [`BlobSource`](pc::BlobSource) straight to disk.
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
    /// In-memory blob seed for `File`/`Resource` mounts (keyed by id).
    blobs: HashMap<String, Vec<u8>>,
    /// Optional content-addressed store consulted after the seed map (Slice 4).
    file_store: Option<Arc<dyn pc::BlobSource>>,
    /// Optional memory-store realizer (FUSE / copy). Absent → a `MemoryStore` mount
    /// fails loud rather than being faked as an empty file (ADR-0053 item 1).
    memory_mounter: Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
}

impl LocalProvider {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            blobs: HashMap::new(),
            file_store: None,
            memory_mounter: Arc::new(std::sync::RwLock::new(None)),
        }
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
        pc::prepare_environment(spec, &Self::caps()).map_err(err)?;

        let mut sandbox = self.build(&spec.scope, &spec.outputs_path);
        // Best-effort egress denial for the rooted `bash` tool (the Workdir-tier
        // equivalent of the legacy `deny_egress`). It is a bwrap `--unshare-net`
        // convenience, NOT admission-gated network isolation (which the Workdir tier
        // cannot enforce — that is `NetworkPolicy`), so it rides the opaque `extra`.
        sandbox.deny_egress = spec
            .extra
            .as_ref()
            .and_then(|v| v.get("deny_egress"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
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

        // Base env: non-secret literals only (a local provider has no egress broker).
        for var in &spec.env {
            if let pc::EnvValue::Inline { value } = &var.value {
                sandbox.base_env.push((var.name.clone(), value.clone()));
            }
        }
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
        if handle.provider_kind != "local" {
            return Err(err(format!(
                "local provider cannot adopt {:?} sandbox",
                handle.provider_kind
            )));
        }
        let outputs_path = handle
            .extra
            .as_ref()
            .and_then(|v| v.get("outputs_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("/mnt/session/outputs");
        Ok(self.build(&handle.sandbox_id, outputs_path))
    }

    fn caps() -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Workdir,
            tool_transparent: false,
            path_fidelity: false,
            enforced_readonly: false,
            network_isolation: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
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
        if let pc::MountSource::MemoryStore { store_id } = &req.source {
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
            let guard = mounter.mount(store_id, &host, req.access).await?;
            let realized = pc::RealizedMount {
                mount_id: req.mount_id.clone(),
                mount_path: req.mount_path.clone(),
                access: req.access,
                realization: guard.realization(),
                content_hash: None,
            };
            return Ok((realized, Some(guard)));
        }
        let bytes = resolve_source(&req.source, &self.blobs, &self.file_store).await;
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
        let dir = self.base.join(id);
        LocalSandbox {
            id: id.to_string(),
            root: IsolatedRoot::new(dir),
            outputs_path: outputs_path.to_string(),
            deny_egress: false,
            base_env: Vec::new(),
            realized: Vec::new(),
            secret_paths: Vec::new(),
            memory_mounts: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl pc::SandboxProvider for LocalProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        Self::caps()
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
    base_env: Vec<(String, String)>,
    realized: Vec<pc::RealizedMount>,
    /// Host paths of realized `Secret` mounts, **shredded** (overwritten) at
    /// [`dispose`](pc::Sandbox::dispose) before the directory is reaped so a
    /// materialized credential does not linger in freed disk blocks (ADR-0023).
    secret_paths: Vec<PathBuf>,
    /// Live memory-store mounts (FUSE / copy), torn down (unmount / harvest) at
    /// [`dispose`](pc::Sandbox::dispose) before the sandbox directory is reaped.
    memory_mounts: std::sync::Mutex<Vec<Box<dyn pc::MemoryMount>>>,
}

impl LocalSandbox {
    /// Materialize a runtime-owned, read-only file tree below the sandbox root.
    /// Every relative path is revalidated by [`IsolatedRoot`], existing symlinks are
    /// rejected, and permissions are narrowed only after the complete tree is
    /// written. This is a generic provisioning primitive; it knows no Skill,
    /// Workspace, principal, or authorization policy.
    pub fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>)],
    ) -> Result<(), pc::SandboxError> {
        let base = self.root.resolve(subdir).map_err(err)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&base) {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(err(format!("read-only tree root `{subdir}` is unsafe")));
            }
        } else {
            std::fs::create_dir_all(&base).map_err(err)?;
        }

        for (relative, bytes) in files {
            if relative.is_empty()
                || relative.contains('\\')
                || std::path::Path::new(relative).is_absolute()
                || std::path::Path::new(relative)
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                return Err(err(format!("read-only tree path `{relative}` is unsafe")));
            }
            let logical = format!(
                "{}/{}",
                subdir.trim_matches('/'),
                relative.trim_start_matches('/')
            );
            let destination = self.root.resolve(&logical).map_err(err)?;
            if !destination.starts_with(&base) || relative.is_empty() {
                return Err(err(format!("read-only tree path `{relative}` is unsafe")));
            }
            let parent = destination
                .parent()
                .ok_or_else(|| err(format!("read-only tree path `{relative}` has no parent")))?;
            std::fs::create_dir_all(parent).map_err(err)?;
            let mut cursor = parent.to_path_buf();
            while cursor.starts_with(&base) {
                if let Ok(metadata) = std::fs::symlink_metadata(&cursor)
                    && metadata.file_type().is_symlink()
                {
                    return Err(err(format!(
                        "read-only tree path `{relative}` crosses a symlink"
                    )));
                }
                if cursor == base || !cursor.pop() {
                    break;
                }
            }
            if let Ok(metadata) = std::fs::symlink_metadata(&destination)
                && (metadata.file_type().is_symlink() || !metadata.is_file())
            {
                return Err(err(format!("read-only tree file `{relative}` is unsafe")));
            }
            // Rehydration commonly realizes the same immutable bundle over an
            // existing read-only file. Identical bytes need no mutation. Different
            // bytes are written beside the target and atomically renamed over it,
            // avoiding a writable window and never following the target as a link.
            if std::fs::read(&destination).is_ok_and(|current| current == *bytes) {
                continue;
            }
            let sequence = READ_ONLY_TREE_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(".awaken-tree-{}-{sequence}", std::process::id()));
            let write = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                let mut permissions = file.metadata()?.permissions();
                permissions.set_readonly(true);
                file.set_permissions(permissions)?;
                drop(file);
                std::fs::rename(&temporary, &destination)
            })();
            if let Err(error) = write {
                let _ = std::fs::remove_file(&temporary);
                return Err(err(error));
            }
            let mut permissions = std::fs::metadata(&destination).map_err(err)?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&destination, permissions).map_err(err)?;
        }
        Ok(())
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
    fn build_command(&self, command: &pc::Command) -> Result<TokioCommand, pc::SandboxError> {
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
        cmd.args(&command.argv[1..]);
        cmd.current_dir(&host_cwd);
        // Reserved, runtime-owned env: where the agent writes/works (the local tier
        // has no path fidelity, so a process finds the outputs dir via this var).
        cmd.env("AWAKEN_OUTPUTS_DIR", &host_outputs);
        cmd.env("AWAKEN_PROJECT_DIR", &host_cwd);
        for (k, v) in &self.base_env {
            cmd.env(k, v);
        }
        for var in &command.env {
            if let pc::EnvValue::Inline { value } = &var.value {
                cmd.env(&var.name, value);
            }
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
        let mut cmd = self.build_command(&command)?;
        cmd.stdin(ProcStdio::piped())
            .stdout(ProcStdio::piped())
            .stderr(ProcStdio::null());
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
        rooted_raw_tools(self.root.clone(), self.deny_egress)
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
}

impl pc::RepositoryRealizer for LocalSandbox {
    fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&str>,
    ) -> Result<(), pc::SandboxError> {
        provision_repo_at(
            &self.root,
            &plan.mount_path,
            &plan.remote_url,
            plan.initial_branch.as_deref(),
            credential,
        )
        .map_err(err)
    }

    fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&str>,
    ) -> Result<bool, pc::SandboxError> {
        push_repo_at(&self.root, &plan.mount_path, credential).map_err(err)
    }
}

#[async_trait]
impl pc::Sandbox for LocalSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        let mut h = pc::SandboxHandle::new("local", &self.id);
        h.extra = Some(json!({ "outputs_path": self.outputs_path }));
        h
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let mut cmd = self.build_command(&command)?;
        let (out, e) = match command.stdio {
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

/// A process launched by a local-machine sandbox tier (shared by `LocalProvider`
/// and `NamespaceProvider`).
pub struct LocalProcess {
    id: String,
    child: AsyncMutex<Child>,
}

impl LocalProcess {
    /// Wrap a freshly spawned child; the id is its OS pid (empty if already reaped).
    pub(crate) fn spawned(child: Child) -> Self {
        let id = child.id().map(|p| p.to_string()).unwrap_or_default();
        Self {
            id,
            child: AsyncMutex::new(child),
        }
    }
}

fn to_exit(status: std::process::ExitStatus) -> pc::ExitStatus {
    pc::ExitStatus {
        code: status.code(),
        signaled: status.code().is_none(),
    }
}

#[async_trait]
impl pc::ProcessHandle for LocalProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        let mut child = self.child.lock().await;
        Ok(to_exit(child.wait().await.map_err(err)?))
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let mut child = self.child.lock().await;
        Ok(child.try_wait().map_err(err)?.map(to_exit))
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        // std/tokio only expose SIGKILL portably; all variants terminate.
        let mut child = self.child.lock().await;
        child.start_kill().map_err(err)
    }
}

#[cfg(test)]
mod shred_tests {
    use super::*;
    use awaken_provisioning_contract::{
        IsolationClass, MountAccess, MountLifetime, MountRequirement, MountSource, NetworkPolicy,
        ResourceLimits, Sandbox, SandboxSpec,
    };

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
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
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
            resolve_source(&file, &blobs, &none_store).await,
            Some(b"file".to_vec())
        );
        let resource = MountSource::Resource {
            resource_id: "r1".into(),
            content_hash: None,
        };
        assert_eq!(
            resolve_source(&resource, &blobs, &none_store).await,
            Some(b"res".to_vec())
        );
        // Inline `Other({content})` short-circuits before any store hit.
        let inline = MountSource::Other(serde_json::json!({ "content": "inline" }));
        assert_eq!(
            resolve_source(&inline, &blobs, &none_store).await,
            Some(b"inline".to_vec())
        );
        let binary = vec![0, 0xff, 0x80, b'\n'];
        let carried = MountSource::InlineBytes {
            contents: binary.clone(),
            content_hash: Some(content_fingerprint(&binary)),
        };
        assert_eq!(
            resolve_source(&carried, &blobs, &none_store).await,
            Some(binary.clone()),
            "binary input must round-trip without UTF-8 coercion"
        );
        assert!(verify(&carried, &binary).is_ok());
        assert!(verify(&carried, b"corrupt").is_err());
        // A memory store is not byte-resolvable through this path.
        let mem = MountSource::MemoryStore {
            store_id: "m".into(),
        };
        assert_eq!(resolve_source(&mem, &blobs, &none_store).await, None);
        // An unknown id resolves to nothing.
        let missing = MountSource::File {
            file_id: "nope".into(),
            content_hash: None,
        };
        assert_eq!(resolve_source(&missing, &blobs, &none_store).await, None);
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
        assert!(verify(&MountSource::Other(serde_json::json!({})), b"any").is_ok());
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

        let mut cmd = pc::Command::new(["/bin/true"]);
        cmd.cwd = "/work".into();
        cmd.env = vec![pc::EnvVar {
            name: "K".into(),
            value: pc::EnvValue::Inline { value: "V".into() },
            visibility: pc::EnvVisibility::Process,
        }];
        assert!(sandbox.build_command(&cmd).is_ok());

        let empty = pc::Command::new(Vec::<String>::new());
        assert!(sandbox.build_command(&empty).is_err());
    }

    #[tokio::test]
    async fn a_spawned_local_process_reports_its_pid_and_reaps() {
        use awaken_provisioning_contract::ProcessHandle;
        let child = TokioCommand::new("true").spawn().unwrap();
        let proc = LocalProcess::spawned(child);
        assert!(!proc.id().is_empty());
        let status = proc.wait().await.unwrap();
        assert_eq!(status.code, Some(0));
    }
}

/// The Workdir-tier host helpers that replace the legacy `Environment` methods —
/// rooted tools, repo provisioning/write-back, artifact/skill scanning — exercised
/// on a `LocalSandbox` built through the pc `SandboxProvider` path.
#[cfg(test)]
mod workdir_helper_tests {
    use super::*;
    use awaken_provisioning_contract::{
        IsolationClass, NetworkPolicy, ResourceLimits, SandboxSpec,
    };

    fn workdir_spec(scope: &str, deny_egress: bool) -> SandboxSpec {
        SandboxSpec {
            scope: scope.into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            // The Workdir tier admits only unrestricted network (it cannot enforce
            // isolation); egress denial for the rooted bash tool rides `extra`.
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: deny_egress.then(|| json!({ "deny_egress": true })),
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

        // A SKILL.md-bearing dir surfaces as neutral file data under a logical dir.
        std::fs::create_dir_all(root.join("skills/greet")).unwrap();
        std::fs::write(root.join("skills/greet/SKILL.md"), "# greet").unwrap();
        let skills = sandbox.scan_skill_dir("skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "greet");
        assert_eq!(skills[0].dir, "skills/greet");
        assert_eq!(skills[0].content, "# greet");
    }

    #[tokio::test]
    async fn provision_clones_then_the_host_pushes_the_agents_own_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        // Seed a bare "remote" with one commit.
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
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

        let provider = LocalProvider::new(base.join("envs"));
        let sandbox = provider
            .create_sandbox(&workdir_spec("t-repo", false))
            .await
            .unwrap();
        let root = sandbox.root.root().to_path_buf();
        let plan = pc::RepositoryRealizationPlan {
            repository_id: "repo-1".into(),
            mount_path: "workspace/repo".into(),
            remote_url: bare.to_string_lossy().into_owned(),
            initial_branch: None,
            access: pc::MountAccess::ReadWrite,
        };

        pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None).unwrap();
        let repo_dir = root.join("workspace/repo");
        assert_eq!(
            std::fs::read_to_string(repo_dir.join("README.md")).unwrap(),
            "hello"
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

        // Nothing authored yet → the host push is a no-op (agent committed nothing).
        assert!(!pc::RepositoryRealizer::publish_repository(&sandbox, &plan, None).unwrap());

        // The AGENT configures its own identity and authors a commit in the jail — a clean
        // working tree afterwards (it committed everything), which the OLD harvest would have
        // wrongly skipped. The host then only pushes.
        git(&repo_dir, &["config", "user.email", "hermes@agent.local"]);
        git(&repo_dir, &["config", "user.name", "Hermes"]);
        std::fs::write(repo_dir.join("NEW.txt"), "agent").unwrap();
        git(&repo_dir, &["add", "-A"]);
        git(&repo_dir, &["commit", "-q", "-m", "agent: add NEW.txt"]);

        // Host push reports true (the branch was ahead) and re-pushing is an idempotent no-op.
        assert!(pc::RepositoryRealizer::publish_repository(&sandbox, &plan, None).unwrap());
        assert!(!pc::RepositoryRealizer::publish_repository(&sandbox, &plan, None).unwrap());

        // The bare remote carries the AGENT's commit — its own message and author, not a
        // canned harvest commit by a fake user.
        let log = std::process::Command::new("git")
            .current_dir(&bare)
            .args(["log", "-1", "--pretty=%an|%ae|%s"])
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
            remote_url: "http://x".into(),
            initial_branch: None,
            access: pc::MountAccess::ReadOnly,
        };
        assert!(pc::RepositoryRealizer::realize_repository(&sandbox, &plan, None).is_err());
    }

    #[tokio::test]
    async fn read_only_tree_preserves_binary_files_and_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = LocalProvider::new(tmp.path())
            .create_sandbox(&workdir_spec("skill-tree", false))
            .await
            .unwrap();
        let binary = vec![0, 159, 146, 150, 255];
        sandbox
            .materialize_read_only_tree(
                ".skills/greet",
                &[
                    ("SKILL.md".into(), b"# greet".to_vec()),
                    ("assets/data.bin".into(), binary.clone()),
                ],
            )
            .unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("skill-tree/.skills/greet/assets/data.bin")).unwrap(),
            binary
        );
        assert!(
            std::fs::metadata(tmp.path().join("skill-tree/.skills/greet/SKILL.md"))
                .unwrap()
                .permissions()
                .readonly()
        );
        // Rehydrating identical bytes is idempotent even though the target is
        // read-only; a changed immutable version is atomically replaced and ends
        // read-only as well.
        sandbox
            .materialize_read_only_tree(
                ".skills/greet",
                &[("SKILL.md".into(), b"# greet".to_vec())],
            )
            .unwrap();
        sandbox
            .materialize_read_only_tree(
                ".skills/greet",
                &[("SKILL.md".into(), b"# greet v2".to_vec())],
            )
            .unwrap();
        let skill_md = tmp.path().join("skill-tree/.skills/greet/SKILL.md");
        assert_eq!(std::fs::read(&skill_md).unwrap(), b"# greet v2");
        assert!(
            std::fs::metadata(skill_md)
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(
            sandbox
                .materialize_read_only_tree(".skills/bad", &[("../escape".into(), vec![])])
                .is_err()
        );
    }
}
