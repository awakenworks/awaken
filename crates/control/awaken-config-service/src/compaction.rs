//! Projection of authored compaction policy into executable configuration.

/// Stamp the effective compaction trigger into both native and ACP realizations.
/// Resolution calls this once; neither runtime realization derives it again.
pub(crate) fn apply_compaction(
    plugin_config: &mut std::collections::BTreeMap<String, serde_json::Value>,
    strategy: &awaken_config_store::CompactionStrategy,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
) {
    let Some(effective) = strategy.effective_window(context_window, max_output_tokens) else {
        return;
    };
    if let Some(config) = plugin_config
        .get_mut("compact")
        .and_then(serde_json::Value::as_object_mut)
    {
        if config
            .get("max_tokens")
            .is_none_or(serde_json::Value::is_null)
        {
            config.insert("max_tokens".into(), serde_json::json!(effective));
            config.insert("trigger_ratio".into(), serde_json::json!(1.0));
        }
        if let Some(keep) = strategy.keep_recent
            && config
                .get("keep_last")
                .is_none_or(serde_json::Value::is_null)
        {
            config.insert("keep_last".into(), serde_json::json!(keep));
        }
    }
    if let Some(config) = plugin_config
        .get_mut("acp")
        .and_then(serde_json::Value::as_object_mut)
        && config
            .get("compact_window")
            .is_none_or(serde_json::Value::is_null)
    {
        config.insert("compact_window".into(), serde_json::json!(effective));
    }
}
