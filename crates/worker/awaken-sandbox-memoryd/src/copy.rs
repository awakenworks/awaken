//! Copy-in / harvest fallback (ADR-0053 D6) for hosts without FUSE.
//!
//! A write-through FUSE mount is the live path, but it needs `/dev/fuse` — absent on
//! macOS, in CI, and in unprivileged containers. There, the same path-addressed
//! [`MemoryFs`] store is materialized the old way: [`materialize`] copies every
//! memory out to plain files the agent reads/writes, and [`harvest`] walks those
//! files back into the store after a turn (create new, CAS-update changed, skip
//! unchanged). [`fuse_available`] is the capability check a provider gates on.

use std::path::{Path, PathBuf};

use awaken_memory_store::MemoryFs;

use crate::FuseError;

/// True when this host can mount FUSE (`/dev/fuse` present + `fusermount` /
/// `fusermount3` on `PATH`). When false, use [`materialize`] / [`harvest`].
#[must_use]
pub fn fuse_available() -> bool {
    if !Path::new("/dev/fuse").exists() {
        return false;
    }
    let on_path = |bin: &str| {
        std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).exists()))
    };
    on_path("fusermount") || on_path("fusermount3")
}

/// Copy every memory in `store` out to `root/<path>` (the no-FUSE materialization).
/// Nested paths create their parent directories. Returns the number of memories
/// written.
pub async fn materialize(fs: &dyn MemoryFs, store: &str, root: &Path) -> Result<usize, FuseError> {
    let entries = fs.list(store, "/").await?;
    let mut written = 0;
    for entry in &entries {
        let Some(memory) = fs.get_by_path(store, &entry.path).await? else {
            continue;
        };
        let dest = root.join(entry.path.trim_start_matches('/'));
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| FuseError::Internal(e.to_string()))?;
        }
        std::fs::write(&dest, memory.content.unwrap_or_default())
            .map_err(|e| FuseError::Internal(e.to_string()))?;
        written += 1;
    }
    Ok(written)
}

/// Walk `root` and fold each file back into `store`: create a new memory, CAS-update
/// a changed one against its live sha, and skip an unchanged one. Returns the number
/// of memories created or updated. A file whose path the store rejects (non-UTF-8
/// name, `..`) is skipped rather than failing the whole harvest.
pub async fn harvest(fs: &dyn MemoryFs, store: &str, root: &Path) -> Result<usize, FuseError> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files);
    let mut changed = 0;
    for (path, host_path) in files {
        let Ok(content) = std::fs::read_to_string(&host_path) else {
            continue; // non-UTF-8 memory bytes are not our model — skip
        };
        match fs.get_by_path(store, &path).await? {
            Some(current) => {
                if current.content.as_deref() != Some(content.as_str()) {
                    // CAS against the live head; a single-agent harvest never races.
                    fs.update(store, &current.id, &content, &current.content_sha256)
                        .await?;
                    changed += 1;
                }
            }
            None => {
                // A path the store rejects (e.g. an odd filename) is skipped.
                if fs.create(store, &path, &content).await.is_ok() {
                    changed += 1;
                }
            }
        }
    }
    Ok(changed)
}

