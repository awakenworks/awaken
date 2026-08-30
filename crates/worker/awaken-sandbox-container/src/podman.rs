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
use tokio::process::Child;
#[cfg(test)]
use tokio::process::Command as OsCommand;

use crate::net::TcpAgentTransport;

mod command;
mod realization;
use crate::runtime::{
    ExistingRealization, ExistingRealizationDecision, ExistingRealizationPhase,
    ExistingRealizationRecovery, PhysicalIncarnation, RebuildContinuityEvidence,
    container_state_observation, existing_realization_decision, legacy_unfenced_fingerprint,
    sandbox_observation,
};
use crate::{
    ContainerCreateAttempt, ContainerPlan, ContainerRealizationContext, ContainerRealizationIntent,
    ContainerRealizationNamespace, ContainerRuntime, ContainerState, MANAGED_SANDBOX_LABEL,
    PackageImageProvisioner, RUNTIME_OWNER_LABEL, RuntimeAgentProcess, RuntimeError,
    RuntimeRestoreTarget, SANDBOX_ADOPTION_LABEL, SANDBOX_ATTEMPT_LABEL,
    SANDBOX_EFFECT_EPOCH_LABEL, SANDBOX_EFFECT_EXPIRY_LABEL, SANDBOX_EFFECT_LABEL,
    SANDBOX_EFFECT_OWNER_LABEL, SANDBOX_EFFECT_RUNTIME_LABEL, SANDBOX_REALIZATION_LABEL,
    SANDBOX_SCOPE_LABEL, container_effect_fence_from_values, container_effect_label_values,
    podman_run_argv, restoration_metadata, restoration_plan_fingerprint, restore_container_name,
    runtime_container_name, sandbox_scope_identity,
};
use command::podman_command;
#[cfg(test)]
use command::rootless_systemd_bus;

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
        if self.poll().await?.is_some() {
            return Ok(());
        }
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
        let status = podman_command(&self.bin)
            .args(["exec", &self.container_id, "sh", "-c", &script])
            .status()
            .await
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if status.success() {
            Ok(())
        } else if self.poll().await?.is_some() {
            // The process may exit naturally between the preflight poll and the
            // container-side kill. Signalling an already-exited owned process is
            // idempotent across every backend.
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
    pub status_code: Option<i32>,
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
        let out = podman_command(bin).args(args).output().await?;
        Ok(CmdOutput {
            ok: out.status.success(),
            status_code: out.status.code(),
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
    realization_namespace: ContainerRealizationNamespace,
    owner_id: String,
    package_builds: tokio::sync::Mutex<()>,
    package_registry: Option<String>,
    package_registry_auth_file: Option<PathBuf>,
    package_cache_ttl: Option<std::time::Duration>,
    package_build_timeout: std::time::Duration,
}

#[derive(serde::Deserialize)]
struct PodmanMount {
    #[serde(rename = "Source", alias = "source")]
    source: PathBuf,
    #[serde(rename = "Destination", alias = "destination")]
    destination: String,
}

#[derive(serde::Deserialize)]
struct PodmanContainerSummary {
    #[serde(rename = "Id", alias = "ID", alias = "id")]
    id: String,
    #[serde(rename = "State", alias = "state")]
    state: String,
    #[serde(rename = "Labels", alias = "labels", default)]
    labels: std::collections::HashMap<String, String>,
}

fn podman_realizations(encoded: &str) -> Result<Vec<ExistingRealization>, RuntimeError> {
    let summaries: Vec<PodmanContainerSummary> =
        serde_json::from_str(if encoded.is_empty() { "[]" } else { encoded }).map_err(backend)?;
    summaries
        .into_iter()
        .map(|summary| {
            if summary.id.is_empty()
                || summary
                    .labels
                    .get(MANAGED_SANDBOX_LABEL)
                    .map(String::as_str)
                    != Some("1")
            {
                return Err(backend(
                    "Podman query returned a non-Awaken container or empty id",
                ));
            }
            let phase = match summary.state.to_ascii_lowercase().as_str() {
                "created" | "configured" => ExistingRealizationPhase::Creating,
                "running" => ExistingRealizationPhase::Ready,
                "exited" | "stopped" => ExistingRealizationPhase::Terminal,
                _ => ExistingRealizationPhase::Indeterminate,
            };
            let locator = summary.id;
            Ok(ExistingRealization {
                locator: locator.clone(),
                incarnation: PhysicalIncarnation {
                    identity: locator,
                    version: None,
                },
                adoption_fingerprint: summary.labels.get(SANDBOX_ADOPTION_LABEL).cloned(),
                fingerprint: summary.labels.get(SANDBOX_REALIZATION_LABEL).cloned(),
                fence: container_effect_fence_from_values(
                    summary.labels.get(SANDBOX_EFFECT_LABEL).map(String::as_str),
                    summary
                        .labels
                        .get(SANDBOX_EFFECT_OWNER_LABEL)
                        .map(String::as_str),
                    summary
                        .labels
                        .get(SANDBOX_EFFECT_RUNTIME_LABEL)
                        .map(String::as_str),
                    summary
                        .labels
                        .get(SANDBOX_EFFECT_EPOCH_LABEL)
                        .map(String::as_str),
                    summary
                        .labels
                        .get(SANDBOX_EFFECT_EXPIRY_LABEL)
                        .map(String::as_str),
                )?,
                attempt_id: summary.labels.get(SANDBOX_ATTEMPT_LABEL).cloned(),
                recovery: ExistingRealizationRecovery::CurrentAttemptOnly,
                phase,
            })
        })
        .collect()
}

impl PodmanRuntime {
    /// Use `podman` from `PATH`.
    #[must_use]
    pub fn new(agent_port: u16) -> Self {
        Self::with_bin(agent_port, "podman")
    }

    /// Use `podman` with the stable deployment namespace of durable Sessions.
    #[must_use]
    pub fn for_realization(
        realization_namespace: ContainerRealizationNamespace,
        agent_port: u16,
    ) -> Self {
        Self::with_bin_for_realization(realization_namespace, agent_port, "podman")
    }

    /// Use the deployment-selected Podman executable.
    #[must_use]
    pub fn with_bin(agent_port: u16, bin: impl Into<String>) -> Self {
        let realization_namespace =
            ContainerRealizationNamespace::from_stable_parts(["legacy-podman-runtime"])
                .expect("constant legacy Podman namespace is valid");
        Self::with_bin_for_realization(realization_namespace, agent_port, bin)
    }

    /// Use the deployment-selected executable with a durable Session namespace.
    #[must_use]
    pub fn with_bin_for_realization(
        realization_namespace: ContainerRealizationNamespace,
        agent_port: u16,
        bin: impl Into<String>,
    ) -> Self {
        Self {
            bin: bin.into(),
            agent_port,
            exec: Arc::new(OsCommandExec),
            realization_namespace,
            owner_id: crate::runtime_owner_id(),
            package_builds: tokio::sync::Mutex::new(()),
            package_registry: None,
            package_registry_auth_file: None,
            package_cache_ttl: None,
            package_build_timeout: crate::packages::PACKAGE_BUILD_TIMEOUT,
        }
    }

    /// Wire a scripted executor (tests) instead of forking a real `podman`.
    #[cfg(test)]
    fn with_exec(
        realization_namespace: ContainerRealizationNamespace,
        agent_port: u16,
        exec: Arc<dyn CommandExec>,
    ) -> Self {
        Self {
            bin: "podman".into(),
            agent_port,
            exec,
            realization_namespace,
            owner_id: crate::runtime_owner_id(),
            package_builds: tokio::sync::Mutex::new(()),
            package_registry: None,
            package_registry_auth_file: None,
            package_cache_ttl: None,
            package_build_timeout: crate::packages::PACKAGE_BUILD_TIMEOUT,
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

    /// Bound one registry lookup/build/push transaction. A timeout fails the
    /// Environment activation closed; it never selects the unmodified base image.
    #[must_use]
    pub fn with_package_build_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.package_build_timeout = timeout;
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

    async fn existing_realizations(
        &self,
        scope: &str,
    ) -> Result<Vec<ExistingRealization>, RuntimeError> {
        let scope_identity = sandbox_scope_identity(self.realization_namespace.as_str(), scope)?;
        let encoded = self
            .run(&[
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={MANAGED_SANDBOX_LABEL}=1"),
                "--filter".into(),
                format!("label={SANDBOX_SCOPE_LABEL}={scope_identity}"),
                "--format".into(),
                "json".into(),
            ])
            .await?;
        podman_realizations(&encoded)
    }

    async fn create_decision(
        &self,
        context: &ContainerRealizationContext<'_>,
        realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<ExistingRealizationDecision, RuntimeError> {
        existing_realization_decision(
            context,
            realization_fingerprint,
            RebuildContinuityEvidence::Unavailable,
            &self.existing_realizations(context.scope).await?,
        )
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
        let mut process = podman_command(&self.bin);
        let mut secret_bindings = Vec::new();
        for var in &command.env {
            match &var.value {
                pc::MaterializedEnvValue::Inline(value) => {
                    // Inline values are explicitly non-secret. Passing them on
                    // the Podman argv keeps runtime-owned HOME/XDG values out of
                    // the Podman CLI process environment, where they would
                    // otherwise redirect Podman's own storage and config roots.
                    args.extend(["--env".into(), format!("{}={value}", var.name)]);
                }
                pc::MaterializedEnvValue::Secret(_) => {
                    // A bare `--env NAME` copies NAME from the Podman process.
                    // Never use the target name there: HOME/XDG/CONTAINERS_*
                    // alter the rootless Podman client before the container exec
                    // starts. Carry the secret under an adapter-owned alias, then
                    // restore the target name inside the container wrapper.
                    let mut alias = format!("AWAKEN_PODMAN_EXEC_SECRET_{}", secret_bindings.len());
                    while command.env.iter().any(|candidate| candidate.name == alias) {
                        alias.push('_');
                    }
                    args.extend(["--env".into(), alias.clone()]);
                    process.env(&alias, var.value.expose());
                    secret_bindings.push((alias, var.name.clone()));
                }
            }
        }
        let mut wrapper = "set -e; pid_file=$1; shift;".to_string();
        for (alias, _) in &secret_bindings {
            wrapper.push_str(&format!(
                " export \"$1=${{{alias}}}\"; unset {alias}; shift;"
            ));
        }
        wrapper.push_str(" printf '%s' \"$$\" > \"$pid_file\"; exec \"$@\"");
        args.extend([
            container_id.to_string(),
            "sh".into(),
            "-c".into(),
            wrapper,
            "awaken-exec".into(),
            pid_file.clone(),
        ]);
        args.extend(secret_bindings.into_iter().map(|(_, target)| target));
        args.extend(command.argv);
        process.args(args);
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
    fn realization_configuration(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, RuntimeError> {
        Ok(std::collections::BTreeMap::from([
            ("backend".into(), "podman".into()),
            ("agent_port".into(), self.agent_port.to_string()),
        ]))
    }

    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        self.ping().await
    }

    fn enforces_network_none(&self) -> bool {
        true
    }

    fn supports_package_provisioning(&self) -> bool {
        true
    }

    fn uses_host_live_input_bind(&self) -> bool {
        true
    }

    async fn project_live_input(
        &self,
        container_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeError> {
        let root = self.live_inputs_root(container_id).await?;
        crate::live_inputs::project_host_input(&root, path, bytes)
    }

    async fn remove_live_input(&self, container_id: &str, path: &str) -> Result<(), RuntimeError> {
        let root = self.live_inputs_root(container_id).await?;
        crate::live_inputs::remove_host_input(&root, path)
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        let operation = async {
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
            let root = crate::staging_dir(&mut guard, &format!("package-{fingerprint}"))
                .map_err(backend)?;
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
        };
        tokio::time::timeout(self.package_build_timeout, operation)
            .await
            .map_err(|_| backend("package image preparation exceeded its deadline"))?
    }

    async fn preflight_create_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        _plan: &ContainerPlan,
        realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<(), RuntimeError> {
        self.create_decision(context, realization_fingerprint)
            .await
            .map(drop)
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let fingerprint = legacy_unfenced_fingerprint(id);
        let attempt = ContainerCreateAttempt::fresh();
        let intent = ContainerRealizationIntent::Create;
        let context = ContainerRealizationContext::new(id, &fingerprint, None, &intent, &attempt);
        self.create_for_effect(&context, plan, &fingerprint).await
    }

    async fn create_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) -> Result<String, RuntimeError> {
        let name = runtime_container_name(self.realization_namespace.as_str(), context.scope)?;
        let mut args = podman_run_argv(&name, plan, &plan.rootfs);
        // Publish the agent's internal port to an ephemeral 127.0.0.1 host port so
        // `open_channel` can dial it (inserted after `--name <name>`, before the image).
        if let Some(i) = args.iter().position(|a| a == &name) {
            args.splice(
                i + 1..i + 1,
                [
                    "--label".to_string(),
                    format!("{RUNTIME_OWNER_LABEL}={}", self.owner_id),
                    "--label".to_string(),
                    format!(
                        "{SANDBOX_SCOPE_LABEL}={}",
                        sandbox_scope_identity(self.realization_namespace.as_str(), context.scope)?
                    ),
                    "--label".to_string(),
                    format!("{SANDBOX_ADOPTION_LABEL}={}", context.adoption_fingerprint),
                    "--label".to_string(),
                    format!("{SANDBOX_REALIZATION_LABEL}={realization_fingerprint}"),
                    "--label".to_string(),
                    format!("{SANDBOX_ATTEMPT_LABEL}={}", context.attempt.as_str()),
                    "-p".to_string(),
                    format!("127.0.0.1::{}", self.agent_port),
                ],
            );
        }
        if let Some(i) = args.iter().position(|arg| arg == &name) {
            let labels = container_effect_label_values(context.effect_fence)
                .into_iter()
                .flat_map(|(key, value)| ["--label".to_owned(), format!("{key}={value}")])
                .collect::<Vec<_>>();
            args.splice(i + 1..i + 1, labels);
        }
        let mut decision = self
            .create_decision(context, Some(realization_fingerprint))
            .await?;
        for _ in 0..4 {
            match decision {
                ExistingRealizationDecision::Create => match self.run(&args).await {
                    Ok(created) if !created.trim().is_empty() => return Ok(created),
                    Ok(_) => {
                        decision = self
                            .create_decision(context, Some(realization_fingerprint))
                            .await
                            .map_err(RuntimeError::after_mutation)?;
                    }
                    Err(error) => {
                        let after = self
                            .create_decision(context, Some(realization_fingerprint))
                            .await
                            .map_err(RuntimeError::after_mutation)?;
                        if after == ExistingRealizationDecision::Create {
                            return Err(error.after_mutation());
                        }
                        decision = after;
                    }
                },
                ExistingRealizationDecision::ConvergeCreating(observed) => {
                    if let Err(error) = self
                        .run(&["start".into(), observed.incarnation.identity.clone()])
                        .await
                    {
                        let after = self
                            .create_decision(context, Some(realization_fingerprint))
                            .await
                            .map_err(RuntimeError::after_mutation)?;
                        if let ExistingRealizationDecision::ReuseReady(observed) = &after {
                            return Ok(observed.incarnation.identity.clone());
                        }
                        if after == ExistingRealizationDecision::Create {
                            return Err(error.after_mutation());
                        }
                        decision = after;
                        continue;
                    }
                    return Ok(observed.incarnation.identity);
                }
                ExistingRealizationDecision::ReuseReady(observed) => {
                    return Ok(observed.incarnation.identity);
                }
                ExistingRealizationDecision::ReplaceExact(observed) => {
                    // Only the shared fingerprint+Session-fence decision can
                    // authorize force removal, and the target is the immutable
                    // observed container id rather than the reusable name.
                    let removal = self
                        .run(&[
                            "rm".into(),
                            "--force".into(),
                            observed.incarnation.identity.clone(),
                        ])
                        .await;
                    let after = self
                        .create_decision(context, Some(realization_fingerprint))
                        .await
                        .map_err(RuntimeError::after_mutation)?;
                    if after == ExistingRealizationDecision::Create {
                        decision = after;
                    } else if let Err(error) = removal {
                        return Err(error.after_mutation());
                    } else {
                        decision = after;
                    }
                }
                ExistingRealizationDecision::ValidateExisting(_) => {
                    return Err(backend(
                        "Podman create reached a fingerprint-deferred decision",
                    ));
                }
            }
        }
        Err(backend("Podman exact realization did not converge").after_mutation())
    }

    async fn recover_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<Option<RuntimeRestoreTarget>, RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(backend("Podman restore plan fingerprint mismatch"));
        }
        let name = restore_container_name(id);
        if !self.container_exists(&name).await? {
            return Ok(None);
        }
        let (container_id, running) = self
            .exact_restoration_id_with_state(&name, plan_fingerprint, evidence)
            .await?;
        if !running {
            // Preserve the exact named container even if the start retry is
            // temporarily unavailable. The provider retains its physical bind
            // before the later running-state completion observation fails.
            let _ = self.run(&["start".into(), container_id.clone()]).await;
        }
        Ok(Some(RuntimeRestoreTarget {
            container_id,
            disposition: pc::SandboxRestoreTargetDisposition::Recovered,
        }))
    }

    async fn restore_or_adopt(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<RuntimeRestoreTarget, RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(backend("Podman restore plan fingerprint mismatch"));
        }
        let name = restore_container_name(id);
        let labels = restoration_metadata(evidence)
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .chain(std::iter::once((
                crate::RESTORE_PLAN_LABEL.to_string(),
                plan_fingerprint.to_string(),
            )));
        let args = self.container_run_args(&name, plan, labels);
        match self.run(&args).await {
            Ok(_) => Ok(RuntimeRestoreTarget {
                container_id: name,
                disposition: pc::SandboxRestoreTargetDisposition::Created,
            }),
            Err(error) => self
                .recover_restore_target(id, plan, plan_fingerprint, evidence)
                .await?
                .ok_or(error),
        }
    }

    async fn restoration_evidence(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
        self.inspect_restoration(container_id)
            .await
            .map(|(_, evidence)| evidence)
    }

    async fn restoration_plan_fingerprint(
        &self,
        container_id: &str,
    ) -> Result<Option<String>, RuntimeError> {
        self.inspect_restoration_identity(container_id)
            .await
            .map(|(_, _, fingerprint)| fingerprint)
    }

    async fn dispose_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<(), RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(backend("Podman restore cleanup plan fingerprint mismatch"));
        }
        let name = restore_container_name(id);
        if !self.container_exists(&name).await? {
            return Ok(());
        }
        // Terminal cleanup owns the exact unpublished target regardless of
        // process state. A failed start must not make the durable target
        // undisposable; identity and immutable-plan evidence remain mandatory.
        self.exact_restoration_id_with_state(&name, plan_fingerprint, evidence)
            .await?;
        let live_inputs = self.live_inputs_root(&name).await?;
        let staging_root = live_inputs
            .parent()
            .ok_or_else(|| backend("Podman restored live-input root has no staging parent"))?;
        crate::remove_host_staging_path(staging_root)?;
        self.run(&["rm".into(), "-f".into(), name]).await?;
        Ok(())
    }

    async fn handle_extra(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::ContainerContinuationHandle>, RuntimeError> {
        if self
            .inspect_restoration_identity_with_state(container_id)
            .await?
            .1
            .is_none()
        {
            return Ok(None);
        }
        let live_inputs = self.live_inputs_root(container_id).await?;
        let staging_root = live_inputs
            .parent()
            .ok_or_else(|| backend("Podman live-input root has no staging parent"))?;
        Ok(Some(pc::ContainerContinuationHandle::HostBindRestoration(
            pc::HostBindRestorationHandle::for_restore(staging_root.to_string_lossy().into_owned())
                .map_err(|error| backend(error.to_string()))?,
        )))
    }

    async fn observe(
        &self,
        expectation: crate::ContainerObservationExpectation<'_>,
    ) -> Result<pc::SandboxObservation, RuntimeError> {
        if expectation.runtime_handle.is_some() {
            return Err(backend(
                "Podman observation received foreign runtime continuation evidence",
            ));
        }
        let expected_incarnation = expectation
            .realization_fingerprint
            .map(|_| expectation.container_id);
        // `ps --all` returns a successful empty JSON set only for absence.
        // Daemon/authorization/transport failures remain `run` errors and can
        // never be collapsed into Gone as the old `inspect` path did.
        let encoded = self
            .run(&[
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("id={}", expectation.container_id),
                "--format".into(),
                "json".into(),
            ])
            .await?;
        sandbox_observation(
            expected_incarnation,
            expectation.adoption_fingerprint,
            expectation.realization_fingerprint,
            expectation.effect_fence,
            &podman_realizations(&encoded)?,
        )
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
        let encoded = self
            .run(&[
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("id={container_id}"),
                "--format".into(),
                "json".into(),
            ])
            .await?;
        container_state_observation(container_id, &podman_realizations(&encoded)?)
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
        // Durable Session/realization state owns the lease; this adapter only
        // proves the target is still live at the renewal boundary.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.run(&["rm".into(), "-f".into(), container_id.into()])
            .await
            .map(|_| ())
    }

    async fn remove_exact_incarnation(
        &self,
        container_id: &str,
        runtime_handle: Option<&pc::ContainerContinuationHandle>,
        _authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), RuntimeError> {
        if runtime_handle.is_some() {
            return Err(backend(
                "Podman exact removal received foreign continuation evidence",
            ));
        }
        self.remove(container_id).await
    }
}

#[async_trait]
impl PackageImageProvisioner for PodmanRuntime {
    async fn package_base_image_identity(&self, reference: &str) -> Result<String, RuntimeError> {
        self.run(&[
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            reference.into(),
        ])
        .await
        .and_then(|identity| {
            (!identity.is_empty())
                .then_some(identity)
                .ok_or_else(|| backend("podman returned an empty base-image identity"))
        })
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        ContainerRuntime::prepare_package_image(self, base_image, packages, network).await
    }

    async fn package_image_available(
        &self,
        _base_image: &str,
        _packages: &pc::PackageRequirements,
        _network: &pc::NetworkPolicy,
        image: &str,
    ) -> Result<bool, RuntimeError> {
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
mod tests;
