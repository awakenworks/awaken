use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_environment_realization_contract::{
    EnvironmentImageBuildClaim, EnvironmentImageBuildDemand, EnvironmentImageBuildError,
    EnvironmentImageBuildRecord, EnvironmentImageBuildState, EnvironmentImageBuildStore,
};

#[derive(Default)]
pub struct InMemoryEnvironmentImageBuildStore {
    records: Mutex<BTreeMap<String, EnvironmentImageBuildRecord>>,
}

impl InMemoryEnvironmentImageBuildStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EnvironmentImageBuildStore for InMemoryEnvironmentImageBuildStore {
    async fn ensure(
        &self,
        demand: EnvironmentImageBuildDemand,
        now_ms: u64,
    ) -> Result<(), EnvironmentImageBuildError> {
        let mut records = self.records.lock().expect("image build store");
        match records.get(&demand.build_key) {
            Some(existing) if existing.demand.same_recipe(&demand) => Ok(()),
            Some(_) => Err(EnvironmentImageBuildError::Conflict(demand.build_key)),
            None => {
                records.insert(
                    demand.build_key.clone(),
                    EnvironmentImageBuildRecord {
                        demand,
                        state: EnvironmentImageBuildState::default(),
                        updated_at_ms: now_ms,
                    },
                );
                Ok(())
            }
        }
    }

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<EnvironmentImageBuildRecord>, EnvironmentImageBuildError> {
        Ok(self
            .records
            .lock()
            .expect("image build store")
            .get(build_key)
            .cloned())
    }

    async fn claim_next(
        &self,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<EnvironmentImageBuildClaim>, EnvironmentImageBuildError> {
        let mut records = self.records.lock().expect("image build store");
        for record in records.values_mut() {
            let Some(state) = record.state.claim(owner, now_ms, lease_ms)? else {
                continue;
            };
            let EnvironmentImageBuildState::Building { lease_epoch, .. } = state else {
                unreachable!("claim transition returns Building")
            };
            record.state = state;
            record.updated_at_ms = now_ms;
            return Ok(Some(EnvironmentImageBuildClaim {
                demand: record.demand.clone(),
                owner: owner.to_owned(),
                lease_epoch,
            }));
        }
        Ok(None)
    }

    async fn complete(
        &self,
        claim: &EnvironmentImageBuildClaim,
        image: &str,
        now_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError> {
        self.transition(&claim.demand.build_key, now_ms, |state| {
            state.complete(&claim.owner, claim.lease_epoch, image, now_ms)
        })
    }

    async fn fail(
        &self,
        claim: &EnvironmentImageBuildClaim,
        message: &str,
        now_ms: u64,
        retry_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError> {
        self.transition(&claim.demand.build_key, now_ms, |state| {
            state.fail(&claim.owner, claim.lease_epoch, message, now_ms, retry_ms)
        })
    }

    async fn invalidate_ready(
        &self,
        build_key: &str,
        now_ms: u64,
    ) -> Result<bool, EnvironmentImageBuildError> {
        self.transition(
            build_key,
            now_ms,
            EnvironmentImageBuildState::invalidate_ready,
        )
    }
}

impl InMemoryEnvironmentImageBuildStore {
    fn transition(
        &self,
        build_key: &str,
        now_ms: u64,
        transition: impl FnOnce(&EnvironmentImageBuildState) -> Option<EnvironmentImageBuildState>,
    ) -> Result<bool, EnvironmentImageBuildError> {
        let mut records = self.records.lock().expect("image build store");
        let Some(record) = records.get_mut(build_key) else {
            return Ok(false);
        };
        let Some(next) = transition(&record.state) else {
            return Ok(false);
        };
        record.state = next;
        record.updated_at_ms = now_ms;
        Ok(true)
    }
}
