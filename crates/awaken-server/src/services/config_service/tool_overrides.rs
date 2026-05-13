use awaken_contract::AuditAction;
use awaken_contract::{ConfigRecord, RecordSource, ToolSpec, ToolSpecPatch, now_ms};
use axum::http::HeaderMap;
use serde_json::{Map, Value};

use crate::services::config_envelope::apply_overrides;

use super::{
    ConfigService, ConfigServiceError, TOOLS_NAMESPACE, map_runtime_error,
    overrides_not_supported_for_user_record,
};

impl<'a> ConfigService<'a> {
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
            .get(TOOLS_NAMESPACE, id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("tools/{id}")))?;

        let mut record = ConfigRecord::<ToolSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(overrides_not_supported_for_user_record());
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
            .cas_put_record_in_namespace(TOOLS_NAMESPACE, id, &mut record, expected_revision)
            .await?;

        if let Err(error) = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error)
        {
            self.emit_audit_apply_failed_in_namespace(
                TOOLS_NAMESPACE,
                id,
                "overrides",
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision_in_namespace(
                TOOLS_NAMESPACE,
                id,
                raw,
                write_revision,
            )
            .await?;
            return Err(error);
        }

        let after_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let after = serde_json::to_value(&after_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix_in_namespace(
            AuditAction::Update,
            TOOLS_NAMESPACE,
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
            .get(TOOLS_NAMESPACE, id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("tools/{id}")))?;

        let mut record = ConfigRecord::<ToolSpec>::from_value(raw.clone())
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
            .cas_put_record_in_namespace(TOOLS_NAMESPACE, id, &mut record, expected_revision)
            .await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed_in_namespace(
                TOOLS_NAMESPACE,
                id,
                "overrides",
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision_in_namespace(
                TOOLS_NAMESPACE,
                id,
                raw,
                write_revision,
            )
            .await?;
            return Err(error);
        }

        let after = serde_json::to_value(&record.spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix_in_namespace(
            AuditAction::Update,
            TOOLS_NAMESPACE,
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
            .get(TOOLS_NAMESPACE, id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("tools/{id}")))?;

        let mut record = ConfigRecord::<ToolSpec>::from_value(raw.clone())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;

        if matches!(record.meta.source, RecordSource::User) {
            return Err(overrides_not_supported_for_user_record());
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
            .cas_put_record_in_namespace(TOOLS_NAMESPACE, id, &mut record, expected_revision)
            .await?;

        let apply_result = manager
            .apply_locked()
            .await
            .map(|_| ())
            .map_err(map_runtime_error);
        if let Err(error) = apply_result {
            self.emit_audit_apply_failed_in_namespace(
                TOOLS_NAMESPACE,
                id,
                &format!("overrides/{field}"),
                Some(before.clone()),
                None,
                error.to_string(),
                headers,
            )
            .await;
            self.rollback_to_raw_after_revision_in_namespace(
                TOOLS_NAMESPACE,
                id,
                raw,
                write_revision,
            )
            .await?;
            return Err(error);
        }

        let after_spec = apply_overrides(record.spec, record.meta.user_overrides.as_ref())
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))?;
        let after = serde_json::to_value(&after_spec)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;

        self.emit_audit_with_suffix_in_namespace(
            AuditAction::Update,
            TOOLS_NAMESPACE,
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
