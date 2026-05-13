use std::sync::Arc;

use awaken_contract::AuditAction;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::{
    AgentSpec, ConfigRecord, McpServerSpec, ModelBindingSpec, ProviderSpec, ToolSpec,
};
use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::services::audit_log::AuditLogger;
use crate::services::config_envelope::unwrap_spec;

use super::config_runtime::ConfigRuntimeError;

mod agent_overrides;
mod audit;
mod mcp;
mod normalization;
mod provider;
mod storage;
mod tool_overrides;

use normalization::{
    classify_tool_source, effective_spec, effective_tool_spec, effective_visible_record,
};

pub(super) const TOOLS_NAMESPACE: &str = "tools";
const OVERRIDES_NOT_SUPPORTED_FOR_USER_RECORD: &str =
    "overrides are not supported for user-source records; use PUT to update";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigNamespace {
    Agents,
    Models,
    Providers,
    McpServers,
}

impl ConfigNamespace {
    /// All 0.4-compatible public managed namespaces in a fixed order.
    pub const ALL: [Self; 4] = [
        Self::Agents,
        Self::Providers,
        Self::Models,
        Self::McpServers,
    ];

    /// Slice over all public namespace variants.
    pub fn all() -> &'static [Self] {
        &Self::ALL
    }

    /// Iterator over the `&'static str` names of all public namespaces.
    pub fn iter_str() -> impl Iterator<Item = &'static str> + 'static {
        Self::ALL.iter().copied().map(Self::as_str)
    }

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

