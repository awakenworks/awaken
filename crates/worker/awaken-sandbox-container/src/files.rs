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
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::io::AsyncWriteExt;

    use super::*;

    struct ScanProcess {
        status: pc::ExitStatus,
    }

    #[async_trait]
    impl pc::ProcessHandle for ScanProcess {
        fn id(&self) -> &str {
            "scan"
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(self.status.clone())
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(Some(self.status.clone()))
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    struct ScanRuntime {
        archive: Vec<u8>,
        code: i32,
    }

    #[async_trait]
    impl ContainerRuntime for ScanRuntime {
        async fn probe_ready(&self) -> Result<(), crate::RuntimeError> {
            Ok(())
        }

        async fn create(
            &self,
            _id: &str,
            _plan: &crate::ContainerPlan,
        ) -> Result<String, crate::RuntimeError> {
            Err(crate::RuntimeError::Backend("not used".into()))
        }

        async fn spawn_agent(
            &self,
            _container_id: &str,
            _command: pc::MaterializedCommand,
        ) -> Result<crate::RuntimeAgentProcess, crate::RuntimeError> {
            let (channel, mut writer) = tokio::io::duplex(self.archive.len().max(1));
            let archive = self.archive.clone();
            tokio::spawn(async move {
                writer.write_all(&archive).await.unwrap();
            });
            Ok(crate::RuntimeAgentProcess {
                process: Box::new(ScanProcess {
                    status: pc::ExitStatus {
                        code: Some(self.code),
                        signaled: false,
                    },
                }),
                channel: Box::new(channel),
            })
        }

        async fn open_channel(
            &self,
            _container_id: &str,
        ) -> Result<Box<dyn awaken_agent_channel::AgentChannel>, crate::RuntimeError> {
            Err(crate::RuntimeError::Backend("not used".into()))
        }

        async fn inspect(
            &self,
            _container_id: &str,
        ) -> Result<crate::ContainerState, crate::RuntimeError> {
            Ok(crate::ContainerState::Running)
        }

        async fn wait(&self, _container_id: &str) -> Result<pc::ExitStatus, crate::RuntimeError> {
            Err(crate::RuntimeError::Backend("not used".into()))
        }

        async fn poll(
            &self,
            _container_id: &str,
        ) -> Result<Option<pc::ExitStatus>, crate::RuntimeError> {
            Ok(None)
        }

        async fn signal(
            &self,
            _container_id: &str,
            _signal: pc::Signal,
        ) -> Result<(), crate::RuntimeError> {
            Ok(())
        }

        async fn artifacts(
            &self,
            _container_id: &str,
        ) -> Result<Vec<pc::Artifact>, crate::RuntimeError> {
            Ok(Vec::new())
        }

        async fn read_artifact(
            &self,
            _container_id: &str,
            _artifact_id: &str,
        ) -> Result<Vec<u8>, crate::RuntimeError> {
            Err(crate::RuntimeError::Backend("not used".into()))
        }

        async fn touch_lease(&self, _container_id: &str) -> Result<(), crate::RuntimeError> {
            Ok(())
        }

        async fn remove(&self, _container_id: &str) -> Result<(), crate::RuntimeError> {
            Ok(())
        }
    }

    fn sandbox(archive: Vec<u8>, code: i32) -> ContainerSandbox<ScanRuntime> {
        ContainerSandbox {
            runtime: Arc::new(ScanRuntime { archive, code }),
            id: "scan-sandbox".into(),
            container_id: "scan-container".into(),
            outputs_path: "/outputs".into(),
            base_env: Vec::new(),
            blobs: Default::default(),
            file_store: None,
            live_input_projection: false,
            runtime_handle: None,
            realized: Vec::new(),
            recovered: false,
            lifecycle: Arc::new(crate::ContainerCleanupState::completed(None)),
        }
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for (path, content) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o600);
                header.set_cksum();
                builder.append_data(&mut header, path, *content).unwrap();
            }
            builder.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn archive_decode_is_sorted_and_file_only() {
        let bytes = archive(&[("./b.txt", b"b"), ("a/x", b"a")]);
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

    #[test]
    fn archive_decode_handles_empty_corrupt_and_bounded_inputs() {
        assert!(decode(&[]).unwrap().is_empty());
        assert!(decode(b"not a tar archive").is_err());

        let mut bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut bytes);
            for index in 0..=MAX_FILES {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o600);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("entry-{index}"), &[][..])
                    .unwrap();
            }
            builder.finish().unwrap();
        }
        assert!(decode(&bytes).is_err(), "the file-count bound is enforced");
    }

    #[tokio::test]
    async fn read_files_drives_attached_exec_and_propagates_scan_failures() {
        let bytes = archive(&[("result.txt", b"result")]);
        assert_eq!(
            read_files(&sandbox(bytes, 0), "/outputs").await.unwrap(),
            vec![EnvironmentFile {
                path: "result.txt".into(),
                bytes: b"result".to_vec(),
            }]
        );

        assert!(
            read_files(&sandbox(Vec::new(), 0), "relative")
                .await
                .is_err()
        );
        assert!(
            read_files(&sandbox(Vec::new(), 17), "/outputs")
                .await
                .is_err()
        );
    }
}
