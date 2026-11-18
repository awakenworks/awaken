// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Reads immutable committed history and frozen views within the owning Session.
use crate::{
    Error, ObjectRef, ReadCursor, Result, SessionStorage, Subscription,
    model::{FrozenView, Head, Record, ThreadHead, ViewPart, next, snapshot_key},
};
use awaken_agent_contract::{
    agent::{message::Message, thread::Id as ThreadId},
    thread::read::transcript::{
        TranscriptPage, TranscriptPageRequest, TranscriptRange, TranscriptSnapshotRef,
        TranscriptView,
    },
};
use awaken_time::WallClockExt as _;
use std::collections::BTreeMap;

impl SessionStorage {
    pub(crate) async fn prefetch(&self, references: &[ObjectRef]) -> Result<()> {
        self.inner.prefetch(&self.key, references).await
    }

    pub(crate) async fn raw_parts(&self, head: &Head, thread: &str) -> Result<Vec<ViewPart>> {
        let Some(state) = head.threads.get(thread) else {
            return if thread == self.key.session {
                Ok(Vec::new())
            } else {
                Err(Error::NotFound)
            };
        };
        let mut parts = if let Some(base) = &state.base {
            self.inner
                .read_json::<FrozenView>(&self.key, base)
                .await?
                .parts
        } else {
            Vec::new()
        };
        for descriptor in self.descriptors(head).await? {
            if descriptor.thread == thread && descriptor.message_end > descriptor.message_start {
                parts.push(ViewPart {
                    body: descriptor.body,
                    start: 0,
                    end: descriptor.message_end - descriptor.message_start,
                });
            }
        }
        if parts_len(&parts)? != state.message_end {
            return Err(Error::Corrupt("Thread transcript length mismatch".into()));
        }
        Ok(parts)
    }
    pub(crate) async fn view_parts(
        &self,
        head: &Head,
        thread: &str,
        view: TranscriptView,
    ) -> Result<Vec<ViewPart>> {
        let raw = self.raw_parts(head, thread).await?;
        let Some(state) = head.threads.get(thread) else {
            return Ok(raw);
        };
        if view == TranscriptView::ContextEffective
            && let Some(effective) = &state.effective
        {
            let mut parts = self
                .inner
                .read_json::<FrozenView>(&self.key, effective)
                .await?
                .parts;
            parts.extend(select_parts(&raw, state.effective_from, state.message_end)?);
            return Ok(parts);
        }
        Ok(raw)
    }

