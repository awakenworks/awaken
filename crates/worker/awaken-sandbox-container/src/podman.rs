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
use crate::{
    ContainerPlan, ContainerRuntime, ContainerState, ManagedContainer, REAPER_LABEL,
    REAPER_OWNER_LABEL, RuntimeError, podman_run_argv, runtime_container_name,
};

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
    owner_id: String,
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
            owner_id: crate::runtime_owner_id(),
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

    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        // Podman has no native lease/TTL; a lightweight reaper watches lease labels.
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.run(&["rm".into(), "-f".into(), container_id.into()])
            .await
            .map(|_| ())
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
                Some(ManagedContainer {
                    id,
                    owned_by_current_runtime: r
                        .get("Labels")
                        .and_then(serde_json::Value::as_object)
                        .and_then(|labels| labels.get(REAPER_OWNER_LABEL))
                        .and_then(serde_json::Value::as_str)
                        == Some(self.owner_id.as_str()),
                    running,
                    age_secs,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::{NetworkMode, RootfsPlan};

    use super::*;

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
