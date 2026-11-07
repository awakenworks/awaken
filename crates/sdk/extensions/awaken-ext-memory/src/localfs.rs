// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Static owner: the local memory directory's one scheduling boundary. Its async handle
// @formal reason: runs each unchanged synchronous read or write whole on the blocking pool, so a stalled
// @formal reason: file system cannot hold the async worker that renews an extraction claim. Evidence:
// @formal reason: the parked-read claim heartbeat decision-table test; this is a product-requirement
// @formal reason: classification, not a formal-proof claim.
//! Local-filesystem access to a directory of memory files: one `<slug>.md` per
//! memory under a root the host provides.
//!
//! This is the runtime's *only* memory persistence surface — plain reads and writes
//! of a local directory. It is deliberately NOT a store: it knows nothing of
//! durability, ids, or restarts (that is the resources plane's `awaken-memory-store`,
//! wired by the host). Content IO is relative to an opened directory capability.
//! [`MemoryDir::new`] trusts the host's root path at each operation;
//! [`MemoryDir::open`] pins an existing directory at trusted admission, so later
//! root-path replacement cannot redirect IO. Reads are ordered newest-first.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use cap_std::fs::Dir;

/// Reduce `name` to a safe single file stem: keep alphanumerics, `-`, `_`; map
/// everything else to `-`; never empty; bounded length.
pub fn sanitize_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            // Map any run of non-word characters to a single '-'.
            out.push('-');
        }
    }
    out.truncate(120);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "memory".to_string()
    } else {
        trimmed
    }
}

/// One saved memory read back from disk.
#[derive(Clone)]
pub struct Entry {
    /// Logical display path, not a capability for a later ambient reopen.
    pub path: PathBuf,
    pub content: String,
    pub modified: SystemTime,
}

/// A handle already scoped to one governed MemoryStore. Authorization and
/// workspace lookup happen before construction; recall and extraction only use
/// the selected store's content operations.
#[async_trait]
pub trait MemoryStoreHandle: Send + Sync {
    async fn write(&self, name: &str, content: &str) -> Result<String, String>;
    async fn entries(&self) -> Result<Vec<Entry>, String>;
}

/// A handle to a local directory of memory files, rooted at `root`. Reads and
/// writes `<slug>.md` files there — nothing more; the directory's durability is the
/// host's concern, not this handle's.
#[derive(Clone)]
pub struct MemoryDir {
    root: PathBuf,
    directory: Option<Arc<Dir>>,
}

impl MemoryDir {
    /// Select a trusted host path without IO. Reads do not create a missing
    /// root; writes create it. The host controls root selection until each
    /// operation opens it. Use [`Self::open`] to pin identity across operations.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            directory: None,
        }
    }

    /// Open and pin an existing directory. Root admission (including resolving
    /// an initial root symlink) is trusted host authority, not a sandbox check.
    /// After success, clones share that directory even if its path or ancestors
    /// are replaced. Failure creates nothing and can be retried by the caller.
    /// Confinement does not imply exclusive inode ownership (for example, a
    /// pre-existing hard link can name an inode also reachable elsewhere).
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        let directory = Arc::new(Dir::open_ambient_dir(&root, cap_std::ambient_authority())?);
        Ok(Self {
            root,
            directory: Some(directory),
        })
    }

    /// Logical display path; IO authority resides in the selected directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn directory(&self) -> std::io::Result<Arc<Dir>> {
        match &self.directory {
            Some(directory) => Ok(Arc::clone(directory)),
            None => Dir::open_ambient_dir(&self.root, cap_std::ambient_authority()).map(Arc::new),
        }
    }

    /// Write `<sanitize(name)>.md` relative to the selected directory. The
    /// trusted-path constructor creates a missing root; a pinned handle never
    /// reopens its display path. Escaping/absolute symlinks are refused by the
    /// directory capability. This remains a non-atomic, in-place overwrite;
    /// errors can leave partial in-root content, with no durability guarantee.
    pub fn write(&self, name: &str, content: &str) -> std::io::Result<PathBuf> {
        if self.directory.is_none() {
            std::fs::create_dir_all(&self.root)?;
        }
        let directory = self.directory()?;
        let relative = format!("{}.md", sanitize_stem(name));
        directory.write(&relative, content)?;
        Ok(self.root.join(relative))
    }

    /// All saved memories, newest-first (by mtime). Empty/unreadable entries are
    /// skipped. A missing root yields an empty list.
    pub fn entries(&self) -> Vec<Entry> {
        let Ok(directory) = self.directory() else {
            return Vec::new();
        };
        let Ok(read_dir) = directory.entries() else {
            return Vec::new();
        };
        let mut entries: Vec<Entry> = read_dir
            .filter_map(|e| e.ok())
            .map(|e| PathBuf::from(e.file_name()))
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .filter_map(|name| self.read_entry(&directory, &name))
            .collect();
        // Newest first; ties broken by path for determinism.
        entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        entries
    }

    fn read_entry(&self, directory: &Dir, name: &Path) -> Option<Entry> {
        let mut file = directory.open(name).ok()?;
        let mut content = String::new();
        file.read_to_string(&mut content).ok()?;
        if content.trim().is_empty() {
            return None;
        }
        let modified = file
            .metadata()
            .and_then(|m| m.modified())
            .map(|time| time.into_std())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        Some(Entry {
            path: self.root.join(name),
            content: content.trim().to_string(),
            modified,
        })
    }
}

