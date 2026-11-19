// @formal entrypoint store_history.s3_sealed_collection_preserves_reachable_objects
use crate::{
    Error, ObjectRef, Result, SessionStorage,
    model::{CommitDescriptor, FrozenView, Head, IndexRef, next},
    objects::storage_error,
};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::ObjectStoreExt;
use std::collections::BTreeSet;

/// Whether a listed object may be swept: an object the sealed mark reached
/// (`reachable`) or a head is never swept, and an object is swept only when its
/// known generation is at or before the sealed cutoff. Root membership arrives
/// as a predicate so the rule is the same for whatever the mark phase holds.
fn sweep_candidate(
    generation: Option<u64>,
    cutoff: u64,
    reachable: impl Fn(&str) -> bool,
    path: &str,
) -> bool {
    !reachable(path)
        && !path.ends_with("/head.json")
        && generation.is_some_and(|generation| generation <= cutoff)
}

pub(crate) fn blob_pin_admitted(
    generation: u64,
    open_generation: u64,
    already_pinned: impl FnOnce() -> bool,
) -> bool {
    generation == open_generation || already_pinned()
}

#[cfg(kani)]
/// @obligation store_history.s3_sealed_collection_preserves_reachable_objects
#[kani::proof]
fn sweep_spares_marked_objects_and_admitted_blob_pins() {
    let generation: u64 = kani::any();
    let known: bool = kani::any();
    let cutoff: u64 = kani::any();
    let open_generation: u64 = kani::any();
    let marked: bool = kani::any();
    let is_head: bool = kani::any();

    // Each branch hands production a concrete listed path, so the head rule
    // and root membership are derived by production code on that path while
    // only the generation arithmetic and the mark stay symbolic. Membership
    // is the predicate production builds over the sealed root set, here
    // marking exactly that object or nothing.
    let sweep = |path: &'static str| {
        sweep_candidate(
            known.then_some(generation),
            cutoff,
            |candidate| marked && candidate == path,
            path,
        )
    };
    let delete = if is_head {
        sweep("session/head.json")
    } else {
        sweep("session/record")
    };
    if !known || marked || is_head || generation > cutoff {
        assert!(!delete);
    } else {
        assert!(delete);
    }

    let pin = blob_pin_admitted(generation, open_generation, || marked);
    assert_eq!(pin, generation == open_generation || marked);
    if pin && open_generation > cutoff {
        assert!(!delete);
    }
}

#[cfg(test)]
mod sweep_mapping_tests {
    use super::*;

