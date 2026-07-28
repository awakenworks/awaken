//! Bounded Worker-side ACP capability negotiation and evidence fingerprinting.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use awaken_acp_contract::{
    AcpCapabilityHandshake, AcpCapabilityProbeConfig, NegotiatedAcpCapabilities,
};
use awaken_agent_channel::{AgentChannel, SplitChannel};
use sha2::{Digest, Sha256};

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
        let fingerprint = capability_fingerprint(&cli_id, &cli_version, &negotiated);
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

#[async_trait]
pub trait AcpCapabilityNegotiator: Send + Sync {
    async fn negotiate(
        &self,
        argv: &[String],
        cwd: &Path,
        auth_method_id: Option<&str>,
    ) -> Result<NegotiatedAcpCapabilities, String>;
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
        let mut command = tokio::process::Command::new(program);
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

fn capability_fingerprint(
    cli_id: &str,
    cli_version: &str,
    capabilities: &NegotiatedAcpCapabilities,
) -> String {
    let mut hash = Sha256::new();
    hash_value(&mut hash, cli_id);
    hash_value(&mut hash, cli_version);
    hash_value(&mut hash, &capabilities.protocol_version);
    for flag in [
        capabilities.load_session,
        capabilities.prompt_image,
        capabilities.prompt_audio,
        capabilities.prompt_embedded_context,
        capabilities.mcp_http,
        capabilities.mcp_sse,
        capabilities.session_list,
    ] {
        hash.update([u8::from(flag)]);
    }
    let mut modes = capabilities.modes.iter().collect::<Vec<_>>();
    modes.sort_by_key(|mode| &mode.native_id);
    for mode in modes {
        hash_value(&mut hash, &mode.native_id);
        hash_value(&mut hash, &mode.name);
        hash_optional(&mut hash, mode.description.as_deref());
        hash.update([u8::from(mode.current)]);
    }
    let mut options = capabilities.config_options.iter().collect::<Vec<_>>();
    options.sort_by_key(|option| &option.native_id);
    for option in options {
        hash_value(&mut hash, &option.native_id);
        hash_value(&mut hash, &option.name);
        hash_optional(&mut hash, option.description.as_deref());
        hash_optional(&mut hash, option.category.as_deref());
        hash_value(&mut hash, &option.current_value);
        let mut choices = option.choices.iter().collect::<Vec<_>>();
        choices.sort_by_key(|choice| (&choice.group_id, &choice.native_value));
        for choice in choices {
            hash_value(&mut hash, &choice.native_value);
            hash_value(&mut hash, &choice.name);
            hash_optional(&mut hash, choice.description.as_deref());
            hash_optional(&mut hash, choice.group_id.as_deref());
            hash_optional(&mut hash, choice.group_name.as_deref());
        }
    }
    format!("{:x}", hash.finalize())
}

fn hash_value(hash: &mut Sha256, value: &str) {
    hash.update(value.len().to_le_bytes());
    hash.update(value.as_bytes());
}

fn hash_optional(hash: &mut Sha256, value: Option<&str>) {
    hash.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        hash_value(hash, value);
    }
}

#[cfg(test)]
mod tests {
    use awaken_acp_contract::{
        AcpSessionConfigChoice, AcpSessionConfigOptionDescriptor, AcpSessionModeDescriptor,
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
        // C2 adapter/version/schema/current value changes -> E2 different hash.
        //
        // Decision table:
        // F1 reorder modes/choices -> same fingerprint
        // F2 change current value  -> different fingerprint
        // F3 change CLI version    -> different fingerprint
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
        assert_ne!(
            fingerprint,
            capability_fingerprint("codex", "1.0", &changed),
            "F2"
        );
        assert_ne!(
            fingerprint,
            capability_fingerprint("codex", "2.0", &original),
            "F3"
        );
    }
}
