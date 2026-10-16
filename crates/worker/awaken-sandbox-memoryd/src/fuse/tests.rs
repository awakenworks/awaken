// @formal evidence memory_invalidation.listener_cleanup_is_owned_and_joined
use super::*;
use awaken_memory_store::{VolatileMemoryRepository, sha256_hex};
use awaken_resource_contract::MemoryWriter as _;

fn test_fs() -> MemoryFuse {
    MemoryFuse::new(
        Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock())),
        "memstore_test".into(),
        &*awaken_time::system_clock(),
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
fn listener_cleanup_action_covers_owned_absent_and_repeated_decision_rows() {
    // Cause/effect graph: C1 an owned handle is present; C2 the owner has no
    // handle; C3 cleanup is repeated after C1. E1 every call requests stop;
    // E2 C1 joins the exact handle after E1 and consumes it; E3 C2/C3 never
    // join; E4 cleanup returns only after the join callback returns. Decision
    // table: R1=C1 -> E1+E2+E4; R2=C2 -> E1+E3; R3=C1+C3 -> E1+E2 once,
    // then E1+E3. Removing/reordering stop, detaching instead of invoking join,
    // retaining the Option or invoking join twice must break these rows.
    for initial in [Some(41_u64), None] {
        let had_handle = initial.is_some();
        let mut handle = initial;
        let events = std::cell::RefCell::new(Vec::new());
        stop_and_join_owned_listener(
            &mut handle,
            || events.borrow_mut().push("stop"),
            |joined| {
                assert_eq!(joined, 41, "R1/E2 exact owned handle");
                events.borrow_mut().push("join");
            },
        );
        assert!(handle.is_none(), "R1-R2 ownership consumed or absent");
        assert_eq!(
            *events.borrow(),
            if had_handle {
                vec!["stop", "join"]
            } else {
                vec!["stop"]
            },
            "R1-R2/E1-E4"
        );

        stop_and_join_owned_listener(
            &mut handle,
            || events.borrow_mut().push("stop"),
            |_| panic!("R3/E3 consumed or absent handle cannot join"),
        );
        assert!(handle.is_none(), "R3 keeps the handle consumed");
        assert_eq!(
            *events.borrow(),
            if had_handle {
                vec!["stop", "join", "stop"]
            } else {
                vec!["stop", "stop"]
            },
            "R3/E1+E3"
        );
    }
}

#[test]
fn failed_kernel_mount_releases_only_its_invalidation_listener() {
    // R1: listener started + kernel setup error + live bus/peer => error returns
    // only after this subscriber exits; peer remains usable. Repeated failures
    // must not accumulate subscribers. The missing child is intentional: this
    // exercises the real setup error even without /dev/fuse, not a skipped mount.
    let root = tempfile::tempdir().unwrap();
    let (bus, mut peer) = broadcast::channel(8);
    for _ in 0..8 {
        let result = spawn_mount_with_invalidations(
            Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock())),
            "s".into(),
            root.path().join("missing-parent/mount"),
            bus.subscribe(),
            &*awaken_time::system_clock(),
        );
        assert!(result.is_err(), "nonexistent mountpoint cannot be mounted");
        assert_eq!(bus.receiver_count(), 1, "only the peer remains");
        let event = ("s".into(), "/still-live.md".into());
        bus.send(event.clone()).unwrap();
        assert_eq!(peer.try_recv().unwrap(), event);
    }
}

