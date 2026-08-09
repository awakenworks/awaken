//! Process-local credential realization capabilities.

use super::*;

impl SharedHost {
    /// Exact independent credential-adapter profiles installed in this process.
    /// Both the process dispatch pool and per-Session durable ingress consume this
    /// one declaration; neither may infer custody from only the Native adapter.
    pub(crate) fn local_credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let mut profiles = vec![self.inference_routing.credential_realization_capabilities()];
        if let (Some(acp), Some(profile)) = (&self.acp, &self.deployment.acp) {
            profiles.extend(profile.cli_ids().filter_map(|cli| {
                let backend =
                    awaken_runtime_contract::resolved::Backend::from_ref(&format!("acp:{cli}"));
                match acp.credential_realization_capabilities(&backend) {
                    Ok(capabilities) => Some(capabilities),
                    Err(error) => {
                        tracing::error!(backend = %format!("acp:{cli}"), %error,
                            "configured ACP credential capability is unavailable");
                        None
                    }
                }
            }));
        }
        awaken_runtime_contract::CredentialRealizationCapabilities::alternatives(profiles)
    }
}
