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

/// Whether a resumed session on the same thread reuses the config home as-is
/// (a warm resume — the CLI keeps its context/auth) or starts from a clean home
/// keeping only the retained files (a forced-cold session).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionReuse {
    /// Reuse the config home untouched — the default cross-session behavior.
    Warm,
    /// Drop everything except the retained files before the session.
    ForcedCold,
}

/// Which top-level config-home entries survive a [`SessionReuse::ForcedCold`] reset
/// — the CLI's auth/config it wrote itself (e.g. `.credentials.json`, `config.toml`).
/// Everything else is ephemeral scratch dropped on a cold session.
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    retained: Vec<String>,
}

impl RetentionPolicy {
    #[must_use]
    pub fn new(retained: impl IntoIterator<Item = String>) -> Self {
        Self {
            retained: retained.into_iter().collect(),
        }
    }

    fn keeps(&self, name: &str) -> bool {
        self.retained.iter().any(|r| r == name)
    }
}

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

    /// Prepare the home for a session per its [`SessionReuse`]: `Warm` leaves it
    /// untouched (the CLI resumes on its own state); `ForcedCold` drops every
    /// top-level entry the `policy` does not retain, so a fresh session starts clean
    /// but keeps the CLI's persisted auth/config across the environment/path change.
    pub fn prepare(&self, reuse: SessionReuse, policy: &RetentionPolicy) -> io::Result<()> {
        if reuse == SessionReuse::Warm {
            return Ok(());
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            if policy.keeps(&name.to_string_lossy()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
        Ok(())
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
    fn forced_cold_drops_scratch_but_keeps_retained_across_sessions() {
        let base = temp_root("cold");
        let _ = fs::remove_dir_all(&base);
        let home = ConfigHome::open(Some(&base), "t").unwrap();
        home.write(".credentials.json", b"token").unwrap();
        home.write("scratch.log", b"junk").unwrap();
        home.write("cache/x", b"junk").unwrap();

        let retain = RetentionPolicy::new([".credentials.json".to_string()]);
        // A warm resume touches nothing.
        home.prepare(SessionReuse::Warm, &retain).unwrap();
        assert!(home.read("scratch.log").unwrap().is_some());

        // A forced-cold session keeps auth, drops scratch (files and dirs).
        home.prepare(SessionReuse::ForcedCold, &retain).unwrap();
        assert_eq!(
            home.read(".credentials.json").unwrap().as_deref(),
            Some(&b"token"[..])
        );
        assert!(home.read("scratch.log").unwrap().is_none());
        assert!(home.read("cache/x").unwrap().is_none());
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
