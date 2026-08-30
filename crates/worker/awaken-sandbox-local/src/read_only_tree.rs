//! Atomic, path-jailed projection of runtime-owned immutable trees.

use awaken_provisioning_contract as pc;

use crate::IsolatedRoot;

fn err(error: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(error.to_string())
}

pub(crate) fn materialize_read_only_tree_at(
    root: &IsolatedRoot,
    root_identity: awaken_sandbox_fs::DirectoryIdentity,
    subdir: &str,
    files: &[(String, Vec<u8>, bool)],
) -> Result<(), pc::SandboxError> {
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
        awaken_sandbox_fs::write_relative_file_atomic(
            root.root(),
            root_identity,
            std::path::Path::new(&logical),
            bytes,
            if *executable { 0o500 } else { 0o400 },
        )
        .map_err(err)?;
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
        std::fs::create_dir(root.root()).unwrap();
        let identity = awaken_sandbox_fs::directory_identity_nofollow(root.root()).unwrap();
        let binary = vec![0, 159, 146, 150, 255];
        materialize_read_only_tree_at(
            &root,
            identity,
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
            identity,
            ".skills/greet",
            &[("SKILL.md".into(), b"# greet".to_vec(), false)],
        )
        .unwrap();
        materialize_read_only_tree_at(
            &root,
            identity,
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
                    identity,
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
                identity,
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
                identity,
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
                    identity,
                    "unsafe-parent",
                    &[("link/value".into(), Vec::new(), false)],
                )
                .is_err(),
                "C3 symlink"
            );
        }
    }
}
