// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This codec validates one Config-owned read frame. Its epoch is
// @formal reason: a concurrency observation, never identity or permission authority.
use awaken_agent_config::catalog_snapshot::{AgentCatalogEntry, AgentCatalogSnapshot};
use awaken_agent_config::{AgentConfigRevision, ConfigStoreError};

pub(super) fn bound(
    scope: &awaken_agent_config::ScopeId,
    maximum: usize,
) -> Result<i64, ConfigStoreError> {
    if scope.as_str().trim().is_empty() || !(1..=4096).contains(&maximum) {
        return Err(ConfigStoreError("invalid bounded catalog read".into()));
    }
    i64::try_from(
        maximum
            .checked_add(1)
            .ok_or_else(|| ConfigStoreError("catalog bound overflow".into()))?,
    )
    .map_err(|e| ConfigStoreError(e.to_string()))
}

pub(super) fn epoch(value: Option<i64>) -> Result<Option<u64>, ConfigStoreError> {
    value
        .map(|value| {
            u64::try_from(value)
                .ok()
                .filter(|v| *v != 0)
                .ok_or_else(|| ConfigStoreError("invalid catalog epoch".into()))
        })
        .transpose()
}

pub(super) fn revision(
    id: &str,
    data: &str,
    generation: i64,
    created: i64,
    updated: i64,
) -> Result<AgentConfigRevision, ConfigStoreError> {
    let config: awaken_agent_config::AgentConfig =
        serde_json::from_str(data).map_err(|e| ConfigStoreError(e.to_string()))?;
    if config.id != id || generation <= 0 {
        return Err(ConfigStoreError(
            "catalog revision identity is corrupt".into(),
        ));
    }
    Ok(AgentConfigRevision {
        config,
        revision: u64::try_from(generation).map_err(|e| ConfigStoreError(e.to_string()))?,
        created_at_unix_ms: Some(
            u64::try_from(created).map_err(|e| ConfigStoreError(e.to_string()))?,
        ),
        updated_at_unix_ms: Some(
            u64::try_from(updated).map_err(|e| ConfigStoreError(e.to_string()))?,
        ),
    })
}

pub(super) fn validate(
    snapshot: &AgentCatalogSnapshot,
    maximum: usize,
) -> Result<(), ConfigStoreError> {
    if snapshot.entries.len() > maximum || snapshot.preferences.len() > maximum {
        return Err(ConfigStoreError(
            "catalog read exceeds its native bound".into(),
        ));
    }
    if snapshot.epoch.is_none() && !snapshot.entries.is_empty() {
        return Err(ConfigStoreError(
            "catalog epoch is missing for stored state".into(),
        ));
    }
    for AgentCatalogEntry {
        head,
        published,
        publication,
    } in &snapshot.entries
    {
        match (published, publication) {
            (None, None) => {}
            (Some(version), Some(receipt))
                if version.config.id == head.config.id
                    && version.revision <= head.revision
                    && receipt.agent_id == head.config.id
                    && receipt.source_revision == version.revision
                    && receipt.state == awaken_agent_config::PublicationState::Published
                    && receipt.fingerprint == receipt.snapshot.fingerprint.0
                    && receipt.publication_id == receipt.fingerprint
                    && version.revision != 0 => {}
            _ => {
                return Err(ConfigStoreError(
                    "catalog publication has no exact immutable source".into(),
                ));
            }
        }
    }
    Ok(())
}