/// Recursively collect `(store_path, host_path)` for every file under `base`, where
/// `store_path` is the memory path (`/` + the path relative to `root`).
fn collect_files(root: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if path.is_file()
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel = rel.to_string_lossy().replace('\\', "/");
            out.push((format!("/{rel}"), path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use awaken_memory_store::InMemoryFs;

    fn temp(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p =
            std::env::temp_dir().join(format!("awaken-memcopy-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn fuse_available_is_a_bool() {
        // Just exercise the probe; the value depends on the host.
        let _ = fuse_available();
    }

    #[test]
    fn materialize_writes_files_including_nested_paths() {
        let rt = rt();
        let fs = Arc::new(InMemoryFs::new());
        rt.block_on(fs.create("s", "/root.md", "top")).unwrap();
        rt.block_on(fs.create("s", "/notes/deep/a.md", "nested"))
            .unwrap();
        let dir = temp("mat");

        let n = rt.block_on(materialize(&*fs, "s", &dir)).unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read_to_string(dir.join("root.md")).unwrap(), "top");
        assert_eq!(
            std::fs::read_to_string(dir.join("notes/deep/a.md")).unwrap(),
            "nested"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_creates_new_updates_changed_and_skips_unchanged() {
        let rt = rt();
        let fs = Arc::new(InMemoryFs::new());
        // Seed one memory; a round-trip will modify it and add another.
        let seed = rt.block_on(fs.create("s", "/keep.md", "v1")).unwrap();
        let dir = temp("harv");
        rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        // Agent edits /keep.md, adds /new.md, and leaves an unchanged copy behind.
        std::fs::write(dir.join("keep.md"), "v2").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/new.md"), "fresh").unwrap();

        let changed = rt.block_on(harvest(&*fs, "s", &dir)).unwrap();
        assert_eq!(
            changed, 2,
            "one updated + one created; the unchanged file is a no-op"
        );

        let keep = rt
            .block_on(fs.get_by_path("s", "/keep.md"))
            .unwrap()
            .unwrap();
        assert_eq!(keep.content.as_deref(), Some("v2"));
        assert!(
            keep.version > seed.version,
            "a changed file bumps the version"
        );
        assert_eq!(
            rt.block_on(fs.get_by_path("s", "/sub/new.md"))
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("fresh"),
        );

        // A second harvest with no edits changes nothing (idempotent).
        assert_eq!(rt.block_on(harvest(&*fs, "s", &dir)).unwrap(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_skips_a_non_utf8_file_rather_than_failing() {
        let rt = rt();
        let fs = Arc::new(InMemoryFs::new());
        let dir = temp("harv-bin");
        std::fs::write(dir.join("ok.md"), "text").unwrap();
        std::fs::write(dir.join("blob.bin"), [0xFF, 0xFE, 0x00]).unwrap();

        // The UTF-8 file is folded in; the binary one is skipped, not fatal.
        assert_eq!(rt.block_on(harvest(&*fs, "s", &dir)).unwrap(), 1);
        assert!(
            rt.block_on(fs.get_by_path("s", "/blob.bin"))
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_never_deletes_a_removed_file_diverging_from_fuse_unlink() {
        // KNOWN BUG (adjudicate): the copy/harvest fallback folds files back with
        // create + CAS-update ONLY — it never deletes. A file the agent removes inside
        // the materialized copy dir is left ALIVE in the store after harvest, whereas the
        // live FUSE path deletes it (`unlink` → `delete_by_path`). So the same agent
        // action ("rm note.md") has DIFFERENT durable outcomes across the two realization
        // tiers: on the no-FUSE tier (bwrap / CI / macOS) a deletion silently does not
        // stick. Pinning the current copy-path behavior and contrasting it with the FUSE
        // delete primitive the live tier uses.
        let rt = rt();
        let fs = Arc::new(InMemoryFs::new());
        rt.block_on(fs.create("s", "/keep.md", "x")).unwrap();
        rt.block_on(fs.create("s", "/gone.md", "y")).unwrap();
        let dir = temp("del-parity");
        rt.block_on(materialize(&*fs, "s", &dir)).unwrap();

        // The agent removes gone.md from the copy dir; the turn ends → harvest.
        std::fs::remove_file(dir.join("gone.md")).unwrap();
        assert_eq!(
            rt.block_on(harvest(&*fs, "s", &dir)).unwrap(),
            0,
            "harvest reports no create/update (it walks only files still present)"
        );

        // Divergence: the removed file is STILL in the store — harvest cannot delete…
        assert!(
            rt.block_on(fs.get_by_path("s", "/gone.md"))
                .unwrap()
                .is_some(),
            "copy harvest leaves a deleted file alive in the store"
        );
        // …whereas the FUSE `unlink` primitive the live tier uses DOES remove it.
        rt.block_on(fs.delete_by_path("s", "/gone.md")).unwrap();
        assert!(
            rt.block_on(fs.get_by_path("s", "/gone.md"))
                .unwrap()
                .is_none(),
            "FUSE unlink (delete_by_path) removes it — the copy path does not"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn harvest_on_a_missing_root_is_empty() {
        let rt = rt();
        let fs = Arc::new(InMemoryFs::new());
        let missing = std::env::temp_dir().join("awaken-memcopy-does-not-exist-xyz");
        assert_eq!(rt.block_on(harvest(&*fs, "s", &missing)).unwrap(), 0);
    }
}
