//! Host-side provisioning for a launched ACP CLI: the [`LaunchResolver`] that turns
//! publication-pinned inference access into concrete launch inputs. Endpoint/model
//! coordinates come only from the immutable snapshot and the exact credential is
//! materialized from the persisted vault. Process environment may advertise which
//! CLI a worker can host, but never supplies provider execution facts.

use awaken_run_executor_acp::{AcpCli, ConfigHome, LaunchResolver, OpenError, ResolvedModel};
use awaken_runtime_contract::activation::RunActivation;
use std::path::PathBuf;

/// Resolves an ACP run from its published access and a worker-side exact credential
/// materializer. This adapter cannot query the model catalog or select a credential.
pub struct PublishedAcpLaunchResolver {
    cli: AcpCli,
    store_dir: Option<PathBuf>,
    credentials: crate::PinnedCredentialMaterializer,
}

impl PublishedAcpLaunchResolver {
    #[must_use]
    pub fn new(
        cli: AcpCli,
        store_dir: Option<PathBuf>,
        credentials: crate::PinnedCredentialMaterializer,
    ) -> Self {
        Self {
            cli,
            store_dir,
            credentials,
        }
    }

    fn resolve_model(&self, activation: &RunActivation) -> Result<ResolvedModel, OpenError> {
        let model_ref = activation.effective_model_ref();
        let published = activation
            .snapshot
            .metadata
            .inference_access
            .as_ref()
            .ok_or_else(|| OpenError("run has no published inference access".to_string()))?;
        let access = published.for_model(model_ref).ok_or_else(|| {
            OpenError(format!(
                "model {model_ref} is outside the publication-pinned candidate set"
            ))
        })?;
        let endpoint = access
            .endpoint
            .clone()
            .ok_or_else(|| OpenError(format!("published model {model_ref} has no endpoint pin")))?;
        if endpoint.base_url.trim().is_empty() || endpoint.upstream_model.trim().is_empty() {
            return Err(OpenError(format!(
                "published model {model_ref} has incomplete endpoint coordinates"
            )));
        }
        let secret = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(self.credentials.materialize_provider(&access))
        })
        .map_err(OpenError)?;
        Ok(ResolvedModel {
            base_url: endpoint.base_url,
            model: endpoint.upstream_model,
            api_key: secret.expose_secret().to_string(),
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

impl LaunchResolver for PublishedAcpLaunchResolver {
    fn model(&self, activation: &RunActivation) -> Result<ResolvedModel, OpenError> {
        self.resolve_model(activation)
    }

    fn extra_env(&self, activation: &RunActivation) -> Vec<(String, String)> {
        self.config_home_env(&activation.thread_id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, AgentSnapshotMetadata, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_runtime_contract::{InferenceAccess, InferenceEndpoint};
    use std::sync::Arc;

    fn claude() -> AcpCli {
        *awaken_run_executor_acp::acp_cli("claude").unwrap()
    }

    fn activation(inference_access: Option<InferenceAccess>) -> RunActivation {
        RunActivation {
            run_id: RunId("r".into()),
            thread_id: ThreadId("th".into()),
            snapshot: ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("s".into()),
                metadata: AgentSnapshotMetadata {
                    inference_access,
                    ..Default::default()
                },
                root_agent_id: AgentId("a".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: Vec::new(),
                    catalog_fingerprint: CatalogFingerprint("fp".into()),
                    instructions: String::new(),
                    max_steps: 4,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("p", "published-model", "acp:claude"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("fp".into()),
            },
            input: Vec::new(),
            delegation_origin: None,
            model_ref_override: None,
            tool_capability_narrowing: Default::default(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolves_only_snapshot_endpoint_and_persisted_credential() {
        let repo = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("persisted-key")),
                oauth_command: None,
            },
            secrets.as_ref(),
            repo.as_ref(),
        );
        let source = source.await.unwrap();
        let access = InferenceAccess::resolved_credential(
            source.id.0,
            1,
            "ws",
            "anthropic@1",
            "anthropic-messages@1",
            InferenceEndpoint {
                adapter_kind: "anthropic".into(),
                base_url: "https://db.example/v1".into(),
                upstream_model: "upstream-model".into(),
            },
        );
        let resolver = PublishedAcpLaunchResolver::new(
            claude(),
            None,
            crate::PinnedCredentialMaterializer::new(repo, secrets),
        );
        let model = resolver.model(&activation(Some(access))).unwrap();
        assert_eq!(model.base_url, "https://db.example/v1");
        assert_eq!(model.model, "upstream-model");
        assert_eq!(model.api_key, "persisted-key");
    }

    #[test]
    fn missing_published_access_fails_closed() {
        let resolver = PublishedAcpLaunchResolver::new(
            claude(),
            None,
            crate::PinnedCredentialMaterializer::new(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
            ),
        );
        assert!(resolver.model(&activation(None)).is_err());
    }

    #[test]
    fn extra_env_points_the_cli_at_the_threads_config_home() {
        let base = std::env::temp_dir().join(format!("awaken-aclr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let r = PublishedAcpLaunchResolver::new(
            claude(),
            Some(base.clone()),
            crate::PinnedCredentialMaterializer::new(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
            ),
        );
        let env = r.config_home_env("thr_x");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].0, "CLAUDE_CONFIG_DIR");
        assert!(std::path::Path::new(&env[0].1).is_dir());
        assert!(env[0].1.contains("thr_x"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
