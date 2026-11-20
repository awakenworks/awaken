// @formal surface obligation=credential.http_effect_usage_binds_exact_fields_and_placements
// @formal reason: Mirrors only the frozen managed provider route into the Session-owned startup config, excluding credentials and instructions.
//! Process-level provider selection in the Session-owned CLI home.
//!
//! Codex validates a thread's provider against the app-server's startup config.
//! Its ACP adapter's per-thread CODEX_CONFIG alone cannot satisfy that check.
//! The same frozen, secret-free route must reach both configuration scopes.
//! Hermes must select its model before new/load registers the ACP MCP toolsets.

use crate::{AcpLaunch, AcpLaunchIdentity, OpenError};
use awaken_acp_catalog::{AcpCli, ManagedProviderConfigCodec};
use awaken_provisioning_contract::EnvValue;

/// A non-secret provider configuration written before the agent is spawned.
pub struct ProviderConfigFile {
    /// Relative to the managed Session's private config home.
    pub relative_path: &'static str,
    pub contents: String,
}

impl AcpLaunch {
    /// Backend-owned launches retain the operator's configuration. Managed
    /// launches project only the route keys, excluding prompt and credential
    /// material; no host configuration is read or updated.
    pub fn provider_config_file(
        &self,
        cli: &AcpCli,
    ) -> Result<Option<ProviderConfigFile>, OpenError> {
        if self.identity != AcpLaunchIdentity::Managed {
            return Ok(None);
        }
        let Some(delivery) = cli.managed_provider_config else {
            return Ok(None);
        };
        if matches!(delivery.codec, ManagedProviderConfigCodec::OpenCode { .. }) {
            return Ok(None);
        }
        let value = self.env.iter().find_map(|entry| {
            (entry.name == delivery.config_env)
                .then_some(&entry.value)
                .and_then(|value| match value {
                    EnvValue::Inline { value } => Some(value.as_str()),
                    _ => None,
                })
        });
        let invalid = || OpenError("managed_provider_config_invalid: missing startup route".into());
        let document: serde_json::Value =
            serde_json::from_str(value.ok_or_else(invalid)?).map_err(|_| invalid())?;
        if delivery.codec == ManagedProviderConfigCodec::Hermes {
            let model = document["model"]["default"].as_str().ok_or_else(invalid)?;
            if document["model"]["provider"].as_str() != Some(delivery.provider_id) {
                return Err(invalid());
            }
            return Ok(Some(ProviderConfigFile {
                relative_path: "config.yaml",
                contents: serde_json::json!({
                    "model": {"default": model, "provider": delivery.provider_id},
                })
                .to_string(),
            }));
        }
        let route = document["model_providers"]
            .get(delivery.provider_id)
            .and_then(serde_json::Value::as_object)
            .ok_or_else(invalid)?;
        if document["model_provider"].as_str() != Some(delivery.provider_id)
            || !document["model"].is_string()
        {
            return Err(invalid());
        }
        let route: serde_json::Map<String, serde_json::Value> = [
            "name",
            "base_url",
            "wire_api",
            "requires_openai_auth",
            "supports_standalone_web_search",
        ]
        .into_iter()
        .filter_map(|key| route.get(key).map(|value| (key.into(), value.clone())))
        .collect();
        let providers = serde_json::Map::from_iter([(
            delivery.provider_id.to_string(),
            serde_json::Value::Object(route),
        )]);
        let mut startup = serde_json::json!({
            "model": document["model"],
            "model_provider": delivery.provider_id,
            "model_providers": providers,
        });
        if let Some(search) = document.get("web_search") {
            startup["web_search"] = search.clone();
        }
        let contents = toml::to_string(&startup)
            .map_err(|_| OpenError("managed_provider_config_invalid: startup encoding".into()))?;
        Ok(Some(ProviderConfigFile {
            relative_path: ".codex/config.toml",
            contents,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialArtifactRequirement, ResolvedModel, acp_cli};

    #[test]
    fn managed_startup_route_matches_thread_route_without_persisting_secrets_or_prompts() {
        // C1 managed Codex + provider route -> E1 startup and thread provider
        // agree. C2 unrelated env/config contains secrets or prompts -> E2
        // neither enters the startup file. C3 backend-owned/other CLI -> E3 no
        // operator file. A real app-server enforces C1 before the first prompt.
        let cli = acp_cli("codex").unwrap();
        let model = ResolvedModel::managed(
            "https://provider.invalid/v1",
            "model\"name",
            None,
            Some(CredentialArtifactRequirement::new(
                "secret://opaque",
                ".codex/auth.json",
            )),
        );
        let mut launch = AcpLaunch::from_catalog(
            cli,
            &model,
            None,
            &[("UNRELATED_API_KEY".into(), "private-value".into())],
        )
        .unwrap();
        launch
            .project_instructions(cli.instructions, "private-instructions")
            .unwrap();
        let file = launch.provider_config_file(cli).unwrap().unwrap();
        let parsed: toml::Value = toml::from_str(&file.contents).unwrap();
        assert_eq!(file.relative_path, ".codex/config.toml");
        assert_eq!(parsed["model_provider"].as_str(), Some("awaken-managed"));
        assert_eq!(parsed["model"].as_str(), Some("model\"name"));
        assert_eq!(
            parsed["model_providers"]["awaken-managed"]["base_url"].as_str(),
            Some("https://provider.invalid/v1")
        );
        for forbidden in [
            "private-value",
            "private-instructions",
            "secret://",
            "OPENAI_API_KEY",
        ] {
            assert!(!file.contents.contains(forbidden), "E2 {forbidden}");
        }
        launch.identity = AcpLaunchIdentity::BackendOwned;
        assert!(launch.provider_config_file(cli).unwrap().is_none(), "E3");
        launch.identity = AcpLaunchIdentity::Managed;
        assert!(
            launch
                .provider_config_file(acp_cli("claude").unwrap())
                .unwrap()
                .is_none()
        );
        launch.env.retain(|entry| entry.name != "CODEX_CONFIG");
        assert!(
            launch.provider_config_file(cli).is_err(),
            "missing route fails before spawn"
        );
    }
}
