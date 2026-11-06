//! T23-C02: Copy materialization reads one atomic snapshot and never stitches a
//! store view from list plus per-path reads, even when those reads exist.
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_resource_contract::{
    MemErr, Memory, MemoryEntry, MemoryQueries, MemorySnapshotSource, memory_sha256_hex,
};

use super::materialize;

fn head(id: &str, path: &str, content: &str, version: u64) -> Memory {
    Memory {
        id: id.into(),
        path: path.into(),
        content_sha256: memory_sha256_hex(content),
        content_size: content.len() as u64,
        version,
        created_unix_nanos: 1,
        updated_unix_nanos: version.into(),
        content: Some(content.into()),
    }
}

/// A backend that offers both the atomic snapshot and split reads; every split
/// read fails loudly so any list/get stitching is detected.
struct SnapshotWithFailingSplitReads {
    heads: Vec<Memory>,
    snapshots: AtomicUsize,
    split_reads: AtomicUsize,
}

#[async_trait::async_trait]
impl MemorySnapshotSource for SnapshotWithFailingSplitReads {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        assert_eq!(store, "s");
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        Ok(self.heads.clone())
    }
}

#[async_trait::async_trait]
impl MemoryQueries for SnapshotWithFailingSplitReads {
    async fn list(&self, _store: &str, _prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        self.split_reads.fetch_add(1, Ordering::SeqCst);
        Err(MemErr::Storage("split read: list".into()))
    }
    async fn get_by_path(&self, _store: &str, _path: &str) -> Result<Option<Memory>, MemErr> {
        self.split_reads.fetch_add(1, Ordering::SeqCst);
        Err(MemErr::Storage("split read: get".into()))
    }
}

/// Negative-control mutant: the pre-snapshot consumer shape (list, then get each path).
async fn split_read_materialize(source: &dyn MemoryQueries) -> Result<Vec<Memory>, MemErr> {
    let mut memories = Vec::new();
    for entry in source.list("s", "/").await? {
        if let Some(memory) = source.get_by_path("s", &entry.path).await? {
            memories.push(memory);
        }
    }
    Ok(memories)
}

/// S1 production materialize: one snapshot read, zero split reads, every head
/// materialized with its exact bytes and CAS base. S2 the split-read mutant
/// run against the same backend is detected by the failing list.
#[test]
fn copy_materialization_never_falls_back_to_split_reads() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let source = SnapshotWithFailingSplitReads {
        heads: vec![
            head("memory-a", "/a.md", "alpha", 1),
            head("memory-b", "/nested/b.md", "beta", 2),
        ],
        snapshots: AtomicUsize::new(0),
        split_reads: AtomicUsize::new(0),
    };
    let dir = tempfile::tempdir().unwrap();

    let snapshot = runtime
        .block_on(materialize(&source, "s", dir.path()))
        .expect("S1 the atomic snapshot alone is sufficient");
    assert_eq!(snapshot.len(), 2, "S1 both heads captured");
    assert_eq!(
        source.snapshots.load(Ordering::SeqCst),
        1,
        "S1 one snapshot"
    );
    assert_eq!(
        source.split_reads.load(Ordering::SeqCst),
        0,
        "S1 no list/get"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.md")).unwrap(),
        "alpha"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("nested").join("b.md")).unwrap(),
        "beta"
    );

    let mutant = runtime.block_on(split_read_materialize(&source));
    assert!(
        matches!(&mutant, Err(MemErr::Storage(message)) if message == "split read: list"),
        "S2 the failing split read detects the stitched consumer: {mutant:?}"
    );
    assert_eq!(source.split_reads.load(Ordering::SeqCst), 1, "S2");
}
