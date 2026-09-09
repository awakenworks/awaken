//! Source publication admission, evaluated against snapshots held by the
//! repository transaction. The repository remains responsible for acquiring
//! those snapshots and atomically writing Source plus the derived WAL phase.

use std::borrow::Cow;

use super::CredentialMutationIntent;
use crate::{CredentialError, CredentialSource};

/// A validated request and its uniquely derived post-publication WAL fact.
/// This borrowed value is a synchronous decision input, not durable state.
pub struct PreparedCredentialPublication<'a> {
    request: Cow<'a, CredentialMutationIntent>,
    reclaiming: CredentialMutationIntent,
}

/// Preserve the repository's existing read/lock schedule: exact replay does
/// not read a distinct replacement's predecessor. Only a fresh replacement
/// needs that second snapshot before it may publish.
pub enum CredentialPublicationAdmission<'a> {
    Replayed(CredentialMutationIntent),
    Apply(CredentialMutationIntent),
    NeedsPredecessor(CredentialPublicationPredecessor<'a>),
}

/// A fresh distinct replacement whose exact WAL and absent destination have
/// already been admitted under the same transaction as the predecessor read.
pub struct CredentialPublicationPredecessor<'a> {
    request: &'a CredentialMutationIntent,
    before: &'a CredentialSource,
    reclaiming: CredentialMutationIntent,
}

impl CredentialMutationIntent {
    /// Complete pre-publication Source equality, shared by begin and apply.
    /// `predecessor` is read only for a distinct replacement; ordinary create
    /// and in-place rotation compare the destination against `before`.
    #[must_use]
    pub fn publication_preimage_matches(
        &self,
        current: Option<&CredentialSource>,
        predecessor: Option<&CredentialSource>,
    ) -> bool {
        if self.is_distinct_replacement() {
            current.is_none() && predecessor == self.before.as_ref()
        } else {
            current == self.before.as_ref()
        }
    }

    /// Validate before storage access, retaining the port's error ordering.
    pub fn prepare_publication(
        &self,
    ) -> Result<PreparedCredentialPublication<'_>, CredentialError> {
        self.validate()?;
        Ok(PreparedCredentialPublication {
            request: Cow::Borrowed(self),
            reclaiming: self.with_material_reclaiming()?,
        })
    }
}

impl PreparedCredentialPublication<'_> {
    /// Move the already validated envelope across a blocking-task boundary.
    /// Only the borrowed request is cloned; neither validation nor derivation
    /// runs again, so domain rejection still precedes connection acquisition.
    #[must_use]
    pub fn into_owned(self) -> PreparedCredentialPublication<'static> {
        PreparedCredentialPublication {
            request: Cow::Owned(self.request.into_owned()),
            reclaiming: self.reclaiming,
        }
    }

    #[must_use]
    pub fn request(&self) -> &CredentialMutationIntent {
        self.request.as_ref()
    }

    /// Cause/effect: exact Reclaiming WAL plus complete published Source is a
    /// replay; otherwise only the exact Ready WAL can publish. Ordinary writes
    /// require the full before image. A distinct replacement first requires an
    /// absent destination, then checks its predecessor using the continuation.
    pub fn admit(
        &self,
        durable: Option<&CredentialMutationIntent>,
        current: Option<&CredentialSource>,
    ) -> Result<CredentialPublicationAdmission<'_>, CredentialError> {
        let request = self.request.as_ref();
        if durable == Some(&self.reclaiming) && current == Some(&request.after) {
            return Ok(CredentialPublicationAdmission::Replayed(
                self.reclaiming.clone(),
            ));
        }
        if durable != Some(request) {
            return Err(CredentialError::MutationConflict(
                "credential mutation does not match durable Ready intent".into(),
            ));
        }
        if request.is_distinct_replacement() {
            if current.is_some() {
                return Err(CredentialError::MutationConflict(
                    "credential replacement id is already occupied".into(),
                ));
            }
            let before = request.before.as_ref().ok_or_else(|| {
                CredentialError::InvalidSource(
                    "distinct replacement requires an exact predecessor".into(),
                )
            })?;
            return Ok(CredentialPublicationAdmission::NeedsPredecessor(
                CredentialPublicationPredecessor {
                    request,
                    before,
                    reclaiming: self.reclaiming.clone(),
                },
            ));
        }
        if !request.publication_preimage_matches(current, None) {
            return Err(CredentialError::MutationConflict(
                "credential revision changed during mutation".into(),
            ));
        }
        Ok(CredentialPublicationAdmission::Apply(
            self.reclaiming.clone(),
        ))
    }
}

impl CredentialPublicationPredecessor<'_> {
    #[must_use]
    pub fn before(&self) -> &CredentialSource {
        self.before
    }

    /// Complete the same admission under the repository's still-held lock or
    /// transaction. SQL insert conflicts remain the adapter's responsibility.
    pub fn admit(
        self,
        current: Option<&CredentialSource>,
    ) -> Result<CredentialMutationIntent, CredentialError> {
        if !self.request.publication_preimage_matches(None, current) {
            return Err(CredentialError::MutationConflict(
                "credential replacement precondition changed during mutation".into(),
            ));
        }
        Ok(self.reclaiming)
    }
}

