use super::*;
use awaken_memory_store::{VolatileMemoryRepository, sha256_hex};

fn test_fs() -> MemoryFuse {
    MemoryFuse::new(
        Arc::new(VolatileMemoryRepository::new()),
        "memstore_test".into(),
    )
    .unwrap()
}

fn memory(path: &str, content: &str) -> Memory {
    Memory {
        id: format!("id-{path}"),
        path: path.to_string(),
        content_sha256: sha256_hex(content),
        content_size: content.len() as u64,
        version: 1,
        created_unix_nanos: 100,
        updated_unix_nanos: 200,
        content: Some(content.to_string()),
    }
}

#[test]
fn invalidation_listener_drops_matching_paths_only() {
    use crate::LocalInvalidator;
    use crate::invalidate::Invalidator;

    let cache = Arc::new(Mutex::new(ContentLruCache::new(8)));
    cache.lock().unwrap().put(memory("/a.md", "one"));
    cache.lock().unwrap().put(memory("/b.md", "two"));

    let bus = LocalInvalidator::new(16);
    let rx = bus.subscribe();
    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let (cache, stop) = (cache.clone(), stop.clone());
        std::thread::spawn(move || run_invalidation_listener(cache, "s".into(), rx, stop))
    };

    // A write on another node for our store drops that path here…
    bus.publish("s", "/a.md");
    // …while a write for a different store is ignored.
    bus.publish("other", "/b.md");

    // Wait for the listener to observe the invalidation (poll cadence is 20ms).
    let mut waited = Duration::ZERO;
    while cache.lock().unwrap().get("/a.md").is_some() && waited < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
        waited += Duration::from_millis(10);
    }
    assert!(
        cache.lock().unwrap().get("/a.md").is_none(),
        "matching path dropped"
    );
    assert!(
        cache.lock().unwrap().get("/b.md").is_some(),
        "other store untouched"
    );

    bus.publish("s", "/");
    let mut waited = Duration::ZERO;
    while cache.lock().unwrap().get("/b.md").is_some() && waited < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
        waited += Duration::from_millis(10);
    }
    assert!(
        cache.lock().unwrap().get("/b.md").is_none(),
        "the root sentinel clears every cached path in the store"
    );

    stop.store(true, Ordering::SeqCst);
    listener.join().unwrap();
}

#[test]
fn fuse_constructs_with_a_seeded_root() {
    let fs = test_fs();
    let root = fs.node(ROOT_INO).expect("root node");
    assert_eq!(root.kind, FileType::Directory);
}

#[test]
fn attr_reports_the_records_timestamps_not_now() {
    let fs = test_fs();
    let (ino, node) = fs.intern_memory(&memory("/a.md", "hi"));
    let attr = MemoryFuse::attr(ino, &node);
    assert_eq!(
        attr.crtime,
        to_systime(100),
        "crtime is the record's created"
    );
    assert_eq!(attr.mtime, to_systime(200), "mtime is the record's updated");
}

#[test]
fn content_lru_cache_evicts_oldest_and_refreshes_on_get() {
    let mut cache = ContentLruCache::new(2);
    cache.put(memory("/a.md", "a"));
    cache.put(memory("/b.md", "b"));
    assert!(cache.get("/a.md").is_some());
    cache.put(memory("/c.md", "c"));
    assert!(cache.get("/a.md").is_some());
    assert!(
        cache.get("/b.md").is_none(),
        "the least-recently-used entry evicted"
    );
    assert!(cache.get("/c.md").is_some());
}

#[test]
fn open_fd_count_tracks_allocated_handles() {
    let fs = test_fs();
    let a = fs.allocate_fh(OpenFile {
        memory_id: "m1".into(),
        base_sha256: sha256_hex("one"),
        buffer: b"one".to_vec(),
        dirty: false,
    });
    let b = fs.allocate_fh(OpenFile {
        memory_id: "m2".into(),
        base_sha256: sha256_hex("two"),
        buffer: b"two".to_vec(),
        dirty: false,
    });
    assert_eq!(fs.mount_state.open_fds.load(Ordering::SeqCst), 2);
    fs.remove_fh(a);
    assert_eq!(fs.mount_state.open_fds.load(Ordering::SeqCst), 1);
    fs.remove_fh(b);
    assert_eq!(fs.mount_state.open_fds.load(Ordering::SeqCst), 0);
}

