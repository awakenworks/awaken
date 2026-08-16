//! Worker-owned observation of configured image/Pod ACP adapters.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_acp_contract::{
    AcpCapabilityNegotiator, AcpCapabilityObservation, AcpCapabilityObservationSource,
    AcpCapabilityObservationState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfiguredAcpCapabilityTarget {
    pub cli_id: String,
    pub adapter_version: String,
    pub argv: Vec<String>,
    pub auth_method_id: Option<String>,
}

impl ConfiguredAcpCapabilityTarget {
    pub(crate) fn new(
        cli_id: impl Into<String>,
        adapter_version: impl Into<String>,
        argv: Vec<String>,
        auth_method_id: Option<String>,
    ) -> Result<Self, String> {
        let target = Self {
            cli_id: cli_id.into(),
            adapter_version: adapter_version.into(),
            argv,
            auth_method_id,
        };
        if target.cli_id.trim().is_empty()
            || target.adapter_version.trim().is_empty()
            || target
                .argv
                .first()
                .is_none_or(|program| program.trim().is_empty())
        {
            return Err(
                "configured ACP capability target requires cli, image identity, and argv".into(),
            );
        }
        Ok(target)
    }
}

pub(crate) struct ConfiguredAcpCapabilityObservationSource {
    targets: Vec<ConfiguredAcpCapabilityTarget>,
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
    cwd: PathBuf,
}

impl ConfiguredAcpCapabilityObservationSource {
    pub(crate) fn new(
        mut targets: Vec<ConfiguredAcpCapabilityTarget>,
        negotiator: Arc<dyn AcpCapabilityNegotiator>,
        cwd: PathBuf,
    ) -> Self {
        targets.sort_by(|left, right| left.cli_id.cmp(&right.cli_id));
        Self {
            targets,
            negotiator,
            cwd,
        }
    }
}

