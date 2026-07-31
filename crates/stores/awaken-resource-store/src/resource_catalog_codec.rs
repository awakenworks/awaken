//! Resources-owned storage codec shared by Resource Catalog adapters.
//!
//! SQL syntax and transaction mechanics remain adapter-specific; this module
//! owns only the canonical aggregate JSON shape.

use std::collections::BTreeMap;

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RepositoryConfigVersion,
    RepositoryDefinition,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryRecord {
    pub definition: MemoryStoreDefinition,
    pub configs: BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RepositoryRecord {
    pub definition: RepositoryDefinition,
    pub configs: BTreeMap<ConfigVersion, RepositoryConfigVersion>,
}

pub(crate) fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}
