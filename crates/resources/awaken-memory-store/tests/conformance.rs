//! Backend-generic conformance for `MemoryBlobStore`, run against every backend
//! (in-memory + filesystem + sqlite, and — when reachable — postgres), so all
//! backends keep identical semantics: dense unique-id minting, byte round-trips,
//! workspace scoping, and empty-on-create.

use awaken_memory_store::{FsMemoryBlobStore, InMemoryBlobStore, MemoryBlobStore};

// The path-addressed `MemoryFs` port (ADR-0053) — exercised below across the same
// backend set as the blob store, so the cross-node CAS backend (`PgMemoryFs`) keeps
// the identical POSIX-replace / compare-and-swap semantics the in-process backends
// already prove in the crate's unit tests.
use awaken_memory_store::memfs::MAX_PATH_BYTES;
use awaken_memory_store::{FsMemoryFs, MAX_MEMORY_BYTES, MemErr, MemoryFs, sha256_hex};
use std::sync::Arc;

async fn create_put_get_exists_and_scope_by_workspace(store: &dyn MemoryBlobStore) {
    // create mints a distinct id per call and resolves empty.
    let a = store.create("ws1").await.unwrap();
    let b = store.create("ws1").await.unwrap();
    assert_ne!(a, b, "ids are distinct");
    assert_eq!(store.get("ws1", &a).await.unwrap(), Some(Vec::new()));
    assert!(store.exists("ws1", &a).await.unwrap());
    assert!(!store.exists("ws1", "memstore_absent").await.unwrap());

    // put overwrites; bytes round-trip.
    store.put("ws1", &a, b"hello bytes").await.unwrap();
    assert_eq!(
        store.get("ws1", &a).await.unwrap().as_deref(),
        Some(&b"hello bytes"[..])
    );

    // A different workspace is isolated: the same id is absent there.
    assert_eq!(store.get("ws2", &a).await.unwrap(), None);
    assert!(!store.exists("ws2", &a).await.unwrap());
    store.put("ws2", "memstore_x", b"other").await.unwrap();
    assert_eq!(
        store.get("ws2", "memstore_x").await.unwrap().as_deref(),
        Some(&b"other"[..])
    );
    assert_eq!(store.get("ws1", "memstore_x").await.unwrap(), None);

    // A crafted `../` id addresses one safe blob: put and get sanitize identically, so
    // it round-trips and can never escape the store root. A very long id is bounded to
    // a stem, not a panic or an over-NAME_MAX filename.
    store.put("wsC", "../../etc/passwd", b"safe").await.unwrap();
    assert_eq!(
        store
            .get("wsC", "../../etc/passwd")
            .await
            .unwrap()
            .as_deref(),
        Some(&b"safe"[..])
    );
    let long = "x".repeat(500);
    store.put("wsC", &long, b"bounded").await.unwrap();
    assert_eq!(
        store.get("wsC", &long).await.unwrap().as_deref(),
        Some(&b"bounded"[..])
    );
}

/// Ids are dense AND monotonic AND globally-unique across workspaces: the first three
/// creates on a fresh store yield `memstore_1`, `memstore_2`, `memstore_3` regardless
/// of which workspace mints them (the counter is store-global, not per-workspace).
/// Pins the "dense `memstore_<n>`" clause of the port contract across every backend.
async fn mints_dense_global_monotonic_ids(store: &dyn MemoryBlobStore) {
    assert_eq!(store.create("ws1").await.unwrap(), "memstore_1");
    assert_eq!(store.create("ws1").await.unwrap(), "memstore_2");
    // A different workspace shares the same monotonic counter — no reset, no gap.
    assert_eq!(store.create("ws2").await.unwrap(), "memstore_3");
    assert_eq!(store.create("ws1").await.unwrap(), "memstore_4");
}

#[tokio::test]
async fn in_memory_mints_dense_ids() {
    mints_dense_global_monotonic_ids(&InMemoryBlobStore::new()).await;
}

#[tokio::test]
async fn fs_mints_dense_ids() {
    let dir = tempfile::tempdir().unwrap();
    mints_dense_global_monotonic_ids(&FsMemoryBlobStore::open(dir.path()).unwrap()).await;
}

#[tokio::test]
async fn in_memory_conforms() {
    create_put_get_exists_and_scope_by_workspace(&InMemoryBlobStore::new()).await;
}

