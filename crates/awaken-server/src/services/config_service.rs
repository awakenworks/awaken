use std::sync::Arc;

use awaken_server_contract::AuditAction;
use awaken_server_contract::contract::config_store::ConfigStore;
use awaken_server_contract::contract::storage::StorageError;
use awaken_server_contract::registry_spec::AWAKEN_BACKEND_KIND;
use awaken_server_contract::{
    AgentSpec, ConfigRecord, McpServerSpec, ModelPoolSpec, ModelSpec, ProviderSpec, SkillSpec,
    ToolSpec,
};
use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::app::{ConfigRoutesState, ServerState};
use crate::services::audit_log::AuditLogger;
use crate::services::config_envelope::unwrap_spec;

use super::config_runtime::ConfigRuntimeError;

mod agent_overrides;
mod audit;
mod dependencies;
mod mcp;
mod normalization;
mod provider;
mod restore;
mod storage;
mod tool_overrides;

use normalization::{
    classify_tool_source, effective_spec, effective_tool_spec, effective_visible_record,
};

pub(super) const TOOLS_NAMESPACE: &str = "tools";
pub(super) const SKILLS_NAMESPACE: &str = "skills";
const OVERRIDES_NOT_SUPPORTED_FOR_USER_RECORD: &str =
    "overrides are not supported for user-source records; use PUT to update";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigNamespace {
    Agents,
    Models,
    ModelPools,
    Providers,
    McpServers,
    Skills,
}

