use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use awaken_contract::AuditAction;
use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::{
    AgentSpec, AgentSpecPatch, ConfigRecord, McpServerSpec, ModelBindingSpec, ProviderSpec,
    RecordMeta, RecordSource, ToolSpec, ToolSpecPatch, now_ms,
};
use axum::http::HeaderMap;
use serde_json::{Map, Value, json};

use crate::app::AppState;
use crate::services::audit_log::AuditLogger;
use crate::services::config_envelope::{
    apply_overrides, extract_timestamps, spec_field, unwrap_spec,
};

use super::config_runtime::ConfigRuntimeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigNamespace {
    Agents,
    Models,
    Providers,
    McpServers,
    Tools,
}

impl ConfigNamespace {
    /// All five managed namespaces in a fixed order.
    pub const ALL: [Self; 5] = [
        Self::Agents,
        Self::Providers,
        Self::Models,
        Self::McpServers,
        Self::Tools,
    ];

    /// Slice over all five namespace variants.
    pub fn all() -> &'static [Self] {
        &Self::ALL
    }

    /// Iterator over the `&'static str` names of all five namespaces.
    pub fn iter_str() -> impl Iterator<Item = &'static str> + 'static {
        Self::ALL.iter().copied().map(Self::as_str)
    }

    pub fn parse(value: &str) -> Result<Self, ConfigServiceError> {
        match value {
            "agents" => Ok(Self::Agents),
            "models" => Ok(Self::Models),
            "providers" => Ok(Self::Providers),
            "mcp-servers" => Ok(Self::McpServers),
            "tools" => Ok(Self::Tools),
            _ => Err(ConfigServiceError::UnknownNamespace(value.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Models => "models",
            Self::Providers => "providers",
            Self::McpServers => "mcp-servers",
            Self::Tools => "tools",
        }
    }

    pub fn schema_json(self) -> Result<Value, ConfigServiceError> {
        let schema = match self {
            Self::Agents => schemars::schema_for!(AgentSpec),
            Self::Models => schemars::schema_for!(ModelBindingSpec),
            Self::Providers => schemars::schema_for!(ProviderSpec),
            Self::McpServers => schemars::schema_for!(McpServerSpec),
            Self::Tools => schemars::schema_for!(ToolSpec),
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
    #[error("overrides are not supported for user-source records; use PUT to update")]
    OverridesNotSupportedForUserRecord,
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
                { "namespace": "mcp-servers", "schema": ConfigNamespace::McpServers.schema_json()? },
                { "namespace": "tools", "schema": ConfigNamespace::Tools.schema_json()? }
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
        if matches!(namespace, ConfigNamespace::Tools) {
            return Err(ConfigServiceError::InvalidPayload(
                "tools namespace is read-only; use PATCH /v1/config/tools/:id/overrides".into(),
            ));
        }
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
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        if matches!(namespace, ConfigNamespace::Tools) {
            return Err(ConfigServiceError::InvalidPayload(
                "tools namespace is read-only; use PATCH /v1/config/tools/:id/overrides".into(),
            ));
        }
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
        force: bool,
        headers: &HeaderMap,
    ) -> Result<(), ConfigServiceError> {
        if matches!(namespace, ConfigNamespace::Tools) {
            return Err(ConfigServiceError::InvalidPayload(
                "tools namespace is read-only; use PATCH /v1/config/tools/:id/overrides".into(),
            ));
        }
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

        let expected_revision = ConfigRecord::<Value>::from_value(previous.clone())
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
            .meta
            .revision;
        self.cas_delete_record(namespace, id, expected_revision)
            .await?;
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
            let mut rollback = ConfigRecord::<Value>::from_value(previous.clone())
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;
            self.insert_record_absent(namespace, id, &mut rollback, expected_revision + 1)
                .await?;
            return Err(error);
        }

        self.emit_audit(
            AuditAction::Delete,
            namespace,
            id,
            Some(unwrap_spec(previous)),
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
                        spec_field(value, "provider_id")
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
                        spec_field(value, "model_id")
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
            ConfigNamespace::Agents | ConfigNamespace::McpServers | ConfigNamespace::Tools => {
                Ok(vec![])
            }
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
        self.emit_audit_with_suffix(action, namespace, id, "", before, after, headers)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_audit_with_suffix(
        &self,
        action: AuditAction,
        namespace: ConfigNamespace,
        id: &str,
        suffix: &str,
        before: Option<Value>,
        after: Option<Value>,
        headers: &HeaderMap,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let resource = if suffix.is_empty() {
            format!("{}/{}", namespace.as_str(), id)
        } else {
            format!("{}/{}/{}", namespace.as_str(), id, suffix)
        };
        audit.emit(action, &resource, before, after, headers).await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn emit_audit_apply_failed(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        suffix: &str,
        before: Option<Value>,
        after: Option<Value>,
        error_msg: String,
        headers: &HeaderMap,
    ) {
        let Some(audit) = &self.audit else {
            return;
        };
        let resource = if suffix.is_empty() {
            format!("{}/{}", namespace.as_str(), id)
        } else {
            format!("{}/{}/{}", namespace.as_str(), id, suffix)
        };
        audit
            .emit_apply_failed(&resource, before, after, error_msg, headers)
            .await;
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

    fn user_record_from_body(body: &Value) -> ConfigRecord<Value> {
        let (created_at, updated_at) = extract_timestamps(body);
        let mut meta = RecordMeta::new_user();
        if created_at != 0 {
            meta.created_at = created_at;
        }
        if updated_at != 0 {
            meta.updated_at = updated_at;
        }
        ConfigRecord {
            spec: body.clone(),
            meta,
        }
    }

    fn storage_write_error(
        namespace: ConfigNamespace,
        id: &str,
        error: StorageError,
    ) -> ConfigServiceError {
        match error {
            StorageError::AlreadyExists(_) => ConfigServiceError::Conflict(format!(
                "{}/{} already exists",
                namespace.as_str(),
                id
            )),
            StorageError::VersionConflict { expected, actual } => {
                ConfigServiceError::Conflict(format!(
                    "{}/{} was modified by another writer (expected revision {expected}, found {actual}); retry the mutation",
                    namespace.as_str(),
                    id,
                ))
            }
            other => ConfigServiceError::Storage(other),
        }
    }

    async fn insert_record_absent<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        record: &mut ConfigRecord<T>,
        revision: u64,
    ) -> Result<u64, ConfigServiceError> {
        record.meta.revision = revision;
        let envelope = record
            .to_value()
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;
        self.store
            .put_if_absent(namespace.as_str(), id, &envelope)
            .await
            .map(|()| revision)
            .map_err(|error| Self::storage_write_error(namespace, id, error))
    }

    /// Write `record` using `put_if_revision`, bumping `meta.revision` from
    /// `expected_revision`. Returns the new revision on success or
    /// `ConfigServiceError::Conflict` on CAS mismatch.
    async fn cas_put_record<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        record: &mut ConfigRecord<T>,
        expected_revision: u64,
    ) -> Result<u64, ConfigServiceError> {
        let next_revision = expected_revision.saturating_add(1);
        record.meta.revision = next_revision;
        let envelope = record
            .to_value()
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;
        self.store
            .put_if_revision(namespace.as_str(), id, &envelope, expected_revision)
            .await
            .map(|()| next_revision)
            .map_err(|error| Self::storage_write_error(namespace, id, error))
    }

    async fn cas_delete_record(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), ConfigServiceError> {
        self.store
            .delete_if_revision(namespace.as_str(), id, expected_revision)
            .await
            .map_err(|error| Self::storage_write_error(namespace, id, error))
    }

    async fn rollback_to_raw_after_revision(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        raw: Value,
        expected_revision: u64,
    ) -> Result<u64, ConfigServiceError> {
        let mut rollback = ConfigRecord::<Value>::from_value(raw)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;
        self.cas_put_record(namespace, id, &mut rollback, expected_revision)
            .await
    }

    async fn persist_and_apply_locked(
        &self,
        manager: &crate::services::config_runtime::ConfigRuntimeManager,
        namespace: ConfigNamespace,
        id: &str,
        previous: Option<Value>,
        body: Value,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        self.validate_payload(namespace, &body)?;
        let mut record = Self::user_record_from_body(&body);
        let write_revision = match previous.as_ref() {
            Some(previous) => {
                let expected_revision = ConfigRecord::<Value>::from_value(previous.clone())
                    .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
                    .meta
                    .revision;
                self.cas_put_record(namespace, id, &mut record, expected_revision)
                    .await?
            }
            None => {
                self.insert_record_absent(namespace, id, &mut record, 1)
                    .await?
            }
        };

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
                previous.as_ref().map(|p| unwrap_spec(p.clone())),
                Some(unwrap_spec(body.clone())),
                error.to_string(),
                headers,
            )
            .await;
            match previous {
                Some(previous) => {
                    self.rollback_to_raw_after_revision(namespace, id, previous, write_revision)
                        .await?;
                }
                None => {
                    self.cas_delete_record(namespace, id, write_revision)
                        .await?
                }
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
            ConfigNamespace::Agents | ConfigNamespace::Models | ConfigNamespace::Tools => {}
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
            // The stored value may be either a bare spec or a ConfigRecord envelope
            // ({"spec": {...}, "meta": {...}}); extract created_at from whichever layer
            // holds it.
            if !object.contains_key("created_at") {
                if let Ok(Some(existing)) = self.store.get(namespace.as_str(), &id).await {
                    let spec_layer = unwrap_spec(existing);
                    if let Some(existing_created_at) = spec_layer
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
        // The stored value may be either a bare spec or a ConfigRecord envelope.
        // Navigate into spec if needed before accessing fields.
        let spec_value = unwrap_spec(existing);
        let Some(existing_object) = spec_value.as_object() else {
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
            ConfigNamespace::Tools => {
                let _: ToolSpec = from_value(body)?;
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
            ConfigNamespace::Agents | ConfigNamespace::Models | ConfigNamespace::Tools => Ok(value),
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

        let spec: ProviderSpec = ConfigRecord::<ProviderSpec>::from_value(raw)
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))
            .map(|r| r.spec)?;

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
                network_tested: false,
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
                    network_tested: false,
                    error: None,
                });
            }
        };
        let mut network_tested = false;
        if matches!(
            kind,
            awaken_runtime::credentials::CredentialKind::GoogleServiceAccountJson
        ) {
            let scope = "https://www.googleapis.com/auth/cloud-platform";
            let mint_start = Instant::now();
            network_tested = true;
            let mint_result = broker.token_for(&spec.id, scope).await;
            latency_ms = latency_ms.saturating_add(mint_start.elapsed().as_millis() as u64);
            if let Err(err) = mint_result {
                return Ok(ProviderTestResult {
                    ok: false,
                    latency_ms,
                    network_tested,
                    error: Some(err.to_string()),
                });
            }
        }

        Ok(ProviderTestResult {
            ok: true,
            latency_ms,
            network_tested,
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
        // The stored value may be either a bare spec or a ConfigRecord envelope.
        let spec_value = unwrap_spec(existing);
        let Some(existing_object) = spec_value.as_object() else {
            return Ok(());
        };
        if let Some(existing_env) = existing_object.get("env") {
            body.insert("env".into(), existing_env.clone());
        }
        Ok(())
    }

    /// PATCH /v1/config/agents/:id/overrides
    ///
    /// Merges the patch body into the existing `user_overrides` of a Builtin
    /// agent record. Null-valued keys in the patch remove overrides; non-null
    /// keys overwrite. Returns the effective AgentSpec after the merge.
    pub async fn patch_agent_overrides(
        &self,
        id: &str,
        body: Value,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;

        let raw = self
            .store
            .get(ConfigNamespace::Agents.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("agents/{id}")))?;

        let mut record = ConfigRecord::<AgentSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(ConfigServiceError::OverridesNotSupportedForUserRecord);
        }

        let expected_revision = record.meta.revision;

        // Validate incoming body field names via AgentSpecPatch (deny_unknown_fields).
        // We use a separate check for null values: replace nulls with a dummy value
        // so that deny_unknown_fields can still catch unknown field names.
        let body_map = match &body {
            Value::Object(m) => m,
            _ => {
                return Err(ConfigServiceError::InvalidPayload(
                    "expected JSON object body".into(),
                ));
            }
        };
        // Null values for Option fields are valid (they mean "clear this override").
        // Pass the body as-is; deny_unknown_fields catches unknown field names.
        let _: AgentSpecPatch = serde_json::from_value(body.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        // Merge patch INTO existing user_overrides (shallow key-level merge).
        // Use the raw body Value to preserve nulls (null = clear the key).
        let mut existing_map: Map<String, Value> = record
            .meta
            .user_overrides
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        for (k, v) in body_map {
            if v.is_null() {
                existing_map.remove(k);
            } else {
                existing_map.insert(k.clone(), v.clone());
            }
        }

        // Validate the merged overrides by round-tripping through AgentSpecPatch.
        let merged_value = Value::Object(existing_map.clone());
        let _: AgentSpecPatch = serde_json::from_value(merged_value.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        let proposed_overrides: Option<Value> = if existing_map.is_empty() {
            None
        } else {
            Some(merged_value.clone())
        };

        // Short-circuit: if the proposed overrides are identical to existing ones,
        // skip the store write, apply_locked, and audit emit — it's a no-op.
        if proposed_overrides == record.meta.user_overrides {
            let effective_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            return serde_json::to_value(&effective_spec)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()));
        }

        let before_spec = apply_overrides(record.spec.clone(), record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let before = serde_json::to_value(&before_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        record.meta.user_overrides = proposed_overrides;
        record.meta.updated_at = now_ms();

        let write_revision = self
            .cas_put_record(ConfigNamespace::Agents, id, &mut record, expected_revision)
            .await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed(
                ConfigNamespace::Agents,
                id,
                "overrides",
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision(ConfigNamespace::Agents, id, raw, write_revision)
                .await?;
            return Err(error);
        }

        let after_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let after = serde_json::to_value(&after_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix(
            AuditAction::Update,
            ConfigNamespace::Agents,
            id,
            "overrides",
            Some(before),
            Some(after.clone()),
            headers,
        )
        .await;

        Ok(after)
    }

    /// DELETE /v1/config/agents/:id/overrides
    ///
    /// Clears all user overrides from a Builtin agent record. Returns the
    /// effective AgentSpec (which is now the bare base spec, no overrides).
    pub async fn clear_agent_overrides(
        &self,
        id: &str,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;

        let raw = self
            .store
            .get(ConfigNamespace::Agents.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("agents/{id}")))?;

        let mut record = ConfigRecord::<AgentSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(ConfigServiceError::OverridesNotSupportedForUserRecord);
        }

        // Short-circuit: if overrides are already None, this is a no-op — skip
        // the store write, apply_locked, and audit emit.
        if record.meta.user_overrides.is_none() {
            return serde_json::to_value(&record.spec)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()));
        }

        let expected_revision = record.meta.revision;

        let before_spec = apply_overrides(record.spec.clone(), record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let before = serde_json::to_value(&before_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        record.meta.user_overrides = None;
        record.meta.updated_at = now_ms();

        let write_revision = self
            .cas_put_record(ConfigNamespace::Agents, id, &mut record, expected_revision)
            .await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed(
                ConfigNamespace::Agents,
                id,
                "overrides",
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision(ConfigNamespace::Agents, id, raw, write_revision)
                .await?;
            return Err(error);
        }

        let after = serde_json::to_value(&record.spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix(
            AuditAction::Update,
            ConfigNamespace::Agents,
            id,
            "overrides",
            Some(before),
            Some(after.clone()),
            headers,
        )
        .await;

        Ok(after)
    }

    /// DELETE /v1/config/agents/:id/overrides/:field
    ///
    /// Removes a single field from the user overrides of a Builtin agent record.
    /// Returns 400 if `field` is not a recognized AgentSpecPatch field.
    /// Idempotent: if the field is not present in user_overrides, returns the
    /// current effective spec without writing to the store or emitting an audit event.
    pub async fn clear_agent_override_field(
        &self,
        id: &str,
        field: &str,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;

        let raw = self
            .store
            .get(ConfigNamespace::Agents.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("agents/{id}")))?;

        let mut record = ConfigRecord::<AgentSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(ConfigServiceError::OverridesNotSupportedForUserRecord);
        }

        let expected_revision = record.meta.revision;

        let before_spec = apply_overrides(record.spec.clone(), record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let before = serde_json::to_value(&before_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        // Validate that field is recognized by AgentSpecPatch before mutating.
        // Use a null probe: `AgentSpecPatch` accepts null for all Option fields, and
        // deny_unknown_fields will reject unknown field names.
        let probe = Value::Object({
            let mut m = Map::new();
            m.insert(field.to_string(), Value::Null);
            m
        });
        let _: AgentSpecPatch = serde_json::from_value(probe).map_err(|_| {
            ConfigServiceError::InvalidPayload(format!("unknown override field: {field}"))
        })?;

        // Remove the field from existing overrides.
        let mut existing_map: Map<String, Value> = record
            .meta
            .user_overrides
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        // Short-circuit: if the field is not present in overrides, this is a no-op —
        // skip the store write, apply_locked, and audit emit.
        if !existing_map.contains_key(field) {
            let effective_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            return serde_json::to_value(&effective_spec)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()));
        }

        existing_map.remove(field);

        let merged_value = Value::Object(existing_map.clone());
        record.meta.user_overrides = if existing_map.is_empty() {
            None
        } else {
            Some(merged_value)
        };
        record.meta.updated_at = now_ms();

        let write_revision = self
            .cas_put_record(ConfigNamespace::Agents, id, &mut record, expected_revision)
            .await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed(
                ConfigNamespace::Agents,
                id,
                &format!("overrides/{field}"),
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision(ConfigNamespace::Agents, id, raw, write_revision)
                .await?;
            return Err(error);
        }

        let after_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let after = serde_json::to_value(&after_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix(
            AuditAction::Update,
            ConfigNamespace::Agents,
            id,
            &format!("overrides/{field}"),
            Some(before),
            Some(after.clone()),
            headers,
        )
        .await;

        Ok(after)
    }

    /// PATCH /v1/config/tools/:id/overrides — see ADR-0029.
    pub async fn patch_tool_overrides(
        &self,
        id: &str,
        body: Value,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        const MAX_DESCRIPTION_LEN: usize = 4096;

        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;

        let raw = self
            .store
            .get(ConfigNamespace::Tools.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("tools/{id}")))?;

        let mut record = ConfigRecord::<ToolSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(ConfigServiceError::OverridesNotSupportedForUserRecord);
        }

        let expected_revision = record.meta.revision;

        let body_map = match &body {
            Value::Object(m) => m,
            _ => {
                return Err(ConfigServiceError::InvalidPayload(
                    "expected JSON object body".into(),
                ));
            }
        };

        // Schema validation: deny_unknown_fields catches bad field names.
        let _: ToolSpecPatch = serde_json::from_value(body.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        // Value validation (non-empty trim, length cap).
        if let Some(Value::String(s)) = body_map.get("description") {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(ConfigServiceError::InvalidPayload(
                    "description must be non-empty".into(),
                ));
            }
            if s.len() > MAX_DESCRIPTION_LEN {
                return Err(ConfigServiceError::InvalidPayload(format!(
                    "description exceeds {MAX_DESCRIPTION_LEN}-byte limit"
                )));
            }
        }

        // Shallow merge into existing user_overrides; null = clear.
        let mut existing_map: Map<String, Value> = record
            .meta
            .user_overrides
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (k, v) in body_map {
            if v.is_null() {
                existing_map.remove(k);
            } else {
                existing_map.insert(k.clone(), v.clone());
            }
        }

        // Re-validate the merged overrides shape.
        let merged_value = Value::Object(existing_map.clone());
        let _: ToolSpecPatch = serde_json::from_value(merged_value.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        let proposed_overrides: Option<Value> = if existing_map.is_empty() {
            None
        } else {
            Some(merged_value.clone())
        };

        if proposed_overrides == record.meta.user_overrides {
            let effective_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            return serde_json::to_value(&effective_spec)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()));
        }

        let before_spec = apply_overrides(record.spec.clone(), record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let before = serde_json::to_value(&before_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        record.meta.user_overrides = proposed_overrides;
        record.meta.updated_at = now_ms();

        let write_revision = self
            .cas_put_record(ConfigNamespace::Tools, id, &mut record, expected_revision)
            .await?;

        if let Err(error) = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error)
        {
            self.emit_audit_apply_failed(
                ConfigNamespace::Tools,
                id,
                "overrides",
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision(ConfigNamespace::Tools, id, raw, write_revision)
                .await?;
            return Err(error);
        }

        let after_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let after = serde_json::to_value(&after_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix(
            AuditAction::Update,
            ConfigNamespace::Tools,
            id,
            "overrides",
            Some(before),
            Some(after.clone()),
            headers,
        )
        .await;

        Ok(after)
    }

    /// DELETE /v1/config/tools/:id/overrides
    ///
    /// Clears all user overrides from a Builtin tool record. Returns the
    /// effective ToolSpec (which is now the bare base spec, no overrides).
    pub async fn clear_tool_overrides(
        &self,
        id: &str,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;

        let raw = self
            .store
            .get(ConfigNamespace::Tools.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("tools/{id}")))?;

        let mut record = ConfigRecord::<ToolSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(ConfigServiceError::OverridesNotSupportedForUserRecord);
        }

        // Short-circuit: if overrides are already None, this is a no-op — skip
        // the store write, apply_locked, and audit emit.
        if record.meta.user_overrides.is_none() {
            return serde_json::to_value(&record.spec)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()));
        }

        let expected_revision = record.meta.revision;

        let before_spec = apply_overrides(record.spec.clone(), record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let before = serde_json::to_value(&before_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        record.meta.user_overrides = None;
        record.meta.updated_at = now_ms();

        let write_revision = self
            .cas_put_record(ConfigNamespace::Tools, id, &mut record, expected_revision)
            .await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed(
                ConfigNamespace::Tools,
                id,
                "overrides",
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision(ConfigNamespace::Tools, id, raw, write_revision)
                .await?;
            return Err(error);
        }

        let after = serde_json::to_value(&record.spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix(
            AuditAction::Update,
            ConfigNamespace::Tools,
            id,
            "overrides",
            Some(before),
            Some(after.clone()),
            headers,
        )
        .await;

        Ok(after)
    }

    /// DELETE /v1/config/tools/:id/overrides/:field
    ///
    /// Removes a single field from the user overrides of a Builtin tool record.
    /// Returns 400 if `field` is not a recognized ToolSpecPatch field.
    /// Idempotent: if the field is not present in user_overrides, returns the
    /// current effective spec without writing to the store or emitting an audit event.
    pub async fn clear_tool_override_field(
        &self,
        id: &str,
        field: &str,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        let manager = self.runtime_manager()?;
        let _apply_guard = manager.lock_apply().await;

        let raw = self
            .store
            .get(ConfigNamespace::Tools.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("tools/{id}")))?;

        let mut record = ConfigRecord::<ToolSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(ConfigServiceError::OverridesNotSupportedForUserRecord);
        }

        let expected_revision = record.meta.revision;

        let before_spec = apply_overrides(record.spec.clone(), record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let before = serde_json::to_value(&before_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        // Validate that field is recognized by ToolSpecPatch before mutating.
        // Use a null probe: `ToolSpecPatch` accepts null for all Option fields, and
        // deny_unknown_fields will reject unknown field names.
        let probe = Value::Object({
            let mut m = Map::new();
            m.insert(field.to_string(), Value::Null);
            m
        });
        let _: ToolSpecPatch = serde_json::from_value(probe).map_err(|_| {
            ConfigServiceError::InvalidPayload(format!("unknown override field: {field}"))
        })?;

        // Remove the field from existing overrides.
        let mut existing_map: Map<String, Value> = record
            .meta
            .user_overrides
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        // Short-circuit: if the field is not present in overrides, this is a no-op —
        // skip the store write, apply_locked, and audit emit.
        if !existing_map.contains_key(field) {
            let effective_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            return serde_json::to_value(&effective_spec)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()));
        }

        existing_map.remove(field);

        let merged_value = Value::Object(existing_map.clone());
        record.meta.user_overrides = if existing_map.is_empty() {
            None
        } else {
            Some(merged_value)
        };
        record.meta.updated_at = now_ms();

        let write_revision = self
            .cas_put_record(ConfigNamespace::Tools, id, &mut record, expected_revision)
            .await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed(
                ConfigNamespace::Tools,
                id,
                &format!("overrides/{field}"),
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision(ConfigNamespace::Tools, id, raw, write_revision)
                .await?;
            return Err(error);
        }

        let after_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let after = serde_json::to_value(&after_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix(
            AuditAction::Update,
            ConfigNamespace::Tools,
            id,
            &format!("overrides/{field}"),
            Some(before),
            Some(after.clone()),
            headers,
        )
        .await;

        Ok(after)
    }
}