#[test]
fn dirty_fd_budget_fails_closed_at_the_limit() {
    let fs = test_fs();
    for index in 0..MAX_DIRTY_OPEN_FILES {
        let fh = fs.allocate_fh(OpenFile {
            memory_id: format!("m{index}"),
            base_sha256: sha256_hex("clean"),
            buffer: Vec::new(),
            dirty: false,
        });
        fs.write_fh_bytes(fh, 0, b"x").expect("dirty fd admitted");
    }
    let blocked = fs.allocate_fh(OpenFile {
        memory_id: "blocked".into(),
        base_sha256: sha256_hex("blocked"),
        buffer: Vec::new(),
        dirty: false,
    });
    assert!(
        matches!(
            fs.write_fh_bytes(blocked, 0, b"x"),
            Err(FuseError::DirtyFileLimitExceeded)
        ),
        "a new dirty fd past the budget fails closed"
    );
}

// These drive the real open/write/flush/rename code paths over a live
// `VolatileMemoryRepository` (no kernel needed). The `MemoryFuse` owns its own runtime and
// its methods block on it internally, so the test stays synchronous and uses a
// separate runtime only for external store setup — the two never nest.
fn setup_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

#[test]
fn a_stale_open_fd_conflicts_and_does_not_clobber_a_newer_write() {
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    let created = rt.block_on(backend.create(store, "/c.md", "v1")).unwrap();

    let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
    // Open captures base sha = sha("v1").
    let fh = fuse.open_file_for_path("/c.md", 0).unwrap();

    // Another writer advances the store out from under the open fd.
    rt.block_on(backend.update(store, &created.id, "server-wins", &created.content_sha256))
        .unwrap();

    // The stale fd writes and flushes: CAS on the now-stale base sha conflicts,
    // the buffer is kept, and the store's newer content is NOT clobbered.
    fuse.write_fh_bytes(fh, 0, b"stale-write").unwrap();
    assert!(
        matches!(
            fuse.flush_fh(fh),
            Err(FuseError::Mem(MemErr::Conflict { .. }))
        ),
        "the stale flush conflicts"
    );
    let live = rt
        .block_on(backend.get_by_path(store, "/c.md"))
        .unwrap()
        .unwrap();
    assert_eq!(
        live.content.as_deref(),
        Some("server-wins"),
        "the newer write survives; the stale fd did not clobber it"
    );
}

#[test]
fn errno_maps_every_fault_class() {
    use awaken_memory_store::Memory;
    let mem = || {
        Box::new(Memory {
            id: "i".into(),
            path: "/p".into(),
            content_sha256: String::new(),
            content_size: 0,
            version: 1,
            created_unix_nanos: 0,
            updated_unix_nanos: 0,
            content: None,
        })
    };
    assert_eq!(MemoryFuse::errno(FuseError::NotFound("x".into())), ENOENT);
    assert_eq!(
        MemoryFuse::errno(FuseError::Mem(MemErr::NotFound("x".into()))),
        ENOENT
    );
    assert_eq!(
        MemoryFuse::errno(FuseError::Mem(MemErr::PathConflict("x".into()))),
        EEXIST
    );
    assert_eq!(
        MemoryFuse::errno(FuseError::Mem(MemErr::InvalidPath("x".into()))),
        EINVAL
    );
    assert_eq!(MemoryFuse::errno(FuseError::DirtyFileLimitExceeded), EAGAIN);
    assert_eq!(
        MemoryFuse::errno(FuseError::Mem(MemErr::Conflict { current: mem() })),
        EAGAIN
    );
    assert_eq!(
        MemoryFuse::errno(FuseError::Mem(MemErr::AtCapacity)),
        ENOSPC
    );
    assert_eq!(MemoryFuse::errno(FuseError::TooLarge), EIO);
    assert_eq!(MemoryFuse::errno(FuseError::Internal("x".into())), EIO);
    assert_eq!(
        MemoryFuse::errno(FuseError::Mem(MemErr::Storage("x".into()))),
        EIO
    );
}

