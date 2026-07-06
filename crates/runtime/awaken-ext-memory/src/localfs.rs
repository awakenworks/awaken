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
}
