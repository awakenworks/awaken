//! Bounded, backend-neutral file harvesting over attached container exec.

use std::io::Read;
use std::path::Component;

use tokio::io::AsyncReadExt;

use crate::{ContainerRuntime, ContainerSandbox, EnvironmentFile};
use awaken_provisioning_contract as pc;

const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 10_000;

fn safe_root(root: &str) -> bool {
    root.starts_with('/')
        && !root
            .split('/')
            .any(|component| component == "." || component == "..")
}

fn decode(bytes: &[u8]) -> Result<Vec<EnvironmentFile>, pc::SandboxError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
    let mut files = Vec::new();
    for entry in archive
        .entries()
        .map_err(|error| pc::SandboxError::new(error.to_string()))?
    {
        let mut entry = entry.map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        let safe: Vec<_> = path
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
                Component::CurDir => None,
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
            })
            .collect();
        if safe.is_empty()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
        {
            return Err(pc::SandboxError::new("unsafe path in container archive"));
        }
        let mut content = Vec::new();
        entry
            .read_to_end(&mut content)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        files.push(EnvironmentFile {
            path: safe.join("/"),
            bytes: content,
        });
        if files.len() > MAX_FILES {
            return Err(pc::SandboxError::new(
                "container archive has too many files",
            ));
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

pub(crate) async fn read_files<R: ContainerRuntime + 'static>(
    sandbox: &ContainerSandbox<R>,
    root: &str,
) -> Result<Vec<EnvironmentFile>, pc::SandboxError> {
    if !safe_root(root) {
        return Err(pc::SandboxError::new("unsafe container file root"));
    }
    let process = sandbox
        .spawn_agent(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "test ! -d \"$1\" || exec tar -C \"$1\" -cf - -- .".into(),
                "awaken-read-files".into(),
                root.into(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await?;
    let mut bytes = Vec::new();
    process
        .channel
        .take((MAX_ARCHIVE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        let _ = process.process.signal(pc::Signal::Kill).await;
        return Err(pc::SandboxError::new(
            "container file archive exceeds limit",
        ));
    }
    let status = process.process.wait().await?;
    if status.code != Some(0) {
        return Err(pc::SandboxError::new(format!(
            "container file scan exited {:?}",
            status.code
        )));
    }
    decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_decode_is_sorted_and_file_only() {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for (path, content) in [("./b.txt", b"b".as_slice()), ("a/x", b"a")] {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o600);
                header.set_cksum();
                builder.append_data(&mut header, path, content).unwrap();
            }
            builder.finish().unwrap();
        }
        assert_eq!(
            decode(&bytes).unwrap(),
            vec![
                EnvironmentFile {
                    path: "a/x".into(),
                    bytes: b"a".to_vec(),
                },
                EnvironmentFile {
                    path: "b.txt".into(),
                    bytes: b"b".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn root_validation_rejects_relative_and_parent_paths() {
        assert!(safe_root("/workspace/outputs"));
        assert!(!safe_root("workspace/outputs"));
        assert!(!safe_root("/workspace/../secret"));
    }
}