#[test]
fn lookup_path_resolves_files_synthetic_dirs_and_missing() {
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/notes/a.md", "a"))
        .unwrap();

    let fuse = MemoryFuse::new(backend, store.into()).unwrap();
    // root
    let (ino, node) = fuse.lookup_path("/").unwrap();
    assert_eq!(ino, ROOT_INO);
    assert_eq!(node.kind, FileType::Directory);
    // a real file
    let (_, f) = fuse.lookup_path("/notes/a.md").unwrap();
    assert_eq!(f.kind, FileType::RegularFile);
    // a synthetic directory (something lives under it)
    let (_, d) = fuse.lookup_path("/notes").unwrap();
    assert_eq!(d.kind, FileType::Directory);
    // nothing there
    assert!(matches!(
        fuse.lookup_path("/ghost"),
        Err(FuseError::NotFound(_))
    ));
    assert!(matches!(
        fuse.lookup_path("/notes/ghost"),
        Err(FuseError::NotFound(_))
    ));
}

#[test]
fn open_create_flush_read_inner_branches() {
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/f.md", "orig")).unwrap();
    let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();

    // open a missing path → NotFound.
    assert!(matches!(
        fuse.open_file_for_path("/ghost", 0),
        Err(FuseError::NotFound(_))
    ));

    // open with O_TRUNC clears the buffer and marks it dirty.
    let fh = fuse.open_file_for_path("/f.md", O_TRUNC).unwrap();
    assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"");

    // read on an unknown fh → NotFound.
    assert!(matches!(
        fuse.read_fh_bytes(9999, 0, 1),
        Err(FuseError::NotFound(_))
    ));

    // flush the truncated fd → the store now holds empty content.
    fuse.flush_fh(fh).unwrap();
    assert_eq!(
        rt.block_on(backend.get_by_path(store, "/f.md"))
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some(""),
        "O_TRUNC open flushed empty"
    );
    // flushing a clean fd is a no-op (dirty was cleared on the prior flush).
    fuse.flush_fh(fh).unwrap();

    // create over an existing path → PathConflict (→ EEXIST).
    assert!(matches!(
        fuse.create_file_for_path("/f.md"),
        Err(FuseError::Mem(MemErr::PathConflict(_)))
    ));

    // truncate_open_writers retargets an open writer (true), else no-op (false).
    let w = fuse.open_file_for_path("/f.md", 0).unwrap();
    assert!(fuse.truncate_open_writers("/f.md", 0).unwrap());
    assert!(!fuse.truncate_open_writers("/absent", 0).unwrap());
    fuse.remove_fh(w);
}

#[test]
fn readdir_entries_lists_children_and_rejects_non_directories() {
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/a.md", "a")).unwrap();
    rt.block_on(backend.create(store, "/sub/b.md", "b"))
        .unwrap();
    let fuse = MemoryFuse::new(backend, store.into()).unwrap();

    let root: Vec<String> = fuse
        .readdir_entries(ROOT_INO)
        .unwrap()
        .into_iter()
        .map(|(_, _, n)| n)
        .collect();
    assert!(root.contains(&"a.md".to_string()) && root.contains(&"sub".to_string()));
    assert!(root.contains(&".".to_string()) && root.contains(&"..".to_string()));

    // an unknown inode and a FILE inode both reject as not-a-directory.
    assert!(matches!(
        fuse.readdir_entries(9999),
        Err(FuseError::NotFound(_))
    ));
    let (file_ino, _) = fuse.lookup_path("/a.md").unwrap();
    assert!(matches!(
        fuse.readdir_entries(file_ino),
        Err(FuseError::NotFound(_))
    ));

    // the synthetic subdirectory lists its child.
    let (dir_ino, _) = fuse.lookup_path("/sub").unwrap();
    let sub: Vec<String> = fuse
        .readdir_entries(dir_ino)
        .unwrap()
        .into_iter()
        .map(|(_, _, n)| n)
        .collect();
    assert!(sub.contains(&"b.md".to_string()));
}

#[test]
fn apply_setattr_handles_metadata_fd_and_store_truncation() {
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/f.md", "hello"))
        .unwrap();
    let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
    let (ino, _) = fuse.lookup_path("/f.md").unwrap();

    // size = None → metadata-only, returns the current node unchanged.
    assert_eq!(fuse.apply_setattr(ino, None, None).unwrap().size, 5);

    // no fh, no open writer → truncate the store directly.
    assert_eq!(fuse.apply_setattr(ino, Some(2), None).unwrap().size, 2);
    assert_eq!(
        rt.block_on(backend.get_by_path(store, "/f.md"))
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("he")
    );

    // with an open fh → resize that fd's buffer.
    let fh = fuse.open_file_for_path("/f.md", 0).unwrap();
    assert_eq!(fuse.apply_setattr(ino, Some(1), Some(fh)).unwrap().size, 1);
    fuse.remove_fh(fh);

    // an unknown inode → NotFound.
    assert!(matches!(
        fuse.apply_setattr(9999, Some(0), None),
        Err(FuseError::NotFound(_))
    ));
}

