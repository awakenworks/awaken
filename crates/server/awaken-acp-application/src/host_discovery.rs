//! Data-driven discovery of ACP agents installed on the trusted local host.
//!
//! The [`AcpCli`] catalog owns every command and classification
//! rule. This module owns the generic process mechanism and produces secret-free
//! observations; it never opens a provider's authentication files.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_run_executor_acp::{AcpCli, AcpProbeCommand, AcpProbePredicate, known_acp_clis};
use awaken_runtime_contract::CredentialObservationState;
use tokio::process::Command;

use crate::AcpCapabilityState;

/// Raw, bounded output from the process-probe port.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AcpProbeOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl AcpProbeOutput {
    fn success(&self) -> bool {
        self.exit_code == Some(0)
    }

    fn matches(&self, predicate: AcpProbePredicate) -> bool {
        match predicate {
            AcpProbePredicate::ExitSuccess => self.success(),
            AcpProbePredicate::ExitCode(code) => self.exit_code == Some(code),
            AcpProbePredicate::CombinedOutputContains(needle) => {
                let needle = needle.to_ascii_lowercase();
                self.stdout.to_ascii_lowercase().contains(&needle)
                    || self.stderr.to_ascii_lowercase().contains(&needle)
            }
            AcpProbePredicate::StdoutJsonBoolean { field, value } => {
                json_boolean_field(&self.stdout, field) == Some(value)
            }
        }
    }

    fn version(&self) -> Option<String> {
        self.stdout
            .lines()
            .chain(self.stderr.lines())
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_string)
    }
}

/// Read one top-level boolean from a probe's JSON object without introducing a
/// general JSON/value dependency into the Worker application boundary.
///
/// Probe descriptors name the exact field and accept only a literal boolean.
/// Strings and nested objects cannot accidentally satisfy the predicate.
fn json_boolean_field(input: &str, field: &str) -> Option<bool> {
    let bytes = input.as_bytes();
    let mut depth = 0_u32;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => {
                depth = depth.checked_add(1)?;
                index += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                index += 1;
            }
            b'"' => {
                let (value, next) = json_string(input, index)?;
                index = next;
                if depth != 1 || value != field {
                    continue;
                }
                index = skip_ascii_whitespace(bytes, index);
                if bytes.get(index) != Some(&b':') {
                    continue;
                }
                index = skip_ascii_whitespace(bytes, index + 1);
                if bytes.get(index..index + 4) == Some(b"true") {
                    return Some(true);
                }
                if bytes.get(index..index + 5) == Some(b"false") {
                    return Some(false);
                }
            }
            _ => index += 1,
        }
    }
    None
}

fn json_string(input: &str, quote: usize) -> Option<(&str, usize)> {
    let bytes = input.as_bytes();
    let start = quote + 1;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = index.checked_add(2)?,
            b'"' => return Some((&input[start..index], index + 1)),
            _ => index += 1,
        }
    }
    None
}

fn skip_ascii_whitespace(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        index += 1;
    }
    index
}

/// Failure of the host process mechanism, before profile classification.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AcpProcessProbeError {
    ExecutableNotFound { executable: String },
    TimedOut { executable: String },
    SpawnFailed { executable: String },
}

impl std::fmt::Display for AcpProcessProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, executable) = match self {
            Self::ExecutableNotFound { executable } => ("executable not found", executable),
            Self::TimedOut { executable } => ("probe timed out", executable),
            Self::SpawnFailed { executable } => ("probe spawn failed", executable),
        };
        write!(formatter, "{kind}: {executable}")
    }
}

impl std::error::Error for AcpProcessProbeError {}

/// Process I/O port for host discovery. Tests and alternative local runtimes
/// implement this port without changing classification behavior.
#[async_trait]
trait AcpProcessProbe: Send + Sync {
    async fn run(&self, command: AcpProbeCommand) -> Result<AcpProbeOutput, AcpProcessProbeError>;
}

/// Tokio adapter for the real local host. It uses the same PATH/HOME-only
/// context as trusted local launch, so a probe cannot bless credentials the
/// eventual ACP process would not receive.
struct TokioAcpProcessProbe {
    timeout: Duration,
}