#[tokio::test]
async fn fs_conforms_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    create_put_get_exists_and_scope_by_workspace(&FsMemoryBlobStore::open(&root).unwrap()).await;

    // A restart re-seeds the counter past what's on disk (no id re-mint) and reads
    // the committed bytes back.
    let store = FsMemoryBlobStore::open(&root).unwrap();
    let id = store.create("wsR").await.unwrap();
    store.put("wsR", &id, b"persist").await.unwrap();
    let reopened = FsMemoryBlobStore::open(&root).unwrap();
    assert_eq!(
        reopened.get("wsR", &id).await.unwrap().as_deref(),
        Some(&b"persist"[..])
    );
    // The reopened store does not re-mint the existing id.
    assert_ne!(reopened.create("wsR").await.unwrap(), id);
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_conforms() {
    use awaken_memory_store::SqliteMemoryBlobStore;
    create_put_get_exists_and_scope_by_workspace(&SqliteMemoryBlobStore::open_in_memory().unwrap())
        .await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_mints_dense_ids() {
    use awaken_memory_store::SqliteMemoryBlobStore;
    mints_dense_global_monotonic_ids(&SqliteMemoryBlobStore::open_in_memory().unwrap()).await;
}

// ---------------------------------------------------------------------------
// Path-addressed MemoryFs conformance (ADR-0053), backend-generic. The in-memory,
// filesystem, and sqlite backends already run these bodies in the crate's unit
// tests; the suites are duplicated here (mirroring the blob-store shape above) so
// the public `PgMemoryFs` — whose cross-node CAS raison d'être had zero coverage —
// runs the SAME conformance / extended / not-found suites under skip-on-unreachable.
// ---------------------------------------------------------------------------

/// The core lifecycle: create/version/sha, PathConflict, get, CAS update (right +
/// stale + idempotent base), list under a prefix, rename-replace, idempotent delete,
/// path validation, and the size cap. (Ported from `memfs::tests::conformance`.)
async fn memfs_conformance(fs: &dyn MemoryFs) {
    let store = "memstore_1";

    let m = fs.create(store, "/notes/today.md", "alpha").await.unwrap();
    assert_eq!(m.path, "/notes/today.md");
    assert_eq!(m.version, 1);
    assert_eq!(m.content_sha256, sha256_hex("alpha"));
    assert_eq!(m.content.as_deref(), Some("alpha"));
    assert!(m.created_unix_nanos > 0 && m.updated_unix_nanos >= m.created_unix_nanos);

    // duplicate path → PathConflict.
    assert!(matches!(
        fs.create(store, "/notes/today.md", "x").await,
        Err(MemErr::PathConflict(_))
    ));

    // get_by_path.
    let got = fs
        .get_by_path(store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.content.as_deref(), Some("alpha"));
    assert!(fs.get_by_path(store, "/nope.md").await.unwrap().is_none());

    // update with the right base sha → version bumps.
    let up = fs
        .update(store, &m.id, "beta", &m.content_sha256)
        .await
        .unwrap();
    assert_eq!(up.version, 2);
    assert_eq!(up.content.as_deref(), Some("beta"));

    // update with a STALE base sha → Conflict carrying live "beta", no clobber.
    match fs.update(store, &m.id, "gamma", &m.content_sha256).await {
        Err(MemErr::Conflict { current }) => {
            assert_eq!(current.content.as_deref(), Some("beta"));
            assert_eq!(current.content_sha256, sha256_hex("beta"));
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert_eq!(
        fs.get_by_path(store, "/notes/today.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("beta")
    );

    // idempotent update: stale base sha but content already current → Ok.
    let idem = fs.update(store, &m.id, "beta", "deadbeef").await.unwrap();
    assert_eq!(idem.content.as_deref(), Some("beta"));

    // list under a prefix.
    fs.create(store, "/notes/other.md", "o").await.unwrap();
    fs.create(store, "/root.md", "r").await.unwrap();
    let mut notes: Vec<_> = fs
        .list(store, "/notes")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    notes.sort();
    assert_eq!(notes, vec!["/notes/other.md", "/notes/today.md"]);
    assert_eq!(fs.list(store, "/").await.unwrap().len(), 3);

    // rename-replace: move over an existing target atomically, id preserved.
    let before = fs
        .get_by_path(store, "/notes/today.md")
        .await
        .unwrap()
        .unwrap();
    let moved = fs
        .rename(store, "/notes/today.md", "/notes/other.md")
        .await
        .unwrap();
    assert_eq!(moved.id, before.id, "rename preserves the memory id");
    assert_eq!(moved.path, "/notes/other.md");
    assert_eq!(moved.content.as_deref(), Some("beta"));
    assert!(
        fs.get_by_path(store, "/notes/today.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs.get_by_path(store, "/notes/other.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("beta"),
        "the destination was atomically replaced with the source content"
    );

    // delete (idempotent).
    fs.delete_by_path(store, "/notes/other.md").await.unwrap();
    assert!(
        fs.get_by_path(store, "/notes/other.md")
            .await
            .unwrap()
            .is_none()
    );
    fs.delete_by_path(store, "/notes/other.md").await.unwrap(); // absent → Ok

    // path validation.
    for bad in ["relative.md", "/a/../b.md", "/a//b.md"] {
        assert!(
            matches!(
                fs.create(store, bad, "x").await,
                Err(MemErr::InvalidPath(_))
            ),
            "expected InvalidPath for {bad:?}"
        );
    }

    // size cap.
    let big = "x".repeat(MAX_MEMORY_BYTES + 1);
    assert!(matches!(
        fs.create(store, "/big.md", &big).await,
        Err(MemErr::TooLarge)
    ));
}

/// Cause-effect-graph edges beyond the core suite: remaining validation branches,
/// CAS/rename precedence (validate-before-lookup, from==to), and prefix-boundary
/// correctness. (Ported from `memfs::tests::extended_conformance`.)
async fn memfs_extended(fs: &dyn MemoryFs) {
    let store = "ext";

    let over_cap = format!("/{}", "a".repeat(MAX_PATH_BYTES)); // len == cap + 1
    for bad in ["/", "/ctrl\nseg.md", "/a/./b.md", over_cap.as_str()] {
        assert!(
            matches!(
                fs.create(store, bad, "x").await,
                Err(MemErr::InvalidPath(_))
            ),
            "expected InvalidPath for {bad:?}"
        );
    }

    // content exactly at the cap is accepted.
    let at_cap = "x".repeat(MAX_MEMORY_BYTES);
    let m_cap = fs.create(store, "/at-cap.md", &at_cap).await.unwrap();
    assert_eq!(m_cap.content_size, MAX_MEMORY_BYTES as u64);

    // validate_size runs BEFORE the id lookup → TooLarge, not NotFound.
    let big = "x".repeat(MAX_MEMORY_BYTES + 1);
    assert!(matches!(
        fs.update(store, "mem_does_not_exist", &big, "sha").await,
        Err(MemErr::TooLarge)
    ));

    // an idempotent update must NOT bump the version.
    let m = fs.create(store, "/idem.md", "v0").await.unwrap();
    let up = fs
        .update(store, &m.id, "v1", &m.content_sha256)
        .await
        .unwrap();
    assert_eq!(up.version, 2);
    let idem = fs
        .update(store, &m.id, "v1", "stale-base-sha")
        .await
        .unwrap();
    assert_eq!(
        idem.version, 2,
        "idempotent write leaves the version unchanged"
    );

    // rename to an invalid path fails before touching the source.
    fs.create(store, "/src.md", "s").await.unwrap();
    assert!(matches!(
        fs.rename(store, "/src.md", "relative").await,
        Err(MemErr::InvalidPath(_))
    ));
    assert!(fs.get_by_path(store, "/src.md").await.unwrap().is_some());

    // from == to returns current, no version bump.
    let src = fs.get_by_path(store, "/src.md").await.unwrap().unwrap();
    let same = fs.rename(store, "/src.md", "/src.md").await.unwrap();
    assert_eq!(same.id, src.id);
    assert_eq!(same.version, src.version);

    // a pure move preserves the id and bumps the version.
    let moved = fs.rename(store, "/src.md", "/moved.md").await.unwrap();
    assert_eq!(moved.id, src.id);
    assert_eq!(moved.version, src.version + 1);
    assert!(fs.get_by_path(store, "/src.md").await.unwrap().is_none());

    // prefix boundary + empty prefix lists everything.
    let pstore = "prefix";
    fs.create(pstore, "/notes", "a").await.unwrap();
    fs.create(pstore, "/notes/x.md", "b").await.unwrap();
    fs.create(pstore, "/notesbar", "c").await.unwrap();
    let mut under: Vec<_> = fs
        .list(pstore, "/notes")
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    under.sort();
    assert_eq!(under, vec!["/notes", "/notes/x.md"]);
    assert_eq!(fs.list(pstore, "").await.unwrap().len(), 3);

    // delete on a store that was never created is a no-op Ok.
    fs.delete_by_path("never_created_store", "/x.md")
        .await
        .unwrap();
}

/// The `NotFound` paths of `update`/`rename`. (Ported from
/// `memfs::tests::not_found_paths`.)
async fn memfs_not_found(fs: &dyn MemoryFs) {
    let store = "s";
    assert!(matches!(
        fs.update(store, "no_id", "x", "sha").await,
        Err(MemErr::NotFound(_))
    ));
    fs.create(store, "/seed.md", "s").await.unwrap();
    assert!(matches!(
        fs.update(store, "no_such_id", "x", "sha").await,
        Err(MemErr::NotFound(_))
    ));
    assert!(matches!(
        fs.rename(store, "/gone.md", "/x.md").await,
        Err(MemErr::NotFound(_))
    ));
    assert!(matches!(
        fs.rename(store, "/gone.md", "/gone.md").await,
        Err(MemErr::NotFound(_))
    ));
}

// --- Concurrency (ADR-0053 P2.5), extended from the in-memory-only unit tests to the
//     durable backends so the real write_lock (fs) / connection-mutex (sqlite)
//     serialize concurrent writers exactly one winner deep. ---

/// 8 concurrent creates of one path → exactly one winner, the rest `PathConflict`.
async fn concurrent_create_one_winner<F>(fs: Arc<F>)
where
    F: MemoryFs + Send + Sync + 'static,
{
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let fs = fs.clone();
        handles.push(tokio::spawn(async move {
            fs.create("s", "/race.md", &format!("v{i}")).await
        }));
    }
    let (mut oks, mut conflicts) = (0, 0);
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => oks += 1,
            Err(MemErr::PathConflict(_)) => conflicts += 1,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(oks, 1, "exactly one create wins the path");
    assert_eq!(conflicts, 7);
}

/// 8 concurrent CAS updates on one base sha → exactly one winner, the rest `Conflict`
/// (none clobbers).
async fn concurrent_cas_one_winner<F>(fs: Arc<F>)
where
    F: MemoryFs + Send + Sync + 'static,
{
    let m = fs.create("s", "/c.md", "v0").await.unwrap();
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let (fs, id, base) = (fs.clone(), m.id.clone(), m.content_sha256.clone());
        handles.push(tokio::spawn(async move {
            fs.update("s", &id, &format!("w{i}"), &base).await
        }));
    }
    let (mut oks, mut conflicts) = (0, 0);
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => oks += 1,
            Err(MemErr::Conflict { .. }) => conflicts += 1,
            other => panic!("unexpected: {other:?}"),
        }
    }
    assert_eq!(oks, 1, "exactly one CAS write wins; none clobbers");
    assert_eq!(conflicts, 7);
}

#[tokio::test]
async fn fs_concurrent_create_has_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    concurrent_create_one_winner(Arc::new(FsMemoryFs::open(dir.path()).unwrap())).await;
}

