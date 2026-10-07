// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: the Workdir implementation of the Runtime Host's auxiliary-run sandbox
// @formal reason: port; it mints fresh process-unique roots and admits no Session effect. This is a
// @formal reason: product-requirement classification, not a formal-proof claim.
//! Auxiliary-run sandboxes realized by the Workdir provider: the concrete side
//! of the Runtime Host's `awaken-session-environment-contract` ports.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_session_environment_contract::{
    AuxiliarySandboxFactory, AuxiliarySandboxProvider, RootedToolSandbox,
};
use awaken_tool_contract::tool::RawTool;

use crate::{LocalProvider, LocalSandbox};

impl RootedToolSandbox for LocalSandbox {
    fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        LocalSandbox::rooted_tools(self)
    }
}

#[async_trait]
impl AuxiliarySandboxProvider for LocalProvider {
    async fn create_rooted_sandbox(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn RootedToolSandbox>, pc::SandboxError> {
        Ok(Box::new(self.create_sandbox(spec).await?))
    }
}

/// One Workdir provider per auxiliary purpose, each at a fresh process-unique
/// root `<base>/<pid>-<purpose>-<n>`. Consumer: process compositions, which
/// hand it to the Runtime Host for its auxiliary Agent Runs.
pub struct LocalAuxiliarySandboxes {
    base: PathBuf,
    sequence: AtomicU64,
}

impl LocalAuxiliarySandboxes {
    /// Auxiliary sandbox roots under `base`.
    #[must_use]
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            sequence: AtomicU64::new(0),
        }
    }

    /// The roots the Runtime Host has always used for auxiliary Runs:
    /// `$TMPDIR/awaken-coordinator`.
    #[must_use]
    pub fn in_process_temp_dir() -> Self {
        Self::new(std::env::temp_dir().join("awaken-coordinator"))
    }
}

impl AuxiliarySandboxFactory for LocalAuxiliarySandboxes {
    fn provider(&self, purpose: &str) -> Arc<dyn AuxiliarySandboxProvider> {
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst);
        let root = self
            .base
            .join(format!("{}-{purpose}-{sequence}", std::process::id()));
        Arc::new(LocalProvider::new(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_purpose_gets_a_fresh_root_whose_sandbox_exposes_rooted_tools() {
        // Decision table: A1 two providers minted for one purpose => distinct
        // roots; A2 a sandbox realized by a minted provider => its rooted tools
        // are the Workdir provider's own non-empty tool set.
        let base = tempfile::tempdir().unwrap();
        let factory = LocalAuxiliarySandboxes::new(base.path());
        let first = factory.provider("compact");
        let second = factory.provider("compact");
        let spec = pc::SandboxSpec {
            process_tree_supervision: false,
            scope: "aux-run".into(),
            isolation: pc::IsolationClass::Workdir,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::Unrestricted,
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            control_services: Default::default(),
        };
        let a = first.create_rooted_sandbox(&spec).await.unwrap();
        let b = second.create_rooted_sandbox(&spec).await.unwrap();
        assert!(!a.rooted_tools().is_empty(), "A2");
        let roots = std::fs::read_dir(base.path())
            .unwrap()
            .filter_map(Result::ok)
            .count();
        assert_eq!(roots, 2, "A1 one fresh root per minted provider");
        drop((a, b));
    }
}
