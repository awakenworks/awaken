//! Live skill discovery cache for a Session-owned remote hand.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{ToolError, ToolExecutor, ToolOutput};
use awaken_sandbox_local::DiscoveredSkillFile;
use tokio::sync::Mutex;

use super::HandExecutorFactory;

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

struct HandBinding {
    process: Box<dyn pc::ProcessHandle>,
    executor: Arc<dyn ToolExecutor>,
}

/// The one Session-Environment-owned Hand binding.
///
/// Kubernetes attached-exec channels are live capabilities, not durable Session
/// identity. They may expire while a client is answering a question. This owner
/// reacquires the process/channel only when the executor proves that a request
/// never crossed the dispatch boundary; post-dispatch failures are never replayed.
pub(crate) struct RefreshingHandExecutor {
    sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
    skills: Arc<ContainerSkillCache>,
    factory: Arc<dyn HandExecutorFactory>,
    hand_bin: String,
    operation_scope: String,
    binding: Mutex<Option<HandBinding>>,
    closed: std::sync::atomic::AtomicBool,
}

impl RefreshingHandExecutor {
    pub(super) async fn new(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        skills: Arc<ContainerSkillCache>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
    ) -> Result<Self, pc::SandboxError> {
        let hand_bin = hand_bin.into();
        let operation_scope = sandbox.id().to_string();
        let binding = Self::launch(
            sandbox.as_ref(),
            factory.as_ref(),
            &hand_bin,
            &operation_scope,
        )
        .await?;
        Ok(Self {
            sandbox,
            skills,
            factory,
            hand_bin,
            operation_scope,
            binding: Mutex::new(Some(binding)),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    async fn launch(
        sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
        factory: &dyn HandExecutorFactory,
        hand_bin: &str,
        operation_scope: &str,
    ) -> Result<HandBinding, pc::SandboxError> {
        let process = sandbox
            .spawn_agent_process(pc::Command {
                argv: vec![hand_bin.to_owned(), "hand".into(), "--stdio".into()],
                cwd: "/workspace".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await?;
        Ok(HandBinding {
            executor: factory.bind(process.channel, operation_scope),
            process: process.process,
        })
    }

    async fn stop_binding(binding: HandBinding) {
        let _ = binding.process.signal(pc::Signal::Term).await;
        let _ = binding.process.wait().await;
    }

    pub(super) async fn stop(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.hibernate().await;
    }

    /// Release the current Hand process/channel without closing its Session owner.
    /// The next invocation reacquires one binding through the same serialized path.
    pub(super) async fn hibernate(&self) -> bool {
        if let Some(binding) = self.binding.lock().await.take() {
            Self::stop_binding(binding).await;
            true
        } else {
            false
        }
    }

    async fn replacement(&self) -> Result<HandBinding, ToolError> {
        Self::launch(
            self.sandbox.as_ref(),
            self.factory.as_ref(),
            &self.hand_bin,
            &self.operation_scope,
        )
        .await
        .map_err(|error| {
            ToolError::UnavailableBeforeDispatch(format!(
                "failed to reacquire Session hand: {error}"
            ))
        })
    }
}

#[async_trait]
impl ToolExecutor for RefreshingHandExecutor {
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed".into(),
            ));
        }
        let mut binding = self.binding.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed".into(),
            ));
        }
        if binding.is_none() {
            *binding = Some(self.replacement().await?);
        }
        let result = binding
            .as_ref()
            .expect("binding installed above")
            .executor
            .invoke(call)
            .await;
        let result = if matches!(result, Err(ToolError::UnavailableBeforeDispatch(_))) {
            tracing::warn!(
                session_environment = %self.operation_scope,
                tool_id = %call.tool_id,
                "reacquiring expired Session hand before tool dispatch"
            );
            if let Some(expired) = binding.take() {
                Self::stop_binding(expired).await;
            }
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                return Err(ToolError::UnavailableBeforeDispatch(
                    "Session hand binding closed during reacquisition".into(),
                ));
            }
            *binding = Some(self.replacement().await?);
            binding
                .as_ref()
                .expect("replacement binding installed above")
                .executor
                .invoke(call)
                .await
        } else {
            result
        };
        if matches!(call.tool_id.as_str(), "bash" | "write" | "edit")
            && let Err(error) = self.skills.refresh(self.sandbox.as_ref()).await
        {
            tracing::warn!(error = %error, "failed to refresh container skill catalog");
        }
        result
    }
}
