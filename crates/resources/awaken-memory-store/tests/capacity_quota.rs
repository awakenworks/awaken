//! T38-C03 (Memory quota): a store at `MAX_MEMORIES_PER_STORE` rejects a new
//! head with the typed `AtCapacity` class and writes nothing; after capacity is
//! restored (one delete) the same create commits exactly once.
//!
//! | Rule | Cause | Effect |
//! |---|---|---|
//! | Q1 | store full, create | `MemErr::AtCapacity`; heads and history unchanged |
//! | Q2 | one head deleted, same create retried (other handle) | one new head + one version |
//! | Q3 | same create retried again | `PathConflict`; no second commit |

use awaken_memory_store::{
    MAX_MEMORIES_PER_STORE, MemErr, MemoryQueries, MemorySnapshotSource, MemoryVersionQueries,
    MemoryWriter,
};

const STORE: &str = "quota";

async fn fill<M: MemoryWriter + ?Sized>(fs: &M) {
    for index in 0..MAX_MEMORIES_PER_STORE {
        MemoryWriter::create(fs, STORE, &format!("/m{index:05}.md"), "q")
            .await
            .unwrap();
    }
}

async fn quota_conformance<A, B>(a: &A, b: &B)
where
    A: MemoryWriter + MemoryQueries + MemorySnapshotSource + MemoryVersionQueries + ?Sized,
    B: MemoryWriter + MemoryQueries + MemorySnapshotSource + MemoryVersionQueries + ?Sized,
{
    fill(a).await;
    let heads = MemorySnapshotSource::snapshot_heads(a, STORE)
        .await
        .unwrap();
    let history = MemoryVersionQueries::list_versions(a, STORE).await.unwrap();
    assert_eq!(heads.len(), MAX_MEMORIES_PER_STORE);

    let rejected = MemoryWriter::create(a, STORE, "/new.md", "retry").await;
    assert!(
        matches!(rejected, Err(MemErr::AtCapacity)),
        "Q1 {rejected:?}"
    );
    assert_eq!(
        MemorySnapshotSource::snapshot_heads(b, STORE)
            .await
            .unwrap(),
        heads,
        "Q1 no partial head"
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(b, STORE).await.unwrap(),
        history,
        "Q1 no partial history"
    );

    MemoryWriter::delete_by_path_as(a, STORE, "/m00000.md", None)
        .await
        .unwrap();
    let after_delete = MemoryVersionQueries::list_versions(b, STORE).await.unwrap();
    let created = MemoryWriter::create(b, STORE, "/new.md", "retry")
        .await
        .expect("Q2 retry after capacity restored");
    let after = MemoryVersionQueries::list_versions(a, STORE).await.unwrap();
    assert_eq!(after.len(), after_delete.len() + 1, "Q2 one version");
    assert_eq!(
        after.last().unwrap().memory_id,
        created.id,
        "Q2 exact version"
    );
    assert_eq!(
        MemoryQueries::get_by_path(a, STORE, "/new.md")
            .await
            .unwrap()
            .unwrap()
            .id,
        created.id
    );
    assert!(
        matches!(
            MemoryWriter::create(a, STORE, "/new.md", "retry").await,
            Err(MemErr::PathConflict(_))
        ),
        "Q3"
    );
    assert_eq!(
        MemoryVersionQueries::list_versions(b, STORE).await.unwrap(),
        after,
        "Q3 no second commit"
    );
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file_memory_quota_rejects_atomically_and_retry_commits_once() {
    use awaken_memory_store::SqliteMemoryRepository;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("quota.db");
    let path = path.to_str().unwrap();
    let a = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    let b = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    quota_conformance(&a, &b).await;
}