#[test]
fn an_open_fd_survives_rename_and_flushes_to_the_renamed_memory() {
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/old.md", "snapshot"))
        .unwrap();

    let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
    let fh = fuse.open_file_for_path("/old.md", 0).unwrap();

    // Rename the memory while the fd is open (through the fs, as the FUSE rename
    // does); the fd keeps its buffer and id.
    rt.block_on(backend.rename(store, "/old.md", "/new.md"))
        .unwrap();
    assert_eq!(
        fuse.read_fh_bytes(fh, 0, 64).unwrap(),
        b"snapshot",
        "the open fd still reads its captured content after rename"
    );

    // A write + close-flush on that fd lands on the renamed memory (same id,
    // unchanged sha, so the CAS succeeds).
    fuse.write_fh_bytes(fh, 0, b"after-rename").unwrap();
    fuse.flush_fh(fh).expect("flush after rename succeeds");
    let moved = rt
        .block_on(backend.get_by_path(store, "/new.md"))
        .unwrap()
        .unwrap();
    assert_eq!(moved.content.as_deref(), Some("after-rename"));
    assert!(
        rt.block_on(backend.get_by_path(store, "/old.md"))
            .unwrap()
            .is_none(),
        "the old path is gone"
    );
}

#[test]
fn child_path_joins_valid_names_and_rejects_traversal() {
    // Path safety: a child name must be a single, non-empty segment. `.`/`..`/an
    // embedded slash are rejected so a lookup/create/rename cannot escape the store.
    assert_eq!(
        MemoryFuse::child_path("/", OsStr::new("a.md")).as_deref(),
        Some("/a.md")
    );
    assert_eq!(
        MemoryFuse::child_path("/notes", OsStr::new("a.md")).as_deref(),
        Some("/notes/a.md")
    );
    // A trailing slash on the parent does not double up.
    assert_eq!(
        MemoryFuse::child_path("/notes/", OsStr::new("a.md")).as_deref(),
        Some("/notes/a.md")
    );
    for bad in ["", "a/b", ".", ".."] {
        assert_eq!(
            MemoryFuse::child_path("/", OsStr::new(bad)),
            None,
            "{bad:?} must be rejected"
        );
    }
}

#[test]
fn write_and_read_at_offsets_splice_and_clamp_the_fd_buffer() {
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/f.md", "abcdef"))
        .unwrap();
    let fuse = MemoryFuse::new(backend, store.into()).unwrap();
    let fh = fuse.open_file_for_path("/f.md", 0).unwrap();

    // An overwrite in the middle of the buffer.
    fuse.write_fh_bytes(fh, 2, b"XY").unwrap();
    assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"abXYef");
    // A write past the end zero-fills the gap.
    fuse.write_fh_bytes(fh, 8, b"Z").unwrap();
    assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"abXYef\0\0Z");
    // A read clamps: an offset past the end yields nothing…
    assert_eq!(fuse.read_fh_bytes(fh, 100, 10).unwrap(), b"");
    // …and a size that overruns the end is truncated to what exists.
    assert_eq!(fuse.read_fh_bytes(fh, 6, 100).unwrap(), b"\0\0Z");
    fuse.remove_fh(fh);
}

#[test]
fn a_flush_conflict_keeps_the_buffer_dirty_for_a_re_drive() {
    // On a CAS conflict the flush returns EAGAIN but must NOT drop the buffer or
    // mark the fd clean — the agent's write is preserved for a re-drive, and a
    // second flush conflicts again (proving the fd was never silently cleared).
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    let created = rt.block_on(backend.create(store, "/c.md", "v1")).unwrap();
    let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
    let fh = fuse.open_file_for_path("/c.md", 0).unwrap();

    // Advance the store out from under the open fd, then write + flush the stale fd.
    rt.block_on(backend.update(store, &created.id, "server", &created.content_sha256))
        .unwrap();
    fuse.write_fh_bytes(fh, 0, b"mine").unwrap();
    assert!(matches!(
        fuse.flush_fh(fh),
        Err(FuseError::Mem(MemErr::Conflict { .. }))
    ));
    // The buffer survives the conflict…
    assert_eq!(fuse.read_fh_bytes(fh, 0, 64).unwrap(), b"mine");
    // …and the fd is still dirty, so re-flushing conflicts again (not a clean no-op).
    assert!(matches!(
        fuse.flush_fh(fh),
        Err(FuseError::Mem(MemErr::Conflict { .. }))
    ));
    fuse.remove_fh(fh);
}

