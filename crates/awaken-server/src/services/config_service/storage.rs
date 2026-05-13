use std::sync::Arc;

use awaken_contract::contract::storage::StorageError;
use awaken_contract::{ConfigRecord, RecordMeta};
use axum::http::HeaderMap;
use serde_json::Value;

use crate::services::config_envelope::unwrap_spec;

use super::{ConfigNamespace, ConfigService, ConfigServiceError, map_runtime_error};

impl<'a> ConfigService<'a> {
    pub(super) fn runtime_manager(
        &self,
    ) -> Result<&Arc<crate::services::config_runtime::ConfigRuntimeManager>, ConfigServiceError>
    {
        self.state
            .config_runtime_manager
            .as_ref()
            .ok_or(ConfigServiceError::NotEnabled)
    }

    fn user_record_from_body(body: &Value, previous: Option<&Value>) -> ConfigRecord<Value> {
        let mut meta = RecordMeta::new_user();
        if let Some(previous) = previous
            && let Ok(previous_record) = ConfigRecord::<Value>::from_value(previous.clone())
            && previous_record.meta.created_at != 0
        {
            meta.created_at = previous_record.meta.created_at;
        }
        ConfigRecord {
            spec: body.clone(),
            meta,
        }
    }

    pub(super) fn storage_write_error(
        namespace: ConfigNamespace,
        id: &str,
        error: StorageError,
    ) -> ConfigServiceError {
        Self::storage_write_error_for_namespace(namespace.as_str(), id, error)
    }

    pub(super) fn storage_write_error_for_namespace(
        namespace: &str,
        id: &str,
        error: StorageError,
    ) -> ConfigServiceError {
        match error {
            StorageError::AlreadyExists(_) => {
                ConfigServiceError::Conflict(format!("{namespace}/{id} already exists"))
            }
            StorageError::VersionConflict { expected, actual } => {
                ConfigServiceError::Conflict(format!(
                    "{}/{} was modified by another writer (expected revision {expected}, found {actual}); retry the mutation",
                    namespace, id,
                ))
            }
            other => ConfigServiceError::Storage(other),
        }
    }

    pub(super) async fn insert_record_absent<T: serde::Serialize + serde::de::DeserializeOwned>(
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
    pub(super) async fn cas_put_record<T: serde::Serialize + serde::de::DeserializeOwned>(
        &self,
        namespace: ConfigNamespace,
        id: &str,
        record: &mut ConfigRecord<T>,
        expected_revision: u64,
    ) -> Result<u64, ConfigServiceError> {
        self.cas_put_record_in_namespace(namespace.as_str(), id, record, expected_revision)
            .await
    }

    pub(super) async fn cas_put_record_in_namespace<
        T: serde::Serialize + serde::de::DeserializeOwned,
    >(
        &self,
        namespace: &str,
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
            .put_if_revision(namespace, id, &envelope, expected_revision)
            .await
            .map(|()| next_revision)
            .map_err(|error| Self::storage_write_error_for_namespace(namespace, id, error))
    }

    pub(super) async fn cas_delete_record(
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

    pub(super) async fn rollback_to_raw_after_revision(
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

    pub(super) async fn rollback_to_raw_after_revision_in_namespace(
        &self,
        namespace: &str,
        id: &str,
        raw: Value,
        expected_revision: u64,
    ) -> Result<u64, ConfigServiceError> {
        let mut rollback = ConfigRecord::<Value>::from_value(raw)
            .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;
        self.cas_put_record_in_namespace(namespace, id, &mut rollback, expected_revision)
            .await
    }

    pub(super) async fn rollback_deleted_records(
        &self,
        deleted_records: Vec<(ConfigNamespace, String, Value, u64)>,
    ) -> Result<(), ConfigServiceError> {
        for (rollback_namespace, rollback_id, raw, revision) in deleted_records.into_iter().rev() {
            let mut rollback = ConfigRecord::<Value>::from_value(raw)
                .map_err(|e| ConfigServiceError::Serialization(e.to_string()))?;
            self.insert_record_absent(
                rollback_namespace,
                &rollback_id,
                &mut rollback,
                revision + 1,
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn persist_and_apply_locked(
        &self,
        manager: &crate::services::config_runtime::ConfigRuntimeManager,
        namespace: ConfigNamespace,
        id: &str,
        previous: Option<Value>,
        body: Value,
        headers: &HeaderMap,
    ) -> Result<Value, ConfigServiceError> {
        self.validate_payload(namespace, &body)?;
        let mut record = Self::user_record_from_body(&body, previous.as_ref());
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
}
