//! Bounded Worker-side ACP capability negotiation and evidence fingerprinting.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_acp_contract::{
    AcpCapabilityHandshake, AcpCapabilityNegotiator, AcpCapabilityProbeConfig,
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