#[tokio::test]
async fn fs_concurrent_cas_has_exactly_one_winner() {
    let dir = tempfile::tempdir().unwrap();
    concurrent_cas_one_winner(Arc::new(FsMemoryFs::open(dir.path()).unwrap())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_concurrent_create_has_exactly_one_winner() {
    use awaken_memory_store::SqliteMemoryFs;
    concurrent_create_one_winner(Arc::new(SqliteMemoryFs::open_in_memory().unwrap())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_concurrent_cas_has_exactly_one_winner() {
    use awaken_memory_store::SqliteMemoryFs;
    concurrent_cas_one_winner(Arc::new(SqliteMemoryFs::open_in_memory().unwrap())).await;
}

/// Live Postgres conformance on a fresh schema. Skips when no Postgres is reachable
/// (`AWAKEN_TEST_DATABASE_URL`), proving the same portable bundle renders there too.
#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use awaken_memory_store::{PgMemoryBlobStore, PgMemoryFs};
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};

    fn database_url() -> String {
        std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
        })
    }

    async fn schema_pool(schema: &'static str) -> Option<PgPool> {
        let admin = match PgPool::connect(&database_url()).await {
            Ok(pool) => pool,
            Err(err) => {
                println!("[skip] no Postgres reachable: {err}");
                return None;
            }
        };
        let _ = admin
            .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
            .await;
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("create schema");
        admin.close().await;
        PgPoolOptions::new()
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    conn.execute(format!("SET search_path = {schema}").as_str())
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url())
            .await
            .ok()
    }

    #[tokio::test]
    async fn postgres_conforms() {
        let Some(pool) = schema_pool("t_memory").await else {
            return;
        };
        let store = PgMemoryBlobStore::with_pool(pool);
        store.ensure_schema().await.unwrap();
        create_put_get_exists_and_scope_by_workspace(&store).await;
    }

    /// The FULL path-addressed `MemoryFs` conformance + extended + not-found suites
    /// against `PgMemoryFs` — the cross-node CAS backend whose `FOR UPDATE` row locks,
    /// transactional POSIX-replace, and `MemErr::Conflict`-on-stale-base are its whole
    /// reason to exist and previously had zero coverage.
    #[tokio::test]
    async fn postgres_memory_fs_conforms() {
        let Some(pool) = schema_pool("t_memoryfs").await else {
            return;
        };
        let fs = PgMemoryFs::with_pool(pool);
        fs.ensure_schema().await.unwrap();
        memfs_conformance(&fs).await;
        memfs_extended(&fs).await;
        memfs_not_found(&fs).await;
    }

    /// `PgMemoryFs::create` reads existence with an unlocked `SELECT 1` and mints its
    /// ordinal with `SELECT MAX(ordinal)+1` — neither `FOR UPDATE` — so two concurrent
    /// same-path creates both pass the existence check and race the `INSERT`. The primary
    /// key `(store_id, path)` serializes them; the loser's unique-violation is translated
    /// to the DOMAIN `MemErr::PathConflict` the serialized in-process backends return, not
    /// leaked as a raw `MemErr::Storage`. This pins that the wire-visible outcome of a
    /// same-path race is the same domain error across every backend.
    #[tokio::test]
    async fn postgres_concurrent_same_path_create_surfaces_path_conflict_not_raw_storage() {
        let Some(pool) = schema_pool("t_memoryfs_race").await else {
            return;
        };
        let fs = Arc::new(PgMemoryFs::with_pool(pool));
        fs.ensure_schema().await.unwrap();

        let mut handles = Vec::new();
        for i in 0..2u32 {
            let fs = fs.clone();
            handles.push(tokio::spawn(async move {
                fs.create("s", "/race.md", &format!("v{i}")).await
            }));
        }
        let mut oks = 0;
        let mut errs = Vec::new();
        for h in handles {
            match h.await.unwrap() {
                Ok(_) => oks += 1,
                Err(e) => errs.push(e),
            }
        }
        assert_eq!(oks, 1, "exactly one same-path create commits");
        assert_eq!(errs.len(), 1, "the other loses the PK race");
        // The losing create's unique-violation is mapped to the domain PathConflict,
        // not leaked as a raw Storage error — parity with the in-process backends.
        assert!(
            matches!(errs[0], MemErr::PathConflict(ref p) if p == "/race.md"),
            "the losing create surfaces the domain PathConflict, not a raw Storage \
             error — got {:?}",
            errs[0]
        );
    }

    /// The sqlite id-reuse-after-delete divergence (pinned in `sqlite::memfs_tests`)
    /// also holds for `PgMemoryFs`: both derive the next id from `MAX(ordinal)` over the
    /// LIVE rows, so deleting the highest-ordinal memory and creating again REUSES the
    /// deleted ordinal (`mem_2`) instead of minting a fresh `mem_3` the way the in-process
    /// monotonic-counter backends do.
    #[tokio::test]
    async fn postgres_reuses_deleted_top_ordinal_like_sqlite() {
        let Some(pool) = schema_pool("t_memoryfs_reuse").await else {
            return;
        };
        let fs = PgMemoryFs::with_pool(pool);
        fs.ensure_schema().await.unwrap();
        fs.create("s", "/a.md", "a").await.unwrap();
        fs.create("s", "/b.md", "b").await.unwrap(); // mem_2 (top ordinal)
        fs.delete_by_path("s", "/b.md").await.unwrap();
        let c = fs.create("s", "/c.md", "c").await.unwrap();
        assert_eq!(
            c.id, "mem_2",
            "BUG(pinned): postgres reuses the deleted top ordinal, matching sqlite \
             (the monotonic in-memory backend would mint mem_3)"
        );
    }
}
