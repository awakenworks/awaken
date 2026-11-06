//! The realization's evidence for a Managed Rebuild.
//!
//! A Rebuild replaces a Resident binding whose substrate died. The Session
//! aggregate decides whether it may (`SessionEnvironmentFacts::source_closed`
//! and `source_access`); this module derives those two facts once, from what
//! the realization physically observed, and nothing else decides them.
//!
//! Derivation table (cause → effect):
//!
//! | Rule | Observation | Source domain | Bound incarnation end | Closed |
//! |---|---|---|---|---|
//! | RS1 | Terminal / DefinitivelyUnavailable | Provider | any | yes |
//! | RS2 | Terminal / DefinitivelyUnavailable | Worker process | Supervised / ExactExit / Superseded | yes |
//! | RS3 | Terminal / DefinitivelyUnavailable | Worker process | Unrecorded | no |
//! | RS4 | anything else (Disposing, Ready, …) | any | any | no |
//!
//! | Rule | Observation | Source domain | Filesystem continuity | Repository input | Access |
//! |---|---|---|---|---|---|
//! | RA1 | Terminal | Worker process | Retained | any | RetainedVolume |
//! | RA2 | otherwise | any | any | yes | Repository |
//! | RA3 | otherwise | any | any | no | none |
//!
//! A provider-owned source (a container or Pod) ends with the provider's own
//! Terminal or Unavailable observation. A source in a Worker's process and
//! filesystem (Workdir, Namespace) ends only with that Worker incarnation's
//! processes, so the provider's view of it is not enough.

use awaken_provisioning_contract as pc;
use awaken_session_contract::{RebuildSourceAccess, RebuildSourceEvidence};

/// Where the bound source lives, as the selected provider realizes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebuildSourceDomain {
    /// The provider (container runtime, cluster) owns the source's lifetime.
    Provider,
    /// The source lives in the realizing Worker's own processes and
    /// filesystem: its processes end only with that Worker incarnation, and
    /// its files stay directly readable after the terminal marker.
    WorkerProcess,
}

impl RebuildSourceDomain {
    /// The one provider fact that separates the two domains: a filesystem
    /// tier keeps direct data access after its terminal marker because its
    /// source is the Worker's own filesystem; a container's file reads need
    /// its running process.
    #[must_use]
    pub fn of(provider: &crate::session_environment::SandboxProviderRoles) -> Self {
        if provider.terminal().terminal_data_is_readable() {
            Self::WorkerProcess
        } else {
            Self::Provider
        }
    }
}

/// What is known about the end of the Worker incarnation a process-bound
/// source ran in. A provider-owned source does not read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundIncarnationEnd {
    /// No end is recorded: that incarnation may still run the source's
    /// processes.
    Unrecorded,
    /// The source ran in this live incarnation, whose own supervision
    /// observed its processes end.
    Supervised,
    /// The incarnation's process exit is recorded exactly.
    ExactExit,
    /// A newer container of the same Pod superseded the incarnation's
    /// container: by the kubelet's one-instance-per-name rule its process has
    /// stopped, with no exit code.
    Superseded,
}

impl BoundIncarnationEnd {
    const fn ended(self) -> bool {
        !matches!(self, Self::Unrecorded)
    }
}

/// Everything the derivation reads, captured at the closed observation.
#[derive(Clone, Copy, Debug)]
pub struct RebuildSourceObservation<'a> {
    pub observation: &'a pc::SandboxObservation,
    pub domain: RebuildSourceDomain,
    pub incarnation_end: BoundIncarnationEnd,
    pub filesystem_continuity: pc::FilesystemContinuity,
    pub has_repository: bool,
}

