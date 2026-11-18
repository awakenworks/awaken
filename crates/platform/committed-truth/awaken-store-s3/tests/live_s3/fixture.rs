#![allow(dead_code)]
use awaken_lease_testkit::ManualClock;
use awaken_store_s3::{Limits, S3Storage, SessionKey, SessionStorage};
use futures::{StreamExt, TryStreamExt};
use object_store::{ObjectStore, ObjectStoreExt, path::Path};
use std::sync::Arc;

pub(super) struct Fixture {
    pub(super) store: Arc<dyn ObjectStore>,
    pub(super) clock: Arc<ManualClock>,
    pub(super) engine: S3Storage,
    pub(super) session: SessionStorage,
    pub(super) prefix: String,
}
pub(super) fn endpoint() -> String {
    std::env::var("AWAKEN_TEST_S3_ENDPOINT")
        .expect("a live S3 endpoint is required; no memory fallback")
}
pub(super) fn connect(endpoint: &str) -> Arc<dyn ObjectStore> {
    let bucket = std::env::var("AWAKEN_TEST_S3_BUCKET").expect("task-owned S3 test bucket");
    let region = std::env::var("AWAKEN_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    awaken_object_storage::s3_transport(
        &bucket,
        Some(&region),
        Some(endpoint),
        endpoint.starts_with("http://"),
    )
    .expect("live S3 transport")
}
pub(super) fn open(
    store: Arc<dyn ObjectStore>,
    prefix: &str,
    now: u64,
    limits: Limits,
) -> (Arc<ManualClock>, S3Storage, SessionStorage) {
    let clock = Arc::new(ManualClock::new(now));
    let engine = S3Storage::new(store, prefix, clock.clone(), limits).unwrap();
    let session = engine.session(SessionKey::new("tenant", "session").unwrap());
    (clock, engine, session)
}
impl Fixture {
    pub(super) async fn new(limits: Limits) -> Self {
        let prefix = format!("awaken-live-{}", uuid::Uuid::new_v4());
        let store = connect(&endpoint());
        let (clock, engine, session) = open(store.clone(), &prefix, 1_000, limits);
        session.create().await.unwrap();
        Self {
            store,
            clock,
            engine,
            session,
            prefix,
        }
    }
    pub(super) fn cold(
        &self,
        now: u64,
        limits: Limits,
    ) -> (Arc<ManualClock>, S3Storage, SessionStorage) {
        open(connect(&endpoint()), &self.prefix, now, limits)
    }
    pub(super) fn head_path(&self) -> Path {
        Path::from(format!(
            "{}/v1/scopes/tenant/sessions/session/head.json",
            self.prefix
        ))
    }
    pub(super) async fn head_bytes(&self) -> bytes::Bytes {
        self.store
            .get(&self.head_path())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
    }
    pub(super) async fn cleanup(&self) {
        let prefix = format!("{}/", self.prefix);
        let objects = self
            .store
            .list(Some(&Path::from(self.prefix.clone())))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        // Check the whole deletion set before deleting anything.
        assert!(
            objects
                .iter()
                .all(|object| object.location.as_ref().starts_with(&prefix))
        );
        futures::stream::iter(objects)
            .map(|object| {
                let store = self.store.clone();
                async move { store.delete(&object.location).await }
            })
            .buffer_unordered(16)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
    }
}
pub(super) fn size(name: &str, default: usize, minimum: usize, maximum: usize) -> usize {
    let value = std::env::var(name)
        .map(|value| value.parse().expect("integer load setting"))
        .unwrap_or(default);
    assert!(
        (minimum..=maximum).contains(&value),
        "{name} outside supported test bounds"
    );
    value
}
pub(super) fn bytes(producer: u64, ordinal: u64, length: usize) -> bytes::Bytes {
    let mut state = producer.wrapping_mul(0x9e3779b97f4a7c15) ^ ordinal ^ 0xdeadbeef;
    bytes::Bytes::from(
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state & 255) as u8
            })
            .collect::<Vec<_>>(),
    )
}
