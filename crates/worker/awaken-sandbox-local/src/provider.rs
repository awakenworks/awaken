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
//! lease lifecycle the contract requires.
//!
//! It is additive: the pre-contract [`crate::Environment`] / [`crate::SandboxProvider`]
//! surface used elsewhere is untouched (a later slice may unify them).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio as ProcStdio;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_provisioning_contract as pc;
use serde_json::json;
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::Mutex as AsyncMutex;

use std::sync::Arc;

use awaken_file_store::FileStore;

use crate::{IsolatedRoot, content_fingerprint};

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

/// Resolve a mount's bytes: an in-memory seed map first, then an optional
/// content-addressed [`FileStore`], then inline `Other({content})`.
pub(crate) async fn resolve_source(
    source: &pc::MountSource,
    blobs: &HashMap<String, Vec<u8>>,
    store: &Option<Arc<dyn FileStore>>,
) -> Option<Vec<u8>> {
    // Inline `Other({content})` and unresolvable memory stores short-circuit before
    // any store hit; the rest resolve by id (seed map first, then the store).
    let id = match source {
        pc::MountSource::File { file_id, .. } => file_id.as_str(),
        pc::MountSource::Resource { resource_id, .. } => resource_id.as_str(),
        // The broker is faked locally by the seed map / store keyed on the reference.
        pc::MountSource::Secret { reference, .. } => reference.as_str(),
        pc::MountSource::Other(v) => {
            return v
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.as_bytes().to_vec());
        }
        pc::MountSource::MemoryStore { .. } => return None,
    };
    if let Some(bytes) = blobs.get(id) {
        return Some(bytes.clone());
    }
    if let Some(store) = store {
        if let Ok(Some(bytes)) = store.get(id).await {
            return Some(bytes);
        }
    }
    None
}

/// The declared content hash of a mount source, if any (verified fail-closed).
pub(crate) fn declared_hash(source: &pc::MountSource) -> Option<&str> {
    match source {
        pc::MountSource::File { content_hash, .. } => content_hash.as_deref(),
        pc::MountSource::Resource { content_hash, .. } => content_hash.as_deref(),
        pc::MountSource::Secret { content_hash, .. } => content_hash.as_deref(),
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

/// Realizes [`LocalSandbox`] environments as directories under a base path.
pub struct LocalProvider {
    base: PathBuf,
    /// In-memory blob seed for `File`/`Resource` mounts (keyed by id).
    blobs: HashMap<String, Vec<u8>>,
    /// Optional content-addressed store consulted after the seed map (Slice 4).
    file_store: Option<Arc<dyn FileStore>>,
}

impl LocalProvider {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            blobs: HashMap::new(),
            file_store: None,
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
    pub fn with_file_store(mut self, store: Arc<dyn FileStore>) -> Self {
        self.file_store = Some(store);
        self
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
        std::fs::create_dir_all(sandbox.root.root()).map_err(err)?;
        // Outputs directory (sandbox-absolute → rejailed host path).
        let host_outputs = sandbox.root.resolve(&spec.outputs_path).map_err(err)?;
        std::fs::create_dir_all(&host_outputs).map_err(err)?;

        // Base env: non-secret literals only (a local provider has no egress broker).
        for var in &spec.env {
            if let pc::EnvValue::Inline { value } = &var.value {
                sandbox.base_env.push((var.name.clone(), value.clone()));
            }
        }
        // All-or-nothing: a failed mount reaps the whole environment (no partial dir).
        for req in &spec.mounts {
            match self.realize_mount(&sandbox.root, req).await {
                Ok(m) => sandbox.realized.push(m),
                Err(e) => {
                    let _ = std::fs::remove_dir_all(sandbox.root.root());
                    return Err(e);
                }
            }
        }
        Ok(sandbox)
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
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        let host = root.resolve(&req.mount_path).map_err(err)?;
        // A memory_store is a genuine keyed store (ADR-0038), not a byte blob. This
        // Workdir provider has no memory backend wired, so realizing it would either
        // silently produce an empty placeholder file (misleading the agent into
        // thinking it has a store) or fake it. Fail loud instead — a real backend
        // realizes it, not a File copy.
        if matches!(req.source, pc::MountSource::MemoryStore { .. }) {
            return Err(err(format!(
                "mount {:?}: memory_store is not realizable on this provider (no memory backend wired)",
                req.mount_id
            )));
        }
        let bytes = resolve_source(&req.source, &self.blobs, &self.file_store).await;
        match bytes {
            Some(bytes) => {
                verify(&req.source, &bytes)?; // fail closed on content-hash mismatch
                if let Some(parent) = host.parent() {
                    std::fs::create_dir_all(parent).map_err(err)?;
                }
                std::fs::write(&host, &bytes).map_err(err)?;
                Ok(pc::RealizedMount {
                    mount_id: req.mount_id.clone(),
                    mount_path: req.mount_path.clone(),
                    access: req.access,
                    realization: pc::Realization::Copy,
                    content_hash: Some(content_fingerprint(&bytes)),
                })
            }
            None if req.required => Err(err(format!(
                "required mount {:?} has no resolvable source on the local provider",
                req.mount_id
            ))),
            None => {
                // Optional + unresolvable: create an empty placeholder so the path exists.
                if let Some(parent) = host.parent() {
                    std::fs::create_dir_all(parent).map_err(err)?;
                }
                std::fs::write(&host, b"").map_err(err)?;
                Ok(pc::RealizedMount {
                    mount_id: req.mount_id.clone(),
                    mount_path: req.mount_path.clone(),
                    access: req.access,
                    realization: pc::Realization::Copy,
                    content_hash: None,
                })
            }
        }
    }

    fn build(&self, id: &str, outputs_path: &str) -> LocalSandbox {
        let dir = self.base.join(id);
        LocalSandbox {
            id: id.to_string(),
            root: IsolatedRoot::new(dir),
            outputs_path: outputs_path.to_string(),
            base_env: Vec::new(),
            realized: Vec::new(),
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
        let outputs_path = handle
            .extra
            .as_ref()
            .and_then(|v| v.get("outputs_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("/mnt/session/outputs");
        Ok(Box::new(self.build(&handle.sandbox_id, outputs_path)))
    }
}

/// A realized local environment: an [`IsolatedRoot`] directory plus its outputs
/// path and base env. Path-jailed but not OS-confined (Workdir tier).
pub struct LocalSandbox {
    id: String,
    root: IsolatedRoot,
    outputs_path: String,
    base_env: Vec<(String, String)>,
    realized: Vec<pc::RealizedMount>,
}

impl LocalSandbox {
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
        let root = self.root.root();
        if root.exists() {
            std::fs::remove_dir_all(root).map_err(err)?;
        }
        Ok(())
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