#[cfg(test)]
mod verification {
    use super::*;
    use crate::repo::{CredentialMaterialMutationFence, CredentialMaterialMutationPhase};
    use crate::{CredentialKind, CredentialSourceId, CredentialStatus};

    // One cause/effect oracle for the real admission, independent of its row
    // comparison expressions. The constructor supplies material-free
    // snapshots: create/in-place/distinct x request phase x missing/exact/
    // reclaiming/foreign WAL x absent/before/after/foreign destination x exact/
    // missing/foreign predecessor. Fixed identities and metadata distinguish
    // rows even when arbitrary signed versions coincide. No UUID, process time,
    // SecretStore, repository IO, or arbitrary collections are proved here.
    //
    // P1 non-Ready => reject. P2 exact derived Reclaiming + after => replay,
    // without consulting predecessor. P3 any other WAL => reject. P4 exact
    // Ready + required before image => apply. P5 every other image => reject.
    // Both successful outcomes return the complete derived Reclaiming envelope.
    fn check_case(shape: u8, phase: u8, wal: u8, current: u8, old: u8, version: i64, next: i64) {
        let source = CredentialSource {
            id: CredentialSourceId("s".into()),
            replacement_of: None,
            workspace_id: "w".into(),
            kind: CredentialKind::Vault,
            descriptor: None,
            provider_id: None,
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version,
        };
        let mut before = source.clone();
        let mut after = source.clone();
        after.version = next;
        after.env_key = Some("a".into());
        if shape == 2 {
            // The distinct-publication contract itself fixes positive old
            // revision and version-one insertion; signed arbitrary revisions
            // remain exercised for create/in-place, not assumed globally.
            before.version = 1;
            after.id = CredentialSourceId("n".into());
            after.version = 1;
            after.replacement_of = Some(awaken_credential_contract::CredentialRef {
                id: "s".into(),
                revision: 1,
            });
        }
        let phase = match phase {
            0 => CredentialMaterialMutationPhase::Writing,
            1 => CredentialMaterialMutationPhase::Ready,
            2 => CredentialMaterialMutationPhase::Reclaiming,
            _ => CredentialMaterialMutationPhase::ReclaimingAbort,
        };
        let request = CredentialMutationIntent {
            material_fence: CredentialMaterialMutationFence {
                format_version: 1,
                phase,
                ..Default::default()
            },
            before: (shape != 0).then_some(before.clone()),
            after: after.clone(),
        };
        let mut reclaiming = request.clone();
        reclaiming.material_fence.phase = CredentialMaterialMutationPhase::Reclaiming;
        let mut foreign_wal = request.clone();
        foreign_wal.after.env_key = Some("f".into());
        let durable = match wal {
            0 => None,
            1 => Some(&request),
            2 => Some(&reclaiming),
            _ => Some(&foreign_wal),
        };
        let mut foreign_row = after.clone();
        foreign_row.env_key = Some("f".into());
        let destination = match current {
            0 => None,
            1 => Some(&before),
            2 => Some(&after),
            _ => Some(&foreign_row),
        };
        let predecessor = match old {
            0 => Some(&before),
            1 => None,
            _ => Some(&foreign_row),
        };
        let expected = if phase != CredentialMaterialMutationPhase::Ready {
            0
        } else if wal == 2 && current == 2 {
            1 // replay
        } else if wal == 1
            && match shape {
                0 => current == 0,
                1 => current == 1,
                _ => current == 0 && old == 0,
            }
        {
            2 // first publication
        } else {
            0
        };
        let result = request.prepare_publication().and_then(|prepared| {
            prepared
                .admit(durable, destination)
                .and_then(|admission| match admission {
                    CredentialPublicationAdmission::Replayed(value) => Ok((1, value)),
                    CredentialPublicationAdmission::Apply(value) => Ok((2, value)),
                    CredentialPublicationAdmission::NeedsPredecessor(check) => {
                        assert_eq!(shape, 2, "P6 only distinct publication reads predecessor");
                        assert_eq!(check.before(), &before, "P6 exact predecessor lookup");
                        check.admit(predecessor).map(|value| (2, value))
                    }
                })
        });
        match result {
            Ok((outcome, value)) => {
                assert_eq!(outcome, expected, "P1-P5 exact publication outcome");
                assert_eq!(value, reclaiming, "P7 complete derived WAL envelope");
            }
            Err(CredentialError::MutationConflict(_)) => {
                assert_eq!(expected, 0, "P1-P5 exact rejection")
            }
            Err(error) => panic!("fixture must reach CAS or phase rejection: {error}"),
        }
    }

    #[test]
    fn complete_publication_admission_follows_the_snapshot_decision_table() {
        for version in [i64::MIN, -1, 0, 1, i64::MAX] {
            for next in [i64::MIN, -1, 0, 1, i64::MAX] {
                for shape in 0..3 {
                    // Distinct construction fixes both revisions to one; do
                    // not repeat identical snapshots for the other 24 pairs.
                    if shape == 2 && (version != 1 || next != 1) {
                        continue;
                    }
                    for phase in 0..4 {
                        for wal in 0..4 {
                            for current in 0..4 {
                                // Only distinct publication consumes the predecessor;
                                // the other shapes have one relevant predecessor case.
                                let predecessor_cases = if shape == 2 { 3 } else { 1 };
                                for old in 0..predecessor_cases {
                                    check_case(shape, phase, wal, current, old, version, next);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
