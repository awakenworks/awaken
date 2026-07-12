//! [`SandboxChannelSource`] — the sandboxed [`AgentChannelSource`]: each turn
//! realizes a bubblewrap (namespace-tier) sandbox scoped to the run's thread and
//! launches the ACP CLI *inside* it, so an opaque agent is OS-confined regardless
//! of what it does. The isolated production counterpart of the trusted-CLI
//! [`awaken_run_executor_acp::SubprocessChannelSource`], behind the same trait —
//! the executor is unchanged either way (ADR-0043 D6/D9: the adapter lives in the
//! host plane, which sees both the executor port and the sandbox provider).
//!
//! Network egress follows the session's environment networking policy: a thread
//! registered deny-egress launches under `bwrap --unshare-net` (no route out, not
//! even to the host loopback), the same [`ThreadEgress`] registrations that drive
//! the native path's bash-tool jail.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::{AcpLaunch, AgentChannelSource, AgentSession, OpenError};
use awaken_runtime_contract::activation::RunActivation;
use awaken_sandbox_local::NamespaceProvider;

/// Shared per-thread deny-egress registrations: the host writes a thread's policy
/// at `prepare_session` (from its environment's networking policy), and both
/// consumers read it — `sandbox_spec` for the native bash-tool jail and
/// [`SandboxChannelSource`] for the ACP agent launch. A thread with no entry
/// shares the host network.
#[derive(Clone, Default)]
pub struct ThreadEgress(Arc<Mutex<HashMap<String, bool>>>);

impl ThreadEgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `thread`'s deny-egress policy (replaces any prior registration).
    pub fn set(&self, thread: &str, deny: bool) {
        self.0
            .lock()
            .expect("thread egress mutex poisoned")
            .insert(thread.to_string(), deny);
    }

    /// Whether `thread` is registered deny-egress.
    pub fn denies(&self, thread: &str) -> bool {
        self.0
            .lock()
            .expect("thread egress mutex poisoned")
            .get(thread)
            .copied()
            .unwrap_or(false)
    }
}

/// Opens each run's [`AgentSession`] inside a fresh namespace-tier sandbox: build
/// the spec from the activation's thread (scope + egress policy), realize it, and
/// `spawn_agent` the launch's argv under bwrap with piped stdio.
pub struct SandboxChannelSource {
    provider: NamespaceProvider,
    launch: AcpLaunch,
    egress: ThreadEgress,
    codec: awaken_run_executor_acp::Codec,
}

impl SandboxChannelSource {
    /// A source realizing its sandboxes under `base` (one root per thread scope).
    /// The provider is constructed here so a composition root names only this
    /// crate, not the sandbox tier. Defaults to the newline stand-in wire (the
    /// in-tree fixture agent); a real CLI sets [`Self::with_codec`] to `Codec::Acp`.
    pub fn new(base: impl Into<std::path::PathBuf>, launch: AcpLaunch) -> Self {
        Self {
            provider: NamespaceProvider::new(base),
            launch,
            egress: ThreadEgress::default(),
            codec: awaken_run_executor_acp::Codec::Newline,
        }
    }

    /// Follow per-thread egress registrations (the host's [`ThreadEgress`] handle).
    /// Without it every launch shares the host network.
    #[must_use]
    pub fn with_thread_egress(mut self, egress: ThreadEgress) -> Self {
        self.egress = egress;
        self
    }

    /// The wire the sandboxed agent speaks (a real `claude --acp` → `Codec::Acp`).
    #[must_use]
    pub fn with_codec(mut self, codec: awaken_run_executor_acp::Codec) -> Self {
        self.codec = codec;
        self
    }

