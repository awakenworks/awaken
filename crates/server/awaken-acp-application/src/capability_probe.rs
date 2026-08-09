//! Bounded Worker-side ACP capability negotiation and evidence fingerprinting.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_acp_contract::{
    AcpCapabilityHandshake, AcpCapabilityNegotiator, AcpCapabilityObservation,
    AcpCapabilityObservationSource, AcpCapabilityObservationState, AcpCapabilityProbeConfig,
    NegotiatedAcpCapabilities,
};
use awaken_agent_channel::{AgentChannel, SplitChannel};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpCapabilityState {
    Verified,
    Unavailable,
    ProbeFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAcpCapabilityProfile {
    pub cli_id: String,
    pub cli_version: String,
    pub observed_at_unix_ms: u64,
    pub fingerprint: String,
    pub negotiated: NegotiatedAcpCapabilities,
}

impl EffectiveAcpCapabilityProfile {
    #[must_use]
    pub fn verified(
        cli_id: impl Into<String>,
        cli_version: impl Into<String>,
        negotiated: NegotiatedAcpCapabilities,
    ) -> Self {
        let cli_id = cli_id.into();
        let cli_version = cli_version.into();
        let fingerprint =
            awaken_acp_contract::capability_fingerprint(&cli_id, &cli_version, &negotiated);
        let observed_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        Self {
            cli_id,
            cli_version,
            observed_at_unix_ms,
            fingerprint,
            negotiated,
        }
    }
}

/// Production trusted-host capability probe. It inherits only PATH/HOME, sends
/// no prompt and kills/waits for the short-lived adapter after negotiation.
pub struct HostAcpCapabilityNegotiator {
    timeout: Duration,
    handshake: std::sync::Arc<dyn AcpCapabilityHandshake>,
}

impl HostAcpCapabilityNegotiator {
    #[must_use]
    pub fn new(timeout: Duration, handshake: std::sync::Arc<dyn AcpCapabilityHandshake>) -> Self {
        Self { timeout, handshake }
    }
}

fn executable_on_path(program: &str) -> PathBuf {
    let requested = Path::new(program);
    if requested.components().count() > 1 {
        return requested.to_path_buf();
    }
    let extensions: Vec<String> = if cfg!(windows) {
        let mut extensions = vec![String::new()];
        extensions.extend(
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
                .split(';')
                .map(str::to_string),
        );
        extensions
    } else {
        vec![String::new()]
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .flat_map(|directory| {
            extensions.iter().map(move |extension| {
                directory.join(if extension.is_empty() {
                    program.to_string()
                } else {
                    format!("{program}{extension}")
                })
            })
        })
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| requested.to_path_buf())
}

#[async_trait]
impl AcpCapabilityNegotiator for HostAcpCapabilityNegotiator {
    async fn negotiate(
        &self,
        argv: &[String],
        cwd: &Path,
        auth_method_id: Option<&str>,
    ) -> Result<NegotiatedAcpCapabilities, String> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| "ACP capability probe requires a non-empty argv".to_string())?;
        let executable = executable_on_path(program);
        let mut command = tokio::process::Command::new(&executable);
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn ACP capability probe `{program}`: {error}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "ACP capability probe has no stdout".to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "ACP capability probe has no stdin".to_string())?;
        let mut channel: Box<dyn AgentChannel> = Box::new(SplitChannel::new(stdout, stdin));
        let config = AcpCapabilityProbeConfig {
            session_cwd: Some(cwd.to_string_lossy().into_owned()),
            auth_method_id: auth_method_id.map(str::to_string),
        };
        let negotiated = tokio::time::timeout(
            self.timeout,
            self.handshake.negotiate(channel.as_mut(), &config),
        )
        .await
        .map_err(|_| format!("ACP capability probe `{program}` timed out"))
        .and_then(|result| result);
        drop(channel);
        let _ = child.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        negotiated
    }
}

/// One configured ACP executable whose installation boundary is an image/Pod
/// rather than the Worker's host filesystem. The adapter version is the exact
/// configured image identity; the live handshake supplies the capability facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredAcpCapabilityTarget {
    pub cli_id: String,
    pub adapter_version: String,
    pub argv: Vec<String>,
    pub auth_method_id: Option<String>,
}