#[test]
fn listener_owner_cleanup_covers_exit_and_disposal_decision_table() {
    // R2-R19: (polling / bus closed / thread panicked) x (empty / queued bus)
    // x (handle Drop / unmount / repeated unmount then Drop). Every row consumes
    // the same listener exactly once, requests stop and releases its receiver.
    // No kernel session is needed to test listener ownership. Thread panic is a
    // terminal join result, never a second panic from cleanup.
    for exit in ["polling", "closed", "panic"] {
        for queued in [false, true] {
            for dispose in ["drop", "unmount", "repeat"] {
                let (bus, rx) = broadcast::channel(1);
                if queued {
                    bus.send(("s".into(), "/a.md".into())).unwrap();
                    bus.send(("s".into(), "/b.md".into())).unwrap();
                }
                let stop = Arc::new(AtomicBool::new(false));
                let thread_stop = stop.clone();
                let worker = std::thread::spawn(move || {
                    if exit == "panic" {
                        // Keep rx live through the injected failure.
                        let _rx = rx;
                        panic!("injected invalidation thread failure");
                    }
                    run_invalidation_listener(
                        Arc::new(Mutex::new(ContentLruCache::new(4))),
                        "s".into(),
                        rx,
                        thread_stop,
                    );
                });
                let mut handle = MemoryMountHandle {
                    session: None,
                    state: Arc::new(MountState::default()),
                    drain_timeout: Duration::ZERO,
                    listener: Some(InvalidationListener {
                        stop: stop.clone(),
                        handle: Some(worker),
                    }),
                    cache_witness: MountedCacheWitness(std::sync::Weak::new()),
                };
                let bus = if exit == "closed" {
                    drop(bus);
                    None
                } else {
                    Some(bus)
                };
                if exit != "polling" {
                    // Force the early-exit-before-owner-cleanup ordering; a
                    // closed bus alone would still race the owner's stop flag.
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !handle
                        .listener
                        .as_ref()
                        .unwrap()
                        .handle
                        .as_ref()
                        .unwrap()
                        .is_finished()
                    {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "{exit} worker did not exit"
                        );
                        std::thread::yield_now();
                    }
                    assert!(!stop.load(Ordering::SeqCst));
                }
                if dispose != "drop" {
                    handle.unmount();
                    assert!(handle.listener.is_none());
                    if dispose == "repeat" {
                        handle.unmount();
                    }
                }
                drop(handle);
                assert!(stop.load(Ordering::SeqCst));
                if let Some(bus) = bus {
                    assert_eq!(bus.receiver_count(), 0, "{exit}/{queued}/{dispose}");
                }
            }
        }
    }
}

#[test]
fn listener_drop_waits_for_exit_not_only_stop_acknowledgement() {
    // R20: stop observed but worker still running => destructor cannot return;
    // after worker exit => join returns. Channels fix the ordering, without a
    // sleep-as-race oracle. This complements the abstract Stop/Exit/Join model.
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let (observed_tx, observed_rx) = std::sync::mpsc::channel();
    let (exit_tx, exit_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        while !worker_stop.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        observed_tx.send(()).unwrap();
        exit_rx.recv().unwrap();
    });
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let disposer = std::thread::spawn(move || {
        drop(InvalidationListener {
            stop,
            handle: Some(worker),
        });
        done_tx.send(()).unwrap();
    });
    observed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        done_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    exit_tx.send(()).unwrap();
    done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    disposer.join().unwrap();
}

#[test]
fn listener_setup_unwind_keeps_the_same_cleanup_owner() {
    // R21: unwind after thread creation, before a MemoryMountHandle exists =>
    // the local listener guard still stops and joins; the caller sees its panic.
    let (bus, rx) = broadcast::channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let worker = std::thread::spawn(move || {
        run_invalidation_listener(
            Arc::new(Mutex::new(ContentLruCache::new(4))),
            "s".into(),
            rx,
            thread_stop,
        );
    });
    let owner_stop = stop.clone();
    let result = std::thread::spawn(move || {
        let _listener = InvalidationListener {
            stop: owner_stop,
            handle: Some(worker),
        };
        panic!("injected setup unwind");
    })
    .join();
    assert!(result.is_err());
    assert!(stop.load(Ordering::SeqCst));
    assert_eq!(bus.receiver_count(), 0);
}