/// Return the effective spec Value for a stored entry, applying `user_overrides`
/// when the namespace supports it (currently only Agents).
///
/// For non-Agent namespaces this is equivalent to `unwrap_spec`.
fn effective_spec(namespace: ConfigNamespace, value: Value) -> Result<Value, ConfigServiceError> {
    match namespace {
        ConfigNamespace::Agents => {
            let record = ConfigRecord::<AgentSpec>::from_value(value)
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            let effective = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            serde_json::to_value(&effective)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))
        }
        ConfigNamespace::Tools => {
            let record = ConfigRecord::<ToolSpec>::from_value(value)
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            let effective = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
                .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
            serde_json::to_value(&effective)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))
        }
        _ => Ok(unwrap_spec(value)),
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
        | ConfigRuntimeError::InvalidConfig(_) => {
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
    use awaken_contract::{AgentSpec, BuiltinSeedSet, BuiltinSpec, ModelBindingSpec, ProviderSpec};
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
                    created_at: None,
                    updated_at: None,
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
            .create(
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
            .delete(
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
        assert_eq!(all.len(), 5, "exactly five namespaces");

        // Each variant must appear exactly once.
        let has = |v: ConfigNamespace| all.iter().filter(|&&x| x == v).count();
        assert_eq!(has(ConfigNamespace::Agents), 1);
        assert_eq!(has(ConfigNamespace::Providers), 1);
        assert_eq!(has(ConfigNamespace::Models), 1);
        assert_eq!(has(ConfigNamespace::McpServers), 1);
        assert_eq!(has(ConfigNamespace::Tools), 1);
    }

    #[test]
    fn namespace_all_matches_builtin_spec_namespace() {
        use awaken_contract::{BuiltinSpec, McpServerSpec, ToolSpec};

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
                    created_at: None,
                    updated_at: None,
                }),
                ConfigNamespace::McpServers => BuiltinSpec::McpServer(McpServerSpec {
                    id: "x".into(),
                    ..Default::default()
                }),
                ConfigNamespace::Tools => BuiltinSpec::Tool(ToolSpec {
                    id: "x".into(),
                    name: "x".into(),
                    description: "x".into(),
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

    // ── ConfigRecord envelope tests ─────────────────────────────────────────

    #[tokio::test]
    async fn put_emits_envelope_with_user_meta() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store.clone()).await;
        let service = ConfigService::new(&state).expect("service");

        service
            .create(
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
            .create(
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
            .update(
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
            .create(
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
    fn config_namespace_parses_tools() {
        assert_eq!(
            ConfigNamespace::parse("tools").unwrap(),
            ConfigNamespace::Tools
        );
        assert_eq!(ConfigNamespace::Tools.as_str(), "tools");
    }

    #[test]
    fn config_namespace_all_includes_tools() {
        assert!(ConfigNamespace::ALL.contains(&ConfigNamespace::Tools));
    }

    #[test]
    fn config_namespace_schema_for_tools_is_object() {
        let schema = ConfigNamespace::Tools.schema_json().expect("schema");
        // schemars 0.8: top-level object schema shape
        assert!(schema.get("$defs").is_some() || schema.get("type").is_some());
    }

    #[tokio::test]
    async fn tools_namespace_rejects_create() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store).await;
        let service = ConfigService::new(&state).expect("config service");
        let body = json!({"id": "x", "name": "x", "description": "x"});
        let err = service
            .create(ConfigNamespace::Tools, body, &axum::http::HeaderMap::new())
            .await
            .expect_err("tools namespace must reject create");
        match err {
            ConfigServiceError::InvalidPayload(msg) => assert!(msg.contains("tools")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tools_namespace_rejects_update() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store).await;
        let service = ConfigService::new(&state).expect("config service");
        let body = json!({"id": "x", "name": "x", "description": "x"});
        let err = service
            .update(
                ConfigNamespace::Tools,
                "x",
                body,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("tools namespace must reject update");
        match err {
            ConfigServiceError::InvalidPayload(msg) => assert!(msg.contains("tools")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tools_namespace_rejects_delete() {
        let config_store = Arc::new(awaken_stores::InMemoryStore::new());
        let (state, _manager) = build_state(config_store).await;
        let service = ConfigService::new(&state).expect("config service");
        let err = service
            .delete(
                ConfigNamespace::Tools,
                "x",
                false,
                &axum::http::HeaderMap::new(),
            )
            .await
            .expect_err("tools namespace must reject delete");
        match err {
            ConfigServiceError::InvalidPayload(msg) => assert!(msg.contains("tools")),
            other => panic!("expected InvalidPayload, got {other:?}"),
        }
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
                    created_at: None,
                    updated_at: None,
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
            .get(ConfigNamespace::Tools, "echo")
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
            .get_meta(ConfigNamespace::Tools, "echo")
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
            .get_meta(ConfigNamespace::Tools, "echo")
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
            .cas_put_record(
                ConfigNamespace::Tools,
                "echo",
                &mut stale_record,
                stale_expected,
            )
            .await
            .expect_err("stale write must conflict");
        assert!(matches!(err, ConfigServiceError::Conflict(_)));

        let meta_final = service
            .get_meta(ConfigNamespace::Tools, "echo")
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
                    created_at: None,
                    updated_at: None,
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
