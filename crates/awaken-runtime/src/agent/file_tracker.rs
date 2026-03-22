//! Tracks file changes between pre/post tool execution snapshots.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Tracks file changes between snapshots.
pub struct FileChangeTracker {
    root: PathBuf,
    /// Snapshot: path -> (size, modified_time)
    before: HashMap<PathBuf, FileStamp>,
}

#[derive(Debug, Clone)]
struct FileStamp {
    size: u64,
    modified: SystemTime,
}

/// A detected file change.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Relative path from workspace root.
    pub path: String,
    /// Type of change.
    pub operation: FileChangeOperation,
    /// File size in bytes after change. None for deletions.
    pub size: Option<u64>,
}

/// Type of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeOperation {
    Created,
    Modified,
    Deleted,
}

/// Directories to skip during scanning.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".hg",
    ".svn",
    "__pycache__",
];

impl FileChangeTracker {
    /// Create a new tracker for the given workspace root.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            before: HashMap::new(),
        }
    }

    /// Take a snapshot of the current file state.
    /// Call this BEFORE tool execution.
    pub async fn snapshot(&mut self) -> std::io::Result<()> {
        self.before.clear();
        let root = self.root.clone();
        let mut stamps = HashMap::new();
        scan_dir_into(&root, &root, &mut stamps).await?;
        self.before = stamps;
        Ok(())
    }

    /// Diff against the snapshot to find changes.
    /// Call this AFTER tool execution.
    pub async fn diff(&self) -> std::io::Result<Vec<FileChange>> {
        let mut after = HashMap::new();
        scan_dir_into(&self.root, &self.root, &mut after).await?;

        let mut changes = Vec::new();

        // Check for created and modified files
        for (path, stamp) in &after {
            match self.before.get(path) {
                None => changes.push(FileChange {
                    path: relative_path(&self.root, path),
                    operation: FileChangeOperation::Created,
                    size: Some(stamp.size),
                }),
                Some(old) if old.modified != stamp.modified || old.size != stamp.size => {
                    changes.push(FileChange {
                        path: relative_path(&self.root, path),
                        operation: FileChangeOperation::Modified,
                        size: Some(stamp.size),
                    });
                }
                _ => {}
            }
        }

        // Check for deleted files
        for path in self.before.keys() {
            if !after.contains_key(path) {
                changes.push(FileChange {
                    path: relative_path(&self.root, path),
                    operation: FileChangeOperation::Deleted,
                    size: None,
                });
            }
        }

        // Sort for deterministic output
        changes.sort_by(|a, b| a.path.cmp(&b.path));

        Ok(changes)
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn should_skip(name: &str) -> bool {
    // Skip hidden files/dirs (starting with '.')
    if name.starts_with('.') {
        return true;
    }
    SKIP_DIRS.contains(&name)
}

async fn scan_dir_into(
    root: &Path,
    dir: &Path,
    stamps: &mut HashMap<PathBuf, FileStamp>,
) -> std::io::Result<()> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e),
    };

    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip root-relative hidden dirs and well-known build dirs
        if dir == root && should_skip(&name_str) {
            continue;
        }
        // Skip hidden entries in subdirectories too
        if name_str.starts_with('.') {
            continue;
        }

        let file_type = match entry.file_type().await {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        let path = entry.path();

        if file_type.is_dir() {
            // Also skip well-known dirs in subdirectories
            if SKIP_DIRS.contains(&name_str.as_ref()) {
                continue;
            }
            Box::pin(scan_dir_into(root, &path, stamps)).await?;
        } else if file_type.is_file() {
            if let Ok(metadata) = tokio::fs::metadata(&path).await {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                stamps.insert(
                    path,
                    FileStamp {
                        size: metadata.len(),
                        modified,
                    },
                );
            }
        }
        // Skip symlinks and other special file types
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::fs;

    async fn make_temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awaken_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).await.unwrap();
        dir
    }

    async fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn snapshot_empty_dir() {
        let dir = make_temp_dir().await;
        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();
        let changes = tracker.diff().await.unwrap();
        assert!(changes.is_empty());
        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn detect_created_file() {
        let dir = make_temp_dir().await;
        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        // Create a file after snapshot
        fs::write(dir.join("new_file.txt"), "hello").await.unwrap();

        let changes = tracker.diff().await.unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "new_file.txt");
        assert_eq!(changes[0].operation, FileChangeOperation::Created);
        assert_eq!(changes[0].size, Some(5));
        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn detect_modified_file() {
        let dir = make_temp_dir().await;
        let file_path = dir.join("existing.txt");
        fs::write(&file_path, "original").await.unwrap();

        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        // Small delay to ensure mtime difference
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Modify the file
        fs::write(&file_path, "modified content").await.unwrap();

        let changes = tracker.diff().await.unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "existing.txt");
        assert_eq!(changes[0].operation, FileChangeOperation::Modified);
        assert_eq!(changes[0].size, Some(16));
        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn detect_deleted_file() {
        let dir = make_temp_dir().await;
        let file_path = dir.join("to_delete.txt");
        fs::write(&file_path, "temporary").await.unwrap();

        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        // Delete the file
        fs::remove_file(&file_path).await.unwrap();

        let changes = tracker.diff().await.unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "to_delete.txt");
        assert_eq!(changes[0].operation, FileChangeOperation::Deleted);
        assert!(changes[0].size.is_none());
        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn detect_multiple_changes() {
        let dir = make_temp_dir().await;

        // Pre-existing files
        fs::write(dir.join("keep.txt"), "unchanged").await.unwrap();
        fs::write(dir.join("modify.txt"), "old").await.unwrap();
        fs::write(dir.join("delete.txt"), "bye").await.unwrap();

        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Create, modify, and delete
        fs::write(dir.join("create.txt"), "new").await.unwrap();
        fs::write(dir.join("modify.txt"), "new content")
            .await
            .unwrap();
        fs::remove_file(dir.join("delete.txt")).await.unwrap();

        let changes = tracker.diff().await.unwrap();
        assert_eq!(changes.len(), 3);

        // Changes are sorted by path
        let ops: Vec<_> = changes.iter().map(|c| (&c.path, c.operation)).collect();
        assert!(ops.contains(&(&"create.txt".to_string(), FileChangeOperation::Created)));
        assert!(ops.contains(&(&"delete.txt".to_string(), FileChangeOperation::Deleted)));
        assert!(ops.contains(&(&"modify.txt".to_string(), FileChangeOperation::Modified)));

        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn skips_hidden_dirs() {
        let dir = make_temp_dir().await;

        // Create a hidden directory with a file
        let hidden = dir.join(".hidden");
        fs::create_dir_all(&hidden).await.unwrap();
        fs::write(hidden.join("secret.txt"), "hidden")
            .await
            .unwrap();

        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        // Add file in hidden dir
        fs::write(hidden.join("new_secret.txt"), "hidden new")
            .await
            .unwrap();

        let changes = tracker.diff().await.unwrap();
        assert!(changes.is_empty(), "hidden dir changes should be ignored");
        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn skips_well_known_dirs() {
        let dir = make_temp_dir().await;

        for skip_dir in &["node_modules", "target"] {
            let skip_path = dir.join(skip_dir);
            fs::create_dir_all(&skip_path).await.unwrap();
            fs::write(skip_path.join("file.txt"), "content")
                .await
                .unwrap();
        }

        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        // Add files in skipped directories
        for skip_dir in &["node_modules", "target"] {
            fs::write(dir.join(skip_dir).join("new.txt"), "new")
                .await
                .unwrap();
        }

        let changes = tracker.diff().await.unwrap();
        assert!(
            changes.is_empty(),
            "well-known dir changes should be ignored"
        );
        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn tracks_nested_directory_files() {
        let dir = make_temp_dir().await;
        let nested = dir.join("src").join("lib");
        fs::create_dir_all(&nested).await.unwrap();

        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        fs::write(nested.join("mod.rs"), "pub mod foo;")
            .await
            .unwrap();

        let changes = tracker.diff().await.unwrap();
        assert_eq!(changes.len(), 1);
        // Path should use forward slashes on all platforms (to_string_lossy)
        assert!(
            changes[0].path.ends_with("mod.rs"),
            "got: {}",
            changes[0].path
        );
        assert_eq!(changes[0].operation, FileChangeOperation::Created);
        cleanup(&dir).await;
    }

    #[tokio::test]
    async fn no_changes_when_nothing_changed() {
        let dir = make_temp_dir().await;
        fs::write(dir.join("stable.txt"), "content").await.unwrap();

        let mut tracker = FileChangeTracker::new(&dir);
        tracker.snapshot().await.unwrap();

        let changes = tracker.diff().await.unwrap();
        assert!(changes.is_empty());
        cleanup(&dir).await;
    }
}
