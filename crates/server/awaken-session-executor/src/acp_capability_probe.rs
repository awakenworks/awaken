//! ACP capability negotiation through the already-selected Session Environment.
//!
//! The Worker ACP application remains the observation owner. This adapter only
//! opens a prompt-free process/channel in the same Namespace/Container provider
//! that later realizes published Runs, then tears the probe environment down.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_acp_contract::{
    AcpCapabilityHandshake, AcpCapabilityNegotiator, AcpCapabilityProbeConfig,
    NegotiatedAcpCapabilities,
};
use awaken_provisioning_contract as pc;

use crate::SharedHost;

pub struct SessionAcpCapabilityNegotiator {
    host: Arc<SharedHost>,
    timeout: Duration,
    handshake: Arc<dyn AcpCapabilityHandshake>,
}

impl SessionAcpCapabilityNegotiator {
    #[must_use]
    pub fn new(
        host: Arc<SharedHost>,
        timeout: Duration,
        handshake: Arc<dyn AcpCapabilityHandshake>,
    ) -> Self {
        Self {
            host,
            timeout,
            handshake,
        }
    }
}

#[async_trait]
impl AcpCapabilityNegotiator for SessionAcpCapabilityNegotiator {
    async fn negotiate(
        &self,
        argv: &[String],
        cwd: &Path,
        auth_method_id: Option<&str>,
    ) -> Result<NegotiatedAcpCapabilities, String> {
        let scope = format!("acp-capability-probe-{}", uuid::Uuid::new_v4().simple());
        let spec = crate::provisioning::agent_run_sandbox_spec(&scope);
        let environment = self
            .host
            .session_provider
            .create(&spec)
            .await
            .map_err(|error| format!("create ACP capability probe environment: {error}"))?;
        let result = async {
            let (process, mut channel) = environment
                .spawn_agent(pc::Command {
                    argv: argv.to_vec(),
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: Vec::new(),
                    stdio: pc::Stdio::Piped,
                })
                .await
                .map_err(|error| format!("spawn ACP capability probe: {error}"))?;
            let config = AcpCapabilityProbeConfig {
                session_cwd: Some(cwd.to_string_lossy().into_owned()),
                auth_method_id: auth_method_id.map(str::to_string),
            };
            let negotiated = tokio::time::timeout(
                self.timeout,
                self.handshake.negotiate(channel.as_mut(), &config),
            )
            .await
            .map_err(|_| "ACP capability probe timed out".to_string())
            .and_then(|result| result);
            drop(channel);
            let _ = process.signal(pc::Signal::Kill).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), process.wait()).await;
            negotiated
        }
        .await;
        let dispose = environment
            .dispose()
            .await
            .map_err(|error| format!("dispose ACP capability probe environment: {error}"));
        match (result, dispose) {
            (Ok(negotiated), Ok(())) => Ok(negotiated),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}
