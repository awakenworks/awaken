//! The owner of cleanup admission for both credential publication envelopes.
//! Adapters must read these arguments and apply the decision under one lock or
//! database transaction. This does not certify external SecretStore deletion.

use super::{
    CredentialMaterialMutationPhase, CredentialMutationIntent, PendingManagedCredentialMutation,
};
use crate::{CredentialError, CredentialSource, catalog::ManagedVaultCredential};

const fn abort_admitted(
    phase: CredentialMaterialMutationPhase,
    exact_authority: bool,
    unpublished: bool,
) -> bool {
    match phase {
        CredentialMaterialMutationPhase::Writing
        | CredentialMaterialMutationPhase::Ready
        | CredentialMaterialMutationPhase::ReclaimingAbort => exact_authority && unpublished,
        CredentialMaterialMutationPhase::Reclaiming => false,
    }
}

const fn completion_admitted(
    phase: CredentialMaterialMutationPhase,
    exact_authority: bool,
    published: bool,
    unpublished: bool,
) -> bool {
    exact_authority
        && match phase {
            CredentialMaterialMutationPhase::Reclaiming => published,
            CredentialMaterialMutationPhase::ReclaimingAbort => unpublished,
            CredentialMaterialMutationPhase::Writing | CredentialMaterialMutationPhase::Ready => {
                false
            }
        }
}

fn require_admission(admitted: bool) -> Result<(), CredentialError> {
    if admitted {
        Ok(())
    } else {
        Err(CredentialError::MutationConflict(
            "credential cleanup does not match exact durable intent and phase truth".into(),
        ))
    }
}

impl CredentialMutationIntent {
    fn is_unpublished_at(&self, current: Option<&CredentialSource>) -> bool {
        if self.is_distinct_replacement() {
            current.is_none()
        } else {
            current == self.before.as_ref()
        }
    }

    /// Decide abort under the repository's transaction. A source-only original
    /// request may replay its exact derived abort, but must recheck unpublished
    /// truth even then. A new owner/epoch or different payload is never a replay.
    pub fn admit_abort(
        &self,
        durable: &Self,
        current: Option<&CredentialSource>,
    ) -> Result<Self, CredentialError> {
        self.validate()?;
        let abort = self.with_material_reclaiming_abort()?;
        require_admission(abort_admitted(
            self.material_fence.phase,
            durable == self || durable == &abort,
            self.is_unpublished_at(current),
        ))?;
        Ok(abort)
    }

    /// Admit retirement of a present WAL row, never merely a matching key.
    /// Missing-row completion remains the port's idempotent no-op.
    pub fn admit_completion(
        &self,
        durable: &Self,
        current: Option<&CredentialSource>,
    ) -> Result<(), CredentialError> {
        self.validate()?;
        require_admission(completion_admitted(
            self.material_fence.phase,
            durable == self,
            current == Some(&self.after),
            self.is_unpublished_at(current),
        ))
    }
}

impl PendingManagedCredentialMutation {
    fn is_unpublished_at(
        &self,
        source: Option<&CredentialSource>,
        child: Option<&ManagedVaultCredential>,
    ) -> bool {
        source == self.before_source.as_ref() && child == self.before_credential.as_ref()
    }

    /// Decide abort of the exact Source+child envelope. Preserve this port's
    /// existing recovery contract: after abort the caller must carry the exact
    /// durable ReclaimingAbort envelope, not the original Writing request.
    pub fn admit_abort(
        &self,
        durable: &Self,
        source: Option<&CredentialSource>,
        child: Option<&ManagedVaultCredential>,
    ) -> Result<Self, CredentialError> {
        self.validate()
            .map_err(|error| CredentialError::MutationConflict(error.to_string()))?;
        require_admission(abort_admitted(
            self.material_fence.phase,
            durable == self,
            self.is_unpublished_at(source, child),
        ))?;
        self.with_material_reclaiming_abort()
    }

    /// Admit retirement only against the full committed or unpublished pair.
    /// The adapter owns the transaction, not this phase/identity decision.
    pub fn admit_completion(
        &self,
        durable: &Self,
        source: Option<&CredentialSource>,
        child: Option<&ManagedVaultCredential>,
    ) -> Result<(), CredentialError> {
        self.validate()
            .map_err(|error| CredentialError::MutationConflict(error.to_string()))?;
        require_admission(completion_admitted(
            self.material_fence.phase,
            durable == self,
            source == Some(&self.after_source) && child == Some(&self.after_credential),
            self.is_unpublished_at(source, child),
        ))
    }
}

