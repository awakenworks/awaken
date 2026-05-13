use awaken_contract::AuditAction;
use awaken_contract::{AgentSpec, ConfigRecord, RecordSource, now_ms};
use axum::http::HeaderMap;
use serde_json::{Map, Value};

use crate::services::config_envelope::apply_overrides;

use super::{
    ConfigNamespace, ConfigService, ConfigServiceError, map_runtime_error,
    overrides_not_supported_for_user_record,
};

impl<'a> ConfigService<'a> {
    /// POST /v1/config/agents/:id/overrides
    ///
    /// Dry-run validation for the override patch payload. It validates the
    /// same body shape and merged override state as `patch_agent_overrides`
    /// without writing the store, applying runtime config, or emitting audit.
    pub async fn validate_agent_overrides(
        &self,
        id: &str,
        body: Value,
    ) -> Result<Value, ConfigServiceError> {
        let raw = self
            .store
            .get(ConfigNamespace::Agents.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("agents/{id}")))?;

        let record = ConfigRecord::<AgentSpec>::from_value(raw)
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(overrides_not_supported_for_user_record());
        }

        let normalized = build_agent_overrides_patch(record.meta.user_overrides.as_ref(), &body)?
            .unwrap_or_else(|| Value::Object(Map::new()));

        Ok(serde_json::json!({
            "ok": true,
            "normalized": normalized,
        }))
    }

    /// PATCH /v1/config/agents/:id/overrides
    ///
    /// Merges the patch body into the existing `user_overrides` of a Builtin
    /// agent record. JSON null clears nullable AgentSpec fields; for other
    /// fields it removes the existing override. Non-null keys overwrite.
    /// Returns the effective AgentSpec after the merge.
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
            return Err(overrides_not_supported_for_user_record());
        }

        let expected_revision = record.meta.revision;

        let proposed_overrides =
            build_agent_overrides_patch(record.meta.user_overrides.as_ref(), &body)?;

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
            return Err(overrides_not_supported_for_user_record());
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
            return Err(overrides_not_supported_for_user_record());
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
        awaken_contract::validate_agent_spec_patch(probe).map_err(|_| {
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
}

fn is_nullable_agent_patch_field(field: &str) -> bool {
    matches!(
        field,
        "context_policy" | "allowed_tools" | "excluded_tools" | "reasoning_effort" | "endpoint"
    )
}

fn build_agent_overrides_patch(
    current_overrides: Option<&Value>,
    body: &Value,
) -> Result<Option<Value>, ConfigServiceError> {
    let body_map = match body {
        Value::Object(m) => m,
        _ => {
            return Err(ConfigServiceError::InvalidPayload(
                "expected JSON object body".into(),
            ));
        }
    };

    awaken_contract::validate_agent_spec_patch(body.clone())
        .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

    let mut existing_map: Map<String, Value> = current_overrides
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    for (key, value) in body_map {
        if value.is_null() {
            if is_nullable_agent_patch_field(key) {
                existing_map.insert(key.clone(), Value::Null);
            } else {
                existing_map.remove(key);
            }
        } else {
            existing_map.insert(key.clone(), value.clone());
        }
    }

    let merged_value = Value::Object(existing_map.clone());
    awaken_contract::validate_agent_spec_patch(merged_value.clone())
        .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

    Ok(if existing_map.is_empty() {
        None
    } else {
        Some(merged_value)
    })
}
