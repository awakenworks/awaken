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
use awaken_sandbox_control::{SANDBOX_CONTROL_DIRECTORY_PATH, SandboxControlServiceKind};
use tokio::process::Command as TokioCommand;

use std::sync::Arc;

use crate::provider::{resolve_source, verify};
use crate::read_only_tree::materialize_read_only_tree_at;
use crate::{
    DiscoveredSkillFile, IsolatedRoot, content_fingerprint, list_files_at, provision_repo_at,
    push_repo_to_at, scan_skill_dir_at,
};

mod control;
mod seatbelt;
use control::{
    NamespaceControlPublicationRegistry, private_control_host_directory, private_control_mount,
};
#[cfg(test)]
use seatbelt::projected_runtime_roots;
use seatbelt::projected_runtime_roots_for_command;
pub use seatbelt::sandbox_exec_argv;

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

/// Process-global memo of the OS-native isolator's availability (the tool's presence
/// is a host property, so the throwaway probe runs at most once — see `probe_ready`).
static OS_SANDBOX_PROBE: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

/// Run the actual throwaway isolation probe: bwrap (Linux) / `sandbox-exec` (macOS)
/// must run a trivial confined `true` here.
async fn run_os_native_probe() -> bool {
    #[cfg(target_os = "macos")]
    let argv = {
        sandbox_exec_argv(&RenderInput {
            host_workspace: std::path::Path::new("/private/var/empty"),
            host_outputs: std::path::Path::new("/private/var/empty"),
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT,
            mounts: &[],
            env: &[],
            network: &pc::NetworkPolicy::None,
            cwd: "",
            argv: &["/usr/bin/true".to_string()],
        })
    };
    #[cfg(target_os = "linux")]
    let argv: Vec<String> = {
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
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
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// One realized bind for the launcher: a host path exposed at a sandbox path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderMount {
    pub host: PathBuf,
    pub dest: String,
    pub read_only: bool,
    pub boundary: RenderMountBoundary,
}

/// Namespace policy owned by the mount's domain, kept separate from byte access.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RenderMountBoundary {
    #[default]
    General,
    /// A MemoryStore child bind whose `/mnt/memory` parent must remain read-only.
    ManagedMemoryStore,
    /// Runtime-owned rendezvous projected read-only into the workload.
    PrivateRendezvous,
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
        pc::WorkspaceLayout::child(dest)
    }
}

fn workspace_relative(logical: &str) -> &str {
    pc::WorkspaceLayout::relative(logical)
        .or_else(|| logical.strip_prefix("workspace/"))
        .or_else(|| (logical == "workspace").then_some(""))
        .unwrap_or_else(|| logical.trim_start_matches('/'))
}

fn host_projection_path(
    root: &IsolatedRoot,
    host_workspace: &std::path::Path,
    logical: &str,
) -> Result<PathBuf, pc::SandboxError> {
    if !logical.starts_with('/') || pc::WorkspaceLayout::contains(logical) {
        IsolatedRoot::new(host_workspace)
            .resolve(workspace_relative(logical))
            .map_err(err)
    } else {
        root.resolve(logical).map_err(err)
    }
}

fn replay_render_layout(
    root: &IsolatedRoot,
    host_workspace: &std::path::Path,
    spec: &pc::SandboxSpec,
) -> Result<Vec<RenderMount>, pc::SandboxError> {
    let mut layout = spec
        .mounts
        .iter()
        .map(|mount| {
            Ok(RenderMount {
                host: host_projection_path(root, host_workspace, &mount.mount_path)?,
                dest: mount.mount_path.clone(),
                read_only: mount.access == pc::MountAccess::ReadOnly,
                boundary: if matches!(mount.source, pc::MountSource::MemoryStore { .. }) {
                    RenderMountBoundary::ManagedMemoryStore
                } else {
                    RenderMountBoundary::General
                },
            })
        })
        .collect::<Result<Vec<_>, pc::SandboxError>>()?;
    if let Some(directory) = control_directory_for(root, &spec.control_services)? {
        layout.push(private_control_mount(directory));
    }
    Ok(layout)
}

fn control_directory_for(
    root: &IsolatedRoot,
    control_services: &std::collections::BTreeSet<SandboxControlServiceKind>,
) -> Result<Option<PathBuf>, pc::SandboxError> {
    if control_services.contains(&SandboxControlServiceKind::RepositoryGitCredential) {
        private_control_host_directory(root).map(Some)
    } else {
        Ok(None)
    }
}

#[path = "namespace/memory_mount.rs"]
mod memory_mount;
use memory_mount::realize_memory_mount;

/// Render a `bwrap` command line (unprivileged, Linux). Deterministic and pure.
/// Layout: unshare namespaces, mount `/proc` `/dev` `/tmp`, read-only-bind the
/// host userland (so interpreters exist), bind the workspace and outputs, bind
/// each declared mount (ro/rw), inject reserved + user env, `--chdir`, then `--`
/// and the program argv.
#[must_use]
pub fn bubblewrap_argv(input: &RenderInput) -> Vec<String> {
    bubblewrap_argv_for(input, true)
}

