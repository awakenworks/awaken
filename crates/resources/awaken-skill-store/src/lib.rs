//! Durable skill catalog for the resources plane.
//!
//! A `SKILL.md`-per-skill store on disk, addressed by a stable id. It is the durable,
//! *delivered* skill source: a skill written here survives a process restart, so a
//! catalog configured through the management plane outlives the process that
//! received it — unlike a static in-process registry.
//!
//! This crate is deliberately store-shaped and nothing more: the runtime's
//! `awaken-ext-skills` never sees it. The host reads the catalog out of here and
//! feeds the bytes to the extension's `SkillSource` port as plain file data, so the
//! runtime only ever perceives local files and the injected catalog — never a store.

use std::path::{Path, PathBuf};

/// The filename backing a skill id: `<safe-stem>.md`. Ids are typically already
/// safe slugs, but a managed client can post an arbitrary id — so it is reduced to a
/// single safe stem here, and the id a `list` reports round-trips back to that stem.
fn id_filename(id: &str) -> String {
    format!("{}.md", sanitize_stem(id))
}

/// Reduce `name` to a safe single file stem: keep alphanumerics, `-`, `_`; map every
/// other run to a single `-`; never empty; bounded length. A `put` cannot escape the
/// root and a crafted `../` id resolves to a harmless stem.
pub fn sanitize_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(120);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// A durable, id-keyed catalog of `SKILL.md` bodies: one file per skill under `root`.
/// The delivered-skill source the host scans; survives a restart because it is on
/// disk, not in a process's heap.
pub struct SkillStore {
    root: PathBuf,
}

impl SkillStore {
    /// Open (creating if absent) the catalog rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The catalog's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store (or overwrite) the `SKILL.md` `content` under `id`. Returns the safe id
    /// the skill is addressable by (the sanitized stem), which is what [`list`](Self::list)
    /// reports back.
    pub fn put(&self, id: &str, content: &str) -> std::io::Result<String> {
        let stem = sanitize_stem(id);
        std::fs::write(self.root.join(format!("{stem}.md")), content)?;
        Ok(stem)
    }

    /// The `SKILL.md` content stored under `id`, or `None` if no such skill exists.
    pub fn get(&self, id: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(id_filename(id))).ok()
    }

    /// Every skill as `(id, content)`, ordered by id for a stable catalog. The id is
    /// the file stem — exactly what [`put`](Self::put) returned.
    pub fn list(&self) -> Vec<(String, String)> {
        let Ok(read_dir) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = read_dir
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .filter_map(|path| {
                let id = path.file_stem()?.to_str()?.to_string();
                let content = std::fs::read_to_string(&path).ok()?;
                Some((id, content))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("awaken-skillstore-{tag}-{stamp}"))
    }

    #[test]
    fn put_list_get_roundtrips_and_reopen_reads_the_same_catalog() {
        let root = scratch("roundtrip");
        {
            let store = SkillStore::open(&root).unwrap();
            assert_eq!(store.put("greet", "---\n---\nGREETING").unwrap(), "greet");
            store.put("review", "REVIEW").unwrap();
        }
        // A fresh process over the same root (a restart) still lists both skills.
        let reopened = SkillStore::open(&root).unwrap();
        let ids: Vec<_> = reopened.list().into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            ids,
            vec!["greet", "review"],
            "catalog survives reopen, sorted"
        );
        assert_eq!(reopened.get("greet").unwrap(), "---\n---\nGREETING");
        assert_eq!(reopened.get("missing"), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn crafted_ids_cannot_escape_root() {
        let root = scratch("escape");
        let store = SkillStore::open(&root).unwrap();
        let id = store.put("../../etc/passwd", "x").unwrap();
        assert_eq!(id, "etc-passwd");
        assert!(root.join("etc-passwd.md").exists());
        assert!(!root.parent().unwrap().join("passwd.md").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
