//! Real kernel-VFS integration test (ADR-0053 P3): mount the FUSE over a durable
//! store, drive real `read`/`write`/`readdir`/`rename`/`unlink` syscalls through the
//! kernel, and prove writes persist across an unmount + fresh remount.
//!
//! Gated on a FUSE-capable host (`/dev/fuse` + `fusermount`). When FUSE is available
//! a mount failure is a hard failure (never a silent skip, per the no-stub rule);
//! when it is absent the test prints why and returns — the copy path is the fallback
//! where FUSE cannot run (CI/macOS).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_memory_store::{FilesystemMemoryRepository, MemoryRepository};
use awaken_sandbox_memoryd::fuse::spawn_mount;
use awaken_sandbox_memoryd::{FuseMountFactory, MountCoordinator};

fn fuse_unavailable_reason() -> Option<String> {
    if !Path::new("/dev/fuse").exists() {
        return Some("/dev/fuse is absent".into());
    }
    let on_path = |bin: &str| {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).exists()))
    };
    if !on_path("fusermount") && !on_path("fusermount3") {
        return Some("fusermount is not on PATH".into());
    }
    None
}

fn unique(tag: &str) -> PathBuf {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("awaken-memfuse-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn kernel_reads_writes_renames_and_persists_across_remount() {
    if let Some(reason) = fuse_unavailable_reason() {
        eprintln!("SKIP kernel_vfs: {reason}");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let store_root = unique("store");
    let mnt = unique("mnt");
    let store = "memstore_1";

    let backend = Arc::new(FilesystemMemoryRepository::open(&store_root).unwrap());
    rt.block_on(backend.create(store, "/seed.md", "hello"))
        .unwrap();
    rt.block_on(backend.create(store, "/notes/a.md", "note-a"))
        .unwrap();

    let handle = spawn_mount(backend.clone(), store.into(), mnt.clone())
        .expect("FUSE mount must succeed on a FUSE-capable host");
    // Give the mount a beat to settle before issuing syscalls.
    std::thread::sleep(Duration::from_millis(100));

    // read — a seeded memory is visible as a file.
    assert_eq!(
        std::fs::read_to_string(mnt.join("seed.md")).unwrap(),
        "hello"
    );

    // readdir — the root lists the file and the synthetic `notes` directory.
    let mut names: Vec<String> = std::fs::read_dir(&mnt)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["notes".to_string(), "seed.md".to_string()]);
    assert_eq!(
        std::fs::read_to_string(mnt.join("notes/a.md")).unwrap(),
        "note-a"
    );

    // create + write — a brand-new file flushes through to the store on close.
    std::fs::write(mnt.join("new.md"), "created via kernel").unwrap();
    // overwrite an existing memory.
    std::fs::write(mnt.join("seed.md"), "overwritten").unwrap();
    // rename through the kernel.
    std::fs::rename(mnt.join("new.md"), mnt.join("renamed.md")).unwrap();
    // unlink.
    std::fs::write(mnt.join("trash.md"), "x").unwrap();
    std::fs::remove_file(mnt.join("trash.md")).unwrap();

    handle.unmount();

    // The store of record holds every write, independent of the mount.
    let content = |p: &str| {
        rt.block_on(backend.get_by_path(store, p))
            .unwrap()
            .and_then(|m| m.content)
    };
    assert_eq!(content("/seed.md").as_deref(), Some("overwritten"));
    assert_eq!(
        content("/renamed.md").as_deref(),
        Some("created via kernel")
    );
    assert_eq!(content("/new.md"), None, "the renamed source is gone");
    assert_eq!(content("/trash.md"), None, "the unlinked file is gone");

    // A fresh handle over the same durable store re-exposes the writes — they were
    // in the store of record, not a first-mount cache artifact.
    let backend2 = Arc::new(FilesystemMemoryRepository::open(&store_root).unwrap());
    let handle2 = spawn_mount(backend2, store.into(), mnt.clone()).expect("remount");
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        std::fs::read_to_string(mnt.join("renamed.md")).unwrap(),
        "created via kernel"
    );
    assert_eq!(
        std::fs::read_to_string(mnt.join("seed.md")).unwrap(),
        "overwritten"
    );
    handle2.unmount();

    std::fs::remove_dir_all(&store_root).ok();
    std::fs::remove_dir_all(&mnt).ok();
}

