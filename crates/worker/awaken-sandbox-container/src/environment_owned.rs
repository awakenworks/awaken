//! One-shot compatibility ownership for a Session environment.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::ContainerEnvironment;

/// Disposes a compatibility caller's environment only after its exec reaches
/// terminal. The unified host retains the environment directly and does not need
/// this wrapper.
pub(crate) struct EnvironmentOwnedProcess {
    pub(crate) inner: Box<dyn pc::ProcessHandle>,
    pub(crate) environment: Arc<dyn ContainerEnvironment>,
}

#[async_trait]
impl pc::ProcessHandle for EnvironmentOwnedProcess {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        let status = self.inner.wait().await?;
        self.environment.dispose().await?;
        Ok(status)
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        let status = self.inner.poll().await?;
        if status.is_some() {
            self.environment.dispose().await?;
        }
        Ok(status)
    }

    async fn signal(&self, signal: pc::Signal) -> Result<(), pc::SandboxError> {
        self.inner.signal(signal).await
    }
}
