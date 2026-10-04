// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: secret-free coordinate and closed outcome vocabulary for the existing
// @formal reason: RepositoryRealizer port. Dynamic path: the exact desired and optional prior commits
// @formal reason: validate before transport, canonical receipts bind the frozen realization plan, and only
// @formal reason: a proven local HEAD mismatch, absent update lease, or third remote commit is
// @formal reason: admissible as permanent rejection; this
// @formal reason: extraction adds no realizer, writer, queue, or state machine. Evidence:
// @formal reason: resource.files.resource_lifecycle, G48, and the adjacent contract decision table; this
// @formal reason: is a product-requirement classification, not a direct formal-proof claim.
//! Exact Repository publication coordinate, outcome, and transport errors.
//!
//! The parent Sandbox contract retains the sole `RepositoryRealizer` port. This
//! module only keeps its secret-free publication values below the source file
//! limit; it owns no transport, retry state, or second realization path.

use serde::{Deserialize, Serialize};

use super::{RepositoryRealizationPlan, SandboxError};

fn is_canonical_git_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Exact Agent-authored Git coordinate approved for one explicit Repository
/// publication effect. The branch is a symbolic local branch name and `commit`
/// is the full SHA-1 object id observed by the authoring workflow. Publication
/// adapters must compare both values exactly before any network operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPublicationExpectation {
    pub branch: String,
    pub commit: String,
    /// Optional exact remote commit which the caller authorizes this publication
    /// to replace. Omission preserves create-only publication. The Git adapter
    /// treats this value as a force-with-lease compare-and-swap precondition, not
    /// as permission to overwrite an arbitrary current ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_prior_commit: Option<String>,
}

impl RepositoryPublicationExpectation {
    /// Validate the transport-independent coordinate shape. The Git adapter
    /// additionally applies `check-ref-format` and compares the live symbolic
    /// branch and HEAD before contacting the frozen remote.
    pub fn validate(&self) -> Result<(), SandboxError> {
        if self.branch.is_empty() || self.branch.trim() != self.branch {
            return Err(SandboxError::invalid(
                "repository publication branch must be non-empty without surrounding whitespace",
            ));
        }
        if !is_canonical_git_oid(&self.commit) {
            return Err(SandboxError::invalid(
                "repository publication commit must be a canonical lowercase 40-hex object id",
            ));
        }
        if self
            .expected_prior_commit
            .as_ref()
            .is_some_and(|commit| !is_canonical_git_oid(commit))
        {
            return Err(SandboxError::invalid(
                "repository publication expected prior commit must be a canonical lowercase 40-hex object id",
            ));
        }
        Ok(())
    }
}

/// Closed, secret-free cause for a permanent Repository publication rejection.
///
/// An exact local HEAD mismatch proved before any remote request, or an exact
/// remote-ref compare-and-swap failure, may cross this boundary. Git
/// transport/authentication/I/O failures remain [`RepositoryPublicationError::Unavailable`]
/// because the adapter cannot prove a permanent coordinate conflict from those
/// failures. The desired commit remains in the immutable command; observations
/// carry only their own explicitly named local or remote source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryPublicationRejection {
    #[error("repository local HEAD is {observed_commit}, not the desired commit")]
    LocalCommitMismatch { observed_commit: String },
    #[error("repository remote branch is absent but an expected prior commit was required")]
    RemoteRefAbsent,
    #[error("repository remote branch is bound to stale commit {observed_commit}")]
    RemoteRefChanged { observed_commit: String },
}

impl RepositoryPublicationRejection {
    /// Verify that the source-specific observation conflicts with this exact
    /// publication expectation. A desired local commit or a desired/expected-
    /// prior remote commit is never permanent rejection evidence.
    pub fn verify(
        &self,
        expectation: &RepositoryPublicationExpectation,
    ) -> Result<(), SandboxError> {
        expectation.validate()?;
        match self {
            Self::LocalCommitMismatch { observed_commit } => {
                is_canonical_git_oid(observed_commit) && observed_commit != &expectation.commit
            }
            Self::RemoteRefAbsent => expectation.expected_prior_commit.is_some(),
            Self::RemoteRefChanged { observed_commit } => {
                is_canonical_git_oid(observed_commit)
                    && observed_commit != &expectation.commit
                    && expectation.expected_prior_commit.as_ref() != Some(observed_commit)
            }
        }
        .then_some(())
        .ok_or_else(|| SandboxError::invalid("repository publication rejection is not proven"))
    }
}

/// Typed result boundary for the sole Repository transport effect. Permanent
/// compare-and-swap rejection is kept distinct from retryable transport loss so
/// Session cleanup can durably close only the former.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryPublicationError {
    #[error("repository publication rejected: {0}")]
    Rejected(RepositoryPublicationRejection),
    #[error("repository publication unavailable: {0}")]
    Unavailable(#[from] SandboxError),
}

/// Secret-free, deterministic evidence for one exact Repository publication.
/// The receipt deliberately has no `changed`/disposition bit: the first push and
/// an exact replay against an already-current remote produce identical evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPublicationReceipt {
    pub repository_id: String,
    pub source_remote_url: String,
    pub branch: String,
    pub commit: String,
}

impl RepositoryPublicationReceipt {
    #[must_use]
    pub fn new(
        plan: &RepositoryRealizationPlan,
        expectation: &RepositoryPublicationExpectation,
    ) -> Self {
        Self {
            repository_id: plan.repository_id.clone(),
            source_remote_url: plan.source_remote_url.clone(),
            branch: expectation.branch.clone(),
            commit: expectation.commit.clone(),
        }
    }

