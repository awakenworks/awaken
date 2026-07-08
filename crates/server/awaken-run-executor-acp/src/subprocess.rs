//! A subprocess-backed [`AgentChannelSource`]: launch an ACP CLI as a child
//! process and pipe its stdio into an [`AgentChannel`] (R4).
//!
//! This is the *local* (unsandboxed) launch path — the composition root uses it
//! for a trusted CLI or a test agent; a sandbox provider is the isolated
//! production counterpart behind the same [`AgentChannelSource`] trait, so the
//! executor is unchanged either way. Per-CLI config projection is data
//! ([`AcpLaunch`]): the host resolves the model (config plane) and hands this
//! module already-materialized strings — no config or secret types cross here.

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::{AgentChannel, SplitChannel};
use awaken_provisioning_contract::{ExitStatus, ProcessHandle, SandboxError, Signal};
use awaken_runtime_contract::activation::RunActivation;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::{AcpCli, AgentChannelSource, AgentSession, OpenError, ResolvedModel};

/// A resolved launch for an ACP CLI: the argv plus the env to set (model, base
/// URL, key). Every runtime-specific env-key name lives in a constructor here —
/// the per-CLI projection, kept as data (G12).
#[derive(Debug, Clone)]
pub struct AcpLaunch {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl AcpLaunch {
    /// The Claude Code (`claude --acp`) launch projected from resolved model
    /// config (R4): base URL, model name, and key land in the Anthropic env the
    /// CLI reads. `api_key` is the already-materialized secret the host injects.
    #[must_use]
    pub fn claude(base_url: &str, model: &str, api_key: &str) -> Self {
        Self {
            argv: vec!["claude".to_string(), "--acp".to_string()],
            env: vec![
                ("ANTHROPIC_BASE_URL".to_string(), base_url.to_string()),
                ("ANTHROPIC_MODEL".to_string(), model.to_string()),
                ("ANTHROPIC_API_KEY".to_string(), api_key.to_string()),
            ],
        }
    }

