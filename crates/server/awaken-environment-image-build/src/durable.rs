use async_trait::async_trait;
use awaken_environment_realization_contract::{
    EnvironmentImageBuildClaim, EnvironmentImageBuildDemand, EnvironmentImageBuildError,
    EnvironmentImageBuildRecord, EnvironmentImageBuildState, EnvironmentImageBuildStore,
};

#[derive(Clone)]
pub(crate) struct VersionedRecord {
    pub(crate) version: u64,
    pub(crate) record: EnvironmentImageBuildRecord,
}

impl VersionedRecord {
    pub(crate) fn decode(
        version: i64,
        demand_json: &str,
        state_json: &str,
        updated_at_ms: i64,
    ) -> Result<Self, EnvironmentImageBuildError> {
        Ok(Self {
            version: u64::try_from(version).map_err(storage)?,
            record: EnvironmentImageBuildRecord {
                demand: serde_json::from_str(demand_json).map_err(storage)?,
                state: serde_json::from_str(state_json).map_err(storage)?,
                updated_at_ms: u64::try_from(updated_at_ms).map_err(storage)?,
            },
        })
    }

    pub(crate) fn encode_record(
        record: &EnvironmentImageBuildRecord,
    ) -> Result<(String, String, i64), EnvironmentImageBuildError> {
        Ok((
            serde_json::to_string(&record.demand).map_err(storage)?,
            serde_json::to_string(&record.state).map_err(storage)?,
            i64::try_from(record.updated_at_ms).map_err(storage)?,
        ))
    }
}

#[async_trait]
pub(crate) trait RecordBackend: Send + Sync {
    async fn insert(
        &self,
        record: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError>;

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<VersionedRecord>, EnvironmentImageBuildError>;

    async fn candidates(&self) -> Result<Vec<VersionedRecord>, EnvironmentImageBuildError>;

    async fn compare_and_swap(
        &self,
        expected: &VersionedRecord,
        next: &EnvironmentImageBuildRecord,
    ) -> Result<bool, EnvironmentImageBuildError>;
}

pub(crate) struct DurableEnvironmentImageBuildStore<B> {
    backend: B,
}

impl<B> DurableEnvironmentImageBuildStore<B> {
    pub(crate) fn new(backend: B) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl<B: RecordBackend> EnvironmentImageBuildStore for DurableEnvironmentImageBuildStore<B> {
    async fn ensure(
        &self,
        demand: EnvironmentImageBuildDemand,
        now_ms: u64,
    ) -> Result<(), EnvironmentImageBuildError> {
        let record = EnvironmentImageBuildRecord {
            demand: demand.clone(),
            state: EnvironmentImageBuildState::default(),
            updated_at_ms: now_ms,
        };
        if self.backend.insert(&record).await? {
            return Ok(());
        }
        match self.backend.get(&demand.build_key).await? {
            Some(existing) if existing.record.demand.same_recipe(&demand) => Ok(()),
            Some(_) => Err(EnvironmentImageBuildError::Conflict(demand.build_key)),
            None => Err(EnvironmentImageBuildError::Storage(
                "image-build job disappeared after insert conflict".into(),
            )),
        }
    }

    async fn get(
        &self,
        build_key: &str,
    ) -> Result<Option<EnvironmentImageBuildRecord>, EnvironmentImageBuildError> {
        Ok(self.backend.get(build_key).await?.map(|value| value.record))
    }

    async fn claim_next(
        &self,
        owner: &str,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<EnvironmentImageBuildClaim>, EnvironmentImageBuildError> {
        for current in self.backend.candidates().await? {
            let Some(state) = current.record.state.claim(owner, now_ms, lease_ms) else {
                continue;
            };
            let EnvironmentImageBuildState::Building { lease_epoch, .. } = state else {
                unreachable!("claim transition returns Building")
            };
            let next = EnvironmentImageBuildRecord {
                demand: current.record.demand.clone(),
                state,
                updated_at_ms: now_ms,
            };
            if self.backend.compare_and_swap(&current, &next).await? {
                return Ok(Some(EnvironmentImageBuildClaim {
                    demand: next.demand,
                    owner: owner.to_owned(),
                    lease_epoch,
                }));
            }
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
        .await
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
        .await
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
        .await
    }
}

impl<B: RecordBackend> DurableEnvironmentImageBuildStore<B> {
    async fn transition(
        &self,
        build_key: &str,
        now_ms: u64,
        transition: impl Fn(&EnvironmentImageBuildState) -> Option<EnvironmentImageBuildState>,
    ) -> Result<bool, EnvironmentImageBuildError> {
        loop {
            let Some(current) = self.backend.get(build_key).await? else {
                return Ok(false);
            };
            let Some(state) = transition(&current.record.state) else {
                return Ok(false);
            };
            let next = EnvironmentImageBuildRecord {
                demand: current.record.demand.clone(),
                state,
                updated_at_ms: now_ms,
            };
            if self.backend.compare_and_swap(&current, &next).await? {
                return Ok(true);
            }
        }
    }
}

pub(crate) fn storage(error: impl std::fmt::Display) -> EnvironmentImageBuildError {
    EnvironmentImageBuildError::Storage(error.to_string())
}
