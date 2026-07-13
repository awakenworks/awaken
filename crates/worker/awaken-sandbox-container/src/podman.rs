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
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, AgentTransport};
use awaken_provisioning_contract as pc;
use tokio::process::Command as OsCommand;

use crate::net::TcpAgentTransport;
use crate::{ContainerPlan, ContainerRuntime, ContainerState, RuntimeError, podman_run_argv};

fn backend(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Backend(e.to_string())
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
}

impl PodmanRuntime {
    /// Use `podman` from `PATH` (override with `PODMAN_BIN`).
    #[must_use]
    pub fn new(agent_port: u16) -> Self {
        let bin = std::env::var("PODMAN_BIN").unwrap_or_else(|_| "podman".to_string());
        Self {
            bin,
            agent_port,
            exec: Arc::new(OsCommandExec),
        }
    }

    /// Wire a scripted executor (tests) instead of forking a real `podman`.
    #[cfg(test)]
    fn with_exec(agent_port: u16, exec: Arc<dyn CommandExec>) -> Self {
        Self {
            bin: "podman".into(),
            agent_port,
            exec,
        }
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
}

#[async_trait]
impl ContainerRuntime for PodmanRuntime {
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let name = format!("awaken-{id}");
        // Idempotent: clear any stale container of this scope first.
        let _ = self.run(&["rm".into(), "-f".into(), name.clone()]).await;

        let mut args = podman_run_argv(&name, plan, &plan.rootfs);
        // Publish the agent's internal port to an ephemeral 127.0.0.1 host port so
        // `open_channel` can dial it (inserted after `--name <name>`, before the image).
        if let Some(i) = args.iter().position(|a| a == &name) {
            args.splice(
                i + 1..i + 1,
                ["-p".to_string(), format!("127.0.0.1::{}", self.agent_port)],
            );
        }
        self.run(&args).await?;
        Ok(name)
    }

    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        // Process-as-container: reach the agent's stdio over its published port (a
        // network dial, not `podman exec`) — the same seam Docker/K8s use.
        let addr = self.agent_addr(container_id).await?;
        TcpAgentTransport::new(addr)
            .open_channel()
            .await
            .map_err(backend)
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

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        // Podman has no native lease/TTL; a lightweight reaper watches lease labels.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.run(&["rm".into(), "-f".into(), container_id.into()])
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::{NetworkMode, RootfsPlan};

    use super::*;

    /// A scripted [`CommandExec`]: a handler maps `(bin, args)` to a canned output,
    /// and every invocation's argv is recorded so tests can assert what was run.
    struct FakeExec {
        handler: Box<dyn Fn(&[String]) -> CmdOutput + Send + Sync>,
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
    fn new_defaults_to_podman_on_path() {
        let rt = PodmanRuntime::new(9000);
        assert_eq!(rt.agent_port, 9000);
        assert!(rt.bin == "podman" || std::env::var("PODMAN_BIN").is_ok());
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

    #[tokio::test]
    async fn create_clears_a_stale_container_then_publishes_the_agent_port() {
        let (rt, fake) = runtime_with(7777, |_| ok(""));
        let name = rt.create("s1", &plan()).await.unwrap();
        assert_eq!(name, "awaken-s1");
        let calls = fake.calls.lock().unwrap();
        // First call is the idempotent `rm -f awaken-s1`.
        assert_eq!(calls[0], vec!["rm", "-f", "awaken-s1"]);
        // The `run` argv publishes the agent port right after `--name awaken-s1`.
        let run = &calls[1];
        let name_at = run.iter().position(|a| a == "awaken-s1").unwrap();
        assert_eq!(run[name_at + 1], "-p");
        assert_eq!(run[name_at + 2], "127.0.0.1::7777");
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
    async fn artifacts_are_out_of_band_and_touch_lease_is_a_noop() {
        let (rt, _) = runtime_with(9000, |_| ok(""));
        assert!(rt.artifacts("cid").await.unwrap().is_empty());
        assert!(rt.touch_lease("cid").await.is_ok());
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
}