fn bubblewrap_argv_for(input: &RenderInput, isolate_process: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![s("bwrap")];
    if isolate_process {
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
    }
    match input.network {
        pc::NetworkPolicy::Unrestricted => {} // share the host network namespace
        _ => a.push(s("--unshare-net")),
    }
    a.extend(["--die-with-parent", "--new-session"].into_iter().map(s));
    a.extend(["--proc", "/proc"].into_iter().map(s));
    a.extend(["--dev", "/dev"].into_iter().map(s));
    a.extend(["--tmpfs", "/tmp"].into_iter().map(s));
    let managed_memory_destinations = input
        .mounts
        .iter()
        .filter(|mount| mount.boundary == RenderMountBoundary::ManagedMemoryStore)
        .map(|mount| sandbox_mount_destination(&mount.dest))
        .filter(|dest| dest.starts_with("/mnt/memory/"))
        .collect::<Vec<_>>();
    if !managed_memory_destinations.is_empty() {
        // Managed Agents reserves the parent as a read-only directory while
        // each named Store bind independently carries its declared RO/RW mode.
        // Create every bind target before sealing the parent; later child bind
        // mounts may still be writable without widening the parent directory.
        a.extend([s("--dir"), s("/mnt")]);
        // `--remount-ro` requires a mount point, so the reserved parent is an
        // empty tmpfs rather than a directory on bubblewrap's root mount.
        a.extend([s("--tmpfs"), s("/mnt/memory")]);
        let mut directories = std::collections::BTreeSet::new();
        for destination in &managed_memory_destinations {
            let mut current = String::new();
            for component in destination.trim_start_matches('/').split('/') {
                current.push('/');
                current.push_str(component);
                if current.starts_with("/mnt/memory/") {
                    directories.insert(current.clone());
                }
            }
        }
        for directory in directories {
            a.extend([s("--dir"), directory]);
        }
        a.extend([s("--remount-ro"), s("/mnt/memory")]);
    }
    if input
        .mounts
        .iter()
        .any(|mount| mount.boundary == RenderMountBoundary::PrivateRendezvous)
    {
        a.extend([s("--dir"), s("/run")]);
        a.extend([s("--dir"), s("/run/awaken")]);
        a.extend([s("--dir"), s(SANDBOX_CONTROL_DIRECTORY_PATH)]);
    }
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
    for root in projected_runtime_roots_for_command(input.env, input.argv) {
        a.push(s("--ro-bind-try"));
        a.push(root.clone());
        a.push(root);
    }
    a.push(s("--bind"));
    a.push(input.host_workspace.to_string_lossy().into_owned());
    a.push(s(pc::WorkspaceLayout::ROOT));
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
        pc::WorkspaceLayout::ROOT
    } else {
        input.cwd
    }));
    a.push(s("--"));
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
    /// The durable handle discriminator owned by this platform's namespace
    /// adapter. Runtime-side adoption preflight consumes this same classifier,
    /// so handle validation cannot drift from the provider that later opens it.
    #[must_use]
    pub fn provider_kind() -> pc::NamespaceProviderKind {
        if cfg!(target_os = "macos") {
            pc::NamespaceProviderKind::Seatbelt
        } else {
            pc::NamespaceProviderKind::Bubblewrap
        }
    }

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
            path_fidelity: cfg!(target_os = "linux"),
            enforced_readonly: true,
            network_isolation: true,
            enforced_network_allowlist: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
            package_provisioning: false,
            control_services: if cfg!(target_os = "linux") {
                std::collections::BTreeSet::from([
                    SandboxControlServiceKind::RepositoryGitCredential,
                ])
            } else {
                Default::default()
            },
        }
    }

    /// Realize the mounts under `root`, all-or-nothing: any failure reaps the tree.
    /// The caller retains every acquired Memory guard immediately, including a
    /// guard whose post-mount validation fails, so compensation cannot consume
    /// or silently discard a failed teardown.
    async fn realize_layout(
        &self,
        root: &IsolatedRoot,
        host_workspace: &std::path::Path,
        spec: &pc::SandboxSpec,
        memory_mounts: &mut Vec<Box<dyn pc::MemoryMount>>,
        memory_materializations: &mut Vec<pc::MemoryMaterializationEvidence>,
        mutation_guard: &crate::realization_marker::ProviderCreationGuard,
    ) -> Result<(Vec<RenderMount>, Vec<pc::RealizedMount>), pc::SandboxError> {
        let root_identity = mutation_guard
            .root_identity()?
            .ok_or_else(|| err("namespace layout has no exact admitted root identity"))?;
        let mut layout = Vec::new();
        let mut realized = Vec::new();
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
            mutation_guard.validate_before_mutation()?;
            if let Some((rendered, mount, materialization)) =
                realize_memory_mount(&self.memory_mounter, req, &host, memory_mounts).await?
            {
                layout.push(rendered);
                realized.push(mount);
                if let Some(materialization) = materialization {
                    memory_materializations.push(materialization);
                }
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
                    let relative = host
                        .strip_prefix(root.root())
                        .map_err(|_| err("namespace mount escaped its exact sandbox root"))?;
                    mutation_guard.validate_before_mutation()?;
                    awaken_sandbox_fs::write_relative_file_atomic(
                        root.root(),
                        root_identity,
                        relative,
                        bytes,
                        0o600,
                    )
                    .map_err(err)?;
                }
                None if req.required => {
                    return Err(err(format!(
                        "required mount {:?} has no resolvable source",
                        req.mount_id
                    )));
                }
                None => {
                    let relative = host
                        .strip_prefix(root.root())
                        .map_err(|_| err("namespace mount escaped its exact sandbox root"))?;
                    mutation_guard.validate_before_mutation()?;
                    awaken_sandbox_fs::write_relative_file_atomic(
                        root.root(),
                        root_identity,
                        relative,
                        b"",
                        0o600,
                    )
                    .map_err(err)?;
                }
            }
            layout.push(RenderMount {
                host: host.clone(),
                dest: req.mount_path.clone(),
                read_only: req.access == pc::MountAccess::ReadOnly,
                boundary: RenderMountBoundary::General,
            });
            realized.push(pc::RealizedMount {
                mount_id: req.mount_id.clone(),
                mount_path: req.mount_path.clone(),
                access: req.access,
                realization: pc::Realization::Bind,
                content_hash: bytes.as_ref().map(|b| content_fingerprint(b)),
            });
        }
        Ok((layout, realized))
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
                if cfg!(any(target_os = "linux", target_os = "macos")) {
                    "OS-native sandbox (bwrap userns / macOS Seatbelt) unavailable on this host"
                } else {
                    "OS-native Namespace sandbox is unsupported on this platform; use sandbox_tier=local or a container-backed Worker"
                },
            ))
        }
    }

    async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.create_sandbox(spec).await?))
    }

    async fn create_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(
            self.create_sandbox_for_effect(spec, effect_fence, None)
                .await?,
        ))
    }

    async fn observe(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        handle.namespace_payload(Self::provider_kind())?;
        crate::realization_marker::observe_adoption(
            &crate::sandbox_dir(&self.base, &handle.sandbox_id),
            handle.realization_fingerprint(),
            handle.filesystem_effect_fence()?,
            handle.filesystem_physical_incarnation()?,
            None,
        )
    }

    async fn observe_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        handle.namespace_payload(Self::provider_kind())?;
        if handle.realization_fingerprint()
            != Some(&pc::SandboxRealizationFingerprint::from_spec(spec))
        {
            return Ok(pc::SandboxObservation::Incompatible {
                reason: "Namespace sandbox handle does not match the effective observation spec"
                    .into(),
            });
        }
        crate::realization_marker::observe_adoption(
            &crate::sandbox_dir(&self.base, &handle.sandbox_id),
            handle.realization_fingerprint(),
            handle.filesystem_effect_fence()?,
            handle.filesystem_physical_incarnation()?,
            Some(effect_fence),
        )
    }

    async fn adopt(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(self.adopt_sandbox(handle).await?))
    }

    async fn adopt_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Box<dyn pc::Sandbox>, pc::SandboxError> {
        Ok(Box::new(
            self.adopt_sandbox_for_effect(spec, handle, effect_fence)
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
        Ok(self
            .prepare_terminal_sandbox_for_effect(
                spec,
                handle,
                expected_effect_fence,
                terminal_effect_fence,
            )?
            .map(|sandbox| Box::new(sandbox) as Box<dyn pc::Sandbox>))
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
        self.create_sandbox_inner(spec, None, None).await
    }

    pub async fn create_sandbox_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: &pc::SandboxEffectFence,
        source_handle: Option<&pc::SandboxHandle>,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        if let Some(handle) = source_handle {
            handle.namespace_payload(Self::provider_kind())?;
        }
        self.create_sandbox_inner(spec, Some(effect_fence), source_handle)
            .await
    }

    async fn create_sandbox_inner(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: Option<&pc::SandboxEffectFence>,
        source_handle: Option<&pc::SandboxHandle>,
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
        let realization_fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
        let mut realization_guard = match effect_fence {
            Some(effect_fence) => crate::realization_marker::ProviderCreationGuard::Current(
                Box::new(crate::realization_marker::begin(
                    &raw_root,
                    &realization_fingerprint,
                    effect_fence,
                    source_handle
                        .map(crate::realization_marker::rebuild_source)
                        .transpose()?,
                    None,
                )?),
            ),
            None if source_handle.is_none() => {
                crate::realization_marker::ProviderCreationGuard::Legacy(
                    crate::realization_marker::begin_legacy(&raw_root)?,
                )
            }
            None => return Err(err("sandbox rebuild requires an aggregate effect fence")),
        };
        let raw_isolated_root = IsolatedRoot::new(raw_root.clone());
        let raw_workspace = raw_isolated_root
            .resolve(pc::WorkspaceLayout::ROOT)
            .map_err(err)?;
        let raw_secret_paths = spec
            .mounts
            .iter()
            .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
            .map(|mount| {
                host_projection_path(&raw_isolated_root, &raw_workspace, &mount.mount_path)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Ready response-loss recovery is projection-only. The completed root
        // and mount decisions are already authoritative in the marker receipt;
        // canonicalization below observes that inode but performs no mount,
        // extraction, directory creation, or byte reconciliation.
        if let Some(receipt) = realization_guard.completed_receipt()?.cloned() {
            let (realized, memory_materializations) =
                crate::replay_completion_receipt(spec, &receipt, pc::Realization::Bind)?;
            let raw_identity = realization_guard
                .root_identity()?
                .ok_or_else(|| err("Ready Namespace sandbox has no root identity"))?;
            let canonical_root = std::fs::canonicalize(&raw_root).map_err(err)?;
            if awaken_sandbox_fs::directory_identity_nofollow(&canonical_root).map_err(err)?
                != raw_identity
            {
                return Err(err(
                    "namespace sandbox root changed identity during Ready replay",
                ));
            }
            let root = IsolatedRoot::new(canonical_root);
            let host_workspace = root.resolve(pc::WorkspaceLayout::ROOT).map_err(err)?;
            let host_outputs = root.resolve(&spec.outputs_path).map_err(err)?;
            let secret_paths = spec
                .mounts
                .iter()
                .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
                .map(|mount| host_projection_path(&root, &host_workspace, &mount.mount_path))
                .collect::<Result<Vec<_>, _>>()?;
            let layout = replay_render_layout(&root, &host_workspace, spec)?;
            let control_directory = layout
                .iter()
                .find(|mount| mount.boundary == RenderMountBoundary::PrivateRendezvous)
                .map(|mount| mount.host.clone());
            let realization = realization_guard.complete(&receipt)?;
            return Ok(NamespaceSandbox {
                id: spec.scope.clone(),
                realization_root: raw_root,
                root,
                outputs_path: spec.outputs_path.clone(),
                host_workspace,
                host_outputs,
                base_env,
                inherit_agent_stderr: self.inherit_agent_stderr,
                secret_broker: self.secret_broker.clone(),
                network: spec.network.clone(),
                control_services: spec.control_services.clone(),
                control_directory,
                control_publication: Arc::new(NamespaceControlPublicationRegistry::default()),
                layout: std::sync::RwLock::new(layout),
                realized,
                secret_paths,
                memory_mounts: tokio::sync::Mutex::new(Vec::new()),
                memory_materializations: std::sync::Mutex::new(memory_materializations),
                memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
                memory_mounter: self.memory_mounter.clone(),
                realization,
                terminal_removal: std::sync::Mutex::new(None),
                owned_paths: std::sync::Mutex::new(
                    spec.mounts
                        .iter()
                        .map(|mount| mount.mount_path.clone())
                        .collect(),
                ),
                adopted_handle: None,
            });
        }
        if realization_guard.is_incomplete() {
            crate::shred_secret_paths_at(
                &raw_isolated_root,
                realization_guard.root_identity()?,
                &raw_secret_paths,
            )?;
        }
        realization_guard.prepare_root()?;
        let mut memory_mounts = Vec::new();
        let mut memory_materializations = Vec::new();
        let realization = async {
            // `/var` is a symlink to `/private/var` on macOS. Seatbelt evaluates some
            // operations against the canonical vnode path, so build every rule/env/cwd
            // from one canonical root or write grants can miss their target.
            let raw_identity =
                awaken_sandbox_fs::directory_identity_nofollow(&raw_root).map_err(err)?;
            let canonical_root = std::fs::canonicalize(&raw_root).map_err(err)?;
            if awaken_sandbox_fs::directory_identity_nofollow(&canonical_root).map_err(err)?
                != raw_identity
            {
                return Err(err(
                    "namespace sandbox root changed identity during canonicalization",
                ));
            }
            let root = IsolatedRoot::new(canonical_root);
            let host_workspace = root.resolve(pc::WorkspaceLayout::ROOT).map_err(err)?;
            let secret_paths = spec
                .mounts
                .iter()
                .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
                .map(|mount| host_projection_path(&root, &host_workspace, &mount.mount_path))
                .collect::<Result<Vec<_>, _>>()?;
            realization_guard.validate_before_mutation()?;
            awaken_sandbox_fs::create_relative_directory_all(
                root.root(),
                raw_identity,
                std::path::Path::new(pc::WorkspaceLayout::ROOT.trim_start_matches('/')),
            )
            .map_err(err)?;
            let host_outputs = root.resolve(&spec.outputs_path).map_err(err)?;
            realization_guard.validate_before_mutation()?;
            awaken_sandbox_fs::create_relative_directory_all(
                root.root(),
                raw_identity,
                std::path::Path::new(spec.outputs_path.trim_start_matches('/')),
            )
            .map_err(err)?;

            // Creating starts from an exact-owned empty root; Ready response-loss
            // recovery preserves the root and reconciles only the immutable
            // spec-owned mount destinations through this same leaf implementation.
            let (mut layout, realized) = self
                .realize_layout(
                    &root,
                    &host_workspace,
                    spec,
                    &mut memory_mounts,
                    &mut memory_materializations,
                    &realization_guard,
                )
                .await?;
            let control_directory = control_directory_for(&root, &spec.control_services)?;
            if let Some(directory) = control_directory.clone() {
                layout.push(private_control_mount(directory));
            }
            let receipt = crate::realization_marker::RealizationCompletionReceipt::new(
                &realized,
                memory_materializations.clone(),
            )?;
            let (realized, canonical_materializations) =
                crate::replay_completion_receipt(spec, &receipt, pc::Realization::Bind)?;
            memory_materializations = canonical_materializations;
            Ok::<_, pc::SandboxError>((
                root,
                host_workspace,
                host_outputs,
                layout,
                realized,
                secret_paths,
                control_directory,
                receipt,
            ))
        }
        .await;
        let (
            root,
            host_workspace,
            host_outputs,
            layout,
            realized,
            secret_paths,
            control_directory,
            receipt,
        ) = match realization {
            Ok(realization) => realization,
            Err(cause) => {
                let teardown = crate::teardown_memory_mounts(&memory_mounts).await;
                if teardown.is_ok() {
                    memory_mounts.clear();
                }
                let shredding = if realization_guard.is_incomplete() {
                    crate::shred_secret_paths_at(
                        &raw_isolated_root,
                        realization_guard.root_identity()?,
                        &raw_secret_paths,
                    )
                } else {
                    Ok(())
                };
                if teardown.is_err() || shredding.is_err() {
                    return Err(err(format!(
                        "namespace realization failed: {cause}; memory teardown: {}; secret shredding: {}; exact root evidence was retained",
                        teardown
                            .as_ref()
                            .err()
                            .map_or_else(|| "ok".to_owned(), ToString::to_string),
                        shredding
                            .as_ref()
                            .err()
                            .map_or_else(|| "ok".to_owned(), ToString::to_string),
                    )));
                }
                if let Err(cleanup) = realization_guard.abort_creation() {
                    return Err(err(format!(
                        "namespace realization failed: {cause}; exact-owned cleanup failed: {cleanup}"
                    )));
                }
                return Err(cause);
            }
        };
        let realization = match realization_guard.complete(&receipt) {
            Ok(realization) => realization,
            Err(cause) => {
                let teardown = crate::teardown_memory_mounts(&memory_mounts).await;
                if teardown.is_ok() {
                    memory_mounts.clear();
                }
                let shredding = if realization_guard.is_incomplete() {
                    crate::shred_secret_paths_at(
                        &raw_isolated_root,
                        realization_guard.root_identity()?,
                        &raw_secret_paths,
                    )
                } else {
                    Ok(())
                };
                if teardown.is_err() || shredding.is_err() {
                    return Err(err(format!(
                        "namespace Ready publication failed: {cause}; memory teardown: {}; secret shredding: {}; exact root evidence was retained",
                        teardown
                            .as_ref()
                            .err()
                            .map_or_else(|| "ok".to_owned(), ToString::to_string),
                        shredding
                            .as_ref()
                            .err()
                            .map_or_else(|| "ok".to_owned(), ToString::to_string),
                    )));
                }
                if let Err(cleanup) = realization_guard.abort_creation() {
                    return Err(err(format!(
                        "namespace Ready publication failed: {cause}; exact-owned cleanup failed: {cleanup}"
                    )));
                }
                return Err(cause);
            }
        };

        Ok(NamespaceSandbox {
            id: spec.scope.clone(),
            realization_root: raw_root,
            root,
            outputs_path: spec.outputs_path.clone(),
            host_workspace,
            host_outputs,
            base_env,
            inherit_agent_stderr: self.inherit_agent_stderr,
            secret_broker: self.secret_broker.clone(),
            network: spec.network.clone(),
            control_services: spec.control_services.clone(),
            control_directory,
            control_publication: Arc::new(NamespaceControlPublicationRegistry::default()),
            layout: std::sync::RwLock::new(layout),
            realized,
            secret_paths,
            memory_mounts: tokio::sync::Mutex::new(memory_mounts),
            memory_materializations: std::sync::Mutex::new(memory_materializations),
            memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
            memory_mounter: self.memory_mounter.clone(),
            realization,
            terminal_removal: std::sync::Mutex::new(None),
            owned_paths: std::sync::Mutex::new(
                spec.mounts
                    .iter()
                    .map(|mount| mount.mount_path.clone())
                    .collect(),
            ),
            adopted_handle: None,
        })
    }

    /// Re-open a namespace sandbox from its durable handle so companion
    /// capabilities operate on the Run's existing environment.
    pub async fn adopt_sandbox(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        self.adopt_sandbox_inner(None, handle, None, None).await
    }

    pub async fn adopt_sandbox_with_control_services(
        &self,
        handle: &pc::SandboxHandle,
        control_services: &std::collections::BTreeSet<SandboxControlServiceKind>,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        self.adopt_sandbox_inner(None, handle, None, Some(control_services))
            .await
    }

    pub async fn adopt_sandbox_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        self.adopt_sandbox_inner(Some(spec), handle, Some(effect_fence), None)
            .await
    }

    /// Reconstruct only the exact terminal participant for a `Removing`
    /// namespace realization. No canonicalization, mount, launcher, or ordinary
    /// adoption effect occurs on this seam.
    pub fn prepare_terminal_sandbox_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
    ) -> Result<Option<NamespaceSandbox>, pc::SandboxError> {
        // Pure validation precedes marker admission. Exact copy evidence is
        // carried for the Host's one recovered-CAS reconciliation; this cold
        // provider object intentionally reconstructs no MemoryMount guard.
        let mut materializations = crate::terminal_copy_materializations(spec, handle)?;
        let fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
        let (id, outputs_path, base_env, network, owned_paths) = if let Some(handle) = handle {
            let payload = handle.namespace_payload(Self::provider_kind())?;
            if handle.realization_fingerprint() != Some(&fingerprint)
                || payload.outputs_path != spec.outputs_path
                || payload.base_env != spec.env
                || payload.network != spec.network
                || payload.control_services != spec.control_services
            {
                return Err(err(
                    "Namespace terminal handle does not match the effective authorization spec",
                ));
            }
            (
                handle.sandbox_id.as_str(),
                payload.outputs_path.as_str(),
                payload.base_env.clone(),
                payload.network.clone(),
                handle.owned_paths().unwrap_or_default().to_vec(),
            )
        } else {
            (
                spec.scope.as_str(),
                spec.outputs_path.as_str(),
                spec.env.clone(),
                spec.network.clone(),
                spec.mounts
                    .iter()
                    .map(|mount| mount.mount_path.clone())
                    .collect(),
            )
        };
        let raw_root = crate::sandbox_dir(&self.base, id);
        let terminal = crate::realization_marker::begin_terminal_takeover(
            &raw_root,
            &fingerprint,
            handle
                .map(crate::realization_marker::rebuild_source)
                .transpose()?,
            expected_effect_fence,
            terminal_effect_fence,
        )?;
        let Some((realization, removal)) = terminal else {
            return Ok(None);
        };
        let mut realized = Vec::new();
        if let Some(receipt) = removal.completed_receipt()? {
            let (receipt_realized, receipt_materializations) =
                crate::replay_completion_receipt(spec, receipt, pc::Realization::Bind)?;
            if receipt_materializations != materializations {
                return Err(err(
                    "Namespace terminal handle Memory evidence differs from the Ready receipt",
                ));
            }
            realized = receipt_realized;
            materializations = receipt_materializations;
        } else if handle.is_some() {
            return Err(err(
                "Namespace terminal handle targets a realization without a Ready receipt",
            ));
        }
        let root = IsolatedRoot::new(raw_root.clone());
        let host_workspace = root.resolve(pc::WorkspaceLayout::ROOT).map_err(err)?;
        let host_outputs = root.resolve(outputs_path).map_err(err)?;
        let secret_paths = spec
            .mounts
            .iter()
            .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
            .map(|mount| host_projection_path(&root, &host_workspace, &mount.mount_path))
            .collect::<Result<_, _>>()?;
        Ok(Some(NamespaceSandbox {
            id: id.to_owned(),
            realization_root: raw_root,
            root,
            outputs_path: outputs_path.to_owned(),
            host_workspace,
            host_outputs,
            base_env,
            inherit_agent_stderr: self.inherit_agent_stderr,
            secret_broker: self.secret_broker.clone(),
            network,
            control_services: spec.control_services.clone(),
            control_directory: None,
            control_publication: Arc::new(NamespaceControlPublicationRegistry::default()),
            layout: std::sync::RwLock::new(Vec::new()),
            realized,
            secret_paths,
            memory_mounts: tokio::sync::Mutex::new(Vec::new()),
            memory_materializations: std::sync::Mutex::new(materializations),
            memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
            memory_mounter: self.memory_mounter.clone(),
            realization: crate::realization_marker::LiveRealization::Current(realization),
            terminal_removal: std::sync::Mutex::new(Some(removal)),
            owned_paths: std::sync::Mutex::new(owned_paths),
            adopted_handle: None,
        }))
    }

    async fn adopt_sandbox_inner(
        &self,
        spec: Option<&pc::SandboxSpec>,
        handle: &pc::SandboxHandle,
        effect_fence: Option<&pc::SandboxEffectFence>,
        requested_control_services: Option<&std::collections::BTreeSet<SandboxControlServiceKind>>,
    ) -> Result<NamespaceSandbox, pc::SandboxError> {
        let payload = handle.namespace_payload(Self::provider_kind())?;
        let requested_control_services = spec
            .map(|spec| &spec.control_services)
            .or(requested_control_services);
        let control_services = pc::validate_adopted_sandbox_control_services(
            requested_control_services,
            &payload.control_services,
            &Self::capabilities(),
        )
        .map_err(err)?;
        if let Some(spec) = spec
            && (handle.realization_fingerprint()
                != Some(&pc::SandboxRealizationFingerprint::from_spec(spec))
                || payload.outputs_path != spec.outputs_path
                || payload.base_env != spec.env
                || payload.network != spec.network)
        {
            return Err(err(
                "Namespace sandbox handle does not match the effective adoption spec",
            ));
        }
        let outputs_path = payload.outputs_path.clone();
        let raw_root = crate::sandbox_dir(&self.base, &handle.sandbox_id);
        let verified = crate::realization_marker::verify_adoption(
            &raw_root,
            handle.realization_fingerprint(),
            handle.filesystem_effect_fence()?,
            handle.filesystem_physical_incarnation()?,
            effect_fence,
        )?;
        let completion = verified.as_ref().map(|(_, receipt)| receipt.clone());
        let realization = match verified {
            Some((evidence, _)) => crate::realization_marker::LiveRealization::Current(evidence),
            None => crate::realization_marker::LiveRealization::LegacyAdopted,
        };
        let raw_identity =
            awaken_sandbox_fs::directory_identity_nofollow(&raw_root).map_err(err)?;
        if realization
            .current()
            .is_some_and(|evidence| evidence.root_identity() != raw_identity)
        {
            return Err(err(
                "namespace sandbox root was substituted after fenced adoption",
            ));
        }
        let canonical_root = std::fs::canonicalize(&raw_root).map_err(err)?;
        if awaken_sandbox_fs::directory_identity_nofollow(&canonical_root).map_err(err)?
            != raw_identity
        {
            return Err(err(
                "namespace sandbox root changed identity during adoption",
            ));
        }
        let root = IsolatedRoot::new(canonical_root);
        let host_workspace = root.resolve(pc::WorkspaceLayout::ROOT).map_err(err)?;
        let host_outputs = root.resolve(&outputs_path).map_err(err)?;
        let secret_paths = if let Some(spec) = spec {
            spec.mounts
                .iter()
                .filter(|mount| matches!(mount.source, pc::MountSource::Secret { .. }))
                .map(|mount| host_projection_path(&root, &host_workspace, &mount.mount_path))
                .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };
        let handle_materializations = handle
            .memory_materializations()?
            .unwrap_or_default()
            .to_vec();
        let (realized, materializations) = if let Some(receipt) = completion {
            let (realized, materializations) = match spec {
                Some(spec) => {
                    crate::replay_completion_receipt(spec, &receipt, pc::Realization::Bind)?
                }
                None => (receipt.mounts(), receipt.memory_materializations().to_vec()),
            };
            if handle_materializations != materializations {
                return Err(err(
                    "Namespace sandbox handle Memory evidence differs from the Ready receipt",
                ));
            }
            (realized, materializations)
        } else {
            (Vec::new(), handle_materializations)
        };
        let mut layout = if let Some(spec) = spec {
            replay_render_layout(&root, &host_workspace, spec)?
        } else {
            Vec::new()
        };
        let control_directory = control_directory_for(&root, &control_services)?;
        if spec.is_none()
            && let Some(directory) = control_directory.clone()
        {
            layout.push(private_control_mount(directory));
        }
        Ok(NamespaceSandbox {
            id: handle.sandbox_id.clone(),
            realization_root: raw_root,
            root,
            outputs_path,
            host_workspace,
            host_outputs,
            base_env: payload.base_env.clone(),
            inherit_agent_stderr: self.inherit_agent_stderr,
            secret_broker: self.secret_broker.clone(),
            network: payload.network.clone(),
            control_services,
            control_directory,
            control_publication: Arc::new(NamespaceControlPublicationRegistry::default()),
            layout: std::sync::RwLock::new(layout),
            realized,
            secret_paths,
            memory_mounts: tokio::sync::Mutex::new(Vec::new()),
            memory_materializations: std::sync::Mutex::new(materializations),
            memory_reconciliation_ack: pc::MemoryReconciliationAck::default(),
            memory_mounter: self.memory_mounter.clone(),
            realization,
            terminal_removal: std::sync::Mutex::new(None),
            owned_paths: std::sync::Mutex::new(handle.owned_paths().unwrap_or_default().to_vec()),
            adopted_handle: handle.restoration().map(|_| handle.clone()),
        })
    }
}

