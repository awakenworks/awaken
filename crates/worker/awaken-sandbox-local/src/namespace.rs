//! `NamespaceProvider` — the OS-namespace (bubblewrap on Linux) sandbox tier
//! (ADR-0041 Slice 2). Unlike the lexical `LocalProvider`, this tier is
//! **tool-transparent**: bubblewrap binds host paths to real sandbox-absolute
//! paths (`/workspace`, `/mnt/session/outputs`) and unshares namespaces, so an
//! *opaque* process (Claude Code, any CLI) is confined by the OS regardless of
//! what it does. It reports `tool_transparent = true`, so `prepare_environment`
//! permits `Namespace`-class workloads here.
//!
//! The launcher argv is rendered by pure functions ([`bubblewrap_argv`],
//! [`sandbox_exec_argv`]) — unit-testable without the tool installed; the actual
//! exec requires `bwrap` on the host.

use std::path::PathBuf;
use std::process::Stdio as ProcStdio;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_provisioning_contract as pc;
use serde_json::json;
use tokio::process::Command as TokioCommand;

use std::sync::Arc;

use crate::provider::{LocalProcess, resolve_source, verify};
use crate::{IsolatedRoot, content_fingerprint};

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

/// One realized bind for the launcher: a host path exposed at a sandbox path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderMount {
    pub host: PathBuf,
    pub dest: String,
    pub read_only: bool,
}

/// Everything the pure launcher renderers need. Host paths appear only here (a
/// renderer input), never in the neutral contract (G3).
pub struct RenderInput<'a> {
    pub host_workspace: &'a std::path::Path,
    pub host_outputs: &'a std::path::Path,
    pub outputs_path: &'a str,
    pub mounts: &'a [RenderMount],
    pub env: &'a [(String, String)],
    pub network: &'a pc::NetworkPolicy,
    pub cwd: &'a str,
    pub argv: &'a [String],
}

fn s(v: impl Into<String>) -> String {
    v.into()
}

/// Render a `bwrap` command line (unprivileged, Linux). Deterministic and pure.
/// Layout: unshare namespaces, mount `/proc` `/dev` `/tmp`, read-only-bind the
/// host userland (so interpreters exist), bind the workspace and outputs, bind
/// each declared mount (ro/rw), inject reserved + user env, `--chdir`, then `--`
/// and the program argv.
#[must_use]
pub fn bubblewrap_argv(input: &RenderInput) -> Vec<String> {
    let mut a: Vec<String> = vec![s("bwrap")];
    a.extend(
        [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
        ]
        .into_iter()
        .map(s),
    );
    match input.network {
        pc::NetworkPolicy::Unrestricted => {} // share the host network namespace
        _ => a.push(s("--unshare-net")),
    }
    a.extend(["--die-with-parent", "--new-session"].into_iter().map(s));
    a.extend(["--proc", "/proc"].into_iter().map(s));
    a.extend(["--dev", "/dev"].into_iter().map(s));
    a.extend(["--tmpfs", "/tmp"].into_iter().map(s));
    for dir in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"] {
        a.push(s("--ro-bind-try"));
        a.push(s(dir));
        a.push(s(dir));
    }
    a.push(s("--bind"));
    a.push(input.host_workspace.to_string_lossy().into_owned());
    a.push(s("/workspace"));
    a.push(s("--bind"));
    a.push(input.host_outputs.to_string_lossy().into_owned());
    a.push(s(input.outputs_path));
    for m in input.mounts {
        a.push(s(if m.read_only { "--ro-bind" } else { "--bind" }));
        a.push(m.host.to_string_lossy().into_owned());
        a.push(m.dest.clone());
    }
    a.extend(
        ["--setenv", "AWAKEN_OUTPUTS_DIR", input.outputs_path]
            .into_iter()
            .map(s),
    );
    a.extend(
        ["--setenv", "AWAKEN_PROJECT_DIR", "/workspace"]
            .into_iter()
            .map(s),
    );
    for (k, v) in input.env {
        a.push(s("--setenv"));
        a.push(k.clone());
        a.push(v.clone());
    }
    a.push(s("--chdir"));
    a.push(s(if input.cwd.is_empty() {
        "/workspace"
    } else {
        input.cwd
    }));
    a.push(s("--"));
    a.extend(input.argv.iter().cloned());
    a
}

