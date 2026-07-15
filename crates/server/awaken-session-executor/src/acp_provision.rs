//! Host-side provisioning for a launched ACP CLI: the [`LaunchResolver`] that turns
//! a run into concrete launch inputs. It resolves the model from the run's resolved
//! spec + the process environment (the `ANTHROPIC_*` / `OPENAI_*` the operator
//! exports — e.g. a MiniMax or Kimi endpoint), materializes the key from that env
//! (never stored in config), and opens the thread's [`ConfigHome`], handing its path
//! back as the CLI's `config_home_env`. The neutral projection ([`AcpCli::project`])
//! then assembles the launch — this module supplies only the host's per-run inputs.

use std::path::PathBuf;
use std::sync::Arc;

use awaken_run_executor_acp::{AcpCli, ConfigHome, LaunchResolver, OpenError, ResolvedModel};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::model_access::ModelAccessGrant;

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

    /// Resolve the model coordinates for `model_ref` under the run's access `grant`.
    ///
    /// One projection path: the effective [`ModelAccessGrant`] is materialized into a
    /// neutral endpoint and the launch coordinates read off it — no hand-written
    /// gateway-vs-local branch. The manifest `grant` is authoritative for the access
    /// mode; only when it names the default local mode do we consult the operator env,
    /// which may itself carry a legacy gateway injection
    /// (`AWAKEN_ACP_GATEWAY_URL`+`AWAKEN_ACP_LEASE_TOKEN`, the pre-manifest path, still
    /// honored during migration — see [`Self::grant_from_env`]).
    ///
    /// A gateway grant's endpoint carries `base_url`/`bearer` structurally, so the
    /// local-env fallback below fires only for a local grant — a gateway run can never
    /// route onto a raw provider key. A local grant with no env base URL or key is a
    /// fail-closed launch error.
    fn resolve_model(
        &self,
        model_ref: &str,
        grant: &ModelAccessGrant,
    ) -> Result<ResolvedModel, OpenError> {
        let d = &self.cli.model_delivery;
        let model = if model_ref.is_empty() {
            (self.env)(d.model).unwrap_or_default()
        } else {
            model_ref.to_string()
        };

        // Manifest grant wins; a default (local) grant defers to the operator env.
        let effective = if grant.is_gateway() {
            grant.clone()
        } else {
            self.grant_from_env()
        };
        let ep = effective.materialize();

        // A gateway endpoint fills base_url/bearer, so `.or_else(env)` only fires for
        // a local grant — the raw key is read solely on the self-credentialed path.
        let base_url = ep
            .base_url
            .or_else(|| (self.env)(d.base_url))
            .ok_or_else(|| OpenError(format!("{} not set in the environment", d.base_url)))?;
        let api_key = ep
            .bearer
            .or_else(|| (self.env)(d.key))
            .ok_or_else(|| OpenError(format!("{} not set in the environment", d.key)))?;
        // The grant's model (a gateway grant names it) wins over the run's `model_ref`
        // when present; an env-reconstructed grant leaves it empty and defers.
        let model = ep.model_ref.filter(|m| !m.is_empty()).unwrap_or(model);
        Ok(ResolvedModel {
            base_url,
            model,
            api_key,
        })
    }

    /// Legacy operator-env → grant bridge (the pre-manifest path). A gateway grant
    /// when both `AWAKEN_ACP_GATEWAY_URL` and `AWAKEN_ACP_LEASE_TOKEN` are exported,
    /// else the local default. Kept so an env-only deployment keeps working until
    /// placement populates `activation.model_access`; remove once it does.
    fn grant_from_env(&self) -> ModelAccessGrant {
        match (
            (self.env)("AWAKEN_ACP_GATEWAY_URL"),
            (self.env)("AWAKEN_ACP_LEASE_TOKEN"),
        ) {
            (Some(gateway), Some(lease)) => ModelAccessGrant::CloudManagedGateway {
                gateway_base_url: gateway,
                // The ACP CLI does not consume `dialect`; the model comes from the
                // run's `model_ref`, applied by the caller.
                dialect: String::new(),
                model_ref: String::new(),
                lease_token: lease,
            },
            _ => ModelAccessGrant::default(),
        }
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
        self.resolve_model(
            &activation.snapshot.resolved_spec.model_binding.model_ref,
            &activation.model_access,
        )
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
        let model = r
            .resolve_model("MiniMax-M3[1m]", &ModelAccessGrant::default())
            .unwrap();
        assert_eq!(model.base_url, "https://api.minimaxi.com/anthropic");
        assert_eq!(model.model, "MiniMax-M3[1m]");
        assert_eq!(model.api_key, "exported-key");
    }

    #[test]
    fn missing_key_or_base_url_fails_closed() {
        let r = resolver_with(&[("ANTHROPIC_BASE_URL", "u")], None);
        assert!(r.resolve_model("m", &ModelAccessGrant::default()).is_err()); // no ANTHROPIC_API_KEY
    }

    #[test]
    fn cloud_managed_gateway_env_yields_a_lease_token_not_a_raw_key() {
        // D-R2: with the gateway env injected, the launched CLI points at the gateway
        // and its "key" is the short-lived lease token — the raw provider key is never
        // read, even when present in the env.
        let r = resolver_with(
            &[
                ("AWAKEN_ACP_GATEWAY_URL", "https://gw.internal/anthropic"),
                ("AWAKEN_ACP_LEASE_TOKEN", "lease-abc"), // awaken-allow: secret
                ("ANTHROPIC_API_KEY", "raw-provider-key"), // awaken-allow: secret
            ],
            None,
        );
        let model = r
            .resolve_model("claude-opus", &ModelAccessGrant::default())
            .unwrap();
        assert_eq!(model.base_url, "https://gw.internal/anthropic");
        assert_eq!(model.model, "claude-opus");
        assert_eq!(model.api_key, "lease-abc"); // the lease, not the raw key
        assert_ne!(model.api_key, "raw-provider-key");
    }

    #[test]
    fn cloud_managed_gateway_needs_no_raw_key_in_the_env() {
        // The gateway path is self-sufficient: no ANTHROPIC_API_KEY / ANTHROPIC_BASE_URL
        // required, since the sandbox is credential-free by design.
        let r = resolver_with(
            &[
                ("AWAKEN_ACP_GATEWAY_URL", "https://gw.internal"),
                ("AWAKEN_ACP_LEASE_TOKEN", "lease-xyz"), // awaken-allow: secret
            ],
            None,
        );
        let model = r.resolve_model("m", &ModelAccessGrant::default()).unwrap();
        assert_eq!(model.base_url, "https://gw.internal");
        assert_eq!(model.api_key, "lease-xyz");
    }

    #[test]
    fn manifest_gateway_grant_yields_lease_without_any_gateway_env() {
        // The typed path: a `CloudManagedGateway` grant on the activation resolves to
        // the gateway + lease with no `AWAKEN_ACP_*` env at all, and its model_ref
        // wins. This is what placement/awaken-cloud populates; the env bridge is only
        // the legacy fallback.
        let r = resolver_with(&[], None);
        let grant = ModelAccessGrant::CloudManagedGateway {
            gateway_base_url: "https://gw.manifest".into(),
            dialect: "AnthropicMessages".into(),
            model_ref: "claude-from-grant".into(),
            lease_token: "lease-manifest".into(), // awaken-allow: secret
        };
        let model = r.resolve_model("run-model-ref", &grant).unwrap();
        assert_eq!(model.base_url, "https://gw.manifest");
        assert_eq!(model.api_key, "lease-manifest"); // the lease, not a raw key
        assert_eq!(model.model, "claude-from-grant"); // the grant's model wins
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
        assert_eq!(
            r.resolve_model("", &ModelAccessGrant::default())
                .unwrap()
                .model,
            "env-model"
        );
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
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    model_binding: ModelBinding::new("p", "run-model", "acp:claude"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: Vec::new(),
            trace: Default::default(),
            model_access: Default::default(),
        };
        assert_eq!(r.model(&act).unwrap().model, "run-model");
        let env = r.extra_env(&act);
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        assert!(env[0].1.contains("th"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_config_home_is_rooted_under_the_store_dir_never_the_host_home() {
        // The launch's config-home env must point INSIDE the provided store dir (the
        // per-thread isolated home), never at the operator's real `$HOME` — so a launched
        // CLI reads/writes its own config there and cannot touch the host default.
        let base = std::env::temp_dir().join(format!("awaken-chroot-{}", std::process::id()));
        let r = EnvLaunchResolver::from_process_env(claude(), Some(base.clone()));
        let env = r.config_home_env("thread-xyz");
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        let dir = &env[0].1;
        assert!(
            dir.starts_with(&base.to_string_lossy().to_string()),
            "config home {dir} must live under the store dir {base:?}"
        );
        assert!(dir.contains("thread-xyz"), "keyed by the thread id");
        if let Ok(home) = std::env::var("HOME") {
            assert!(
                !dir.starts_with(&format!("{home}/.claude")),
                "config home must never be the host's ~/.claude"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn config_home_open_failure_yields_no_env_not_a_panic() {
        // The store dir is a regular FILE, so `ConfigHome::open`'s `create_dir_all`
        // fails. The resolver must degrade to no env (the CLI falls back to its own
        // default home) rather than abort the launch.
        let file = std::env::temp_dir().join(format!("awaken-ch-notdir-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let r = resolver_with(&[], Some(file.clone()));
        assert!(
            r.config_home_env("thr").is_empty(),
            "an unopenable config home must degrade to no env"
        );
        let _ = std::fs::remove_file(&file);
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
