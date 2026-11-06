//! Projection of authored compaction policy into executable configuration.

/// Stamp the effective compaction trigger into both native and ACP realizations.
/// Resolution calls this once; neither runtime realization derives it again.
pub(crate) fn apply_compaction(
    plugin_config: &mut std::collections::BTreeMap<String, serde_json::Value>,
    strategy: &awaken_agent_config::CompactionStrategy,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
) {
    let effective = strategy.effective_window(context_window, max_output_tokens);
    if let Some(config) = plugin_config
        .get_mut("compact")
        .and_then(serde_json::Value::as_object_mut)
    {
        // Message-count thresholds were the retired parallel trigger authoring
        // path. Once publication owns the typed strategy they must not survive
        // into the executable snapshot or perturb its fingerprint.
        config.remove("threshold");
        config.remove("trigger_ratio");
        match effective {
            Some(effective) => {
                config.insert("max_tokens".into(), serde_json::json!(effective));
            }
            None => {
                config.remove("max_tokens");
            }
        }
        match strategy.keep_recent {
            Some(keep) => {
                config.insert("keep_last".into(), serde_json::json!(keep));
            }
            None => {
                config.remove("keep_last");
            }
        }
    }
    if let Some(config) = plugin_config
        .get_mut("acp")
        .and_then(serde_json::Value::as_object_mut)
    {
        match effective {
            Some(effective) => {
                config.insert("compact_window".into(), serde_json::json!(effective));
            }
            None => {
                config.remove("compact_window");
            }
        }
    }
}
