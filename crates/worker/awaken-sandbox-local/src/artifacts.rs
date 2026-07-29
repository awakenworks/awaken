//! Shared outputs-directory scan used by every local-machine provider tier: an
//! artifact is a file the agent wrote under the environment's outputs path,
//! addressed by the immutable content id used by the shared FileStore.

use std::path::PathBuf;

use awaken_provisioning_contract as pc;

use crate::content_fingerprint;

fn err(e: impl ToString) -> pc::SandboxError {
    pc::SandboxError::new(e.to_string())
}

/// Depth-first scan of `host_outputs` → `(artifact, host_path)` pairs, sorted by
/// sandbox path. `outputs_path` is the sandbox-absolute mount (e.g.
/// `/mnt/session/outputs`) used to build each artifact's reported path. Missing
/// or unreadable entries are skipped; the id lets `read_artifact` recompute a match.
pub(crate) fn scan_outputs(
    host_outputs: &std::path::Path,
    outputs_path: &str,
) -> Result<Vec<(pc::Artifact, PathBuf)>, pc::SandboxError> {
    let mut out = Vec::new();
    let mut stack = vec![host_outputs.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            // Never follow links out of the sandbox output tree. Provider-owned
            // artifact discovery accepts regular files only.
            if !ft.is_file() {
                continue;
            }
            let rel = path.strip_prefix(host_outputs).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let sandbox_path = format!("{}/{}", outputs_path.trim_end_matches('/'), rel_str);
            let bytes = std::fs::read(&path).map_err(err)?;
            let content_hash = content_fingerprint(&bytes);
            out.push((
                pc::Artifact {
                    id: content_hash.clone(),
                    path: sandbox_path,
                    size_bytes: bytes.len() as u64,
                    content_hash,
                },
                path,
            ));
        }
    }
    out.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_outputs_dir_scans_to_empty() {
        let missing = std::path::Path::new("/no/such/awaken/outputs/dir");
        assert!(
            scan_outputs(missing, "/mnt/session/outputs")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn it_recurses_subdirs_and_sorts_by_sandbox_path_addressing_by_content() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("b.txt"), b"bbb").unwrap();
        std::fs::write(root.path().join("sub/a.txt"), b"aa").unwrap();

        let arts = scan_outputs(root.path(), "/mnt/session/outputs/").unwrap();
        assert_eq!(arts.len(), 2);
        // Sorted by the sandbox path string: "b.txt" < "sub/a.txt".
        assert_eq!(arts[0].0.path, "/mnt/session/outputs/b.txt");
        assert_eq!(arts[1].0.path, "/mnt/session/outputs/sub/a.txt");
        assert_eq!(arts[0].0.size_bytes, 3);
        // Cause C1: regular output bytes; effect E1: Artifact.id is the canonical
        // content hash (ADR-0038), independent of its logical path.
        assert_eq!(arts[0].0.id, content_fingerprint(b"bbb"));
        assert_eq!(arts[0].0.id, arts[0].0.content_hash);
        // The returned host path points back at the real file.
        assert_eq!(arts[1].1, root.path().join("sub/a.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_not_an_artifact() {
        // Decision rule R2: non-regular output (C1=false) => no artifact and no
        // attempt to read through the sandbox boundary (E2).
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/no/such/target", root.path().join("dangling")).unwrap();
        assert!(
            scan_outputs(root.path(), "/mnt/session/outputs")
                .unwrap()
                .is_empty()
        );
    }
}
