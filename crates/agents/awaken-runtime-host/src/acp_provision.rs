//! Host-side provisioning for a launched ACP CLI: the [`LaunchResolver`] that turns
//! a run into concrete launch inputs. It resolves the model from the run's resolved
//! spec + the process environment (the `ANTHROPIC_*` / `OPENAI_*` the operator
//! exports — e.g. a MiniMax or Kimi endpoint), materializes the key from that env
//! (never stored in config), and opens the thread's [`ConfigHome`], handing its path
//! back as the CLI's `config_home_env`. The neutral projection ([`AcpCli::project`])
//! then assembles the launch — this module supplies only the host's per-run inputs.

use std::path::PathBuf;
use std::sync::Arc;

use awaken_run_executor_acp::{AcpCli, LaunchResolver, OpenError, ResolvedModel};
use awaken_runtime_contract::activation::RunActivation;

use crate::config_home::ConfigHome;

/// Reads a var from the environment source; injectable so tests need no global env.
type EnvSource = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Resolves an ACP run's launch inputs from the process environment + the run's
/// resolved model, and the thread's config home.
pub struct EnvLaunchResolver {
    cli: AcpCli,
    store_dir: Option<PathBuf>,
    env: EnvSource,
}

impl EnvLaunchResolver {
    /// Read the model endpoint/key from the process environment (the operator's
    /// exported `*_BASE_URL` / `*_API_KEY`). `store_dir` is the durable root for the
    /// config home (`AWAKEN_STORAGE_DIR`).
    #[must_use]
    pub fn from_process_env(cli: AcpCli, store_dir: Option<PathBuf>) -> Self {
        Self {
            cli,
            store_dir,
            env: Arc::new(|key| std::env::var(key).ok()),
        }
    }

    /// Resolve the model coordinates for `model_ref` (from the run's spec): base URL
    /// and key come from the env under this CLI's delivery keys; the model is the
    /// run's `model_ref`, falling back to the env model key when the run left it
    /// unset. A missing base URL or key is a fail-closed launch error.
    fn resolve_model(&self, model_ref: &str) -> Result<ResolvedModel, OpenError> {
        let d = &self.cli.model_delivery;
        let base_url = (self.env)(d.base_url)
            .ok_or_else(|| OpenError(format!("{} not set in the environment", d.base_url)))?;
        let api_key = (self.env)(d.key)
            .ok_or_else(|| OpenError(format!("{} not set in the environment", d.key)))?;
        let model = if model_ref.is_empty() {
            (self.env)(d.model).unwrap_or_default()
        } else {
            model_ref.to_string()
        };
        Ok(ResolvedModel {
            base_url,
            model,
            api_key,
        })
    }

    /// Open the thread's config home and return it as the CLI's `config_home_env`.
    /// An open failure yields no env rather than aborting — the CLI then falls back
    /// to its own default home (degraded, not broken).
    fn config_home_env(&self, thread_id: &str) -> Vec<(String, String)> {
        match ConfigHome::open(self.store_dir.as_deref(), thread_id) {
            Ok(home) => vec![(
                self.cli.config_home_env.to_string(),
                home.root().display().to_string(),
            )],
            Err(_) => Vec::new(),
        }
    }
}

impl LaunchResolver for EnvLaunchResolver {
    fn model(&self, activation: &RunActivation) -> Result<ResolvedModel, OpenError> {
        self.resolve_model(&activation.snapshot.resolved_spec.model_binding.model_ref)
    }

    fn extra_env(&self, activation: &RunActivation) -> Vec<(String, String)> {
        self.config_home_env(&activation.thread_id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> AcpCli {
        *awaken_run_executor_acp::acp_cli("claude").unwrap()
    }

    fn resolver_with(pairs: &[(&str, &str)], store: Option<PathBuf>) -> EnvLaunchResolver {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        EnvLaunchResolver {
            cli: claude(),
            store_dir: store,
            env: Arc::new(move |k| map.get(k).cloned()),
        }
    }

    #[test]
    fn resolves_model_from_env_and_run_model_ref() {
        let r = resolver_with(
            &[
                ("ANTHROPIC_BASE_URL", "https://api.minimaxi.com/anthropic"),
                ("ANTHROPIC_API_KEY", "exported-key"), // awaken-allow: secret
            ],
            None,
        );
        let model = r.resolve_model("MiniMax-M3[1m]").unwrap();
        assert_eq!(model.base_url, "https://api.minimaxi.com/anthropic");
        assert_eq!(model.model, "MiniMax-M3[1m]");
        assert_eq!(model.api_key, "exported-key");
    }

    #[test]
    fn missing_key_or_base_url_fails_closed() {
        let r = resolver_with(&[("ANTHROPIC_BASE_URL", "u")], None);
        assert!(r.resolve_model("m").is_err()); // no ANTHROPIC_API_KEY
    }

    #[test]
    fn from_process_env_builds_a_usable_resolver() {
        let base = std::env::temp_dir().join(format!("awaken-fpe-{}", std::process::id()));
        let r = EnvLaunchResolver::from_process_env(claude(), Some(base.clone()));
        let env = r.config_home_env("thr");
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_run_model_falls_back_to_the_env_model_key() {
        let r = resolver_with(
            &[
                ("ANTHROPIC_BASE_URL", "u"),
                ("ANTHROPIC_API_KEY", "k"), // awaken-allow: secret
                ("ANTHROPIC_MODEL", "env-model"),
            ],
            None,
        );
        assert_eq!(r.resolve_model("").unwrap().model, "env-model");
    }

    #[test]
    fn launch_resolver_trait_reads_the_run_model_and_thread() {
        use awaken_agent_contract::agent::run::Id as RunId;
        use awaken_agent_contract::agent::thread::Id as ThreadId;
        use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
        use awaken_runtime_contract::snapshot::{
            AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
        };
        let base = std::env::temp_dir().join(format!("awaken-lrt-{}", std::process::id()));
        let r = resolver_with(
            &[("ANTHROPIC_BASE_URL", "u"), ("ANTHROPIC_API_KEY", "k")], // awaken-allow: secret
            Some(base.clone()),
        );
        let act = RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("th".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    model_binding: ModelBinding::new("p", "run-model", "acp:claude"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: Vec::new(),
            trace: Default::default(),
        };
        assert_eq!(r.model(&act).unwrap().model, "run-model");
        let env = r.extra_env(&act);
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        assert!(env[0].1.contains("th"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn extra_env_points_the_cli_at_the_threads_config_home() {
        let base = std::env::temp_dir().join(format!("awaken-aclr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let r = resolver_with(&[], Some(base.clone()));
        let env = r.config_home_env("thr_x");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        assert!(std::path::Path::new(&env[0].1).is_dir());
        assert!(env[0].1.contains("thr_x"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
