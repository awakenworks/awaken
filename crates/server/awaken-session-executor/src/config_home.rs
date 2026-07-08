//! `ConfigHome`: a thread's isolated config directory for a launched ACP CLI — the
//! place its memory entrypoint (`CLAUDE.md`), MCP config file, and self-written auth
//! live. Keyed by `thread_id`, so a resumed session on the same thread mounts the
//! *same* directory at a stable path (the cross-session / cross-environment
//! migration seam). Durable under `AWAKEN_STORAGE_DIR/threads/<t>/config_home` when
//! a storage dir is set; a process-lifetime temp dir otherwise — matching how the
//! host picks durability for the commit and memory stores.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A thread's config-home directory. The CLI's `config_home_env` (e.g.
/// `CLAUDE_CONFIG_DIR`) points at [`root`](ConfigHome::root); the host writes the
/// memory entrypoint / MCP config into it before launch.
#[derive(Debug, Clone)]
pub struct ConfigHome {
    root: PathBuf,
}

impl ConfigHome {
    /// Open (creating) the config home for `thread_id`. `store_dir` is the durable
    /// root (`AWAKEN_STORAGE_DIR`) when set; otherwise a per-process temp dir keeps
    /// it in-run only. The path is stable across sessions for a given thread, so a
    /// warm resume reuses whatever the CLI persisted here.
    pub fn open(store_dir: Option<&Path>, thread_id: &str) -> io::Result<Self> {
        let root = match store_dir {
            Some(dir) => dir.join("threads").join(thread_id).join("config_home"),
            None => std::env::temp_dir()
                .join(format!("awaken-config-home-{}", std::process::id()))
                .join(thread_id),
        };
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The directory the CLI's `config_home_env` is set to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write a file (creating parent dirs) relative to the config home — the memory
    /// entrypoint, an MCP config file, etc. `rel` must be relative.
    pub fn write(&self, rel: &str, contents: &[u8]) -> io::Result<()> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)
    }

    /// Read a config-home file, `None` if absent.
    pub fn read(&self, rel: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.root.join(rel)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("awaken-ch-test-{}-{}", std::process::id(), tag))
    }

    #[test]
    fn durable_config_home_has_a_stable_per_thread_path_and_persists() {
        let base = temp_root("durable");
        let _ = fs::remove_dir_all(&base);

        let home = ConfigHome::open(Some(&base), "thr_1").unwrap();
        home.write("CLAUDE.md", b"# memory").unwrap();

        // A second open for the same thread lands on the same path and sees the file
        // (a warm cross-session resume).
        let again = ConfigHome::open(Some(&base), "thr_1").unwrap();
        assert_eq!(again.root(), home.root());
        assert_eq!(
            again.read("CLAUDE.md").unwrap().as_deref(),
            Some(&b"# memory"[..])
        );

        // A different thread gets a different home.
        let other = ConfigHome::open(Some(&base), "thr_2").unwrap();
        assert_ne!(other.root(), home.root());
        assert!(other.read("CLAUDE.md").unwrap().is_none());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn write_creates_nested_parents_and_read_missing_is_none() {
        let base = temp_root("nested");
        let _ = fs::remove_dir_all(&base);
        let home = ConfigHome::open(Some(&base), "t").unwrap();
        home.write(".config/mcp/config.toml", b"[servers]").unwrap();
        assert_eq!(
            home.read(".config/mcp/config.toml").unwrap().as_deref(),
            Some(&b"[servers]"[..])
        );
        assert!(home.read("absent").unwrap().is_none());
        let _ = fs::remove_dir_all(&base);
    }
}
