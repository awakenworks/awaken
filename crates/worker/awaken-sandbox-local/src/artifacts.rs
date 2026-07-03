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
