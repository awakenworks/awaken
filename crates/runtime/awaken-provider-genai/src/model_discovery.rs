//! Provider model-directory transport and pagination.
//!
//! This module normalizes provider-owned model identifiers only. Credential
//! selection and management-catalog writes remain outside this runtime adapter.

use std::time::Duration;

use genai::adapter::AdapterKind;

/// Failure to obtain a complete provider model listing. Callers must not
/// reconcile a partial response because doing so could falsely mark offerings
/// unavailable.
#[derive(Debug, thiserror::Error)]
pub enum ModelDiscoveryError {
    #[error("model discovery is unsupported for adapter {0:?}")]
    Unsupported(AdapterKind),
    #[error("model discovery endpoint is invalid: {0}")]
    InvalidEndpoint(String),
    #[error("model discovery request failed: {0}")]
    Transport(String),
    #[error("model discovery returned HTTP {status}")]
    Http { status: u16 },
    #[error("model discovery response is invalid: {0}")]
    InvalidResponse(String),
}

/// Fetch the complete model directory exposed by a provider API. This adapter
/// performs transport/protocol work only: it neither selects credentials nor
/// writes the management-plane catalog. The already-materialized key exists only
/// for these requests and the result is a normalized, secret-free list of ids.
pub async fn discover_model_ids(
    adapter: AdapterKind,
    base_url: &str,
    authentication: &str,
) -> Result<Vec<String>, ModelDiscoveryError> {
    let mut url = reqwest::Url::parse(base_url)
        .map_err(|error| ModelDiscoveryError::InvalidEndpoint(error.to_string()))?;
    let model_path = if adapter == AdapterKind::Vertex {
        "publishers/google/models"
    } else {
        "models"
    };
    if !url.path().trim_end_matches('/').ends_with(model_path) {
        let path = format!("{}/{model_path}", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ModelDiscoveryError::Transport(error.to_string()))?;
    let mut ids = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::BTreeSet::new();
    loop {
        let mut page_url = url.clone();
        {
            let mut query = page_url.query_pairs_mut();
            match adapter {
                AdapterKind::Anthropic => {
                    query.append_pair("limit", "1000");
                    if let Some(cursor) = &cursor {
                        query.append_pair("after_id", cursor);
                    }
                }
                AdapterKind::Gemini | AdapterKind::Vertex => {
                    query.append_pair("pageSize", "1000");
                    if let Some(cursor) = &cursor {
                        query.append_pair("pageToken", cursor);
                    }
                    if adapter == AdapterKind::Gemini {
                        query.append_pair("key", authentication);
                    }
                }
                _ => {}
            }
        }
        let mut request = client.get(page_url);
        request = match adapter {
            AdapterKind::Anthropic => request
                .header("x-api-key", authentication)
                .header("anthropic-version", "2023-06-01"),
            AdapterKind::OpenAI | AdapterKind::Vertex => request.bearer_auth(authentication),
            AdapterKind::Gemini => request,
            other => return Err(ModelDiscoveryError::Unsupported(other)),
        };
        let response = request
            .send()
            .await
            .map_err(|error| ModelDiscoveryError::Transport(error.without_url().to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ModelDiscoveryError::Transport(error.without_url().to_string()))?;
        if !status.is_success() {
            return Err(ModelDiscoveryError::Http {
                status: status.as_u16(),
            });
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| ModelDiscoveryError::InvalidResponse(error.to_string()))?;
        let entries = match adapter {
            AdapterKind::Anthropic | AdapterKind::OpenAI => value.get("data"),
            AdapterKind::Gemini | AdapterKind::Vertex => value.get("models"),
            other => return Err(ModelDiscoveryError::Unsupported(other)),
        }
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ModelDiscoveryError::InvalidResponse("missing model array".into()))?;
        for entry in entries {
            let raw = entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ModelDiscoveryError::InvalidResponse("model has no id/name".into())
                })?;
            let id = raw.strip_prefix("models/").unwrap_or(raw).trim();
            if !id.is_empty() {
                ids.insert(id.to_string());
            }
        }
        let next_cursor = match adapter {
            AdapterKind::Anthropic
                if value.get("has_more").and_then(serde_json::Value::as_bool) == Some(true) =>
            {
                Some(
                    value
                        .get("last_id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|cursor| !cursor.is_empty())
                        .ok_or_else(|| {
                            ModelDiscoveryError::InvalidResponse(
                                "Anthropic page has_more=true but carries no last_id".into(),
                            )
                        })?
                        .to_string(),
                )
            }
            AdapterKind::Gemini | AdapterKind::Vertex => value
                .get("nextPageToken")
                .and_then(serde_json::Value::as_str)
                .filter(|token| !token.is_empty())
                .map(str::to_string),
            _ => None,
        };
        let Some(next_cursor) = next_cursor else {
            break;
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            return Err(ModelDiscoveryError::InvalidResponse(
                "provider repeated a pagination cursor".into(),
            ));
        }
        cursor = Some(next_cursor);
    }
    Ok(ids.into_iter().collect())
}