/// A realized OS-confined environment (bubblewrap on Linux, Seatbelt on macOS).
pub struct NamespaceSandbox {
    id: String,
    /// Stable provider pathname that owns marker/stage publication. The
    /// execution root may be canonicalized for Seatbelt, but lifecycle evidence
    /// must never silently migrate to that alternate spelling.
    realization_root: PathBuf,
    root: IsolatedRoot,
    outputs_path: String,
    host_workspace: PathBuf,
    host_outputs: PathBuf,
    base_env: Vec<pc::EnvVar>,
    inherit_agent_stderr: bool,
    secret_broker: Arc<std::sync::RwLock<Option<Arc<dyn pc::SecretBroker>>>>,
    network: pc::NetworkPolicy,
    control_services: std::collections::BTreeSet<SandboxControlServiceKind>,
    control_directory: Option<PathBuf>,
    control_publication: Arc<NamespaceControlPublicationRegistry>,
    layout: std::sync::RwLock<Vec<RenderMount>>,
    realized: Vec<pc::RealizedMount>,
    /// Host paths of realized secrets, shredded at dispose before the tree is reaped
    /// (ADR-0023). Empty after an `adopt`.
    secret_paths: Vec<PathBuf>,
    /// Live memory-store mounts, harvested / unmounted at dispose before the tree is
    /// reaped. Empty after an `adopt` (a reconnected sandbox owns no fresh guards).
    memory_mounts: tokio::sync::Mutex<Vec<Box<dyn pc::MemoryMount>>>,
    /// Canonical durable heads for copy-backed Memory mounts.
    memory_materializations: std::sync::Mutex<Vec<pc::MemoryMaterializationEvidence>>,
    /// One process-local exact-evidence/effect acknowledgement. Durable Memory
    /// heads remain owned by the handle and store; this only gates guard drain
    /// against physical disposal.
    memory_reconciliation_ack: pc::MemoryReconciliationAck,
    memory_mounter: Arc<std::sync::RwLock<Option<Arc<dyn pc::MemoryMounter>>>>,
    /// One coherent marker/legacy realization authority.
    realization: crate::realization_marker::LiveRealization,
    terminal_removal: std::sync::Mutex<Option<crate::realization_marker::RemovalGuard>>,
    owned_paths: std::sync::Mutex<Vec<String>>,
    /// Complete Phase-A future restoration wire retained only for pass-through.
    /// Ordinary P/V2 handles remain derived from the provider marker authority.
    adopted_handle: Option<pc::SandboxHandle>,
}