impl RebuildSourceObservation<'_> {
    /// Derive the Rebuild's source evidence (tables RS and RA above).
    #[must_use]
    pub fn evidence(&self) -> RebuildSourceEvidence {
        let observed_closed = matches!(
            self.observation,
            pc::SandboxObservation::Terminal { .. }
                | pc::SandboxObservation::DefinitivelyUnavailable { .. }
        );
        let processes_ended = match self.domain {
            RebuildSourceDomain::Provider => true,
            RebuildSourceDomain::WorkerProcess => self.incarnation_end.ended(),
        };
        let retained_volume = matches!(self.observation, pc::SandboxObservation::Terminal { .. })
            && self.domain == RebuildSourceDomain::WorkerProcess
            && self.filesystem_continuity == pc::FilesystemContinuity::Retained;
        let access = if retained_volume {
            Some(RebuildSourceAccess::RetainedVolume)
        } else if self.has_repository {
            Some(RebuildSourceAccess::Repository)
        } else {
            None
        };
        RebuildSourceEvidence::new(observed_closed && processes_ended, access)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observations() -> [pc::SandboxObservation; 6] {
        [
            pc::SandboxObservation::Terminal {
                physical_incarnation: "old".into(),
            },
            pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: Some("old".into()),
            },
            pc::SandboxObservation::DefinitivelyUnavailable {
                physical_incarnation: None,
            },
            pc::SandboxObservation::Disposing {
                physical_incarnation: "old".into(),
            },
            pc::SandboxObservation::Ready,
            pc::SandboxObservation::Provisioning { deadline: None },
        ]
    }

    #[test]
    fn rebuild_source_evidence_follows_the_closure_and_access_tables() {
        // Sweeps every observation × domain × incarnation end × continuity ×
        // Repository input against rows RS1–RS4 and RA1–RA3.
        for observation in observations() {
            for domain in [
                RebuildSourceDomain::Provider,
                RebuildSourceDomain::WorkerProcess,
            ] {
                for incarnation_end in [
                    BoundIncarnationEnd::Unrecorded,
                    BoundIncarnationEnd::Supervised,
                    BoundIncarnationEnd::ExactExit,
                    BoundIncarnationEnd::Superseded,
                ] {
                    for filesystem_continuity in [
                        pc::FilesystemContinuity::Retained,
                        pc::FilesystemContinuity::Ephemeral,
                    ] {
                        for has_repository in [false, true] {
                            let evidence = RebuildSourceObservation {
                                observation: &observation,
                                domain,
                                incarnation_end,
                                filesystem_continuity,
                                has_repository,
                            }
                            .evidence();
                            let terminal =
                                matches!(observation, pc::SandboxObservation::Terminal { .. });
                            let observed_closed = terminal
                                || matches!(
                                    observation,
                                    pc::SandboxObservation::DefinitivelyUnavailable { .. }
                                );
                            let closed = observed_closed
                                && (domain == RebuildSourceDomain::Provider
                                    || incarnation_end != BoundIncarnationEnd::Unrecorded);
                            let label = format!(
                                "{observation:?} {domain:?} {incarnation_end:?} \
                                 {filesystem_continuity:?} repository={has_repository}"
                            );
                            assert_eq!(evidence.source_closed(), closed, "RS {label}");
                            let access = if terminal
                                && domain == RebuildSourceDomain::WorkerProcess
                                && filesystem_continuity == pc::FilesystemContinuity::Retained
                            {
                                Some(RebuildSourceAccess::RetainedVolume)
                            } else if has_repository {
                                Some(RebuildSourceAccess::Repository)
                            } else {
                                None
                            };
                            assert_eq!(evidence.source_access(), access, "RA {label}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_process_bound_source_closes_with_either_recorded_end() {
        // RS2/RS3 named rows: a Worker-process source the provider reports
        // Terminal is closed by an exact exit or by supersession alike, and
        // stays open while its incarnation's end is unrecorded.
        let terminal = pc::SandboxObservation::Terminal {
            physical_incarnation: "old".into(),
        };
        for (rule, incarnation_end, closed) in [
            ("RS3", BoundIncarnationEnd::Unrecorded, false),
            ("RS2 supervised", BoundIncarnationEnd::Supervised, true),
            ("RS2 exact exit", BoundIncarnationEnd::ExactExit, true),
            ("RS2 superseded", BoundIncarnationEnd::Superseded, true),
        ] {
            let evidence = RebuildSourceObservation {
                observation: &terminal,
                domain: RebuildSourceDomain::WorkerProcess,
                incarnation_end,
                filesystem_continuity: pc::FilesystemContinuity::Retained,
                has_repository: false,
            }
            .evidence();
            assert_eq!(evidence.source_closed(), closed, "{rule}");
            assert_eq!(
                evidence.source_access(),
                Some(RebuildSourceAccess::RetainedVolume),
                "{rule} RA1"
            );
        }
    }
}