    /// The provisioning request for one run: sandbox scoped to the thread (so a
    /// multi-turn session reuses one workspace), network from its registration.
    fn spec(&self, thread: &str) -> pc::SandboxSpec {
        let network = if self.egress.denies(thread) {
            pc::NetworkPolicy::None
        } else {
            pc::NetworkPolicy::Unrestricted
        };
        pc::SandboxSpec {
            scope: thread.to_string(),
            isolation: pc::IsolationClass::Namespace,
            mounts: Vec::new(),
            env: Vec::new(),
            network,
            outputs_path: "/mnt/session/outputs".to_string(),
            limits: pc::ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    /// The launch projected into the neutral process vocabulary: argv + env as
    /// per-process inline vars (the CLI sees the real values; a secret-splitting
    /// broker is a container-tier capability).
    fn command(&self) -> pc::Command {
        pc::Command {
            argv: self.launch.argv.clone(),
            cwd: String::new(),
            env: self
                .launch
                .env
                .iter()
                .map(|(name, value)| pc::EnvVar {
                    name: name.clone(),
                    value: pc::EnvValue::Inline {
                        value: value.clone(),
                    },
                    visibility: pc::EnvVisibility::Process,
                })
                .collect(),
            stdio: pc::Stdio::Piped,
        }
    }
}

#[async_trait]
impl AgentChannelSource for SandboxChannelSource {
    async fn open(&self, activation: &RunActivation) -> Result<AgentSession, OpenError> {
        let thread = activation.thread_id.0.as_str();
        let sandbox = self
            .provider
            .create_sandbox(&self.spec(thread))
            .await
            .map_err(|e| OpenError(format!("sandbox create: {e}")))?;
        let (process, channel) = sandbox
            .spawn_agent(self.command())
            .await
            .map_err(|e| OpenError(format!("sandboxed agent launch: {e}")))?;
        Ok(AgentSession {
            channel,
            process: Arc::from(process),
            codec: self.codec,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("awaken-sbxsrc-ut-{}", std::process::id()))
    }

    #[test]
    fn spec_shares_the_host_network_without_a_registration() {
        let src = SandboxChannelSource::new(base(), AcpLaunch::custom(vec!["a".into()], vec![]));
        let spec = src.spec("t");
        assert!(matches!(spec.network, pc::NetworkPolicy::Unrestricted));
        assert_eq!(spec.isolation, pc::IsolationClass::Namespace);
        assert_eq!(spec.scope, "t");
        assert_eq!(spec.outputs_path, "/mnt/session/outputs");
    }

    #[test]
    fn spec_maps_a_deny_egress_thread_to_no_network() {
        let egress = ThreadEgress::new();
        egress.set("iso", true);
        let src = SandboxChannelSource::new(base(), AcpLaunch::custom(vec!["a".into()], vec![]))
            .with_thread_egress(egress);
        assert!(matches!(src.spec("iso").network, pc::NetworkPolicy::None));
        // A sibling thread with no registration still shares the host network.
        assert!(matches!(
            src.spec("open").network,
            pc::NetworkPolicy::Unrestricted
        ));
    }

    #[test]
    fn command_projects_launch_env_as_inline_process_vars_with_piped_stdio() {
        let src = SandboxChannelSource::new(
            base(),
            AcpLaunch::custom(
                vec!["prog".into(), "--flag".into()],
                vec![("K".into(), "V".into())],
            ),
        );
        let cmd = src.command();
        assert_eq!(cmd.argv, vec!["prog".to_string(), "--flag".to_string()]);
        assert!(matches!(cmd.stdio, pc::Stdio::Piped));
        assert_eq!(cmd.env.len(), 1);
        assert_eq!(cmd.env[0].name, "K");
        assert!(matches!(cmd.env[0].value, pc::EnvValue::Inline { .. }));
        assert!(matches!(cmd.env[0].visibility, pc::EnvVisibility::Process));
    }

    #[test]
    fn thread_egress_defaults_false_and_set_overwrites() {
        let e = ThreadEgress::new();
        assert!(
            !e.denies("x"),
            "an unregistered thread shares the host network"
        );
        e.set("x", true);
        assert!(e.denies("x"));
        e.set("x", false); // a later registration replaces the prior one
        assert!(!e.denies("x"));
    }
}
