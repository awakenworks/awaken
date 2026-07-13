//! Shared outputs-directory scan used by every local-machine provider tier: an
//! artifact is a file the agent wrote under the environment's outputs path,
//! addressed by a stable id derived from its sandbox-relative path.

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
            let rel = path.strip_prefix(host_outputs).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let sandbox_path = format!("{}/{}", outputs_path.trim_end_matches('/'), rel_str);
            let bytes = std::fs::read(&path).map_err(err)?;
            out.push((
                pc::Artifact {
                    id: content_fingerprint(rel_str.as_bytes()),
                    path: sandbox_path,
                    size_bytes: bytes.len() as u64,
                    content_hash: content_fingerprint(&bytes),
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
    fn it_recurses_subdirs_and_sorts_by_sandbox_path_addressing_by_rel_path() {
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
        // The id is derived from the sandbox-relative path so read_artifact can match.
        assert_eq!(arts[0].0.id, content_fingerprint(b"b.txt"));
        // The returned host path points back at the real file.
        assert_eq!(arts[1].1, root.path().join("sub/a.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_entry_surfaces_a_sandbox_error() {
        // A dangling symlink is listed by read_dir but fails to read → the err() path.
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/no/such/target", root.path().join("dangling")).unwrap();
        assert!(scan_outputs(root.path(), "/mnt/session/outputs").is_err());
    }
}
