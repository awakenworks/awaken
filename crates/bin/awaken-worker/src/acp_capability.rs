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
    // The source is rebuilt with the Worker/image incarnation, so one complete
    // successful batch is immutable for exactly that bounded lifetime.
    successful_observations: tokio::sync::OnceCell<Vec<AcpCapabilityObservation>>,
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
            successful_observations: tokio::sync::OnceCell::new(),
        }
    }

    async fn probe_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
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

#[async_trait]
impl AcpCapabilityObservationSource for ConfiguredAcpCapabilityObservationSource {
    async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
        self.successful_observations
            .get_or_try_init(|| self.probe_observations())
            .await
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_acp_contract::NegotiatedAcpCapabilities;

    use super::*;

    fn negotiated_capabilities() -> NegotiatedAcpCapabilities {
        NegotiatedAcpCapabilities {
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
        }
    }

    fn configured_target(id: &str, argv: &str) -> ConfiguredAcpCapabilityTarget {
        ConfiguredAcpCapabilityTarget::new(id, "container-image:immutable", vec![argv.into()], None)
            .unwrap()
    }

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
                .then(negotiated_capabilities)
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
            Ok(negotiated_capabilities())
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
                .map(|id| configured_target(id, "probe"))
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

    #[derive(Default)]
    struct CountingLifecycleProbe {
        creates: AtomicUsize,
        disposes: AtomicUsize,
        failures_left: AtomicUsize,
    }

    #[async_trait]
    impl AcpCapabilityNegotiator for CountingLifecycleProbe {
        async fn negotiate(
            &self,
            _argv: &[String],
            _cwd: &Path,
            _auth_method_id: Option<&str>,
        ) -> Result<NegotiatedAcpCapabilities, String> {
            // One production negotiation owns one ephemeral provider create and
            // its matching dispose. Model those physical effects separately so
            // cache behavior cannot regress into hidden Sandbox churn.
            self.creates.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let failed = self
                .failures_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            self.disposes.fetch_add(1, Ordering::SeqCst);
            if failed {
                Err("injected capability failure".into())
            } else {
                Ok(negotiated_capabilities())
            }
        }
    }

    /// Configured capability batch cause/effect graph:
    /// C1=the incarnation-local cache is empty; C2=the complete ordered batch
    /// succeeds; C3=refreshes overlap or repeat; C4=a physical probe batch fails;
    /// C5=the source is rebuilt for a new Worker/image incarnation.
    /// Effects: E1=C1+C2 probes every target once and caches only the complete
    /// ordered result; E2=C1+C2+C3 joins or reuses E1 with no extra provider
    /// create/dispose; E3=C1+C4 publishes and caches nothing; E4=the next call
    /// after E3 retries physical create/dispose; E5=C5 owns a fresh cache and
    /// probes once. Decision rules covered here: R1(C1,C2), R2(C1,C2,C3),
    /// R3(C5,C2). The failure rules R4-R5 are covered by the following test.
    #[tokio::test]
    async fn successful_batch_is_single_flight_reused_and_incarnation_scoped() {
        let probe = Arc::new(CountingLifecycleProbe::default());
        let source = ConfiguredAcpCapabilityObservationSource::new(
            ["second", "first"]
                .into_iter()
                .map(|id| configured_target(id, "probe"))
                .collect(),
            probe.clone(),
            PathBuf::from("/workspace"),
        );

        let (first, joined) = tokio::join!(
            source.capability_observations(),
            source.capability_observations(),
        );
        let first = first.unwrap();
        assert_eq!(joined.unwrap(), first, "R2");
        assert_eq!(source.capability_observations().await.unwrap(), first, "R2");
        assert_eq!(probe.creates.load(Ordering::SeqCst), 2, "R1-R2");
        assert_eq!(probe.disposes.load(Ordering::SeqCst), 2, "R1-R2");

        let rebuilt = ConfiguredAcpCapabilityObservationSource::new(
            ["second", "first"]
                .into_iter()
                .map(|id| configured_target(id, "probe"))
                .collect(),
            probe.clone(),
            PathBuf::from("/workspace"),
        );
        rebuilt.capability_observations().await.unwrap();
        assert_eq!(probe.creates.load(Ordering::SeqCst), 4, "R3");
        assert_eq!(probe.disposes.load(Ordering::SeqCst), 4, "R3");
    }

    /// R4(C1,C4)->E3 and R5(C1,C4,then C2)->E4: a failed batch,
    /// including any partial observations completed before its error, never
    /// initializes the success cell. The next refresh performs one new provider
    /// create/dispose; after that complete success, later refreshes reuse it.
    #[tokio::test]
    async fn failed_batch_is_not_cached_and_complete_retry_is_reused() {
        let probe = Arc::new(CountingLifecycleProbe {
            failures_left: AtomicUsize::new(1),
            ..Default::default()
        });
        let source = ConfiguredAcpCapabilityObservationSource::new(
            vec![configured_target("retry", "probe")],
            probe.clone(),
            PathBuf::from("/workspace"),
        );

        assert!(source.capability_observations().await.is_err(), "R4");
        let retried = source.capability_observations().await.unwrap();
        assert_eq!(
            source.capability_observations().await.unwrap(),
            retried,
            "R5"
        );
        assert_eq!(probe.creates.load(Ordering::SeqCst), 2, "R4-R5");
        assert_eq!(probe.disposes.load(Ordering::SeqCst), 2, "R4-R5");
    }
}
