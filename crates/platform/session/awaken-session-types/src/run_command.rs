//! Session Run command values read by both the Session application and the
//! durable dispatch that persists them.
//!
//! The Session application authors both values; dispatch stores and compares
//! them without interpreting how they were derived. Their serialized forms are
//! persisted in dispatch rows and completion tombstones and must not change.

use serde::{Deserialize, Serialize};

/// Queue effect requested by one Session Run admission.
///
/// The value is part of the immutable reservation identity: recovery must
/// preserve whether this Run merely follows prior work or replaces every older
/// live dispatch on its logical Thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRunReplacement {
    #[default]
    PreservePrior,
    SupersedePrior,
}

impl SessionRunReplacement {
    #[must_use]
    pub const fn supersedes_prior(self) -> bool {
        matches!(self, Self::SupersedePrior)
    }
}

/// Versioned compact identity of the immutable command admitted by the
/// Session application.
///
/// Runtime projections are deliberately absent: a retry may observe newer
/// Agent, model, Resource, Environment, Skill, or placement facts without
/// changing the command that already owns the reserved Run. Distributed trace
/// context is also excluded because it identifies an attempt, not the command.
/// The digest itself is computed by the command's owner
/// (`AdmitSessionRun::command_fingerprint`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionRunCommandFingerprint(String);

impl SessionRunCommandFingerprint {
    pub const CURRENT_PREFIX: &'static str = "session-command-v1:sha256:";

    /// Wrap a SHA-256 digest of the current command tuple. Only a lowercase
    /// 64-digit hex digest is accepted, exactly the form [`Self::is_current`]
    /// admits, so no caller can author a current value that is not current.
    #[must_use]
    pub fn from_sha256_hex(digest: &str) -> Option<Self> {
        is_sha256_hex(digest).then(|| Self(format!("{}{digest}", Self::CURRENT_PREFIX)))
    }

    /// Whether this value is the exact current wire format. Unknown versions
    /// remain deserializable for durable-row inspection but never authorize a
    /// new reservation or replay.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.0
            .strip_prefix(Self::CURRENT_PREFIX)
            .is_some_and(is_sha256_hex)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn is_sha256_hex(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{SessionRunCommandFingerprint, SessionRunReplacement};

    #[test]
    fn current_fingerprint_construction_admits_exactly_what_is_current_accepts() {
        // Cause/effect table: C1 the digest is 64 lowercase hex digits, or it is
        // short, long, uppercase or non-hex. Effects: E1 C1 builds a value
        // that `is_current` accepts with the versioned prefix; E2 every other
        // digest builds nothing. Rules R1=C1=>E1, R2..R5=>E2.
        let digest = "0123456789abcdef".repeat(4);
        let fingerprint =
            SessionRunCommandFingerprint::from_sha256_hex(&digest).expect("R1 valid digest");
        assert!(fingerprint.is_current(), "R1/E1");
        assert_eq!(
            fingerprint.as_str(),
            format!("{}{digest}", SessionRunCommandFingerprint::CURRENT_PREFIX),
            "R1/E1"
        );
        for invalid in [
            digest[..63].to_string(),
            format!("{digest}0"),
            digest.to_uppercase(),
            format!("{}g", &digest[..63]),
        ] {
            assert!(
                SessionRunCommandFingerprint::from_sha256_hex(&invalid).is_none(),
                "R2-R5/E2 {invalid}"
            );
        }
    }

    #[test]
    fn replacement_wire_is_the_external_unit_variant_name() {
        // Dispatch rows persist this enum; its two wire names are fixed.
        assert_eq!(
            serde_json::to_string(&SessionRunReplacement::SupersedePrior).unwrap(),
            "\"SupersedePrior\""
        );
        assert_eq!(
            serde_json::from_str::<SessionRunReplacement>("\"PreservePrior\"").unwrap(),
            SessionRunReplacement::PreservePrior
        );
        assert!(SessionRunReplacement::SupersedePrior.supersedes_prior());
        assert!(!SessionRunReplacement::default().supersedes_prior());
    }
}
