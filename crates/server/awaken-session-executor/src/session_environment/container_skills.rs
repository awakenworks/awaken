//! Live skill discovery cache for a Session-owned remote hand.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{ToolError, ToolExecutor, ToolOutput};
use awaken_sandbox_local::DiscoveredSkillFile;

#[derive(Default)]
pub(crate) struct ContainerSkillCache {
    dirs: std::sync::Mutex<std::collections::BTreeSet<String>>,
    files: std::sync::RwLock<std::collections::BTreeMap<String, Vec<DiscoveredSkillFile>>>,
}

impl ContainerSkillCache {
    pub(super) fn register(&self, subdir: &str) {
        self.dirs.lock().unwrap().insert(subdir.to_string());
    }

    pub(super) fn get(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        self.register(subdir);
        self.files
            .read()
            .unwrap()
            .get(subdir)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) async fn refresh(
        &self,
        sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    ) -> Result<(), pc::SandboxError> {
        let dirs: Vec<_> = self.dirs.lock().unwrap().iter().cloned().collect();
        for subdir in dirs {
            let root = super::container_files::workspace_path(&subdir)?;
            let mut discovered = Vec::new();
            for file in sandbox.read_files(&root).await? {
                let Some((id, name)) = file.path.split_once('/') else {
                    continue;
                };
                if name != "SKILL.md" || id.is_empty() || id.contains('/') {
                    continue;
                }
                let Ok(content) = String::from_utf8(file.bytes) else {
                    continue;
                };
                discovered.push(DiscoveredSkillFile {
                    id: id.to_string(),
                    content,
                    dir: format!("{subdir}/{id}"),
                });
            }
            discovered.sort_by(|left, right| left.id.cmp(&right.id));
            self.files.write().unwrap().insert(subdir, discovered);
        }
        Ok(())
    }
}

pub(super) struct RefreshingHandExecutor {
    pub(super) inner: Arc<dyn ToolExecutor>,
    pub(super) sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
    pub(super) skills: Arc<ContainerSkillCache>,
}

#[async_trait]
impl ToolExecutor for RefreshingHandExecutor {
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        let result = self.inner.invoke(call).await;
        if matches!(call.tool_id.as_str(), "bash" | "write" | "edit")
            && let Err(error) = self.skills.refresh(self.sandbox.as_ref()).await
        {
            tracing::warn!(error = %error, "failed to refresh container skill catalog");
        }
        result
    }
}
