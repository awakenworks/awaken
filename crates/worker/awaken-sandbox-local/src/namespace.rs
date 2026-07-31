//! `NamespaceProvider` — the OS-native process sandbox tier: bubblewrap namespaces
//! on Linux and Seatbelt on macOS (ADR-0041 Slice 2). Unlike the lexical
//! `LocalProvider`, this tier is **tool-transparent**: an opaque process (Claude
//! Code, any CLI) is confined by the OS regardless of what it does. Linux also has
//! real sandbox-absolute bind paths; Seatbelt has no mount namespace, so macOS
//! advertises `path_fidelity = false` and translates cwd/known argv paths while
//! exporting the realized workspace/output host paths through reserved env vars.
//!
//! The launcher argv is rendered by pure functions ([`bubblewrap_argv`],
//! [`sandbox_exec_argv`]) — unit-testable without the tool installed; the actual
//! exec requires the matching OS launcher on the host.

use std::path::PathBuf;
use std::process::Stdio as ProcStdio;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_local_process::LocalProcess;
use awaken_provisioning_contract as pc;
use serde_json::json;
use tokio::process::Command as TokioCommand;

use std::sync::Arc;

use crate::provider::{materialize_read_only_tree_at, resolve_source, restrict_to_owner, verify};
use crate::{
    DiscoveredSkillFile, IsolatedRoot, content_fingerprint, jailed_at, list_files_at,
    namespace_raw_tools, provision_repo_at, push_repo_at, scan_skill_dir_at,
};

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

/// Process-global memo of the OS-native isolator's availability (the tool's presence
/// is a host property, so the throwaway probe runs at most once — see `probe_ready`).
static OS_SANDBOX_PROBE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

