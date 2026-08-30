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
            return Err(SandboxError::new(
                "repository publication branch must be non-empty without surrounding whitespace",
            ));
        }
        if !is_canonical_git_oid(&self.commit) {
            return Err(SandboxError::new(
                "repository publication commit must be a canonical lowercase 40-hex object id",
            ));
        }
        if self
            .expected_prior_commit
            .as_ref()
            .is_some_and(|commit| !is_canonical_git_oid(commit))
        {
            return Err(SandboxError::new(
                "repository publication expected prior commit must be a canonical lowercase 40-hex object id",
            ));
        }
        Ok(())
    }
}

/// Closed, secret-free cause for a permanent Repository publication rejection.
///
/// Only exact remote-ref compare-and-swap failures cross this boundary. Git
/// transport/authentication/I/O failures remain [`RepositoryPublicationError::Unavailable`]
/// because the adapter cannot prove a permanent stale coordinate from those
/// failures. The expected and desired commits remain in the immutable command;
/// this evidence records only the conflicting remote observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryPublicationRejection {
    #[error("repository remote branch is absent but an expected prior commit was required")]
    RemoteRefAbsent,
    #[error("repository remote branch is bound to stale commit {observed_commit}")]
    RemoteRefChanged { observed_commit: String },
}

impl RepositoryPublicationRejection {
    /// Verify that the observation is a genuine conflict with this exact
    /// publication expectation. A desired or expected-prior commit is never
    /// admissible as permanent rejection evidence.
    pub fn verify(
        &self,
        expectation: &RepositoryPublicationExpectation,
    ) -> Result<(), SandboxError> {
        expectation.validate()?;
        match self {
            Self::RemoteRefAbsent if expectation.expected_prior_commit.is_some() => Ok(()),
            Self::RemoteRefAbsent => Err(SandboxError::new(
                "an absent remote ref is not stale for create-only publication",
            )),
            Self::RemoteRefChanged { observed_commit }
                if is_canonical_git_oid(observed_commit)
                    && observed_commit != &expectation.commit
                    && expectation.expected_prior_commit.as_ref() != Some(observed_commit) =>
            {
                Ok(())
            }
            Self::RemoteRefChanged { .. } => Err(SandboxError::new(
                "repository publication rejection does not prove a stale remote ref",
            )),
        }
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
            Err(SandboxError::new(
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
