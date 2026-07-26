//! Free helpers shared by the [`ManagedState`] event path: rubric normalization,
//! usage projection, and content-block text extraction.

use super::*;

pub(crate) fn default_environment_snapshot(
    environment_id: String,
    runtime: Option<&str>,
) -> awaken_session_contract::EnvironmentSnapshot {
    let acp = runtime.is_some_and(|runtime| runtime.starts_with("acp:"));
    let holder = if acp {
        awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Workload,
            "awaken.workload.acp",
        )
    } else {
        awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            "awaken.worker",
        )
    };
    let sandbox = serde_json::json!({});
    let network = awaken_session_contract::SessionNetworkPolicy::Unrestricted;
    let credential_realization = awaken_credential_contract::CredentialRealizationProfile {
        inference_holder: holder,
        mcp_holder: awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            awaken_credential_contract::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        ),
    };
    awaken_session_contract::EnvironmentSnapshot {
        environment_id,
        revision: awaken_session_contract::env_registry::EnvironmentRevision(0),
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
            awaken_session_contract::stable_fingerprint(&(
                &sandbox,
                &network,
                &credential_realization,
            )),
        ),
        sandbox,
        network,
        credential_realization,
    }
}

pub(crate) fn lifecycle_fact(
    id: String,
    session_id: &str,
    workspace_id: Option<String>,
    event_type: &str,
) -> SessionLifecycleFact {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    SessionLifecycleFact {
        id,
        session_id: session_id.to_string(),
        workspace_id,
        event_type: event_type.to_string(),
        timestamp,
    }
}

/// Normalize a Managed rubric (a bare string or `{type:"text",content}`) to text.
pub(crate) fn rubric_text(rubric: &serde_json::Value) -> String {
    match rubric {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// The session's `usage` object (`BetaManagedAgentsSessionUsage`): cumulative input +
/// output (+ prompt-cache) token counts across all turns. Emitted whenever a turn ran.
pub(crate) fn session_usage_value(usage: SessionUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_tokens,
        cache_creation_input_tokens: usage.cache_creation_tokens,
    }
}

/// Concatenate the text of a content-block list.
pub(crate) fn content_text(
    content: &[awaken_agent_contract::agent::content::ContentBlock],
) -> String {
    use awaken_agent_contract::agent::content::ContentBlock;
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
