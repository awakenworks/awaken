//! Shared fake-container adapter, used by the existing lifecycle tests.
use super::*;

#[async_trait]
impl awaken_container_contract::ContainerEnvironment for FakeContainer {
    async fn owned_process_binding(&self) -> Result<Option<String>, pc::SandboxError> {
        let shared = self.shared.lock().unwrap();
        if shared.contains_key("__process_binding_error") {
            return Err(pc::SandboxError::backend("process_binding", "unobservable"));
        }
        Ok(shared.get("__process_binding").map(|bytes| {
            String::from_utf8(bytes.clone()).expect("fixture process reference is UTF-8")
        }))
    }

    async fn checkpoint_upload(
        &self,
        _access: awaken_container_contract::ContainerCheckpointAccess<'_>,
        _mutation: awaken_container_contract::ContainerCheckpointMutation<'_>,
    ) -> Result<
        Option<awaken_provisioning_contract::SandboxCheckpointUpload>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test container environment does not implement a fenced checkpoint participant",
        ))
    }

    async fn remove_live_input_path(
        &self,
        _path: &str,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "late mount removal is unsupported on this test container",
        ))
    }

    fn record_owned_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        self.shared
            .lock()
            .unwrap()
            .entry(format!("__owned_path:{path}"))
            .or_default();
        Ok(())
    }

    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannelType>, pc::SandboxError> {
        self.resident_channel_opens
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut sink = Vec::new();
            let _ = theirs.read_to_end(&mut sink).await;
        });
        Ok(Box::new(ours))
    }

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<awaken_container_contract::RuntimeAgentProcess, pc::SandboxError> {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let repository_export = command.argv.iter().any(|part| part == "awaken-repo-push");
        if command.argv.iter().any(|part| part == "--stdio") {
            // P7 Container already exposes /mnt: preserve its deployed Hand
            // CLI rather than requiring the new local projection option. All
            // cold/reacquired container Hand cases share this contract oracle.
            assert_eq!(&command.argv[1..], &["hand", "--stdio"]);
            let spawn = self
                .hand_spawns
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if self
                .fail_hand_spawn_at
                .load(std::sync::atomic::Ordering::SeqCst)
                == spawn
            {
                return Err(pc::SandboxError::backend(
                    "spawn_agent_process",
                    "scripted Hand spawn failure",
                ));
            }
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut theirs = theirs;
                let mut sink = Vec::new();
                let _ = theirs.read_to_end(&mut sink).await;
            });
        } else if command.argv.iter().any(|part| part.contains("cat")) {
            let bytes = self
                .shared
                .lock()
                .unwrap()
                .get("marker")
                .cloned()
                .unwrap_or_default();
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let mut theirs = theirs;
                let _ = theirs.write_all(&bytes).await;
            });
        } else if repository_export {
            let observation = self
                .shared
                .lock()
                .unwrap()
                .get("__repository_export_observation")
                .cloned();
            if let Some(observation) = observation {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let mut theirs = theirs;
                    let _ = theirs.write_all(&observation).await;
                });
            } else {
                drop(theirs);
            }
        } else {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut theirs = theirs;
                let mut bytes = Vec::new();
                let _ = theirs.read_to_end(&mut bytes).await;
            });
        }
        let exit_code = if repository_export
            && self
                .shared
                .lock()
                .unwrap()
                .contains_key("__fail_repository_export")
        {
            19
        } else if repository_export
            && self
                .shared
                .lock()
                .unwrap()
                .contains_key("__repository_export_observation")
        {
            42
        } else {
            0
        };
        let unreapable = command.argv.iter().any(|part| part == "--stdio")
            && self
                .shared
                .lock()
                .unwrap()
                .contains_key("__unreapable_hand");
        let slow_reap = command.argv.iter().any(|part| part == "--stdio")
            && self.shared.lock().unwrap().contains_key("__slow_reap_hand");
        let process: Box<dyn pc::ProcessHandle> = if unreapable {
            Box::new(UnreapableProcess)
        } else if slow_reap {
            Box::new(SlowReapProcess)
        } else {
            Box::new(DoneProcess::exited("container-exec", exit_code))
        };
        Ok(awaken_container_contract::RuntimeAgentProcess {
            process,
            channel: Box::new(ours),
        })
    }

    async fn read_files(
        &self,
        _root: &str,
    ) -> Result<Vec<awaken_container_contract::EnvironmentFile>, pc::SandboxError> {
        if self
            .shared
            .lock()
            .unwrap()
            .contains_key("__fail_skill_read")
        {
            return Err(pc::SandboxError::backend(
                "read_files",
                "scripted Skill read outage",
            ));
        }
        Ok(self
            .shared
            .lock()
            .unwrap()
            .iter()
            .map(|(path, bytes)| awaken_container_contract::EnvironmentFile {
                path: path.clone(),
                bytes: bytes.clone(),
            })
            .collect())
    }
}