    /// Pin an immutable logical view. Physical packing never changes its identity.
    pub async fn freeze(
        &self,
        thread: &ThreadId,
        view: TranscriptView,
    ) -> Result<TranscriptSnapshotRef> {
        self.freeze_view(thread, view, None).await
    }
    async fn freeze_view(
        &self,
        thread: &ThreadId,
        view: TranscriptView,
        ttl_ms: Option<u64>,
    ) -> Result<TranscriptSnapshotRef> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            let now = self.inner.clock.now_ms();
            expire_snapshots(&mut head, now);
            let parts = self.view_parts(&head, &thread.0, view).await?;
            let expiry = ttl_ms
                .map(|ttl| {
                    now.checked_add(ttl)
                        .ok_or_else(|| Error::Capacity("snapshot expiry exhausted".into()))
                })
                .transpose()?;
            if let Some(expiry) = expiry {
                let mut reuse = None;
                for id in head.snapshot_deadlines.keys() {
                    let object = head
                        .snapshots
                        .get(id)
                        .ok_or_else(|| Error::Corrupt("snapshot deadline has no pin".into()))?;
                    let frozen: FrozenView = self.inner.read_json(&self.key, object).await?;
                    if frozen.reference.thread_id == *thread
                        && frozen.reference.view == view
                        && frozen.parts == parts
                    {
                        reuse = Some(frozen.reference);
                        break;
                    }
                }
                if let Some(reference) = reuse {
                    head.snapshot_deadlines
                        .insert(snapshot_key(&reference), expiry);
                    if self.publish_head(&mut head, version).await? {
                        return Ok(reference);
                    }
                    continue;
                }
            }
            let reference = TranscriptSnapshotRef {
                thread_id: thread.clone(),
                view,
                version: next(head.generation)?,
                end_seq: parts_len(&parts)?,
            };
            let frozen = FrozenView {
                reference: reference.clone(),
                parts,
            };
            let object = self
                .inner
                .put_json(&self.key, head.gc_generation, "views", &frozen)
                .await?;
            head.snapshots.insert(snapshot_key(&reference), object);
            if let Some(expiry) = expiry {
                head.snapshot_deadlines
                    .insert(snapshot_key(&reference), expiry);
            }
            if self.publish_head(&mut head, version).await? {
                return Ok(reference);
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    pub async fn release_snapshot(&self, reference: &TranscriptSnapshotRef) -> Result<()> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            if head.snapshots.remove(&snapshot_key(reference)).is_none() {
                return Ok(());
            }
            head.snapshot_deadlines.remove(&snapshot_key(reference));
            if self.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
    pub(crate) async fn frozen(
        &self,
        head: &Head,
        reference: &TranscriptSnapshotRef,
    ) -> Result<(FrozenView, ObjectRef)> {
        if head
            .snapshot_deadlines
            .get(&snapshot_key(reference))
            .is_some_and(|deadline| self.inner.clock.now_ms() >= *deadline)
        {
            return Err(Error::Invalid("page snapshot expired".into()));
        }
        let object = head
            .snapshots
            .get(&snapshot_key(reference))
            .ok_or_else(|| Error::Invalid("snapshot unavailable in this Session".into()))?;
        let frozen: FrozenView = self.inner.read_json(&self.key, object).await?;
        if &frozen.reference != reference || parts_len(&frozen.parts)? != reference.end_seq {
            return Err(Error::Corrupt("frozen transcript identity mismatch".into()));
        }
        Ok((frozen, object.clone()))
    }

    pub async fn read_page(&self, request: TranscriptPageRequest) -> Result<TranscriptPage> {
        let reference = match request.snapshot {
            Some(ref reference)
                if reference.thread_id == request.thread_id && reference.view == request.view =>
            {
                reference.clone()
            }
            Some(_) => return Err(Error::Invalid("snapshot Thread or view mismatch".into())),
            None => {
                self.freeze_view(
                    &request.thread_id,
                    request.view,
                    Some(self.inner.limits.page_snapshot_ttl_ms),
                )
                .await?
            }
        };
        let (head, _) = self.inner.load_head(&self.key).await?;
        let (frozen, _) = self.frozen(&head, &reference).await?;
        if request.start > reference.end_seq {
            return Err(Error::Invalid("page starts after snapshot end".into()));
        }
        let end = request
            .start
            .saturating_add(request.limit.get() as u64)
            .min(reference.end_seq);
        let selected = select_parts(&frozen.parts, request.start, end)?;
        // Prefetch complete nearby frames from the same object; later API pages hit cache.
        let mut read_ahead = selected.iter().map(|p| p.body.clone()).collect::<Vec<_>>();
        if let Some(last) = read_ahead.last().cloned() {
            let bound = last
                .offset
                .saturating_add(last.length)
                .saturating_add(self.inner.limits.read_ahead_bytes);
            for part in &frozen.parts {
                if part.body.key == last.key
                    && part.body.offset > last.offset
                    && part.body.offset.saturating_add(part.body.length) <= bound
                {
                    read_ahead.push(part.body.clone());
                }
            }
        }
        self.prefetch(&read_ahead).await?;
        let messages = self.materialize(&selected).await?;
        Ok(TranscriptPage {
            snapshot: reference,
            range: TranscriptRange::new(request.start, end),
            messages,
        })
    }
    pub(crate) async fn materialize(&self, parts: &[ViewPart]) -> Result<Vec<Message>> {
        let mut messages = Vec::new();
        for part in parts {
            let all = match self.inner.read_json(&self.key, &part.body).await? {
                Record::StepCommitted(operation) => operation.commit.messages,
                Record::Transcript(messages) => messages,
                _ => return Err(Error::Corrupt("non-transcript frame in view".into())),
            };
            if part.start > part.end || part.end > all.len() as u64 {
                return Err(Error::Corrupt("view frame slice outside transcript".into()));
            }
            messages.extend_from_slice(&all[part.start as usize..part.end as usize]);
        }
        Ok(messages)
    }

    /// Same-Session nested forks share flattened immutable frame references.
    pub async fn fork(&self, source: &TranscriptSnapshotRef, destination: ThreadId) -> Result<()> {
        if destination.0.is_empty() {
            return Err(Error::Invalid("blank destination Thread".into()));
        }
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            let (_, object) = self.frozen(&head, source).await?;
            if let Some(existing) = head.threads.get(&destination.0) {
                return if existing.base.as_ref() == Some(&object) {
                    Ok(())
                } else {
                    Err(Error::Conflict("destination Thread exists".into()))
                };
            }
            if head.archived {
                return Err(Error::Conflict("Session archived".into()));
            }
            if head.threads.len() >= self.inner.limits.max_threads {
                return Err(Error::Capacity("Thread limit".into()));
            }
            let mut thread = ThreadHead::empty();
            thread.base = Some(object);
            thread.message_end = source.end_seq;
            head.threads.insert(destination.0.clone(), thread);
            if self.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    /// Rollback changes the effective view and keeps all raw committed history.
    pub async fn rollback(&self, thread: &ThreadId, target: &TranscriptSnapshotRef) -> Result<()> {
        if &target.thread_id != thread {
            return Err(Error::Invalid(
                "rollback requires this Thread's snapshot".into(),
            ));
        }
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            let (_, object) = self.frozen(&head, target).await?;
            if head
                .component_markers
                .get("dispatch")
                .is_some_and(|markers| {
                    markers.contains_key(&format!("history-blocked:{}", thread.0))
                })
                || head.runs.values().any(|r| {
                    r.thread == thread.0 && (r.claim.is_some() || r.physical_attempt.is_some())
                })
            {
                return Err(Error::Conflict("Thread has an active writer".into()));
            }
            let state = head.threads.get_mut(&thread.0).ok_or(Error::NotFound)?;
            state.effective = Some(object);
            state.effective_from = state.message_end;
            if self.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    /// Attach before loading the watermark. Notifications are hints; the head is truth.
    pub async fn subscribe(&self, cursor: Option<ReadCursor>) -> Result<Subscription> {
        let after = match cursor {
            Some(cursor) if cursor.key == self.key => cursor.after,
            Some(_) => return Err(Error::Invalid("subscription scope mismatch".into())),
            None => 0,
        };
        let receiver = self.inner.channel(&self.key)?.subscribe();
        let (head, _) = self.inner.load_head(&self.key).await?;
        if after > head.commit_sequence {
            return Err(Error::Invalid("subscription cursor ahead of head".into()));
        }
        Ok(Subscription {
            key: self.key.clone(),
            after,
            receiver,
        })
    }
    pub async fn next_commits(
        &self,
        subscription: &mut Subscription,
        limit: usize,
    ) -> Result<Vec<crate::CommitDescriptor>> {
        if subscription.key != self.key {
            return Err(Error::Invalid("subscription scope mismatch".into()));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        loop {
            let (head, _) = self.inner.load_head(&self.key).await?;
            let records = self
                .descriptors(&head)
                .await?
                .into_iter()
                .filter(|r| r.receipt.commit_sequence > subscription.after)
                .take(limit)
                .collect::<Vec<_>>();
            if let Some(last) = records.last() {
                subscription.after = last.receipt.commit_sequence;
                return Ok(records);
            }
            // Cross-process wake loss and broadcast lag are repaired by polling.
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                subscription.receiver.recv(),
            )
            .await;
        }
    }
}
impl Subscription {
    pub fn cursor(&self) -> ReadCursor {
        ReadCursor {
            key: self.key.clone(),
            after: self.after,
        }
    }
}
pub(crate) fn parts_len(parts: &[ViewPart]) -> Result<u64> {
    parts.iter().try_fold(0_u64, |sum, p| {
        sum.checked_add(
            p.end
                .checked_sub(p.start)
                .ok_or_else(|| Error::Corrupt("reversed view part".into()))?,
        )
        .ok_or_else(|| Error::Capacity("transcript length".into()))
    })
}
pub(crate) fn select_parts(parts: &[ViewPart], start: u64, end: u64) -> Result<Vec<ViewPart>> {
    if start > end || end > parts_len(parts)? {
        return Err(Error::Invalid("transcript range outside snapshot".into()));
    }
    let mut offset = 0_u64;
    let mut selected = Vec::new();
    for part in parts {
        let length = part.end - part.start;
        let next = offset + length;
        if start < next && end > offset {
            selected.push(ViewPart {
                body: part.body.clone(),
                start: part.start + start.saturating_sub(offset),
                end: part.start + (end.min(next) - offset),
            });
        }
        offset = next;
        if offset >= end {
            break;
        }
    }
    Ok(selected)
}

// Coalesce only spans within one immutable S3 object. Every frame is verified separately.
impl crate::objects::Inner {
    pub(crate) async fn prefetch(
        &self,
        key: &crate::SessionKey,
        references: &[ObjectRef],
    ) -> Result<()> {
        let mut objects: BTreeMap<String, Vec<ObjectRef>> = BTreeMap::new();
        let mut budget = self.limits.cache_bytes as u64;
        for reference in references {
            self.validate_reference(key, reference)?;
            if !self.cached(reference)? && reference.length <= budget {
                budget -= reference.length;
                objects
                    .entry(reference.key.clone())
                    .or_default()
                    .push(reference.clone());
            }
        }
        for (path, mut refs) in objects {
            refs.sort_by_key(|r| r.offset);
            refs.dedup();
            let mut begin = 0;
            while begin < refs.len() {
                let mut end = begin + 1;
                let start_offset = refs[begin].offset;
                let mut end_offset = start_offset + refs[begin].length;
                while end < refs.len() {
                    let candidate = refs[end].offset + refs[end].length;
                    if refs[end].offset > end_offset
                        || candidate - start_offset
                            > self.limits.read_ahead_bytes.max(refs[begin].length)
                    {
                        break;
                    }
                    end_offset = end_offset.max(candidate);
                    end += 1;
                }
                let data = self.read_range(&path, start_offset..end_offset).await?;
                for reference in &refs[begin..end] {
                    let start = (reference.offset - start_offset) as usize;
                    let end = start + reference.length as usize;
                    if end > data.len() {
                        return Err(Error::Corrupt("short object range".into()));
                    }
                    self.cache_frame(reference, &data.slice(start..end))?;
                }
                begin = end;
            }
        }
        Ok(())
    }
}

impl SessionStorage {
    /// Publish a compacted effective context. Runtime authors the summary; storage preserves raw truth.
    pub async fn compact(
        &self,
        thread: &ThreadId,
        messages: Vec<Message>,
    ) -> Result<TranscriptSnapshotRef> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            if head.archived {
                return Err(Error::Conflict("Session archived".into()));
            }
            if head
                .component_markers
                .get("dispatch")
                .is_some_and(|markers| {
                    markers.contains_key(&format!("history-blocked:{}", thread.0))
                })
                || head.runs.values().any(|r| {
                    r.thread == thread.0 && (r.claim.is_some() || r.physical_attempt.is_some())
                })
            {
                return Err(Error::Conflict("Thread has an active writer".into()));
            }
            let raw_end = head
                .threads
                .get(&thread.0)
                .ok_or(Error::NotFound)?
                .message_end;
            let body = self
                .inner
                .put_json(
                    &self.key,
                    head.gc_generation,
                    "segments",
                    &Record::Transcript(messages.clone()),
                )
                .await?;
            let reference = TranscriptSnapshotRef {
                thread_id: thread.clone(),
                view: TranscriptView::ContextEffective,
                version: next(head.generation)?,
                end_seq: messages.len() as u64,
            };
            let frozen = FrozenView {
                reference: reference.clone(),
                parts: vec![ViewPart {
                    body,
                    start: 0,
                    end: messages.len() as u64,
                }],
            };
            let object = self
                .inner
                .put_json(&self.key, head.gc_generation, "views", &frozen)
                .await?;
            head.snapshots
                .insert(snapshot_key(&reference), object.clone());
            let state = head.threads.get_mut(&thread.0).ok_or(Error::NotFound)?;
            state.effective = Some(object);
            state.effective_from = raw_end;
            if self.publish_head(&mut head, version).await? {
                return Ok(reference);
            }
        }
        Err(Error::Storage("Session busy".into()))
    }

    /// Cross-Session copy materializes an explicitly pinned source; no cross-partition GC roots.
    pub async fn copy_from(
        &self,
        source: &SessionStorage,
        snapshot: &TranscriptSnapshotRef,
        destination: ThreadId,
    ) -> Result<()> {
        if source.key.scope != self.key.scope {
            return Err(Error::Invalid(
                "cross-scope copy requires an application export/import".into(),
            ));
        }
        if destination.0.is_empty() {
            return Err(Error::Invalid("blank destination Thread".into()));
        }
        let (source_head, _) = source.inner.load_head(&source.key).await?;
        let (frozen, _) = source.frozen(&source_head, snapshot).await?;
        let messages = source.materialize(&frozen.parts).await?;
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            if head.archived {
                return Err(Error::Conflict("Session archived".into()));
            }
            if head.threads.contains_key(&destination.0) {
                return Err(Error::Conflict("destination Thread exists".into()));
            }
            if head.threads.len() >= self.inner.limits.max_threads {
                return Err(Error::Capacity("Thread limit".into()));
            }
            let mut parts = Vec::new();
            for chunk in messages.chunks(256) {
                let body = self
                    .inner
                    .put_json(
                        &self.key,
                        head.gc_generation,
                        "segments",
                        &Record::Transcript(chunk.to_vec()),
                    )
                    .await?;
                parts.push(ViewPart {
                    body,
                    start: 0,
                    end: chunk.len() as u64,
                });
            }
            let reference = TranscriptSnapshotRef {
                thread_id: destination.clone(),
                view: snapshot.view,
                version: next(head.generation)?,
                end_seq: snapshot.end_seq,
            };
            let object = self
                .inner
                .put_json(
                    &self.key,
                    head.gc_generation,
                    "views",
                    &FrozenView { reference, parts },
                )
                .await?;
            let mut thread = ThreadHead::empty();
            thread.base = Some(object);
            thread.message_end = snapshot.end_seq;
            head.threads.insert(destination.0.clone(), thread);
            if self.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
}

/// Page cursors are leased read pins; explicit freezes remain caller-owned.
/// Inherited fork roots retain their immutable objects independently.
pub(crate) fn expire_snapshots(head: &mut Head, now_ms: u64) {
    let expired = head
        .snapshot_deadlines
        .iter()
        .filter(|(_, deadline)| now_ms >= **deadline)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in expired {
        head.snapshot_deadlines.remove(&id);
        head.snapshots.remove(&id);
    }
}
