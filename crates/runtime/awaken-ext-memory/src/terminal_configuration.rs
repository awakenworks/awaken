// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Pure frozen plugin and model selection owns no executor, Session state or scheduler.
//! Select the Memory extraction configuration of an exact published Agent.
pub fn published_extraction_configuration(
    snapshot: &awaken_runtime_contract::ExecutableAgentSnapshot,
    effective_model_ref: &str,
) -> Result<
    Option<(
        crate::MemoryConfig,
        awaken_runtime_contract::resolved::ResolvedModelCandidate,
    )>,
    String,
> {
    if !snapshot
        .resolved_spec
        .plugin_ids
        .iter()
        .any(|id| id == crate::MEMORY_PLUGIN_ID)
    {
        return Ok(None);
    }
    let config = crate::MemoryConfig::from_value(
        snapshot
            .resolved_spec
            .plugin_config
            .get(crate::MEMORY_PLUGIN_ID),
    )
    .map_err(|error| format!("invalid frozen Memory plugin configuration: {error}"))?;
    let model = snapshot
            .resolved_spec
            .candidate_for_model(effective_model_ref)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "memory extraction model `{effective_model_ref}` is absent from frozen snapshot `{}`",
                    snapshot.id.0
                )
            })?;
    Ok(Some((config, model)))
}

/// Select the unpinned SDK Session's Memory configuration and host model.
pub fn direct_extraction_configuration(
    value: Option<&serde_json::Value>,
    effective_model_ref: &str,
) -> Result<
    (
        crate::MemoryConfig,
        awaken_runtime_contract::resolved::ResolvedModelCandidate,
    ),
    String,
> {
    let config = crate::MemoryConfig::from_value(value)
        .map_err(|error| format!("invalid direct Memory plugin configuration: {error}"))?;
    let binding = awaken_runtime_contract::resolved::ModelBinding::parse(
        "default",
        effective_model_ref,
        "default",
    )
    .map_err(|error| {
        format!(
            "effective model reference `{effective_model_ref}` for direct Memory \
         extraction is not an executable binding: {error}"
        )
    })?;
    Ok((
        config,
        awaken_runtime_contract::resolved::ResolvedModelCandidate::host(binding),
    ))
}