impl NamespaceSandbox {
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
    /// live projection effect starts. Reservations remain monotonic so crash
    /// recovery never loses evidence for a possibly materialized tree.
    pub fn reserve_owned_path(&self, path: &str) {
        let mut owned = self.owned_paths.lock().expect("owned paths lock poisoned");
        if !owned.iter().any(|current| current == path) {
            owned.push(path.to_string());
        }
    }

    fn workspace_root(&self) -> IsolatedRoot {
        IsolatedRoot::new(self.host_workspace.clone())
    }

    /// Materialize an immutable runtime-owned tree through the shared lexical jail.
    pub fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>, bool)],
    ) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        let sandbox_relative = pc::WorkspaceLayout::child(workspace_relative(subdir));
        materialize_read_only_tree_at(
            &self.root,
            root_identity,
            sandbox_relative.trim_start_matches('/'),
            files,
        )
    }

    /// Tear down every live memory mount (harvest a copy / unmount a FUSE), draining
    /// the guard list so a later dispose is a no-op.
    pub async fn release_memory_mounts(&self) -> Result<(), pc::SandboxError> {
        crate::release_memory_mounts(&self.memory_mounts).await
    }

    /// Remove only the runtime-owned resource projection while retaining the
    /// Session workspace and every non-resource file.
    pub fn clear_resource_projection(&self) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        awaken_sandbox_fs::remove_relative_entry_exact(
            self.root.root(),
            root_identity,
            std::path::Path::new(
                pc::WorkspaceLayout::RESOURCE_PROJECTION_ROOT.trim_start_matches('/'),
            ),
        )
        .map_err(err)
    }

    pub fn provision_repo(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        self.require_root_identity()?;
        plan.validate_mount_path()?;
        provision_repo_at(
            &self.workspace_root(),
            workspace_relative(&plan.mount_path),
            &plan.transport_url,
            plan.initial_branch.as_deref(),
            plan.initial_commit.as_deref(),
            credential,
        )
        .map_err(err)?;
        self.reserve_owned_path(&plan.mount_path);
        Ok(())
    }

    pub fn push_repo(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        self.require_root_identity()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        plan.validate_mount_path()
            .map_err(pc::RepositoryPublicationError::Unavailable)?;
        push_repo_to_at(
            &self.workspace_root(),
            workspace_relative(&plan.mount_path),
            plan,
            expectation,
            credential,
        )
    }

    pub fn list_files(&self, subdir: &str) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        let has_copy_materializations = !self
            .memory_materializations
            .lock()
            .map_err(|_| err("Memory materializations lock poisoned"))?
            .is_empty();
        let Some(identity) = self.root_identity_for_access()? else {
            if has_copy_materializations {
                return Err(err(
                    "copy-backed Memory lost its exact namespace root before reconciliation",
                ));
            }
            return Ok(Vec::new());
        };
        let files = if subdir.starts_with('/') && !pc::WorkspaceLayout::contains(subdir) {
            list_files_at(&self.root, identity, subdir.trim_start_matches('/'))
        } else {
            let sandbox_relative = pc::WorkspaceLayout::child(workspace_relative(subdir));
            list_files_at(
                &self.root,
                identity,
                sandbox_relative.trim_start_matches('/'),
            )
        }?;
        // A recovered copy must never turn concurrent physical-root loss into
        // an empty CAS candidate. The bytes above are descriptor-captured; this
        // postcheck distinguishes a missing-root convenience result.
        if has_copy_materializations {
            self.require_root_identity()?;
        }
        Ok(files)
    }

    pub fn scan_skill_dir(
        &self,
        subdir: &str,
    ) -> Result<Vec<DiscoveredSkillFile>, pc::SandboxError> {
        let Some(identity) = self.root_identity_for_access()? else {
            return Ok(Vec::new());
        };
        let logical_subdir = workspace_relative(subdir);
        let sandbox_relative = pc::WorkspaceLayout::child(logical_subdir);
        scan_skill_dir_at(
            &self.root,
            identity,
            sandbox_relative.trim_start_matches('/'),
            logical_subdir,
        )
        .map_err(Into::into)
    }

    pub fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        let sandbox_relative = pc::WorkspaceLayout::child(workspace_relative(logical));
        awaken_sandbox_fs::write_relative_file_atomic(
            self.root.root(),
            root_identity,
            std::path::Path::new(sandbox_relative.trim_start_matches('/')),
            contents,
            0o600,
        )
        .map_err(err)
    }

    /// Remove one dynamically projected workspace path. Missing paths are an
    /// idempotent success and lexical traversal is rejected by the shared jail.
    pub fn remove_inline(&self, logical: &str) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        let sandbox_relative = pc::WorkspaceLayout::child(workspace_relative(logical));
        awaken_sandbox_fs::remove_relative_entry_exact(
            self.root.root(),
            root_identity,
            std::path::Path::new(sandbox_relative.trim_start_matches('/')),
        )
        .map_err(err)
    }

    /// Revoke a dynamically attached mount and its runtime-owned backing path.
    /// Non-mount workspace paths retain the ordinary lexical removal behavior.
    pub fn remove_mount(&self, logical: &str) -> Result<(), pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
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
        let relative = path
            .strip_prefix(self.root.root())
            .map_err(|_| err("namespace mount path escaped its sandbox root"))?;
        awaken_sandbox_fs::remove_relative_entry_exact(self.root.root(), root_identity, relative)
            .map_err(err)
    }

    fn translate_macos_path(&self, value: &str) -> Option<String> {
        let mut mappings: Vec<(&str, &std::path::Path)> = vec![
            (pc::WorkspaceLayout::ROOT, &self.host_workspace),
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
            process.current_dir(cwd);
            crate::RuntimePathEnv::new(
                self.host_workspace.to_string_lossy().into_owned(),
                self.host_outputs.to_string_lossy().into_owned(),
            )
            .apply(process);
        } else {
            crate::RuntimePathEnv::new(pc::WorkspaceLayout::ROOT, self.outputs_path.clone())
                .apply(process);
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
        #[cfg(target_os = "macos")]
        let argv = sandbox_exec_argv(&input);
        #[cfg(target_os = "linux")]
        let argv = bubblewrap_argv(&input);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            Ok(argv)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = input;
            Err(err(
                "OS-native Namespace sandbox is unsupported on this platform",
            ))
        }
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
        self.require_root_identity()?;
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

#[path = "namespace/sandbox_contract.rs"]
mod sandbox_contract;
impl NamespaceSandbox {
    fn shred_secrets(&self) -> Result<(), pc::SandboxError> {
        crate::shred_secret_paths_at(
            &self.root,
            self.root_identity_for_access()?,
            &self.secret_paths,
        )
    }
}

#[cfg(test)]
#[path = "namespace/tests.rs"]
mod tests;
