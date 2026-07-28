//! Declarative host-observation commands attached to the ACP catalog.
//!
//! These are inert catalog values. The Worker application owns process I/O,
//! classification, refresh and observation publication.

use awaken_runtime_contract::CredentialObservationState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpProbeCommand {
    pub executable: &'static str,
    pub args: &'static [&'static str],
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
    pub version: AcpProbeCommand,
    pub login: AcpLoginProbe,
    pub install_remediation: &'static str,
}
