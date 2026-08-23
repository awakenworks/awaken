//! Real kernel-VFS integration test (ADR-0053 P3): mount the FUSE over a durable
//! store, drive real `read`/`write`/`readdir`/`rename`/`unlink` syscalls through the
//! kernel, and prove writes persist across an unmount + fresh remount.
//!
//! Gated on a FUSE-capable host (`/dev/fuse` + `fusermount`). When FUSE is available
//! a mount failure is a hard failure (never a silent skip, per the no-stub rule);
//! when it is absent the test prints why and returns — the copy path is the fallback
//! where FUSE cannot run (CI/macOS).

#![cfg(all(feature = "fuse", target_os = "linux"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_memory_store::{MemoryRepository, SqliteMemoryRepository};
use awaken_provisioning_contract::{MemoryMounter, MountAccess, Realization};
use awaken_sandbox_memoryd::MemoryStoreMounter;
use awaken_sandbox_memoryd::fuse::spawn_mount;

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
    let store_db = store_root.join("memory.db");
    let mnt = unique("mnt");
    let store = "memstore_1";

    let backend = Arc::new(SqliteMemoryRepository::open(store_db.to_str().unwrap()).unwrap());
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
    let backend2 = Arc::new(SqliteMemoryRepository::open(store_db.to_str().unwrap()).unwrap());
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
fn independent_mounter_projections_invalidate_peer_caches_and_teardown_independently() {
    if let Some(reason) = fuse_unavailable_reason() {
        eprintln!("SKIP kernel_vfs coherence: {reason}");
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let store_root = unique("cstore");
    let store_db = store_root.join("memory.db");
    let mnt_a = unique("cmnt-a");
    let mnt_b = unique("cmnt-b");
    let store = "memstore_1";

    let backend = Arc::new(SqliteMemoryRepository::open(store_db.to_str().unwrap()).unwrap());
    rt.block_on(backend.create(store, "/x.md", "one")).unwrap();
    let mounter = MemoryStoreMounter::new(backend.clone());

    // Cause/effect design (ADR-0053 D5): C1=same store, C2=two distinct sandbox
    // paths, C3=peer cache warmed, C4=write through A, C5=A torn down first.
    // R1 C1+C2+C3+C4 => E1 B invalidates and refetches the durable head.
    // R2 R1+C5 => E2 B remains mounted and readable until its own teardown.
    let mount_a = rt
        .block_on(mounter.mount(store, &mnt_a, MountAccess::ReadWrite))
        .unwrap();
    let mount_b = rt
        .block_on(mounter.mount(store, &mnt_b, MountAccess::ReadWrite))
        .unwrap();
    assert_eq!(mount_a.realization(), Realization::Fuse);
    assert_eq!(mount_b.realization(), Realization::Fuse);
    assert_ne!(mnt_a, mnt_b, "each sandbox owns a distinct projection");
    std::thread::sleep(Duration::from_millis(100));

    assert_eq!(std::fs::read_to_string(mnt_b.join("x.md")).unwrap(), "one");
    std::fs::write(mnt_a.join("x.md"), "two").unwrap();

    let mut observed = None;
    for _ in 0..50 {
        let content = std::fs::read_to_string(mnt_b.join("x.md")).unwrap();
        if content == "two" {
            observed = Some(content);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        observed.as_deref(),
        Some("two"),
        "peer cache must invalidate"
    );

    rt.block_on(mount_a.teardown());
    assert_eq!(
        std::fs::read_to_string(mnt_b.join("x.md")).unwrap(),
        "two",
        "tearing down one sandbox must not tear down its peer"
    );
    rt.block_on(mount_b.teardown());

    std::fs::remove_dir_all(&store_root).ok();
    std::fs::remove_dir_all(&mnt_a).ok();
    std::fs::remove_dir_all(&mnt_b).ok();
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
    let store_db = store_root.join("memory.db");
    let mnt = unique("emnt");
    let store = "memstore_1";

    let backend = Arc::new(SqliteMemoryRepository::open(store_db.to_str().unwrap()).unwrap());
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
