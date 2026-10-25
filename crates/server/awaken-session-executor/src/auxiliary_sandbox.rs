//! The Host's auxiliary-run sandboxes before the process composition installs
//! its factory (`SessionExecutorBuilder::with_auxiliary_sandboxes`).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_session_environment_contract::{
    AuxiliarySandboxFactory, AuxiliarySandboxProvider, RootedToolSandbox,
};

/// Refuses every auxiliary sandbox: the shared Host never chooses a sandbox
/// implementation itself. Consumer: a Host whose composition installed none.
struct UninstalledAuxiliarySandboxes;

impl AuxiliarySandboxFactory for UninstalledAuxiliarySandboxes {
    fn provider(&self, purpose: &str) -> Arc<dyn AuxiliarySandboxProvider> {
        Arc::new(UninstalledAuxiliarySandboxProvider {
            purpose: purpose.to_owned(),
        })
    }
}

struct UninstalledAuxiliarySandboxProvider {
    purpose: String,
}

#[async_trait]
impl AuxiliarySandboxProvider for UninstalledAuxiliarySandboxProvider {
    async fn create_rooted_sandbox(
        &self,
        _spec: &pc::SandboxSpec,
    ) -> Result<Box<dyn RootedToolSandbox>, pc::SandboxError> {
        Err(pc::SandboxError::not_ready(format!(
            "the {} auxiliary Agent Run needs the process composition to install auxiliary \
             sandboxes on this Host",
            self.purpose
        )))
    }
}

/// The Host's construction-time factory: the Workdir auxiliary sandboxes in its
/// own unit tests (a dev-dependency), otherwise the refusing factory until the
/// process composition installs one.
pub(crate) fn default_auxiliary_sandboxes() -> Arc<dyn AuxiliarySandboxFactory> {
    #[cfg(test)]
    {
        Arc::new(awaken_sandbox_local::LocalAuxiliarySandboxes::in_process_temp_dir())
    }
    #[cfg(not(test))]
    {
        Arc::new(UninstalledAuxiliarySandboxes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_uninstalled_factory_refuses_every_auxiliary_sandbox() {
        // Decision table: U1 any purpose from the uninstalled factory =>
        // refused as not ready, naming that purpose; no sandbox is realized.
        let spec = crate::sandbox_layout::agent_run_sandbox_spec("aux-thread");
        for purpose in ["compact", "mem", "mem-select"] {
            let error = match UninstalledAuxiliarySandboxes
                .provider(purpose)
                .create_rooted_sandbox(&spec)
                .await
            {
                Ok(_) => panic!("U1 {purpose} must be refused"),
                Err(error) => error,
            };
            assert!(error.to_string().contains(purpose), "U1 {purpose}: {error}");
            assert!(error.to_string().contains("auxiliary"), "U1 {purpose}");
        }
    }
}