    /// A custom launch (argv + env) — another CLI, or a test ACP agent.
    #[must_use]
    pub fn custom(argv: Vec<String>, env: Vec<(String, String)>) -> Self {
        Self { argv, env }
    }
}

/// Launches an ACP CLI as a local child (`env_clear` + only the projected env, so
/// no ambient leak), piping its stdio into an [`AgentChannel`].
pub struct SubprocessChannelSource {
    launch: AcpLaunch,
}

impl SubprocessChannelSource {
    #[must_use]
    pub fn new(launch: AcpLaunch) -> Self {
        Self { launch }
    }
}

/// Spawn an ACP CLI child from a resolved [`AcpLaunch`] and pipe its stdio into an
/// [`AgentChannel`]. `env_clear` + only the projected env, so no ambient leak.
fn spawn(launch: &AcpLaunch) -> std::result::Result<AgentSession, OpenError> {
    let (program, args) = launch
        .argv
        .split_first()
        .ok_or_else(|| OpenError("empty argv".to_string()))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in &launch.env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|e| OpenError(format!("spawn `{program}`: {e}")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| OpenError("child has no stdout".to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| OpenError("child has no stdin".to_string()))?;
    let channel: Box<dyn AgentChannel> = Box::new(SplitChannel::new(stdout, stdin));
    let process: Arc<dyn ProcessHandle> = Arc::new(ChildProcess {
        child: Mutex::new(child),
    });
    Ok(AgentSession { channel, process })
}

#[async_trait]
impl AgentChannelSource for SubprocessChannelSource {
    async fn open(
        &self,
        _activation: &RunActivation,
    ) -> std::result::Result<AgentSession, OpenError> {
        spawn(&self.launch)
    }
}

/// Resolves the host-provided inputs for one ACP run: the model coordinates (base
/// URL, model name, materialized key — the config-plane + vault lookup the executor
/// must not do itself) and any per-run non-secret env (e.g. the thread's config-home
/// path, which depends on `activation.thread_id` and so cannot be fixed up front).
/// The one seam between the neutral projection and the host's config/secret world.
pub trait LaunchResolver: Send + Sync {
    fn model(&self, activation: &RunActivation) -> std::result::Result<ResolvedModel, OpenError>;

    /// Host-provided non-secret env for this run (config-home path, passthrough).
    /// Merged under the typed model delivery — it can never shadow the model or key.
    fn extra_env(&self, _activation: &RunActivation) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// An [`AgentChannelSource`] that projects a run onto a launch via its [`AcpCli`]
/// row (R4): it reads the run's inputs through a host [`LaunchResolver`] and hands
/// the data to [`AcpCli::project`], so *which* CLI and *how* the model is delivered
/// are data, not a branch.
pub struct ProjectingChannelSource {
    cli: AcpCli,
    resolver: Arc<dyn LaunchResolver>,
}

impl ProjectingChannelSource {
    #[must_use]
    pub fn new(cli: AcpCli, resolver: Arc<dyn LaunchResolver>) -> Self {
        Self { cli, resolver }
    }

    /// Project this run onto a concrete [`AcpLaunch`] (no spawn): resolve the model +
    /// per-run env, read the compaction window from the run's config, and hand all of
    /// it to the CLI's [`AcpCli::project`] row.
    pub(crate) fn plan(
        &self,
        activation: &RunActivation,
    ) -> std::result::Result<AcpLaunch, OpenError> {
        let model = self.resolver.model(activation)?;
        let extra_env = self.resolver.extra_env(activation);
        let window = compact_window(&activation.snapshot.resolved_spec);
        Ok(self.cli.project(&model, window, &extra_env))
    }
}

/// The launched CLI's own auto-compaction window, from the run's config. Read from
/// the neutral `plugin_config["acp"]["compact_window"]` (a token count) — an
/// ACP-scoped setting, kept separate from the native compactor's message-count
/// [`ContextPolicy`](awaken_runtime_contract::resolved::ContextPolicy) since a CLI's
/// window is tokens, not messages. Absent → the CLI keeps its own default.
fn compact_window(spec: &awaken_runtime_contract::resolved::ResolvedSpec) -> Option<u64> {
    spec.plugin_config
        .get("acp")?
        .get("compact_window")?
        .as_u64()
}

#[async_trait]
impl AgentChannelSource for ProjectingChannelSource {
    async fn open(
        &self,
        activation: &RunActivation,
    ) -> std::result::Result<AgentSession, OpenError> {
        spawn(&self.plan(activation)?)
    }
}

/// A [`ProcessHandle`] over a tokio child. Local best-effort reaping: tokio's
/// `Child` exposes SIGKILL; the SIGTERM→grace→SIGKILL ladder is a sandbox-provider
/// concern (`kill_on_drop` covers the drop path).
struct ChildProcess {
    child: Mutex<Child>,
}

fn exit(status: std::process::ExitStatus) -> ExitStatus {
    ExitStatus {
        code: status.code(),
        signaled: status.code().is_none(),
    }
}

#[async_trait]
impl ProcessHandle for ChildProcess {
    fn id(&self) -> &str {
        "acp-subprocess"
    }
    async fn wait(&self) -> std::result::Result<ExitStatus, SandboxError> {
        let status = self
            .child
            .lock()
            .await
            .wait()
            .await
            .map_err(|e| SandboxError::new(e.to_string()))?;
        Ok(exit(status))
    }
    async fn poll(&self) -> std::result::Result<Option<ExitStatus>, SandboxError> {
        Ok(self
            .child
            .lock()
            .await
            .try_wait()
            .map_err(|e| SandboxError::new(e.to_string()))?
            .map(exit))
    }
    async fn signal(&self, _signal: Signal) -> std::result::Result<(), SandboxError> {
        self.child
            .lock()
            .await
            .start_kill()
            .map_err(|e| SandboxError::new(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AcpRunExecutor;
    use awaken_agent_contract::agent::run::{EndCause, Phase};
    use awaken_runtime_contract::execution::RunExecutor;

    // Reuse the fixture activation from the crate tests.
    use crate::tests::activation;

    #[test]
    fn claude_projection_puts_model_and_base_in_anthropic_env() {
        let l = AcpLaunch::claude("https://gw/v1/", "kimi-k2", "vault-key");
        assert_eq!(l.argv, vec!["claude", "--acp"]);
        assert!(
            l.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://gw/v1/")
        );
        assert!(
            l.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_MODEL" && v == "kimi-k2")
        );
        assert!(
            l.env
                .iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "vault-key")
        );
    }

    #[tokio::test]
    async fn drives_a_real_subprocess_acp_agent_end_to_end() {
        // A tiny ACP agent in shell: read the prompt line, emit a message + turn_end.
        let script = "read _prompt; \
             printf '%s\\n' '{\"type\":\"message\",\"text\":\"from subprocess\"}'; \
             printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
        let launch = AcpLaunch::custom(
            vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            vec![],
        );
        let exec = AcpRunExecutor::new(Arc::new(SubprocessChannelSource::new(launch)));
        let phase = exec
            .execute(activation(), Default::default())
            .await
            .unwrap();
        assert_eq!(phase, Phase::Ended(EndCause::NaturalEnd));
    }
}