#[cfg(kani)]
#[kani::proof]
fn credential_cleanup_requires_exact_authority_and_phase_truth() {
    // Causes: all four phases, exact intent identity, published/unpublished
    // projection. No relation is assumed between the projections (a no-op
    // mutation may satisfy both). Rules: foreign authority always rejects;
    // Writing/Ready never complete; Reclaiming only completes published truth;
    // abort only admits unpublished truth and can never abort Reclaiming.
    // This proves the production scalar decisions, not heap Eq or SQL locking.
    use CredentialMaterialMutationPhase::*;
    let phase = match kani::any::<u8>() % 4 {
        0 => Writing,
        1 => Ready,
        2 => Reclaiming,
        _ => ReclaimingAbort,
    };
    let exact = kani::any::<bool>();
    let published = kani::any::<bool>();
    let unpublished = kani::any::<bool>();
    let complete = completion_admitted(phase, exact, published, unpublished);
    let abort = abort_admitted(phase, exact, unpublished);
    match phase {
        Writing | Ready => {
            assert!(!complete);
            assert_eq!(abort, exact && unpublished);
        }
        Reclaiming => {
            assert_eq!(complete, exact && published);
            assert!(!abort);
        }
        ReclaimingAbort => {
            assert_eq!(complete, exact && unpublished);
            assert_eq!(abort, complete);
        }
    }
    if !exact {
        assert!(!complete && !abort);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CredentialKind, CredentialStatus, SecretRef,
        catalog::{ManagedCredentialAuth, ManagedCredentialLifecycle},
    };

    fn source() -> CredentialSource {
        CredentialSource {
            id: crate::CredentialSourceId("cleanup-source".into()),
            replacement_of: None,
            workspace_id: "cleanup-workspace".into(),
            kind: CredentialKind::Vault,
            descriptor: None,
            provider_id: None,
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: Some(SecretRef("cleanup-material".into())),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        }
    }

    const PHASES: [CredentialMaterialMutationPhase; 4] = [
        CredentialMaterialMutationPhase::Writing,
        CredentialMaterialMutationPhase::Ready,
        CredentialMaterialMutationPhase::Reclaiming,
        CredentialMaterialMutationPhase::ReclaimingAbort,
    ];

    #[test]
    fn source_cleanup_projection_covers_creation_mutation_and_distinct_replacement() {
        // Causes: C1 create/in-place/cross-id, C2 all phases, C3 absent/before/
        // after/foreign full row, C4 exact/changed durable writer. Constraints:
        // cross-id keeps the predecessor alive and abort expects NEW id absent.
        // Decision rules: R1 foreign writer => reject; R2 Writing/Ready => no
        // completion; R3 Reclaiming => exact after only; R4 ReclaimingAbort =>
        // exact unpublished only; R5 abort rejects published phase. Full heap
        // projections are tested here; Kani proves only their scalar consumer.
        for shape in 0..3 {
            let before = (shape != 0).then(source);
            let mut after = source();
            after.material_ref = Some(SecretRef("replacement-material".into()));
            if shape == 1 {
                after.version = 2;
            }
            if shape == 2 {
                after.id = crate::CredentialSourceId("replacement-source".into());
                after.replacement_of = Some(awaken_credential_contract::CredentialRef {
                    id: before.as_ref().unwrap().id.0.clone(),
                    revision: 1,
                });
            }
            let original = CredentialMutationIntent::prepare(before, after).unwrap();
            let mut foreign_row = original.after.clone();
            foreign_row.workspace_id = "foreign-workspace".into();
            for phase in PHASES {
                let mut request = original.clone();
                request.material_fence.phase = phase;
                for current in [
                    None,
                    request.before.as_ref(),
                    Some(&request.after),
                    Some(&foreign_row),
                ] {
                    for exact in [false, true] {
                        let mut durable = request.clone();
                        if !exact {
                            durable.material_fence.writer_epoch += 1;
                        }
                        let unpublished = if shape == 2 {
                            current.is_none()
                        } else {
                            current == request.before.as_ref()
                        };
                        let expected_complete = exact
                            && match phase {
                                CredentialMaterialMutationPhase::Reclaiming => {
                                    current == Some(&request.after)
                                }
                                CredentialMaterialMutationPhase::ReclaimingAbort => unpublished,
                                _ => false,
                            };
                        assert_eq!(
                            request.admit_completion(&durable, current).is_ok(),
                            expected_complete
                        );
                        assert_eq!(
                            request.admit_abort(&durable, current).is_ok(),
                            exact
                                && unpublished
                                && phase != CredentialMaterialMutationPhase::Reclaiming
                        );
                    }
                }
            }
            // R6 Source-only replay may name the original request or its exact
            // derived abort, but neither bypasses the full unpublished check.
            let abort = original.with_material_reclaiming_abort().unwrap();
            let current = if shape == 2 {
                None
            } else {
                original.before.as_ref()
            };
            assert_eq!(original.admit_abort(&abort, current).unwrap(), abort);
            assert!(original.admit_abort(&abort, Some(&foreign_row)).is_err());
        }
    }

    #[test]
    fn managed_cleanup_requires_both_rows_and_exact_full_envelope() {
        // Causes: C1 all phases; C2 absent/before/after/foreign source and child
        // independently; C3 exact/changed operation, owner, or payload; C4
        // create/update. R1 completion is allowed only for exact authority and
        // the phase's FULL pair, never one matching participant. R2 abort needs
        // full before pair and non-published phase; R3 Managed retry must carry
        // the durable abort itself, preserving the existing port contract.
        let source = source();
        let child = ManagedVaultCredential {
            id: "cleanup-child".into(),
            vault_id: "cleanup-vault".into(),
            workspace_id: source.workspace_id.clone(),
            source_id: source.id.clone(),
            auth: ManagedCredentialAuth::StaticBearer {
                mcp_server_url: "https://mcp.example.test".into(),
            },
            metadata: Default::default(),
            display_name: None,
            revision: 1,
            lifecycle: ManagedCredentialLifecycle::Active,
        };
        let create = PendingManagedCredentialMutation::create(source, child).unwrap();
        let mut next_source = create.after_source.clone();
        next_source.version = 2;
        next_source.material_ref = Some(SecretRef("changed-material".into()));
        let mut next_child = create.after_credential.clone();
        next_child.revision = 2;
        let update = PendingManagedCredentialMutation::change(
            "cleanup-update".into(),
            super::super::ManagedCredentialOperation::Update,
            create.after_source.clone(),
            next_source,
            create.after_credential.clone(),
            next_child,
            true,
        )
        .unwrap();
        for original in [create, update] {
            let mut foreign_source = original.after_source.clone();
            foreign_source.workspace_id = "foreign".into();
            let mut foreign_child = original.after_credential.clone();
            foreign_child.vault_id = "foreign".into();
            for phase in PHASES {
                let mut request = original.clone();
                request.material_fence.phase = phase;
                for source in [
                    None,
                    request.before_source.as_ref(),
                    Some(&request.after_source),
                    Some(&foreign_source),
                ] {
                    for child in [
                        None,
                        request.before_credential.as_ref(),
                        Some(&request.after_credential),
                        Some(&foreign_child),
                    ] {
                        for mutation in 0..4 {
                            let mut durable = request.clone();
                            match mutation {
                                1 => durable.operation_id.push_str("-foreign"),
                                2 => durable.material_fence.writer_epoch += 1,
                                3 => durable.after_credential.display_name = Some("foreign".into()),
                                _ => {}
                            }
                            let before = source == request.before_source.as_ref()
                                && child == request.before_credential.as_ref();
                            let after = source == Some(&request.after_source)
                                && child == Some(&request.after_credential);
                            let complete = mutation == 0
                                && match phase {
                                    CredentialMaterialMutationPhase::Reclaiming => after,
                                    CredentialMaterialMutationPhase::ReclaimingAbort => before,
                                    _ => false,
                                };
                            assert_eq!(
                                request.admit_completion(&durable, source, child).is_ok(),
                                complete
                            );
                            assert_eq!(
                                request.admit_abort(&durable, source, child).is_ok(),
                                mutation == 0
                                    && before
                                    && phase != CredentialMaterialMutationPhase::Reclaiming
                            );
                        }
                    }
                }
            }
            let abort = original.with_material_reclaiming_abort().unwrap();
            assert!(
                original
                    .admit_abort(
                        &abort,
                        original.before_source.as_ref(),
                        original.before_credential.as_ref()
                    )
                    .is_err()
            );
            assert_eq!(
                abort
                    .admit_abort(
                        &abort,
                        original.before_source.as_ref(),
                        original.before_credential.as_ref()
                    )
                    .unwrap(),
                abort
            );
        }
    }
}
