use axum::http::HeaderMap;
use serde_json::Value;

use crate::services::config_envelope::unwrap_spec;

use super::{ConfigNamespace, ConfigService, ConfigServiceError, RestoreError};

impl ConfigService {
    /// Restore a resource to a previous version identified by the audit event ULID `version`.
    ///
    /// Per ADR-0028 D2-D4:
    /// - Looks up the audit event; returns `RestoreError::VersionNotFound` if missing.
    /// - Validates that the event resource matches `<namespace>/<id>` (cross-resource rejected).
    /// - Selects the spec payload: `after` for Create/Update/Publish/Restore; `before` for Delete.
    /// - Returns `RestoreError::NotRestorable` for Restart events (no spec payload).
    /// - Calls `persist_only_locked` (both create and update paths). Per ADR-0035 D11
    ///   restore is an editing-store operation: it does NOT trigger the runtime
    ///   hot-swap. Operators must run `publish` to promote the restored payload.
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
            // ADR-0035 D11: restore writes ConfigStore only; runtime
            // hot-swap is reserved for an explicit publish step.
            self.persist_only_locked(namespace, id, before.clone(), prepared)
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

            // ADR-0035 D11: restore-from-deleted is editing-store only.
            self.persist_only_locked(namespace, &body_id, None, prepared.clone())
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
}
