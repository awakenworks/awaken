use super::*;

#[derive(Default)]
struct FakeRegistryStore {
    records: std::sync::Mutex<Vec<VersionedRecord<Value>>>,
}

#[async_trait]
impl VersionedRegistryStore for FakeRegistryStore {
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
            .map_err(|error| VersionedRegistryError::Backend(error.to_string()))?
            .iter()
            .rev()
            .find(|record| record.kind == kind && record.id == id)
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
            .map_err(|error| VersionedRegistryError::Backend(error.to_string()))?
            .iter()
            .find(|record| record.kind == kind && record.id == id && record.version == version)
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
            .map_err(|error| VersionedRegistryError::Backend(error.to_string()))?
            .iter()
            .filter(|record| record.kind == kind && record.id == id)
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
        let mut records = self
            .records
            .lock()
            .map_err(|error| VersionedRegistryError::Backend(error.to_string()))?;
        let record = VersionedRecord {
            kind: kind.to_string(),
            id: id.to_string(),
            version: records.len() as u64 + 1,
            content_hash,
            value_schema_version,
            value,
            canonical_json_bytes: bytes,
            created_at_ms: 0,
            metadata,
        };
        records.push(record.clone());
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
        Err(VersionedRegistryError::Backend(
            "not implemented in test".into(),
        ))
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
        Err(VersionedRegistryError::Backend(
            "not implemented in test".into(),
        ))
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
        Err(VersionedRegistryError::Backend(
            "not implemented in test".into(),
        ))
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

#[test]
fn typed_wrapper_can_bind_validated_scope_id() {
    let store = std::sync::Arc::new(FakeRegistryStore::default());
    let typed: ScopedVersionedRegistry<Value> = ScopedVersionedRegistry::new_scoped(
        store.clone(),
        ScopeId::new("scope-a").unwrap(),
        "tool",
    );

    assert_eq!(typed.scope_id(), "scope-a");
    assert!(matches!(
        ScopedVersionedRegistry::<Value>::try_new(store, " ", "tool"),
        Err(ScopeError::Empty)
    ));
}

#[tokio::test]
async fn typed_wrapper_rejects_incompatible_schema_versions() {
    let store = std::sync::Arc::new(FakeRegistryStore::default());
    let typed: TypedVersionedRegistry<Value> =
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

    store
        .publish_resource("default", "tool", "t2", json!({"v": 1}), 1, json!({}))
        .await
        .unwrap();
    let typed: TypedVersionedRegistry<Value> =
        TypedVersionedRegistry::new(store, "default", "tool").with_supported_schema_versions([2]);
    let err = typed.get("t2", 1).await.unwrap_err();
    assert!(matches!(
        err,
        VersionedRegistryError::IncompatibleSchema { stored: 1, .. }
    ));
}
