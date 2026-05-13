use awaken_contract::{AgentSpec, ConfigRecord, McpServerSpec, ToolSpec};
use serde_json::{Map, Value, json};

use crate::services::config_envelope::{apply_overrides, unwrap_spec};

use super::{ConfigNamespace, ConfigService, ConfigServiceError};

impl<'a> ConfigService<'a> {
    pub(super) async fn prepare_body(
        &self,
        namespace: ConfigNamespace,
        path_id: Option<&str>,
        body: Value,
    ) -> Result<(String, Value), ConfigServiceError> {
        let mut object = into_object(body)?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or(ConfigServiceError::MissingId)?
            .to_string();

        if let Some(path_id) = path_id
            && path_id != id
        {
            return Err(ConfigServiceError::InvalidPayload(format!(
                "path id '{path_id}' does not match body id '{id}'"
            )));
        }

        match namespace {
            ConfigNamespace::Providers => {
                object.remove("has_api_key");
                self.normalize_provider_payload(path_id, &mut object)
                    .await?;
            }
            ConfigNamespace::McpServers => {
                object.remove("has_env");
                object.remove("env_keys");
                self.normalize_mcp_server_payload(path_id, &mut object)
                    .await?;
            }
            ConfigNamespace::Agents | ConfigNamespace::Models => {}
        }

        object.remove("created_at");
        object.remove("updated_at");

        Ok((id, Value::Object(object)))
    }

    pub(super) fn validate_payload(
        &self,
        namespace: ConfigNamespace,
        body: &Value,
    ) -> Result<(), ConfigServiceError> {
        match namespace {
            ConfigNamespace::Agents => {
                awaken_contract::validate_agent_spec(body.clone())
                    .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            }
            ConfigNamespace::Models => {
                awaken_contract::validate_model_binding_spec(body.clone())
                    .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            }
            ConfigNamespace::Providers => {
                let spec = awaken_contract::validate_provider_spec(body.clone())
                    .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
                // Eager credential validation: parse `credentials_kind` and the
                // (kind × adapter × api_key) shape so misconfigured providers
                // are rejected at write time, not at first inference. The
                // adapter string is **not** validated here because the provider
                // may be saved before its target adapter is rolled out (admin
                // UI accepts unknown adapter names with a server-side error
                // surface). Adapter-specific validation lives in the runtime
                // build path.
                let kind = awaken_runtime::credentials::CredentialKind::from_options(
                    &spec.adapter_options,
                )
                .map_err(ConfigServiceError::InvalidPayload)?;
                awaken_runtime::credentials::build_material(
                    &spec.adapter,
                    kind,
                    spec.api_key.as_ref(),
                )
                .map_err(ConfigServiceError::InvalidPayload)?;
            }
            ConfigNamespace::McpServers => {
                let spec: McpServerSpec = from_value(body)?;
                if spec.id.trim().is_empty() {
                    return Err(ConfigServiceError::InvalidPayload(
                        "mcp server id cannot be empty".into(),
                    ));
                }

                match spec.transport {
                    awaken_contract::McpTransportKind::Stdio => {
                        if spec
                            .command
                            .as_deref()
                            .is_none_or(|value| value.trim().is_empty())
                        {
                            return Err(ConfigServiceError::InvalidPayload(
                                "stdio mcp server requires a non-empty command".into(),
                            ));
                        }
                    }
                    awaken_contract::McpTransportKind::Http => {
                        if spec
                            .url
                            .as_deref()
                            .is_none_or(|value| value.trim().is_empty())
                        {
                            return Err(ConfigServiceError::InvalidPayload(
                                "http mcp server requires a non-empty url".into(),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// Return the effective spec Value for a stored entry, applying `user_overrides`
/// when the namespace supports it (currently Agents and tools).
///
/// For non-Agent namespaces this is equivalent to `unwrap_spec`.
pub(super) fn effective_spec(
    namespace: ConfigNamespace,
    value: Value,
) -> Result<Value, ConfigServiceError> {
    match namespace {
        ConfigNamespace::Agents => {
            let record = ConfigRecord::<AgentSpec>::from_value(value)
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            let effective = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            serde_json::to_value(&effective)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))
        }
        _ => Ok(unwrap_spec(value)),
    }
}

pub(super) fn effective_visible_record<T>(value: Value) -> Result<Option<T>, ConfigServiceError>
where
    T: serde::de::DeserializeOwned + awaken_contract::ConfigRecordMerge,
{
    let record = ConfigRecord::<T>::from_value(value)
        .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
    if record.meta.hidden {
        return Ok(None);
    }
    apply_overrides(record.spec, record.meta.user_overrides.as_ref())
        .map(Some)
        .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))
}

pub(super) fn effective_tool_spec(value: Value) -> Result<Value, ConfigServiceError> {
    let record = ConfigRecord::<ToolSpec>::from_value(value)
        .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
    let effective = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
        .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
    serde_json::to_value(&effective).map_err(|e| ConfigServiceError::Serialization(e.to_string()))
}

/// Classify a tool's source from its id.
///
/// MCP tools registered by `awaken-ext-mcp` follow `mcp__{server}__{tool}`.
/// The global registry currently holds only built-in tools, but the classifier
/// is written defensively so it still works if MCP tools are ever surfaced here.
pub(super) fn classify_tool_source(id: &str) -> Value {
    if let Some(rest) = id.strip_prefix("mcp__") {
        // Extract server id: the segment between the two `__` delimiters.
        let server = rest.split("__").next().unwrap_or(rest);
        return json!({ "kind": "mcp", "id": server });
    }
    json!({ "kind": "builtin" })
}

pub(super) fn into_object(value: Value) -> Result<Map<String, Value>, ConfigServiceError> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(ConfigServiceError::InvalidPayload(
            "expected JSON object body".into(),
        )),
    }
}

fn from_value<T>(value: &Value) -> Result<T, ConfigServiceError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(value.clone())
        .map_err(|error| ConfigServiceError::InvalidPayload(error.to_string()))
}
