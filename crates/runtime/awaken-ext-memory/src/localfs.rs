//! Local-filesystem access to a directory of memory files: one `<slug>.md` per
//! memory under a root the host provides.
//!
//! This is the runtime's *only* memory persistence surface — plain reads and writes
//! of a local directory. It is deliberately NOT a store: it knows nothing of
//! durability, ids, or restarts (that is the resources plane's `awaken-memory-store`,
//! wired by the host). Writes are scoped to `root` (a memory cannot escape it), and
//! reads are ordered newest-first so a bounded recall keeps the most recent memories.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    pub path: PathBuf,
    pub content: String,
    pub modified: SystemTime,
}

/// A handle to a local directory of memory files, rooted at `root`. Reads and
/// writes `<slug>.md` files there — nothing more; the directory's durability is the
/// host's concern, not this handle's.
#[derive(Clone)]
pub struct MemoryDir {
    root: PathBuf,
}

impl MemoryDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write `content` to `<root>/<sanitize(name)>.md`, creating the directory.
    /// Returns the path written. `name` is sanitized so a write cannot escape root.
    pub fn write(&self, name: &str, content: &str) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join(format!("{}.md", sanitize_stem(name)));
        std::fs::write(&path, content)?;
        Ok(path)
    }

    /// All saved memories, newest-first (by mtime). Empty/unreadable entries are
    /// skipped. A missing root yields an empty list.
    pub fn entries(&self) -> Vec<Entry> {
        let Ok(read_dir) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut entries: Vec<Entry> = read_dir
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .filter_map(|path| {
                let content = std::fs::read_to_string(&path).ok()?;
                if content.trim().is_empty() {
                    return None;
                }
                let modified = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Some(Entry {
                    path,
                    content: content.trim().to_string(),
                    modified,
                })
            })
            .collect();
        // Newest first; ties broken by path for determinism.
        entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The whole isolation story: `sanitize_stem` output is a bare, separator-free
    /// file stem, so `root.join(stem + ".md")` can never climb out of `root`.
    /// A hostile-input table (no proptest in this workspace) stands in for a fuzz
    /// harness — every case must satisfy the same structural invariants.
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
