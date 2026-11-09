//! Shared decoding for the existing Config-store preference adapters.
use awaken_agent_config::personal_selection::{AgentSelectionReceipt, AgentSelectionState};
use awaken_agent_config::{ConfigStoreError, ScopeId};

pub(crate) fn target(scope: &ScopeId, agent: &str) -> Result<(), ConfigStoreError> {
    if [scope.0.as_str(), agent].iter().any(|value| {
        value.is_empty()
            || value.len() > 300
            || value.trim() != *value
            || value.chars().any(char::is_control)
    }) {
        return Err(ConfigStoreError(
            "invalid personal Agent selection target".into(),
        ));
    }
    Ok(())
}

pub(crate) fn state(
    generation: i64,
    selected: i64,
) -> Result<AgentSelectionState, ConfigStoreError> {
    if generation < 1 || ![0, 1].contains(&selected) {
        return Err(ConfigStoreError(
            "invalid persisted Agent selection state".into(),
        ));
    }
    Ok(AgentSelectionState {
        generation: u64::try_from(generation).map_err(reject)?,
        selected: selected == 1,
    })
}

pub(crate) fn receipt(data: &str, key: &str) -> Result<AgentSelectionReceipt, ConfigStoreError> {
    let value: AgentSelectionReceipt = serde_json::from_str(data).map_err(reject)?;
    value.intent.validate().map_err(reject)?;
    if value.intent.request_key != key
        || value.intent.expected_generation.checked_add(1) != Some(value.applied_generation)
    {
        return Err(ConfigStoreError(
            "invalid original Agent selection receipt".into(),
        ));
    }
    Ok(value)
}

fn reject(error: impl std::fmt::Display) -> ConfigStoreError {
    ConfigStoreError(error.to_string())
}