#[async_trait]
impl AcpCapabilityObservationSource for ConfiguredAcpCapabilityObservationSource {
    async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
        let mut probes = tokio::task::JoinSet::new();
        for (index, target) in self.targets.iter().cloned().enumerate() {
            let negotiator = self.negotiator.clone();
            let cwd = self.cwd.clone();
            probes.spawn(async move {
                let observed_at_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let observation = match negotiator
                    .negotiate(&target.argv, &cwd, target.auth_method_id.as_deref())
                    .await
                {
                    Ok(negotiated) => AcpCapabilityObservation {
                        backend_ref: format!("acp:{}", target.cli_id),
                        fingerprint: Some(awaken_acp_contract::capability_fingerprint(
                            &target.cli_id,
                            &target.adapter_version,
                            &negotiated,
                        )),
                        adapter_version: target.adapter_version,
                        state: AcpCapabilityObservationState::Verified,
                        observed_at_ms,
                        negotiated: Some(negotiated),
                        reason_code: None,
                    },
                    Err(error) => {
                        return Err(format!(
                            "acp:{} capability probe failed: {error}",
                            target.cli_id
                        ));
                    }
                };
                Ok::<_, String>((index, observation))
            });
        }
        let mut observations = Vec::with_capacity(self.targets.len());
        while let Some(result) = probes.join_next().await {
            observations.push(result.map_err(|error| error.to_string())??);
        }
        observations.sort_by_key(|(index, _)| *index);
        Ok(observations
            .into_iter()
            .map(|(_, observation)| observation)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use awaken_acp_contract::NegotiatedAcpCapabilities;

    use super::*;

    struct Probe;

    #[async_trait]
    impl AcpCapabilityNegotiator for Probe {
        async fn negotiate(
            &self,
            argv: &[String],
            _cwd: &Path,
            _auth_method_id: Option<&str>,
        ) -> Result<NegotiatedAcpCapabilities, String> {
            (argv[0] == "verified")
                .then(|| NegotiatedAcpCapabilities {
                    protocol_version: "1".into(),
                    load_session: true,
                    prompt_image: false,
                    prompt_audio: false,
                    prompt_embedded_context: false,
                    mcp_http: true,
                    mcp_sse: false,
                    session_list: false,
                    modes: Vec::new(),
                    config_options: Vec::new(),
                })
                .ok_or_else(|| "probe failed".to_string())
        }
    }

    #[tokio::test]
    async fn configured_capability_decision_table() {
        // Causes: C1 identity/image/argv are complete; C2 live negotiation
        // succeeds. Effects: E1 incomplete targets fail before observation; E2
        // success emits coherent Verified evidence; E3 a failed causal batch
        // returns an error so the liveness cache retains prior verified evidence.
        //
        // | Rule | C1 | C2 | Effect |
        // | T1 | no  | -   | E1 |
        // | T2 | yes | yes | E2 |
        // | T3 | yes | no  | E3 |
        assert!(
            ConfiguredAcpCapabilityTarget::new("", "image:v1", vec!["ok".into()], None).is_err(),
            "T1"
        );
        let verified = ConfiguredAcpCapabilityObservationSource::new(
            vec![
                ConfiguredAcpCapabilityTarget::new(
                    "a-verified",
                    "image:good",
                    vec!["verified".into()],
                    None,
                )
                .unwrap(),
            ],
            Arc::new(Probe),
            PathBuf::from("/workspace"),
        );
        let observations = verified.capability_observations().await.unwrap();
        assert_eq!(
            observations[0].state,
            AcpCapabilityObservationState::Verified,
            "T2"
        );
        assert!(observations[0].fingerprint.is_some(), "T2");
        let failed = ConfiguredAcpCapabilityObservationSource::new(
            vec![
                ConfiguredAcpCapabilityTarget::new(
                    "z-failed",
                    "image:bad",
                    vec!["failed".into()],
                    None,
                )
                .unwrap(),
            ],
            Arc::new(Probe),
            PathBuf::from("/workspace"),
        );
        assert!(
            failed
                .capability_observations()
                .await
                .unwrap_err()
                .contains("acp:z-failed capability probe failed"),
            "T3"
        );
    }

    struct ConcurrentProbe {
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl AcpCapabilityNegotiator for ConcurrentProbe {
        async fn negotiate(
            &self,
            _argv: &[String],
            _cwd: &Path,
            _auth_method_id: Option<&str>,
        ) -> Result<NegotiatedAcpCapabilities, String> {
            self.barrier.wait().await;
            Ok(NegotiatedAcpCapabilities {
                protocol_version: "1".into(),
                load_session: true,
                prompt_image: false,
                prompt_audio: false,
                prompt_embedded_context: false,
                mcp_http: true,
                mcp_sse: false,
                session_list: false,
                modes: Vec::new(),
                config_options: Vec::new(),
            })
        }
    }

    /// Cause/effect rule: two independent configured targets reaching a barrier
    /// complete only when observations run concurrently; sequential probing
    /// times out. The resulting observations remain deterministically sorted.
    #[tokio::test]
    async fn configured_targets_probe_concurrently_and_sort_results() {
        let source = ConfiguredAcpCapabilityObservationSource::new(
            ["second", "first"]
                .into_iter()
                .map(|id| {
                    ConfiguredAcpCapabilityTarget::new(
                        id,
                        "image:immutable",
                        vec!["probe".into()],
                        None,
                    )
                    .unwrap()
                })
                .collect(),
            Arc::new(ConcurrentProbe {
                barrier: Arc::new(tokio::sync::Barrier::new(2)),
            }),
            PathBuf::from("/workspace"),
        );
        let observations = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            source.capability_observations(),
        )
        .await
        .expect("probes must overlap")
        .unwrap();
        assert_eq!(observations[0].backend_ref, "acp:first");
        assert_eq!(observations[1].backend_ref, "acp:second");
    }
}
