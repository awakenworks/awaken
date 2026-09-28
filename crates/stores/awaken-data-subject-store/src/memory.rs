// @formal entrypoint data_subject.stale_writer_never_applies
// @formal evidence erasure.concurrent_replicas_have_one_checkpoint_winner
use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_data_subject_application::{
    DataSubject, DataSubjectError, DataSubjectId, DataSubjectRepo, ErasureJobRepo, ErasureProgress,
};

/// Volatile data-subject adapter for tests and explicitly enabled fixtures only.
#[cfg(any(test, feature = "test-support"))]
#[derive(Default)]
pub struct InMemoryDataSubjectRepo {
    inner: Mutex<BTreeMap<String, DataSubject>>,
    order: Mutex<Vec<String>>,
    erasures: Mutex<BTreeMap<String, ErasureProgress>>,
}

impl InMemoryDataSubjectRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl DataSubjectRepo for InMemoryDataSubjectRepo {
    async fn create(&self, subject: DataSubject) -> Result<(), DataSubjectError> {
        let key = subject.id.0.clone();
        let mut map = self.inner.lock().expect("data-subject memory store");
        if map.contains_key(&key) {
            return Err(DataSubjectError::AlreadyExists(key));
        }
        self.order
            .lock()
            .expect("data-subject memory order")
            .push(key.clone());
        map.insert(key, subject);
        Ok(())
    }

    async fn compare_and_swap(
        &self,
        expected_revision: u64,
        subject: DataSubject,
    ) -> Result<(), DataSubjectError> {
        let key = subject.id.0.clone();
        if !subject.is_successor_of(expected_revision) {
            return Err(DataSubjectError::Conflict(key));
        }
        let mut map = self.inner.lock().expect("data-subject memory store");
        match map.get(&key) {
            Some(current) if subject.follows_revision(expected_revision, current.revision) => {
                map.insert(key, subject);
                Ok(())
            }
            _ => Err(DataSubjectError::Conflict(key)),
        }
    }

    async fn get(&self, id: &DataSubjectId) -> Result<DataSubject, DataSubjectError> {
        self.inner
            .lock()
            .expect("data-subject memory store")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| DataSubjectError::NotFound(id.0.clone()))
    }

    async fn list(&self, org: &str) -> Result<Vec<DataSubject>, DataSubjectError> {
        let map = self.inner.lock().expect("data-subject memory store");
        Ok(self
            .order
            .lock()
            .expect("data-subject memory order")
            .iter()
            .filter_map(|key| map.get(key))
            .filter(|subject| subject.org == org)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl ErasureJobRepo for InMemoryDataSubjectRepo {
    async fn load(&self, id: &DataSubjectId) -> Result<Option<ErasureProgress>, DataSubjectError> {
        Ok(self
            .erasures
            .lock()
            .expect("data-subject memory erasures")
            .get(&id.0)
            .cloned())
    }

    async fn compare_and_swap_progress(
        &self,
        id: &DataSubjectId,
        expected_revision: Option<u64>,
        progress: &ErasureProgress,
    ) -> Result<(), DataSubjectError> {
        let mut erasures = self.erasures.lock().expect("data-subject memory erasures");
        if !progress.follows(expected_revision, erasures.get(&id.0)) {
            return Err(DataSubjectError::Conflict(id.0.clone()));
        }
        erasures.insert(id.0.clone(), progress.clone());
        Ok(())
    }
}