impl TokioAcpProcessProbe {
    #[must_use]
    fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

#[async_trait]
impl AcpProcessProbe for TokioAcpProcessProbe {
    async fn run(&self, probe: AcpProbeCommand) -> Result<AcpProbeOutput, AcpProcessProbeError> {
        let mut command = Command::new(probe.executable);
        command
            .args(probe.args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| AcpProcessProbeError::TimedOut {
                executable: probe.executable.to_string(),
            })?
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    AcpProcessProbeError::ExecutableNotFound {
                        executable: probe.executable.to_string(),
                    }
                } else {
                    AcpProcessProbeError::SpawnFailed {
                        executable: probe.executable.to_string(),
                    }
                }
            })?;
        Ok(AcpProbeOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Whether a supported catalog row is launchable on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpDetectionState {
    Detected,
    Missing,
    ProbeFailed,
}

/// Secret-free join input for Worker capability publication and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpHostObservation {
    pub cli_id: String,
    pub display_name: String,
    pub detection: AcpDetectionState,
    pub version: Option<String>,
    pub credential_state: Option<CredentialObservationState>,
    pub reason_code: Option<String>,
    pub capability_state: Option<AcpCapabilityState>,
    pub capability_fingerprint: Option<String>,
    pub capability_reason_code: Option<String>,
}

impl AcpHostObservation {
    #[must_use]
    pub fn detected(&self) -> bool {
        self.detection == AcpDetectionState::Detected
    }
}

/// Generic domain service over the catalog and process port.
pub struct AcpHostDiscovery {
    process: Arc<dyn AcpProcessProbe>,
}

/// Secret-free discovery port shared by Worker liveness and diagnostics.
#[async_trait]
pub trait AcpDiscovery: Send + Sync {
    async fn discover(&self, cli: &AcpCli) -> AcpHostObservation;

    async fn discover_all(&self) -> Vec<AcpHostObservation> {
        let mut observations = Vec::with_capacity(known_acp_clis().len());
        for cli in known_acp_clis() {
            observations.push(self.discover(cli).await);
        }
        observations
    }
}

impl AcpHostDiscovery {
    /// Discover against the real trusted host with a per-command timeout.
    #[must_use]
    pub fn local(timeout: Duration) -> Self {
        Self {
            process: Arc::new(TokioAcpProcessProbe::new(timeout)),
        }
    }

    #[cfg(test)]
    fn with_process(process: Arc<dyn AcpProcessProbe>) -> Self {
        Self { process }
    }
}

#[async_trait]
impl AcpDiscovery for AcpHostDiscovery {
    async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
        let version = match self.process.run(cli.discovery.version).await {
            Err(AcpProcessProbeError::ExecutableNotFound { .. }) => {
                return observation(cli, AcpDetectionState::Missing, "acp_agent_missing");
            }
            Err(_) => {
                return observation(
                    cli,
                    AcpDetectionState::ProbeFailed,
                    "acp_version_probe_failed",
                );
            }
            Ok(output) if !output.success() => {
                return observation(
                    cli,
                    AcpDetectionState::ProbeFailed,
                    "acp_version_probe_failed",
                );
            }
            Ok(output) => output.version(),
        };

        let login_output = match self.process.run(cli.discovery.login.command).await {
            Ok(output) => output,
            Err(_) => {
                return AcpHostObservation {
                    cli_id: cli.id.to_string(),
                    display_name: cli.display_name.to_string(),
                    detection: AcpDetectionState::Detected,
                    version,
                    credential_state: Some(CredentialObservationState::ProbeFailed),
                    reason_code: Some("acp_login_probe_failed".to_string()),
                    capability_state: None,
                    capability_fingerprint: None,
                    capability_reason_code: None,
                };
            }
        };
        let classified = cli
            .discovery
            .login
            .rules
            .iter()
            .find(|rule| login_output.matches(rule.predicate));
        AcpHostObservation {
            cli_id: cli.id.to_string(),
            display_name: cli.display_name.to_string(),
            detection: AcpDetectionState::Detected,
            version,
            credential_state: Some(
                classified
                    .map(|rule| rule.state)
                    .unwrap_or(CredentialObservationState::ProbeFailed),
            ),
            reason_code: Some(
                classified
                    .map(|rule| rule.reason_code)
                    .unwrap_or("acp_login_probe_unrecognized")
                    .to_string(),
            ),
            capability_state: None,
            capability_fingerprint: None,
            capability_reason_code: None,
        }
    }
}

