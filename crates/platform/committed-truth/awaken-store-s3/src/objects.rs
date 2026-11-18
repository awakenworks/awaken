// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Performs conditional head updates and validates referenced immutable objects.
use crate::{
    Error, Result, SessionStorage,
    model::{Head, Limits, ObjectRef, SessionKey},
};
use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion, path::Path};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::broadcast;

#[derive(Default)]
struct Counts {
    get: AtomicU64,
    put: AtomicU64,
    list: AtomicU64,
    delete: AtomicU64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequestCounts {
    pub get: u64,
    pub put: u64,
    pub list: u64,
    pub delete: u64,
}
#[derive(Default)]
struct Cache {
    bytes: usize,
    data: BTreeMap<String, Bytes>,
    order: VecDeque<String>,
}

pub(crate) struct Inner {
    pub store: Arc<dyn ObjectStore>,
    pub prefix: String,
    pub clock: Arc<dyn awaken_time::WallClock>,
    pub limits: Limits,
    counts: Counts,
    pub namespace: tokio::sync::OnceCell<String>,
    cache: awaken_sync::InvariantLock<Cache>,
    channels: awaken_sync::InvariantLock<BTreeMap<String, broadcast::Sender<u64>>>,
}
#[derive(Clone)]
pub struct S3Storage {
    pub(crate) inner: Arc<Inner>,
}

impl S3Storage {
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.inner.clock.clone()
    }

    /// Recovery discovery is internal to this engine and stays inside one authorized scope.
    pub async fn discover_sessions(&self, scope: &str) -> Result<Vec<SessionKey>> {
        use futures::TryStreamExt;
        let validated = SessionKey::new(scope, "_")?;
        let prefix = format!("v1/scopes/{}/sessions/", validated.scope);
        let path = self.inner.path(&prefix);
        self.inner.count_list();
        let objects = self
            .inner
            .store
            .list(Some(&path))
            .try_collect::<Vec<_>>()
            .await
            .map_err(storage_error)?;
        let mut sessions = Vec::new();
        for object in objects {
            let Some(relative) = self.inner.relative_key(object.location.as_ref()) else {
                continue;
            };
            let Some(suffix) = relative.strip_prefix(&prefix) else {
                continue;
            };
            let Some(session) = suffix.strip_suffix("/head.json") else {
                continue;
            };
            if !session.contains('/') {
                sessions.push(SessionKey::new(scope, session)?);
            }
        }
        sessions.sort_by(|a, b| a.session.cmp(&b.session));
        sessions.dedup();
        Ok(sessions)
    }
    pub fn new(
        store: Arc<dyn ObjectStore>,
        prefix: &str,
        clock: Arc<dyn awaken_time::WallClock>,
        limits: Limits,
    ) -> Result<Self> {
        if prefix.split('/').any(|part| part == "." || part == "..") || prefix.starts_with('/') {
            return Err(Error::Invalid("invalid object prefix".into()));
        }
        if limits.max_cas_attempts == 0
            || limits.recent_commits == 0
            || limits.max_head_bytes == 0
            || limits.max_record_bytes == 0
            || limits.read_ahead_bytes == 0
            || limits.page_snapshot_ttl_ms == 0
        {
            return Err(Error::Invalid("storage limits must be positive".into()));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                prefix: prefix.trim_matches('/').into(),
                clock,
                limits,
                counts: Counts::default(),
                namespace: tokio::sync::OnceCell::new(),
                cache: awaken_sync::InvariantLock::new(Cache::default()),
                channels: awaken_sync::InvariantLock::new(BTreeMap::new()),
            }),
        })
    }
    pub fn session(&self, key: SessionKey) -> SessionStorage {
        SessionStorage {
            inner: self.inner.clone(),
            key,
        }
    }
    pub fn request_counts(&self) -> RequestCounts {
        let c = &self.inner.counts;
        RequestCounts {
            get: c.get.load(Ordering::Relaxed),
            put: c.put.load(Ordering::Relaxed),
            list: c.list.load(Ordering::Relaxed),
            delete: c.delete.load(Ordering::Relaxed),
        }
    }
}

