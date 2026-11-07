// @formal entrypoint environment_image_build.single_claim_exact_reclaim_and_stale_fence
use async_trait::async_trait;
use awaken_executable_environment_contract::{
    EnvironmentImageBuildClaim, EnvironmentImageBuildDemand, EnvironmentImageBuildError,
    EnvironmentImageBuildRecord, EnvironmentImageBuildStore,
};
use awaken_sync::LeafLock;
use std::collections::BTreeMap;

use crate::durable::{DurableEnvironmentImageBuildStore, RecordBackend, VersionedRecord};

/// Test substrate for the same compare-and-swap store used by durable adapters.
/// Only record IO lives here; claim, replay and terminal rules have one owner.
#[derive(Default)]
struct MemoryRecordBackend(LeafLock<MemoryRecords>);

#[derive(Default)]
struct MemoryRecords {
    jobs: BTreeMap<String, VersionedRecord>,
    bindings: BTreeMap<String, String>,
}

#[async_trait]
impl RecordBackend for MemoryRecordBackend {
    async fn insert(
        &self,
        record: &EnvironmentImageBuildRecord,
        binding_key: Option<&str>,
    ) -> Result<String, EnvironmentImageBuildError> {
        let mut rows = self.0.lock();
        if let Some(key) = binding_key.and_then(|key| rows.bindings.get(key)) {
            return Ok(key.clone());
        }
        if rows
            .jobs
            .get(&record.demand.build_key)
            .is_some_and(|current| !current.record.demand.same_recipe(&record.demand))
        {
            return Err(EnvironmentImageBuildError::Conflict(
                record.demand.build_key.clone(),
            ));
        }
        rows.jobs
            .entry(record.demand.build_key.clone())
            .or_insert(VersionedRecord {
                version: 0,
                record: record.clone(),
            });
        if let Some(binding) = binding_key {
            rows.bindings
                .insert(binding.into(), record.demand.build_key.clone());
        }
        Ok(record.demand.build_key.clone())
    }
    async fn pinned_key(
        &self,
        binding_key: &str,
    ) -> Result<Option<String>, EnvironmentImageBuildError> {
        Ok(self.0.lock().bindings.get(binding_key).cloned())
    }
    async fn get(&self, key: &str) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError> {
        Ok(self.0.lock().jobs.get(key).cloned())
    }
    async fn candidates(&self) -> Result<Vec<VersionedRecord>, EnvironmentImageBuildError> {
        Ok(self.0.lock().jobs.values().cloned().collect())
    }
    async fn compare_and_swap(
        &self,
        expected: &VersionedRecord,
        next: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let mut rows = self.0.lock();
        let Some(current) = rows.jobs.get_mut(&expected.record.demand.build_key) else {
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
        binding_key: Option<&str>,
    ) -> Result<EnvironmentImageBuildDemand, EnvironmentImageBuildError> {
        self.inner.ensure(demand, now_ms, binding_key).await
    }
    async fn pinned_demand(
        &self,
        binding_key: &str,
    ) -> Result<Option<EnvironmentImageBuildDemand>, EnvironmentImageBuildError> {
        self.inner.pinned_demand(binding_key).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_executable_environment_contract::EnvironmentImageBuildState;
    use awaken_lease_contract::fence::Epoch;

    #[tokio::test]
    async fn an_integrity_refused_build_is_skipped_permanently_and_never_blocks_the_queue() {
        // The one claim scan (`DurableEnvironmentImageBuildStore::claim_next`)
        // refuses only the build whose persisted attempt is untrustworthy.
        // | Rule | head of the scan                         | effect                                   |
        // |------|------------------------------------------|------------------------------------------|
        // | Q1   | due Failed at attempt 0 / Pending at MAX | skipped, record unchanged                |
        // | Q2   | a healthy Pending build behind it        | claimed (no head-of-line blocking)       |
        // | Q3   | every later scan                         | the refused build is never claimed: the  |
        // |      |                                          | claim count stays 0 over 3 scans         |
        let store = InMemoryEnvironmentImageBuildStore::new();
        let mut keys = Vec::new();
        for (suffix, state) in [
            (
                "a-zero",
                EnvironmentImageBuildState::Failed {
                    message: "m".into(),
                    retry_at_ms: 0,
                    attempt: 0,
                },
            ),
            (
                "b-exhausted",
                EnvironmentImageBuildState::Pending { attempt: u64::MAX },
            ),
        ] {
            let mut demand = crate::test_demand();
            demand.build_key = format!("{suffix}:{}", demand.build_key);
            store.ensure(demand.clone(), 1, None).await.unwrap();
            let current = store.inner_backend_record(&demand.build_key);
            let next = EnvironmentImageBuildRecord {
                state,
                ..current.record.clone()
            };
            assert!(
                store
                    .inner
                    .backend()
                    .compare_and_swap(&current, &next)
                    .await
                    .unwrap()
            );
            keys.push(demand.build_key);
        }
        let mut healthy = crate::test_demand();
        healthy.build_key = format!("c-healthy:{}", healthy.build_key);
        store.ensure(healthy.clone(), 1, None).await.unwrap();
        let refused = keys
            .iter()
            .map(|key| store.inner_backend_record(key).record)
            .collect::<Vec<_>>();

        let claimed = store.claim_next("worker", 10, 10).await.unwrap().unwrap();
        assert_eq!(claimed.demand.build_key, healthy.build_key, "Q2");
        assert_eq!(claimed.lease_epoch, Epoch::FIRST, "Q2");
        for scan in 0..3 {
            assert!(
                store
                    .claim_next("worker", 11 + scan, 10)
                    .await
                    .unwrap()
                    .is_none(),
                "Q3 scan {scan}: only the refused builds remain, and none is claimed"
            );
        }
        for (key, before) in keys.iter().zip(refused) {
            assert_eq!(store.inner_backend_record(key).record, before, "Q1 {key}");
        }
    }

    impl InMemoryEnvironmentImageBuildStore {
        fn inner_backend_record(&self, key: &str) -> VersionedRecord {
            self.inner
                .backend()
                .0
                .lock()
                .jobs
                .get(key)
                .cloned()
                .unwrap()
        }
    }
}