    /// Verify that this receipt is the canonical projection of the exact frozen
    /// realization plan and publication expectation.
    pub fn verify(
        &self,
        plan: &RepositoryRealizationPlan,
        expectation: &RepositoryPublicationExpectation,
    ) -> Result<(), SandboxError> {
        expectation.validate()?;
        if self == &Self::new(plan, expectation) {
            Ok(())
        } else {
            Err(SandboxError::conflict(
                "repository publication receipt does not match its exact intent",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MountAccess;

    #[test]
    fn local_commit_mismatch_requires_exact_pre_io_evidence() {
        // C1 observed local HEAD is canonical lowercase full SHA; C2 it differs
        // from the frozen desired commit. R1 C1+C2 -> permanent local rejection;
        // R2 !C1 or !C2 -> invalid evidence, never a remote-ref claim or
        // terminal rejection. A matching prior commit is still a valid local
        // mismatch because the prior is a distinct remote CAS coordinate.
        let expectation = RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: "1".repeat(40),
            expected_prior_commit: Some("2".repeat(40)),
        };
        let valid = RepositoryPublicationRejection::LocalCommitMismatch {
            observed_commit: "2".repeat(40),
        };
        valid.verify(&expectation).expect("R1");
        assert_eq!(
            serde_json::to_value(&valid).unwrap(),
            serde_json::json!({"reason":"local_commit_mismatch","observed_commit":"2".repeat(40)}),
            "R1 source-specific wire"
        );
        for observed_commit in [
            expectation.commit.clone(),
            "short".into(),
            "ABCDEF0123456789abcdef0123456789abcdef01".into(),
        ] {
            assert!(
                RepositoryPublicationRejection::LocalCommitMismatch { observed_commit }
                    .verify(&expectation)
                    .is_err(),
                "R2"
            );
        }
    }

    #[test]
    fn repository_publication_coordinate_and_receipt_are_canonical() {
        // Contract cause/effect rules: C1 nonempty exact branch + 40-hex desired
        // and optional prior commit => valid coordinate; C2 malformed branch,
        // desired commit, or prior commit => reject; C3 one plan+coordinate => one
        // deterministic secret-free receipt; C4 any receipt field differs =>
        // verification rejects it; C5 omitted prior preserves create-only wire.
        let plan = RepositoryRealizationPlan {
            repository_id: "repository-a".into(),
            mount_path: "workspace/repository-a".into(),
            source_remote_url: "https://example.invalid/repository-a.git".into(),
            transport_url: "https://gateway.invalid/git/repository-a".into(),
            initial_branch: None,
            initial_commit: None,
            access: MountAccess::ReadWrite,
        };
        let expectation = RepositoryPublicationExpectation {
            branch: "awf/issue-1".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            expected_prior_commit: None,
        };
        expectation.validate().expect("C1");
        for invalid in [
            RepositoryPublicationExpectation {
                branch: String::new(),
                commit: expectation.commit.clone(),
                expected_prior_commit: None,
            },
            RepositoryPublicationExpectation {
                branch: " awf/issue-1".into(),
                commit: expectation.commit.clone(),
                expected_prior_commit: None,
            },
            RepositoryPublicationExpectation {
                branch: expectation.branch.clone(),
                commit: "short".into(),
                expected_prior_commit: None,
            },
            RepositoryPublicationExpectation {
                branch: expectation.branch.clone(),
                commit: "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".into(),
                expected_prior_commit: None,
            },
        ] {
            assert!(invalid.validate().is_err(), "C2: {invalid:?}");
        }
        let mut invalid_prior = expectation.clone();
        invalid_prior.expected_prior_commit = Some("short".into());
        assert!(invalid_prior.validate().is_err(), "C2 prior");
        let mut uppercase_desired = expectation.clone();
        uppercase_desired.commit = "ABCDEF0123456789abcdef0123456789abcdef01".into();
        assert!(
            uppercase_desired.validate().is_err(),
            "C2 uppercase desired"
        );
        let mut uppercase_prior = expectation.clone();
        uppercase_prior.expected_prior_commit =
            Some("ABCDEF0123456789abcdef0123456789abcdef01".into());
        assert!(uppercase_prior.validate().is_err(), "C2 uppercase prior");
        assert_eq!(
            serde_json::to_value(&expectation).unwrap(),
            serde_json::json!({
                "branch": "awf/issue-1",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            }),
            "C5 historical create-only wire"
        );
        let mut update = expectation.clone();
        update.expected_prior_commit = Some("1111111111111111111111111111111111111111".into());
        update.validate().expect("C1 prior");
        RepositoryPublicationRejection::RemoteRefAbsent
            .verify(&update)
            .expect("C1 absent is stale only with an expected prior");
        assert!(
            RepositoryPublicationRejection::RemoteRefChanged {
                observed_commit: "ABCDEF0123456789abcdef0123456789abcdef01".into(),
            }
            .verify(&update)
            .is_err(),
            "C2 uppercase observed commit is not canonical rejection evidence"
        );
        assert!(
            RepositoryPublicationRejection::RemoteRefAbsent
                .verify(&expectation)
                .is_err(),
            "C5 absent remains createable without a prior"
        );

        let receipt = RepositoryPublicationReceipt::new(&plan, &expectation);
        receipt.verify(&plan, &expectation).expect("C3");
        assert_eq!(
            serde_json::to_value(&receipt).unwrap(),
            serde_json::json!({
                "repository_id": "repository-a",
                "source_remote_url": "https://example.invalid/repository-a.git",
                "branch": "awf/issue-1",
                "commit": "0123456789abcdef0123456789abcdef01234567"
            }),
            "C3 stable wire has no changed/disposition or credential field"
        );
        let mut mismatched = receipt.clone();
        mismatched.branch = "awf/other".into();
        assert!(mismatched.verify(&plan, &expectation).is_err(), "C4");
    }
}
