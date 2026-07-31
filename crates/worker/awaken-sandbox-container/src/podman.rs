//! Rootless **Podman** backend (`podman` feature) — the daemonless, worker-parented
//! executor for the container tier (awaken-next / oversight parity).
//!
//! Unlike the bollard/kube adapters, Podman is driven over its **CLI** (`podman run`
//! …): there is no daemon, so the container is a direct child of the worker and is
//! reaped as a unit (`--init` handles PID 1). It realizes the same [`ContainerRuntime`]
//! port as Docker/K8s and honors the [`crate::RootfsPlan`] (an `Image` or a private
//! `IsolatedRoot`), reached over the published agent port via [`crate::net`] — the
//! same dial the Docker adapter uses. Compile-verified here; running needs `podman`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport, SplitChannel};
use awaken_provisioning_contract as pc;
use tokio::process::{Child, Command as OsCommand};

use crate::net::TcpAgentTransport;
use crate::{
    ContainerPlan, ContainerRuntime, ContainerState, ManagedContainer, PackageImageProvisioner,
    REAPER_LABEL, REAPER_OWNER_LABEL, RuntimeAgentProcess, RuntimeError, podman_run_argv,
    runtime_container_name,
};

static EXEC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
}

struct PodmanExecState {
    child: Option<Child>,
    status: Option<pc::ExitStatus>,
}

struct PodmanExecProcess {
    id: String,
    container_id: String,
    bin: String,
    pid_file: String,
    state: tokio::sync::Mutex<PodmanExecState>,
}

impl PodmanExecProcess {
    fn exit_status(status: std::process::ExitStatus) -> pc::ExitStatus {
        pc::ExitStatus {
            code: status.code(),
            signaled: status.code().is_none(),
        }
    }
}

#[async_trait]
impl pc::ProcessHandle for PodmanExecProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        let mut state = self.state.lock().await;
        if let Some(status) = &state.status {
            return Ok(status.clone());
        }
        let child = state
            .child
            .as_mut()
            .ok_or_else(|| pc::SandboxError::new("podman exec process is not attached"))?;
        let status = child
            .wait()
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let status = Self::exit_status(status);
        state.status = Some(status.clone());
        Ok(status)
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let mut state = self.state.lock().await;
        if let Some(status) = &state.status {
            return Ok(Some(status.clone()));
        }
        let child = state
            .child
            .as_mut()
            .ok_or_else(|| pc::SandboxError::new("podman exec process is not attached"))?;
        let status = child
            .try_wait()
            .map_err(|error| pc::SandboxError::new(error.to_string()))?
            .map(Self::exit_status);
        if let Some(status) = &status {
            state.status = Some(status.clone());
        }
        Ok(status)
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        let name = match signal {
            pc::Signal::Term => "TERM",
            pc::Signal::Kill => "KILL",
            pc::Signal::Int => "INT",
        };
        let script = format!(
            "pid=$(cat -- '{}') && kill -{} \"$pid\"",
            self.pid_file.replace('\'', "'\\''"),
            name
        );
        let status = OsCommand::new(&self.bin)
            .args(["exec", &self.container_id, "sh", "-c", &script])
            .status()
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if status.success() {
            Ok(())
        } else {
            Err(pc::SandboxError::new(format!(
                "podman exec signal failed with {status}"
            )))
        }
    }
}

