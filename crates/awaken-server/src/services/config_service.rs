use std::sync::Arc;

use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::{AgentSpec, McpServerSpec, ModelSpec, ProviderSpec};
use serde_json::{Map, Value, json};

use crate::app::AppState;

use super::config_runtime::ConfigRuntimeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigNamespace {
    Agents,
    Models,
    Providers,
    McpServers,
}

impl ConfigNamespace {
    pub fn parse(value: &str) -> Result<Self, ConfigServiceError> {
        match value {
            "agents" => Ok(Self::Agents),
            "models" => Ok(Self::Models),
            "providers" => Ok(Self::Providers),
            "mcp-servers" => Ok(Self::McpServers),
            _ => Err(ConfigServiceError::UnknownNamespace(value.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Models => "models",
            Self::Providers => "providers",
            Self::McpServers => "mcp-servers",
        }
    }

    pub fn schema_json(self) -> Result<Value, ConfigServiceError> {
        let schema = match self {
            Self::Agents => schemars::schema_for!(AgentSpec),
            Self::Models => schemars::schema_for!(ModelSpec),
            Self::Providers => schemars::schema_for!(ProviderSpec),
            Self::McpServers => schemars::schema_for!(McpServerSpec),
        };
        serde_json::to_value(schema)
            .map_err(|error| ConfigServiceError::Serialization(error.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigServiceError {
    #[error("config management API not enabled")]
    NotEnabled,
    #[error("unknown namespace: {0}")]
    UnknownNamespace(String),
    #[error("missing 'id' field in body")]
    MissingId,
    #[error("invalid payload: {0}")]
    InvalidPayload(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("runtime apply failed: {0}")]
    Apply(String),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

pub struct ConfigService<'a> {
    state: &'a AppState,
    store: Arc<dyn ConfigStore>,
}

impl<'a> ConfigService<'a> {
    pub fn new(state: &'a AppState) -> Result<Self, ConfigServiceError> {
        let store = state
            .config_store
            .clone()
            .ok_or(ConfigServiceError::NotEnabled)?;
        Ok(Self { state, store })
    }

    pub async fn capabilities(&self) -> Result<Value, ConfigServiceError> {
        let registries = self
            .state
            .runtime
            .registry_set()
            .ok_or(ConfigServiceError::Apply(
                "runtime does not expose a configurable registry snapshot".into(),
            ))?;

        let tools = registries
            .tools
            .tool_ids()
            .into_iter()
            .filter_map(|id| {
                registries.tools.get_tool(&id).map(|tool| {
                    let descriptor = tool.descriptor();
                    json!({
                        "id": descriptor.id,
                        "name": descriptor.name,
                        "description": descriptor.description,
                    })
                })
            })
            .collect::<Vec<_>>();

        let plugins = registries
            .plugins
            .plugin_ids()
            .into_iter()
            .filter_map(|id| {
                registries.plugins.get_plugin(&id).map(|plugin| {
                    let schemas = plugin
                        .config_schemas()
                        .into_iter()
                        .map(|schema| json!({ "key": schema.key, "schema": schema.json_schema }))
                        .collect::<Vec<_>>();
                    json!({
                        "id": plugin.descriptor().name,
                        "config_schemas": schemas,
                    })
                })
            })
            .collect::<Vec<_>>();

        let models = registries
            .models
            .model_ids()
            .into_iter()
            .filter_map(|id| {
                registries.models.get_model(&id).map(|model| {
                    json!({
                        "id": id,
                        "provider": model.provider,
                        "model": model.model_name,
                    })
                })
            })
            .collect::<Vec<_>>();

        let providers = registries
            .providers
            .provider_ids()
            .into_iter()
            .map(|id| json!({ "id": id }))
            .collect::<Vec<_>>();

        let skills = self
            .state
            .skill_catalog_provider
            .as_ref()
            .map(|provider| provider.list_skills())
            .unwrap_or_default();

        Ok(json!({
            "agents": self.state.resolver.agent_ids(),
            "tools": tools,
            "plugins": plugins,
            "skills": skills,
            "models": models,
            "providers": providers,
            "supported_adapters": super::config_runtime::supported_adapters(),
            "namespaces": [
                { "namespace": "agents", "schema": ConfigNamespace::Agents.schema_json()? },
                { "namespace": "models", "schema": ConfigNamespace::Models.schema_json()? },
                { "namespace": "providers", "schema": ConfigNamespace::Providers.schema_json()? },
                { "namespace": "mcp-servers", "schema": ConfigNamespace::McpServers.schema_json()? }
            ],
        }))
    }

    pub async fn list(
        &self,
        namespace: ConfigNamespace,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Value>, ConfigServiceError> {
        let values = self.store.list(namespace.as_str(), offset, limit).await?;
        values
            .into_iter()
            .map(|(_, value)| self.redact_response(namespace, value))
            .collect()
    }

    pub async fn get(
        &self,
        namespace: ConfigNamespace,
        id: &str,
    ) -> Result<Option<Value>, ConfigServiceError> {
        let value = self.store.get(namespace.as_str(), id).await?;
        value
            .map(|value| self.redact_response(namespace, value))
            .transpose()
    }

    pub async fn create(
        &self,
        namespace: ConfigNamespace,
        body: Value,
    ) -> Result<Value, ConfigServiceError> {
        let (id, body) = self.prepare_body(namespace, None, body).await?;
        if self.store.exists(namespace.as_str(), &id).await? {
            return Err(ConfigServiceError::Conflict(format!(
                "{}/{} already exists",
                namespace.as_str(),
                id
            )));
        }

        self.persist_and_apply(namespace, &id, None, body).await
    }

    pub async fn update(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        body: Value,
    ) -> Result<Value, ConfigServiceError> {
        let (body_id, body) = self.prepare_body(namespace, Some(id), body).await?;
        if body_id != id {
            return Err(ConfigServiceError::InvalidPayload(format!(
                "path id '{id}' does not match body id '{body_id}'"
            )));
        }

        let previous = self.store.get(namespace.as_str(), id).await?;
        self.persist_and_apply(namespace, id, previous, body).await
    }

    pub async fn delete(
        &self,
        namespace: ConfigNamespace,
        id: &str,
    ) -> Result<(), ConfigServiceError> {
        let previous = self
            .store
            .get(namespace.as_str(), id)
            .await?
            .ok_or_else(|| {
                ConfigServiceError::NotFound(format!("{}/{}", namespace.as_str(), id))
            })?;

        self.store.delete(namespace.as_str(), id).await?;
        let apply_result = self.apply_runtime_changes().await;
        if let Err(error) = apply_result {
            self.store.put(namespace.as_str(), id, &previous).await?;
            return Err(error);
        }
        Ok(())
    }

    async fn persist_and_apply(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        previous: Option<Value>,
        body: Value,
    ) -> Result<Value, ConfigServiceError> {
        self.validate_payload(namespace, &body)?;
        self.store.put(namespace.as_str(), id, &body).await?;

        let apply_result = self.apply_runtime_changes().await;
        if let Err(error) = apply_result {
            match previous {
                Some(previous) => self.store.put(namespace.as_str(), id, &previous).await?,
                None => self.store.delete(namespace.as_str(), id).await?,
            }
            return Err(error);
        }

        self.redact_response(namespace, body)
    }

    async fn apply_runtime_changes(&self) -> Result<(), ConfigServiceError> {
        let manager = self
            .state
            .config_runtime_manager
            .as_ref()
            .ok_or(ConfigServiceError::NotEnabled)?;
        manager.apply().await.map(|_| ()).map_err(map_runtime_error)
    }

    async fn prepare_body(
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

        if let Some(path_id) = path_id {
            if path_id != id {
                return Err(ConfigServiceError::InvalidPayload(format!(
                    "path id '{path_id}' does not match body id '{id}'"
                )));
            }
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

        Ok((id, Value::Object(object)))
    }

    async fn normalize_provider_payload(
        &self,
        path_id: Option<&str>,
        body: &mut Map<String, Value>,
    ) -> Result<(), ConfigServiceError> {
        let explicit_clear = matches!(body.get("api_key"), Some(Value::String(value)) if value.is_empty())
            || matches!(body.get("api_key"), Some(Value::Null));
        if explicit_clear {
            body.remove("api_key");
            return Ok(());
        }

        if body.contains_key("api_key") || path_id.is_none() {
            return Ok(());
        }

        let Some(path_id) = path_id else {
            return Ok(());
        };
        let Some(existing) = self
            .store
            .get(ConfigNamespace::Providers.as_str(), path_id)
            .await?
        else {
            return Ok(());
        };
        let Some(existing_object) = existing.as_object() else {
            return Ok(());
        };
        if let Some(existing_key) = existing_object.get("api_key") {
            body.insert("api_key".into(), existing_key.clone());
        }
        Ok(())
    }

    fn validate_payload(
        &self,
        namespace: ConfigNamespace,
        body: &Value,
    ) -> Result<(), ConfigServiceError> {
        match namespace {
            ConfigNamespace::Agents => {
                let _: AgentSpec = from_value(body)?;
            }
            ConfigNamespace::Models => {
                let _: ModelSpec = from_value(body)?;
            }
            ConfigNamespace::Providers => {
                let spec: ProviderSpec = from_value(body)?;
                if spec.adapter.trim().is_empty() {
                    return Err(ConfigServiceError::InvalidPayload(
                        "provider adapter cannot be empty".into(),
                    ));
                }
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

    fn redact_response(
        &self,
        namespace: ConfigNamespace,
        value: Value,
    ) -> Result<Value, ConfigServiceError> {
        match namespace {
            ConfigNamespace::Providers => {
                let mut object = into_object(value)?;
                let has_api_key = object
                    .get("api_key")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty());
                object.remove("api_key");
                if has_api_key {
                    object.insert("has_api_key".into(), Value::Bool(true));
                }
                Ok(Value::Object(object))
            }
            ConfigNamespace::McpServers => {
                let mut object = into_object(value)?;
                let env_keys = object
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|env| {
                        let mut keys = env.keys().cloned().collect::<Vec<_>>();
                        keys.sort();
                        keys
                    })
                    .unwrap_or_default();
                object.remove("env");
                if !env_keys.is_empty() {
                    object.insert("has_env".into(), Value::Bool(true));
                    object.insert(
                        "env_keys".into(),
                        Value::Array(env_keys.into_iter().map(Value::String).collect()),
                    );
                }
                Ok(Value::Object(object))
            }
            ConfigNamespace::Agents | ConfigNamespace::Models => Ok(value),
        }
    }

    async fn normalize_mcp_server_payload(
        &self,
        path_id: Option<&str>,
        body: &mut Map<String, Value>,
    ) -> Result<(), ConfigServiceError> {
        if body.contains_key("env") || path_id.is_none() {
            return Ok(());
        }

        let Some(path_id) = path_id else {
            return Ok(());
        };
        let Some(existing) = self
            .store
            .get(ConfigNamespace::McpServers.as_str(), path_id)
            .await?
        else {
            return Ok(());
        };
        let Some(existing_object) = existing.as_object() else {
            return Ok(());
        };
        if let Some(existing_env) = existing_object.get("env") {
            body.insert("env".into(), existing_env.clone());
        }
        Ok(())
    }
}

fn map_runtime_error(error: ConfigRuntimeError) -> ConfigServiceError {
    match error {
        ConfigRuntimeError::UnsupportedProviderAdapter(_)
        | ConfigRuntimeError::InvalidConfig(_)
        | ConfigRuntimeError::PartialBootstrap => {
            ConfigServiceError::InvalidPayload(error.to_string())
        }
        ConfigRuntimeError::RuntimeNotConfigurable
        | ConfigRuntimeError::PeriodicRefresh(_)
        | ConfigRuntimeError::ChangeListener(_) => ConfigServiceError::Apply(error.to_string()),
        ConfigRuntimeError::Storage(error) => ConfigServiceError::Storage(error),
    }
}

fn into_object(value: Value) -> Result<Map<String, Value>, ConfigServiceError> {
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
