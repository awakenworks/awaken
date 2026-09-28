// @formal entrypoint environment_image_build.single_claim_exact_reclaim_and_stale_fence
use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_executable_environment_contract::{
    EnvironmentImageBuildClaim, EnvironmentImageBuildDemand, EnvironmentImageBuildError,
    EnvironmentImageBuildRecord, EnvironmentImageBuildStore,
};

use crate::durable::{DurableEnvironmentImageBuildStore, RecordBackend, VersionedRecord};

/// Test substrate for the same compare-and-swap store used by durable adapters.
/// Only record IO lives here; claim, replay and terminal rules have one owner.
#[derive(Default)]
struct MemoryRecordBackend(Mutex<BTreeMap<String, VersionedRecord>>);

#[async_trait]
impl RecordBackend for MemoryRecordBackend {
    async fn insert(
        &self,
        record: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let mut rows = self.0.lock().expect("image build store");
        if rows.contains_key(&record.demand.build_key) {
            return Ok(false);
        }
        rows.insert(
            record.demand.build_key.clone(),
            VersionedRecord {
                version: 0,
                record: record.clone(),
            },
        );
        Ok(true)
    }
    async fn get(&self, key: &str) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError> {
        Ok(self.0.lock().expect("image build store").get(key).cloned())
    }
    async fn candidates(&self) -> Result<Vec<VersionedRecord>, EnvironmentImageBuildError> {
        Ok(self
            .0
            .lock()
            .expect("image build store")
            .values()
            .cloned()
            .collect())
    }
    async fn compare_and_swap(
        &self,
        expected: &VersionedRecord,
        next: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let mut rows = self.0.lock().expect("image build store");
        let Some(current) = rows.get_mut(&expected.record.demand.build_key) else {
            return Ok(false);
        };
        if current.version != expected.version {
            return Ok(false);
        }
        let version = current
            .version
            .checked_add(1)
            .ok_or_else(|| crate::durable::storage("image build revision exhausted"))?;
        *current = VersionedRecord {
            version,
            record: next.clone(),
        };
        Ok(true)
    }
}

pub struct InMemoryEnvironmentImageBuildStore {
    inner: DurableEnvironmentImageBuildStore<MemoryRecordBackend>,
}

impl Default for InMemoryEnvironmentImageBuildStore {
    fn default() -> Self {
        Self::new()
    }
}
impl InMemoryEnvironmentImageBuildStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: DurableEnvironmentImageBuildStore::new(MemoryRecordBackend::default()),
        }
    }
}
#[async_trait]
impl EnvironmentImageBuildStore for InMemoryEnvironmentImageBuildStore {
    async fn ensure(
        &self,
        demand: EnvironmentImageBuildDemand,
        now_ms: u64,
    ) -> Result<(), EnvironmentImageBuildError> {
        self.inner.ensure(demand, now_ms).await
    }
    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<EnvironmentImageBuildRecord>, EnvironmentImageBuildError> {
        self.inner.get(build_key).await
    }
    async fn claim_next(
        &self,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<EnvironmentImageBuildClaim>, EnvironmentImageBuildError> {
        self.inner.claim_next(owner, now_ms, lease_ms).await
    }
    async fn complete(
        &self,
        claim: &EnvironmentImageBuildClaim,
        image: &str,
        now_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError> {
        self.inner.complete(claim, image, now_ms).await
    }
    async fn fail(
        &self,
        claim: &EnvironmentImageBuildClaim,
        message: &str,
        now_ms: u64,
        retry_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError> {
        self.inner.fail(claim, message, now_ms, retry_ms).await
    }
    async fn invalidate_ready(
        &self,
        build_key: &str,
        now_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError> {
        self.inner.invalidate_ready(build_key, now_ms).await
    }
}