/// The outcome of one subcommand, decoupled from `std::process` so the CLI logic
/// (argv assembly, stdout parsing, error mapping) is unit-testable without a real
/// `podman` binary. The live dial in [`ContainerRuntime::open_channel`] still needs
/// a running container and is exercised only by the gated integration test.
#[derive(Debug, Clone)]
pub(crate) struct CmdOutput {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The command-execution seam. Production forks a real process; tests script it.
#[async_trait]
pub(crate) trait CommandExec: Send + Sync {
    async fn exec(&self, bin: &str, args: &[String]) -> std::io::Result<CmdOutput>;
}

/// The real executor — forks `bin args` and captures its output.
struct OsCommandExec;

#[async_trait]
impl CommandExec for OsCommandExec {
    async fn exec(&self, bin: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        let out = OsCommand::new(bin).args(args).output().await?;
        Ok(CmdOutput {
            ok: out.status.success(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

fn signal_flag(signal: pc::Signal) -> &'static str {
    match signal {
        pc::Signal::Term => "TERM",
        pc::Signal::Kill => "KILL",
        pc::Signal::Int => "INT",
    }
}

/// A rootless-Podman [`ContainerRuntime`]. `agent_port` is the container-internal TCP
/// port the agent listens on; it is published to an ephemeral `127.0.0.1` host port
/// that [`ContainerRuntime::open_channel`] discovers (`podman port`) and dials.
pub struct PodmanRuntime {
    bin: String,
    agent_port: u16,
    exec: Arc<dyn CommandExec>,
    owner_id: String,
    /// Podman labels are immutable; successfully renewed/adopted containers are
    /// protected from this incarnation's crash reaper through this ownership set.
    adopted: std::sync::Mutex<std::collections::HashSet<String>>,
    package_builds: tokio::sync::Mutex<()>,
    package_registry: Option<String>,
    package_registry_auth_file: Option<PathBuf>,
    package_cache_ttl: Option<std::time::Duration>,
}

impl PodmanRuntime {
    /// Use `podman` from `PATH`.
    #[must_use]
    pub fn new(agent_port: u16) -> Self {
        Self::with_bin(agent_port, "podman")
    }

    /// Use the deployment-selected Podman executable.
    #[must_use]
    pub fn with_bin(agent_port: u16, bin: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            agent_port,
            exec: Arc::new(OsCommandExec),
            owner_id: crate::runtime_owner_id(),
            adopted: std::sync::Mutex::new(std::collections::HashSet::new()),
            package_builds: tokio::sync::Mutex::new(()),
            package_registry: None,
            package_registry_auth_file: None,
            package_cache_ttl: None,
        }
    }

    /// Wire a scripted executor (tests) instead of forking a real `podman`.
    #[cfg(test)]
    fn with_exec(agent_port: u16, exec: Arc<dyn CommandExec>) -> Self {
        Self {
            bin: "podman".into(),
            agent_port,
            exec,
            owner_id: crate::runtime_owner_id(),
            adopted: std::sync::Mutex::new(std::collections::HashSet::new()),
            package_builds: tokio::sync::Mutex::new(()),
            package_registry: None,
            package_registry_auth_file: None,
            package_cache_ttl: None,
        }
    }

    /// Publish content-addressed package images to a shared OCI registry.
    #[must_use]
    pub fn with_package_registry(mut self, registry: impl Into<String>) -> Self {
        self.package_registry = Some(registry.into().trim_end_matches('/').to_string());
        self
    }

    /// Bound unused Awaken-derived images in the local rootless engine cache.
    #[must_use]
    pub fn with_package_cache_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.package_cache_ttl = Some(ttl);
        self
    }

    async fn resolve_package_build(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
    ) -> Result<(String, String, String), RuntimeError> {
        let base_identity = self
            .run(&[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}\n{{.Config.User}}".into(),
                base_image.into(),
            ])
            .await?;
        let mut base_lines = base_identity.lines();
        let base_identity = base_lines.next().unwrap_or_default();
        let base_user = base_lines.next().unwrap_or_default();
        if base_identity.is_empty() {
            return Err(backend("podman returned an empty base-image identity"));
        }
        let (containerfile, fingerprint) =
            crate::packages::package_image_recipe(base_identity, base_user, packages)?;
        let image = self.package_registry.as_ref().map_or_else(
            || format!("localhost/awaken-packages:{fingerprint}"),
            |registry| format!("{registry}/awaken-packages:{fingerprint}"),
        );
        Ok((containerfile, fingerprint, image))
    }

    async fn prune_package_cache(&self) {
        let Some(ttl) = self.package_cache_ttl else {
            return;
        };
        let _ = self
            .run(&[
                "image".into(),
                "prune".into(),
                "--force".into(),
                "--all".into(),
                "--filter".into(),
                "label=org.awaken.package-recipe".into(),
                "--filter".into(),
                format!("until={}s", ttl.as_secs()),
            ])
            .await;
    }

    /// Select a Docker/containers authentication file for registry pull/push.
    /// Podman reads it only in the Worker-side builder process.
    pub fn with_package_registry_auth_file(
        mut self,
        path: impl AsRef<Path>,
    ) -> Result<Self, RuntimeError> {
        if self.package_registry.is_none() {
            return Err(backend(
                "registry authentication requires a package registry",
            ));
        }
        let path = path.as_ref();
        if !path.is_file() {
            return Err(backend(format!(
                "registry authentication file `{}` is not a file",
                path.display()
            )));
        }
        self.package_registry_auth_file = Some(path.to_owned());
        Ok(self)
    }

    fn registry_command(&self, command: &str, image: &str) -> Vec<String> {
        let mut args = vec![command.to_owned()];
        if let Some(path) = &self.package_registry_auth_file {
            args.extend(["--authfile".to_owned(), path.to_string_lossy().into_owned()]);
        }
        args.push(image.to_owned());
        args
    }

    /// Run a podman subcommand, returning trimmed stdout (or a backend error).
    async fn run(&self, args: &[String]) -> Result<String, RuntimeError> {
        let out = self.exec.exec(&self.bin, args).await.map_err(backend)?;
        if !out.ok {
            return Err(RuntimeError::Backend(format!(
                "podman {}: {}",
                args.first().cloned().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Probe the binary (for tests / health checks): `Ok` iff `podman` responds.
    pub async fn ping(&self) -> Result<(), RuntimeError> {
        self.run(&["info".into(), "--format".into(), "{{.Host.Arch}}".into()])
            .await
            .map(|_| ())
    }

    async fn package_image_reference(&self, image: &str) -> Result<String, RuntimeError> {
        if self.package_registry.is_none() {
            return Ok(image.to_string());
        }
        let digest = self
            .run(&[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{index .RepoDigests 0}}".into(),
                image.to_string(),
            ])
            .await?;
        if digest.contains("@sha256:") {
            Ok(digest)
        } else {
            Err(backend("package image has no immutable repository digest"))
        }
    }

    /// The ephemeral host address the agent port was published to (`podman port`).
    async fn agent_addr(&self, container_id: &str) -> Result<SocketAddr, RuntimeError> {
        let mapping = self
            .run(&[
                "port".into(),
                container_id.into(),
                format!("{}/tcp", self.agent_port),
            ])
            .await?;
        // e.g. "127.0.0.1:49153" (first line if multiple bindings).
        let host_port = mapping
            .lines()
            .next()
            .and_then(|l| l.rsplit(':').next())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| backend("agent port is not published yet"))?;
        format!("127.0.0.1:{host_port}")
            .parse()
            .map_err(|e| backend(format!("bad published addr: {e}")))
    }

    fn exec_process(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
        attached_agent: bool,
    ) -> Result<(String, String, Child), RuntimeError> {
        if command.argv.is_empty() {
            return Err(backend("exec command argv is empty"));
        }
        if !attached_agent && command.stdio == pc::Stdio::Piped {
            return Err(backend(
                "piped container exec requires the agent-channel capability",
            ));
        }
        let id = format!(
            "podman-exec-{}-{}",
            std::process::id(),
            EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let pid_file = format!("/tmp/{id}.pid");
        let mut args = vec!["exec".to_string()];
        if attached_agent {
            args.push("-i".into());
        }
        if !command.cwd.is_empty() {
            args.extend(["--workdir".into(), command.cwd.clone()]);
        }
        for var in &command.env {
            // Podman copies a named variable from its own environment into the
            // container. Keep the value out of the CLI argv, where `ps` and
            // `/proc/*/cmdline` would otherwise expose process credentials.
            args.extend(["--env".into(), var.name.clone()]);
        }
        args.extend([
            container_id.to_string(),
            "sh".into(),
            "-c".into(),
            "pid_file=$1; shift; printf '%s' \"$$\" > \"$pid_file\"; exec \"$@\"".into(),
            "awaken-exec".into(),
            pid_file.clone(),
        ]);
        args.extend(command.argv);
        let mut process = OsCommand::new(&self.bin);
        process.args(args);
        for var in &command.env {
            process.env(&var.name, var.value.expose());
        }
        if attached_agent {
            process
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
        } else {
            match command.stdio {
                pc::Stdio::Inherit => {
                    process
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit());
                }
                pc::Stdio::Null => {
                    process
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null());
                }
                pc::Stdio::Piped => unreachable!("rejected above"),
            }
        }
        let child = process.spawn().map_err(backend)?;
        Ok((id, pid_file, child))
    }
}

#[async_trait]
impl ContainerRuntime for PodmanRuntime {
    fn enforces_network_none(&self) -> bool {
        true
    }

    fn supports_package_provisioning(&self) -> bool {
        true
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        if packages.is_empty() {
            return Ok(base_image.to_string());
        }
        let _build_guard = self.package_builds.lock().await;
        self.prune_package_cache().await;
        let (dockerfile, fingerprint, image) =
            self.resolve_package_build(base_image, packages).await?;
        if self.package_registry.is_none()
            && self
                .run(&["image".into(), "exists".into(), image.clone()])
                .await
                .is_ok()
        {
            return self.package_image_reference(&image).await;
        }
        if self.package_registry.is_some()
            && self
                .run(&self.registry_command("pull", &image))
                .await
                .is_ok()
        {
            return self.package_image_reference(&image).await;
        }
        if self.package_registry.is_some()
            && self
                .run(&["image".into(), "exists".into(), image.clone()])
                .await
                .is_ok()
        {
            self.run(&self.registry_command("push", &image)).await?;
            return self.package_image_reference(&image).await;
        }
        let mut guard = None;
        let root =
            crate::staging_dir(&mut guard, &format!("package-{fingerprint}")).map_err(backend)?;
        let containerfile = root.join("Containerfile");
        std::fs::write(&containerfile, dockerfile).map_err(backend)?;
        let mut build = vec!["build".into()];
        match network {
            pc::NetworkPolicy::Unrestricted => {}
            pc::NetworkPolicy::None => build.extend(["--network".into(), "none".into()]),
            pc::NetworkPolicy::Allowlist { .. } => {
                return Err(backend(
                    "package image build has no no-bypass allowlist network",
                ));
            }
        }
        build.extend([
            "--tag".into(),
            image.clone(),
            "--file".into(),
            containerfile.to_string_lossy().into_owned(),
            root.to_string_lossy().into_owned(),
        ]);
        self.run(&build).await?;
        if self.package_registry.is_some() {
            self.run(&self.registry_command("push", &image)).await?;
            return self.package_image_reference(&image).await;
        }
        Ok(image)
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let name = runtime_container_name(&self.owner_id, id);
        // Idempotent: clear any stale container of this scope first.
        let _ = self.run(&["rm".into(), "-f".into(), name.clone()]).await;

        let mut args = podman_run_argv(&name, plan, &plan.rootfs);
        // Publish the agent's internal port to an ephemeral 127.0.0.1 host port so
        // `open_channel` can dial it (inserted after `--name <name>`, before the image).
        if let Some(i) = args.iter().position(|a| a == &name) {
            args.splice(
                i + 1..i + 1,
                [
                    "--label".to_string(),
                    format!("{REAPER_OWNER_LABEL}={}", self.owner_id),
                    "-p".to_string(),
                    format!("127.0.0.1::{}", self.agent_port),
                ],
            );
        }
        self.run(&args).await?;
        Ok(name)
    }

    async fn spawn(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        let (id, pid_file, child) = self.exec_process(container_id, command, false)?;
        Ok(Box::new(PodmanExecProcess {
            id,
            container_id: container_id.to_string(),
            bin: self.bin.clone(),
            pid_file,
            state: tokio::sync::Mutex::new(PodmanExecState {
                child: Some(child),
                status: None,
            }),
        }))
    }

    async fn spawn_agent(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        let (id, pid_file, mut child) = self.exec_process(container_id, command, true)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| backend("podman agent exec has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| backend("podman agent exec has no stdout"))?;
        Ok(RuntimeAgentProcess {
            process: Box::new(PodmanExecProcess {
                id,
                container_id: container_id.to_string(),
                bin: self.bin.clone(),
                pid_file,
                state: tokio::sync::Mutex::new(PodmanExecState {
                    child: Some(child),
                    status: None,
                }),
            }),
            channel: Box::new(SplitChannel::new(stdout, stdin)),
        })
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // Process-as-container: reach the agent's stdio over its published port (a
        // network dial, not `podman exec`) — the same seam Docker/K8s use. Retry the
        // port lookup + dial with a short backoff so the FIRST turn on a cold container
        // does not race the agent's port bind; bounded (~6s) so a dead agent fails closed.
        let mut last: Option<RuntimeError> = None;
        for _ in 0..40 {
            match self.agent_addr(container_id).await {
                Ok(addr) => match TcpAgentTransport::new(addr).open_channel().await {
                    Ok(channel) => return Ok(channel),
                    Err(e) => last = Some(backend(e)),
                },
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        Err(last.unwrap_or_else(|| backend("agent channel never became reachable")))
    }

    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        match self
            .run(&[
                "inspect".into(),
                "-f".into(),
                "{{.State.Running}}".into(),
                container_id.into(),
            ])
            .await
        {
            Ok(s) if s.trim() == "true" => Ok(ContainerState::Running),
            // Not running or not found → gone (adoption reconciles this to an orphan).
            _ => Ok(ContainerState::Gone),
        }
    }

    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        let code = self
            .run(&["wait".into(), container_id.into()])
            .await?
            .trim()
            .parse::<i32>()
            .map_err(|e| backend(format!("bad exit code: {e}")))?;
        Ok(pc::ExitStatus {
            code: Some(code),
            signaled: false,
        })
    }

    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        let s = self
            .run(&[
                "inspect".into(),
                "-f".into(),
                "{{.State.Status}} {{.State.ExitCode}}".into(),
                container_id.into(),
            ])
            .await?;
        let mut it = s.split_whitespace();
        let status = it.next().unwrap_or_default();
        if status == "running" {
            return Ok(None);
        }
        let code = it.next().and_then(|c| c.parse::<i32>().ok());
        Ok(Some(pc::ExitStatus {
            code,
            signaled: false,
        }))
    }

    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        self.run(&[
            "kill".into(),
            "--signal".into(),
            signal_flag(signal).into(),
            container_id.into(),
        ])
        .await
        .map(|_| ())
    }

    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        // Out-of-band (like the Docker adapter): outputs are listed from the volume /
        // object store by the deployment, not streamed through the CLI.
        Ok(Vec::new())
    }

    async fn read_artifact(
        &self,
        container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        // Copy the file out as a tar stream to stdout (`podman cp <cid>:<path> -`),
        // mirroring the Docker adapter's tar-stream fallback.
        let out = self
            .exec
            .exec(
                &self.bin,
                &[
                    "cp".into(),
                    format!("{container_id}:{artifact_id}"),
                    "-".into(),
                ],
            )
            .await
            .map_err(backend)?;
        if !out.ok {
            return Err(backend(String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(out.stdout)
    }

    async fn touch_lease(&self, container_id: &str) -> Result<(), RuntimeError> {
        if self.inspect(container_id).await? != ContainerState::Running {
            return Err(RuntimeError::NotFound(container_id.into()));
        }
        // `podman ps` reports the canonical container ID while handles may carry a
        // stable name. Protect both aliases so list/reap cannot miss an adoption.
        let canonical = self
            .run(&[
                "inspect".into(),
                "-f".into(),
                "{{.Id}}".into(),
                container_id.into(),
            ])
            .await?;
        if canonical.is_empty() {
            return Err(RuntimeError::NotFound(container_id.into()));
        }
        let mut adopted = self.adopted.lock().unwrap();
        adopted.insert(container_id.to_string());
        adopted.insert(canonical);
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        let result = self
            .run(&["rm".into(), "-f".into(), container_id.into()])
            .await
            .map(|_| ());
        if result.is_ok() {
            self.adopted.lock().unwrap().remove(container_id);
        }
        result
    }

    async fn list_managed(&self) -> Result<Vec<ManagedContainer>, RuntimeError> {
        // Discover every awaken-labeled container (running or stopped) for the reaper.
        // `-a` includes exited ones (finished work); JSON is the stable machine format.
        let json = self
            .run(&[
                "ps".into(),
                "-a".into(),
                "--filter".into(),
                format!("label={REAPER_LABEL}=1"),
                "--format".into(),
                "json".into(),
            ])
            .await?;
        if json.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&json).map_err(|e| RuntimeError::Backend(e.to_string()))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                // Podman ps json: `Id` (string), `State` (string e.g. "running"/"exited"),
                // `Created` (unix seconds). Field names are stable across podman 3/4/5.
                let id = r.get("Id")?.as_str()?.to_string();
                let running = r
                    .get("State")
                    .and_then(|s| s.as_str())
                    .map(|s| s.eq_ignore_ascii_case("running"))
                    .unwrap_or(false);
                let age_secs = r
                    .get("Created")
                    .and_then(serde_json::Value::as_i64)
                    .map(|created| now.saturating_sub(created.max(0) as u64))
                    .unwrap_or(0);
                let owned_by_label = r
                    .get("Labels")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|labels| labels.get(REAPER_OWNER_LABEL))
                    .and_then(serde_json::Value::as_str)
                    == Some(self.owner_id.as_str());
                let owned_by_adoption = self.adopted.lock().unwrap().contains(&id);
                Some(ManagedContainer {
                    id,
                    owned_by_current_runtime: owned_by_label || owned_by_adoption,
                    running,
                    age_secs,
                })
            })
            .collect())
    }
}