#[test]
fn apply_setattr_grow_zero_fills_through_the_store() {
    // Truncation that GROWS a file (no open fd) zero-fills to the new size and
    // writes it through the store — the mirror of the shrink case.
    let store = "s";
    let backend = Arc::new(VolatileMemoryRepository::new());
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/g.md", "hi")).unwrap();
    let fuse = MemoryFuse::new(backend.clone(), store.into()).unwrap();
    let (ino, _) = fuse.lookup_path("/g.md").unwrap();

    assert_eq!(fuse.apply_setattr(ino, Some(5), None).unwrap().size, 5);
    assert_eq!(
        rt.block_on(backend.get_by_path(store, "/g.md"))
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("hi\0\0\0"),
        "grow zero-fills to the requested length"
    );
}

#[test]
fn content_lru_handles_zero_capacity_prefix_clear_and_removal() {
    // A zero-capacity cache never stores anything (defensive: a misconfigured cap
    // must not panic on eviction).
    let mut zero = ContentLruCache::new(0);
    zero.put(memory("/a.md", "a"));
    assert!(zero.get("/a.md").is_none());

    let mut cache = ContentLruCache::new(8);
    cache.put(memory("/notes/a.md", "a"));
    cache.put(memory("/notes/deep/b.md", "b"));
    cache.put(memory("/other.md", "o"));
    // `clear_path_prefix` (used by rmdir) drops only the matching subtree.
    cache.clear_path_prefix("/notes/");
    assert!(cache.get("/notes/a.md").is_none());
    assert!(cache.get("/notes/deep/b.md").is_none());
    assert!(cache.get("/other.md").is_some(), "a sibling survives");
    // `remove_path` drops one entry; `clear` drops all.
    cache.remove_path("/other.md");
    assert!(cache.get("/other.md").is_none());
    cache.put(memory("/x.md", "x"));
    cache.clear();
    assert!(cache.get("/x.md").is_none());
}

#[test]
fn a_lagged_listener_clears_the_whole_cache() {
    use crate::LocalInvalidator;
    use crate::invalidate::Invalidator;
    // A listener that fell behind by more than the bus capacity has missed
    // invalidations, so it cannot know which paths are stale — it clears the
    // whole cache rather than risk serving a stale entry.
    let cache = Arc::new(Mutex::new(ContentLruCache::new(8)));
    cache.lock().unwrap().put(memory("/a.md", "a"));
    cache.lock().unwrap().put(memory("/b.md", "b"));

    // Capacity-2 bus; publish far more than that BEFORE the listener drains, so
    // its first `try_recv` observes `Lagged`.
    let bus = LocalInvalidator::new(2);
    let rx = bus.subscribe();
    for i in 0..20 {
        bus.publish("s", &format!("/p{i}.md"));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let (cache, stop) = (cache.clone(), stop.clone());
        std::thread::spawn(move || run_invalidation_listener(cache, "s".into(), rx, stop))
    };

    let mut waited = Duration::ZERO;
    while cache.lock().unwrap().get("/a.md").is_some() && waited < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
        waited += Duration::from_millis(10);
    }
    assert!(
        cache.lock().unwrap().get("/a.md").is_none(),
        "a lagged listener clears the cache"
    );
    assert!(cache.lock().unwrap().get("/b.md").is_none());

    stop.store(true, Ordering::SeqCst);
    listener.join().unwrap();
}

