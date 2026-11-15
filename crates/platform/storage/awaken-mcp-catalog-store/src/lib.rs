// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: One conditional object per scoped discovery identity; readers can rebuild on misses.
use std::sync::Arc;

use awaken_session_configuration_contract::mcp::tool_catalog::{
    StdioToolCatalog, StdioToolCatalogError, StdioToolCatalogKey, StdioToolDiscovery,
    VersionedStdioToolCatalog,
};
use object_store::aws::{AmazonS3Builder, S3ConditionalPut};
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion};

const MAX_BYTES: u64 = 8 * 1024 * 1024;

pub struct ObjectStdioCatalogStore {
    objects: Arc<dyn ObjectStore>,
    prefix: Path,
}

impl ObjectStdioCatalogStore {
    /// Startup adapter used by both composition roots to select one optional
    /// bucket and prefix. The request path uses only the injected backend.
    #[expect(
        clippy::disallowed_methods,
        reason = "this startup adapter reads only its own S3 selection"
    )]
    pub fn from_environment() -> Result<Option<Arc<dyn StdioToolDiscovery>>, String> {
        let Some(bucket) = std::env::var_os("AWAKEN_MCP_CATALOG_S3_BUCKET") else {
            return Ok(None);
        };
        let bucket = bucket
            .to_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("AWAKEN_MCP_CATALOG_S3_BUCKET must name a bucket")?;
        let prefix = std::env::var("AWAKEN_MCP_CATALOG_S3_PREFIX")
            .unwrap_or_else(|_| "mcp-tool-catalog".to_owned());
        let path = Path::parse(&prefix).map_err(|_| "invalid MCP catalog object prefix")?;
        if prefix.is_empty() || path.as_ref() != prefix {
            return Err("MCP catalog requires a normalized nonempty prefix".into());
        }
        let objects = AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_conditional_put(S3ConditionalPut::ETagMatch)
            .build()
            .map_err(|error| format!("configure MCP catalog object store: {error}"))?;
        Ok(Some(Arc::new(Self {
            objects: Arc::new(objects),
            prefix: path,
        })))
    }

    fn key(&self, key: &StdioToolCatalogKey) -> Path {
        Path::from(format!("{}/{}", self.prefix.as_ref(), key.storage_path()))
    }
}

#[async_trait::async_trait]
impl StdioToolDiscovery for ObjectStdioCatalogStore {
    async fn get(
        &self,
        key: &StdioToolCatalogKey,
    ) -> Result<Option<VersionedStdioToolCatalog>, StdioToolCatalogError> {
        let value = match self.objects.get(&self.key(key)).await {
            Ok(value) => value,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(_) => return Err(StdioToolCatalogError::Unavailable),
        };
        if value.meta.size > MAX_BYTES {
            return Err(StdioToolCatalogError::Unavailable);
        }
        let version = value
            .meta
            .e_tag
            .clone()
            .ok_or(StdioToolCatalogError::Unavailable)?;
        let bytes = value
            .bytes()
            .await
            .map_err(|_| StdioToolCatalogError::Unavailable)?;
        let stored: StoredCatalog =
            serde_json::from_slice(&bytes).map_err(|_| StdioToolCatalogError::Unavailable)?;
        Ok(Some(VersionedStdioToolCatalog {
            version,
            catalog: stored.catalog,
        }))
    }

    async fn put_if_version(
        &self,
        key: &StdioToolCatalogKey,
        expected: Option<&str>,
        catalog: &StdioToolCatalog,
    ) -> Result<bool, StdioToolCatalogError> {
        let key = self.key(key);
        let observation = uuid::Uuid::new_v4().to_string();
        let bytes = serde_json::to_vec(&StoredCatalogRef {
            observation: &observation,
            catalog,
        })
        .map_err(|_| StdioToolCatalogError::Unavailable)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(StdioToolCatalogError::Unavailable);
        }
        let mode = expected.map_or(PutMode::Create, |version| {
            PutMode::Update(UpdateVersion {
                e_tag: Some(version.to_owned()),
                version: None,
            })
        });
        match self
            .objects
            .put_opts(
                &key,
                bytes.into(),
                PutOptions {
                    mode,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(error) => {
                let conflicted = matches!(
                    error,
                    object_store::Error::Precondition { .. }
                        | object_store::Error::AlreadyExists { .. }
                );
                // A lost success response may be observed by exact observation id.
                let response = match self.objects.get(&key).await {
                    Ok(response) => response,
                    Err(_) if conflicted => return Ok(false),
                    Err(_) => return Err(StdioToolCatalogError::Unavailable),
                };
                if response.meta.size > MAX_BYTES {
                    return Err(StdioToolCatalogError::Unavailable);
                }
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|_| StdioToolCatalogError::Unavailable)?;
                let stored: StoredCatalog = serde_json::from_slice(&bytes)
                    .map_err(|_| StdioToolCatalogError::Unavailable)?;
                if stored.observation == observation {
                    Ok(true)
                } else if conflicted {
                    Ok(false)
                } else {
                    Err(StdioToolCatalogError::Unavailable)
                }
            }
        }
    }
}