/// Run the actual throwaway isolation probe: bwrap (Linux) / `sandbox-exec` (macOS)
/// must run a trivial confined `true` here.
async fn run_os_native_probe() -> bool {
    let argv = if cfg!(target_os = "macos") {
        sandbox_exec_argv(&RenderInput {
            host_workspace: std::path::Path::new("/private/var/empty"),
            host_outputs: std::path::Path::new("/private/var/empty"),
            outputs_path: "/mnt/session/outputs",
            mounts: &[],
            env: &[],
            network: &pc::NetworkPolicy::None,
            cwd: "",
            argv: &["/usr/bin/true".to_string()],
        })
    } else {
        [
            "bwrap",
            "--unshare-user",
            "--ro-bind",
            "/",
            "/",
            "--",
            "true",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    };
    TokioCommand::new(&argv[0])
        .args(&argv[1..])
        .stdin(ProcStdio::null())
        .stdout(ProcStdio::null())
        .stderr(ProcStdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
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

fn sandbox_mount_destination(dest: &str) -> String {
    if dest.starts_with('/') {
        dest.to_string()
    } else {
        format!("/workspace/{dest}")
    }
}

fn workspace_relative(logical: &str) -> &str {
    logical
        .strip_prefix("/workspace/")
        .or_else(|| logical.strip_prefix("workspace/"))
        .or_else(|| (logical == "/workspace").then_some(""))
        .or_else(|| (logical == "workspace").then_some(""))
        .unwrap_or_else(|| logical.trim_start_matches('/'))
}

fn host_projection_path(
    root: &IsolatedRoot,
    host_workspace: &std::path::Path,
    logical: &str,
) -> Result<PathBuf, pc::SandboxError> {
    if !logical.starts_with('/') || logical == "/workspace" || logical.starts_with("/workspace/") {
        IsolatedRoot::new(host_workspace)
            .resolve(workspace_relative(logical))
            .map_err(err)
    } else {
        root.resolve(logical).map_err(err)
    }
}

type RealizedMemoryMount = (RenderMount, pc::RealizedMount, Box<dyn pc::MemoryMount>);

async fn realize_memory_mount(
    memory_mounter: &Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    req: &pc::MountRequirement,
    host: &std::path::Path,
) -> Result<Option<RealizedMemoryMount>, pc::SandboxError> {
    let pc::MountSource::MemoryStore {
        store_id,
        materialization_reference,
        write_consistency,
    } = &req.source
    else {
        return Ok(None);
    };
    let Some(mounter) = memory_mounter
        .read()
        .expect("memory mounter lock poisoned")
        .clone()
    else {
        return Err(err(format!(
            "mount {:?}: memory_store is not realizable on this provider (no memory mounter wired)",
            req.mount_id
        )));
    };
    let guard = mounter
        .mount(
            materialization_reference.as_deref().unwrap_or(store_id),
            host,
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
    let realization = guard.realization();
    Ok(Some((
        RenderMount {
            host: host.to_path_buf(),
            dest: req.mount_path.clone(),
            read_only: req.access == pc::MountAccess::ReadOnly,
        },
        pc::RealizedMount {
            mount_id: req.mount_id.clone(),
            mount_path: req.mount_path.clone(),
            access: req.access,
            realization,
            content_hash: None,
        },
        guard,
    )))
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
    // `/etc/resolv.conf` is commonly a symlink into one of these `/run`
    // directories. Binding `/etc` alone leaves a dangling link and makes every
    // otherwise-unrestricted namespace fail DNS with EAI_AGAIN. The `-try`
    // form stays portable when either resolver runtime directory is absent.
    for dir in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/etc",
        "/run/systemd/resolve",
        "/run/NetworkManager",
    ] {
        a.push(s("--ro-bind-try"));
        a.push(s(dir));
        a.push(s(dir));
    }
    // A projected PATH may name an operator-managed runtime outside the system
    // roots above (NVM, ~/.local/bin, etc.). Expose only those explicitly
    // allowlisted PATH roots read-only. NVM's bin entries symlink into the
    // sibling lib tree, so bind the complete version root.
    for root in projected_runtime_roots(input.env) {
        a.push(s("--ro-bind-try"));
        a.push(root.clone());
        a.push(root);
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
        a.push(sandbox_mount_destination(&m.dest));
    }
    // Environment is supplied through the wrapper process after `env_clear`, not
    // as bwrap argv. This keeps process-secret values out of `/proc/*/cmdline`.
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

fn projected_runtime_roots(env: &[(String, String)]) -> Vec<String> {
    let Some(path) = env
        .iter()
        .rev()
        .find_map(|(key, value)| (key == "PATH").then_some(value))
    else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for entry in std::env::split_paths(path) {
        if !entry.is_absolute()
            || entry.starts_with("/usr")
            || entry.starts_with("/bin")
            || entry.starts_with("/sbin")
        {
            continue;
        }
        let is_bin = entry.file_name().is_some_and(|name| name == "bin");
        let path = entry.to_string_lossy();
        let root = if let Some((prefix, _)) = path.split_once("/.local/share/uv/python/") {
            // uv virtualenv interpreters use absolute links through a moving
            // version alias (for example `cpython-3.11-linux-*`) that resolves to
            // a concrete patch directory. Project the uv Python directory so both
            // the literal alias and its target exist inside the namespace.
            PathBuf::from(format!("{prefix}/.local/share/uv/python"))
        } else if is_bin && (path.contains("/venv/") || path.ends_with("/venv/bin")) {
            // Editable Python installs resolve modules beside the virtualenv
            // (Hermes installs `hermes_cli` this way). The application root is the
            // virtualenv's parent, so mounting only `venv/` starts Python but loses
            // the selected CLI's package.
            entry
                .parent()
                .and_then(std::path::Path::parent)
                .unwrap_or(&entry)
                .to_path_buf()
        } else if is_bin && path.contains("/.nvm/versions/node/") {
            entry.parent().unwrap_or(&entry).to_path_buf()
        } else {
            entry
        };
        let root = root.to_string_lossy().into_owned();
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

fn seatbelt_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn seatbelt_path_filters(path: &std::path::Path) -> String {
    let quoted = seatbelt_string(&path.to_string_lossy());
    format!("(literal {quoted})(subpath {quoted})")
}

/// Render a macOS `sandbox-exec` (Seatbelt) command line. The Apple system profile
/// supplies the minimum Mach/sysctl/runtime reads required to start ordinary macOS
/// binaries; user data remains deny-by-default. Workspace/output/mount permissions
/// and the on/off network policy are then added explicitly.
#[must_use]
pub fn sandbox_exec_argv(input: &RenderInput) -> Vec<String> {
    let mut readable = vec![
        input.host_workspace.to_path_buf(),
        input.host_outputs.to_path_buf(),
    ];
    readable.extend(input.mounts.iter().map(|mount| mount.host.clone()));
    let mut writable = vec![
        input.host_workspace.to_path_buf(),
        input.host_outputs.to_path_buf(),
    ];
    writable.extend(
        input
            .mounts
            .iter()
            .filter(|mount| !mount.read_only)
            .map(|mount| mount.host.clone()),
    );
    let readonly: Vec<_> = input
        .mounts
        .iter()
        .filter(|mount| mount.read_only)
        .map(|mount| mount.host.clone())
        .collect();

    let mut profile = String::from(
        "(version 1)(deny default)(import \"system.sb\")\
         (allow process-fork)(allow process-exec)\
         (allow file-read-metadata)\
         (allow file-read* (subpath \"/private/var/select\")\
          (subpath \"/opt/homebrew\") (subpath \"/usr/local\"))",
    );
    for path in readable {
        profile.push_str("(allow file-read* ");
        profile.push_str(&seatbelt_path_filters(&path));
        profile.push(')');
    }
    for path in writable {
        profile.push_str("(allow file-write* ");
        profile.push_str(&seatbelt_path_filters(&path));
        profile.push(')');
    }
    // A specific deny wins over the enclosing workspace write grant, preserving
    // declared read-only files/directories even when they live below /workspace.
    for path in readonly {
        profile.push_str("(deny file-write* ");
        profile.push_str(&seatbelt_path_filters(&path));
        profile.push(')');
    }
    if matches!(input.network, pc::NetworkPolicy::Unrestricted) {
        profile.push_str("(allow network*)");
    }
    let mut a = vec![s("sandbox-exec"), s("-p"), profile, s("--")];
    a.extend(input.argv.iter().cloned());
    a
}

/// Realizes [`NamespaceSandbox`] environments (bubblewrap tier).
pub struct NamespaceProvider {
    base: PathBuf,
    inherit_agent_stderr: bool,
    blobs: std::collections::HashMap<String, Vec<u8>>,
    file_store: Option<Arc<dyn pc::BlobSource>>,
    secret_broker: Arc<std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>>,
    /// Optional memory-store realizer. A FUSE-preferring mounter
    /// ([`MemoryStoreMounter::new`]) works on this tier: the store is FUSE-mounted on the
    /// host path that then binds into the bwrap namespace, so the agent reads/writes it
    /// LIVE (write-through) — the ADR-0053 item-2 splice, which survives bwrap's
    /// `--unshare-user` (the mount's owner uid is identity-mapped, so no `allow_other` is
    /// needed; proven by `namespace_provider::bwrap_splices_a_live_fuse_memory_mount…`).
    /// A copy-only mounter ([`MemoryStoreMounter::copy_only`]) is the fallback for a host
    /// without `/dev/fuse`: the store is materialized to files that bind in, harvested on
    /// dispose. The FUSE-preferring mounter degrades to copy automatically off `/dev/fuse`.
    memory_mounter: Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
}

impl NamespaceProvider {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            inherit_agent_stderr: false,
            blobs: std::collections::HashMap::new(),
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

    /// Realize `MemoryStore` mounts via an injected mounter. It uses live FUSE where
    /// the platform supports it and automatically copy+harvests otherwise. Without
    /// a mounter, a `MemoryStore` mount fails loud.
    #[must_use]
    pub fn with_memory_mounter(self, mounter: Arc<dyn pc::MemoryMounter>) -> Self {
        self.install_memory_mounter(mounter);
        self
    }

    pub fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>) {
        *self
            .memory_mounter
            .write()
            .expect("memory mounter lock poisoned") = Some(mounter);
    }

    /// Static capability evidence shared by provider admission and owners of an
    /// already-created namespace environment.
    pub fn capabilities() -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Namespace,
            tool_transparent: true,
            // Seatbelt is a policy boundary, not a mount namespace: it cannot make
            // host paths appear at Linux-style /workspace or /mnt paths. The launcher
            // translates cwd/known argv paths and exports real host paths instead.
            path_fidelity: !cfg!(target_os = "macos"),
            enforced_readonly: true,
            network_isolation: true,
            enforced_network_allowlist: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
            package_provisioning: false,
        }
    }

    /// Realize the mounts under `root`, all-or-nothing: any failure reaps the tree.
    /// Returns the bind layout, the realized refs, and any live memory-store guards
    /// (torn down / harvested at dispose).
    async fn realize_layout(
        &self,
        root: &IsolatedRoot,
        host_workspace: &std::path::Path,
        spec: &pc::SandboxSpec,
    ) -> Result<
        (
            Vec<RenderMount>,
            Vec<pc::RealizedMount>,
            Vec<Box<dyn pc::MemoryMount>>,
            Vec<PathBuf>,
        ),
        pc::SandboxError,
    > {
        let mut layout = Vec::new();
        let mut realized = Vec::new();
        let mut memory_mounts: Vec<Box<dyn pc::MemoryMount>> = Vec::new();
        // Host paths of realized secrets — shredded at dispose (ADR-0023).
        let mut secret_paths: Vec<PathBuf> = Vec::new();
        for req in &spec.mounts {
            let host = host_projection_path(root, host_workspace, &req.mount_path)?;
            if matches!(req.source, pc::MountSource::CacheVolume { .. }) {
                return Err(err(format!(
                    "mount {:?}: cache_volume is not supported by the local OS sandbox",
                    req.mount_id
                )));
            }
            // memory_store is a keyed store, not a byte blob (ADR-0038/0053): the mounter
            // realizes it at `host` (a live FUSE mount with a FUSE-preferring mounter, or
            // materialized files on the copy fallback) which then binds into the namespace
            // — live write-through FUSE-in-bwrap works (ADR-0053 item 2); copy harvests on
            // dispose.
            if let Some((rendered, mount, guard)) =
                realize_memory_mount(&self.memory_mounter, req, &host).await?
            {
                layout.push(rendered);
                realized.push(mount);
                memory_mounts.push(guard);
                continue;
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
                    "the Namespace provider cannot write back a brokered writable Secret mount",
                ));
            }
            let bytes = resolve_source(
                &req.source,
                &self.blobs,
                &self.file_store,
                secret_broker.as_ref(),
            )
            .await?;
            match &bytes {
                Some(bytes) => {
                    verify(&req.source, bytes)?; // fail closed on content-hash mismatch
                    if let Some(parent) = host.parent() {
                        std::fs::create_dir_all(parent).map_err(err)?;
                    }
                    std::fs::write(&host, bytes).map_err(err)?;
                    // A realized secret is owner-only on disk (0600) before it is
                    // bind-mounted; still shredded on dispose via `secret_paths`.
                    if matches!(req.source, pc::MountSource::Secret { .. }) {
                        restrict_to_owner(&host)?;
                    }
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
            if matches!(req.source, pc::MountSource::Secret { .. }) {
                secret_paths.push(host);
            }
        }
        Ok((layout, realized, memory_mounts, secret_paths))
    }
}

#[async_trait]
impl pc::SandboxProvider for NamespaceProvider {
    fn capabilities(&self) -> pc::SandboxCapabilities {
        Self::capabilities()
    }

    /// A real readiness probe of the OS-native isolator: bwrap must run an
    /// unprivileged user namespace here (Linux), or `sandbox-exec` must run a trivial
    /// Seatbelt profile (macOS). On a host that can't, this returns an error so
    /// `select_provider` fails closed at selection instead of deferring the failure
    /// to `create` (or, worse, launching an unisolated process).
    ///
    /// **Memoized per process**: the underlying tool's availability is a host
    /// property, so the throwaway probe runs at most once regardless of how many
    /// times / providers ask (a hot selection path never re-spawns it).
    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        let ok = *OS_SANDBOX_PROBE.get_or_init(run_os_native_probe).await;
        if ok {
            Ok(())
        } else {
            Err(err(
                "OS-native sandbox (bwrap userns / macOS Seatbelt) unavailable on this host",
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
        Ok(Box::new(self.adopt_sandbox(handle).await?))
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
        pc::prepare_environment(spec, &Self::capabilities()).map_err(err)?;
        // Neither bwrap nor the Seatbelt adapter can enforce a DNS-host allowlist;
        // refuse it rather than silently blocking all egress or allowing too much.
        if matches!(spec.network, pc::NetworkPolicy::Allowlist { .. }) {
            return Err(err(
                "local OS sandbox supports on/off egress only, not a host allowlist",
            ));
        }

        let base_env = spec.env.clone();

        let raw_root = crate::sandbox_dir(&self.base, &spec.scope);
        std::fs::create_dir_all(&raw_root).map_err(err)?;
        // `/var` is a symlink to `/private/var` on macOS. Seatbelt evaluates some
        // operations against the canonical vnode path, so build every rule/env/cwd
        // from one canonical root or write grants can miss their target.
        let root = IsolatedRoot::new(std::fs::canonicalize(&raw_root).map_err(err)?);
        let host_workspace = root.resolve("/workspace").map_err(err)?;
        std::fs::create_dir_all(&host_workspace).map_err(err)?;
        let host_outputs = root.resolve(&spec.outputs_path).map_err(err)?;
        std::fs::create_dir_all(&host_outputs).map_err(err)?;

        let (layout, realized, memory_mounts, secret_paths) =
            match self.realize_layout(&root, &host_workspace, spec).await {
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
            inherit_agent_stderr: self.inherit_agent_stderr,
            secret_broker: self.secret_broker.clone(),
            network: spec.network.clone(),
            layout: std::sync::RwLock::new(layout),
            realized,
            secret_paths,
            memory_mounts: std::sync::Mutex::new(memory_mounts),
            memory_mounter: self.memory_mounter.clone(),
        })
    }

    /// Re-open a namespace sandbox from its durable handle so companion
    /// capabilities operate on the Run's existing environment.
    pub async fn adopt_sandbox(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        // Older macOS handles were mislabeled as `bwrap`; accept them during
        // adoption so the provider-kind correction does not strand a persisted
        // Session environment.
        let compatible_provider = if cfg!(target_os = "macos") {
            matches!(handle.provider_kind.as_str(), "seatbelt" | "bwrap")
        } else {
            handle.provider_kind == "bwrap"
        };
        if !compatible_provider {
            return Err(err(format!(
                "namespace provider cannot adopt {:?} sandbox",
                handle.provider_kind
            )));
        }
        let outputs_path = handle
            .extra
            .as_ref()
            .and_then(|v| v.get("outputs_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("/mnt/session/outputs")
            .to_string();
        let raw_root = crate::sandbox_dir(&self.base, &handle.sandbox_id);
        let root = IsolatedRoot::new(std::fs::canonicalize(&raw_root).unwrap_or(raw_root));
        let host_workspace = root.resolve("/workspace").map_err(err)?;
        let host_outputs = root.resolve(&outputs_path).map_err(err)?;
        Ok(NamespaceSandbox {
            id: handle.sandbox_id.clone(),
            root,
            outputs_path,
            host_workspace,
            host_outputs,
            base_env: handle
                .extra
                .as_ref()
                .and_then(|value| value.get("base_env"))
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default(),
            inherit_agent_stderr: self.inherit_agent_stderr,
            secret_broker: self.secret_broker.clone(),
            network: pc::NetworkPolicy::Unrestricted,
            layout: std::sync::RwLock::new(Vec::new()),
            realized: Vec::new(),
            secret_paths: Vec::new(),
            memory_mounts: std::sync::Mutex::new(Vec::new()),
            memory_mounter: self.memory_mounter.clone(),
        })
    }
}

/// A realized OS-confined environment (bubblewrap on Linux, Seatbelt on macOS).
pub struct NamespaceSandbox {
    id: String,
    root: IsolatedRoot,
    outputs_path: String,
    host_workspace: PathBuf,
    host_outputs: PathBuf,
    base_env: Vec<pc::EnvVar>,
    inherit_agent_stderr: bool,
    secret_broker: Arc<std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>>,
    network: pc::NetworkPolicy,
    layout: std::sync::RwLock<Vec<RenderMount>>,
    realized: Vec<pc::RealizedMount>,
    /// Host paths of realized secrets, shredded at dispose before the tree is reaped
    /// (ADR-0023). Empty after an `adopt`.
    secret_paths: Vec<PathBuf>,
    /// Live memory-store mounts, harvested / unmounted at dispose before the tree is
    /// reaped. Empty after an `adopt` (a reconnected sandbox owns no fresh guards).
    memory_mounts: std::sync::Mutex<Vec<Box<dyn pc::MemoryMount>>>,
    memory_mounter: Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
}

impl NamespaceSandbox {
    fn workspace_root(&self) -> IsolatedRoot {
        IsolatedRoot::new(self.host_workspace.clone())
    }

    /// Materialize an immutable runtime-owned tree through the shared lexical jail.
    pub fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>, bool)],
    ) -> Result<(), pc::SandboxError> {
        materialize_read_only_tree_at(&self.workspace_root(), workspace_relative(subdir), files)
    }

    /// Tear down every live memory mount (harvest a copy / unmount a FUSE), draining
    /// the guard list so a later dispose is a no-op.
    pub async fn release_memory_mounts(&self) {
        let mounts: Vec<Box<dyn pc::MemoryMount>> =
            std::mem::take(&mut self.memory_mounts.lock().unwrap());
        for mount in mounts {
            mount.teardown().await;
        }
    }

    /// Remove only the runtime-owned resource projection while retaining the
    /// Session workspace and every non-resource file.
    pub fn clear_resource_projection(&self) -> Result<(), pc::SandboxError> {
        let projection = self.root.resolve(".mnt").map_err(err)?;
        match std::fs::symlink_metadata(&projection) {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
                std::fs::remove_file(projection).map_err(err)
            }
            Ok(_) => std::fs::remove_dir_all(projection).map_err(err),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(err(error)),
        }
    }

    /// Native hand tools bound to this same environment. Shell is always wrapped
    /// in bwrap; path tools remain rooted through the shared lexical jail.
    pub fn rooted_tools(&self) -> Vec<Arc<dyn awaken_runtime_contract::tool::RawTool>> {
        namespace_raw_tools(
            self.workspace_root(),
            matches!(self.network, pc::NetworkPolicy::None),
        )
    }

    pub fn provision_repo(
        &self,
        logical: &str,
        url: &str,
        initial_branch: Option<&str>,
        initial_commit: Option<&str>,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        provision_repo_at(
            &self.workspace_root(),
            workspace_relative(logical),
            url,
            initial_branch,
            initial_commit,
            credential,
        )
        .map_err(err)
    }

    pub fn push_repo(
        &self,
        logical: &str,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<bool, pc::SandboxError> {
        push_repo_at(
            &self.workspace_root(),
            workspace_relative(logical),
            credential,
        )
        .map_err(err)
    }

    pub fn list_files(&self, subdir: &str) -> Vec<(String, Vec<u8>)> {
        if subdir.starts_with('/') && !subdir.starts_with("/workspace") {
            list_files_at(&self.root, subdir.trim_start_matches('/'))
        } else {
            list_files_at(&self.workspace_root(), workspace_relative(subdir))
        }
    }

    pub fn scan_skill_dir(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        scan_skill_dir_at(&self.workspace_root(), workspace_relative(subdir))
    }

    pub fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        let path = jailed_at(&self.workspace_root(), workspace_relative(logical)).map_err(err)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        std::fs::write(&path, contents).map_err(err)?;
        restrict_to_owner(&path)
    }

    /// Remove one dynamically projected workspace path. Missing paths are an
    /// idempotent success and lexical traversal is rejected by the shared jail.
    pub fn remove_inline(&self, logical: &str) -> Result<(), pc::SandboxError> {
        let path = self
            .workspace_root()
            .resolve(workspace_relative(logical))
            .map_err(err)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path).map_err(err),
            Ok(_) => std::fs::remove_file(path).map_err(err),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(err(error)),
        }
    }

    /// Revoke a dynamically attached mount and its runtime-owned backing path.
    /// Non-mount workspace paths retain the ordinary lexical removal behavior.
    pub fn remove_mount(&self, logical: &str) -> Result<(), pc::SandboxError> {
        let removed = {
            let mut layout = self.layout.write().expect("namespace layout lock poisoned");
            let before = layout.len();
            layout.retain(|mount| mount.dest != logical);
            layout.len() != before
        };
        if !removed {
            return self.remove_inline(logical);
        }
        let path = host_projection_path(&self.root, &self.host_workspace, logical)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path).map_err(err),
            Ok(_) => std::fs::remove_file(path).map_err(err),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(err(error)),
        }
    }

    fn translate_macos_path(&self, value: &str) -> Option<String> {
        let mut mappings: Vec<(&str, &std::path::Path)> = vec![
            ("/workspace", &self.host_workspace),
            (&self.outputs_path, &self.host_outputs),
        ];
        let layout = self.layout.read().expect("namespace layout lock poisoned");
        mappings.extend(
            layout
                .iter()
                .map(|mount| (mount.dest.as_str(), mount.host.as_path())),
        );
        // Prefer the most-specific mount when paths overlap.
        mappings.sort_by_key(|(dest, _)| std::cmp::Reverse(dest.len()));
        for (dest, host) in mappings {
            if value == dest {
                return Some(host.to_string_lossy().into_owned());
            }
            if let Some(suffix) = value.strip_prefix(dest)
                && suffix.starts_with('/')
            {
                return Some(format!("{}{suffix}", host.to_string_lossy()));
            }
        }
        None
    }

    fn translate_macos_argument(&self, value: &str) -> String {
        if let Some(translated) = self.translate_macos_path(value) {
            return translated;
        }
        if let Some((key, path)) = value.split_once('=')
            && let Some(translated) = self.translate_macos_path(path)
        {
            return format!("{key}={translated}");
        }
        value.to_string()
    }

    async fn materialize_command(
        &self,
        command: pc::Command,
    ) -> Result<pc::MaterializedCommand, pc::SandboxError> {
        let broker = self
            .secret_broker
            .read()
            .expect("secret broker lock poisoned")
            .clone();
        pc::materialize_process_command(&self.base_env, command, broker.as_ref()).await
    }

    fn configure_command(
        &self,
        process: &mut TokioCommand,
        command: &pc::MaterializedCommand,
    ) -> Result<(), pc::SandboxError> {
        process.env_clear();
        if cfg!(target_os = "macos") {
            let cwd = if command.cwd.is_empty() {
                self.host_workspace.clone()
            } else if let Some(translated) = self.translate_macos_path(&command.cwd) {
                PathBuf::from(translated)
            } else {
                self.root.resolve(&command.cwd).map_err(err)?
            };
            process
                .current_dir(cwd)
                .env("AWAKEN_OUTPUTS_DIR", &self.host_outputs)
                .env("AWAKEN_PROJECT_DIR", &self.host_workspace);
        } else {
            process
                .env("AWAKEN_OUTPUTS_DIR", &self.outputs_path)
                .env("AWAKEN_PROJECT_DIR", "/workspace");
        }
        for var in &command.env {
            process.env(&var.name, var.value.expose());
        }
        Ok(())
    }

    /// The rendered launcher argv for `command` (bwrap or Seatbelt wrapping the program).
    fn render_argv(
        &self,
        command: &pc::MaterializedCommand,
    ) -> Result<Vec<String>, pc::SandboxError> {
        if command.argv.is_empty() {
            return Err(err("command argv is empty"));
        }
        // Only PATH is needed by the pure renderer to expose an explicitly
        // selected runtime root. Secret values never enter renderer/argv data.
        let path_env: Vec<(String, String)> = command
            .env
            .iter()
            .filter(|var| var.name == "PATH" && !var.value.is_secret())
            .map(|var| (var.name.clone(), var.value.expose().to_string()))
            .collect();
        let macos_argv: Vec<String> = command
            .argv
            .iter()
            .map(|arg| self.translate_macos_argument(arg))
            .collect();
        let layout = self.layout.read().expect("namespace layout lock poisoned");
        let input = RenderInput {
            host_workspace: &self.host_workspace,
            host_outputs: &self.host_outputs,
            outputs_path: &self.outputs_path,
            mounts: &layout,
            env: &path_env,
            network: &self.network,
            cwd: &command.cwd,
            argv: if cfg!(target_os = "macos") {
                &macos_argv
            } else {
                &command.argv
            },
        };
        // OS-native launcher: bubblewrap on Linux, Seatbelt (`sandbox-exec`) on macOS.
        // `cfg!` keeps both branches type-checked on every target; only the matching
        // one is live. Both renderers take the same `RenderInput`.
        let argv = if cfg!(target_os = "macos") {
            sandbox_exec_argv(&input)
        } else {
            bubblewrap_argv(&input)
        };
        Ok(argv)
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
        let command = self.materialize_command(command).await?;
        let argv = self.render_argv(&command)?;
        let mut cmd = TokioCommand::new(&argv[0]);
        awaken_local_process::configure_process_group(&mut cmd);
        cmd.args(&argv[1..]);
        self.configure_command(&mut cmd, &command)?;
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
}