fn observation(
    cli: &AcpCli,
    detection: AcpDetectionState,
    reason_code: &str,
) -> AcpHostObservation {
    AcpHostObservation {
        cli_id: cli.id.to_string(),
        display_name: cli.display_name.to_string(),
        detection,
        version: None,
        credential_state: None,
        reason_code: Some(reason_code.to_string()),
        capability_state: None,
        capability_fingerprint: None,
        capability_reason_code: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;
    use awaken_run_executor_acp::acp_cli;

    #[derive(Default)]
    struct ScriptedProbe {
        outputs: Mutex<BTreeMap<Vec<String>, Result<AcpProbeOutput, AcpProcessProbeError>>>,
    }

    impl ScriptedProbe {
        fn with(self, argv: &[&str], output: Result<AcpProbeOutput, AcpProcessProbeError>) -> Self {
            self.outputs
                .lock()
                .unwrap()
                .insert(argv.iter().map(ToString::to_string).collect(), output);
            self
        }
    }

    #[async_trait]
    impl AcpProcessProbe for ScriptedProbe {
        async fn run(
            &self,
            command: AcpProbeCommand,
        ) -> Result<AcpProbeOutput, AcpProcessProbeError> {
            let argv: Vec<String> = std::iter::once(command.executable)
                .chain(command.args.iter().copied())
                .map(ToString::to_string)
                .collect();
            self.outputs
                .lock()
                .unwrap()
                .get(&argv)
                .cloned()
                .unwrap_or_else(|| {
                    Err(AcpProcessProbeError::ExecutableNotFound {
                        executable: command.executable.to_string(),
                    })
                })
        }
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> AcpProbeOutput {
        AcpProbeOutput {
            exit_code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn json_boolean_probe_matches_only_the_named_top_level_literal() {
        // Cause graph:
        // C1 named top-level key + boolean literal -> E1 exact boolean.
        // C2 whitespace/order -> E2 same result.
        // C3 string/nested/other key/malformed input -> E3 no result.
        //
        // Decision table:
        // J1 top-level true          -> Some(true)
        // J2 top-level false/spaces  -> Some(false)
        // J3 value inside a string   -> None
        // J4 value in nested object  -> None
        // J5 malformed/non-boolean   -> None
        assert_eq!(
            json_boolean_field(r#"{"loggedIn":true,"other":false}"#, "loggedIn"),
            Some(true),
            "J1"
        );
        assert_eq!(
            json_boolean_field("{ \"other\": true, \"loggedIn\" : false }", "loggedIn"),
            Some(false),
            "J2"
        );
        assert_eq!(
            json_boolean_field(r#"{"message":"\"loggedIn\":true"}"#, "loggedIn"),
            None,
            "J3"
        );
        assert_eq!(
            json_boolean_field(r#"{"nested":{"loggedIn":true}}"#, "loggedIn"),
            None,
            "J4"
        );
        for input in [
            r#"{"loggedIn":"true"}"#,
            r#"{"loggedIn":null}"#,
            r#"{"loggedIn":tru}"#,
            "not-json",
        ] {
            assert_eq!(json_boolean_field(input, "loggedIn"), None, "J5 {input}");
        }
    }

    #[tokio::test]
    async fn profile_rules_classify_login_without_cli_id_branches() {
        // Cause graph:
        // CLI/version evidence ──> Detected ──> ordered profile rules
        // missing evidence       ──> Missing       ├─ match -> exact state
        // broken evidence        ──> ProbeFailed  └─ none  -> ProbeFailed
        //
        // Decision table (representative catalog mechanisms):
        // D1 Codex marker      -> Available
        // D2 Claude JSON false -> LoginRequired
        // D3 Gemini exit 41    -> LoginRequired
        // D4 OpenCode 0 creds  -> LoginRequired
        let probe = ScriptedProbe::default()
            .with(&["codex", "--version"], Ok(output(0, "codex 1", "")))
            .with(
                &["codex", "login", "status"],
                Ok(output(0, "Logged in using ChatGPT", "")),
            )
            .with(&["claude", "--version"], Ok(output(0, "claude 2", "")))
            .with(
                &["claude", "auth", "status", "--json"],
                Ok(output(1, r#"{"loggedIn":false}"#, "")),
            )
            .with(&["gemini", "--version"], Ok(output(0, "3", "")))
            .with(&["gemini", "--list-sessions"], Ok(output(41, "", "auth")))
            .with(&["opencode", "--version"], Ok(output(0, "4", "")))
            .with(
                &["opencode", "auth", "list"],
                Ok(output(0, "0 credentials", "")),
            );
        let observations = AcpHostDiscovery::with_process(Arc::new(probe))
            .discover_all()
            .await;
        let state = |id: &str| {
            observations
                .iter()
                .find(|observation| observation.cli_id == id)
                .and_then(|observation| observation.credential_state)
        };
        assert_eq!(state("codex"), Some(CredentialObservationState::Available));
        for id in ["claude", "gemini", "opencode"] {
            assert_eq!(
                state(id),
                Some(CredentialObservationState::LoginRequired),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn missing_broken_and_unrecognized_evidence_fail_closed() {
        // Decision table:
        // D1 missing CLI          -> Missing/no credential state
        // D2 nonzero version      -> ProbeFailed/no credential state
        // D3 unrecognized login   -> Detected/ProbeFailed
        let missing = AcpHostDiscovery::with_process(Arc::new(ScriptedProbe::default()))
            .discover(acp_cli("codex").unwrap())
            .await;
        assert_eq!(missing.detection, AcpDetectionState::Missing, "D1");
        assert_eq!(missing.credential_state, None, "D1");

        let broken_probe =
            ScriptedProbe::default().with(&["gemini", "--version"], Ok(output(2, "", "broken")));
        let broken = AcpHostDiscovery::with_process(Arc::new(broken_probe))
            .discover(acp_cli("gemini").unwrap())
            .await;
        assert_eq!(broken.detection, AcpDetectionState::ProbeFailed, "D2");
        assert_eq!(broken.credential_state, None, "D2");

        let unknown_probe = ScriptedProbe::default()
            .with(&["gemini", "--version"], Ok(output(0, "gemini 3", "")))
            .with(
                &["gemini", "--list-sessions"],
                Ok(output(9, "", "unexpected")),
            );
        let unknown = AcpHostDiscovery::with_process(Arc::new(unknown_probe))
            .discover(acp_cli("gemini").unwrap())
            .await;
        assert_eq!(unknown.detection, AcpDetectionState::Detected, "D3");
        assert_eq!(
            unknown.credential_state,
            Some(CredentialObservationState::ProbeFailed),
            "D3"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_process_probe_is_bounded_and_uses_the_launch_environment() {
        // Mechanism decision table:
        // D1 exits in time -> bounded stdout/status
        // D2 missing       -> ExecutableNotFound
        // D3 exceeds bound -> TimedOut
        // The shell also proves Cargo's ambient env is cleared while HOME remains.
        // Successful process startup is not a timing assertion. Give the host a
        // production-like budget so scheduler/load variance cannot turn valid
        // environment evidence into a false timeout.
        let ordinary_probe = TokioAcpProcessProbe::new(Duration::from_secs(2));
        let output = ordinary_probe
            .run(AcpProbeCommand {
                executable: "/bin/sh",
                args: &["-c", "test -n \"$HOME\" && test -n \"$PATH\" && env"],
            })
            .await
            .expect("D1");
        assert_eq!(output.exit_code, Some(0), "D1");
        assert!(output.stdout.contains("HOME="), "D1");
        assert!(output.stdout.contains("PATH="), "D1");
        assert!(!output.stdout.contains("CARGO_"), "D1");

        assert!(matches!(
            ordinary_probe
                .run(AcpProbeCommand {
                    executable: "/definitely/missing/awaken-acp-probe",
                    args: &[],
                })
                .await,
            Err(AcpProcessProbeError::ExecutableNotFound { .. })
        ));
        // Timeout behavior is a separate deterministic case: `exec` makes the
        // shell and sleeper one kill-on-drop process instead of leaving a child
        // that can retain the captured output pipes.
        let bounded_probe = TokioAcpProcessProbe::new(Duration::from_millis(25));
        assert!(matches!(
            bounded_probe
                .run(AcpProbeCommand {
                    executable: "/bin/sh",
                    args: &["-c", "exec sleep 10"],
                })
                .await,
            Err(AcpProcessProbeError::TimedOut { .. })
        ));
    }
}
