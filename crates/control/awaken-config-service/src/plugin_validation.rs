//! Publication-side port for extension-owned configuration semantics.
//!
//! JSON Schema drives discovery and forms; the extension remains the semantic
//! authority. A composition supplies one validator catalog so ConfigService
//! never imports concrete plugins or duplicates their parsing rules.

use awaken_config_store::AgentConfig;

pub trait PluginConfigurationValidator: Send + Sync {
    fn plugin_id(&self) -> &str;
    fn validate(&self, config: Option<&serde_json::Value>) -> Result<(), String>;
}

pub(crate) fn validate_plugin_configuration(
    validators: &[std::sync::Arc<dyn PluginConfigurationValidator>],
    config: &AgentConfig,
) -> Result<(), crate::publication::ValidationIssue> {
    for plugin_id in &config.plugin_ids {
        let mut owned = validators
            .iter()
            .filter(|validator| validator.plugin_id() == plugin_id);
        if let Some(validator) = owned.next() {
            if owned.next().is_some() {
                return Err(crate::publication::ValidationIssue {
                    path: format!("plugin_config.{plugin_id}"),
                    message: format!("plugin `{plugin_id}` has more than one semantic validator"),
                });
            }
            validator
                .validate(config.plugin_config.get(plugin_id))
                .map_err(|message| crate::publication::ValidationIssue {
                    path: format!("plugin_config.{plugin_id}"),
                    message,
                })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Catalog;

    impl PluginConfigurationValidator for Catalog {
        fn plugin_id(&self) -> &str {
            "owned"
        }

        fn validate(&self, config: Option<&serde_json::Value>) -> Result<(), String> {
            config
                .and_then(|value| value.get("valid"))
                .and_then(serde_json::Value::as_bool)
                .filter(|valid| *valid)
                .map(|_| ())
                .ok_or_else(|| "owned config is invalid".into())
        }
    }

    #[test]
    fn active_owned_plugin_uses_extension_semantics() {
        // Cause/effect table: active+owned+valid succeeds; active+owned+bad
        // reports the plugin path; inactive sections and active unowned plugins
        // are not reinterpreted by this catalog.
        let validators: Vec<std::sync::Arc<dyn PluginConfigurationValidator>> =
            vec![std::sync::Arc::new(Catalog)];
        let mut config = AgentConfig {
            plugin_ids: vec!["owned".into(), "external".into()],
            plugin_config: [
                ("owned".into(), serde_json::json!({ "valid": true })),
                ("external".into(), serde_json::json!({ "anything": true })),
                ("inactive".into(), serde_json::json!({ "valid": false })),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        assert!(validate_plugin_configuration(&validators, &config).is_ok());
        config
            .plugin_config
            .insert("owned".into(), serde_json::json!({ "valid": false }));
        assert_eq!(
            validate_plugin_configuration(&validators, &config)
                .unwrap_err()
                .path,
            "plugin_config.owned"
        );
    }
}
