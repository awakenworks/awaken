//! Finite admission kernel consumed by the proof-carrying release validator.
//!
//! JSON shape, hashing, Git provenance, and external observations remain with
//! `scripts/release/verification_manifest.py`. This module owns only the final
//! synchronous decision over the facts that validator projects.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseAdmission {
    Admit,
    ArtifactNameMismatch,
    ArtifactDigestMismatch,
    ProfileMismatch,
    ProofIncomplete,
    SourceMismatch,
    ManifestMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleaseFacts {
    pub artifact_name_matches: bool,
    pub artifact_digest_matches: bool,
    pub profile_matches: bool,
    pub proof_complete: bool,
    pub source_matches: bool,
    pub manifest_matches: bool,
}

/// Return the first fail-closed reason for one frozen release snapshot.
#[must_use]
pub const fn release_admission(facts: ReleaseFacts) -> ReleaseAdmission {
    if !facts.artifact_name_matches {
        ReleaseAdmission::ArtifactNameMismatch
    } else if !facts.artifact_digest_matches {
        ReleaseAdmission::ArtifactDigestMismatch
    } else if !facts.profile_matches {
        ReleaseAdmission::ProfileMismatch
    } else if !facts.proof_complete {
        ReleaseAdmission::ProofIncomplete
    } else if !facts.source_matches {
        ReleaseAdmission::SourceMismatch
    } else if !facts.manifest_matches {
        ReleaseAdmission::ManifestMismatch
    } else {
        ReleaseAdmission::Admit
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TcbAdmission {
    Admit,
    ContractMismatch,
    InvalidObservedVersion,
    InvalidTargetVersion,
    VersionMismatch,
    InvalidObservedConfiguration,
    InvalidTargetConfiguration,
    ConfigurationMismatch,
    InvalidEvidenceDigest,
    InvalidMaximumAge,
    InvalidObservationTime,
    EvidenceExpired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcbFacts {
    pub contract_matches: bool,
    pub observed_version_valid: bool,
    pub target_version_valid: bool,
    pub version_matches: bool,
    pub observed_configuration_valid: bool,
    pub target_configuration_valid: bool,
    pub configuration_matches: bool,
    pub evidence_digest_valid: bool,
    pub maximum_age_seconds: u64,
    pub observed_at_unix: u64,
    pub now_unix: u64,
}

/// Admit one already indexed TCB dependency only for an exact fresh snapshot.
#[must_use]
pub const fn tcb_admission(facts: TcbFacts) -> TcbAdmission {
    if !facts.contract_matches {
        TcbAdmission::ContractMismatch
    } else if !facts.observed_version_valid {
        TcbAdmission::InvalidObservedVersion
    } else if !facts.target_version_valid {
        TcbAdmission::InvalidTargetVersion
    } else if !facts.version_matches {
        TcbAdmission::VersionMismatch
    } else if !facts.observed_configuration_valid {
        TcbAdmission::InvalidObservedConfiguration
    } else if !facts.target_configuration_valid {
        TcbAdmission::InvalidTargetConfiguration
    } else if !facts.configuration_matches {
        TcbAdmission::ConfigurationMismatch
    } else if !facts.evidence_digest_valid {
        TcbAdmission::InvalidEvidenceDigest
    } else if facts.maximum_age_seconds == 0 {
        TcbAdmission::InvalidMaximumAge
    } else {
        match facts.now_unix.checked_sub(facts.observed_at_unix) {
            None => TcbAdmission::InvalidObservationTime,
            Some(age) if age > facts.maximum_age_seconds => TcbAdmission::EvidenceExpired,
            Some(_) => TcbAdmission::Admit,
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn verified_release_and_tcb_admission_are_total_exact_and_fail_closed() {
    // Cause/effect decision table for both production-consumed dispositions:
    // | rule | release facts | TCB facts | effect |
    // | R1 | all exact and complete | n/a | release Admit |
    // | R2 | first false in artifact/profile/proof/source/manifest order | n/a | matching reject |
    // | R3 | n/a | all identities/hashes exact, max>0, 0<=age<=max | TCB Admit |
    // | R4 | n/a | first false scalar guard | matching reject |
    // | R5 | n/a | max=0, future observation, or age>max | matching reject |
    // The Python caller supplies parsed facts; JSON, hashes, clocks, process
    // execution, and snapshot authentication are outside this pure proof.
    let release = ReleaseFacts {
        artifact_name_matches: kani::any(),
        artifact_digest_matches: kani::any(),
        profile_matches: kani::any(),
        proof_complete: kani::any(),
        source_matches: kani::any(),
        manifest_matches: kani::any(),
    };
    let expected_release = if !release.artifact_name_matches {
        ReleaseAdmission::ArtifactNameMismatch
    } else if !release.artifact_digest_matches {
        ReleaseAdmission::ArtifactDigestMismatch
    } else if !release.profile_matches {
        ReleaseAdmission::ProfileMismatch
    } else if !release.proof_complete {
        ReleaseAdmission::ProofIncomplete
    } else if !release.source_matches {
        ReleaseAdmission::SourceMismatch
    } else if !release.manifest_matches {
        ReleaseAdmission::ManifestMismatch
    } else {
        ReleaseAdmission::Admit
    };
    assert_eq!(release_admission(release), expected_release);

    let tcb = TcbFacts {
        contract_matches: kani::any(),
        observed_version_valid: kani::any(),
        target_version_valid: kani::any(),
        version_matches: kani::any(),
        observed_configuration_valid: kani::any(),
        target_configuration_valid: kani::any(),
        configuration_matches: kani::any(),
        evidence_digest_valid: kani::any(),
        maximum_age_seconds: kani::any(),
        observed_at_unix: kani::any(),
        now_unix: kani::any(),
    };
    let expected_tcb = if !tcb.contract_matches {
        TcbAdmission::ContractMismatch
    } else if !tcb.observed_version_valid {
        TcbAdmission::InvalidObservedVersion
    } else if !tcb.target_version_valid {
        TcbAdmission::InvalidTargetVersion
    } else if !tcb.version_matches {
        TcbAdmission::VersionMismatch
    } else if !tcb.observed_configuration_valid {
        TcbAdmission::InvalidObservedConfiguration
    } else if !tcb.target_configuration_valid {
        TcbAdmission::InvalidTargetConfiguration
    } else if !tcb.configuration_matches {
        TcbAdmission::ConfigurationMismatch
    } else if !tcb.evidence_digest_valid {
        TcbAdmission::InvalidEvidenceDigest
    } else if tcb.maximum_age_seconds == 0 {
        TcbAdmission::InvalidMaximumAge
    } else {
        match tcb.now_unix.checked_sub(tcb.observed_at_unix) {
            None => TcbAdmission::InvalidObservationTime,
            Some(age) if age > tcb.maximum_age_seconds => TcbAdmission::EvidenceExpired,
            Some(_) => TcbAdmission::Admit,
        }
    };
    assert_eq!(tcb_admission(tcb), expected_tcb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_tcb_decision_tables_preserve_every_failure_cut() {
        // Cause/effect table is owned by the Kani harness above. This native
        // cross-check uses concrete boundary values: R1 exact release accepts;
        // R2 flips every release fact independently; R3 accepts age=max; R4
        // flips every TCB scalar guard; R5 covers zero max, future and expiry.
        let release = ReleaseFacts {
            artifact_name_matches: true,
            artifact_digest_matches: true,
            profile_matches: true,
            proof_complete: true,
            source_matches: true,
            manifest_matches: true,
        };
        assert_eq!(release_admission(release), ReleaseAdmission::Admit, "R1");
        for (index, expected) in [
            ReleaseAdmission::ArtifactNameMismatch,
            ReleaseAdmission::ArtifactDigestMismatch,
            ReleaseAdmission::ProfileMismatch,
            ReleaseAdmission::ProofIncomplete,
            ReleaseAdmission::SourceMismatch,
            ReleaseAdmission::ManifestMismatch,
        ]
        .into_iter()
        .enumerate()
        {
            let mut changed = release;
            match index {
                0 => changed.artifact_name_matches = false,
                1 => changed.artifact_digest_matches = false,
                2 => changed.profile_matches = false,
                3 => changed.proof_complete = false,
                4 => changed.source_matches = false,
                5 => changed.manifest_matches = false,
                _ => unreachable!(),
            }
            assert_eq!(release_admission(changed), expected, "R2.{index}");
        }

        let tcb = TcbFacts {
            contract_matches: true,
            observed_version_valid: true,
            target_version_valid: true,
            version_matches: true,
            observed_configuration_valid: true,
            target_configuration_valid: true,
            configuration_matches: true,
            evidence_digest_valid: true,
            maximum_age_seconds: 60,
            observed_at_unix: 100,
            now_unix: 160,
        };
        assert_eq!(tcb_admission(tcb), TcbAdmission::Admit, "R3");
        for (index, expected) in [
            TcbAdmission::ContractMismatch,
            TcbAdmission::InvalidObservedVersion,
            TcbAdmission::InvalidTargetVersion,
            TcbAdmission::VersionMismatch,
            TcbAdmission::InvalidObservedConfiguration,
            TcbAdmission::InvalidTargetConfiguration,
            TcbAdmission::ConfigurationMismatch,
            TcbAdmission::InvalidEvidenceDigest,
        ]
        .into_iter()
        .enumerate()
        {
            let mut changed = tcb;
            match index {
                0 => changed.contract_matches = false,
                1 => changed.observed_version_valid = false,
                2 => changed.target_version_valid = false,
                3 => changed.version_matches = false,
                4 => changed.observed_configuration_valid = false,
                5 => changed.target_configuration_valid = false,
                6 => changed.configuration_matches = false,
                7 => changed.evidence_digest_valid = false,
                _ => unreachable!(),
            }
            assert_eq!(tcb_admission(changed), expected, "R4.{index}");
        }
        assert_eq!(
            tcb_admission(TcbFacts {
                maximum_age_seconds: 0,
                ..tcb
            }),
            TcbAdmission::InvalidMaximumAge,
            "R5.zero"
        );
        assert_eq!(
            tcb_admission(TcbFacts {
                observed_at_unix: 161,
                ..tcb
            }),
            TcbAdmission::InvalidObservationTime,
            "R5.future"
        );
        assert_eq!(
            tcb_admission(TcbFacts {
                now_unix: 161,
                ..tcb
            }),
            TcbAdmission::EvidenceExpired,
            "R5.expired"
        );
    }
}