#[test]
fn a_shared_mount_is_coherent_and_refcounted_across_acquirers() {
    if let Some(reason) = fuse_unavailable_reason() {
        eprintln!("SKIP kernel_vfs coherence: {reason}");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let store_root = unique("cstore");
    let mnt_root = unique("cmnt");
    let store = "memstore_1";

    let backend = Arc::new(FilesystemMemoryRepository::open(&store_root).unwrap());
    rt.block_on(backend.create(store, "/x.md", "one")).unwrap();

    let coord = MountCoordinator::new(Box::new(FuseMountFactory::new(backend.clone(), &mnt_root)));

    // Two acquirers of the same store share ONE mount (one cache) — the D5 model.
    let mp_a = coord.acquire(store).unwrap();
    let mp_b = coord.acquire(store).unwrap();
    assert_eq!(mp_a, mp_b, "both acquirers see the same shared mountpoint");
    assert_eq!(coord.refcount(store), 2);
    std::thread::sleep(Duration::from_millis(100));

    // A write through one view is coherently visible through the other — there is no
    // second cache to go stale.
    std::fs::write(mp_a.join("x.md"), "two").unwrap();
    assert_eq!(
        std::fs::read_to_string(mp_b.join("x.md")).unwrap(),
        "two",
        "the write is coherent across the shared mount"
    );

    // Releasing one reference keeps the mount alive for the other.
    coord.release(store);
    assert_eq!(coord.refcount(store), 1);
    assert_eq!(std::fs::read_to_string(mp_a.join("x.md")).unwrap(), "two");

    // The last release unmounts.
    coord.release(store);
    assert_eq!(
        coord.active_mounts(),
        0,
        "the shared mount is torn down at refcount 0"
    );

    std::fs::remove_dir_all(&store_root).ok();
    std::fs::remove_dir_all(&mnt_root).ok();
}

#[test]
fn kernel_exercises_metadata_truncate_offsets_dirs_and_errors() {
    use std::io::{Read, Seek, SeekFrom, Write};

    if let Some(reason) = fuse_unavailable_reason() {
        eprintln!("SKIP kernel_vfs edges: {reason}");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let store_root = unique("estore");
    let mnt = unique("emnt");
    let store = "memstore_1";

    let backend = Arc::new(FilesystemMemoryRepository::open(&store_root).unwrap());
    rt.block_on(backend.create(store, "/f.md", "0123456789"))
        .unwrap();

    let handle = spawn_mount(backend.clone(), store.into(), mnt.clone())
        .expect("mount")
        .with_drain_timeout(Duration::from_secs(2));
    assert_eq!(handle.open_fd_count(), 0, "no fds open before any syscall");
    std::thread::sleep(Duration::from_millis(100));

    // stat via an OPEN handle (getattr with fh) reports the live size.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(mnt.join("f.md"))
        .unwrap();
    assert_eq!(file.metadata().unwrap().len(), 10);

    // read at an offset (seek + partial read).
    file.seek(SeekFrom::Start(4)).unwrap();
    let mut buf = [0u8; 3];
    file.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"456");

    // write at an offset, then ftruncate (setattr size on an open fd).
    file.seek(SeekFrom::Start(2)).unwrap();
    file.write_all(b"XY").unwrap();
    file.set_len(4).unwrap();
    file.sync_all().unwrap(); // fsync path
    drop(file); // release → flush
    assert_eq!(
        rt.block_on(backend.get_by_path(store, "/f.md"))
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("01XY"),
        "offset write + truncate flushed coherently"
    );

    // stat and read a nonexistent path → ENOENT (lookup error arm).
    assert_eq!(
        std::fs::metadata(mnt.join("ghost.md")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(std::fs::read(mnt.join("ghost.md")).is_err());

    // mkdir a synthetic dir, put a file under it, then rmdir fails (not empty),
    // succeeds once emptied.
    std::fs::create_dir(mnt.join("d")).unwrap();
    std::fs::write(mnt.join("d/inner.md"), "in").unwrap();
    assert!(
        std::fs::remove_dir(mnt.join("d")).is_err(),
        "rmdir a non-empty directory fails"
    );
    std::fs::remove_file(mnt.join("d/inner.md")).unwrap();
    std::fs::remove_dir(mnt.join("d")).unwrap();

    // rename over an EXISTING target atomically replaces it.
    std::fs::write(mnt.join("a.md"), "aaa").unwrap();
    std::fs::write(mnt.join("b.md"), "bbb").unwrap();
    std::fs::rename(mnt.join("a.md"), mnt.join("b.md")).unwrap();
    assert_eq!(std::fs::read_to_string(mnt.join("b.md")).unwrap(), "aaa");
    assert!(std::fs::metadata(mnt.join("a.md")).is_err());

    // renaming a nonexistent source errors.
    assert!(std::fs::rename(mnt.join("nope.md"), mnt.join("x.md")).is_err());

    // truncate BY PATH (no open fd) → setattr without a file handle.
    truncate_path(&mnt.join("b.md"), 1);
    assert_eq!(std::fs::read_to_string(mnt.join("b.md")).unwrap(), "a");
    // chmod → setattr with size=None (metadata-only; returns the current attr).
    let mut perms = std::fs::metadata(mnt.join("b.md")).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(mnt.join("b.md"), perms).unwrap();

    handle.unmount();
    std::fs::remove_dir_all(&store_root).ok();
    std::fs::remove_dir_all(&mnt).ok();
}

fn truncate_path(path: &Path, len: i64) {
    nix::unistd::truncate(path, len).expect("truncate by path");
}