#[test]
fn invalidation_listener_drops_matching_paths_only() {
    use crate::LocalInvalidator;
    use awaken_resource_contract::MemoryInvalidator;

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
    // R1 full-precision injected mount time -> the synthetic root's created
    // and updated attributes preserve that same reading. Record-backed file
    // attributes remain governed by the adjacent record-timestamp test.
    let reading = awaken_time::UtcTime::from_unix_nanos(1_234_567_890);
    let clock = awaken_lease_testkit::FixedClock::at(reading);
    let fs = MemoryFuse::new(
        Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock())),
        "s".into(),
        &clock,
    )
    .unwrap();
    let root = fs.node(ROOT_INO).expect("root node");
    assert_eq!(root.kind, FileType::Directory);
    let attr = MemoryFuse::attr(ROOT_INO, &root);
    assert_eq!(attr.crtime, to_systime(reading.as_unix_nanos()));
    assert_eq!(attr.mtime, attr.crtime);
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
        generation: MemoryGeneration::new("m1", 1, sha256_hex("one")),
        buffer: b"one".to_vec(),
        dirty: false,
    });
    let b = fs.allocate_fh(OpenFile {
        generation: MemoryGeneration::new("m2", 1, sha256_hex("two")),
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
            generation: MemoryGeneration::new(format!("m{index}"), 1, sha256_hex("clean")),
            buffer: Vec::new(),
            dirty: false,
        });
        fs.write_fh_bytes(fh, 0, b"x").expect("dirty fd admitted");
    }
    let blocked = fs.allocate_fh(OpenFile {
        generation: MemoryGeneration::new("blocked", 1, sha256_hex("blocked")),
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    let created = rt.block_on(backend.create(store, "/c.md", "v1")).unwrap();

    let fuse =
        MemoryFuse::new(backend.clone(), store.into(), &*awaken_time::system_clock()).unwrap();
    // Open captures base sha = sha("v1").
    let fh = fuse.open_file_for_path("/c.md", 0).unwrap();

    // Another writer advances the store out from under the open fd.
    rt.block_on(backend.update(store, &created.generation(), "server-wins"))
        .unwrap();

    // The stale fd writes and flushes: CAS on the now-stale generation conflicts,
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
        .block_on(MemoryFilesystem::get_by_path(
            backend.as_ref(),
            store,
            "/c.md",
        ))
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/notes/a.md", "a"))
        .unwrap();

    let fuse = MemoryFuse::new(backend, store.into(), &*awaken_time::system_clock()).unwrap();
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/f.md", "orig")).unwrap();
    let fuse =
        MemoryFuse::new(backend.clone(), store.into(), &*awaken_time::system_clock()).unwrap();

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
        rt.block_on(MemoryFilesystem::get_by_path(
            backend.as_ref(),
            store,
            "/f.md"
        ))
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/a.md", "a")).unwrap();
    rt.block_on(backend.create(store, "/sub/b.md", "b"))
        .unwrap();
    let fuse = MemoryFuse::new(backend, store.into(), &*awaken_time::system_clock()).unwrap();

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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/f.md", "hello"))
        .unwrap();
    let fuse =
        MemoryFuse::new(backend.clone(), store.into(), &*awaken_time::system_clock()).unwrap();
    let (ino, _) = fuse.lookup_path("/f.md").unwrap();

    // size = None → metadata-only, returns the current node unchanged.
    assert_eq!(fuse.apply_setattr(ino, None, None).unwrap().size, 5);

    // no fh, no open writer → truncate the store directly.
    assert_eq!(fuse.apply_setattr(ino, Some(2), None).unwrap().size, 2);
    assert_eq!(
        rt.block_on(MemoryFilesystem::get_by_path(
            backend.as_ref(),
            store,
            "/f.md"
        ))
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/old.md", "snapshot"))
        .unwrap();

    let fuse =
        MemoryFuse::new(backend.clone(), store.into(), &*awaken_time::system_clock()).unwrap();
    let fh = fuse.open_file_for_path("/old.md", 0).unwrap();

    // Cause/effect rule FR1: a FUSE-owned rename advances the durable generation
    // and refreshes every open descriptor for the same immutable id; its buffer
    // survives and a later flush follows the renamed head. An external rename is
    // covered separately by the stale-generation conflict tests.
    fuse.rename_path("/old.md", "/new.md").unwrap();
    assert_eq!(
        fuse.read_fh_bytes(fh, 0, 64).unwrap(),
        b"snapshot",
        "the open fd still reads its captured content after rename"
    );

    // FR1 effect: a write + close-flush uses the refreshed complete generation.
    fuse.write_fh_bytes(fh, 0, b"after-rename").unwrap();
    fuse.flush_fh(fh).expect("flush after rename succeeds");
    let moved = rt
        .block_on(MemoryFilesystem::get_by_path(
            backend.as_ref(),
            store,
            "/new.md",
        ))
        .unwrap()
        .unwrap();
    assert_eq!(moved.content.as_deref(), Some("after-rename"));
    assert!(
        rt.block_on(MemoryFilesystem::get_by_path(
            backend.as_ref(),
            store,
            "/old.md"
        ))
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/f.md", "abcdef"))
        .unwrap();
    let fuse = MemoryFuse::new(backend, store.into(), &*awaken_time::system_clock()).unwrap();
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    let created = rt.block_on(backend.create(store, "/c.md", "v1")).unwrap();
    let fuse =
        MemoryFuse::new(backend.clone(), store.into(), &*awaken_time::system_clock()).unwrap();
    let fh = fuse.open_file_for_path("/c.md", 0).unwrap();

    // Advance the store out from under the open fd, then write + flush the stale fd.
    rt.block_on(backend.update(store, &created.generation(), "server"))
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
    let backend = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let rt = setup_rt();
    rt.block_on(backend.create(store, "/g.md", "hi")).unwrap();
    let fuse =
        MemoryFuse::new(backend.clone(), store.into(), &*awaken_time::system_clock()).unwrap();
    let (ino, _) = fuse.lookup_path("/g.md").unwrap();

    assert_eq!(fuse.apply_setattr(ino, Some(5), None).unwrap().size, 5);
    assert_eq!(
        rt.block_on(MemoryFilesystem::get_by_path(
            backend.as_ref(),
            store,
            "/g.md"
        ))
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
    use awaken_resource_contract::MemoryInvalidator;
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
    use awaken_resource_application::memory::InvalidatingMemoryRepository;
    use awaken_resource_contract::MemoryInvalidator;
    // The D5 coherence model end-to-end, WITHOUT the kernel FUSE path: model two
    // hosts mounting one store over the in-process `VolatileMemoryRepository`. Host A writes through
    // an `InvalidatingMemoryRepository` (the real write side); host B keeps its own
    // `ContentLruCache` fed by a listener draining the shared `LocalInvalidator`. A
    // write on A must drop the path from B's cache so B's next read refetches the new
    // content from the shared durable store — coherence with no shared mount.
    let rt = setup_rt();
    let durable = Arc::new(VolatileMemoryRepository::new(awaken_time::system_clock()));
    let bus = Arc::new(LocalInvalidator::new(16));
    let inval: Arc<dyn MemoryInvalidator> = bus.clone();
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
    rt.block_on(fs_a.update("s", &seed.generation(), "v2"))
        .unwrap();

    // B's cache drops the stale path (bounded poll on the real condition — the file's
    // established idiom for the async listener, not a fixed ordering sleep).
    let mut waited = Duration::ZERO;
    while cache_b.lock().unwrap().entries.contains_key("/note.md")
        && waited < Duration::from_secs(2)
    {
        std::thread::sleep(Duration::from_millis(10));
        waited += Duration::from_millis(10);
    }
    assert!(
        !cache_b.lock().unwrap().entries.contains_key("/note.md"),
        "the peer mount dropped the written path from its cache"
    );

    // A refetch on B (now a cache miss) sees host A's new content from the shared store.
    let refetched = rt
        .block_on(MemoryFilesystem::get_by_path(
            durable.as_ref(),
            "s",
            "/note.md",
        ))
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
        cache_witness: MountedCacheWitness(std::sync::Weak::new()),
    };
    let start = std::time::Instant::now();
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

/// Public HTTP mutations reach the selected mounter's bus and the real listener
/// algorithm. This isolated test warms a separate cache; the actual mount wiring
/// is exercised by `managed_api_writes_invalidate_actual_kernel_mount_without_cache_ttl`.
/// Raw membership (not TTL-aware `get`) proves deletion in either case.
#[test]
fn managed_api_writes_invalidate_the_same_mounter_without_waiting_for_cache_ttl() {
    use awaken_protocol_managed::types::memory::AuthenticatedMemoryActor;
    use awaken_resource_application::{
        RegistryApplication, ResourceAuthorities, ResourcesApplication,
    };
    use awaken_resource_contract::{MemoryActor, MemoryVersionOperation};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    let rt = setup_rt();
    let clock = awaken_time::system_clock();
    let bus = Arc::new(crate::LocalInvalidator::new(32));
    let physical = Arc::new(VolatileMemoryRepository::new(clock.clone()));
    let resources = rt.block_on(async {
        let storage = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .unwrap(),
        );
        let registry = Arc::new(RegistryApplication::new(storage.clone(), clock.clone()));
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        ResourcesApplication::new(ResourceAuthorities::new(
            registry,
            files.clone(),
            files,
            Arc::new(
                awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                    physical,
                    bus.clone(),
                ),
            ),
            Arc::new(awaken_skill_store::InMemorySkillStore::new()),
            storage,
            clock.clone(),
        ))
    });
    let ports = resources.authorities();
    let catalog = ports.resource_registry();
    assert!(Arc::ptr_eq(
        &catalog,
        &resources.authorities().resource_registry()
    ));
    let physical = Arc::as_ptr(&ports.memory_queries()) as *const ();
    assert_eq!(physical, Arc::as_ptr(&ports.memory_writer()) as *const ());
    assert_eq!(
        physical,
        Arc::as_ptr(&ports.memory_snapshot_source()) as *const ()
    );
    assert_eq!(
        physical,
        Arc::as_ptr(&ports.memory_filesystem()) as *const ()
    );
    assert_eq!(
        physical,
        Arc::as_ptr(&ports.memory_copy_writeback()) as *const ()
    );
    assert_eq!(
        physical,
        Arc::as_ptr(&ports.memory_copy_conflict_queries()) as *const ()
    );
    assert_eq!(
        physical,
        Arc::as_ptr(&ports.memory_copy_conflict_preservation()) as *const ()
    );
    assert_eq!(physical, Arc::as_ptr(&ports.memory_versions()) as *const ());
    assert_eq!(
        physical,
        Arc::as_ptr(&ports.memory_version_redaction()) as *const ()
    );
    assert_eq!(
        physical,
        Arc::as_ptr(&ports.memory_content_purge()) as *const ()
    );

    let router = awaken_protocol_managed::memory_stores_router(
        ports.memory_management_content(),
        resources.memory_stores(),
        clock.clone(),
    );
    let mounter =
        crate::MemoryStoreMounter::fuse_preferred(ports.memory_mount_source(), bus.clone(), clock)
            .expect("selected Memory source and root bus match");
    assert!(
        mounter.shares_invalidator_for_test(&bus),
        "the selected mounter retains the root bus Arc"
    );
    let request = |method: &str, path: &str, body: Value| {
        let router = router.clone();
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        req.extensions_mut()
            .insert(awaken_tenancy::WorkspaceScope("cache-workspace".into()));
        req.extensions_mut()
            .insert(AuthenticatedMemoryActor(MemoryActor::ApiActor {
                api_key_id: "cache-api".into(),
            }));
        async move {
            let response = router.oneshot(req).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let (status, store) = rt.block_on(request(
        "POST",
        "/v1/memory_stores",
        json!({"name":"cache"}),
    ));
    assert_eq!(status, StatusCode::OK);
    let store = store["id"].as_str().unwrap();
    assert!(
        rt.block_on(catalog.find_memory_store("cache-workspace", store))
            .unwrap()
            .is_some(),
        "public API authored the same exact root catalog"
    );
    let mut observer = bus.subscribe();
    let (status, created) = rt.block_on(request(
        "POST",
        &format!("/v1/memory_stores/{store}/memories"),
        json!({"path":"/note.md","content":"v1"}),
    ));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        observer.try_recv().unwrap(),
        (store.into(), "/note.md".into())
    );
    assert!(
        matches!(
            observer.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "one successful create emits once"
    );
    let seed = rt
        .block_on(ports.memory_queries().get_by_path(store, "/note.md"))
        .unwrap()
        .unwrap();
    let cache = Arc::new(Mutex::new(ContentLruCache::new(8)));
    let assert_store_cache_cleared = |operation: &str| {
        let started = std::time::Instant::now();
        while !cache.lock().unwrap().entries.is_empty()
            && started.elapsed() < Duration::from_secs(2)
        {
            std::thread::yield_now();
        }
        assert!(
            cache.lock().unwrap().entries.is_empty(),
            "{operation} clears raw membership for the entire store, independently of TTL"
        );
    };
    cache.lock().unwrap().put(seed.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let listener = InvalidationListener {
        stop: stop.clone(),
        handle: Some({
            let (cache, stop, rx, store) = (
                cache.clone(),
                stop.clone(),
                mounter.invalidations_for_test(),
                store.to_owned(),
            );
            std::thread::spawn(move || run_invalidation_listener(cache, store, rx, stop))
        }),
    };
    let uri = format!(
        "/v1/memory_stores/{store}/memories/{}",
        created["id"].as_str().unwrap()
    );
    let (status, _) = rt.block_on(request("PATCH", &uri, json!({"content":"v2", "precondition":{"type":"content_sha256", "content_sha256":seed.content_sha256}})));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        observer.try_recv().unwrap(),
        (store.into(), "/note.md".into())
    );
    assert!(matches!(
        observer.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let started = std::time::Instant::now();
    while cache.lock().unwrap().entries.contains_key("/note.md")
        && started.elapsed() < Duration::from_secs(2)
    {
        std::thread::yield_now();
    }
    assert!(
        !cache.lock().unwrap().entries.contains_key("/note.md"),
        "listener removed raw membership; TTL cannot pass this assertion"
    );
    let current = rt
        .block_on(ports.memory_queries().get_by_path(store, "/note.md"))
        .unwrap()
        .unwrap();
    assert_eq!(current.content.as_deref(), Some("v2"));
    cache.lock().unwrap().put(current.clone());
    let (status, _) = rt.block_on(request("PATCH", &uri, json!({"content":"loser", "precondition":{"type":"content_sha256", "content_sha256":seed.content_sha256}})));
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        matches!(
            observer.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "failed CAS emits nothing"
    );
    assert!(cache.lock().unwrap().entries.contains_key("/note.md"));
    let (status, _) = rt.block_on(request(
        "PATCH",
        &uri,
        json!({"path":"/moved.md", "content":"v3"}),
    ));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(observer.try_recv().unwrap(), (store.into(), "/".into()));
    assert!(matches!(
        observer.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_store_cache_cleared("API path update");
    let moved = rt
        .block_on(ports.memory_queries().get_by_path(store, "/moved.md"))
        .unwrap()
        .unwrap();
    cache.lock().unwrap().put(moved);
    cache
        .lock()
        .unwrap()
        .put(memory("/unrelated.md", "cached peer path"));
    rt.block_on(
        ports
            .memory_filesystem()
            .rename(store, "/moved.md", "/renamed.md"),
    )
    .unwrap();
    assert_eq!(observer.try_recv().unwrap(), (store.into(), "/".into()));
    assert!(matches!(
        observer.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_store_cache_cleared("filesystem rename");
    let renamed = rt
        .block_on(ports.memory_queries().get_by_path(store, "/renamed.md"))
        .unwrap()
        .unwrap();
    cache.lock().unwrap().put(renamed);
    cache
        .lock()
        .unwrap()
        .put(memory("/unrelated.md", "cached peer path"));
    let versions = rt
        .block_on(ports.memory_versions().list_versions(store))
        .unwrap();
    let expected_actor = Some(MemoryActor::ApiActor {
        api_key_id: "cache-api".into(),
    });
    for (path, content, operation) in [
        ("/note.md", "v1", MemoryVersionOperation::Created),
        ("/note.md", "v2", MemoryVersionOperation::Modified),
        ("/moved.md", "v3", MemoryVersionOperation::Modified),
    ] {
        let version = versions
            .iter()
            .find(|version| {
                version.path == path
                    && version.content.as_deref() == Some(content)
                    && version.operation == operation
            })
            .expect("every successful HTTP mutation retains its audit version");
        assert_eq!(
            version.created_by, expected_actor,
            "HTTP actor survives the shared decorator"
        );
    }
    rt.block_on(ports.memory_content_purge().purge_store(store))
        .unwrap();
    assert_eq!(observer.try_recv().unwrap(), (store.into(), "/".into()));
    assert!(matches!(
        observer.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    assert_store_cache_cleared("physical purge");
    assert!(
        rt.block_on(ports.memory_snapshot_source().snapshot_heads(store))
            .unwrap()
            .is_empty()
    );
    drop(listener);
}

/// Exercise the actual mounter subscription and kernel cache allocation. Raw
/// membership is observed after the TTL has already elapsed and without any
/// intervening VFS/cache get: expiry cannot stand in for the mounted listener.
#[test]
fn managed_api_writes_invalidate_actual_kernel_mount_without_cache_ttl() {
    use awaken_protocol_managed::types::memory::AuthenticatedMemoryActor;
    use awaken_provisioning_contract::{
        MemoryMountOutcome, MemoryMounter, MountAccess, Realization,
    };
    use awaken_resource_application::{
        RegistryApplication, ResourceAuthorities, ResourcesApplication,
    };
    use awaken_resource_contract::MemoryActor;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    if !crate::fuse_available() {
        eprintln!("SKIP actual kernel cache wiring: /dev/fuse or fusermount unavailable");
        return;
    }
    let rt = setup_rt();
    let clock = awaken_time::system_clock();
    let bus = Arc::new(crate::LocalInvalidator::new(32));
    let resources = rt.block_on(async {
        let storage = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .unwrap(),
        );
        let registry = Arc::new(RegistryApplication::new(storage.clone(), clock.clone()));
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let memory = Arc::new(
            awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                Arc::new(VolatileMemoryRepository::new(clock.clone())),
                bus.clone(),
            ),
        );
        ResourcesApplication::new(ResourceAuthorities::new(
            registry,
            files.clone(),
            files,
            memory,
            Arc::new(awaken_skill_store::InMemorySkillStore::new()),
            storage,
            clock.clone(),
        ))
    });
    let ports = resources.authorities();
    let router = awaken_protocol_managed::memory_stores_router(
        ports.memory_management_content(),
        resources.memory_stores(),
        clock.clone(),
    );
    let request = |method: &str, path: &str, body: Value| {
        let router = router.clone();
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        req.extensions_mut().insert(awaken_tenancy::WorkspaceScope(
            "kernel-cache-workspace".into(),
        ));
        req.extensions_mut()
            .insert(AuthenticatedMemoryActor(MemoryActor::ApiActor {
                api_key_id: "kernel-cache-api".into(),
            }));
        async move {
            let response = router.oneshot(req).await.unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, serde_json::from_slice::<Value>(&bytes).unwrap())
        }
    };
    let (status, store) = rt.block_on(request(
        "POST",
        "/v1/memory_stores",
        json!({"name":"kernel-cache"}),
    ));
    assert_eq!(status, StatusCode::OK);
    let store = store["id"].as_str().unwrap();
    let seed = rt
        .block_on(ports.memory_writer().create(store, "/note.md", "old"))
        .unwrap();
    let mounter =
        crate::MemoryStoreMounter::fuse_preferred(ports.memory_mount_source(), bus.clone(), clock)
            .unwrap();
    assert!(
        mounter.mounted_cache_for_test().is_none(),
        "construction does not create a phantom cache or mount"
    );
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("real-kernel-cache");
    let mount = match rt.block_on(mounter.mount(store, &path, MountAccess::ReadWrite)) {
        MemoryMountOutcome::Mounted(mount) => mount,
        MemoryMountOutcome::NoEffect(error) | MemoryMountOutcome::RecoveryRequired(error) => {
            panic!("actual kernel mount must succeed on a FUSE-capable host: {error}")
        }
    };
    assert_eq!(
        mount.realization(),
        Realization::Fuse,
        "Copy fallback cannot prove mounted listener wiring"
    );
    let cache = mounter
        .mounted_cache_for_test()
        .expect("actual successful mount records its exact cache allocation");
    assert_eq!(
        std::fs::read_to_string(path.join("note.md")).unwrap(),
        "old"
    );
    assert!(
        cache.contains_raw("/note.md"),
        "kernel read warms the observed actual cache"
    );
    std::thread::sleep(CONTENT_CACHE_TTL + Duration::from_millis(50));
    assert!(
        cache.contains_raw("/note.md"),
        "elapsed TTL alone does not evict raw membership"
    );

    let (status, _) = rt.block_on(request("PATCH",
        &format!("/v1/memory_stores/{store}/memories/{}", seed.id),
        json!({"content":"new","precondition":{"type":"content_sha256","content_sha256":seed.content_sha256}}),
    ));
    assert_eq!(status, StatusCode::OK);
    // Do not issue VFS reads or TTL-aware cache lookups until raw removal is
    // proven. No writes/renames/purge/capacity pressure occur on this mount.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while cache.contains_raw("/note.md") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !cache.contains_raw("/note.md"),
        "the listener started by the real mount removes the actual kernel cache entry"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("note.md")).unwrap(),
        "new"
    );
    rt.block_on(mount.teardown()).unwrap();
    assert!(
        !cache.is_live(),
        "Weak observation does not keep the kernel cache or listener alive after teardown"
    );
}