/// The async handle runs each directory operation whole on the blocking pool.
/// Recall and extraction await it beside a claim heartbeat; a synchronous read
/// or write on the async worker would hold that heartbeat for as long as the
/// filesystem stalls, and under writeback pressure that outlasts the lease.
/// Each operation is the synchronous one above, unchanged: the same files in
/// the same order, with no added or removed sync point. Dropping the awaiting
/// future does not cut an operation short: the blocking job finishes it, so a
/// cancelled write still leaves the whole content, as an uncancelled one does.
#[async_trait]
impl MemoryStoreHandle for MemoryDir {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        let directory = self.clone();
        let (name, content) = (name.to_owned(), content.to_owned());
        off_async_worker(move || {
            MemoryDir::write(&directory, &name, &content)
                .map(|path| path.display().to_string())
                .map_err(|error| error.to_string())
        })
        .await?
    }

    async fn entries(&self) -> Result<Vec<Entry>, String> {
        let directory = self.clone();
        off_async_worker(move || MemoryDir::entries(&directory)).await
    }
}

/// The one scheduling boundary for [`MemoryDir`]'s synchronous file IO. A
/// panic inside the operation is resumed on the caller, as it was inline.
async fn off_async_worker<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(value) => Ok(value),
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(format!("memory directory operation did not run: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_preserve_missing_root_and_retry_without_caching_failure() {
        // ND2-T01/T02: legacy construction/reads do no creation; pinned open
        // requires an existing root. A failed open is not retained state.
        // Once admitted, both modes keep the same overwrite/display/trim law.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let legacy = MemoryDir::new(&root);
        assert!(!root.exists());
        assert!(legacy.entries().is_empty());
        assert!(!root.exists());
        assert!(MemoryDir::open(&root).is_err());
        assert!(!root.exists());
        legacy.write("note", "first").unwrap();
        let pinned = MemoryDir::open(&root).unwrap();
        assert_eq!(pinned.root(), root);
        assert_eq!(
            pinned.write("note!!!", " second ").unwrap(),
            root.join("note.md")
        );
        assert_eq!(legacy.entries()[0].content, "second");
        assert_eq!(pinned.entries()[0].content, "second");
        let file = temporary.path().join("not-a-directory");
        std::fs::write(&file, "unchanged").unwrap();
        assert!(MemoryDir::open(&file).is_err());
        assert!(MemoryDir::new(&file).entries().is_empty());
        assert!(MemoryDir::new(&file).write("note", "refused").is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn relative_io_refuses_escaping_and_absolute_leaf_symlinks_before_overwrite() {
        use std::os::unix::fs::symlink;

        // ND2-T03/T04: constructor x absolute/relative x inside/outside leaf.
        // Only a contained relative link succeeds. Every rejection must leave
        // an EXISTING nonempty outside sentinel intact (including O_TRUNC),
        // never recall its bytes, and never create a missing outside target.
        // An absolute link is refused even when its current target is inside.
        for pinned in [false, true] {
            for kind in [
                "relative-inside",
                "absolute-inside",
                "relative-outside",
                "absolute-outside",
            ] {
                let temporary = tempfile::tempdir().unwrap();
                let root = temporary.path().join("root");
                std::fs::create_dir(&root).unwrap();
                let outside = temporary.path().join("outside.md");
                std::fs::write(&outside, "OUTSIDE NONEMPTY SENTINEL").unwrap();
                std::fs::write(root.join("inside.md"), "inside").unwrap();
                let target = match kind {
                    "relative-inside" => PathBuf::from("inside.md"),
                    "absolute-inside" => root.join("inside.md"),
                    "relative-outside" => PathBuf::from("../outside.md"),
                    _ => outside.clone(),
                };
                symlink(target, root.join("link.md")).unwrap();
                let store = if pinned {
                    MemoryDir::open(&root).unwrap()
                } else {
                    MemoryDir::new(&root)
                };
                assert!(
                    store
                        .entries()
                        .iter()
                        .all(|e| !e.content.contains("OUTSIDE")),
                    "{kind}"
                );
                let result = store.write("link", "replacement");
                assert_eq!(
                    result.is_ok(),
                    kind == "relative-inside",
                    "{kind}, pinned={pinned}"
                );
                assert_eq!(
                    std::fs::read_to_string(&outside).unwrap(),
                    "OUTSIDE NONEMPTY SENTINEL"
                );
                assert_eq!(
                    std::fs::read_to_string(root.join("inside.md")).unwrap(),
                    if kind == "relative-inside" {
                        "replacement"
                    } else {
                        "inside"
                    }
                );
                let absent = temporary.path().join("absent.md");
                symlink(&absent, root.join("missing.md")).unwrap();
                assert!(store.write("missing", "refused").is_err());
                assert!(!absent.exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn enumerated_leaf_cannot_be_replaced_with_an_escaping_link_before_io() {
        use std::os::unix::fs::symlink;
        use std::sync::Barrier;

        // ND2-T06: capture the real enumeration name, then an independent
        // thread swaps it before the SAME read_entry/write boundary. The
        // barrier and join force check-before-swap-before-use; no sleep guess
        // and no product test hook. Both reads and destructive writes refuse.
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let leaf = root.join("note.md");
        let outside = temporary.path().join("outside.md");
        std::fs::write(&leaf, "inside").unwrap();
        std::fs::write(&outside, "OUTSIDE SENTINEL").unwrap();
        let store = MemoryDir::open(&root).unwrap();
        let directory = store.directory().unwrap();
        let name = PathBuf::from(
            directory
                .entries()
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name(),
        );
        let barrier = Arc::new(Barrier::new(2));
        let adversary = Arc::clone(&barrier);
        let external = outside.clone();
        let swap = std::thread::spawn(move || {
            adversary.wait();
            std::fs::remove_file(&leaf).unwrap();
            symlink(external, leaf).unwrap();
        });
        barrier.wait();
        swap.join().unwrap();
        assert!(store.read_entry(&directory, &name).is_none());
        assert!(store.entries().is_empty());
        assert!(store.write("note", "must-not-truncate").is_err());
        assert_eq!(
            std::fs::read_to_string(outside).unwrap(),
            "OUTSIDE SENTINEL"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pinned_root_clone_survives_root_ancestor_and_initial_alias_replacement() {
        use std::os::unix::fs::symlink;
        use std::sync::Barrier;

        // ND2-T05/T07: the host trusts the initial root (including an alias)
        // until open. After admission an independent thread replaces the root,
        // ancestor or alias. A clone retains the ORIGINAL directory after the
        // first handle drops; reads/writes never reach the replacement tree.
        // Returned paths remain logical display, not authority to reopen them.
        for kind in ["root", "ancestor", "alias"] {
            let temporary = tempfile::tempdir().unwrap();
            let parent = temporary.path().join("parent");
            let root = parent.join("root");
            let outside_parent = temporary.path().join("outside");
            let outside = outside_parent.join("root");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(root.join("note.md"), "inside").unwrap();
            std::fs::write(outside.join("note.md"), "OUTSIDE").unwrap();
            let alias = temporary.path().join("alias");
            symlink(&root, &alias).unwrap();
            let logical = if kind == "alias" {
                alias.clone()
            } else {
                root.clone()
            };
            let store = MemoryDir::open(&logical).unwrap();
            let clone = store.clone();
            assert!(Arc::ptr_eq(
                store.directory.as_ref().unwrap(),
                clone.directory.as_ref().unwrap()
            ));
            drop(store);
            let held = temporary.path().join("held");
            let original_file = match kind {
                "root" => held.join("note.md"),
                "ancestor" => held.join("root/note.md"),
                _ => root.join("note.md"),
            };
            let barrier = Arc::new(Barrier::new(2));
            let adversary = Arc::clone(&barrier);
            let external = outside.clone();
            let swap = std::thread::spawn(move || {
                adversary.wait();
                match kind {
                    "root" => {
                        std::fs::rename(&root, &held).unwrap();
                        symlink(external, root).unwrap();
                    }
                    "ancestor" => {
                        std::fs::rename(&parent, &held).unwrap();
                        symlink(outside_parent, parent).unwrap();
                    }
                    _ => {
                        std::fs::remove_file(&alias).unwrap();
                        symlink(external, alias).unwrap();
                    }
                }
            });
            barrier.wait();
            swap.join().unwrap();
            assert_eq!(clone.entries()[0].content, "inside", "{kind}");
            assert_eq!(
                clone.write("note", "updated").unwrap(),
                logical.join("note.md")
            );
            assert_eq!(
                std::fs::read_to_string(original_file).unwrap(),
                "updated",
                "{kind}"
            );
            assert_eq!(
                std::fs::read_to_string(outside.join("note.md")).unwrap(),
                "OUTSIDE",
                "{kind}"
            );
            assert_eq!(clone.entries()[0].path, logical.join("note.md"));
        }
    }

    #[test]
    fn pinned_entries_preserve_skip_trim_and_deterministic_mtime_ties() {
        // ND2-T08: equal modification times -> logical path tie-break; blank,
        // non-md and unreadable-as-text entries -> skip. The same opened file
        // supplies content and metadata, so ambient path replacement is unused.
        let temporary = tempfile::tempdir().unwrap();
        let store = MemoryDir::open(temporary.path()).unwrap();
        for (name, content) in [("z", " zed "), ("a", " alpha "), ("blank", " \n ")] {
            store.write(name, content).unwrap();
        }
        std::fs::write(temporary.path().join("ignore.txt"), "ignored").unwrap();
        std::fs::write(temporary.path().join("invalid.md"), [0xff]).unwrap();
        let time = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(123);
        for name in ["a.md", "z.md"] {
            std::fs::File::options()
                .write(true)
                .open(temporary.path().join(name))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(time))
                .unwrap();
        }
        let entries = store.entries();
        assert_eq!(
            entries
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zed"]
        );
        assert_eq!(entries[0].modified, entries[1].modified);
    }

    #[cfg(unix)]
    #[test]
    fn permission_failures_remain_errors_or_skips_and_can_be_retried() {
        use std::os::unix::fs::PermissionsExt;

        // ND2-T09: real unprivileged permissions, not a mocked IO error.
        // Read-only overwrite -> Err + unchanged bytes; unreadable -> skipped;
        // permissions restored -> normal overwrite succeeds (no error cache).
        // A privileged process that bypasses permission denial fails this
        // precondition explicitly; it must not count a skipped negative as PASS.
        let temporary = tempfile::tempdir().unwrap();
        let store = MemoryDir::open(temporary.path()).unwrap();
        let file = store.write("note", "original").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(
            std::fs::OpenOptions::new().write(true).open(&file).is_err(),
            "permission negative requires an unprivileged process"
        );
        assert!(store.write("note", "refused").is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(store.entries().is_empty());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        store.write("note", "recovered").unwrap();
        assert_eq!(store.entries()[0].content, "recovered");
    }

    #[tokio::test]
    async fn pinned_directory_uses_the_existing_memory_store_handle_port() {
        // ND2-T10: constructor changes no plugin/managed-memory authority.
        // The existing handle still returns the logical write path and trimmed
        // entries; no new store, CAS, lifecycle or alternate recall path.
        let temporary = tempfile::tempdir().unwrap();
        let store: Arc<dyn MemoryStoreHandle> =
            Arc::new(MemoryDir::open(temporary.path()).unwrap());
        assert_eq!(
            store.write("note", " value ").await.unwrap(),
            temporary.path().join("note.md").display().to_string()
        );
        assert_eq!(store.entries().await.unwrap()[0].content, "value");
    }

    #[test]
    fn sanitize_stem_is_safe() {
        assert_eq!(sanitize_stem("user prefs"), "user-prefs");
        assert_eq!(sanitize_stem("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_stem("   "), "memory");
        assert_eq!(sanitize_stem(""), "memory");
    }

    #[test]
    fn sanitize_stem_collapses_trims_truncates_and_drops_non_ascii() {
        // Leading/trailing runs of non-word chars collapse then trim away entirely.
        assert_eq!(sanitize_stem("--a--b--"), "a-b");
        // Non-ASCII is not a word char: it maps to '-', which then trims off.
        assert_eq!(sanitize_stem("café"), "caf");
        // Bounded to 120 chars (all word chars, so no trim shrinkage).
        assert_eq!(sanitize_stem(&"x".repeat(200)).len(), 120);
    }

    #[test]
    fn entries_skips_empty_and_non_md_trims_content_and_tolerates_missing_root() {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("awaken-mem-entries-{stamp}"));
        let store = MemoryDir::new(&root);

        // A never-created root reads back as no memories (not an error).
        assert!(store.entries().is_empty());

        // A whitespace-only memory file is skipped as empty.
        store.write("blank", "   \n  ").unwrap();
        // A non-.md sibling file is ignored by the extension filter.
        std::fs::write(root.join("note.txt"), "ignored").unwrap();
        // A real memory, with surrounding whitespace trimmed on read-back.
        store.write("real", "  hello  ").unwrap();

        let entries = store.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "hello");
    }

    #[test]
    fn write_overwrites_the_same_stem_in_place() {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("awaken-mem-overwrite-{stamp}"));
        let store = MemoryDir::new(&root);
        // Two names that sanitize to the same stem share one file; the second
        // write replaces the first rather than appending a new memory.
        store.write("dup", "v1").unwrap();
        store.write("dup!!!", "v2").unwrap();
        let entries = store.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "v2");
    }

    #[test]
    fn write_scopes_to_root_and_entries_read_back_newest_first() {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("awaken-mem-store-{stamp}"));
        let store = MemoryDir::new(&root);

        store.write("first", "one").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.write("second/../escape", "two").unwrap();

        // The path-traversal name was clamped to a single stem under root.
        assert!(root.join("second-escape.md").exists());
        assert!(!root.parent().unwrap().join("escape.md").exists());

        let entries = store.entries();
        assert_eq!(entries.len(), 2);
        // Newest first: "second..." was written last.
        assert_eq!(entries[0].content, "two");
        assert_eq!(entries[1].content, "one");
    }

    /// Lexical isolation: `sanitize_stem` output is a bare, separator-free
    /// file stem, so `root.join(stem + ".md")` can never climb out of `root`.
    /// A hostile-input table (no proptest in this workspace) stands in for a fuzz
    /// harness — every case must satisfy the same structural invariants. Physical
    /// symlink/root replacement is a separate capability-boundary test below.
    #[test]
    fn sanitize_stem_never_yields_a_path_that_escapes_root() {
        let hostile = [
            "/etc/passwd",
            "//etc//passwd",
            "../../etc/passwd",
            "../../../../../../root/.ssh/id_rsa",
            "..\\..\\Windows\\system32",
            "C:\\Windows\\System32",
            "foo/bar/baz",
            "a\0b",                 // embedded NUL
            "\0\0\0",               // all NUL
            "\u{202E}drowssap",     // right-to-left override
            "e\u{0301}",            // NFD "é" (combining acute) — normalization trick
            "\u{FF0F}etc\u{FF0F}x", // fullwidth solidus (looks like '/')
            "%2e%2e%2f",            // percent-encoded ../
            ".",
            "..",
            "./.././.",
            "",
            "   ",
            "\n\t\r",
            "😀🔥",            // emoji only
            &"x/".repeat(500), // very long with separators
        ];
        let root = PathBuf::from("/srv/awaken/memories");
        for input in hostile {
            let stem = sanitize_stem(input);
            // 1. A stem is never empty (falls back to "memory").
            assert!(!stem.is_empty(), "empty stem for {input:?}");
            // 2. A stem holds no path separators, dots, NULs, or whitespace — only
            //    the alphabet `sanitize_stem` promises (word chars and '-').
            for ch in stem.chars() {
                assert!(
                    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-',
                    "stem {stem:?} from {input:?} leaked char {ch:?}"
                );
            }
            assert!(!stem.contains('/'), "stem {stem:?} from {input:?} has '/'");
            assert!(
                !stem.contains('\\'),
                "stem {stem:?} from {input:?} has '\\'"
            );
            assert!(!stem.contains('.'), "stem {stem:?} from {input:?} has '.'");
            assert!(!stem.contains('\0'), "stem {stem:?} from {input:?} has NUL");
            // 3. The written path is always a direct child of root — one component
            //    beyond it, never a sibling or an ancestor.
            let path = root.join(format!("{stem}.md"));
            assert!(
                path.starts_with(&root),
                "path {path:?} from {input:?} escaped root"
            );
            assert_eq!(
                path.parent(),
                Some(root.as_path()),
                "path {path:?} from {input:?} is not a direct child of root"
            );
        }
    }

    #[test]
    fn two_memory_dirs_at_distinct_roots_cannot_read_each_others_entries() {
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("awaken-mem-iso-{stamp}"));
        let root_a = base.join("a");
        let root_b = base.join("b");
        let dir_a = MemoryDir::new(&root_a);
        let dir_b = MemoryDir::new(&root_b);

        dir_a.write("secret", "ALPHA-ONLY").unwrap();
        dir_b.write("secret", "BETA-ONLY").unwrap();
        // Even a hostile name in B cannot plant a file A will enumerate: the stem is
        // clamped, so it lands under B's own root, not A's.
        dir_b.write("../a/leak", "BETA-ESCAPE-ATTEMPT").unwrap();

        let a_entries = dir_a.entries();
        assert_eq!(a_entries.len(), 1, "A sees only its own memory");
        assert_eq!(a_entries[0].content, "ALPHA-ONLY");
        for e in &a_entries {
            assert!(
                e.path.starts_with(&root_a),
                "A entry escaped its root: {:?}",
                e.path
            );
            assert!(
                !e.content.contains("BETA"),
                "A read B's memory: {}",
                e.content
            );
        }

        let b_entries = dir_b.entries();
        // B holds its own two writes; the escape attempt stayed inside B.
        assert_eq!(b_entries.len(), 2);
        for e in &b_entries {
            assert!(
                e.path.starts_with(&root_b),
                "B entry escaped its root: {:?}",
                e.path
            );
            assert!(
                !e.content.contains("ALPHA"),
                "B read A's memory: {}",
                e.content
            );
        }
    }
}
