use super::*;

#[test]
fn canonical_json_sorts_nested_object_keys() {
    // Nested keys must sort independent of insertion order so re-encoding
    // through a `preserve_order` Map does not silently change the hash.
    let a = json!({"outer": {"b": 2, "a": 1}, "alpha": "first"});
    let b = json!({"alpha": "first", "outer": {"a": 1, "b": 2}});
    let bytes_a = canonical_registry_json_bytes(1, &a).unwrap();
    let bytes_b = canonical_registry_json_bytes(1, &b).unwrap();
    assert_eq!(bytes_a, bytes_b);
    let text = std::str::from_utf8(&bytes_a).unwrap();
    let outer = text.find("\"outer\":").unwrap();
    let a_key = text[outer..].find("\"a\":").unwrap();
    let b_key = text[outer..].find("\"b\":").unwrap();
    assert!(a_key < b_key, "nested keys not sorted in {text}");
}

#[test]
fn canonical_envelope_locks_byte_shape() {
    // Lock the byte shape so any future change to the canonical envelope
    // invalidates published content_hashes loudly via this test failure
    // rather than silently in production.
    let bytes = canonical_registry_json_bytes(7, &json!({"id": "a"})).unwrap();
    assert_eq!(bytes, br#"{"value":{"id":"a"},"value_schema_version":7}"#);
}

#[test]
fn canonical_json_sorts_keys_by_encoded_octets() {
    // RFC 8785 ordering: by the JSON-encoded UTF-8 octet sequence.
    let value = json!({"z": 1, "a": 2, "中": 3});
    let bytes = canonical_registry_json_bytes(1, &value).unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    let a = text.find("\"a\"").unwrap();
    let z = text.find("\"z\"").unwrap();
    let cn = text.find("中").unwrap();
    assert!(a < z, "ASCII keys not sorted");
    assert!(z < cn, "multibyte key not sorted after ASCII");
}

#[test]
fn registry_content_hash_is_stable_for_equivalent_object_order() {
    let a = json!({"b": 2, "a": 1});
    let b = json!({"a": 1, "b": 2});

    let (hash_a, bytes_a) = registry_content_hash(1, &a).unwrap();
    let (hash_b, bytes_b) = registry_content_hash(1, &b).unwrap();

    assert_eq!(hash_a, hash_b);
    assert_eq!(bytes_a, bytes_b);
}

#[test]
fn build_rollback_metadata_injects_restored_from_when_absent() {
    let value = build_rollback_metadata(json!({"reason": "regression"}), 7).unwrap();
    assert_eq!(value["restored_from"], json!(7));
    assert_eq!(value["reason"], "regression");
}

#[test]
fn build_rollback_metadata_accepts_matching_restored_from() {
    let value = build_rollback_metadata(json!({"restored_from": 7}), 7).unwrap();
    assert_eq!(value["restored_from"], json!(7));
}

#[test]
fn build_rollback_metadata_rejects_mismatched_restored_from() {
    let err = build_rollback_metadata(json!({"restored_from": 9}), 7).unwrap_err();
    assert!(matches!(err, VersionedRegistryError::InvalidRequest(_)));
}

#[test]
fn build_rollback_metadata_rejects_non_object_metadata() {
    let err = build_rollback_metadata(json!([1, 2]), 7).unwrap_err();
    assert!(matches!(err, VersionedRegistryError::InvalidRequest(_)));
}

#[test]
fn build_rollback_metadata_accepts_null_and_returns_object() {
    let value = build_rollback_metadata(Value::Null, 3).unwrap();
    assert_eq!(value, json!({"restored_from": 3}));
}

#[tokio::test]
async fn typed_wrapper_rejects_publish_with_unsupported_schema_version() {
    use crate::contract::versioned_registry::TypedVersionedRegistry;

    let store =
        std::sync::Arc::new(crate::contract::versioned_registry::tests::FakeStore::default());
    let typed: TypedVersionedRegistry<serde_json::Value> =
        TypedVersionedRegistry::new(store.clone(), "default", "tool")
            .with_supported_schema_versions([1, 2]);
    let err = typed
        .publish("t1", json!({}), 3, json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        VersionedRegistryError::IncompatibleSchema { stored: 3, .. }
    ));
}

#[derive(Default)]
pub(super) struct FakeStore {
    records: parking_lot::Mutex<Vec<VersionedRecord<Value>>>,
}