#[async_trait]
impl PackageImageProvisioner for PodmanRuntime {
    async fn package_image_coordination_key(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        if packages.is_empty() {
            return serde_json::to_string(&(base_image, packages, network)).map_err(backend);
        }
        let (_, _, image) = self.resolve_package_build(base_image, packages).await?;
        serde_json::to_string(&(image, network)).map_err(backend)
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        ContainerRuntime::prepare_package_image(self, base_image, packages, network).await
    }

    async fn package_image_available(&self, image: &str) -> Result<bool, RuntimeError> {
        if self.package_registry.is_none() {
            return Ok(self
                .run(&["image".into(), "exists".into(), image.to_string()])
                .await
                .is_ok());
        }
        Ok(self
            .run(&self.registry_command("pull", image))
            .await
            .is_ok())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use awaken_provisioning_contract::ProcessHandle;

    use crate::{NetworkMode, RootfsPlan};

    use super::*;

    struct FixedBroker;

    #[async_trait]
    impl pc::SecretBroker for FixedBroker {
        async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Ok(b"podman-secret".to_vec())
        }

        async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
            self.materialize(reference).await
        }

        async fn write_back(
            &self,
            _reference: &str,
            _bytes: Vec<u8>,
        ) -> Result<(), pc::SandboxError> {
            Err(pc::SandboxError::new("not supported"))
        }
    }

    async fn materialized(command: pc::Command) -> pc::MaterializedCommand {
        pc::materialize_process_command(&[], command, None)
            .await
            .unwrap()
    }

    /// A scripted [`CommandExec`]: a handler maps `(bin, args)` to a canned output,
    /// and every invocation's argv is recorded so tests can assert what was run.
    type CommandHandler = dyn Fn(&[String]) -> CmdOutput + Send + Sync;

    struct FakeExec {
        handler: Box<CommandHandler>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl CommandExec for FakeExec {
        async fn exec(&self, _bin: &str, args: &[String]) -> std::io::Result<CmdOutput> {
            self.calls.lock().unwrap().push(args.to_vec());
            Ok((self.handler)(args))
        }
    }

    fn ok(stdout: &str) -> CmdOutput {
        CmdOutput {
            ok: true,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn err(stderr: &str) -> CmdOutput {
        CmdOutput {
            ok: false,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// Build a runtime whose executor replies per `handler`, plus a handle to the
    /// recorded argv list.
    fn runtime_with(
        port: u16,
        handler: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static,
    ) -> (PodmanRuntime, Arc<FakeExec>) {
        let fake = Arc::new(FakeExec {
            handler: Box::new(handler),
            calls: Mutex::new(Vec::new()),
        });
        (PodmanRuntime::with_exec(port, fake.clone()), fake)
    }

    fn plan() -> ContainerPlan {
        ContainerPlan {
            image: "img:latest".into(),
            command: vec!["/agent".into()],
            env: vec![],
            packages: Default::default(),
            binds: vec![],
            outputs_volume: "/out".into(),
            network: NetworkMode::None,
            limits: pc::ResourceLimits::default(),
            memory_mounts: vec![],
            rootfs: RootfsPlan::Image("img:latest".into()),
        }
    }

    #[test]
    fn signal_flag_maps_every_signal() {
        assert_eq!(signal_flag(pc::Signal::Term), "TERM");
        assert_eq!(signal_flag(pc::Signal::Kill), "KILL");
        assert_eq!(signal_flag(pc::Signal::Int), "INT");
    }

    #[test]
    fn executable_is_constructor_owned_without_ambient_precedence() {
        // Cause/effect table:
        // | constructor input | executable |
        // | default | `podman` on PATH |
        // | explicit typed deployment value | exact supplied path |
        let rt = PodmanRuntime::new(9000);
        assert_eq!(rt.agent_port, 9000);
        assert_eq!(rt.bin, "podman");
        assert_eq!(
            PodmanRuntime::with_bin(9000, "/opt/podman").bin,
            "/opt/podman"
        );
    }

    #[tokio::test]
    async fn run_maps_a_nonzero_exit_to_a_backend_error_naming_the_subcommand() {
        let (rt, _) = runtime_with(9000, |_| err("boom"));
        let e = rt.run(&["info".into()]).await.unwrap_err();
        assert!(
            matches!(e, RuntimeError::Backend(m) if m.contains("podman info") && m.contains("boom"))
        );
    }

    #[tokio::test]
    async fn ping_succeeds_when_the_binary_responds() {
        let (rt, _) = runtime_with(9000, |_| ok("x86_64"));
        assert!(rt.ping().await.is_ok());
    }

    #[test]
    fn registry_auth_file_is_scoped_to_pull_and_push() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let rt = PodmanRuntime::with_exec(
            9000,
            Arc::new(FakeExec {
                handler: Box::new(|_| ok("")),
                calls: Mutex::new(Vec::new()),
            }),
        )
        .with_package_registry("registry.internal")
        .with_package_registry_auth_file(file.path())
        .unwrap();
        let pull = rt.registry_command("pull", "registry.internal/awaken-packages@sha256:abc");
        assert_eq!(pull[0], "pull");
        assert_eq!(pull[1], "--authfile");
        assert_eq!(pull[2], file.path().to_string_lossy());
        assert_eq!(pull[3], "registry.internal/awaken-packages@sha256:abc");
    }

    #[tokio::test]
    async fn registry_mode_repairs_a_missing_remote_from_the_local_cache() {
        let (rt, fake) = runtime_with(9000, |args| match args.first().map(String::as_str) {
            Some("pull") => err("manifest unknown"),
            Some("push") => ok(""),
            Some("image") if args.get(1).map(String::as_str) == Some("exists") => ok(""),
            Some("image") if args.iter().any(|arg| arg.contains("RepoDigests")) => {
                ok("registry.internal/awaken-packages@sha256:remote")
            }
            Some("image") => ok("sha256:exact-base"),
            other => panic!("unexpected podman command: {other:?}"),
        });
        let rt = rt.with_package_registry("registry.internal");
        let requirements = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let image = ContainerRuntime::prepare_package_image(
            &rt,
            "python:3.13",
            &requirements,
            &pc::NetworkPolicy::Unrestricted,
        )
        .await
        .unwrap();
        assert_eq!(image, "registry.internal/awaken-packages@sha256:remote");
        let calls = fake.calls.lock().unwrap();
        let pull = calls
            .iter()
            .position(|args| args.first().map(String::as_str) == Some("pull"))
            .unwrap();
        let push = calls
            .iter()
            .position(|args| args.first().map(String::as_str) == Some("push"))
            .unwrap();
        assert!(pull < push, "remote probe must precede repair push");
        assert!(
            !calls
                .iter()
                .any(|args| args.first().map(String::as_str) == Some("build")),
            "a deterministic local hit repairs the registry without rebuilding"
        );
    }

    /// Podman package-image cause graph:
    /// mutable base reference -> exact local image ID; exact ID + exact package
    /// requirements -> content-addressed Containerfile/tag -> cache probe.
    /// Cache miss builds exactly once; cache hit performs no build. A workload
    /// container is never created by this operation.
    ///
    /// | Rule | base inspect | cache | observable behavior |
    /// |---|---|---|---|
    /// | P1 | exact ID | miss | build once FROM exact ID; return derived ref |
    /// | P2 | exact ID | hit | return derived ref without a build |
    /// | P3 | missing/empty | n/a | fail before cache probe/build |
    #[tokio::test]
    async fn package_requirements_build_one_content_addressed_image_on_cache_miss() {
        let captured = Arc::new(Mutex::new(None::<String>));
        let captured_build = captured.clone();
        let (rt, fake) = runtime_with(9000, move |args| {
            match (
                args.first().map(String::as_str),
                args.get(1).map(String::as_str),
            ) {
                (Some("image"), Some("inspect")) => ok("sha256:exact-base"),
                (Some("image"), Some("exists")) => err("not found"),
                (Some("build"), _) => {
                    let file = args
                        .iter()
                        .position(|arg| arg == "--file")
                        .and_then(|index| args.get(index + 1))
                        .expect("build carries Containerfile");
                    *captured_build.lock().unwrap() = Some(
                        std::fs::read_to_string(file).expect("Containerfile exists during build"),
                    );
                    ok("")
                }
                other => panic!("unexpected podman command: {other:?}"),
            }
        });
        let requirements = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let image = ContainerRuntime::prepare_package_image(
            &rt,
            "python:3.13",
            &requirements,
            &pc::NetworkPolicy::Unrestricted,
        )
        .await
        .expect("cache miss builds");
        assert!(image.starts_with("localhost/awaken-packages:"));
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 3, "P1: inspect, cache probe, then build");
        assert_eq!(&calls[0][..2], &["image", "inspect"]);
        assert_eq!(&calls[1][..2], &["image", "exists"]);
        assert_eq!(calls[2].first().map(String::as_str), Some("build"));
        let file = captured.lock().unwrap().clone().unwrap();
        assert!(file.starts_with("FROM sha256:exact-base\n"), "P1: {file}");
        assert!(
            file.contains(
                r#"RUN ["/usr/bin/env","pip","install","--no-cache-dir","httpx==0.28.0"]"#
            )
        );
    }

    #[tokio::test]
    async fn package_image_cache_hit_does_not_build_or_create_a_workload() {
        let (rt, fake) = runtime_with(9000, |args| {
            match (
                args.first().map(String::as_str),
                args.get(1).map(String::as_str),
            ) {
                (Some("image"), Some("inspect")) => ok("sha256:exact-base"),
                (Some("image"), Some("exists")) => ok(""),
                other => panic!("P2 forbids build/run calls: {other:?}"),
            }
        });
        let requirements = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let image = ContainerRuntime::prepare_package_image(
            &rt,
            "python:3.13",
            &requirements,
            &pc::NetworkPolicy::Unrestricted,
        )
        .await
        .expect("P2 cache hit");

        assert!(image.starts_with("localhost/awaken-packages:"));
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "P2: inspect and cache probe only");
        assert_eq!(&calls[0][..2], &["image", "inspect"]);
        assert_eq!(&calls[1][..2], &["image", "exists"]);
    }

    #[tokio::test]
    async fn missing_base_identity_fails_before_cache_or_build_side_effects() {
        let (rt, fake) = runtime_with(9000, |args| {
            match (
                args.first().map(String::as_str),
                args.get(1).map(String::as_str),
            ) {
                (Some("image"), Some("inspect")) => ok(""),
                other => panic!("P3 forbids cache/build calls: {other:?}"),
            }
        });
        let requirements = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let error = ContainerRuntime::prepare_package_image(
            &rt,
            "missing:latest",
            &requirements,
            &pc::NetworkPolicy::Unrestricted,
        )
        .await
        .expect_err("P3 empty identity fails closed");

        assert!(error.to_string().contains("empty base-image identity"));
        assert_eq!(fake.calls.lock().unwrap().len(), 1, "P3: inspect only");
    }

    #[tokio::test]
    async fn create_clears_a_stale_container_then_publishes_the_agent_port() {
        let (rt, fake) = runtime_with(7777, |_| ok(""));
        let expected = runtime_container_name(&rt.owner_id, "s1");
        let name = rt.create("s1", &plan()).await.unwrap();
        assert_eq!(name, expected);
        let calls = fake.calls.lock().unwrap();
        // First call is the idempotent removal of this runtime instance's name.
        assert_eq!(calls[0], vec!["rm", "-f", expected.as_str()]);
        // The `run` argv stamps this worker instance's ownership and publishes the
        // agent port right after its daemon-global name.
        let run = &calls[1];
        let name_at = run.iter().position(|a| a == &expected).unwrap();
        assert_eq!(run[name_at + 1], "--label");
        assert!(run[name_at + 2].starts_with(&format!("{REAPER_OWNER_LABEL}=")));
        assert_eq!(run[name_at + 3], "-p");
        assert_eq!(run[name_at + 4], "127.0.0.1::7777");
    }

    #[tokio::test]
    async fn agent_addr_parses_the_published_host_port_taking_the_first_binding() {
        let (rt, _) = runtime_with(9000, |_| ok("127.0.0.1:49153\n[::]:49153"));
        let addr = rt.agent_addr("cid").await.unwrap();
        assert_eq!(addr, "127.0.0.1:49153".parse().unwrap());
    }

    #[tokio::test]
    async fn agent_addr_errs_when_nothing_is_published_yet() {
        let (rt, _) = runtime_with(9000, |_| ok(""));
        assert!(rt.agent_addr("cid").await.is_err());
    }

    /// Cold-start bounded-retry-then-fail-closed: when the agent's port is NEVER
    /// published (`podman port` keeps returning empty), `open_channel` must retry a
    /// BOUNDED number of times and then fail closed rather than spin forever — so a
    /// genuinely dead agent still surfaces an error. Driven entirely through the scripted
    /// `CommandExec` (no daemon, no binary); `start_paused` auto-advances the backoff so
    /// the ~6s bound resolves instantly and deterministically. This exercises the SAME
    /// loop shape the (non-injectable, bollard-bound) `docker::open_channel` runs.
    #[tokio::test(start_paused = true)]
    async fn open_channel_retries_a_bounded_number_then_fails_closed() {
        // Every `podman port` reports nothing published → agent_addr errs each attempt.
        let (rt, fake) = runtime_with(9000, |_| ok(""));
        let e = rt.open_channel("cid").await;
        assert!(
            e.is_err(),
            "a never-reachable agent must fail closed, not hang"
        );
        // The retry is bounded (the loop is `for _ in 0..40`): exactly 40 port lookups
        // were attempted, then it gave up — never an unbounded spin.
        let port_attempts = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|argv| argv.first().map(String::as_str) == Some("port"))
            .count();
        assert_eq!(
            port_attempts, 40,
            "open_channel must retry a bounded number of times then fail closed"
        );
    }

    #[tokio::test]
    async fn inspect_reads_running_true_as_running_and_anything_else_as_gone() {
        let (running, _) = runtime_with(9000, |_| ok("true"));
        assert!(matches!(
            running.inspect("cid").await.unwrap(),
            ContainerState::Running
        ));
        let (stopped, _) = runtime_with(9000, |_| ok("false"));
        assert!(matches!(
            stopped.inspect("cid").await.unwrap(),
            ContainerState::Gone
        ));
        let (missing, _) = runtime_with(9000, |_| err("no such container"));
        assert!(matches!(
            missing.inspect("cid").await.unwrap(),
            ContainerState::Gone
        ));
    }

    #[tokio::test]
    async fn wait_parses_the_exit_code_and_rejects_garbage() {
        let (rt, _) = runtime_with(9000, |_| ok("0"));
        assert_eq!(rt.wait("cid").await.unwrap().code, Some(0));
        let (bad, _) = runtime_with(9000, |_| ok("not-a-number"));
        assert!(bad.wait("cid").await.is_err());
    }

    #[tokio::test]
    async fn poll_is_none_while_running_and_carries_the_code_once_exited() {
        let (running, _) = runtime_with(9000, |_| ok("running 0"));
        assert_eq!(running.poll("cid").await.unwrap(), None);
        let (exited, _) = runtime_with(9000, |_| ok("exited 3"));
        assert_eq!(exited.poll("cid").await.unwrap().unwrap().code, Some(3));
        // Malformed second field → code None, still terminal.
        let (weird, _) = runtime_with(9000, |_| ok("exited"));
        assert_eq!(weird.poll("cid").await.unwrap().unwrap().code, None);
    }

    #[tokio::test]
    async fn signal_forwards_the_mapped_flag() {
        let (rt, fake) = runtime_with(9000, |_| ok(""));
        rt.signal("cid", pc::Signal::Kill).await.unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap().last().unwrap(),
            vec!["kill", "--signal", "KILL", "cid"]
        );
    }

    #[tokio::test]
    async fn artifacts_are_out_of_band_and_touch_lease_claims_a_live_container() {
        let (rt, _) = runtime_with(9000, |_| ok("true"));
        assert!(rt.artifacts("cid").await.unwrap().is_empty());
        assert!(rt.touch_lease("cid").await.is_ok());
        assert!(rt.adopted.lock().unwrap().contains("cid"));
    }

    #[tokio::test]
    async fn read_artifact_returns_the_tar_stream_or_maps_the_error() {
        let (rt, fake) = runtime_with(9000, |_| ok("TARBYTES"));
        assert_eq!(
            rt.read_artifact("cid", "/out/f").await.unwrap(),
            b"TARBYTES"
        );
        assert_eq!(
            *fake.calls.lock().unwrap().last().unwrap(),
            vec!["cp", "cid:/out/f", "-"]
        );
        let (missing, _) = runtime_with(9000, |_| err("no such file"));
        assert!(missing.read_artifact("cid", "/nope").await.is_err());
    }

    #[tokio::test]
    async fn remove_force_deletes_the_container() {
        let (rt, fake) = runtime_with(9000, |_| ok(""));
        rt.remove("cid").await.unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap().last().unwrap(),
            vec!["rm", "-f", "cid"]
        );
    }

    fn exec_process(child: Option<Child>, bin: &str) -> PodmanExecProcess {
        PodmanExecProcess {
            id: "exec-test".into(),
            container_id: "container-test".into(),
            bin: bin.into(),
            pid_file: "/tmp/does-not-matter-for-scripted-bin".into(),
            state: tokio::sync::Mutex::new(PodmanExecState {
                child,
                status: None,
            }),
        }
    }

    #[tokio::test]
    async fn exec_process_wait_and_poll_cache_the_terminal_status() {
        let child = OsCommand::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("spawn fixture");
        let process = exec_process(Some(child), "true");
        assert_eq!(process.id(), "exec-test");
        assert_eq!(process.wait().await.unwrap().code, Some(7));
        assert_eq!(process.wait().await.unwrap().code, Some(7));
        assert_eq!(process.poll().await.unwrap().unwrap().code, Some(7));
    }

    #[tokio::test]
    async fn exec_process_poll_reports_running_then_terminal() {
        let child = OsCommand::new("sh")
            .args(["-c", "sleep 0.05; exit 3"])
            .spawn()
            .expect("spawn fixture");
        let process = exec_process(Some(child), "true");
        assert_eq!(process.poll().await.unwrap(), None);
        assert_eq!(process.wait().await.unwrap().code, Some(3));
    }

    #[tokio::test]
    async fn detached_exec_process_fails_closed_and_signal_propagates_status() {
        let detached = exec_process(None, "true");
        assert!(detached.wait().await.is_err());
        assert!(detached.poll().await.is_err());
        detached.signal(pc::Signal::Term).await.unwrap();

        let failing_signal = exec_process(None, "false");
        assert!(failing_signal.signal(pc::Signal::Int).await.is_err());

        let missing_binary = exec_process(None, "/definitely/missing/podman");
        assert!(missing_binary.signal(pc::Signal::Kill).await.is_err());
    }

    #[tokio::test]
    async fn exec_admission_rejects_empty_and_unsupported_piped_commands() {
        let (rt, _) = runtime_with(9000, |_| ok(""));

        assert!(
            rt.exec_process(
                "cid",
                pc::MaterializedCommand::new(Vec::<String>::new()),
                false
            )
            .is_err()
        );

        let mut piped = pc::MaterializedCommand::new(["echo", "value"]);
        piped.stdio = pc::Stdio::Piped;
        assert!(rt.exec_process("cid", piped, false).is_err());
    }

    #[tokio::test]
    async fn spawn_and_attached_spawn_cover_stdio_cwd_and_inline_environment() {
        let (mut rt, _) = runtime_with(9000, |_| ok(""));
        // `true` is a deterministic stand-in for the Podman CLI. It ignores the
        // assembled `exec ...` argv while preserving the exact child stdio shape.
        rt.bin = "true".into();

        let mut inherited = pc::Command::new(["echo", "inherited"]);
        inherited.cwd = "/workspace".into();
        inherited.env.push(pc::EnvVar {
            name: "MODE".into(),
            value: pc::EnvValue::Inline {
                value: "test".into(),
            },
            visibility: pc::EnvVisibility::Process,
        });
        let inherited = materialized(inherited).await;
        let inherited = rt.spawn("cid", inherited).await.unwrap();
        assert_eq!(inherited.wait().await.unwrap().code, Some(0));

        let mut null = pc::MaterializedCommand::new(["echo", "discarded"]);
        null.stdio = pc::Stdio::Null;
        let null = rt.spawn("cid", null).await.unwrap();
        assert_eq!(null.wait().await.unwrap().code, Some(0));

        let mut piped = pc::MaterializedCommand::new(["agent", "--stdio"]);
        piped.stdio = pc::Stdio::Piped;
        let attached = rt.spawn_agent("cid", piped).await.unwrap();
        assert_eq!(attached.process.wait().await.unwrap().code, Some(0));
    }

    /// Podman secret-delivery cause graph:
    ///
    /// C1 command contains a brokered process secret -> C2 the Worker resolves it
    /// -> C3 the Podman adapter forwards only the variable name in argv and places
    /// the value in the child environment -> E1 the target receives the value while
    /// the host command line remains secret-free. Any value in argv is E2/failure.
    ///
    /// | Rule | C1 | C2 | value in argv | value in child env | Result |
    /// |---|---|---|---|---|---|
    /// | D1 | T | T | F | T | launch succeeds |
    /// | D2 | T | T | T | * | helper rejects observation |
    #[cfg(unix)]
    #[tokio::test]
    async fn process_secret_is_forwarded_by_name_without_entering_podman_argv() {
        use std::os::unix::fs::PermissionsExt;

        let script = std::env::temp_dir().join(format!(
            "awaken-podman-secret-check-{}-{}",
            std::process::id(),
            EXEC_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \" $* \" in *podman-secret*) exit 91;; esac\n[ \"$TOKEN\" = podman-secret ] || exit 92\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let (mut rt, _) = runtime_with(9000, |_| ok(""));
        rt.bin = script.to_string_lossy().into_owned();
        let mut command = pc::Command::new(["echo", "value"]);
        command.stdio = pc::Stdio::Null;
        command.env.push(pc::EnvVar {
            name: "TOKEN".into(),
            value: pc::EnvValue::Secret {
                reference: "lease://exact".into(),
            },
            visibility: pc::EnvVisibility::Process,
        });
        let broker: Arc<dyn pc::SecretBroker> = Arc::new(FixedBroker);
        let command = pc::materialize_process_command(&[], command, Some(&broker))
            .await
            .unwrap();
        let process = rt.spawn("cid", command).await.unwrap();
        assert_eq!(process.wait().await.unwrap().code, Some(0));

        std::fs::remove_file(script).unwrap();
    }
}