#[async_trait]
impl pc::Sandbox for NamespaceSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        let provider_kind = if cfg!(target_os = "macos") {
            "seatbelt"
        } else {
            "bwrap"
        };
        let mut h = pc::SandboxHandle::new(provider_kind, &self.id);
        h.extra = Some(json!({
            "outputs_path": self.outputs_path,
            "base_env": self.base_env,
        }));
        h
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        let stdio = command.stdio;
        let command = self.materialize_command(command).await?;
        let argv = self.render_argv(&command)?;
        let mut cmd = TokioCommand::new(&argv[0]);
        awaken_local_process::configure_process_group(&mut cmd);
        cmd.args(&argv[1..]);
        self.configure_command(&mut cmd, &command)?;
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
        req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        // Dynamic mount decision table:
        // Inline bytes + RO/RW -> materialize and add one bind;
        // any source requiring an external resolver -> reject without layout change.
        pc::validate_mount_requirements(
            std::slice::from_ref(&req),
            &NamespaceProvider::capabilities(),
        )
        .map_err(err)?;
        let host = host_projection_path(&self.root, &self.host_workspace, &req.mount_path)?;
        if let Some((rendered, realized, guard)) =
            realize_memory_mount(&self.memory_mounter, &req, &host).await?
        {
            let mut layout = self.layout.write().expect("namespace layout lock poisoned");
            layout.retain(|mount| mount.dest != req.mount_path);
            layout.push(rendered);
            self.memory_mounts.lock().unwrap().push(guard);
            return Ok(realized);
        }
        let (contents, content_hash) = match &req.source {
            pc::MountSource::Inline { contents } => (contents.as_bytes(), None),
            pc::MountSource::InlineBytes {
                contents,
                content_hash,
            } => (contents.as_slice(), content_hash.clone()),
            _ => {
                return Err(err(format!(
                    "runtime attach for mount {:?} requires a provider-owned resolver",
                    req.mount_id
                )));
            }
        };
        verify(&req.source, contents)?;
        if let Some(parent) = host.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        std::fs::write(&host, contents).map_err(err)?;
        restrict_to_owner(&host)?;
        let mut layout = self.layout.write().expect("namespace layout lock poisoned");
        layout.retain(|mount| mount.dest != req.mount_path);
        layout.push(RenderMount {
            host,
            dest: req.mount_path.clone(),
            read_only: req.access == pc::MountAccess::ReadOnly,
        });
        Ok(pc::RealizedMount {
            mount_id: req.mount_id,
            mount_path: req.mount_path,
            access: req.access,
            realization: pc::Realization::Bind,
            content_hash,
        })
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
        // Order: harvest memory (reads edits back) → shred secrets → reap the tree, so
        // a promised memory write-back is never lost and no credential lingers on disk.
        self.release_memory_mounts().await;
        for path in &self.secret_paths {
            if let Ok(meta) = std::fs::metadata(path) {
                let _ = std::fs::write(path, vec![0u8; meta.len() as usize]);
            }
        }
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

    struct Broker;

    #[async_trait]
    impl pc::SecretBroker for Broker {
        async fn materialize(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            assert_eq!(reference, "broker://namespace");
            Ok(b"namespace-secret".to_vec())
        }

        async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Err(pc::SandboxError::new("process secrets are not supported"))
        }

        async fn write_back(
            &self,
            _reference: &str,
            _bytes: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            unreachable!("the Namespace provider accepts only read-only brokered secrets")
        }
    }

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

    fn ns_spec(scope: &str, mounts: Vec<pc::MountRequirement>) -> pc::SandboxSpec {
        pc::SandboxSpec {
            scope: scope.into(),
            isolation: pc::IsolationClass::Namespace,
            mounts,
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: "/mnt/session/outputs".into(),
            limits: Default::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[tokio::test]
    async fn create_sandbox_realizes_a_resolvable_and_an_optional_unresolvable_mount() {
        use pc::Sandbox;
        let tmp = tempfile::tempdir().unwrap();
        let provider = NamespaceProvider::new(tmp.path()).with_blob("blob-x", b"payload".to_vec());
        let spec = ns_spec(
            "t-ns-mounts",
            vec![
                pc::MountRequirement {
                    mount_id: "data".into(),
                    source: pc::MountSource::File {
                        file_id: "blob-x".into(),
                        content_hash: None,
                    },
                    mount_path: "/workspace/deep/data.bin".into(),
                    access: pc::MountAccess::ReadOnly,
                    lifetime: pc::MountLifetime::PerRun,
                    required: true,
                },
                pc::MountRequirement {
                    mount_id: "opt".into(),
                    source: pc::MountSource::File {
                        file_id: "absent".into(),
                        content_hash: None,
                    },
                    mount_path: "/workspace/deep2/opt.bin".into(),
                    access: pc::MountAccess::ReadOnly,
                    lifetime: pc::MountLifetime::PerRun,
                    required: false,
                },
            ],
        );
        let sandbox = provider.create_sandbox(&spec).await.unwrap();
        assert_eq!(sandbox.realized().len(), 2);
    }

    #[tokio::test]
    async fn a_read_only_secret_is_materialized_by_the_dedicated_broker() {
        use pc::Sandbox;
        let tmp = tempfile::tempdir().unwrap();
        let provider = NamespaceProvider::new(tmp.path()).with_secret_broker(Arc::new(Broker));
        let spec = ns_spec(
            "t-ns-secret",
            vec![pc::MountRequirement {
                mount_id: "auth".into(),
                source: pc::MountSource::Secret {
                    reference: "broker://namespace".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/.auth".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            }],
        );

        let sandbox = provider.create_sandbox(&spec).await.unwrap();
        assert_eq!(sandbox.realized().len(), 1);
        assert_eq!(
            std::fs::read(&sandbox.secret_paths[0]).unwrap(),
            b"namespace-secret"
        );
    }

    #[tokio::test]
    async fn a_memory_store_mount_without_a_mounter_fails_loud() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = NamespaceProvider::new(tmp.path());
        let spec = ns_spec(
            "t-ns-mem",
            vec![pc::MountRequirement {
                mount_id: "mem".into(),
                source: pc::MountSource::MemoryStore {
                    store_id: "s1".into(),
                    materialization_reference: None,
                    write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
                },
                mount_path: "/workspace/mem".into(),
                access: pc::MountAccess::ReadWrite,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            }],
        );
        assert!(provider.create_sandbox(&spec).await.is_err());
    }

    #[tokio::test]
    async fn namespace_lifecycle_helpers_cover_adoption_and_projection_boundaries() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = NamespaceProvider::new(tmp.path());
        let sandbox = provider
            .create_sandbox(&ns_spec("t-ns-lifecycle", Vec::new()))
            .await
            .unwrap();

        let wrong = pc::SandboxHandle::new("local", "t-ns-lifecycle");
        assert!(provider.adopt_sandbox(&wrong).await.is_err());

        sandbox
            .materialize_inline("nested/value.txt", b"value")
            .unwrap();
        assert_eq!(
            sandbox.list_files("nested"),
            vec![("value.txt".to_string(), b"value".to_vec())]
        );
        sandbox.remove_inline("nested").unwrap();
        sandbox.remove_inline("nested").unwrap();

        let outputs = sandbox.root.resolve("/outputs").unwrap();
        std::fs::create_dir_all(&outputs).unwrap();
        std::fs::write(outputs.join("result.txt"), b"result").unwrap();
        assert_eq!(
            sandbox.list_files("/outputs"),
            vec![("result.txt".to_string(), b"result".to_vec())]
        );

        let projection = sandbox.root.resolve(".mnt").unwrap();
        std::fs::create_dir_all(projection.join("resource")).unwrap();
        sandbox.clear_resource_projection().unwrap();
        sandbox.clear_resource_projection().unwrap();

        std::fs::write(&projection, b"stale").unwrap();
        sandbox.clear_resource_projection().unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outputs.join("result.txt"), &projection).unwrap();
            sandbox.clear_resource_projection().unwrap();
        }
    }

    /// Live attach cause/effect decision table:
    /// | source | path | access | effect |
    /// |---|---|---|---|
    /// | InlineBytes | absolute | read-only | one runtime-owned file + RO bind |
    /// | unresolved external source | any | any | reject, layout unchanged |
    /// Detach removes both the bind entry and backing file, so a later process
    /// cannot observe a stale mount.
    #[tokio::test]
    async fn live_inline_mount_updates_and_revokes_the_namespace_bind_layout() {
        use pc::Sandbox;

        let tmp = tempfile::tempdir().unwrap();
        let sandbox = NamespaceProvider::new(tmp.path())
            .create_sandbox(&ns_spec("t-ns-live-mount", Vec::new()))
            .await
            .unwrap();
        let mount_path = "/mnt/session/uploads/workspace/live.txt";
        let realized = sandbox
            .attach(pc::MountRequirement {
                mount_id: "file_live".into(),
                source: pc::MountSource::InlineBytes {
                    contents: b"live".to_vec(),
                    content_hash: None,
                },
                mount_path: mount_path.into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            })
            .await
            .unwrap();
        assert_eq!(realized.mount_path, mount_path);
        assert!(
            sandbox
                .layout
                .read()
                .unwrap()
                .iter()
                .any(|mount| mount.dest == mount_path && mount.read_only)
        );
        let backing = sandbox.root.resolve(mount_path).unwrap();
        assert_eq!(std::fs::read(&backing).unwrap(), b"live");

        let before = sandbox.layout.read().unwrap().len();
        assert!(
            sandbox
                .attach(pc::MountRequirement {
                    mount_id: "external".into(),
                    source: pc::MountSource::File {
                        file_id: "unresolved".into(),
                        content_hash: None,
                    },
                    mount_path: "/mnt/session/uploads/external".into(),
                    access: pc::MountAccess::ReadOnly,
                    lifetime: pc::MountLifetime::PerRun,
                    required: true,
                })
                .await
                .is_err()
        );
        assert_eq!(sandbox.layout.read().unwrap().len(), before);

        sandbox.remove_mount(mount_path).unwrap();
        assert!(!backing.exists());
        assert!(sandbox.layout.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn native_tools_and_runtime_projections_share_the_workspace_root() {
        // Cause-effect graph: C1=runtime projects a workspace-relative file;
        // C2=Native path tool reads the same relative path; C3=a same-named path
        // does not exist at the outer namespace root. E1=projected bytes are
        // readable; E2=the outer root cannot become a competing tool workspace.
        //
        // | Rule | C1 | C2 | C3 | Effects |
        // | W1   | yes | yes | yes | E1,E2 |
        let tmp = tempfile::tempdir().unwrap();
        let provider = NamespaceProvider::new(tmp.path());
        let sandbox = provider
            .create_sandbox(&ns_spec("t-ns-tool-workspace", Vec::new()))
            .await
            .unwrap();
        sandbox
            .materialize_inline(".awaken/tool-results/result.txt", b"complete")
            .unwrap();

        let read = sandbox
            .rooted_tools()
            .into_iter()
            .find(|tool| tool.id() == "read")
            .expect("read tool");
        let output = read
            .invoke(awaken_runtime_contract::llm::ToolCall {
                call_id: "read-projection".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({
                    "path": ".awaken/tool-results/result.txt"
                }),
            })
            .await
            .unwrap();

        assert!(output.text().contains("complete"), "W1/E1");
        assert!(
            !sandbox
                .root
                .resolve(".awaken/tool-results/result.txt")
                .unwrap()
                .exists(),
            "W1/E2"
        );
    }

    async fn bwrap_usable() -> bool {
        tokio::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn spawn_agent_launches_an_opaque_process_confined_by_bwrap() {
        if !bwrap_usable().await {
            eprintln!("skipping: no usable bwrap / user namespaces");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let provider = NamespaceProvider::new(tmp.path());
        let sandbox = provider
            .create_sandbox(&ns_spec("t-ns-spawn", Vec::new()))
            .await
            .unwrap();
        let (proc, _channel) = sandbox
            .spawn_agent(pc::Command::new(["true"]))
            .await
            .unwrap();
        assert!(!proc.id().is_empty());
        assert_eq!(proc.wait().await.unwrap().code, Some(0));
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
        assert!(
            !joined.contains("--setenv"),
            "environment is injected through the cleared wrapper env, never argv"
        );
        assert!(joined.contains("--chdir /workspace"));
        // program follows the -- separator, in order
        let sep = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(&a[sep + 1..], &["claude", "--acp"]);
        // unrestricted net => no --unshare-net
        assert!(!a.iter().any(|x| x == "--unshare-net"));
        assert!(
            a.windows(3).any(|window| {
                window
                    == [
                        "--ro-bind-try",
                        "/run/systemd/resolve",
                        "/run/systemd/resolve",
                    ]
            }),
            "systemd-resolved's resolv.conf target is visible"
        );
        assert!(
            a.windows(3).any(|window| {
                window
                    == [
                        "--ro-bind-try",
                        "/run/NetworkManager",
                        "/run/NetworkManager",
                    ]
            }),
            "NetworkManager's resolv.conf target is visible"
        );
    }

    #[test]
    fn bubblewrap_projects_explicit_non_system_path_runtimes_read_only() {
        let ws = PathBuf::from("/host/ws");
        let out = PathBuf::from("/host/out");
        let argv = vec![s("npx"), s("agent")];
        let env = vec![(
            "PATH".to_string(),
            "/home/u/.nvm/versions/node/v22.22.0/bin:/home/u/.local/bin:/usr/bin".to_string(),
        )];
        let rendered = bubblewrap_argv(&input(
            &ws,
            &out,
            &[],
            &env,
            &pc::NetworkPolicy::Unrestricted,
            &argv,
        ));
        let joined = rendered.join(" ");
        assert!(joined.contains(
            "--ro-bind-try /home/u/.nvm/versions/node/v22.22.0 \
             /home/u/.nvm/versions/node/v22.22.0"
        ));
        assert!(joined.contains("--ro-bind-try /home/u/.local/bin /home/u/.local/bin"));
        assert_eq!(
            projected_runtime_roots(&env),
            vec![
                "/home/u/.nvm/versions/node/v22.22.0".to_string(),
                "/home/u/.local/bin".to_string(),
            ]
        );
        assert!(
            !rendered.iter().any(|value| value == "--clearenv"),
            "the Tokio wrapper clears and rebuilds env before bwrap"
        );
    }

    #[test]
    fn bubblewrap_projects_a_python_virtualenv_root_for_symlinked_clis() {
        let env = vec![(
            "PATH".to_string(),
            "/home/u/.hermes/hermes-agent/venv/bin:\
             /home/u/.local/share/uv/python/cpython-3.11/bin:\
             /home/u/.local/bin:/usr/bin"
                .to_string(),
        )];
        assert_eq!(
            projected_runtime_roots(&env),
            vec![
                "/home/u/.hermes/hermes-agent".to_string(),
                "/home/u/.local/share/uv/python".to_string(),
                "/home/u/.local/bin".to_string(),
            ]
        );
    }

    #[test]
    fn bubblewrap_chdirs_into_a_custom_cwd_when_the_command_sets_one() {
        // The cwd decision branch: an empty cwd renders `--chdir /workspace` (covered
        // elsewhere); a non-empty sandbox-absolute cwd must render `--chdir <cwd>` so a
        // launched process starts in the directory the command asked for.
        let ws = PathBuf::from("/w");
        let out = PathBuf::from("/o");
        let argv = vec![s("true")];
        let mut inp = input(&ws, &out, &[], &[], &pc::NetworkPolicy::Unrestricted, &argv);
        inp.cwd = "/workspace/sub";
        let a = bubblewrap_argv(&inp);
        // The chdir target is the custom cwd, not the /workspace default.
        let pos = a.iter().position(|x| x == "--chdir").unwrap();
        assert_eq!(a[pos + 1], "/workspace/sub");
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
                dest: ".mnt/data".into(),
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
        assert!(j.contains("--bind /h/rw /workspace/.mnt/data"));
        assert!(!j.contains("UTC"), "environment values stay out of argv");
    }

    #[test]
    fn relative_and_workspace_prefixed_projections_share_the_workspace_root() {
        let root_dir = tempfile::tempdir().unwrap();
        let root = IsolatedRoot::new(root_dir.path());
        let workspace = root_dir.path().join("workspace");
        assert_eq!(
            host_projection_path(&root, &workspace, ".mnt/notes").unwrap(),
            workspace.join(".mnt/notes")
        );
        assert_eq!(
            host_projection_path(&root, &workspace, "/workspace/repo").unwrap(),
            workspace.join("repo")
        );
        assert_eq!(workspace_relative("workspace/repo"), "repo");
        assert_eq!(
            host_projection_path(&root, &workspace, "/outputs/result").unwrap(),
            root_dir.path().join("outputs/result")
        );
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
        assert!(a[2].contains("(import \"system.sb\")"));
        assert!(a[2].contains("/w"));
        assert!(a[2].contains("(allow network*)"));
        assert_eq!(a.last().unwrap(), "claude");
    }

    #[test]
    fn sandbox_exec_renders_mount_permissions_none_network_and_escaped_paths() {
        let ws = PathBuf::from("/host/w\"s");
        let out = PathBuf::from("/host/out");
        let mounts = vec![
            RenderMount {
                host: PathBuf::from("/host/w\"s/readonly"),
                dest: "/workspace/readonly".into(),
                read_only: true,
            },
            RenderMount {
                host: PathBuf::from("/host/rw"),
                dest: "/data".into(),
                read_only: false,
            },
        ];
        let argv = vec![s("true")];
        let rendered = sandbox_exec_argv(&input(
            &ws,
            &out,
            &mounts,
            &[],
            &pc::NetworkPolicy::None,
            &argv,
        ));
        let profile = &rendered[2];
        assert!(profile.contains("/host/w\\\"s"));
        assert!(profile.contains("(deny file-write*"));
        assert!(profile.contains("/host/rw"));
        assert!(!profile.contains("(allow network*)"));
    }
}