/// Render a macOS `sandbox-exec` (Seatbelt) command line. Deny-by-default with
/// read of the workspace and write to workspace + outputs. Pure; not executed on
/// Linux CI (present for tier parity + unit coverage).
#[must_use]
pub fn sandbox_exec_argv(input: &RenderInput) -> Vec<String> {
    let ws = input.host_workspace.to_string_lossy();
    let out = input.host_outputs.to_string_lossy();
    let profile = format!(
        "(version 1)(deny default)(allow process-fork)(allow process-exec)\
         (allow file-read* (subpath \"{ws}\"))\
         (allow file-write* (subpath \"{ws}\"))(allow file-write* (subpath \"{out}\"))"
    );
    let mut a = vec![s("sandbox-exec"), s("-p"), profile, s("--")];
    a.extend(input.argv.iter().cloned());
    a
}

/// Realizes [`NamespaceSandbox`] environments (bubblewrap tier).
pub struct NamespaceProvider {
    base: PathBuf,
    blobs: std::collections::HashMap<String, Vec<u8>>,
    file_store: Option<Arc<dyn pc::BlobSource>>,
    /// Optional memory-store realizer. On this tier it should be a copy-only mounter
    /// ([`MemoryStoreMounter::copy_only`]): a host FUSE mount cannot yet be spliced
    /// into the bwrap namespace (ADR-0053 item 2), so the store is materialized to
    /// files that bind in, and harvested back on dispose.
    memory_mounter: Option<Arc<dyn pc::MemoryMounter>>,
}

impl NamespaceProvider {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            blobs: std::collections::HashMap::new(),
            file_store: None,
            memory_mounter: None,
        }
    }

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

    /// Realize `MemoryStore` mounts via an injected mounter (copy-only on this tier).
    /// Without one, a `MemoryStore` mount fails loud.
    #[must_use]
    pub fn with_memory_mounter(mut self, mounter: Arc<dyn pc::MemoryMounter>) -> Self {
        self.memory_mounter = Some(mounter);
        self
    }

    fn caps() -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Namespace,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: true,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
        }
    }

    /// Realize the mounts under `root`, all-or-nothing: any failure reaps the tree.
    /// Returns the bind layout, the realized refs, and any live memory-store guards
    /// (torn down / harvested at dispose).
    async fn realize_layout(
        &self,
        root: &IsolatedRoot,
        spec: &pc::SandboxSpec,
    ) -> Result<
        (
            Vec<RenderMount>,
            Vec<pc::RealizedMount>,
            Vec<Box<dyn pc::MemoryMount>>,
        ),
        pc::SandboxError,
    > {
        let mut layout = Vec::new();
        let mut realized = Vec::new();
        let mut memory_mounts: Vec<Box<dyn pc::MemoryMount>> = Vec::new();
        for req in &spec.mounts {
            let host = root.resolve(&req.mount_path).map_err(err)?;
            // memory_store is a keyed store, not a byte blob (ADR-0038/0053): realize
            // it as a directory of materialized files that bind into the namespace,
            // harvested back on dispose (copy-only tier; live FUSE-in-bwrap is item 2).
            if let pc::MountSource::MemoryStore { store_id } = &req.source {
                let Some(mounter) = &self.memory_mounter else {
                    return Err(err(format!(
                        "mount {:?}: memory_store is not realizable on this provider (no memory mounter wired)",
                        req.mount_id
                    )));
                };
                let guard = mounter.mount(store_id, &host, req.access).await?;
                layout.push(RenderMount {
                    host: host.clone(),
                    dest: req.mount_path.clone(),
                    read_only: req.access == pc::MountAccess::ReadOnly,
                });
                realized.push(pc::RealizedMount {
                    mount_id: req.mount_id.clone(),
                    mount_path: req.mount_path.clone(),
                    access: req.access,
                    realization: guard.realization(),
                    content_hash: None,
                });
                memory_mounts.push(guard);
                continue;
            }
            let bytes = resolve_source(&req.source, &self.blobs, &self.file_store).await;
            match &bytes {
                Some(bytes) => {
                    verify(&req.source, bytes)?; // fail closed on content-hash mismatch
                    if let Some(parent) = host.parent() {
                        std::fs::create_dir_all(parent).map_err(err)?;
                    }
                    std::fs::write(&host, bytes).map_err(err)?;
                }
                None if req.required => {
                    return Err(err(format!(
                        "required mount {:?} has no resolvable source",
                        req.mount_id
                    )));
                }
                None => {
                    if let Some(parent) = host.parent() {
                        std::fs::create_dir_all(parent).map_err(err)?;
                    }
                    std::fs::write(&host, b"").map_err(err)?;
                }
            }
            layout.push(RenderMount {
                host: host.clone(),
                dest: req.mount_path.clone(),
                read_only: req.access == pc::MountAccess::ReadOnly,
            });
            realized.push(pc::RealizedMount {
                mount_id: req.mount_id.clone(),
                mount_path: req.mount_path.clone(),
                access: req.access,
                realization: pc::Realization::Bind,
                content_hash: bytes.as_ref().map(|b| content_fingerprint(b)),
            });
        }
        Ok((layout, realized, memory_mounts))
    }
}