impl ConfigNamespace {
    /// All writable public managed namespaces in a fixed order.
    pub const ALL: [Self; 6] = [
        Self::Agents,
        Self::Providers,
        Self::Models,
        Self::ModelPools,
        Self::McpServers,
        Self::Skills,
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
            "model-pools" => Ok(Self::ModelPools),
            "providers" => Ok(Self::Providers),
            "mcp-servers" => Ok(Self::McpServers),
            SKILLS_NAMESPACE => Ok(Self::Skills),
            _ => Err(ConfigServiceError::UnknownNamespace(value.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agents => "agents",
            Self::Models => "models",
            Self::ModelPools => "model-pools",
            Self::Providers => "providers",
            Self::McpServers => "mcp-servers",
            Self::Skills => SKILLS_NAMESPACE,
        }
    }

    pub fn schema_json(self) -> Result<Value, ConfigServiceError> {
        let schema = match self {
            Self::Agents => schemars::schema_for!(AgentSpec),
            Self::Models => schemars::schema_for!(ModelSpec),
            Self::ModelPools => schemars::schema_for!(ModelPoolSpec),
            Self::Providers => schemars::schema_for!(ProviderSpec),
            Self::McpServers => schemars::schema_for!(McpServerSpec),
            Self::Skills => schemars::schema_for!(SkillSpec),
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

pub trait ConfigServiceStateProvider {
    fn config_service_state(&self) -> Result<ConfigRoutesState, ConfigServiceError>;
}

impl ConfigServiceStateProvider for ConfigRoutesState {
    fn config_service_state(&self) -> Result<ConfigRoutesState, ConfigServiceError> {
        Ok(self.clone())
    }
}

impl ConfigServiceStateProvider for ServerState {
    fn config_service_state(&self) -> Result<ConfigRoutesState, ConfigServiceError> {
        self.config_routes_state()
            .ok_or(ConfigServiceError::NotEnabled)
    }
}

pub struct ConfigService {
    state: ConfigRoutesState,
    store: Arc<dyn ConfigStore>,
    audit: Option<Arc<AuditLogger>>,
}

impl ConfigService {
    pub fn new<S: ConfigServiceStateProvider + ?Sized>(
        state: &S,
    ) -> Result<Self, ConfigServiceError> {
        let state = state.config_service_state()?;
        let store = state.config.config_store.clone();
        let audit = state.config.audit_log.clone();
        Ok(Self {
            state,
            store,
            audit,
        })
    }

    pub async fn capabilities(&self) -> Result<Value, ConfigServiceError> {
        let registries = self
            .state
            .run
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
                        .map(|schema| {
                            json!({
                                "key": schema.key,
                                "schema": schema.json_schema,
                                "display_name": schema.display_name,
                                "description": schema.description,
                                "category": schema.category,
                                "editor": schema.editor,
                                "default_value": schema.default_value,
                                "ui_schema": schema.ui_schema,
                            })
                        })
                        .collect::<Vec<_>>();
                    json!({
                        "id": plugin.descriptor().name,
                        "config_schemas": schemas,
                    })
                })
            })
            .collect::<Vec<_>>();

        // Capabilities exposes the full `ModelSpec` shape so consumers
        // (admin UI dropdowns, agent editor, external dashboards) can read
        // capability + pricing fields without a second fetch. Round-trips
        // are tested in `crates/awaken-server/tests/config_api.rs`.
        //
        // Serialization failure surfaces as an error rather than silently
        // dropping the model — a partial catalog would hide the problem.
        let mut model_ids = registries.models.model_ids();
        model_ids.sort();
        let models = model_ids
            .iter()
            .filter_map(|id| registries.models.get_model(id))
            .map(|model| serde_json::to_value(&model))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                ConfigServiceError::Apply(format!("failed to serialize model spec: {error}"))
            })?;
        let admin_assistant_config = crate::admin_assistant::load_config(&self.state.config)
            .await
            .map_err(|error| ConfigServiceError::Apply(error.to_string()))?;
        let admin_assistant_model_id = crate::admin_assistant::resolve_admin_assistant_model_id(
            registries.models.as_ref(),
            registries.providers.as_ref(),
            admin_assistant_config.model_id.as_deref(),
        );

        let providers = registries
            .providers
            .provider_ids()
            .into_iter()
            .map(|id| json!({ "id": id }))
            .collect::<Vec<_>>();

        let mut backend_ids = registries.backends.backend_ids();
        backend_ids.sort();
        let mut backends = vec![backend_schema_json(
            AWAKEN_BACKEND_KIND,
            awaken_runtime::awaken_backend_config_schema(),
        )];
        backends.extend(backend_ids.into_iter().filter_map(|kind| {
            if kind == AWAKEN_BACKEND_KIND {
                return None;
            }
            registries
                .backends
                .get_backend_factory(&kind)
                .map(|factory| backend_schema_json(factory.backend(), factory.config_schema()))
        }));

        let skills = self
            .state
            .config
            .skill_catalog_provider
            .as_ref()
            .map(|provider| provider.list_skills())
            .unwrap_or_default();

        Ok(json!({
            "agents": self.state.run.resolver.agent_ids(),
            "tools": tools,
            "plugins": plugins,
            "skills": skills,
            "models": models,
            "providers": providers,
            "backends": backends,
            "admin_assistant": crate::admin_assistant::admin_assistant_capability(
                &model_ids,
                admin_assistant_model_id,
            ),
            "supported_adapters": super::config_runtime::supported_adapters(),
            "namespaces": [
                { "namespace": "agents", "schema": ConfigNamespace::Agents.schema_json()? },
                { "namespace": "models", "schema": ConfigNamespace::Models.schema_json()? },
                { "namespace": "model-pools", "schema": ConfigNamespace::ModelPools.schema_json()? },
                { "namespace": "providers", "schema": ConfigNamespace::Providers.schema_json()? },
                { "namespace": "mcp-servers", "schema": ConfigNamespace::McpServers.schema_json()? },
                { "namespace": SKILLS_NAMESPACE, "schema": ConfigNamespace::Skills.schema_json()? },
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
    ) -> Result<Option<awaken_server_contract::RecordMeta>, ConfigServiceError> {
        let value = self.store.get(namespace.as_str(), id).await?;
        let Some(value) = value else {
            return Ok(None);
        };
        // For legacy records the envelope may not have been written yet
        // (legacy bare-spec). ConfigRecord::from_value handles both shapes.
        let meta = awaken_server_contract::ConfigRecord::<Value>::from_value(value)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?
            .meta;
        Ok(Some(meta))
    }

    pub(crate) async fn get_tool_meta(
        &self,
        id: &str,
    ) -> Result<Option<awaken_server_contract::RecordMeta>, ConfigServiceError> {
        let value = self.store.get(TOOLS_NAMESPACE, id).await?;
        let Some(value) = value else {
            return Ok(None);
        };
        let meta = awaken_server_contract::ConfigRecord::<Value>::from_value(value)
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
    ) -> Result<Vec<(String, awaken_server_contract::RecordMeta)>, ConfigServiceError> {
        let values = self.store.list(namespace.as_str(), offset, limit).await?;
        let mut out = Vec::with_capacity(values.len());
        for (id, value) in values {
            let meta = awaken_server_contract::ConfigRecord::<Value>::from_value(value)
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
    ) -> Result<Vec<(String, awaken_server_contract::RecordMeta)>, ConfigServiceError> {
        let values = self.store.list(TOOLS_NAMESPACE, offset, limit).await?;
        let mut out = Vec::with_capacity(values.len());
        for (id, value) in values {
            let meta = awaken_server_contract::ConfigRecord::<Value>::from_value(value)
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
        // The validate echo mirrors the normalized payload back to the admin;
        // redact it on the same boundary as list/get so a backend/provider
        // secret in the submitted body is never reflected verbatim.
        self.redact_response(namespace, normalized)
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
}

fn backend_schema_json(kind: &str, schema: awaken_runtime::ExecutionBackendConfigSchema) -> Value {
    json!({
        "kind": kind,
        "version": schema.version,
        "schema": schema.schema,
        "display_name": schema.display_name,
        "description": schema.description,
        "default_config": schema.default_config,
        "ui_schema": schema.ui_schema,
    })
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
        | ConfigRuntimeError::ChangeListener(_)
        | ConfigRuntimeError::VersionedRegistry(_) => ConfigServiceError::Apply(error.to_string()),
        ConfigRuntimeError::Storage(error) => ConfigServiceError::Storage(error),
    }
}

#[cfg(test)]
mod tests;