#[test]
fn cross_host_write_invalidates_a_peer_mounts_cache_end_to_end() {
    use crate::LocalInvalidator;
    use crate::invalidate::{InvalidatingMemoryRepository, Invalidator};
    // The D5 coherence model end-to-end, WITHOUT the kernel FUSE path: model two
    // hosts mounting one store over the in-process `VolatileMemoryRepository`. Host A writes through
    // an `InvalidatingMemoryRepository` (the real write side); host B keeps its own
    // `ContentLruCache` fed by a listener draining the shared `LocalInvalidator`. A
    // write on A must drop the path from B's cache so B's next read refetches the new
    // content from the shared durable store — coherence with no shared mount.
    let rt = setup_rt();
    let durable = Arc::new(VolatileMemoryRepository::new());
    let bus = Arc::new(LocalInvalidator::new(16));
    let inval: Arc<dyn Invalidator> = bus.clone();
    let fs_a = InvalidatingMemoryRepository::new(durable.clone(), inval);

    // Seed /note.md and let host B cache it (as if B had just read it).
    let seed = rt.block_on(fs_a.create("s", "/note.md", "v1")).unwrap();
    let cache_b = Arc::new(Mutex::new(ContentLruCache::new(8)));
    cache_b.lock().unwrap().put(seed.clone());
    assert_eq!(
        cache_b
            .lock()
            .unwrap()
            .get("/note.md")
            .unwrap()
            .content
            .as_deref(),
        Some("v1"),
        "B has cached the seed content"
    );

    // Projection B's listener drains the shared bus into its cache. It
    // subscribes AFTER the seed create, so it only sees the write below.
    let stop = Arc::new(AtomicBool::new(false));
    let listener = {
        let (cache, stop, rx) = (cache_b.clone(), stop.clone(), bus.subscribe());
        std::thread::spawn(move || run_invalidation_listener(cache, "s".into(), rx, stop))
    };

    // Host A updates /note.md → the write publishes an invalidation B must observe.
    rt.block_on(fs_a.update("s", &seed.id, "v2", &seed.content_sha256))
        .unwrap();

    // B's cache drops the stale path (bounded poll on the real condition — the file's
    // established idiom for the async listener, not a fixed ordering sleep).
    let mut waited = Duration::ZERO;
    while cache_b.lock().unwrap().get("/note.md").is_some() && waited < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(10));
        waited += Duration::from_millis(10);
    }
    assert!(
        cache_b.lock().unwrap().get("/note.md").is_none(),
        "the peer mount dropped the written path from its cache"
    );

    // A refetch on B (now a cache miss) sees host A's new content from the shared store.
    let refetched = rt
        .block_on(durable.get_by_path("s", "/note.md"))
        .unwrap()
        .unwrap();
    assert_eq!(
        refetched.content.as_deref(),
        Some("v2"),
        "the refetch after invalidation sees A's write"
    );

    stop.store(true, Ordering::SeqCst);
    listener.join().unwrap();
}

#[test]
fn unmount_drain_timeout_elapses_with_lingering_open_fds() {
    // The bounded unmount drain: `unmount` waits up to `drain_timeout` for open fds to
    // close, then warns and proceeds — it must never hang. Drive the timeout branch
    // deterministically by injecting a lingering fd (open_fds pinned at 1, never
    // drains) and a tiny timeout; no kernel session (`None`), so `join` is skipped.
    let state = Arc::new(MountState::default());
    state.open_fds.store(1, Ordering::SeqCst);
    let mut handle = MemoryMountHandle {
        session: None,
        state: state.clone(),
        drain_timeout: Duration::from_millis(30),
        listener: None,
    };
    let start = Instant::now();
    handle.unmount();
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(30),
        "unmount waited out the full drain timeout: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "but returned promptly after the timeout, never hung: {elapsed:?}"
    );
    assert_eq!(
        state.open_fds.load(Ordering::SeqCst),
        1,
        "the lingering fd is left as-is (drain warns, it does not force-close)"
    );
}

#[test]
fn a_closed_bus_stops_the_listener_without_the_stop_flag() {
    use crate::LocalInvalidator;
    // Dropping the sender closes the bus; the listener observes `Closed` and exits
    // on its own (the drop-without-explicit-unmount path relies on this).
    let cache = Arc::new(Mutex::new(ContentLruCache::new(4)));
    let bus = LocalInvalidator::new(4);
    let rx = bus.subscribe();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let (cache, stop) = (cache.clone(), stop.clone());
        std::thread::spawn(move || run_invalidation_listener(cache, "s".into(), rx, stop))
    };

    drop(bus);
    let start = std::time::Instant::now();
    handle.join().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "a closed bus exits the listener promptly"
    );
    assert!(
        !stop.load(Ordering::SeqCst),
        "it exited via Closed, not the stop flag"
    );
}