#[async_trait]
impl pc::SandboxProvider for NamespaceProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        Self::caps()
    }

    /// A real readiness probe: bwrap must actually run an unprivileged user
    /// namespace *here*. On a host that blocks userns this returns an error, so
    /// `select_provider` fails closed at selection instead of deferring the failure
    /// to `create` (or, worse, launching an unisolated process).
    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        let ok = TokioCommand::new("bwrap")
            .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
            .stdin(ProcStdio::null())
            .stdout(ProcStdio::null())
            .stderr(ProcStdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err(err(
                "bwrap/unprivileged user namespaces unavailable on this host",
            ))
        }
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
            .unwrap_or("/mnt/session/outputs")
            .to_string();
        let root = IsolatedRoot::new(self.base.join(&handle.sandbox_id));
        let host_workspace = root.resolve("/workspace").map_err(err)?;
        let host_outputs = root.resolve(&outputs_path).map_err(err)?;
        Ok(Box::new(NamespaceSandbox {
            id: handle.sandbox_id.clone(),
            root,
            outputs_path,
            host_workspace,
            host_outputs,
            base_env: Vec::new(),
            network: pc::NetworkPolicy::Unrestricted,
            layout: Vec::new(),
            realized: Vec::new(),
            memory_mounts: std::sync::Mutex::new(Vec::new()),
        }))
    }
}

impl NamespaceProvider {
    /// Realize a sandbox and return the concrete [`NamespaceSandbox`], so a caller
    /// can use the tool-transparent [`NamespaceSandbox::spawn_agent`] capability.
    /// The trait `create` delegates here and boxes the result.
    pub async fn create_sandbox(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        pc::prepare_environment(spec, &Self::caps()).map_err(err)?;
        // bwrap can share or unshare the net namespace, but cannot enforce a
        // host allowlist; refuse it rather than silently blocking all egress.
        if matches!(spec.network, pc::NetworkPolicy::Allowlist { .. }) {
            return Err(err(
                "bwrap tier supports on/off egress only, not a host allowlist",
            ));
        }

        let root = IsolatedRoot::new(self.base.join(&spec.scope));
        std::fs::create_dir_all(root.root()).map_err(err)?;
        let host_workspace = root.resolve("/workspace").map_err(err)?;
        std::fs::create_dir_all(&host_workspace).map_err(err)?;
        let host_outputs = root.resolve(&spec.outputs_path).map_err(err)?;
        std::fs::create_dir_all(&host_outputs).map_err(err)?;

        let mut base_env = Vec::new();
        for var in &spec.env {
            if let pc::EnvValue::Inline { value } = &var.value {
                base_env.push((var.name.clone(), value.clone()));
            }
        }

        let (layout, realized, memory_mounts) = match self.realize_layout(&root, spec).await {
            Ok(v) => v,
            Err(e) => {
                // On a failed layout, `realize_layout`'s already-realized guards drop
                // as it returns: a FUSE mount unmounts via its handle's Drop, and a
                // copy's files are reaped with the directory below (no harvest — the
                // durable store is left untouched on an aborted create).
                let _ = std::fs::remove_dir_all(root.root());
                return Err(e);
            }
        };

        Ok(NamespaceSandbox {
            id: spec.scope.clone(),
            root,
            outputs_path: spec.outputs_path.clone(),
            host_workspace,
            host_outputs,
            base_env,
            network: spec.network.clone(),
            layout,
            realized,
            memory_mounts: std::sync::Mutex::new(memory_mounts),
        })
    }
}

