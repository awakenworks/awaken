//! Atomic, path-jailed projection of runtime-owned immutable trees.

use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_provisioning_contract as pc;

use crate::IsolatedRoot;

static WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn err(error: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(error.to_string())
}

pub(crate) fn materialize_read_only_tree_at(
    root: &IsolatedRoot,
    subdir: &str,
    files: &[(String, Vec<u8>, bool)],
) -> Result<(), pc::SandboxError> {
    let base = root.resolve(subdir).map_err(err)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&base) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(err(format!("read-only tree root `{subdir}` is unsafe")));
        }
    } else {
        std::fs::create_dir_all(&base).map_err(err)?;
    }

    for (relative, bytes, executable) in files {
        if relative.is_empty()
            || relative.contains('\\')
            || std::path::Path::new(relative).is_absolute()
            || std::path::Path::new(relative)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(err(format!("read-only tree path `{relative}` is unsafe")));
        }
        let logical = format!(
            "{}/{}",
            subdir.trim_matches('/'),
            relative.trim_start_matches('/')
        );
        let destination = root.resolve(&logical).map_err(err)?;
        if !destination.starts_with(&base) || relative.is_empty() {
            return Err(err(format!("read-only tree path `{relative}` is unsafe")));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| err(format!("read-only tree path `{relative}` has no parent")))?;
        std::fs::create_dir_all(parent).map_err(err)?;
        let mut cursor = parent.to_path_buf();
        while cursor.starts_with(&base) {
            if let Ok(metadata) = std::fs::symlink_metadata(&cursor)
                && metadata.file_type().is_symlink()
            {
                return Err(err(format!(
                    "read-only tree path `{relative}` crosses a symlink"
                )));
            }
            if cursor == base || !cursor.pop() {
                break;
            }
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&destination)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(err(format!("read-only tree file `{relative}` is unsafe")));
        }
        if !std::fs::read(&destination).is_ok_and(|current| current == *bytes) {
            let sequence = WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(".awaken-tree-{}-{sequence}", std::process::id()));
            let write = (|| -> std::io::Result<()> {
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary)?;
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(&temporary, &destination)
            })();
            if let Err(error) = write {
                let _ = std::fs::remove_file(&temporary);
                return Err(err(error));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = if *executable { 0o500 } else { 0o400 };
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(mode))
                .map_err(err)?;
        }
        #[cfg(not(unix))]
        let mut permissions = std::fs::metadata(&destination).map_err(err)?.permissions();
        #[cfg(not(unix))]
        permissions.set_readonly(true);
        #[cfg(not(unix))]
        std::fs::set_permissions(&destination, permissions).map_err(err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cause/effect rules: C1 binary non-executable -> exact read-only bytes;
    // C2 executable script -> owner execute without write; C3 unsafe lexical,
    // root, destination, or symlink path -> reject; C4 exact replay/changed
    // immutable version -> idempotent success/atomic read-only replacement.
    #[test]
    fn preserves_binary_permissions_and_rejects_unsafe_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = IsolatedRoot::new(temp.path().join("skill-tree"));
        let binary = vec![0, 159, 146, 150, 255];
        materialize_read_only_tree_at(
            &root,
            ".skills/greet",
            &[
                ("SKILL.md".into(), b"# greet".to_vec(), false),
                ("assets/data.bin".into(), binary.clone(), false),
                ("scripts/run.sh".into(), b"#!/bin/sh\n".to_vec(), true),
            ],
        )
        .unwrap();
        let projection = root.root().join(".skills/greet");
        assert_eq!(
            std::fs::read(projection.join("assets/data.bin")).unwrap(),
            binary
        );
        assert!(
            std::fs::metadata(projection.join("SKILL.md"))
                .unwrap()
                .permissions()
                .readonly()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let data_mode = std::fs::metadata(projection.join("assets/data.bin"))
                .unwrap()
                .permissions()
                .mode();
            let script_mode = std::fs::metadata(projection.join("scripts/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(data_mode & 0o333, 0, "C1");
            assert_eq!(script_mode & 0o222, 0, "C2/read-only");
            assert_ne!(script_mode & 0o100, 0, "C2/executable");
        }

        materialize_read_only_tree_at(
            &root,
            ".skills/greet",
            &[("SKILL.md".into(), b"# greet".to_vec(), false)],
        )
        .unwrap();
        materialize_read_only_tree_at(
            &root,
            ".skills/greet",
            &[("SKILL.md".into(), b"# greet v2".to_vec(), false)],
        )
        .unwrap();
        let skill = projection.join("SKILL.md");
        assert_eq!(std::fs::read(&skill).unwrap(), b"# greet v2", "C4");
        assert!(
            std::fs::metadata(skill).unwrap().permissions().readonly(),
            "C4"
        );

        for relative in ["../escape", "bad\\path", "/absolute"] {
            assert!(
                materialize_read_only_tree_at(
                    &root,
                    ".skills/bad",
                    &[(relative.into(), Vec::new(), false)],
                )
                .is_err(),
                "C3 {relative}"
            );
        }
        std::fs::write(root.root().join("unsafe-root"), b"file").unwrap();
        assert!(
            materialize_read_only_tree_at(
                &root,
                "unsafe-root",
                &[("value".into(), Vec::new(), false)],
            )
            .is_err(),
            "C3 root"
        );
        std::fs::create_dir_all(root.root().join("unsafe-destination/dir")).unwrap();
        assert!(
            materialize_read_only_tree_at(
                &root,
                "unsafe-destination",
                &[("dir".into(), b"not-a-directory".to_vec(), false)],
            )
            .is_err(),
            "C3 destination"
        );
        #[cfg(unix)]
        {
            let outside = temp.path().join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::create_dir_all(root.root().join("unsafe-parent")).unwrap();
            std::os::unix::fs::symlink(&outside, root.root().join("unsafe-parent/link")).unwrap();
            assert!(
                materialize_read_only_tree_at(
                    &root,
                    "unsafe-parent",
                    &[("link/value".into(), Vec::new(), false)],
                )
                .is_err(),
                "C3 symlink"
            );
        }
    }
}