#[derive(serde::Serialize)]
struct StoredCatalogRef<'a> {
    observation: &'a str,
    catalog: &'a StdioToolCatalog,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCatalog {
    observation: String,
    catalog: StdioToolCatalog,
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::McpTarget;

    #[tokio::test]
    async fn exact_object_version_controls_replacement_across_store_instances() {
        let objects = Arc::new(object_store::memory::InMemory::new());
        let worker_a = ObjectStdioCatalogStore {
            objects: objects.clone(),
            prefix: Path::from("mcp"),
        };
        let worker_b = ObjectStdioCatalogStore {
            objects,
            prefix: Path::from("mcp"),
        };
        let target = McpTarget::sandbox_stdio("/bin/mcp", vec![]).unwrap();
        let target = target.sandbox_stdio_target().unwrap();
        let key = StdioToolCatalogKey::for_preview("workspace-a", target, "env", 1);
        let other = StdioToolCatalogKey::for_preview("workspace-b", target, "env", 1);
        let entry = |time| StdioToolCatalog {
            observed_at_unix_ms: time,
            launch_fingerprint: "image".into(),
            tools: vec![],
        };
        assert!(
            worker_a
                .put_if_version(&key, None, &entry(10))
                .await
                .unwrap()
        );
        assert!(
            !worker_b
                .put_if_version(&key, None, &entry(20))
                .await
                .unwrap()
        );
        assert!(worker_b.get(&other).await.unwrap().is_none());
        let before = worker_b.get(&key).await.unwrap().unwrap();
        assert_eq!(before.catalog.observed_at_unix_ms, 10);
        assert!(
            worker_b
                .put_if_version(&key, Some(&before.version), &entry(30))
                .await
                .unwrap()
        );
        assert!(
            !worker_a
                .put_if_version(&key, Some(&before.version), &entry(20))
                .await
                .unwrap()
        );
        assert_eq!(
            worker_a
                .get(&key)
                .await
                .unwrap()
                .unwrap()
                .catalog
                .observed_at_unix_ms,
            30
        );
    }

    #[tokio::test]
    #[ignore = "requires a task-owned S3 bucket"]
    async fn live_s3_retains_a_catalog_across_independent_clients() {
        let writer = ObjectStdioCatalogStore::from_environment()
            .unwrap()
            .expect("test S3 bucket");
        let restarted = ObjectStdioCatalogStore::from_environment()
            .unwrap()
            .expect("test S3 bucket");
        let target =
            McpTarget::sandbox_stdio("/bin/mcp", vec![uuid::Uuid::new_v4().to_string()]).unwrap();
        let target = target.sandbox_stdio_target().unwrap();
        let key = StdioToolCatalogKey::for_preview("fixture-one", target, "image", 1);
        let other = StdioToolCatalogKey::for_preview("fixture-two", target, "image", 1);
        let original = StdioToolCatalog {
            observed_at_unix_ms: 1,
            launch_fingerprint: "fixture".into(),
            tools: Vec::new(),
        };
        assert!(writer.put_if_version(&key, None, &original).await.unwrap());
        assert!(
            !restarted
                .put_if_version(&key, None, &original)
                .await
                .unwrap()
        );
        assert!(restarted.get(&other).await.unwrap().is_none());
        let read = restarted.get(&key).await.unwrap().unwrap();
        assert_eq!(read.catalog.observed_at_unix_ms, 1);
        let next = StdioToolCatalog {
            observed_at_unix_ms: 2,
            ..original.clone()
        };
        assert!(
            restarted
                .put_if_version(&key, Some(&read.version), &next)
                .await
                .unwrap()
        );
        assert!(
            !writer
                .put_if_version(&key, Some(&read.version), &original)
                .await
                .unwrap()
        );
        assert_eq!(
            writer
                .get(&key)
                .await
                .unwrap()
                .unwrap()
                .catalog
                .observed_at_unix_ms,
            2
        );
    }
}