pub(crate) fn tool_schema_json() -> Result<Value, ConfigServiceError> {
    serde_json::to_value(schemars::schema_for!(ToolSpec))
        .map_err(|error| ConfigServiceError::Serialization(error.to_string()))
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
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

fn blocked_by_dependents(used_by: Vec<DependentRef>) -> ConfigServiceError {
    ConfigServiceError::Conflict(format!(
        "blocked: {used_by:?} record(s) depend on this resource"
    ))
}

pub(super) fn overrides_not_supported_for_user_record() -> ConfigServiceError {
    ConfigServiceError::InvalidPayload(OVERRIDES_NOT_SUPPORTED_FOR_USER_RECORD.into())
}

pub(crate) fn is_overrides_not_supported_for_user_record(error: &ConfigServiceError) -> bool {
    matches!(error, ConfigServiceError::InvalidPayload(message) if message == OVERRIDES_NOT_SUPPORTED_FOR_USER_RECORD)
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
    pub network_tested: bool,
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
            audit: state.audit_log(),
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
                { "namespace": "mcp-servers", "schema": ConfigNamespace::McpServers.schema_json()? },
                { "namespace": TOOLS_NAMESPACE, "schema": tool_schema_json()? }
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
            .map(|(_, value)| self.redact_response(namespace, effective_spec(namespace, value)?))
            .collect()
    }

    pub async fn get(
        &self,
        namespace: ConfigNamespace,
        id: &str,
    ) -> Result<Option<Value>, ConfigServiceError> {
        let value = self.store.get(namespace.as_str(), id).await?;
        value
            .map(|value| self.redact_response(namespace, effective_spec(namespace, value)?))
            .transpose()
    }

    pub(crate) async fn list_tools(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<Value>, ConfigServiceError> {
        let values = self.store.list(TOOLS_NAMESPACE, offset, limit).await?;
        values
            .into_iter()
            .map(|(_, value)| effective_tool_spec(value))
            .collect()
    }

    pub(crate) async fn get_tool(&self, id: &str) -> Result<Option<Value>, ConfigServiceError> {
        self.store
            .get(TOOLS_NAMESPACE, id)
            .await?
            .map(effective_tool_spec)
            .transpose()
    }

    /// Return just the `RecordMeta` for a stored entry. Returns `None` when
    /// the record does not exist.  Does not apply redaction (meta contains no
    /// secrets) and does not apply overrides (meta is the raw provenance).
    pub async fn get_meta(
        &self,
        namespace: ConfigNamespace,
        id: &str,
    ) -> Result<Option<awaken_contract::RecordMeta>, ConfigServiceError> {
        let value = self.store.get(namespace.as_str(), id).await?;
        let Some(value) = value else {
            return Ok(None);
        };
        // For non-Agent namespaces the envelope may not have been written yet
        // (legacy bare-spec). ConfigRecord::from_value handles both shapes.
        let meta = awaken_contract::ConfigRecord::<Value>::from_value(value)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
            .meta;
        Ok(Some(meta))
    }

    pub(crate) async fn get_tool_meta(
        &self,
        id: &str,
    ) -> Result<Option<awaken_contract::RecordMeta>, ConfigServiceError> {
        let value = self.store.get(TOOLS_NAMESPACE, id).await?;
        let Some(value) = value else {
            return Ok(None);
        };
        let meta = awaken_contract::ConfigRecord::<Value>::from_value(value)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
            .meta;
        Ok(Some(meta))
    }

    /// Return `RecordMeta` for every record in the namespace. Pairs are
    /// `(id, RecordMeta)`.
    pub async fn list_meta(
        &self,
        namespace: ConfigNamespace,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, awaken_contract::RecordMeta)>, ConfigServiceError> {
        let values = self.store.list(namespace.as_str(), offset, limit).await?;
        let mut out = Vec::with_capacity(values.len());
        for (id, value) in values {
            let meta = awaken_contract::ConfigRecord::<Value>::from_value(value)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
                .meta;
            out.push((id, meta));
        }
        Ok(out)
    }

    pub(crate) async fn list_tool_meta(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, awaken_contract::RecordMeta)>, ConfigServiceError> {
        let values = self.store.list(TOOLS_NAMESPACE, offset, limit).await?;
        let mut out = Vec::with_capacity(values.len());
        for (id, value) in values {
            let meta = awaken_contract::ConfigRecord::<Value>::from_value(value)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
                .meta;
            out.push((id, meta));
        }
        Ok(out)
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
    ) -> Result<Value, ConfigServiceError> {
        self.create_with_headers(namespace, body, &HeaderMap::new())
            .await
    }

    pub async fn create_with_headers(
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
            .persist_and_apply_locked(
                manager.as_ref(),
                namespace,
                &id,
                None,
                body.clone(),
                headers,
            )
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
    ) -> Result<Value, ConfigServiceError> {
        self.update_with_headers(namespace, id, body, &HeaderMap::new())
            .await
    }

    pub async fn update_with_headers(
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
                headers,
            )
            .await?;

        self.emit_audit(
            AuditAction::Update,
            namespace,
            id,
            previous.map(unwrap_spec),
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
    ) -> Result<(), ConfigServiceError> {
        self.delete_with_options(namespace, id, false, &HeaderMap::new())
            .await
    }

    pub async fn delete_with_options(
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

        let provider_force = force && matches!(namespace, ConfigNamespace::Providers);
        if !provider_force {
            let blockers = self.find_dependents(namespace, id).await?;
            if !blockers.is_empty() {
                return Err(blocked_by_dependents(blockers));
            }
        }

        let cascade_model_ids = if provider_force {
            let provider_models = self.find_dependents(ConfigNamespace::Providers, id).await?;
            let model_ids = provider_models
                .into_iter()
                .map(|model_ref| model_ref.id)
                .collect::<Vec<_>>();
            let agent_blockers = self
                .agents_referencing_models(&model_ids)
                .await?
                .into_iter()
                .map(|agent_id| DependentRef {
                    namespace: "agents",
                    id: agent_id,
                })
                .collect::<Vec<_>>();
            if !agent_blockers.is_empty() {
                return Err(blocked_by_dependents(agent_blockers));
            }
            model_ids
        } else {
            Vec::new()
        };

        let mut records_to_delete: Vec<(ConfigNamespace, String, Value, u64)> = Vec::new();
        for model_id in cascade_model_ids {
            let raw = self
                .store
                .get(ConfigNamespace::Models.as_str(), &model_id)
                .await?
                .ok_or_else(|| ConfigServiceError::NotFound(format!("models/{model_id}")))?;
            let revision = ConfigRecord::<Value>::from_value(raw.clone())
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
                .meta
                .revision;
            records_to_delete.push((ConfigNamespace::Models, model_id, raw, revision));
        }

        let expected_revision = ConfigRecord::<Value>::from_value(previous.clone())
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
            .meta
            .revision;
        records_to_delete.push((
            namespace,
            id.to_string(),
            previous.clone(),
            expected_revision,
        ));

        let mut deleted_records: Vec<(ConfigNamespace, String, Value, u64)> = Vec::new();
        for (delete_namespace, delete_id, raw, revision) in records_to_delete {
            if let Err(error) = self
                .cas_delete_record(delete_namespace, &delete_id, revision)
                .await
            {
                self.rollback_deleted_records(deleted_records).await?;
                return Err(error);
            }
            deleted_records.push((delete_namespace, delete_id, raw, revision));
        }

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed(
                namespace,
                id,
                "",
                Some(unwrap_spec(previous.clone())),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_deleted_records(deleted_records).await?;
            return Err(error);
        }

        for (deleted_namespace, deleted_id, raw, _) in deleted_records {
            self.emit_audit(
                AuditAction::Delete,
                deleted_namespace,
                &deleted_id,
                Some(unwrap_spec(raw)),
                None,
                headers,
            )
            .await;
        }

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

        // Verify cross-resource guard. Override events for this same record carry
        // a `/overrides[/{field}]` suffix; treat them as in-scope.
        let expected_resource = format!("{}/{}", namespace.as_str(), id);
        let expected_prefix = format!("{expected_resource}/");
        if event.resource != expected_resource && !event.resource.starts_with(&expected_prefix) {
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
            A::Restart | A::SeedApply | A::ApplyFailed => return Err(RestoreError::NotRestorable),
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
            self.persist_and_apply_locked(
                manager.as_ref(),
                namespace,
                id,
                before.clone(),
                prepared,
                headers,
            )
            .await
            .map_err(RestoreError::Service)?
        } else {
            // Resource does not exist — restore from a deleted state.
            let (body_id, prepared) = self
                .prepare_body(namespace, None, payload.clone())
                .await
                .map_err(RestoreError::Service)?;
            if body_id != id {
                return Err(RestoreError::Service(ConfigServiceError::InvalidPayload(
                    format!("restored payload id '{body_id}' does not match URL id '{id}'"),
                )));
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
                headers,
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
                    before.map(unwrap_spec),
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
    pub(crate) async fn find_dependents(
        &self,
        namespace: ConfigNamespace,
        id: &str,
    ) -> Result<Vec<DependentRef>, ConfigServiceError> {
        match namespace {
            ConfigNamespace::Providers => {
                let models = self.store.list("models", 0, usize::MAX).await?;
                let mut refs = Vec::new();
                for (model_id, value) in models {
                    let Some(model) = effective_visible_record::<ModelBindingSpec>(value)? else {
                        continue;
                    };
                    if model.provider_id == id {
                        refs.push(DependentRef {
                            namespace: "models",
                            id: model_id,
                        });
                    }
                }
                Ok(refs)
            }
            ConfigNamespace::Models => {
                let agents = self.store.list("agents", 0, usize::MAX).await?;
                let mut refs = Vec::new();
                for (agent_id, value) in agents {
                    let Some(agent) = effective_visible_record::<AgentSpec>(value)? else {
                        continue;
                    };
                    if agent.endpoint.is_none() && agent.model_id == id {
                        refs.push(DependentRef {
                            namespace: "agents",
                            id: agent_id,
                        });
                    }
                }
                Ok(refs)
            }
            ConfigNamespace::Agents | ConfigNamespace::McpServers => Ok(vec![]),
        }
    }
}

pub(super) fn map_runtime_error(error: ConfigRuntimeError) -> ConfigServiceError {
    match error {
        ConfigRuntimeError::UnsupportedProviderAdapter(_)
        | ConfigRuntimeError::InvalidConfig(_) => {
            ConfigServiceError::InvalidPayload(error.to_string())
        }
        ConfigRuntimeError::RuntimeNotConfigurable
        | ConfigRuntimeError::PartialBootstrap
        | ConfigRuntimeError::PeriodicRefresh(_)
        | ConfigRuntimeError::ChangeListener(_) => ConfigServiceError::Apply(error.to_string()),
        ConfigRuntimeError::Storage(error) => ConfigServiceError::Storage(error),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use awaken_contract::contract::config_store::ConfigStore;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::{AgentSpec, BuiltinSeedSet, BuiltinSpec, ModelBindingSpec, ProviderSpec};
    use awaken_runtime::builder::AgentRuntimeBuilder;
    use awaken_runtime::registry::traits::ModelBinding;
    use serde_json::{Value, json};
    use tokio::sync::Notify;

    use crate::app::{AppState, ServerConfig};
    use crate::mailbox::{Mailbox, MailboxConfig};
    use crate::services::config_runtime::{ConfigRuntimeManager, ProviderExecutorFactory};

    use super::{
        ConfigNamespace, ConfigService, ConfigServiceError, TOOLS_NAMESPACE, tool_schema_json,
    };

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

    struct FailingModelDeleteConfigStore {
        inner: Arc<awaken_stores::InMemoryStore>,
        fail_model_delete_call: usize,
        model_delete_calls: AtomicUsize,
    }

    impl FailingModelDeleteConfigStore {
        fn new(inner: Arc<awaken_stores::InMemoryStore>, fail_model_delete_call: usize) -> Self {
            Self {
                inner,
                fail_model_delete_call,
                model_delete_calls: AtomicUsize::new(0),
            }
        }
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

    #[async_trait]
    impl ConfigStore for FailingModelDeleteConfigStore {
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

        async fn put_if_absent(
            &self,
            namespace: &str,
            id: &str,
            value: &Value,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            ConfigStore::put_if_absent(self.inner.as_ref(), namespace, id, value).await
        }

        async fn put_if_revision(
            &self,
            namespace: &str,
            id: &str,
            value: &Value,
            expected_revision: u64,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            ConfigStore::put_if_revision(
                self.inner.as_ref(),
                namespace,
                id,
                value,
                expected_revision,
            )
            .await
        }

        async fn delete_if_revision(
            &self,
            namespace: &str,
            id: &str,
            expected_revision: u64,
        ) -> Result<(), awaken_contract::contract::storage::StorageError> {
            if namespace == ConfigNamespace::Models.as_str() {
                let call = self.model_delete_calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call == self.fail_model_delete_call {
                    return Err(awaken_contract::contract::storage::StorageError::Io(
                        format!("forced model delete failure for {id}"),
                    ));
                }
            }
            ConfigStore::delete_if_revision(self.inner.as_ref(), namespace, id, expected_revision)
                .await
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
        let seed = BuiltinSeedSet {
            binary_version: "test".to_string(),
            specs: vec![
                BuiltinSpec::provider(ProviderSpec {
                    id: "bootstrap".into(),
                    adapter: "stub".into(),
                    ..Default::default()
                }),
                BuiltinSpec::model(ModelBindingSpec {
                    id: "bootstrap".into(),
                    provider_id: "bootstrap".into(),
                    upstream_model: "bootstrap-model".into(),
                }),
                BuiltinSpec::agent(bootstrap_agent()),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply_seed");
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
                    .create_with_headers(
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
        // The stored value is now a ConfigRecord envelope; extract id from spec layer.
        assert_eq!(
            stored
                .as_ref()
                .and_then(|value| {
                    // Prefer spec layer for envelope, fall back to bare spec.
                    value.get("spec").or(Some(value))
                })
                .and_then(|spec| spec.get("id"))
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
            .create_with_headers(
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
            .create_with_headers(
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
            .create_with_headers(
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
    async fn find_dependents_model_uses_effective_agent_model_override() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "prov-b", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider-b");
        service
            .create_with_headers(
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

        let raw = ConfigStore::get(config_store.as_ref(), "agents", "bootstrap")
            .await
            .expect("read bootstrap agent")
            .expect("bootstrap agent exists");
        let mut record = awaken_contract::ConfigRecord::<AgentSpec>::from_value(raw)
            .expect("parse bootstrap agent record");
        record.meta.user_overrides = Some(json!({ "model_id": "model-b" }));
        ConfigStore::put(
            config_store.as_ref(),
            "agents",
            "bootstrap",
            &record.to_value().expect("serialize bootstrap override"),
        )
        .await
        .expect("write bootstrap override");

        let effective_deps = service
            .find_dependents(ConfigNamespace::Models, "model-b")
            .await
            .expect("find effective model dependents");
        assert!(effective_deps.iter().any(|dep| dep.id == "bootstrap"));

        let base_deps = service
            .find_dependents(ConfigNamespace::Models, "bootstrap")
            .await
            .expect("find base model dependents");
        assert!(!base_deps.iter().any(|dep| dep.id == "bootstrap"));

        let preview = service
            .preview_remove_provider("prov-b")
            .await
            .expect("preview provider removal");
        assert_eq!(preview.model_ids, vec!["model-b"]);
        assert_eq!(preview.agent_ids, vec!["bootstrap"]);
    }

    #[tokio::test]
    async fn find_dependents_model_ignores_effective_remote_endpoint_agents() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        let raw = ConfigStore::get(config_store.as_ref(), "agents", "bootstrap")
            .await
            .expect("read bootstrap agent")
            .expect("bootstrap agent exists");
        let mut record = awaken_contract::ConfigRecord::<AgentSpec>::from_value(raw)
            .expect("parse bootstrap agent record");
        record.meta.user_overrides = Some(json!({
            "endpoint": {
                "base_url": "http://remote-agent.example/"
            }
        }));
        ConfigStore::put(
            config_store.as_ref(),
            "agents",
            "bootstrap",
            &record.to_value().expect("serialize endpoint override"),
        )
        .await
        .expect("write endpoint override");

        let dependents = service
            .find_dependents(ConfigNamespace::Models, "bootstrap")
            .await
            .expect("find model dependents");
        assert!(!dependents.iter().any(|dep| dep.id == "bootstrap"));
    }

    #[tokio::test]
    async fn provider_removal_preview_ignores_effective_remote_endpoint_agents() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "prov-remote", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider");
        service
            .create_with_headers(
                ConfigNamespace::Models,
                json!({
                    "id": "model-remote",
                    "provider_id": "prov-remote",
                    "upstream_model": "gpt-4"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create model");
        service
            .create_with_headers(
                ConfigNamespace::Agents,
                json!({
                    "id": "agent-remote",
                    "model_id": "model-remote",
                    "system_prompt": "remote",
                    "max_rounds": 1,
                    "endpoint": {
                        "base_url": "http://remote-agent.example/"
                    }
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create remote endpoint agent");

        let preview = service
            .preview_remove_provider("prov-remote")
            .await
            .expect("preview provider removal");
        assert_eq!(preview.model_ids, vec!["model-remote"]);
        assert!(
            preview.agent_ids.is_empty(),
            "remote endpoint agents must not block provider model cascade"
        );
    }

    #[tokio::test]
    async fn provider_removal_preview_collects_dependents_across_multiple_models() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        for provider_id in ["prov-fanout", "prov-other"] {
            service
                .create_with_headers(
                    ConfigNamespace::Providers,
                    json!({ "id": provider_id, "adapter": "stub" }),
                    &axum::http::HeaderMap::new(),
                )
                .await
                .expect("create provider");
        }
        for (model_id, provider_id) in [
            ("fanout-a", "prov-fanout"),
            ("fanout-b", "prov-fanout"),
            ("fanout-c", "prov-fanout"),
            ("other-a", "prov-other"),
        ] {
            service
                .create_with_headers(
                    ConfigNamespace::Models,
                    json!({
                        "id": model_id,
                        "provider_id": provider_id,
                        "upstream_model": "gpt-4"
                    }),
                    &axum::http::HeaderMap::new(),
                )
                .await
                .expect("create model");
        }
        for (agent_id, model_id) in [
            ("agent-uses-a", "fanout-a"),
            ("agent-uses-b", "fanout-b"),
            ("agent-uses-c-1", "fanout-c"),
            ("agent-uses-c-2", "fanout-c"),
            ("agent-uses-other", "other-a"),
        ] {
            service
                .create_with_headers(
                    ConfigNamespace::Agents,
                    json!({
                        "id": agent_id,
                        "model_id": model_id,
                        "system_prompt": "fanout",
                        "max_rounds": 1
                    }),
                    &axum::http::HeaderMap::new(),
                )
                .await
                .expect("create agent");
        }

        let preview = service
            .preview_remove_provider("prov-fanout")
            .await
            .expect("preview provider removal");
        assert_eq!(
            preview.model_ids,
            vec!["fanout-a".to_string(), "fanout-b".into(), "fanout-c".into()]
        );
        assert_eq!(
            preview.agent_ids,
            vec![
                "agent-uses-a".to_string(),
                "agent-uses-b".into(),
                "agent-uses-c-1".into(),
                "agent-uses-c-2".into(),
            ],
            "preview must collect dependents across all provider models in a single pass"
        );
        assert!(!preview.block_if_referenced_allowed);
        assert!(!preview.cascade_unused_model_bindings_allowed);
    }

    #[tokio::test]
    async fn test_provider_redacts_provider_secrets_from_error() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        let secret = "sk-provider-test-secret-redaction";
        let mut headers = serde_json::Map::new();
        headers.insert(format!("{secret} invalid"), json!("header-value"));
        let mut adapter_options = std::collections::BTreeMap::new();
        adapter_options.insert("headers".to_string(), Value::Object(headers));
        let record = awaken_contract::ConfigRecord {
            spec: ProviderSpec {
                id: "leaky-provider".into(),
                adapter: "openai".into(),
                api_key: Some(secret.to_string().into()),
                adapter_options,
                ..Default::default()
            },
            meta: awaken_contract::RecordMeta::new_user(),
        };
        ConfigStore::put(
            config_store.as_ref(),
            "providers",
            "leaky-provider",
            &record.to_value().expect("serialize provider"),
        )
        .await
        .expect("write provider");

        let result = service
            .test_provider("leaky-provider")
            .await
            .expect("test provider");

        assert!(!result.ok);
        let error = result.error.expect("provider test error");
        assert!(
            !error.contains(secret),
            "provider preflight error leaked secret: {error}"
        );
        assert!(
            error.contains("***"),
            "provider preflight error should include a redaction marker: {error}"
        );
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
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "prov-b", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider-b");

        service
            .create_with_headers(
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
            .delete_with_options(
                ConfigNamespace::Providers,
                "prov-b",
                false,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("should be blocked");

        assert!(
            matches!(err, ConfigServiceError::Conflict(ref message) if message.contains("model-b")),
            "expected dependency conflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_with_force_cascades_unused_provider_models() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "prov-c", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider-c");

        service
            .create_with_headers(
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

        service
            .delete_with_options(
                ConfigNamespace::Providers,
                "prov-c",
                true,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("force delete should succeed");

        assert!(
            config_store
                .get(ConfigNamespace::Models.as_str(), "model-c")
                .await
                .unwrap()
                .is_none(),
            "provider force delete must remove model bindings that point to it"
        );
    }

    #[tokio::test]
    async fn delete_with_force_rolls_back_cascade_when_model_delete_fails() {
        let raw_store = Arc::new(awaken_stores::InMemoryStore::new());
        let failing_store = Arc::new(FailingModelDeleteConfigStore::new(raw_store.clone(), 2));
        let config_store = failing_store.clone() as Arc<dyn ConfigStore>;
        let (state, _manager) = build_state(config_store).await;
        let service = ConfigService::new(&state).expect("config service");

        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "prov-e", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider-e");

        for model_id in ["model-e-a", "model-e-b"] {
            service
                .create_with_headers(
                    ConfigNamespace::Models,
                    json!({
                        "id": model_id,
                        "provider_id": "prov-e",
                        "upstream_model": "gpt-4"
                    }),
                    &axum::http::HeaderMap::new(),
                )
                .await
                .expect("create provider model");
        }

        let err = service
            .delete_with_options(
                ConfigNamespace::Providers,
                "prov-e",
                true,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("forced model delete failure must reject the delete");
        assert!(err.to_string().contains("forced model delete failure"));

        for (namespace, id) in [
            (ConfigNamespace::Providers.as_str(), "prov-e"),
            (ConfigNamespace::Models.as_str(), "model-e-a"),
            (ConfigNamespace::Models.as_str(), "model-e-b"),
        ] {
            assert!(
                ConfigStore::get(raw_store.as_ref(), namespace, id)
                    .await
                    .expect("read after rollback")
                    .is_some(),
                "{namespace}/{id} should be restored after cascade failure"
            );
        }
    }

    #[tokio::test]
    async fn delete_provider_with_force_blocks_when_agents_use_provider_models() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "prov-d", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create provider-d");

        service
            .create_with_headers(
                ConfigNamespace::Models,
                json!({
                    "id": "model-d",
                    "provider_id": "prov-d",
                    "upstream_model": "gpt-4"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create model-d");

        service
            .create_with_headers(
                ConfigNamespace::Agents,
                json!({
                    "id": "agent-d",
                    "model_id": "model-d",
                    "system_prompt": "test"
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create agent-d");

        let err = service
            .delete_with_options(
                ConfigNamespace::Providers,
                "prov-d",
                true,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("force delete must not orphan agent model references");

        assert!(
            matches!(err, ConfigServiceError::Conflict(ref message) if message.contains("agent-d")),
            "expected agent dependency blocker, got {err:?}"
        );
    }

    struct FailingProviderFactory;

    impl ProviderExecutorFactory for FailingProviderFactory {
        fn build(
            &self,
            _spec: &ProviderSpec,
        ) -> Result<Arc<dyn LlmExecutor>, crate::services::config_runtime::ConfigRuntimeError>
        {
            Err(
                crate::services::config_runtime::ConfigRuntimeError::InvalidConfig(
                    "forced failure for rollback test".into(),
                ),
            )
        }
    }

    #[tokio::test]
    async fn delete_rollback_re_emits_envelope() {
        // Step 1: build a manager with the succeeding TestProviderFactory and PUT a provider.
        let config_store: Arc<dyn awaken_contract::contract::config_store::ConfigStore> =
            Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("config service");

        service
            .create_with_headers(
                ConfigNamespace::Providers,
                json!({ "id": "rollback-prov", "adapter": "stub" }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create rollback-prov");

        // Step 2: verify the stored record is already an envelope (precondition).
        let stored_before = ConfigStore::get(config_store.as_ref(), "providers", "rollback-prov")
            .await
            .expect("read before delete")
            .expect("provider must exist");
        assert!(
            stored_before.get("spec").is_some(),
            "stored record must be envelope-shaped before delete (has 'spec' key)"
        );

        // Step 3: build a second manager over the same store, with FailingProviderFactory.
        let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
        let runtime_failing = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider("bootstrap", Arc::new(ImmediateExecutor))
                .with_thread_run_store(thread_store.clone())
                .build()
                .expect("build runtime"),
        );
        let manager_failing = Arc::new(
            crate::services::config_runtime::ConfigRuntimeManager::new(
                runtime_failing.clone(),
                config_store.clone(),
            )
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(FailingProviderFactory)),
        );

        let mailbox_failing = Arc::new(crate::mailbox::Mailbox::new(
            runtime_failing.clone(),
            Arc::new(awaken_stores::InMemoryMailboxStore::new()),
            thread_store.clone(),
            "rollback-test".into(),
            crate::mailbox::MailboxConfig::default(),
        ));
        let state_failing = crate::app::AppState::new(
            runtime_failing.clone(),
            mailbox_failing,
            thread_store,
            runtime_failing.resolver_arc(),
            crate::app::ServerConfig::default(),
        )
        .with_config_store(config_store.clone())
        .with_config_runtime_manager(manager_failing);

        // Step 4: attempt DELETE via the failing service — apply_locked will fail.
        let service_failing = ConfigService::new(&state_failing).expect("failing config service");
        let delete_result = service_failing
            .delete_with_options(
                ConfigNamespace::Providers,
                "rollback-prov",
                true,
                &axum::http::HeaderMap::new(),
            )
            .await;

        assert!(
            delete_result.is_err(),
            "delete must fail when apply_locked fails"
        );

        // Step 5: assert the store still has the provider AND it is envelope-shaped.
        let stored_after = ConfigStore::get(config_store.as_ref(), "providers", "rollback-prov")
            .await
            .expect("read after delete")
            .expect("provider must have been rolled back");

        assert!(
            stored_after.get("spec").is_some(),
            "rolled-back record must be envelope-shaped (has 'spec' key)"
        );
        assert!(
            stored_after.get("meta").is_some(),
            "rolled-back record must be envelope-shaped (has 'meta' key)"
        );
        assert_eq!(
            stored_after["spec"]["id"],
            Value::String("rollback-prov".into()),
            "rolled-back spec must preserve the original provider id"
        );
    }

    // ── ConfigNamespace::all() / iter_str() tests ─────────────────────────

    #[test]
    fn namespace_all_lists_every_variant() {
        let all = ConfigNamespace::all();
        assert_eq!(all.len(), 4, "exactly four 0.4-compatible namespaces");

        // Each variant must appear exactly once.
        let has = |v: ConfigNamespace| all.iter().filter(|&&x| x == v).count();
        assert_eq!(has(ConfigNamespace::Agents), 1);
        assert_eq!(has(ConfigNamespace::Providers), 1);
        assert_eq!(has(ConfigNamespace::Models), 1);
        assert_eq!(has(ConfigNamespace::McpServers), 1);
    }

    #[test]
    fn namespace_all_matches_builtin_spec_namespace() {
        use awaken_contract::{BuiltinSpec, McpServerSpec};

        for &ns in ConfigNamespace::all() {
            let spec = match ns {
                ConfigNamespace::Agents => BuiltinSpec::Agent(Box::new(AgentSpec {
                    id: "x".into(),
                    model_id: "m".into(),
                    system_prompt: "s".into(),
                    ..Default::default()
                })),
                ConfigNamespace::Providers => BuiltinSpec::Provider(ProviderSpec {
                    id: "x".into(),
                    adapter: "openai".into(),
                    ..Default::default()
                }),
                ConfigNamespace::Models => BuiltinSpec::Model(ModelBindingSpec {
                    id: "x".into(),
                    provider_id: "p".into(),
                    upstream_model: "m".into(),
                }),
                ConfigNamespace::McpServers => BuiltinSpec::McpServer(McpServerSpec {
                    id: "x".into(),
                    ..Default::default()
                }),
            };
            assert_eq!(
                spec.namespace(),
                ns.as_str(),
                "BuiltinSpec::namespace() drifted from ConfigNamespace::as_str() for {ns:?}"
            );
        }
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
                .create_with_headers(
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
                .create_with_headers(
                    ConfigNamespace::Agents,
                    json!({ "id": "upd-agent", "model_id": "bootstrap", "system_prompt": "v1", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create");

            service
                .update_with_headers(
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
                .create_with_headers(
                    ConfigNamespace::Agents,
                    json!({ "id": "del-agent", "model_id": "bootstrap", "system_prompt": "hi", "max_rounds": 1 }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create");

            service
                .delete_with_options(
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
        async fn provider_force_delete_emits_audit_for_cascaded_model_delete() {
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let (state, _manager) = build_state(config_store.clone()).await;
            let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
            let state = state.with_audit_log(audit_logger.clone());

            let service = ConfigService::new(&state).expect("service");
            service
                .create_with_headers(
                    ConfigNamespace::Providers,
                    json!({ "id": "audit-cascade-prov", "adapter": "stub" }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create provider");
            service
                .create_with_headers(
                    ConfigNamespace::Models,
                    json!({
                        "id": "audit-cascade-model",
                        "provider_id": "audit-cascade-prov",
                        "upstream_model": "gpt-4"
                    }),
                    &HeaderMap::new(),
                )
                .await
                .expect("create model");

            service
                .delete_with_options(
                    ConfigNamespace::Providers,
                    "audit-cascade-prov",
                    true,
                    &HeaderMap::new(),
                )
                .await
                .expect("force delete provider");

            let page = audit_logger
                .query(AuditQuery {
                    action: Some(AuditAction::Delete),
                    ..Default::default()
                })
                .await
                .unwrap();
            let mut resources = page
                .items
                .iter()
                .map(|event| event.resource.as_str())
                .collect::<Vec<_>>();
            resources.sort_unstable();
            assert_eq!(
                resources,
                vec!["models/audit-cascade-model", "providers/audit-cascade-prov"]
            );
            for event in page.items {
                assert!(
                    event.before.is_some(),
                    "delete audit for {} must include before payload",
                    event.resource
                );
                assert!(
                    event.after.is_none(),
                    "delete audit for {} must omit after payload",
                    event.resource
                );
            }
        }

        #[tokio::test]
        async fn config_write_succeeds_even_when_audit_store_separate_and_no_logger() {
            // Verify that without an audit logger, create still succeeds.
            let config_store = Arc::new(awaken_stores::InMemoryStore::new());
            let (state, _manager) = build_state(config_store.clone()).await;
            // No audit_log attached.

            let service = ConfigService::new(&state).expect("service");
            service
                .create_with_headers(
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

    // ── ConfigRecord envelope tests ─────────────────────────────────────────

    #[tokio::test]
    async fn put_emits_envelope_with_user_meta() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("service");

        service
            .create_with_headers(
                ConfigNamespace::Agents,
                json!({
                    "id": "env-agent",
                    "model_id": "bootstrap",
                    "system_prompt": "test",
                    "max_rounds": 1
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create agent");

        let raw = awaken_contract::contract::config_store::ConfigStore::get(
            config_store.as_ref(),
            "agents",
            "env-agent",
        )
        .await
        .expect("store read")
        .expect("entry present");

        let obj = raw.as_object().expect("must be JSON object");
        assert!(
            obj.contains_key("spec"),
            "stored value must have 'spec' key"
        );
        assert!(
            obj.contains_key("meta"),
            "stored value must have 'meta' key"
        );

        let meta = &raw["meta"];
        assert_eq!(
            meta["source"]["kind"].as_str(),
            Some("user"),
            "source.kind must be 'user'"
        );
        assert_ne!(
            meta["created_at"].as_u64(),
            Some(0),
            "created_at must be non-zero"
        );
    }

    #[tokio::test]
    async fn put_existing_envelope_preserves_created_at() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("service");

        service
            .create_with_headers(
                ConfigNamespace::Agents,
                json!({
                    "id": "ts-agent",
                    "model_id": "bootstrap",
                    "system_prompt": "v1",
                    "max_rounds": 1
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create agent");

        // Read back created_at from envelope
        let first = awaken_contract::contract::config_store::ConfigStore::get(
            config_store.as_ref(),
            "agents",
            "ts-agent",
        )
        .await
        .expect("read")
        .expect("present");
        let created_at_1 = first["meta"]["created_at"].as_u64().expect("created_at");

        // Sleep briefly so updated_at will differ
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        service
            .update_with_headers(
                ConfigNamespace::Agents,
                "ts-agent",
                json!({
                    "id": "ts-agent",
                    "model_id": "bootstrap",
                    "system_prompt": "v2",
                    "max_rounds": 1
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("update agent");

        let second = awaken_contract::contract::config_store::ConfigStore::get(
            config_store.as_ref(),
            "agents",
            "ts-agent",
        )
        .await
        .expect("read")
        .expect("present");

        let created_at_2 = second["meta"]["created_at"]
            .as_u64()
            .expect("created_at after update");
        let updated_at_2 = second["meta"]["updated_at"]
            .as_u64()
            .expect("updated_at after update");

        assert_eq!(
            created_at_1, created_at_2,
            "created_at must be preserved across updates"
        );
        assert!(
            updated_at_2 >= created_at_2,
            "updated_at must be >= created_at after update"
        );
    }

    #[tokio::test]
    async fn audit_payload_is_bare_spec_not_envelope() {
        use crate::services::audit_log::{AuditLogger, AuditQuery};

        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let audit_logger = Arc::new(AuditLogger::new(config_store.clone()));
        let state = state.with_audit_log(audit_logger.clone());

        let service = ConfigService::new(&state).expect("service");
        service
            .create_with_headers(
                ConfigNamespace::Agents,
                json!({
                    "id": "audit-env-agent",
                    "model_id": "bootstrap",
                    "system_prompt": "audit test",
                    "max_rounds": 1
                }),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("create");

        let page = audit_logger
            .query(AuditQuery::default())
            .await
            .expect("query");
        assert_eq!(page.items.len(), 1);

        let after = page.items[0].after.as_ref().expect("after must be present");
        let after_obj = after.as_object().expect("after must be JSON object");
        assert!(
            !after_obj.contains_key("meta"),
            "audit 'after' must not contain 'meta' key (must be bare spec)"
        );
        assert!(
            !after_obj.contains_key("spec"),
            "audit 'after' must not contain 'spec' wrapper key (must be bare spec)"
        );
        assert!(
            after_obj.contains_key("id"),
            "audit 'after' must contain spec field 'id'"
        );
    }

    #[test]
    fn config_namespace_rejects_tools_to_keep_public_enum_compatible() {
        assert!(ConfigNamespace::parse("tools").is_err());
    }

    #[test]
    fn config_namespace_all_excludes_tools_to_keep_public_enum_compatible() {
        assert_eq!(ConfigNamespace::ALL.len(), 4);
    }

    #[test]
    fn config_namespace_schema_for_tools_is_object() {
        let schema = tool_schema_json().expect("schema");
        // schemars 0.8: top-level object schema shape
        assert!(schema.get("$defs").is_some() || schema.get("type").is_some());
    }

    // ── patch_tool_overrides helpers ──────────────────────────────────────────

    use crate::services::audit_log::{AuditLogger, AuditQuery};
    use awaken_contract::ToolSpec;
    use awaken_contract::contract::audit_log::AuditEvent;
    use awaken_contract::contract::tool::{
        Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult,
    };

    struct StubTool {
        id: String,
        desc: String,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor::new(self.id.clone(), self.id.clone(), self.desc.clone())
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCallContext,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolResult::success(&self.id, serde_json::json!({})).into())
        }
    }

    async fn build_test_service_with_tool(
        id: &str,
        description: &str,
    ) -> (ConfigService<'static>, Arc<AuditLogger>) {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec, RecordMeta};

        let config_store: Arc<dyn awaken_contract::contract::config_store::ConfigStore> =
            Arc::new(awaken_stores::InMemoryStore::new());
        let audit_store: Arc<dyn awaken_contract::contract::config_store::ConfigStore> =
            Arc::new(awaken_stores::InMemoryStore::new());
        let audit_logger = Arc::new(AuditLogger::new(audit_store));

        let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider("bootstrap", Arc::new(ImmediateExecutor))
                .with_thread_run_store(thread_store.clone())
                .with_tool(
                    id,
                    Arc::new(StubTool {
                        id: id.to_string(),
                        desc: description.to_string(),
                    }),
                )
                .build()
                .expect("build runtime"),
        );

        let manager = Arc::new(
            crate::services::config_runtime::ConfigRuntimeManager::new(
                runtime.clone(),
                config_store.clone(),
            )
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(TestProviderFactory)),
        );
        let resolver = runtime.resolver_arc();
        let seed = BuiltinSeedSet {
            binary_version: "test".to_string(),
            specs: vec![
                BuiltinSpec::provider(ProviderSpec {
                    id: "bootstrap".into(),
                    adapter: "stub".into(),
                    ..Default::default()
                }),
                BuiltinSpec::model(awaken_contract::ModelBindingSpec {
                    id: "bootstrap".into(),
                    provider_id: "bootstrap".into(),
                    upstream_model: "bootstrap-model".into(),
                }),
                BuiltinSpec::agent(bootstrap_agent()),
            ],
        };
        manager.apply_seed(&seed).await.expect("apply_seed");
        manager.apply().await.expect("publish config");

        // Write a Builtin ConfigRecord for the tool directly into the store.
        let tool_spec = ToolSpec {
            id: id.to_string(),
            name: id.to_string(),
            description: description.to_string(),
            ..Default::default()
        };
        let mut meta = RecordMeta::new_builtin("test");
        meta.user_overrides = None;
        meta.revision = 1;
        let record = awaken_contract::ConfigRecord {
            spec: tool_spec,
            meta,
        };
        let envelope = record.to_value().expect("serialize tool record");
        awaken_contract::contract::config_store::ConfigStore::put_if_absent(
            config_store.as_ref(),
            "tools",
            id,
            &envelope,
        )
        .await
        .expect("put tool record");

        let mailbox = Arc::new(crate::mailbox::Mailbox::new(
            runtime.clone(),
            Arc::new(awaken_stores::InMemoryMailboxStore::new()),
            thread_store.clone(),
            "tool-override-test".into(),
            crate::mailbox::MailboxConfig::default(),
        ));
        let state = AppState::new(
            runtime,
            mailbox,
            thread_store,
            resolver,
            crate::app::ServerConfig::default(),
        )
        .with_config_store(config_store)
        .with_config_runtime_manager(manager)
        .with_audit_log(audit_logger.clone());

        // SAFETY: state is owned for the duration of the test; the 'static bound is
        // satisfied by leaking the Box – acceptable in tests only.
        let state: &'static AppState = Box::leak(Box::new(state));
        let service = ConfigService::new(state).expect("config service");
        (service, audit_logger)
    }

    async fn recent_audit_events(audit_logger: &AuditLogger, resource: &str) -> Vec<AuditEvent> {
        let page = audit_logger
            .query(AuditQuery::default())
            .await
            .expect("audit query");
        page.items
            .into_iter()
            .filter(|e| e.resource == resource || e.resource.starts_with(&format!("{resource}/")))
            .collect()
    }

    // ── patch_tool_overrides tests ────────────────────────────────────────────

    #[tokio::test]
    async fn patch_tool_overrides_replaces_description_and_emits_audit() {
        let (service, audit_logger) =
            build_test_service_with_tool("echo", "stock description").await;
        let patch = serde_json::json!({"description": "custom override"});
        let after = service
            .patch_tool_overrides("echo", patch, &axum::http::HeaderMap::new())
            .await
            .expect("patch ok");
        assert_eq!(after["description"], "custom override");
        assert_eq!(after["id"], "echo");
        let events: Vec<AuditEvent> = recent_audit_events(&audit_logger, "tools/echo").await;
        let event = events
            .iter()
            .find(|e| e.action == awaken_contract::AuditAction::Update)
            .expect("audit event missing");
        assert_eq!(event.resource, "tools/echo/overrides");
        let before = event.before.as_ref().expect("before payload missing");
        let after_payload = event.after.as_ref().expect("after payload missing");
        assert_eq!(before["description"], "stock description");
        assert_eq!(after_payload["description"], "custom override");
    }

    #[tokio::test]
    async fn get_tools_merges_overrides_into_effective_spec() {
        // Regression for the bug where `effective_spec` only merged for
        // Agents, leaving Tools' GET endpoint returning the unpatched
        // description even though the override was persisted in meta.
        let (service, _audit_logger) =
            build_test_service_with_tool("echo", "stock description").await;
        service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"description": "patched"}),
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect("patch ok");
        let value = service
            .get_tool("echo")
            .await
            .expect("get ok")
            .expect("present");
        assert_eq!(value["description"], "patched");
    }

    #[tokio::test]
    async fn patch_tool_overrides_404_for_unknown_id() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
        let err = service
            .patch_tool_overrides(
                "nope",
                serde_json::json!({"description": "x"}),
                &Default::default(),
            )
            .await
            .expect_err("unknown id");
        assert!(matches!(err, ConfigServiceError::NotFound(_)));
    }

    #[tokio::test]
    async fn patch_tool_overrides_422_for_unknown_field() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
        let err = service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"name": "renamed"}),
                &Default::default(),
            )
            .await
            .expect_err("unknown field");
        assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn patch_tool_overrides_rejects_empty_description() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
        let err = service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"description": ""}),
                &Default::default(),
            )
            .await
            .expect_err("empty description");
        assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn patch_tool_overrides_rejects_overlong_description() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "x").await;
        let too_long = "x".repeat(4097);
        let err = service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"description": too_long}),
                &Default::default(),
            )
            .await
            .expect_err("overlong");
        assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn clear_tool_overrides_reverts_to_builtin() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
        service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"description": "custom"}),
                &Default::default(),
            )
            .await
            .unwrap();
        let after = service
            .clear_tool_overrides("echo", &Default::default())
            .await
            .unwrap();
        assert_eq!(after["description"], "stock");
    }

    #[tokio::test]
    async fn clear_tool_overrides_idempotent_when_already_empty() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
        let after = service
            .clear_tool_overrides("echo", &Default::default())
            .await
            .unwrap();
        assert_eq!(after["description"], "stock");
    }

    #[tokio::test]
    async fn clear_tool_override_field_unknown_returns_422() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
        let err = service
            .clear_tool_override_field("echo", "garbage", &Default::default())
            .await
            .expect_err("unknown field");
        assert!(matches!(err, ConfigServiceError::InvalidPayload(_)));
    }

    #[tokio::test]
    async fn clear_tool_override_field_known_clears_only_that_field() {
        let (service, _audit_logger) = build_test_service_with_tool("echo", "stock").await;
        service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"description": "custom"}),
                &Default::default(),
            )
            .await
            .unwrap();
        let after = service
            .clear_tool_override_field("echo", "description", &Default::default())
            .await
            .unwrap();
        assert_eq!(after["description"], "stock");
    }

    // ── CAS / revision tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn patch_tool_overrides_bumps_revision() {
        let (service, _audit) = build_test_service_with_tool("echo", "stock").await;

        let meta_before = service
            .get_tool_meta("echo")
            .await
            .expect("get_meta")
            .expect("present");
        assert_eq!(
            meta_before.revision, 1,
            "fresh seed must start at revision 1"
        );

        service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"description": "patched"}),
                &Default::default(),
            )
            .await
            .expect("first patch ok");

        let meta_after = service
            .get_tool_meta("echo")
            .await
            .expect("get_meta")
            .expect("present");
        assert!(
            meta_after.revision > meta_before.revision,
            "patch must bump revision: before={}, after={}",
            meta_before.revision,
            meta_after.revision,
        );
    }

    #[tokio::test]
    async fn patch_tool_overrides_conflict_on_stale_revision() {
        use awaken_contract::ConfigRecord;

        let (service, _audit) = build_test_service_with_tool("echo", "stock").await;

        let store = service.store.clone();
        let raw = awaken_contract::contract::config_store::ConfigStore::get(
            store.as_ref(),
            "tools",
            "echo",
        )
        .await
        .expect("read")
        .expect("present");

        let mut stale_record = ConfigRecord::<awaken_contract::ToolSpec>::from_value(raw.clone())
            .expect("parse record");
        let stale_expected = stale_record.meta.revision;

        let mut concurrent_record =
            ConfigRecord::<awaken_contract::ToolSpec>::from_value(raw).expect("parse current");
        concurrent_record.spec.description = "concurrent".into();
        concurrent_record.meta.revision = stale_expected + 1;
        let concurrent_envelope = concurrent_record.to_value().expect("serialize concurrent");
        awaken_contract::contract::config_store::ConfigStore::put_if_revision(
            store.as_ref(),
            "tools",
            "echo",
            &concurrent_envelope,
            stale_expected,
        )
        .await
        .expect("concurrent writer succeeds");

        stale_record.spec.description = "stale".into();
        let err = service
            .cas_put_record_in_namespace(TOOLS_NAMESPACE, "echo", &mut stale_record, stale_expected)
            .await
            .expect_err("stale write must conflict");
        assert!(matches!(err, ConfigServiceError::Conflict(_)));

        let meta_final = service
            .get_tool_meta("echo")
            .await
            .expect("get_meta final")
            .expect("present final");
        assert_eq!(
            meta_final.revision,
            stale_expected + 1,
            "stale writer must not advance the stored revision"
        );
    }

    // ── ApplyFailed audit emission tests ─────────────────────────────────────

    #[tokio::test]
    async fn patch_tool_overrides_apply_failure_emits_apply_failed_audit_event() {
        use awaken_contract::{BuiltinSeedSet, BuiltinSpec, RecordMeta};

        // Step 1: seed a config store with a builtin tool, apply successfully.
        let config_store: Arc<dyn awaken_contract::contract::config_store::ConfigStore> =
            Arc::new(awaken_stores::InMemoryStore::new());
        let audit_store: Arc<dyn awaken_contract::contract::config_store::ConfigStore> =
            Arc::new(awaken_stores::InMemoryStore::new());
        let audit_logger = Arc::new(AuditLogger::new(audit_store));

        let thread_store = Arc::new(awaken_stores::InMemoryStore::new());
        let runtime = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider("bootstrap", Arc::new(ImmediateExecutor))
                .with_thread_run_store(thread_store.clone())
                .with_tool(
                    "echo",
                    Arc::new(StubTool {
                        id: "echo".to_string(),
                        desc: "stock".to_string(),
                    }),
                )
                .build()
                .expect("build runtime"),
        );

        let manager_ok = Arc::new(
            crate::services::config_runtime::ConfigRuntimeManager::new(
                runtime.clone(),
                config_store.clone(),
            )
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(TestProviderFactory)),
        );
        let seed = BuiltinSeedSet {
            binary_version: "test".to_string(),
            specs: vec![
                BuiltinSpec::provider(ProviderSpec {
                    id: "bootstrap".into(),
                    adapter: "stub".into(),
                    ..Default::default()
                }),
                BuiltinSpec::model(awaken_contract::ModelBindingSpec {
                    id: "bootstrap".into(),
                    provider_id: "bootstrap".into(),
                    upstream_model: "bootstrap-model".into(),
                }),
                BuiltinSpec::agent(bootstrap_agent()),
            ],
        };
        manager_ok.apply_seed(&seed).await.expect("apply_seed");
        manager_ok.apply().await.expect("initial apply");

        // Write a builtin tool record directly.
        let tool_spec = ToolSpec {
            id: "echo".to_string(),
            name: "echo".to_string(),
            description: "stock".to_string(),
            ..Default::default()
        };
        let mut meta = RecordMeta::new_builtin("test");
        meta.user_overrides = None;
        meta.revision = 1;
        let record = awaken_contract::ConfigRecord {
            spec: tool_spec,
            meta,
        };
        let envelope = record.to_value().expect("serialize tool record");
        awaken_contract::contract::config_store::ConfigStore::put_if_absent(
            config_store.as_ref(),
            "tools",
            "echo",
            &envelope,
        )
        .await
        .expect("put tool record");

        // Step 2: build a second manager with FailingProviderFactory over the same store.
        let runtime_failing = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider("bootstrap", Arc::new(ImmediateExecutor))
                .with_thread_run_store(thread_store.clone())
                .build()
                .expect("build failing runtime"),
        );
        let manager_failing = Arc::new(
            crate::services::config_runtime::ConfigRuntimeManager::new(
                runtime_failing.clone(),
                config_store.clone(),
            )
            .expect("config runtime manager")
            .with_provider_factory(Arc::new(FailingProviderFactory)),
        );
        let mailbox = Arc::new(crate::mailbox::Mailbox::new(
            runtime_failing.clone(),
            Arc::new(awaken_stores::InMemoryMailboxStore::new()),
            thread_store.clone(),
            "apply-failed-test".into(),
            crate::mailbox::MailboxConfig::default(),
        ));
        let state = AppState::new(
            runtime_failing.clone(),
            mailbox,
            thread_store,
            runtime_failing.resolver_arc(),
            crate::app::ServerConfig::default(),
        )
        .with_config_store(config_store.clone())
        .with_config_runtime_manager(manager_failing)
        .with_audit_log(audit_logger.clone());

        let state: &'static AppState = Box::leak(Box::new(state));
        let service = ConfigService::new(state).expect("failing config service");

        // Step 3: attempt patch_tool_overrides — apply_locked fails.
        let result = service
            .patch_tool_overrides(
                "echo",
                serde_json::json!({"description": "patched"}),
                &axum::http::HeaderMap::new(),
            )
            .await;
        assert!(result.is_err(), "patch must fail when apply_locked fails");

        // Step 4: assert ApplyFailed event was emitted with the correct fields.
        let page = audit_logger
            .query(crate::services::audit_log::AuditQuery::default())
            .await
            .expect("audit query");
        let failed_events: Vec<_> = page
            .items
            .iter()
            .filter(|e| e.action == awaken_contract::AuditAction::ApplyFailed)
            .collect();
        assert_eq!(
            failed_events.len(),
            1,
            "exactly one ApplyFailed event must be emitted"
        );
        let ev = &failed_events[0];
        assert!(
            ev.resource.contains("tools/echo"),
            "resource must reference tools/echo, got: {}",
            ev.resource
        );
        assert!(
            ev.error.is_some(),
            "ApplyFailed event must carry an error string"
        );
        assert!(
            ev.before.is_some(),
            "ApplyFailed event must carry the before spec"
        );
    }
}
