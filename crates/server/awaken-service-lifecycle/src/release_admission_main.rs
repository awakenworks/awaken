// @formal entrypoint assembly_role.release_consumption_requires_exact_artifact_profile_and_completed_proofs
// @formal entrypoint assembly_role.verified_admission_requires_exact_fresh_tcb_fingerprint
#[cfg(feature = "standalone-release-admission")]
pub mod release_admission;

#[cfg(not(feature = "standalone-release-admission"))]
use awaken_service_lifecycle::release_admission::{
    ReleaseAdmission, ReleaseFacts, TcbAdmission, TcbFacts, release_admission, tcb_admission,
};
#[cfg(feature = "standalone-release-admission")]
use release_admission::{
    ReleaseAdmission, ReleaseFacts, TcbAdmission, TcbFacts, release_admission, tcb_admission,
};

fn parse_bool(value: Option<String>) -> Result<bool, ()> {
    match value.as_deref() {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => Err(()),
    }
}

fn parse_u64(value: Option<String>) -> Result<u64, ()> {
    value.ok_or(())?.parse().map_err(|_| ())
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let decision = match arguments.next().as_deref() {
        Some("release") => (|| {
            let facts = ReleaseFacts {
                artifact_name_matches: parse_bool(arguments.next())?,
                artifact_digest_matches: parse_bool(arguments.next())?,
                profile_matches: parse_bool(arguments.next())?,
                proof_complete: parse_bool(arguments.next())?,
                source_matches: parse_bool(arguments.next())?,
                manifest_matches: parse_bool(arguments.next())?,
            };
            if arguments.next().is_some() {
                return Err(());
            }
            Ok(match release_admission(facts) {
                ReleaseAdmission::Admit => "admit",
                ReleaseAdmission::ArtifactNameMismatch => "artifact-name",
                ReleaseAdmission::ArtifactDigestMismatch => "artifact-digest",
                ReleaseAdmission::ProfileMismatch => "profile",
                ReleaseAdmission::ProofIncomplete => "proof",
                ReleaseAdmission::SourceMismatch => "source",
                ReleaseAdmission::ManifestMismatch => "manifest",
            })
        })(),
        Some("tcb") => (|| {
            let facts = TcbFacts {
                contract_matches: parse_bool(arguments.next())?,
                observed_version_valid: parse_bool(arguments.next())?,
                target_version_valid: parse_bool(arguments.next())?,
                version_matches: parse_bool(arguments.next())?,
                observed_configuration_valid: parse_bool(arguments.next())?,
                target_configuration_valid: parse_bool(arguments.next())?,
                configuration_matches: parse_bool(arguments.next())?,
                evidence_digest_valid: parse_bool(arguments.next())?,
                maximum_age_seconds: parse_u64(arguments.next())?,
                observed_at_unix: parse_u64(arguments.next())?,
                now_unix: parse_u64(arguments.next())?,
            };
            if arguments.next().is_some() {
                return Err(());
            }
            Ok(match tcb_admission(facts) {
                TcbAdmission::Admit => "admit",
                TcbAdmission::ContractMismatch => "contract",
                TcbAdmission::InvalidObservedVersion => "observed-version",
                TcbAdmission::InvalidTargetVersion => "target-version",
                TcbAdmission::VersionMismatch => "version",
                TcbAdmission::InvalidObservedConfiguration => "observed-configuration",
                TcbAdmission::InvalidTargetConfiguration => "target-configuration",
                TcbAdmission::ConfigurationMismatch => "configuration",
                TcbAdmission::InvalidEvidenceDigest => "evidence-digest",
                TcbAdmission::InvalidMaximumAge => "maximum-age",
                TcbAdmission::InvalidObservationTime => "observation-time",
                TcbAdmission::EvidenceExpired => "expired",
            })
        })(),
        _ => Err(()),
    };
    match decision {
        Ok(value) => println!("{value}"),
        Err(()) => {
            eprintln!("invalid release-admission kernel invocation");
            std::process::exit(2);
        }
    }
}
