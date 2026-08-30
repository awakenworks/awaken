//! Shared outputs-directory scan used by every local-machine provider tier: an
//! artifact is a file the agent wrote under the environment's outputs path,
//! addressed by the immutable content id used by the shared FileStore.

use awaken_provisioning_contract as pc;

use crate::{IsolatedRoot, content_fingerprint};

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

/// Capture the outputs tree once as `(artifact, bytes)` pairs, sorted by sandbox
/// path. The descriptor-relative leaf binds the walk to `root_identity` and
/// never follows a symlink. A missing outputs root is empty; every other read
/// fault is surfaced rather than being misreported as an empty output set.
pub(crate) fn scan_outputs(
    root: &IsolatedRoot,
    root_identity: awaken_sandbox_fs::DirectoryIdentity,
    outputs_path: &str,
) -> Result<Vec<(pc::Artifact, Vec<u8>)>, pc::SandboxError> {
    awaken_sandbox_fs::read_regular_tree_nofollow(
        root.root(),
        root_identity,
        std::path::Path::new(outputs_path.trim_start_matches('/')),
    )
    .map_err(err)?
    .into_iter()
    .map(|file| {
        let relative = file
            .relative_path
            .to_str()
            .ok_or_else(|| err("sandbox outputs contain a non-UTF-8 logical path"))?;
        let sandbox_path = format!(
            "{}/{}",
            outputs_path.trim_end_matches('/'),
            relative.replace('\\', "/")
        );
        let content_hash = content_fingerprint(&file.bytes);
        Ok((
            pc::Artifact {
                id: content_hash.clone(),
                path: sandbox_path,
                size_bytes: file.bytes.len() as u64,
                content_hash,
            },
            file.bytes,
        ))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_outputs_dir_scans_to_empty() {
        let missing = std::path::Path::new("/no/such/awaken/outputs/dir");
        assert!(
            scan_outputs(
                &IsolatedRoot::new(missing),
                awaken_sandbox_fs::DirectoryIdentity {
                    device: 0,
                    inode: 0
                },
                "/mnt/session/outputs"
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn it_recurses_subdirs_and_sorts_by_sandbox_path_addressing_by_content() {
        let root = tempfile::tempdir().unwrap();
        let outputs = root.path().join("mnt/session/outputs");
        std::fs::create_dir_all(outputs.join("sub")).unwrap();
        std::fs::write(outputs.join("b.txt"), b"bbb").unwrap();
        std::fs::write(outputs.join("sub/a.txt"), b"aa").unwrap();

        let identity = awaken_sandbox_fs::directory_identity_nofollow(root.path()).unwrap();
        let arts = scan_outputs(
            &IsolatedRoot::new(root.path()),
            identity,
            "/mnt/session/outputs/",
        )
        .unwrap();
        assert_eq!(arts.len(), 2);
        // Sorted by the sandbox path string: "b.txt" < "sub/a.txt".
        assert_eq!(arts[0].0.path, "/mnt/session/outputs/b.txt");
        assert_eq!(arts[1].0.path, "/mnt/session/outputs/sub/a.txt");
        assert_eq!(arts[0].0.size_bytes, 3);
        // Cause C1: regular output bytes; effect E1: Artifact.id is the canonical
        // content hash (ADR-0038), independent of its logical path.
        assert_eq!(arts[0].0.id, content_fingerprint(b"bbb"));
        assert_eq!(arts[0].0.id, arts[0].0.content_hash);
        assert_eq!(arts[1].1, b"aa");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_an_artifact() {
        // Decision rule R2: a symlinked output makes the complete scan fail;
        // no caller may reinterpret a partial nofollow walk as an empty set.
        let root = tempfile::tempdir().unwrap();
        let outputs = root.path().join("mnt/session/outputs");
        std::fs::create_dir_all(&outputs).unwrap();
        std::os::unix::fs::symlink("/no/such/target", outputs.join("dangling")).unwrap();
        let identity = awaken_sandbox_fs::directory_identity_nofollow(root.path()).unwrap();
        assert!(
            scan_outputs(
                &IsolatedRoot::new(root.path()),
                identity,
                "/mnt/session/outputs/"
            )
            .is_err()
        );
    }
}