impl ConfiguredAcpCapabilityTarget {
    pub fn new(
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

/// Capability observation owner for configured image/Pod ACP adapters. It uses
/// the same neutral negotiator as host discovery and publishes only live,
/// coherent evidence; a failed probe replaces no fact with a static declaration.
pub struct ConfiguredAcpCapabilityObservationSource {
    targets: Vec<ConfiguredAcpCapabilityTarget>,
    negotiator: std::sync::Arc<dyn AcpCapabilityNegotiator>,
    cwd: PathBuf,
}

impl ConfiguredAcpCapabilityObservationSource {
    #[must_use]
    pub fn new(
        mut targets: Vec<ConfiguredAcpCapabilityTarget>,
        negotiator: std::sync::Arc<dyn AcpCapabilityNegotiator>,
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
        // Targets are independent image-local handshakes. Running them as one
        // bounded concurrent batch prevents N adapters from consuming N times
        // the per-probe timeout and creating a freshness gap in the shared
        // Worker observation lease. Results are sorted back into catalog order.
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
                    Ok(negotiated) => {
                        let profile = EffectiveAcpCapabilityProfile::verified(
                            &target.cli_id,
                            &target.adapter_version,
                            negotiated,
                        );
                        AcpCapabilityObservation {
                            backend_ref: format!("acp:{}", target.cli_id),
                            adapter_version: profile.cli_version,
                            state: AcpCapabilityObservationState::Verified,
                            observed_at_ms,
                            fingerprint: Some(profile.fingerprint),
                            negotiated: Some(profile.negotiated),
                            reason_code: None,
                        }
                    }
                    Err(_) => AcpCapabilityObservation {
                        backend_ref: format!("acp:{}", target.cli_id),
                        adapter_version: target.adapter_version,
                        state: AcpCapabilityObservationState::ProbeFailed,
                        observed_at_ms,
                        fingerprint: None,
                        negotiated: None,
                        reason_code: Some("acp_capability_probe_failed".into()),
                    },
                };
                (index, observation)
            });
        }
        let mut observations = Vec::with_capacity(self.targets.len());
        while let Some(result) = probes.join_next().await {
            observations.push(result.map_err(|error| error.to_string())?);
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
    use awaken_acp_contract::{
        AcpSessionConfigChoice, AcpSessionConfigOptionDescriptor, AcpSessionModeDescriptor,
        capability_fingerprint,
    };

    use super::*;

    fn capabilities() -> NegotiatedAcpCapabilities {
        NegotiatedAcpCapabilities {
            protocol_version: "1".into(),
            load_session: true,
            prompt_image: false,
            prompt_audio: false,
            prompt_embedded_context: true,
            mcp_http: true,
            mcp_sse: false,
            session_list: false,
            modes: vec![
                AcpSessionModeDescriptor {
                    native_id: "plan".into(),
                    name: "Plan".into(),
                    description: None,
                    current: false,
                },
                AcpSessionModeDescriptor {
                    native_id: "code".into(),
                    name: "Code".into(),
                    description: None,
                    current: true,
                },
            ],
            config_options: vec![AcpSessionConfigOptionDescriptor {
                native_id: "reasoning".into(),
                name: "Reasoning".into(),
                description: None,
                category: Some("thought_level".into()),
                current_value: "high".into(),
                choices: vec![
                    AcpSessionConfigChoice {
                        native_value: "high".into(),
                        name: "High".into(),
                        description: None,
                        group_id: None,
                        group_name: None,
                    },
                    AcpSessionConfigChoice {
                        native_value: "low".into(),
                        name: "Low".into(),
                        description: None,
                        group_id: None,
                        group_name: None,
                    },
                ],
            }],
        }
    }

    struct ConfiguredProbeFake;

    #[async_trait]
    impl AcpCapabilityNegotiator for ConfiguredProbeFake {
        async fn negotiate(
            &self,
            argv: &[String],
            _cwd: &Path,
            _auth_method_id: Option<&str>,
        ) -> Result<NegotiatedAcpCapabilities, String> {
            if argv.first().is_some_and(|program| program == "verified") {
                Ok(capabilities())
            } else {
                Err("injected probe failure".into())
            }
        }
    }

    struct ConcurrentProbeFake {
        barrier: std::sync::Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl AcpCapabilityNegotiator for ConcurrentProbeFake {
        async fn negotiate(
            &self,
            _argv: &[String],
            _cwd: &Path,
            _auth_method_id: Option<&str>,
        ) -> Result<NegotiatedAcpCapabilities, String> {
            self.barrier.wait().await;
            Ok(capabilities())
        }
    }

    #[tokio::test]
    async fn configured_targets_are_probed_as_one_concurrent_batch() {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let targets = ["first", "second"]
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
            .collect();
        let source = ConfiguredAcpCapabilityObservationSource::new(
            targets,
            std::sync::Arc::new(ConcurrentProbeFake { barrier }),
            PathBuf::from("/workspace"),
        );
        let observations =
            tokio::time::timeout(Duration::from_secs(1), source.capability_observations())
                .await
                .expect("independent probes must overlap")
                .unwrap();
        assert_eq!(observations.len(), 2);
        assert!(
            observations.iter().all(|observation| {
                observation.state == AcpCapabilityObservationState::Verified
            })
        );
    }

    #[tokio::test]
    async fn configured_capability_observation_decision_table() {
        // Cause/effect graph: C1 target identity/argv complete; C2 live
        // negotiation succeeds. Effects: E1 incomplete configuration is
        // rejected before observation; E2 success publishes one coherent
        // Verified fact and fingerprint; E3 failure publishes ProbeFailed with
        // no negotiated/fingerprint residue. Target ordering is deterministic.
        //
        // | Rule | target | handshake | effect |
        // | T1 | invalid | n/a | constructor rejects |
        // | T2 | valid | success | Verified + coherent evidence |
        // | T3 | valid | failure | ProbeFailed + no evidence |
        assert!(
            ConfiguredAcpCapabilityTarget::new("", "image:v1", vec!["ok".into()], None).is_err(),
            "T1"
        );
        let failed = ConfiguredAcpCapabilityTarget::new(
            "z-failed",
            "image:sha-failed",
            vec!["failed".into()],
            None,
        )
        .expect("T3 target");
        let verified = ConfiguredAcpCapabilityTarget::new(
            "a-verified",
            "image:sha-verified",
            vec!["verified".into()],
            Some("login".into()),
        )
        .expect("T2 target");
        let source = ConfiguredAcpCapabilityObservationSource::new(
            vec![failed, verified],
            std::sync::Arc::new(ConfiguredProbeFake),
            PathBuf::from("/workspace"),
        );

        let observations = source.capability_observations().await.expect("observe");
        assert_eq!(observations.len(), 2);
        let verified = &observations[0];
        assert_eq!(verified.backend_ref, "acp:a-verified", "T2 sorted");
        assert_eq!(
            verified.state,
            AcpCapabilityObservationState::Verified,
            "T2"
        );
        assert_eq!(verified.adapter_version, "image:sha-verified", "T2");
        assert_eq!(verified.negotiated.as_ref(), Some(&capabilities()), "T2");
        let expected_fingerprint =
            capability_fingerprint("a-verified", "image:sha-verified", &capabilities());
        assert_eq!(
            verified.fingerprint.as_deref(),
            Some(expected_fingerprint.as_str()),
            "T2 coherent fingerprint"
        );
        assert!(verified.reason_code.is_none(), "T2");

        let failed = &observations[1];
        assert_eq!(failed.backend_ref, "acp:z-failed", "T3 sorted");
        assert_eq!(
            failed.state,
            AcpCapabilityObservationState::ProbeFailed,
            "T3"
        );
        assert!(failed.fingerprint.is_none(), "T3");
        assert!(failed.negotiated.is_none(), "T3");
        assert_eq!(
            failed.reason_code.as_deref(),
            Some("acp_capability_probe_failed"),
            "T3"
        );
    }

    #[test]
    fn fingerprint_is_order_independent_but_changes_with_effective_evidence() {
        // Cause graph:
        // C1 identical semantic evidence in another wire order -> E1 same hash.
        // C2 effective protocol evidence changes -> E2 different hash.
        //
        // Decision table:
        // F1 reorder modes/choices -> same fingerprint
        // F2 change route-derived current value -> same fingerprint
        // F3 change protocol version -> different fingerprint
        // F4 change CLI version      -> different fingerprint
        let original = capabilities();
        let fingerprint = capability_fingerprint("codex", "1.0", &original);

        let mut reordered = original.clone();
        reordered.modes.reverse();
        reordered.config_options[0].choices.reverse();
        assert_eq!(
            fingerprint,
            capability_fingerprint("codex", "1.0", &reordered),
            "F1"
        );

        let mut changed = original.clone();
        changed.config_options[0].current_value = "low".into();
        assert_eq!(
            fingerprint,
            capability_fingerprint("codex", "1.0", &changed),
            "F2"
        );
        changed.protocol_version = "2".into();
        assert_ne!(
            fingerprint,
            capability_fingerprint("codex", "1.0", &changed),
            "F3"
        );
        assert_ne!(
            fingerprint,
            capability_fingerprint("codex", "2.0", &original),
            "F4"
        );
    }
}
