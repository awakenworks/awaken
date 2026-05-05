use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use awaken_contract::AuditAction;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::{AgentSpec, McpServerSpec, ModelBindingSpec, ProviderSpec};
use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

use crate::app::AppState;
use crate::services::audit_log::AuditLogger;

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
            Self::Models => schemars::schema_for!(ModelBindingSpec),
            Self::Providers => schemars::schema_for!(ProviderSpec),
            Self::McpServers => schemars::schema_for!(McpServerSpec),
        };
        serde_json::to_value(schema)
            .map_err(|error| ConfigServiceError::Serialization(error.to_string()))
    }
}

/// A record that depends on the resource being deleted.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DependentRef {
    pub namespace: &'static str,
    pub id: String,
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
    #[error("blocked: {used_by:?} record(s) depend on this resource")]
    Blocked { used_by: Vec<DependentRef> },
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Error type for the config restore operation.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    #[error("audit log is not configured")]
    AuditNotConfigured,
    #[error("version not found")]
    VersionNotFound,
    #[error(
        "cross-resource restore not allowed: event is for '{event_resource}', expected '{expected}'"
    )]
    ResourceMismatch {
        event_resource: String,
        expected: String,
    },
    #[error("action '{0:?}' does not carry a restorable spec")]
    NoPayload(AuditAction),
    #[error("restart events are not restorable")]
    NotRestorable,
    #[error("config service error: {0}")]
    Service(#[from] ConfigServiceError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Result returned by the provider test endpoint.
#[derive(Debug, serde::Serialize)]
pub struct ProviderTestResult {
    pub ok: bool,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct ConfigService<'a> {
    state: &'a AppState,
    store: Arc<dyn ConfigStore>,
    audit: Option<Arc<AuditLogger>>,
}

impl<'a> ConfigService<'a> {
    pub fn new(state: &'a AppState) -> Result<Self, ConfigServiceError> {
        let store = state
            .config_store
            .clone()
            .ok_or(ConfigServiceError::NotEnabled)?;
        Ok(Self {
            state,
            store,
            audit: state.audit_log.clone(),
        })
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
                    // First-pass classifier: derive source from tool id prefix.
                    // MCP tools follow "mcp__{server}__{tool}"; plugin tools use
                    // arbitrary ids registered by plugins. If the registry gains
                    // explicit source tracking, replace this with that.
                    let source = classify_tool_source(&descriptor.id);
                    json!({
                        "id": descriptor.id,
                        "name": descriptor.name,
                        "description": descriptor.description,
                        "source": source,
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
                        "provider_id": model.provider_id,
                        "upstream_model": model.upstream_model,
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

    /// Dry-run validation. Runs the same `prepare_body` + `validate_payload`
    /// pass that `create` / `update` perform, but does **not** touch the
    /// config store and does **not** apply the resulting snapshot to the
    /// running runtime. Useful for the admin console's "Validate before save"
    /// affordance.
    ///
    /// Returns the normalized body (with id, timestamps, and namespace-specific
    /// rewrites applied) so callers can preview exactly what would be persisted.
    pub async fn validate(
        &self,
        namespace: ConfigNamespace,
        path_id: Option<&str>,
        body: Value,
    ) -> Result<Value, ConfigServiceError> {
        let (id, normalized) = self.prepare_body(namespace, path_id, body).await?;
        if let Some(path_id) = path_id
            && path_id != id
        {
            return Err(ConfigServiceError::InvalidPayload(format!(
                "path id '{path_id}' does not match body id '{id}'"
            )));
        }
        self.validate_payload(namespace, &normalized)?;
        Ok(normalized)
    }

    pub async fn create(
        &self,
        namespace: ConfigNamespace,
        body: Value,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;
        let (id, body) = self.prepare_body(namespace, None, body).await?;
        if self.store.exists(namespace.as_str(), &id).await? {
            return Err(ConfigServiceError::Conflict(format!(
                "{}/{} already exists",
                namespace.as_str(),
                id
            )));
        }

        let result = self
            .persist_and_apply_locked(manager.as_ref(), namespace, &id, None, body.clone())
            .await?;

        self.emit_audit(
            AuditAction::Create,
            namespace,
            &id,
            None,
            Some(body),
            headers,
        )
        .await;

        Ok(result)
    }

    pub async fn update(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        body: Value,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;
        let (body_id, body) = self.prepare_body(namespace, Some(id), body).await?;
        if body_id != id {
            return Err(ConfigServiceError::InvalidPayload(format!(
                "path id '{id}' does not match body id '{body_id}'"
            )));
        }

        let previous = self.store.get(namespace.as_str(), id).await?;
        let result = self
            .persist_and_apply_locked(
                manager.as_ref(),
                namespace,
                id,
                previous.clone(),
                body.clone(),
            )
            .await?;

        self.emit_audit(
            AuditAction::Update,
            namespace,
            id,
            previous,
            Some(body),
            headers,
        )
        .await;

        Ok(result)
    }

    pub async fn delete(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        force: bool,
        headers: &HeaderMap,
    ) -> Result<(), ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;
        let previous = self
            .store
            .get(namespace.as_str(), id)
            .await?
            .ok_or_else(|| {
                ConfigServiceError::NotFound(format!("{}/{}", namespace.as_str(), id))
            })?;

        if !force {
            let blockers = self.find_dependents(namespace, id).await?;
            if !blockers.is_empty() {
                return Err(ConfigServiceError::Blocked { used_by: blockers });
            }
        }

        self.store.delete(namespace.as_str(), id).await?;
        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.store.put(namespace.as_str(), id, &previous).await?;
            return Err(error);
        }

        self.emit_audit(
            AuditAction::Delete,
            namespace,
            id,
            Some(previous),
            None,
            headers,
        )
        .await;

        Ok(())
    }

    /// Restore a resource to a previous version identified by the audit event ULID `version`.
    ///
    /// Per ADR-0028 D2-D4:
    /// - Looks up the audit event; returns `RestoreError::VersionNotFound` if missing.
    /// - Validates that the event resource matches `<namespace>/<id>` (cross-resource rejected).
    /// - Selects the spec payload: `after` for Create/Update/Publish/Restore; `before` for Delete.
    /// - Returns `RestoreError::NotRestorable` for Restart events (no spec payload).
    /// - Calls `persist_and_apply_locked` directly (both create and update paths) to avoid
    ///   emitting a spurious Update audit event; only a single Restore event is written.
    /// - Emits a Restore audit event with `restored_from` set to the source ULID.
    pub async fn restore(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        version: &str,
        headers: &HeaderMap,
    ) -> Result<Value, RestoreError> {
        use awaken_contract::AuditAction as A;

        let audit = self
            .audit
            .as_ref()
            .ok_or(RestoreError::AuditNotConfigured)?;

        // Look up the source audit event.
        let event = audit
            .get_event(version)
            .await
            .map_err(RestoreError::Storage)?
            .ok_or(RestoreError::VersionNotFound)?;

        // Verify cross-resource guard.
        let expected_resource = format!("{}/{}", namespace.as_str(), id);
        if event.resource != expected_resource {
            return Err(RestoreError::ResourceMismatch {
                event_resource: event.resource.clone(),
                expected: expected_resource,
            });
        }

        // Select payload per D3 mapping table.
        let payload = match &event.action {
            A::Create | A::Update | A::Publish | A::Restore => event
                .after
                .clone()
                .ok_or(RestoreError::NoPayload(event.action.clone()))?,
            A::Delete => event
                .before
                .clone()
                .ok_or(RestoreError::NoPayload(event.action.clone()))?,
            A::Restart => return Err(RestoreError::NotRestorable),
        };

        // Single store read: determines both existence and the pre-restore snapshot.
        let before = self
            .store
            .get(namespace.as_str(), id)
            .await
            .map_err(RestoreError::Storage)?;

        let manager = self.runtime_manager().map_err(RestoreError::Service)?;
        let _apply_guard = manager.lock_apply().await;

        let result = if before.is_some() {
            // Resource exists — inline the update logic so we emit only a Restore
            // audit event (calling update() would also fire an Update event).
            let (body_id, prepared) = self
                .prepare_body(namespace, Some(id), payload.clone())
                .await
                .map_err(RestoreError::Service)?;
            if body_id != id {
                return Err(RestoreError::Service(ConfigServiceError::InvalidPayload(
                    format!("restored payload id '{body_id}' does not match URL id '{id}'"),
                )));
            }
            self.persist_and_apply_locked(manager.as_ref(), namespace, id, before.clone(), prepared)
                .await
                .map_err(RestoreError::Service)?
        } else {
            // Resource does not exist — restore from a deleted state.
            // We need to preserve created_at from the restored payload.
            let (body_id, mut prepared) = self
                .prepare_body(namespace, None, payload.clone())
                .await
                .map_err(RestoreError::Service)?;
            if body_id != id {
                return Err(RestoreError::Service(ConfigServiceError::InvalidPayload(
                    format!("restored payload id '{body_id}' does not match URL id '{id}'"),
                )));
            }

            // Restore created_at from the original payload if present.
            if let (Some(original_created_at), Some(obj)) = (
                payload
                    .as_object()
                    .and_then(|o| o.get("created_at"))
                    .cloned(),
                prepared.as_object_mut(),
            ) {
                obj.insert("created_at".to_string(), original_created_at);
            }

            if self
                .store
                .exists(namespace.as_str(), &body_id)
                .await
                .map_err(RestoreError::Storage)?
            {
                return Err(RestoreError::Service(ConfigServiceError::Conflict(
                    format!("{}/{} already exists", namespace.as_str(), body_id),
                )));
            }

            self.persist_and_apply_locked(
                manager.as_ref(),
                namespace,
                &body_id,
                None,
                prepared.clone(),
            )
            .await
            .map_err(RestoreError::Service)?
        };

        // Emit restore audit event.
        if let Some(audit) = &self.audit {
            let resource = format!("{}/{}", namespace.as_str(), id);
            audit
                .emit_restore(
                    &resource,
                    before,
                    Some(payload),
                    version.to_string(),
                    headers,
                )
                .await;
        }

        Ok(result)
    }

    /// Return all records in other namespaces that reference `id` in `namespace`.
    ///
    /// - Providers: scans models for `provider_id == id`
    /// - Models: scans agents for `model_id == id`
    /// - Agents / McpServers: leaf nodes, no dependents
    async fn find_dependents(
        &self,
        namespace: ConfigNamespace,
        id: &str,
    ) -> Result<Vec<DependentRef>, ConfigServiceError> {
        match namespace {
            ConfigNamespace::Providers => {
                let models = self.store.list("models", 0, usize::MAX).await?;
                let refs = models
                    .into_iter()
                    .filter(|(_, value)| {
                        value
                            .get("provider_id")
                            .and_then(Value::as_str)
                            .is_some_and(|pid| pid == id)
                    })
                    .map(|(model_id, _)| DependentRef {
                        namespace: "models",
                        id: model_id,
                    })
                    .collect();
                Ok(refs)
            }
            ConfigNamespace::Models => {
                let agents = self.store.list("agents", 0, usize::MAX).await?;
                let refs = agents
                    .into_iter()
                    .filter(|(_, value)| {
                        value
                            .get("model_id")
                            .and_then(Value::as_str)
                            .is_some_and(|mid| mid == id)
                    })
                    .map(|(agent_id, _)| DependentRef {
                        namespace: "agents",
                        id: agent_id,
                    })
                    .collect();
                Ok(refs)
            }
            ConfigNamespace::Agents | ConfigNamespace::McpServers => Ok(vec![]),
        }
    }

    /// Emit an audit event if an audit logger is configured.
    ///
    /// Best-effort: the call is fire-and-forget (matching the `AuditLogger::emit` contract).
    async fn emit_audit(
        &self,
        action: AuditAction,
        namespace: ConfigNamespace,
        id: &str,
        before: Option<Value>,
        after: Option<Value>,
        headers: &HeaderMap,
    ) {
        let Some(audit) = &self.audit else { return };
        let resource = format!("{}/{}", namespace.as_str(), id);
        audit.emit(action, &resource, before, after, headers).await;
    }

    fn runtime_manager(
        &self,
    ) -> Result<&Arc<crate::services::config_runtime::ConfigRuntimeManager>, ConfigServiceError>
    {
        self.state
            .config_runtime_manager
            .as_ref()
            .ok_or(ConfigServiceError::NotEnabled)
    }

    async fn persist_and_apply_locked(
        &self,
        manager: &crate::services::config_runtime::ConfigRuntimeManager,
        namespace: ConfigNamespace,
        id: &str,
        previous: Option<Value>,
        body: Value,
    ) -> Result<Value, ConfigServiceError> {
        self.validate_payload(namespace, &body)?;
        self.store.put(namespace.as_str(), id, &body).await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            match previous {
                Some(previous) => self.store.put(namespace.as_str(), id, &previous).await?,
                None => self.store.delete(namespace.as_str(), id).await?,
            }
            return Err(error);
        }

        self.redact_response(namespace, body)
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

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        if path_id.is_none() {
            // Create: set both timestamps.
            object.insert("created_at".into(), Value::Number(now.into()));
            object.insert("updated_at".into(), Value::Number(now.into()));
        } else {
            // Update: preserve existing created_at if present; always refresh updated_at.
            if !object.contains_key("created_at") {
                if let Ok(Some(existing)) = self.store.get(namespace.as_str(), &id).await {
                    if let Some(existing_created_at) = existing
                        .as_object()
                        .and_then(|obj| obj.get("created_at"))
                        .cloned()
                    {
                        object.insert("created_at".into(), existing_created_at);
                    } else {
                        object.insert("created_at".into(), Value::Number(now.into()));
                    }
                } else {
                    object.insert("created_at".into(), Value::Number(now.into()));
                }
            }
            object.insert("updated_at".into(), Value::Number(now.into()));
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
                let _: ModelBindingSpec = from_value(body)?;
            }
            ConfigNamespace::Providers => {
                let spec: ProviderSpec = from_value(body)?;
                if spec.adapter.trim().is_empty() {
                    return Err(ConfigServiceError::InvalidPayload(
                        "provider adapter cannot be empty".into(),
                    ));
                }
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

    /// Test whether a stored provider config is usable.
    ///
    /// Strategy depends on `credentials_kind`:
    ///
    /// - **Static / bearer** (default): construction-only probe. Loads the
    ///   stored `ProviderSpec` and runs `build_genai_provider_executor` to
    ///   prove that the adapter name parses, the api_key (if any) is the
    ///   right shape, and adapter_options are valid. **No network call.**
    ///
    /// - **Dynamic** (`service_account_json`, future cloud creds):
    ///   construction-only probe **plus** a live token mint via the
    ///   credential broker. This catches revoked keys, deleted service
    ///   accounts, unreachable token endpoints, and missing scopes —
    ///   problems that a construction probe cannot see. The mint reuses
    ///   the same broker code that production inference does, so a
    ///   passing test is strong evidence that the next inference will
    ///   succeed at the auth layer.
    ///
    /// In both cases the LLM endpoint itself is not contacted; that
    /// would require a full runtime context (cancellation, streaming,
    /// observability) and would also bill the user for a token.
    pub async fn test_provider(&self, id: &str) -> Result<ProviderTestResult, ConfigServiceError> {
        let raw = self
            .store
            .get(ConfigNamespace::Providers.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("providers/{id}")))?;

        let spec: ProviderSpec = serde_json::from_value(raw)
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        // Construction probe: catches adapter parsing, material parsing,
        // header validation, and any other build-time check. Reuses the
        // production builder so any change to the build path is covered
        // here automatically.
        let start = Instant::now();
        let broker: std::sync::Arc<dyn awaken_runtime::credentials::CredentialBroker> =
            std::sync::Arc::new(awaken_runtime::credentials::AwakenCredentialBroker::new());
        let build_result = crate::services::config_runtime::build_genai_provider_executor(
            &spec,
            std::sync::Arc::clone(&broker),
        );
        let mut latency_ms = start.elapsed().as_millis() as u64;

        if let Err(e) = build_result {
            return Ok(ProviderTestResult {
                ok: false,
                latency_ms,
                error: Some(e.to_string()),
            });
        }

        // Pre-flight token mint for dynamic credentials. Skipped for bearer
        // (static / env-var fallback) where the broker would either no-op
        // or hand back the static value — neither tests anything new.
        let kind = match awaken_runtime::credentials::CredentialKind::from_options(
            &spec.adapter_options,
        ) {
            Ok(k) => k,
            Err(_) => {
                // Already caught by the build probe above; defensive-coded.
                return Ok(ProviderTestResult {
                    ok: true,
                    latency_ms,
                    error: None,
                });
            }
        };
        if matches!(
            kind,
            awaken_runtime::credentials::CredentialKind::GoogleServiceAccountJson
        ) {
            let scope = "https://www.googleapis.com/auth/cloud-platform";
            let mint_start = Instant::now();
            let mint_result = broker.token_for(&spec.id, scope).await;
            latency_ms = latency_ms.saturating_add(mint_start.elapsed().as_millis() as u64);
            if let Err(err) = mint_result {
                return Ok(ProviderTestResult {
                    ok: false,
                    latency_ms,
                    error: Some(err.to_string()),
                });
            }
        }

        Ok(ProviderTestResult {
            ok: true,
            latency_ms,
            error: None,
        })
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

/// Classify a tool's source from its id.
///
/// MCP tools registered by `awaken-ext-mcp` follow `mcp__{server}__{tool}`.
/// The global registry currently holds only built-in tools, but the classifier
/// is written defensively so it still works if MCP tools are ever surfaced here.
fn classify_tool_source(id: &str) -> Value {
    if let Some(rest) = id.strip_prefix("mcp__") {
        // Extract server id: the segment between the two `__` delimiters.
        let server = rest.split("__").next().unwrap_or(rest);
        return json!({ "kind": "mcp", "id": server });
    }
    json!({ "kind": "builtin" })
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::{AgentSpec, ModelBindingSpec, ProviderSpec};
    use awaken_runtime::builder::AgentRuntimeBuilder;
    use awaken_runtime::registry::traits::ModelBinding;
    use serde_json::{Value, json};
    use tokio::sync::Notify;

    use crate::app::{AppState, ServerConfig};
    use crate::mailbox::{Mailbox, MailboxConfig};
    use crate::services::config_runtime::{ConfigRuntimeManager, ProviderExecutorFactory};

    use super::{ConfigNamespace, ConfigService, ConfigServiceError};

    struct ImmediateExecutor;

    #[async_trait]
    impl LlmExecutor for ImmediateExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "immediate"
        }
    }

    struct TestProviderFactory;

    impl ProviderExecutorFactory for TestProviderFactory {
        fn build(
            &self,
            spec: &ProviderSpec,
        ) -> Result<Arc<dyn LlmExecutor>, crate::services::config_runtime::ConfigRuntimeError>
        {
            if spec.adapter.eq_ignore_ascii_case("stub") {
                return Ok(Arc::new(ImmediateExecutor));
            }

            Err(
                crate::services::config_runtime::ConfigRuntimeError::UnsupportedProviderAdapter(
                    spec.adapter.clone(),
                ),
            )
        }
    }

    struct BlockingConfigStore {
        inner: Arc<awaken_stores::InMemoryStore>,
        block_lists: AtomicBool,
        list_started: AtomicBool,
        release_lists: Notify,
    }

    impl BlockingConfigStore {
        fn new(inner: Arc<awaken_stores::InMemoryStore>) -> Self {
            Self {
                inner,
                block_lists: AtomicBool::new(false),
                list_started: AtomicBool::new(false),
                release_lists: Notify::new(),
            }
        }

        fn block_lists(&self) {
            self.list_started.store(false, Ordering::SeqCst);
            self.block_lists.store(true, Ordering::SeqCst);
        }

        fn unblock_lists(&self) {
            self.block_lists.store(false, Ordering::SeqCst);
            self.release_lists.notify_waiters();
        }

        fn list_started(&self) -> bool {
            self.list_started.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ConfigStore for BlockingConfigStore {
        async fn get(
            &self,
            namespace: &str,
            id: &str,
        ) -> Result<Option<Value>, awaken_contract::contract::storage::StorageError> {
            ConfigStore::get(self.inner.as_ref(), namespace, id).await
        }

        async fn list(
            &self,
            namespace: &str,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<(String, Value)>, awaken_contract::contract::storage::StorageError>
        {
            if self.block_lists.load(Ordering::SeqCst) {
                self.list_started.store(true, Ordering::SeqCst);
                self.release_lists.notified().await;
            }

            ConfigStore::list(self.inner.as_ref(), namespace, offset, limit).await
        }

        async fn put(
            &self,
            namespace: &str,
            id: &str,
            value: &Value,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            ConfigStore::put(self.inner.as_ref(), namespace, id, value).await
        }

        async fn delete(
            &self,
            namespace: &str,
            id: &str,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            ConfigStore::delete(self.inner.as_ref(), namespace, id).await
        }
    }

    fn bootstrap_agent() -> AgentSpec {
        AgentSpec {
            id: "bootstrap".into(),
            model_id: "bootstrap".into(),
            system_prompt: "bootstrap".into(),
            max_rounds: 1,
            ..Default::default()
        }
    }

    async fn build_state(
        config_store: Arc<dyn ConfigStore>,
    ) -> (AppState, Arc<ConfigRuntimeManager>) {
        let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider("bootstrap", Arc::new(ImmediateExecutor))
                .with_model_binding(
                    "bootstrap",
                    ModelBinding {
                        provider_id: "bootstrap".into(),
                        upstream_model: "bootstrap-model".into(),
                    },
                )
                .with_agent_spec(bootstrap_agent())
                .with_thread_run_store(thread_store.clone())
                .build()
                .expect("build runtime"),
        );

        let manager = Arc::new(
            ConfigRuntimeManager::new(runtime.clone(), config_store.clone())
                .expect("config runtime manager")
                .with_provider_factory(Arc::new(TestProviderFactory)),
        );
        let resolver = runtime.resolver_arc();
        manager
            .bootstrap_if_empty(
                &[ProviderSpec {
                    id: "bootstrap".into(),
                    adapter: "stub".into(),
                    ..Default::default()
                }],
                &[ModelBindingSpec {
                    id: "bootstrap".into(),
                    provider_id: "bootstrap".into(),
                    upstream_model: "bootstrap-model".into(),
                    created_at: None,
                    updated_at: None,
                }],
                &[bootstrap_agent()],
                &[],
            )
            .await
            .expect("bootstrap config store");
        manager.apply().await.expect("publish config");

        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            Arc::new(awaken_stores::InMemoryMailboxStore::new()),
            thread_store.clone(),
            "config-service-test".into(),
            MailboxConfig::default(),
        ));
        let state = AppState::new(
            runtime,
            mailbox,
            thread_store,
            resolver,
            ServerConfig::default(),
        )
        .with_config_store(config_store)
        .with_config_runtime_manager(manager.clone());

        (state, manager)
    }

    async fn wait_until(
        timeout: Duration,
        interval: Duration,
        mut predicate: impl FnMut() -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if predicate() {
                return true;
            }
            tokio::time::sleep(interval).await;
        }
        predicate()
    }

    #[tokio::test]
    async fn create_waits_for_in_flight_apply_before_writing_store() {
        let raw_store = Arc::new(awaken_stores::InMemoryStore::new());
        let blocking_store = Arc::new(BlockingConfigStore::new(raw_store.clone()));
        let config_store = blocking_store.clone() as Arc<dyn ConfigStore>;
        let (state, manager) = build_state(config_store.clone()).await;

        blocking_store.block_lists();
        let apply_task = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .apply_if_changed()
                    .await
                    .expect("apply_if_changed should complete")
            }
        });

        let list_blocked = wait_until(Duration::from_secs(1), Duration::from_millis(10), || {
            blocking_store.list_started()
        })
        .await;
        assert!(
            list_blocked,
            "background apply should enter the config snapshot load"
        );

        let create_task = tokio::spawn({
            let state = state.clone();
            async move {
                let service = ConfigService::new(&state).expect("config service");
                service
                    .create(
                        ConfigNamespace::Providers,
                        json!({
                            "id": "serialized",
                            "adapter": "stub"
                        }),
                        &axum::http::HeaderMap::new(),
                    )
                    .await
            }
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        let pending = ConfigStore::get(config_store.as_ref(), "providers", "serialized")
            .await
            .expect("read provider");
        assert!(
            pending.is_none(),
            "config writes must wait for in-flight apply snapshots before touching the store"
        );
        assert!(
            !create_task.is_finished(),
            "create should stay blocked behind the apply lock"
        );

        blocking_store.unblock_lists();
        let apply_result = apply_task.await.expect("join apply task");
        assert_eq!(apply_result, None);

        let created = create_task
            .await
            .expect("join create task")
            .expect("create should succeed");
        assert_eq!(created["id"], "serialized");

        let stored = ConfigStore::get(config_store.as_ref(), "providers", "serialized")
            .await
            .expect("read provider after create");
        assert_eq!(
            stored
                .as_ref()
                .and_then(|value| value.get("id"))
                .and_then(Value::as_str),
            Some("serialized")
        );
    }

    #[tokio::test]
    async fn service_requires_runtime_manager_for_mutations() {
        let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider("bootstrap", Arc::new(ImmediateExecutor))
                .with_model_binding(
                    "bootstrap",
                    ModelBinding {
                        provider_id: "bootstrap".into(),
                        upstream_model: "bootstrap-model".into(),
                    },
                )
                .with_agent_spec(bootstrap_agent())
                .with_thread_run_store(thread_store.clone())
                .build()
                .expect("build runtime"),
        );
        let mailbox = Arc::new(Mailbox::new(
            runtime.clone(),
            Arc::new(awaken_stores::InMemoryMailboxStore::new()),
            thread_store.clone(),
            "config-service-test".into(),
            MailboxConfig::default(),
        ));
        let state = AppState::new(
            runtime.clone(),
            mailbox,
            thread_store,
            runtime.resolver_arc(),
            ServerConfig::default(),
        )
        .with_config_store(Arc::new(awaken_stores::InMemoryStore::new()));

        let service = ConfigService::new(&state).expect("config service");
        let error = service
            .create(
                ConfigNamespace::Providers,
                json!({
                    "id": "missing-manager",
                    "adapter": "stub"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("missing manager should reject writes");
        assert!(matches!(error, ConfigServiceError::NotEnabled));
    }

    // ── find_dependents / blocked delete tests ──────────────────────────────

    #[tokio::test]
    async fn find_dependents_provider_returns_referencing_models() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        // Create a model that references provider "bootstrap"
        service
            .create(
                ConfigNamespace::Models,
                json!({
                    "id": "model-ref-bootstrap",
                    "provider_id": "bootstrap",
                    "upstream_model": "gpt-4"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create model");

        let dependents = service
            .find_dependents(ConfigNamespace::Providers, "bootstrap")
            .await
            .expect("find_dependents");

        assert_eq!(dependents.len(), 2, "bootstrap model + model-ref-bootstrap");
        let ids: Vec<&str> = dependents.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"model-ref-bootstrap"));
        for d in &dependents {
            assert_eq!(d.namespace, "models");
        }
    }

    #[tokio::test]
    async fn find_dependents_model_returns_referencing_agents() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        // Create an agent referencing the bootstrap model
        service
            .create(
                ConfigNamespace::Agents,
                json!({
                    "id": "agent-ref-bootstrap",
                    "model_id": "bootstrap",
                    "system_prompt": "test",
                    "max_rounds": 1
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create agent");

        let dependents = service
            .find_dependents(ConfigNamespace::Models, "bootstrap")
            .await
            .expect("find_dependents");

        assert!(!dependents.is_empty());
        let ids: Vec<&str> = dependents.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"agent-ref-bootstrap"));
        for d in &dependents {
            assert_eq!(d.namespace, "agents");
        }
    }

    #[tokio::test]
    async fn find_dependents_agents_and_mcp_servers_are_leaf_nodes() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        let agent_deps = service
            .find_dependents(ConfigNamespace::Agents, "any-agent")
            .await
            .expect("find_dependents agents");
        assert!(agent_deps.is_empty());

        let mcp_deps = service
            .find_dependents(ConfigNamespace::McpServers, "any-mcp")
            .await
            .expect("find_dependents mcp-servers");
        assert!(mcp_deps.is_empty());
    }

    #[tokio::test]
    async fn delete_without_force_returns_blocked_when_dependents_exist() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        // Create a second provider and a model referencing it
        service
            .create(
                ConfigNamespace::Providers,
                json!({ "id": "prov-b", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider-b");

        service
            .create(
                ConfigNamespace::Models,
                json!({
                    "id": "model-b",
                    "provider_id": "prov-b",
                    "upstream_model": "gpt-4"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create model-b");

        let err = service
            .delete(
                ConfigNamespace::Providers,
                "prov-b",
                false,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("should be blocked");

        assert!(
            matches!(err, ConfigServiceError::Blocked { ref used_by } if !used_by.is_empty()),
            "expected Blocked error"
        );
    }

    #[tokio::test]
    async fn delete_with_force_removes_despite_dependents() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        service
            .create(
                ConfigNamespace::Providers,
                json!({ "id": "prov-c", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider-c");

        service
            .create(
                ConfigNamespace::Models,
                json!({
                    "id": "model-c",
                    "provider_id": "prov-c",
                    "upstream_model": "gpt-4"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create model-c");

        // Force delete should succeed even with dependents
        service
            .delete(
                ConfigNamespace::Providers,
                "prov-c",
                true,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("force delete should succeed");
    }

    // ── audit integration tests ────────────────────────────────────────────

    mod audit_integration {
        use std::sync::Arc;

        use awaken_contract::AuditAction;
        use axum::http::HeaderMap;
        use serde_json::json;

        use crate::services::audit_log::{AUDIT_NAMESPACE, AuditLogger, AuditQuery};
        use crate::services::config_service::{ConfigNamespace, ConfigService};

        use super::build_state;

        #[tokio::test]
        async fn create_emits_audit_create_event() {
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let (state, _manager) = build_state(config_store.clone()).await;
            let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
            let state = state.with_audit_log(audit_logger.clone());

            let service = ConfigService::new(&state).expect("service");
            service
                .create(
                    ConfigNamespace::Providers,
                    json!({ "id": "audit-prov", "adapter": "stub" }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create");

            let page = audit_logger.query(AuditQuery::default()).await.unwrap();
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].action, AuditAction::Create);
            assert_eq!(page.items[0].resource, "providers/audit-prov");
            assert!(page.items[0].before.is_none());
            assert!(page.items[0].after.is_some());
        }

        #[tokio::test]
        async fn update_emits_audit_update_event_with_before_after() {
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let (state, _manager) = build_state(config_store.clone()).await;
            let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
            let state = state.with_audit_log(audit_logger.clone());

            let service = ConfigService::new(&state).expect("service");
            service
                .create(
                    ConfigNamespace::Agents,
                    json!({ "id": "upd-agent", "model_id": "bootstrap", "system_prompt": "v1", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create");

            service
                .update(
                    ConfigNamespace::Agents,
                    "upd-agent",
                    json!({ "id": "upd-agent", "model_id": "bootstrap", "system_prompt": "v2", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("update");

            let page = audit_logger
                .query(AuditQuery {
                    action: Some(AuditAction::Update),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].action, AuditAction::Update);
            assert!(page.items[0].before.is_some(), "before must be set");
            assert!(page.items[0].after.is_some(), "after must be set");
        }

        #[tokio::test]
        async fn delete_emits_audit_delete_event_with_before() {
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let (state, _manager) = build_state(config_store.clone()).await;
            let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
            let state = state.with_audit_log(audit_logger.clone());

            let service = ConfigService::new(&state).expect("service");
            service
                .create(
                    ConfigNamespace::Agents,
                    json!({ "id": "del-agent", "model_id": "bootstrap", "system_prompt": "hi", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create");

            service
                .delete(
                    ConfigNamespace::Agents,
                    "del-agent",
                    false,
                    &HeaderMap::new(),
                )
                .await
                .expect("delete");

            // Only the Delete event should be in audit (create is there too but filter by Delete).
            let page = audit_logger
                .query(AuditQuery {
                    action: Some(AuditAction::Delete),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].action, AuditAction::Delete);
            assert!(
                page.items[0].before.is_some(),
                "before must contain deleted payload"
            );
            assert!(
                page.items[0].after.is_none(),
                "after must be None for delete"
            );
        }

        #[tokio::test]
        async fn config_write_succeeds_even_when_audit_store_separate_and_no_logger() {
            // Verify that without an audit logger, create still succeeds.
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let (state, _manager) = build_state(config_store.clone()).await;
            // No audit_log attached.

            let service = ConfigService::new(&state).expect("service");
            service
                .create(
                    ConfigNamespace::Agents,
                    json!({ "id": "no-audit-agent", "model_id": "bootstrap", "system_prompt": "hi", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create without audit should succeed");

            // Confirm no audit entries exist.
            let audit_entries = awaken_contract::contract::config_store::ConfigStore::list(
                config_store.as_ref(),
                AUDIT_NAMESPACE,
                0,
                usize::MAX,
            )
            .await
            .unwrap();
            assert!(audit_entries.is_empty());
        }
    }
}
