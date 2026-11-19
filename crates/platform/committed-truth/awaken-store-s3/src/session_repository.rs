// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Adapts tenant-scoped Session repository operations to conditional object publication.
//! Component persistence in the shared Session head. Identity routes never grant writes.
use crate::{
    Error, ObjectRef, Result, S3Storage, SessionKey, SessionStorage,
    model::{ClaimCandidateRef, Head, Record},
    objects::{digest, storage_error},
};
use awaken_object_storage::PublicationError;
use awaken_object_storage::{
    RunnableSessions, SessionObject, SessionObjectStore, SessionObjectUpdate,
};
use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt};
use object_store::{ObjectStoreExt, PutMode, PutOptions, UpdateVersion, path::Path};
use std::collections::BTreeMap;
const MANAGED: &str = "managed_session";
const PREPARED_CANDIDATE_GRACE_MS: u64 = 60_000;

#[derive(serde::Serialize, serde::Deserialize)]
struct ClaimCandidate {
    key: SessionKey,
    base_generation: u64,
    prepared_at_ms: u64,
    due_at_ms: u64,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Condition {
    format: u32,
    issuer: String,
    key: SessionKey,
    component: String,
    marker: String,
    value: String,
}
impl crate::objects::Inner {
    pub(crate) async fn storage_namespace(&self) -> Result<String> {
        let inner = self;
        let identity = inner
            .namespace
            .get_or_try_init(|| async {
                let path = inner.path("v1/identity.json");
                #[derive(serde::Serialize, serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Identity {
                    format: u32,
                    id: uuid::Uuid,
                }
                // Create is immutable; lost acknowledgements reconcile by reading
                // the winner rather than issuing another logical namespace.
                match inner.store.get(&path).await {
                    Ok(response) => {
                        let identity: Identity =
                            serde_json::from_slice(&response.bytes().await.map_err(storage_error)?)
                                .map_err(crate::objects::corrupt)?;
                        if identity.format != 1 {
                            return Err(Error::Corrupt("unsupported storage identity".into()));
                        }
                        return Ok(identity.id.to_string());
                    }
                    Err(object_store::Error::NotFound { .. }) => {}
                    Err(error) => return Err(storage_error(error)),
                }
                let candidate = Identity {
                    format: 1,
                    id: uuid::Uuid::new_v4(),
                };
                let bytes =
                    Bytes::from(serde_json::to_vec(&candidate).map_err(crate::objects::corrupt)?);
                let write = inner
                    .store
                    .put_opts(
                        &path,
                        bytes.into(),
                        PutOptions {
                            mode: PutMode::Create,
                            ..Default::default()
                        },
                    )
                    .await;
                if write.is_ok() {
                    return Ok(candidate.id.to_string());
                }
                let response = inner.store.get(&path).await.map_err(storage_error)?;
                let winner: Identity =
                    serde_json::from_slice(&response.bytes().await.map_err(storage_error)?)
                        .map_err(crate::objects::corrupt)?;
                if winner.format != 1 {
                    return Err(Error::Corrupt("unsupported storage identity".into()));
                }
                Ok::<_, Error>(winner.id.to_string())
            })
            .await?;
        Ok(identity.clone())
    }
    pub(crate) async fn validate_claim_condition(
        &self,
        head: &Head,
        claim: &awaken_worker_contract::RunClaim,
        condition: &awaken_session_types::SessionPublicationCondition,
        allow_cancelled: bool,
    ) -> Result<()> {
        let token: Condition = serde_json::from_str(condition.token())
            .map_err(|_| Error::Invalid("invalid claim condition".into()))?;
        let active = format!(
            "commit:{}:{}:{}:active",
            claim.run_id.0, claim.owner, claim.epoch
        );
        let cancelled = format!(
            "commit:{}:{}:{}:cancelled",
            claim.run_id.0, claim.owner, claim.epoch
        );
        if token.component != "dispatch"
            || (token.marker != active && !(allow_cancelled && token.marker == cancelled))
        {
            return Err(Error::Fenced);
        }
        self.validate_publication_condition(head, condition).await
    }
    pub(crate) async fn validate_publication_condition(
        &self,
        head: &Head,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<()> {
        use awaken_time::WallClockExt as _;
        let token: Condition = serde_json::from_str(condition.token())
            .map_err(|_| Error::Invalid("invalid publication condition".into()))?;
        if token.format != 2
            || token.issuer != self.storage_namespace().await?
            || token.key != head.key
        {
            return Err(Error::Fenced);
        }
        let current = head
            .component_markers
            .get(&token.component)
            .and_then(|markers| markers.get(&token.marker))
            .ok_or(Error::Fenced)?;
        if current.value != token.value || self.clock.now_ms() >= current.expires_ms {
            return Err(Error::Fenced);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct S3ComponentStore {
    storage: S3Storage,
    component: String,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredRecord {
    kind: String,
    hash: String,
    body: ObjectRef,
}
type RecordIndex = BTreeMap<String, StoredRecord>;
impl S3Storage {
    fn managed_component(&self) -> S3ComponentStore {
        S3ComponentStore {
            storage: self.clone(),
            component: MANAGED.into(),
        }
    }
    pub fn component_store(&self, name: &str) -> Result<S3ComponentStore> {
        if name.is_empty()
            || name.len() > 64
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(Error::Invalid("invalid storage component".into()));
        }
        Ok(S3ComponentStore {
            storage: self.clone(),
            component: name.into(),
        })
    }
}
impl S3ComponentStore {
    fn route_prefix(&self) -> String {
        "v1/routing/sessions/".into()
    }
    fn route(&self, id: &str) -> object_store::path::Path {
        self.storage
            .inner
            .path(&format!("{}{}", self.route_prefix(), digest(id.as_bytes())))
    }
    async fn key(&self, id: &str) -> Result<Option<SessionKey>> {
        match self.storage.inner.store.get(&self.route(id)).await {
            Ok(response) => {
                let key: SessionKey =
                    serde_json::from_slice(&response.bytes().await.map_err(storage_error)?)
                        .map_err(crate::objects::corrupt)?;
                if key.session_id() != id {
                    return Err(Error::Corrupt("component routing identity mismatch".into()));
                }
                Ok(Some(key))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(storage_error(error)),
        }
    }
    async fn register(&self, key: &SessionKey) -> Result<bool> {
        let bytes = serde_json::to_vec(key).map_err(crate::objects::corrupt)?;
        match self
            .storage
            .inner
            .store
            .put_opts(
                &self.route(key.session_id()),
                Bytes::from(bytes).into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(
                object_store::Error::Precondition { .. }
                | object_store::Error::AlreadyExists { .. },
            ) => Ok(self.key(key.session_id()).await?.as_ref() == Some(key)),
            Err(error) => Err(Error::OutcomeUnknown(error.to_string())),
        }
    }
    async fn snapshot(&self, key: SessionKey) -> Result<Option<SessionObject>> {
        let (head, version) = match self.storage.inner.load_head(&key).await {
            Ok(value) => value,
            Err(Error::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        self.snapshot_at(key, head, version).await.map(Some)
    }

    async fn snapshot_at(
        &self,
        key: SessionKey,
        head: Head,
        version: UpdateVersion,
    ) -> Result<SessionObject> {
        let payload = match head.components.get(&self.component) {
            Some(component) => {
                let payload: serde_json::Value =
                    self.storage.inner.read_json(&key, component).await?;
                Some(serde_json::to_string(&payload).map_err(crate::objects::corrupt)?)
            }
            None => None,
        };
        let issuer = self.storage.inner.storage_namespace().await?;
        Ok(SessionObject {
            session_id: key.session_id().into(),
            owner_scope: key.scope().into(),
            version: version
                .e_tag
                .ok_or_else(|| Error::Corrupt("head ETag absent".into()))?,
            payload,
            conditions: head
                .component_markers
                .get(&self.component)
                .into_iter()
                .flatten()
                .map(|(marker, value)| {
                    let token = serde_json::to_string(&Condition {
                        format: 2,
                        issuer: issuer.clone(),
                        key: key.clone(),
                        component: self.component.clone(),
                        marker: marker.clone(),
                        value: value.value.clone(),
                    })
                    .map_err(crate::objects::corrupt)?;
                    Ok((marker.clone(), token))
                })
                .collect::<Result<_>>()?,
        })
    }
    fn record_route(&self, id: &str) -> object_store::path::Path {
        self.storage.inner.path(&format!(
            "v1/routing/records/{}/{}",
            self.component,
            digest(id.as_bytes())
        ))
    }
    async fn read_record_route(&self, id: &str) -> Result<Option<SessionKey>> {
        self.read_route(&self.record_route(id)).await
    }

    async fn read_route(&self, path: &Path) -> Result<Option<SessionKey>> {
        let response = match self.storage.inner.store.get(path).await {
            Ok(response) => response,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(storage_error(error)),
        };
        serde_json::from_slice(&response.bytes().await.map_err(storage_error)?)
            .map(Some)
            .map_err(crate::objects::corrupt)
    }

    async fn register_record(&self, key: &SessionKey, id: &str) -> Result<()> {
        let bytes = Bytes::from(serde_json::to_vec(key).map_err(crate::objects::corrupt)?);
        match self
            .storage
            .inner
            .store
            .put_opts(
                &self.record_route(id),
                bytes.into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(
                object_store::Error::AlreadyExists { .. }
                | object_store::Error::Precondition { .. },
            ) => {
                let stored = self
                    .storage
                    .inner
                    .store
                    .get(&self.record_route(id))
                    .await
                    .map_err(storage_error)?;
                let stored: SessionKey =
                    serde_json::from_slice(&stored.bytes().await.map_err(storage_error)?)
                        .map_err(crate::objects::corrupt)?;
                if &stored == key {
                    Ok(())
                } else {
                    Err(Error::Identity(
                        "immutable identity belongs to another Session".into(),
                    ))
                }
            }
            Err(error) => Err(Error::OutcomeUnknown(error.to_string())),
        }
    }
    fn candidate_prefix(&self) -> String {
        format!("v1/routing/runnable-v2/{}/", self.component)
    }

    fn candidate_index_marker(&self) -> Path {
        self.storage
            .inner
            .path(&format!("v1/routing/runnable-v2-ready/{}", self.component))
    }

    async fn prepare_candidate(&self, head: &mut Head, due_at_ms: Option<u64>) -> Result<()> {
        use awaken_time::WallClockExt as _;
        let Some(due_at_ms) = due_at_ms else {
            head.claim_candidates.remove(&self.component);
            return Ok(());
        };
        let now_ms = self.storage.inner.clock.now_ms();
        if head
            .claim_candidates
            .get(&self.component)
            .is_some_and(|present| {
                present.path.starts_with(&self.candidate_prefix())
                    && (present.due_at_ms == due_at_ms
                        || (present.due_at_ms > now_ms && present.due_at_ms < due_at_ms))
            })
        {
            // The early marker is intentionally retained across normal renewals.
            return Ok(());
        }
        let shard =
            awaken_dispatch_contract::dispatch_shard(head.key.scope(), head.key.session_id());
        let path = format!(
            "{}{shard:02x}/{due_at_ms:020}-{}",
            self.candidate_prefix(),
            uuid::Uuid::new_v4()
        );
        let marker = ClaimCandidate {
            key: head.key.clone(),
            base_generation: head.generation,
            prepared_at_ms: now_ms,
            due_at_ms,
        };
        let body = serde_json::to_vec(&marker).map_err(crate::objects::corrupt)?;
        self.storage
            .inner
            .store
            .put_opts(
                &self.storage.inner.path(&path),
                Bytes::from(body).into(),
                PutOptions {
                    mode: PutMode::Create,
                    ..Default::default()
                },
            )
            .await
            .map_err(storage_error)?;
        head.claim_candidates.insert(
            self.component.clone(),
            ClaimCandidateRef { path, due_at_ms },
        );
        Ok(())
    }

    async fn publish_update(
        &self,
        head: &mut Head,
        version: UpdateVersion,
        update: &SessionObjectUpdate,
    ) -> Result<bool> {
        let previous = head
            .claim_candidates
            .get(&self.component)
            .map(|item| item.path.clone());
        self.prepare(head, update).await?;
        self.prepare_candidate(head, update.next_claim_at_ms)
            .await?;
        let retired = previous.filter(|previous| {
            head.claim_candidates
                .get(&self.component)
                .is_none_or(|current| &current.path != previous)
        });
        let applied = self.storage.inner.publish(head, Some(version)).await?;
        if applied && let Some(previous) = retired {
            // A later publication cannot refer to a retired marker identity.
            // A dropped acknowledgement is repaired by the candidate scanner.
            let _ = self
                .storage
                .inner
                .store
                .delete(&self.storage.inner.path(&previous))
                .await;
        }
        Ok(applied)
    }

    async fn read_candidate(
        &self,
        location: Path,
        due_at_ms: u64,
    ) -> Result<Option<SessionObject>> {
        use awaken_time::WallClockExt as _;
        let inner = &self.storage.inner;
        let response = match inner.store.get(&location).await {
            Ok(response) => response,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(storage_error(error)),
        };
        let candidate: ClaimCandidate =
            serde_json::from_slice(&response.bytes().await.map_err(storage_error)?)
                .map_err(crate::objects::corrupt)?;
        let (mut head, version) = inner.load_head(&candidate.key).await?;
        if head
            .claim_candidates
            .get(&self.component)
            .is_some_and(|current| inner.path(&current.path) == location)
        {
            return self
                .snapshot_at(candidate.key, head, version)
                .await
                .map(Some);
        }
        // No successful publisher from base_generation can subsequently win
        // once the head moves. A crashed preparer may leave that exact version
        // behind; fence it before removing its only discovery record.
        if head.generation > candidate.base_generation
            || (head.generation == candidate.base_generation
                && inner
                    .clock
                    .now_ms()
                    .min(due_at_ms)
                    .saturating_sub(candidate.prepared_at_ms)
                    >= PREPARED_CANDIDATE_GRACE_MS
                && inner.publish(&mut head, Some(version)).await?)
        {
            let _ = inner.store.delete(&location).await;
        }
        Ok(None)
    }

    async fn prepare(&self, head: &mut Head, update: &SessionObjectUpdate) -> Result<()> {
        if let Some(token) = &update.condition {
            let condition = awaken_session_types::SessionPublicationCondition::new(token.clone())
                .map_err(Error::Invalid)?;
            self.storage
                .inner
                .validate_publication_condition(head, &condition)
                .await?;
        }
        if !update.markers.is_empty() {
            self.storage.inner.storage_namespace().await?;
        }
        let markers = update
            .markers
            .iter()
            .map(|(id, value)| {
                (
                    id.clone(),
                    awaken_object_storage::PublicationMarker {
                        value: digest(value.value.as_bytes()),
                        expires_ms: value.expires_ms,
                    },
                )
            })
            .collect();
        head.component_markers
            .insert(self.component.clone(), markers);
        let payload: serde_json::Value =
            serde_json::from_str(&update.payload).map_err(crate::objects::corrupt)?;
        let inner = &self.storage.inner;
        let key = &head.key;
        if !update.records.is_empty() {
            let mut records: RecordIndex = match head.records.get(&self.component) {
                Some(reference) => inner.read_json(key, reference).await?,
                None => BTreeMap::new(),
            };
            for record in &update.records {
                self.register_record(key, &record.id).await?;
                if record.id.is_empty() || record.kind.is_empty() {
                    return Err(Error::Invalid("empty immutable record identity".into()));
                }
                let value: serde_json::Value =
                    serde_json::from_str(&record.payload).map_err(crate::objects::corrupt)?;
                let hash = digest(&serde_json::to_vec(&value).map_err(crate::objects::corrupt)?);
                if let Some(old) = records.get(&record.id) {
                    if old.kind != record.kind || old.hash != hash {
                        return Err(Error::Identity("immutable component record reused".into()));
                    }
                    continue;
                }
                let body = inner
                    .put_json(
                        key,
                        head.gc_generation,
                        "segments",
                        &Record::ComponentEvent {
                            component: self.component.clone(),
                            id: record.id.clone(),
                            kind: record.kind.clone(),
                            value,
                        },
                    )
                    .await?;
                records.insert(
                    record.id.clone(),
                    StoredRecord {
                        kind: record.kind.clone(),
                        hash,
                        body,
                    },
                );
            }
            let index = inner
                .put_json(key, head.gc_generation, "indexes", &records)
                .await?;
            head.records.insert(self.component.clone(), index);
        }
        let object = inner
            .put_json(key, head.gc_generation, "components", &payload)
            .await?;
        head.components.insert(self.component.clone(), object);
        Ok(())
    }
}
#[async_trait::async_trait]
impl SessionObjectStore for S3ComponentStore {
    async fn load(&self, id: &str) -> std::result::Result<Option<SessionObject>, PublicationError> {
        let Some(key) = self.key(id).await.map_err(repository_error)? else {
            return Ok(None);
        };
        self.snapshot(key).await.map_err(repository_error)
    }
    async fn insert(
        &self,
        owner: &str,
        id: &str,
        update: &SessionObjectUpdate,
    ) -> std::result::Result<bool, PublicationError> {
        let key = SessionKey::new(owner, id).map_err(repository_error)?;
        if !self.register(&key).await.map_err(repository_error)? {
            return Err(PublicationError::AlreadyExists);
        }
        self.storage
            .session(key.clone())
            .create()
            .await
            .map_err(repository_error)?;
        let (mut head, version) = self
            .storage
            .inner
            .load_head(&key)
            .await
            .map_err(repository_error)?;
        if head.components.contains_key(&self.component) {
            return Ok(false);
        }
        self.publish_update(&mut head, version, update)
            .await
            .map_err(repository_error)
    }
    async fn replace(
        &self,
        observed: &SessionObject,
        update: &SessionObjectUpdate,
    ) -> std::result::Result<bool, PublicationError> {
        let key = SessionKey::new(&observed.owner_scope, &observed.session_id)
            .map_err(repository_error)?;
        let (mut head, version) = self
            .storage
            .inner
            .load_head(&key)
            .await
            .map_err(repository_error)?;
        if version.e_tag.as_deref() != Some(observed.version.as_str()) {
            return Ok(false);
        }
        if head.components.contains_key(&self.component) != observed.payload.is_some() {
            return Err(PublicationError::Corrupt(
                "component existence changed without a version change".into(),
            ));
        }
        self.publish_update(&mut head, version, update)
            .await
            .map_err(repository_error)
    }
    async fn validate_condition(
        &self,
        id: &str,
        token: &str,
        version: Option<&str>,
    ) -> std::result::Result<bool, PublicationError> {
        let condition = awaken_session_types::SessionPublicationCondition::new(token.into())
            .map_err(PublicationError::InvalidMutation)?;
        let token: Condition = serde_json::from_str(token)
            .map_err(|error| PublicationError::InvalidMutation(error.to_string()))?;
        if token.key.session_id() != id {
            return Err(PublicationError::NotApplied(
                "condition targets another Session".into(),
            ));
        }
        let (head, observed) = self
            .storage
            .inner
            .load_head(&token.key)
            .await
            .map_err(repository_error)?;
        self.storage
            .inner
            .validate_publication_condition(&head, &condition)
            .await
            .map_err(repository_error)?;
        Ok(version.is_none_or(|expected| observed.e_tag.as_deref() == Some(expected)))
    }

    async fn scan_namespace(&self) -> std::result::Result<String, PublicationError> {
        let identity = self
            .storage
            .inner
            .storage_namespace()
            .await
            .map_err(repository_error)?;
        Ok(format!("{identity}:{}", self.component))
    }
    async fn claim_request_partition(
        &self,
        identity: &str,
        expires_at_ms: u64,
        proposed: Option<&SessionObject>,
    ) -> std::result::Result<Option<SessionObject>, PublicationError> {
        self.resolve_claim_request(identity, expires_at_ms, proposed)
            .await
            .map_err(repository_error)
    }

    async fn claim_request_cutoff(&self) -> std::result::Result<u64, PublicationError> {
        self.claim_cutoff()
            .await
            .map(|(cutoff, _)| cutoff)
            .map_err(repository_error)
    }

    async fn locate_record(
        &self,
        id: &str,
    ) -> std::result::Result<Option<SessionObject>, PublicationError> {
        let Some(key) = self.read_record_route(id).await.map_err(repository_error)? else {
            return Ok(None);
        };
        let (head, _) = match self.storage.inner.load_head(&key).await {
            Ok(head) => head,
            Err(Error::NotFound) => return Ok(None),
            Err(error) => return Err(repository_error(error)),
        };
        let Some(index) = head.records.get(&self.component) else {
            return Ok(None);
        };
        let index: RecordIndex = self
            .storage
            .inner
            .read_json(&key, index)
            .await
            .map_err(repository_error)?;
        if !index.contains_key(id) {
            return Ok(None);
        }
        self.snapshot(key).await.map_err(repository_error)
    }
    async fn claim_index_initialized(&self) -> std::result::Result<bool, PublicationError> {
        match self
            .storage
            .inner
            .store
            .head(&self.candidate_index_marker())
            .await
        {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(repository_error(storage_error(error))),
        }
    }

    async fn finish_claim_index_backfill(&self) -> std::result::Result<(), PublicationError> {
        self.storage
            .inner
            .store
            .put(
                &self.candidate_index_marker(),
                Bytes::from_static(b"1").into(),
            )
            .await
            .map(|_| ())
            .map_err(|error| repository_error(storage_error(error)))
    }

    async fn claimable_shards(
        &self,
        _due_at_ms: u64,
    ) -> std::result::Result<Option<Vec<u8>>, PublicationError> {
        let inner = &self.storage.inner;
        let prefix = inner.path(&self.candidate_prefix());
        // The top-level survey returns at most the 256 shard prefixes,
        // independent of how many Sessions or leases each shard contains.
        let page = inner
            .store
            .list_with_delimiter(Some(&prefix))
            .await
            .map_err(|error| repository_error(storage_error(error)))?;
        if !page.objects.is_empty() {
            return Err(PublicationError::Corrupt(
                "candidate without a shard".into(),
            ));
        }
        page.common_prefixes
            .into_iter()
            .map(|path| {
                let relative = path
                    .prefix_match(&prefix)
                    .and_then(|mut parts| parts.next())
                    .ok_or_else(|| PublicationError::Corrupt("candidate shard path".into()))?;
                u8::from_str_radix(relative.as_ref(), 16)
                    .map_err(|error| PublicationError::Corrupt(error.to_string()))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Some)
    }

    async fn scan_runnable_sessions(
        &self,
        due_at_ms: u64,
        shard: Option<u8>,
        after: Option<&str>,
        limit: usize,
    ) -> std::result::Result<RunnableSessions, PublicationError> {
        let inner = &self.storage.inner;
        let prefix = inner.path(&match shard {
            Some(shard) => format!("{}{shard:02x}/", self.candidate_prefix()),
            None => self.candidate_prefix(),
        });
        let mut listing = match after {
            Some(after) => {
                let after = Path::parse(after)
                    .map_err(|e| PublicationError::InvalidMutation(e.to_string()))?;
                if after.prefix_match(&prefix).is_none() {
                    return Err(PublicationError::InvalidMutation(
                        "discovery offset outside its shard".into(),
                    ));
                }
                inner.store.list_with_offset(Some(&prefix), &after)
            }
            None => inner.store.list(Some(&prefix)),
        };
        let limit = limit.clamp(1, 64);
        let deadline = awaken_time::mono::now() + std::time::Duration::from_millis(750);
        let mut keys = Vec::with_capacity(limit);
        let mut complete = false;
        for _ in 0..limit {
            match tokio::time::timeout_at(deadline, listing.try_next()).await {
                Ok(Ok(Some(object))) => keys.push(object.location),
                Ok(Ok(None)) => {
                    complete = true;
                    break;
                }
                Ok(Err(error)) => return Err(repository_error(storage_error(error))),
                Err(_) => break,
            }
        }
        let mut reads = futures::stream::iter(keys.into_iter().map(|location| async move {
            let result = if candidate_deadline(&location)? <= due_at_ms {
                self.read_candidate(location.clone(), due_at_ms)
                    .await
                    .map_err(repository_error)?
            } else {
                None
            };
            Ok::<_, PublicationError>((location, result))
        }))
        .buffered(8);
        let mut sessions = BTreeMap::new();
        let mut resume_after = None;
        loop {
            match tokio::time::timeout_at(deadline, reads.next()).await {
                Ok(Some(Ok((location, result)))) => {
                    if let Some(object) = result {
                        sessions.insert(
                            (object.owner_scope.clone(), object.session_id.clone()),
                            object,
                        );
                    }
                    resume_after = Some(location.to_string());
                }
                Ok(Some(Err(error))) => return Err(error),
                Ok(None) => break,
                Err(_) => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete && resume_after.is_none() {
            return Err(PublicationError::AuthorityUnavailable(
                "Dispatch discovery deadline reached before making progress".into(),
            ));
        }
        Ok(RunnableSessions {
            sessions: sessions.into_values().collect(),
            resume_after: if complete { None } else { resume_after },
        })
    }

    async fn list(&self) -> std::result::Result<Vec<SessionObject>, PublicationError> {
        let prefix = self.storage.inner.path(&self.route_prefix());
        let objects = self
            .storage
            .inner
            .store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .map_err(|e| PublicationError::AuthorityUnavailable(e.to_string()))?;
        let mut result = Vec::new();
        for object in objects {
            if !object.location.as_ref().starts_with(prefix.as_ref()) {
                return Err(PublicationError::Corrupt(
                    "routing list escaped namespace".into(),
                ));
            }
            let response = self
                .storage
                .inner
                .store
                .get(&object.location)
                .await
                .map_err(|e| PublicationError::AuthorityUnavailable(e.to_string()))?;
            let key: SessionKey = serde_json::from_slice(
                &response
                    .bytes()
                    .await
                    .map_err(|e| PublicationError::AuthorityUnavailable(e.to_string()))?,
            )
            .map_err(|e| PublicationError::Corrupt(e.to_string()))?;
            if self.route(key.session_id()) != object.location {
                return Err(PublicationError::Corrupt(
                    "component route key mismatch".into(),
                ));
            }
            if let Some(snapshot) = self.snapshot(key).await.map_err(repository_error)?
                && snapshot.payload.is_some()
            {
                result.push(snapshot);
            }
        }
        Ok(result)
    }
}
#[async_trait::async_trait]
impl SessionObjectStore for S3Storage {
    async fn load(&self, id: &str) -> std::result::Result<Option<SessionObject>, PublicationError> {
        self.managed_component().load(id).await
    }
    async fn insert(
        &self,
        owner: &str,
        id: &str,
        update: &SessionObjectUpdate,
    ) -> std::result::Result<bool, PublicationError> {
        self.managed_component().insert(owner, id, update).await
    }
    async fn replace(
        &self,
        observed: &SessionObject,
        update: &SessionObjectUpdate,
    ) -> std::result::Result<bool, PublicationError> {
        self.managed_component().replace(observed, update).await
    }
    async fn list(&self) -> std::result::Result<Vec<SessionObject>, PublicationError> {
        self.managed_component().list().await
    }
}
impl SessionStorage {
    pub async fn component_record(
        &self,
        component: &str,
        id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let (head, _) = self.inner.load_head(&self.key).await?;
        self.component_record_at(&head, component, id).await
    }
    pub(crate) async fn component_record_at(
        &self,
        head: &Head,
        component: &str,
        id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let Some(index) = head.records.get(component) else {
            return Ok(None);
        };
        let index: RecordIndex = self.inner.read_json(&self.key, index).await?;
        let Some(record) = index.get(id) else {
            return Ok(None);
        };
        match self.inner.read_json(&self.key, &record.body).await? {
            Record::ComponentEvent {
                component: stored,
                id: stored_id,
                kind,
                value,
            } if stored == component
                && stored_id == id
                && kind == record.kind
                && digest(&serde_json::to_vec(&value).map_err(crate::objects::corrupt)?)
                    == record.hash =>
            {
                Ok(Some(value))
            }
            _ => Err(Error::Corrupt(
                "immutable component record identity mismatch".into(),
            )),
        }
    }
    pub(crate) async fn component_record_roots(&self, head: &Head) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        for index in head.records.values() {
            keys.push(index.key.clone());
            let index: RecordIndex = self.inner.read_json(&self.key, index).await?;
            keys.extend(index.into_values().map(|record| record.body.key));
        }
        Ok(keys)
    }
}
fn candidate_deadline(path: &Path) -> std::result::Result<u64, PublicationError> {
    let file = path
        .filename()
        .ok_or_else(|| PublicationError::Corrupt("candidate path".into()))?;
    let (deadline, _) = file
        .split_once('-')
        .ok_or_else(|| PublicationError::Corrupt("candidate deadline".into()))?;
    deadline
        .parse()
        .map_err(|error: std::num::ParseIntError| PublicationError::Corrupt(error.to_string()))
}

fn repository_error(error: Error) -> PublicationError {
    match error {
        Error::Corrupt(detail) => PublicationError::Corrupt(detail),
        Error::Invalid(detail) | Error::Identity(detail) => {
            PublicationError::InvalidMutation(detail)
        }
        Error::Capacity(detail) | Error::Conflict(detail) => PublicationError::NotApplied(detail),
        Error::Fenced => PublicationError::NotApplied("publication condition is stale".into()),
        other => PublicationError::AuthorityUnavailable(other.to_string()),
    }
}

mod claim_routes;