    #[test]
    fn sweep_candidate_reads_the_sealed_root_set_and_the_head_suffix() {
        // The production adapter over the sealed BTreeSet: every (marked,
        // path shape, generation side of the cutoff) combination.
        for path in [
            "session/record",
            "session/head.json",
            "a/b/head.json",
            "head.json.bak",
        ] {
            for marked in [false, true] {
                let roots = if marked {
                    BTreeSet::from([path.to_string(), "other".to_string()])
                } else {
                    BTreeSet::from(["other".to_string()])
                };
                for generation in [None, Some(4), Some(5), Some(6)] {
                    let expected = !marked
                        && !path.ends_with("/head.json")
                        && generation.is_some_and(|generation| generation <= 5);
                    assert_eq!(
                        sweep_candidate(generation, 5, |candidate| roots.contains(candidate), path),
                        expected,
                        "{path} marked={marked} generation={generation:?}",
                    );
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GcReport {
    pub sealed_generation: u64,
    pub examined: u64,
    pub deleted: u64,
}
impl SessionStorage {
    /// Relocate immutable compressed frames; receipts and logical chain stay unchanged.
    pub async fn pack(&self) -> Result<usize> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            let mut descriptors = self.descriptors(&head).await?;
            if descriptors.is_empty() {
                return Ok(0);
            }
            let mut packed = 0_usize;
            let mut begin = 0;
            while begin < descriptors.len() {
                let mut data = Vec::new();
                let mut end = begin;
                while end < descriptors.len() {
                    let bytes = self
                        .inner
                        .read_bytes(&self.key, &descriptors[end].body)
                        .await?;
                    if !data.is_empty()
                        && data.len().saturating_add(bytes.len())
                            > self.inner.limits.max_record_bytes
                    {
                        break;
                    }
                    data.extend_from_slice(&bytes);
                    end += 1;
                }
                let object = self
                    .inner
                    .put_bytes(&self.key, head.gc_generation, "packs", Bytes::from(data))
                    .await?;
                let mut offset = 0;
                for descriptor in &mut descriptors[begin..end] {
                    descriptor.body = ObjectRef {
                        key: object.key.clone(),
                        offset,
                        length: descriptor.body.length,
                        sha256: descriptor.body.sha256.clone(),
                        generation: object.generation,
                    };
                    offset += descriptor.body.length;
                    packed += 1;
                }
                begin = end;
            }
            let index = self.write_index(&head, &descriptors).await?;
            head.indexes = vec![index];
            head.recent.clear();
            if self.publish_head(&mut head, version).await? {
                return Ok(packed);
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
    pub(crate) async fn write_index(
        &self,
        head: &Head,
        records: &[CommitDescriptor],
    ) -> Result<IndexRef> {
        let mut threads = std::collections::BTreeMap::new();
        for record in records {
            threads
                .entry(record.thread.clone())
                .and_modify(|range: &mut (u64, u64)| range.1 = record.message_end)
                .or_insert((record.message_start, record.message_end));
        }
        let first = records
            .first()
            .ok_or_else(|| Error::Invalid("empty index".into()))?
            .receipt
            .commit_sequence;
        let last = records
            .last()
            .ok_or_else(|| Error::Invalid("empty index".into()))?
            .receipt
            .commit_sequence;
        let object = self
            .inner
            .put_json(&self.key, head.gc_generation, "indexes", &records)
            .await?;
        Ok(IndexRef {
            object,
            first,
            last,
            threads,
        })
    }

    /// Seal before listing. Prepared writes from the sealed epoch fail their old head CAS.
    pub async fn collect_garbage(&self) -> Result<GcReport> {
        let mut sealed = None;
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            crate::history::expire_snapshots(
                &mut head,
                awaken_time::WallClockExt::now_ms(&*self.inner.clock),
            );
            let cutoff = head.gc_generation;
            head.gc_generation = next(cutoff)?;
            if self.publish_head(&mut head, version).await? {
                sealed = Some(cutoff);
                break;
            }
        }
        let cutoff = sealed.ok_or_else(|| Error::Storage("Session busy".into()))?;
        // Every reference reachable at sealing remains reachable or may be conservatively retained.
        let (head, _) = self.inner.load_head(&self.key).await?;
        let roots = self.reachable(&head).await?;
        let prefix = self.inner.path(&format!("{}/", self.key.root()));
        self.inner.count_list();
        let objects = self
            .inner
            .store
            .list(Some(&prefix))
            .try_collect::<Vec<_>>()
            .await
            .map_err(storage_error)?;
        let mut report = GcReport {
            sealed_generation: cutoff,
            ..GcReport::default()
        };
        for object in objects {
            report.examined += 1;
            let key = object.location.as_ref();
            let Some(relative) = self.inner.relative_key(key) else {
                continue;
            };
            if !relative.starts_with(&format!("{}/", self.key.root())) {
                return Err(Error::Corrupt(
                    "GC listing escaped the Session prefix".into(),
                ));
            }
            let epoch = relative
                .rsplit('/')
                .next()
                .and_then(|file| file.split_once('-'))
                .and_then(|(epoch, _)| epoch.parse::<u64>().ok());
            if !sweep_candidate(epoch, cutoff, |path| roots.contains(path), relative) {
                continue;
            }
            self.inner.count_delete();
            self.inner
                .store
                .delete(&object.location)
                .await
                .map_err(storage_error)?;
            report.deleted += 1;
        }
        Ok(report)
    }
    async fn reachable(&self, head: &Head) -> Result<BTreeSet<String>> {
        let mut keys = BTreeSet::new();
        keys.extend(
            head.components
                .values()
                .map(|reference| reference.key.clone()),
        );
        keys.extend(self.component_record_roots(head).await?);
        for row in head.runs.values() {
            keys.insert(row.activation.key.clone());
        }
        for index in &head.indexes {
            keys.insert(index.object.key.clone());
        }
        for record in self.descriptors(head).await? {
            keys.insert(record.body.key);
            for artifact in record.artifacts {
                keys.insert(artifact.object.key);
            }
        }
        let mut views = head.snapshots.values().cloned().collect::<Vec<_>>();
        for thread in head.threads.values() {
            views.extend(thread.base.clone());
            views.extend(thread.effective.clone());
        }
        for object in views {
            keys.insert(object.key.clone());
            let frozen: FrozenView = self.inner.read_json(&self.key, &object).await?;
            for part in frozen.parts {
                keys.insert(part.body.key);
            }
        }
        for checkpoint in head.checkpoints.values() {
            keys.insert(checkpoint.object.key.clone());
        }
        if let Some(reference) = &head.stream_archive {
            keys.insert(reference.key.clone());
        }
        for frames in self.streams().stream_logs_at(head).await?.values() {
            keys.extend(self.streams().stream_roots(frames).await?);
        }
        for blob in head.blobs.values() {
            keys.insert(blob.object.key.clone());
        }
        Ok(keys)
    }
    pub async fn archive(&self, archived: bool) -> Result<()> {
        for _ in 0..self.inner.limits.max_cas_attempts {
            let (mut head, version) = self.inner.load_head(&self.key).await?;
            if archived
                && (head
                    .runs
                    .values()
                    .any(|row| row.claim.is_some() || row.physical_attempt.is_some())
                    || head
                        .component_markers
                        .get("dispatch")
                        .is_some_and(|markers| {
                            markers
                                .keys()
                                .any(|key| key.starts_with("history-blocked:"))
                        }))
            {
                return Err(Error::Conflict("Session has active attempts".into()));
            }
            if head.archived == archived {
                return Ok(());
            }
            head.archived = archived;
            if self.publish_head(&mut head, version).await? {
                return Ok(());
            }
        }
        Err(Error::Storage("Session busy".into()))
    }
}
