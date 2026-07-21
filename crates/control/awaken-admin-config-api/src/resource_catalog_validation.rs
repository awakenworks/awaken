//! Backend-neutral Resource Catalog invariants shared by SQLite and Postgres.

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, RepositoryConfigVersion, ResourceCatalogError,
    ResourceState,
};

pub(crate) fn validate_live_definition(
    id: &str,
    state: ResourceState,
) -> Result<(), ResourceCatalogError> {
    if state == ResourceState::Active {
        Ok(())
    } else {
        Err(ResourceCatalogError::NotActive {
            id: id.into(),
            state,
        })
    }
}

pub(crate) fn validate_memory_config(
    id: &str,
    version: ConfigVersion,
    config: &MemoryStoreConfigVersion,
) -> Result<(), ResourceCatalogError> {
    if config.memory_store_id == id && config.version == version {
        Ok(())
    } else {
        Err(ResourceCatalogError::Storage(format!(
            "MemoryStore `{id}` config version {} is corrupt",
            version.0
        )))
    }
}

pub(crate) fn validate_repository_config(
    id: &str,
    version: ConfigVersion,
    config: &RepositoryConfigVersion,
) -> Result<(), ResourceCatalogError> {
    if config.repository_id == id && config.version == version {
        Ok(())
    } else {
        Err(ResourceCatalogError::Storage(format!(
            "Repository `{id}` config version {} is corrupt",
            version.0
        )))
    }
}

pub(crate) fn validate_initial(
    id: &str,
    workspace_id: &str,
    current: ConfigVersion,
    config_id: &str,
    config_version: ConfigVersion,
) -> Result<(), ResourceCatalogError> {
    if id.trim().is_empty() || workspace_id.trim().is_empty() {
        return Err(ResourceCatalogError::Invalid(
            "resource id and workspace id must be non-empty".into(),
        ));
    }
    if id != config_id
        || current != ConfigVersion::INITIAL
        || config_version != ConfigVersion::INITIAL
    {
        return Err(ResourceCatalogError::Invalid(
            "initial resource definition/config must agree at version 1".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_publish(
    id: &str,
    current: ConfigVersion,
    expected: ConfigVersion,
    next: ConfigVersion,
) -> Result<(), ResourceCatalogError> {
    if current != expected {
        return Err(ResourceCatalogError::ConfigConflict {
            id: id.into(),
            expected,
            current,
        });
    }
    let required = current.checked_next().ok_or_else(|| {
        ResourceCatalogError::Invalid(format!("resource `{id}` exhausted config versions"))
    })?;
    if next != required {
        return Err(ResourceCatalogError::Invalid(format!(
            "resource `{id}` config version must advance from {} to {}",
            current.0, required.0
        )));
    }
    Ok(())
}
