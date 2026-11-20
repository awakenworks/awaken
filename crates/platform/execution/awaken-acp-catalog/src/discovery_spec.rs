//! Declarative host-observation commands attached to the ACP catalog.
//!
//! These are inert catalog values. The Worker application owns process I/O,
//! classification, refresh and observation publication.

use awaken_credential_contract::CredentialObservationState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpProbeCommand {
    pub executable: &'static str,
    pub args: &'static [&'static str],
}

/// Minimum adapter/CLI version whose ACP behavior is covered by one catalog
/// row. Newer versions remain admissible for forward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AcpVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl AcpVersion {
    #[must_use]
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpProbePredicate {
    ExitSuccess,
    ExitCode(i32),
    CombinedOutputContains(&'static str),
    StdoutJsonBoolean { field: &'static str, value: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpLoginRule {
    pub predicate: AcpProbePredicate,
    pub state: CredentialObservationState,
    pub reason_code: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpLoginProbe {
    pub command: AcpProbeCommand,
    pub rules: &'static [AcpLoginRule],
    pub remediation: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpDiscoverySpec {
    /// Optional npm installation of the host CLI, distinct from its ACP adapter.
    pub npm_package: Option<&'static str>,
    pub version: AcpProbeCommand,
    pub minimum_version: AcpVersion,
    pub login: AcpLoginProbe,
    pub install_remediation: &'static str,
}

/// Extract the first `major.minor.patch` triple from vendor-decorated version
/// output such as `codex-cli 0.146.0` or `Hermes Agent v0.19.0 (...)`.
pub fn parse_version(input: &str) -> Option<AcpVersion> {
    let bytes = input.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_digit() || start > 0 && bytes[start - 1].is_ascii_digit() {
            start += 1;
            continue;
        }
        let Some((major, first_dot)) = parse_number(bytes, start) else {
            start += 1;
            continue;
        };
        if bytes.get(first_dot) != Some(&b'.') {
            start += 1;
            continue;
        }
        let Some((minor, second_dot)) = parse_number(bytes, first_dot + 1) else {
            start += 1;
            continue;
        };
        if bytes.get(second_dot) != Some(&b'.') {
            start += 1;
            continue;
        }
        let Some((patch, _end)) = parse_number(bytes, second_dot + 1) else {
            start += 1;
            continue;
        };
        return Some(AcpVersion::new(major, minor, patch));
    }
    None
}

fn parse_number(bytes: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut end = start;
    let mut value = 0_u64;
    while let Some(byte) = bytes.get(end).copied().filter(u8::is_ascii_digit) {
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
        end += 1;
    }
    (end > start).then_some((value, end))
}