/// A realized bubblewrap environment. Path-fidelity + OS-confined.
pub struct NamespaceSandbox {
    id: String,
    root: IsolatedRoot,
    outputs_path: String,
    host_workspace: PathBuf,
    host_outputs: PathBuf,
    base_env: Vec<(String, String)>,
    network: pc::NetworkPolicy,
    layout: Vec<RenderMount>,
    realized: Vec<pc::RealizedMount>,
    /// Live memory-store mounts, harvested / unmounted at dispose before the tree is
    /// reaped. Empty after an `adopt` (a reconnected sandbox owns no fresh guards).
    memory_mounts: std::sync::Mutex<Vec<Box<dyn pc::MemoryMount>>>,
}

impl NamespaceSandbox {
    /// Tear down every live memory mount (harvest a copy / unmount a FUSE), draining
    /// the guard list so a later dispose is a no-op.
    async fn teardown_memory_mounts(&self) {
        let mounts: Vec<Box<dyn pc::MemoryMount>> =
            std::mem::take(&mut self.memory_mounts.lock().unwrap());
        for mount in mounts {
            mount.teardown().await;
        }
    }

    /// The rendered launcher argv for `command` (bwrap wrapping the program).
    fn render_argv(&self, command: &pc::Command) -> Result<Vec<String>, pc::SandboxError> {
        if command.argv.is_empty() {
            return Err(err("command argv is empty"));
        }
        let mut cmd_env = self.base_env.clone();
        for var in &command.env {
            if let pc::EnvValue::Inline { value } = &var.value {
                cmd_env.push((var.name.clone(), value.clone()));
            }
        }
        let input = RenderInput {
            host_workspace: &self.host_workspace,
            host_outputs: &self.host_outputs,
            outputs_path: &self.outputs_path,
            mounts: &self.layout,
            env: &cmd_env,
            network: &self.network,
            cwd: &command.cwd,
            argv: &command.argv,
        };
        Ok(bubblewrap_argv(&input))
    }