#[async_trait::async_trait]
impl VersionedRegistryStore for FakeStore {
    async fn resource_state(
        &self,
        _scope_id: &str,
        _kind: &str,
        _id: &str,
    ) -> Result<Option<VersionedResourceState>, VersionedRegistryError> {
        Ok(None)
    }
    async fn current(
        &self,
        _scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        Ok(self
            .records
            .lock()
            .iter()
            .rev()
            .find(|r| r.kind == kind && r.id == id)
            .cloned())
    }
    async fn get(
        &self,
        _scope_id: &str,
        kind: &str,
        id: &str,
        version: u64,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        Ok(self
            .records
            .lock()
            .iter()
            .find(|r| r.kind == kind && r.id == id && r.version == version)
            .cloned())
    }
    async fn list_versions(
        &self,
        _scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Vec<VersionedRecord<Value>>, VersionedRegistryError> {
        Ok(self
            .records
            .lock()
            .iter()
            .filter(|r| r.kind == kind && r.id == id)
            .cloned()
            .collect())
    }
    async fn publish_resource(
        &self,
        _scope_id: &str,
        kind: &str,
        id: &str,
        value: Value,
        value_schema_version: u32,
        metadata: Value,
    ) -> Result<PublishOutcome<Value>, VersionedRegistryError> {
        let (content_hash, bytes) = registry_content_hash(value_schema_version, &value)?;
        let record = VersionedRecord {
            kind: kind.to_string(),
            id: id.to_string(),
            version: self.records.lock().len() as u64 + 1,
            content_hash,
            value_schema_version,
            value,
            canonical_json_bytes: bytes,
            created_at_ms: 0,
            metadata,
        };
        self.records.lock().push(record.clone());
        Ok(PublishOutcome::Created(record))
    }
    async fn rollback_resource(
        &self,
        _scope_id: &str,
        _kind: &str,
        _id: &str,
        _to_version: u64,
        _metadata: Value,
    ) -> Result<VersionedRecord<Value>, VersionedRegistryError> {
        unimplemented!()
    }
    async fn archive_resource(
        &self,
        _scope_id: &str,
        _kind: &str,
        _id: &str,
    ) -> Result<(), VersionedRegistryError> {
        Ok(())
    }
    async fn unarchive_resource(
        &self,
        _scope_id: &str,
        _kind: &str,
        _id: &str,
    ) -> Result<(), VersionedRegistryError> {
        Ok(())
    }
    async fn create_publication(
        &self,
        _scope_id: &str,
        _publication_id: &str,
        _entries: Vec<VersionRef>,
        _source_config_revisions: Vec<ConfigRevisionRef>,
        _created_by: Option<String>,
        _metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError> {
        unimplemented!()
    }
    async fn publish_resources_and_create_publication(
        &self,
        _scope_id: &str,
        _publication_id: &str,
        _resources: Vec<RegistryResourcePublish>,
        _source_config_revisions: Vec<ConfigRevisionRef>,
        _created_by: Option<String>,
        _metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError> {
        unimplemented!()
    }
    async fn latest_publication(
        &self,
        _scope_id: &str,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        Ok(None)
    }
    async fn get_publication(
        &self,
        _scope_id: &str,
        _snapshot_version: u64,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        Ok(None)
    }
}

#[tokio::test]
async fn typed_wrapper_get_rejects_stored_record_with_unsupported_schema_version() {
    use crate::contract::versioned_registry::TypedVersionedRegistry;

    let store = std::sync::Arc::new(FakeStore::default());
    // Publish using version 1 directly.
    store
        .publish_resource("default", "tool", "t1", json!({"v": 1}), 1, json!({}))
        .await
        .unwrap();

    // Wrapper only supports v2 going forward.
    let typed: TypedVersionedRegistry<serde_json::Value> =
        TypedVersionedRegistry::new(store, "default", "tool").with_supported_schema_versions([2]);

    let err = typed.get("t1", 1).await.unwrap_err();
    assert!(matches!(
        err,
        VersionedRegistryError::IncompatibleSchema { stored: 1, .. }
    ));
}

#[test]
fn verify_content_hash_rejects_tampered_canonical_bytes() {
    let value = json!({"id": "agent-1"});
    let (hash, bytes) = registry_content_hash(1, &value).unwrap();
    let mut record = VersionedRecord {
        kind: "agent".to_string(),
        id: "agent-1".to_string(),
        version: 1,
        content_hash: hash,
        value_schema_version: 1,
        value,
        canonical_json_bytes: bytes,
        created_at_ms: 0,
        metadata: Value::Null,
    };
    record.verify_content_hash().unwrap();
    record.canonical_json_bytes.push(b' ');
    assert!(matches!(
        record.verify_content_hash().unwrap_err(),
        VersionedRegistryError::Backend(_)
    ));
}

#[test]
fn registry_content_hash_includes_schema_version() {
    let value = json!({"id": "agent-1"});

    let (v1, _) = registry_content_hash(1, &value).unwrap();
    let (v2, _) = registry_content_hash(2, &value).unwrap();

    assert_ne!(v1, v2);
}