impl Inner {
    pub(crate) fn count_list(&self) {
        self.counts.list.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn count_delete(&self) {
        self.counts.delete.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn relative_key<'a>(&self, key: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            Some(key)
        } else {
            key.strip_prefix(&format!("{}/", self.prefix))
        }
    }
    pub(crate) fn path(&self, key: &str) -> Path {
        Path::from(if self.prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{}/{key}", self.prefix)
        })
    }
    pub(crate) async fn load_head(&self, key: &SessionKey) -> Result<(Head, UpdateVersion)> {
        self.counts.get.fetch_add(1, Ordering::Relaxed);
        let response = self
            .store
            .get(&self.path(&format!("{}/head.json", key.root())))
            .await
            .map_err(|e| match e {
                object_store::Error::NotFound { .. } => Error::NotFound,
                other => Error::Storage(other.to_string()),
            })?;
        let version = UpdateVersion {
            e_tag: response.meta.e_tag.clone(),
            version: response.meta.version.clone(),
        };
        if version.e_tag.is_none() {
            return Err(Error::Corrupt("backend omitted ETag".into()));
        }
        let bytes = response.bytes().await.map_err(storage_error)?;
        if bytes.len() > self.limits.max_head_bytes {
            return Err(Error::Corrupt("oversized head".into()));
        }
        let head: Head = serde_json::from_slice(&bytes).map_err(corrupt)?;
        if head.format != 1 || &head.key != key {
            return Err(Error::Corrupt("head identity or format mismatch".into()));
        }
        if head.deleted {
            return Err(Error::NotFound);
        }
        for (id, row) in &head.runs {
            if !head.threads.contains_key(&row.thread)
                || row.claim.as_ref().is_some_and(|claim| {
                    claim.run_id != *id
                        || claim.owner.is_empty()
                        || Some(claim.epoch) != row.last_epoch
                        || row.phase != crate::RunPhase::Running
                })
                || row
                    .physical_attempt
                    .as_ref()
                    .is_some_and(|claim| claim.run_id.0 != *id)
            {
                return Err(Error::Corrupt(
                    "invalid claim or Thread binding in head".into(),
                ));
            }
        }
        Ok((head, version))
    }
    pub(crate) async fn publish(
        &self,
        head: &mut Head,
        version: Option<UpdateVersion>,
    ) -> Result<bool> {
        head.generation = crate::model::next(head.generation)?;
        let body = serde_json::to_vec(head).map_err(corrupt)?;
        if body.len() > self.limits.max_head_bytes {
            return Err(Error::Capacity("Session head size".into()));
        }
        self.counts.put.fetch_add(1, Ordering::Relaxed);
        let mode = version.map_or(PutMode::Create, PutMode::Update);
        let result = self
            .store
            .put_opts(
                &self.path(&format!("{}/head.json", head.key.root())),
                Bytes::from(body).into(),
                PutOptions {
                    mode,
                    ..Default::default()
                },
            )
            .await;
        match result {
            Ok(_) => {
                self.notify(&head.key, head.commit_sequence);
                Ok(true)
            }
            Err(
                object_store::Error::Precondition { .. }
                | object_store::Error::AlreadyExists { .. },
            ) => Ok(false),
            Err(error) => Err(Error::OutcomeUnknown(error.to_string())),
        }
    }
    pub(crate) async fn put_bytes(
        &self,
        key: &SessionKey,
        generation: u64,
        folder: &str,
        bytes: Bytes,
    ) -> Result<ObjectRef> {
        if bytes.len() > self.limits.max_record_bytes {
            return Err(Error::Capacity("object size".into()));
        }
        let hash = digest(&bytes);
        let reference = ObjectRef {
            key: format!("{}/{folder}/{generation}-{hash}", key.root()),
            sha256: hash,
            offset: 0,
            length: bytes.len() as u64,
            generation,
        };
        self.counts.put.fetch_add(1, Ordering::Relaxed);
        match self
            .store
            .put_opts(
                &self.path(&reference.key),
                bytes.clone().into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => {
                self.cache_frame(&reference, &bytes)?;
                Ok(reference)
            }
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => {
                self.read_bytes(key, &reference).await?;
                Ok(reference)
            }
            Err(e) => Err(Error::Storage(e.to_string())),
        }
    }
    pub(crate) async fn put_json<T: Serialize>(
        &self,
        key: &SessionKey,
        generation: u64,
        folder: &str,
        value: &T,
    ) -> Result<ObjectRef> {
        let mut raw = serde_json::to_vec(value).map_err(corrupt)?;
        raw.push(b'\n');
        if raw.len() > self.limits.max_record_bytes {
            return Err(Error::Capacity("decoded object size".into()));
        }
        let encoded = zstd::stream::encode_all(raw.as_slice(), 1).map_err(corrupt)?;
        self.put_bytes(key, generation, folder, Bytes::from(encoded))
            .await
    }
    pub(crate) fn validate_reference(&self, key: &SessionKey, reference: &ObjectRef) -> Result<()> {
        let prefix = format!("{}/", key.root());
        let relative = reference.key.strip_prefix(&prefix);
        let valid = relative
            .and_then(|part| part.split_once('/'))
            .is_some_and(|(folder, file)| {
                [
                    "segments",
                    "indexes",
                    "views",
                    "streams",
                    "blobs",
                    "packs",
                    "components",
                ]
                .contains(&folder)
                    && file.split_once('-').is_some_and(|(epoch, hash)| {
                        epoch.parse::<u64>().ok() == Some(reference.generation)
                            && hash.len() == 64
                            && hash.bytes().all(|b| b.is_ascii_hexdigit())
                    })
            });
        if !valid
            || reference.length > self.limits.max_record_bytes as u64
            || reference.offset.checked_add(reference.length).is_none()
        {
            return Err(Error::Invalid(
                "object reference is outside Session or bounds".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn cached(&self, reference: &ObjectRef) -> Result<bool> {
        Ok(self
            .cache
            .lock()
            .map_err(corrupt)?
            .data
            .contains_key(&reference.sha256))
    }
    pub(crate) async fn read_range(&self, key: &str, range: std::ops::Range<u64>) -> Result<Bytes> {
        self.counts.get.fetch_add(1, Ordering::Relaxed);
        self.store
            .get_range(&self.path(key), range)
            .await
            .map_err(storage_error)
    }
    pub(crate) async fn read_bytes(
        &self,
        key: &SessionKey,
        reference: &ObjectRef,
    ) -> Result<Bytes> {
        self.validate_reference(key, reference)?;
        let cached = self
            .cache
            .lock()
            .map_err(corrupt)?
            .data
            .get(&reference.sha256)
            .cloned();
        if let Some(bytes) = cached {
            if bytes.len() as u64 != reference.length {
                return Err(Error::Corrupt("cached frame length mismatch".into()));
            }
            return Ok(bytes);
        }
        if reference.length == 0 {
            let empty = Bytes::new();
            self.cache_frame(reference, &empty)?;
            return Ok(empty);
        }
        self.counts.get.fetch_add(1, Ordering::Relaxed);
        let end = reference
            .offset
            .checked_add(reference.length)
            .ok_or_else(|| Error::Corrupt("object range overflow".into()))?;
        let bytes = self
            .store
            .get_range(&self.path(&reference.key), reference.offset..end)
            .await
            .map_err(storage_error)?;
        self.cache_frame(reference, &bytes)?;
        Ok(bytes)
    }
    pub(crate) fn cache_frame(&self, reference: &ObjectRef, bytes: &Bytes) -> Result<()> {
        if bytes.len() as u64 != reference.length || digest(bytes) != reference.sha256 {
            return Err(Error::Corrupt("object digest or length mismatch".into()));
        }
        let mut cache = self.cache.lock().map_err(corrupt)?;
        if bytes.len() <= self.limits.cache_bytes && !cache.data.contains_key(&reference.sha256) {
            while cache.bytes.saturating_add(bytes.len()) > self.limits.cache_bytes {
                let Some(old) = cache.order.pop_front() else {
                    break;
                };
                if let Some(removed) = cache.data.remove(&old) {
                    cache.bytes -= removed.len();
                }
            }
            cache.bytes += bytes.len();
            cache.order.push_back(reference.sha256.clone());
            cache.data.insert(reference.sha256.clone(), bytes.clone());
        }
        Ok(())
    }
    pub(crate) async fn read_json<T: DeserializeOwned>(
        &self,
        key: &SessionKey,
        reference: &ObjectRef,
    ) -> Result<T> {
        use std::io::Read;
        let bytes = self.read_bytes(key, reference).await?;
        let reader = zstd::stream::read::Decoder::new(bytes.as_ref()).map_err(corrupt)?;
        let mut decoded = Vec::new();
        reader
            .take(self.limits.max_record_bytes as u64 + 1)
            .read_to_end(&mut decoded)
            .map_err(corrupt)?;
        if decoded.len() > self.limits.max_record_bytes {
            return Err(Error::Corrupt("decoded object limit".into()));
        }
        serde_json::from_slice(&decoded).map_err(corrupt)
    }
    pub(crate) fn channel(&self, key: &SessionKey) -> Result<broadcast::Sender<u64>> {
        let mut channels = self.channels.lock().map_err(corrupt)?;
        Ok(channels
            .entry(key.root())
            .or_insert_with(|| broadcast::channel(256).0)
            .clone())
    }
    fn notify(&self, key: &SessionKey, sequence: u64) {
        if let Ok(sender) = self.channel(key) {
            let _ = sender.send(sequence);
        }
    }
}
pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(crate) fn corrupt(error: impl std::fmt::Display) -> Error {
    Error::Corrupt(error.to_string())
}
pub(crate) fn storage_error(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
