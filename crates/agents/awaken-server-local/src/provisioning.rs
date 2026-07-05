//! Session provisioning + capability advertisement on [`SharedHost`]: the sandbox
//! spec (with staged resource mounts, ADR-0038), the per-thread resource staging +
//! blob store accessor, and the tool/skill/delegate sets advertised on a managed
//! session. Split out of `host.rs` to keep that file under the length limit; these
//! are the same `SharedHost` (fields are `pub(crate)`).

use std::sync::Arc;

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_sandbox_local::{FileStore, Mount, SandboxSpec};

use crate::host::SharedHost;

/// A thread's staged resources (ADR-0038): the legacy [`Mount`]s realized into its
/// sandbox plus the prompt fragments appended to its system prompt. Built by a
/// session's `prepare_session` from the wire `resources[]`.
#[derive(Default, Clone)]
pub(crate) struct StagedResources {
    pub mounts: Vec<Mount>,
    pub prompts: Vec<String>,
}

impl SharedHost {
    /// The provisioning request for a thread. Skills are not a sandbox mount
    /// (ADR-0036); the environment provisions isolation tools plus the session's
    /// staged resource mounts (ADR-0038), each realized read-only under `.mnt/`.
    pub(crate) fn sandbox_spec(&self, thread: &str) -> SandboxSpec {
        let mut spec = SandboxSpec::new(thread);
        if let Some(staged) = self.thread_resources.lock().unwrap().get(thread) {
            for mount in &staged.mounts {
                spec = spec.with_mount(mount.clone());
            }
        }
        spec
    }

    /// Stage a thread's resources (mounts + prompt fragments); consumed by
    /// `sandbox_spec` and injected into the run's system prompt. From `prepare_session`.
    pub(crate) fn register_thread_resources(&self, thread: &str, staged: StagedResources) {
        self.thread_resources
            .lock()
            .unwrap()
            .insert(thread.to_string(), staged);
    }

    /// The prompt fragments staged for `thread`'s bound resources (ADR-0038 A3a).
    pub(crate) fn thread_resource_prompts(&self, thread: &str) -> Vec<String> {
        self.thread_resources
            .lock()
            .unwrap()
            .get(thread)
            .map(|s| s.prompts.clone())
            .unwrap_or_default()
    }

    /// The content-addressed blob store (Files API, file-resource mounts, artifacts).
    pub fn file_store(&self) -> Arc<dyn FileStore> {
        self.file_store.clone()
    }

    /// Collect a session's output artifacts (ADR-0038): the files the agent wrote
    /// under the environment's `outputs/` dir, each stored into the blob store and
    /// returned as `(content_id, logical_path)`. This is the sandbox→host reverse
    /// channel behind `GET /v1/files?scope_id=<session>`; empty when the session has
    /// no environment or wrote nothing.
    pub async fn session_artifacts(&self, thread: &str) -> Vec<(String, String)> {
        let env = {
            let sessions = self.sessions.lock().await;
            sessions.get(thread).map(|ctx| ctx.env.clone())
        };
        let Some(env) = env else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (path, bytes) in env.list_files("outputs") {
            if let Ok(id) = self.file_store.put(&bytes).await {
                out.push((id, path));
            }
        }
        out
    }

    /// The registered built-in tools advertised on a managed session's agent object:
    /// each hand-tool id and whether its calls require confirmation. Folded into the
    /// public `agent_toolset` by the adapter. Deterministic from host config.
    pub fn builtin_tools(&self) -> Vec<(String, bool)> {
        crate::config::builtin_hand_tools()
    }

    /// The client-executed (custom) tools advertised on a managed session: their
    /// descriptors, so the adapter can shape each as a `custom` tool definition.
    pub fn custom_tools(&self) -> Vec<ToolDescriptor> {
        self.client_tools
            .iter()
            .map(|id| crate::config::client_tool_descriptor(id))
            .collect()
    }

    /// The skill ids offered on every thread (advertised as the agent's `skills`).
    pub fn skill_ids(&self) -> Vec<String> {
        self.skills.iter().map(|s| s.id.clone()).collect()
    }

    /// The delegate agent ids (advertised as the agent's `multiagent` roster).
    pub fn delegate_ids(&self) -> Vec<String> {
        self.delegates.iter().cloned().collect()
    }
}