    /// The tool-transparent agent launch (ADR-0041 amendment), namespace-tier twin
    /// of [`crate::LocalSandbox::spawn_agent`]: run an opaque agent under bwrap
    /// with piped stdio and hand back its [`pc::ProcessHandle`] plus a duplex
    /// [`AgentChannel`] (its stdout+stdin). The bridge drives ACP over the channel
    /// while the supervisor polls the handle; the OS confines the process — and,
    /// under [`pc::NetworkPolicy::None`], unshares its network namespace —
    /// regardless of what the agent does.
    pub async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannel>), pc::SandboxError> {
        let argv = self.render_argv(&command)?;
        let mut cmd = TokioCommand::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(ProcStdio::piped())
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
impl pc::Sandbox for NamespaceSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        let mut h = pc::SandboxHandle::new("bwrap", &self.id);
        h.extra = Some(json!({ "outputs_path": self.outputs_path }));
        h
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let argv = self.render_argv(&command)?;
        let mut cmd = TokioCommand::new(&argv[0]);
        cmd.args(&argv[1..]);
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
        Err(err("runtime attach is a Slice-4 (FileStore) concern"))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        Ok(
            crate::artifacts::scan_outputs(&self.host_outputs, &self.outputs_path)?
                .into_iter()
                .map(|(a, _)| a)
                .collect(),
        )
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        for (artifact, host) in
            crate::artifacts::scan_outputs(&self.host_outputs, &self.outputs_path)?
        {
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
        Err(err(
            "local namespace tier cannot reattach to a process across owners",
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
        Ok(())
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        // Harvest / unmount memory stores BEFORE the tree is reaped (a copy harvest
        // reads the edited files back).
        self.teardown_memory_mounts().await;
        let root = self.root.root();
        if root.exists() {
            std::fs::remove_dir_all(root).map_err(err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        ws: &'a std::path::Path,
        out: &'a std::path::Path,
        mounts: &'a [RenderMount],
        env: &'a [(String, String)],
        net: &'a pc::NetworkPolicy,
        argv: &'a [String],
    ) -> RenderInput<'a> {
        RenderInput {
            host_workspace: ws,
            host_outputs: out,
            outputs_path: "/mnt/session/outputs",
            mounts,
            env,
            network: net,
            cwd: "",
            argv,
        }
    }

    #[test]
    fn bubblewrap_binds_workspace_outputs_and_ends_with_argv() {
        let ws = PathBuf::from("/host/ws");
        let out = PathBuf::from("/host/out");
        let argv = vec![s("claude"), s("--acp")];
        let a = bubblewrap_argv(&input(
            &ws,
            &out,
            &[],
            &[],
            &pc::NetworkPolicy::Unrestricted,
            &argv,
        ));

        assert_eq!(a[0], "bwrap");
        // workspace + outputs binds present
        let joined = a.join(" ");
        assert!(joined.contains("--bind /host/ws /workspace"));
        assert!(joined.contains("--bind /host/out /mnt/session/outputs"));
        assert!(joined.contains("--setenv AWAKEN_OUTPUTS_DIR /mnt/session/outputs"));
        assert!(joined.contains("--chdir /workspace"));
        // program follows the -- separator, in order
        let sep = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(&a[sep + 1..], &["claude", "--acp"]);
        // unrestricted net => no --unshare-net
        assert!(!a.iter().any(|x| x == "--unshare-net"));
    }

    #[test]
    fn bubblewrap_unshares_net_when_not_unrestricted() {
        let ws = PathBuf::from("/w");
        let out = PathBuf::from("/o");
        let argv = vec![s("true")];
        let a = bubblewrap_argv(&input(&ws, &out, &[], &[], &pc::NetworkPolicy::None, &argv));
        assert!(a.iter().any(|x| x == "--unshare-net"));
    }

    #[test]
    fn bubblewrap_renders_ro_and_rw_mounts_and_env() {
        let ws = PathBuf::from("/w");
        let out = PathBuf::from("/o");
        let mounts = vec![
            RenderMount {
                host: PathBuf::from("/h/in"),
                dest: "/workspace/in.txt".into(),
                read_only: true,
            },
            RenderMount {
                host: PathBuf::from("/h/rw"),
                dest: "/data".into(),
                read_only: false,
            },
        ];
        let env = vec![("TZ".to_string(), "UTC".to_string())];
        let argv = vec![s("sh")];
        let a = bubblewrap_argv(&input(
            &ws,
            &out,
            &mounts,
            &env,
            &pc::NetworkPolicy::Unrestricted,
            &argv,
        ));
        let j = a.join(" ");
        assert!(j.contains("--ro-bind /h/in /workspace/in.txt"));
        assert!(j.contains("--bind /h/rw /data"));
        assert!(j.contains("--setenv TZ UTC"));
    }

    #[test]
    fn sandbox_exec_wraps_argv_with_a_profile() {
        let ws = PathBuf::from("/w");
        let out = PathBuf::from("/o");
        let argv = vec![s("claude")];
        let a = sandbox_exec_argv(&input(
            &ws,
            &out,
            &[],
            &[],
            &pc::NetworkPolicy::Unrestricted,
            &argv,
        ));
        assert_eq!(a[0], "sandbox-exec");
        assert_eq!(a[1], "-p");
        assert!(a[2].contains("(deny default)"));
        assert!(a[2].contains("/w"));
        assert_eq!(a.last().unwrap(), "claude");
    }
}
