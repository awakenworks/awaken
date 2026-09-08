//! The owner of cleanup admission for both credential publication envelopes.
//! Admission consumes snapshots held under one repository lock/transaction.
//! Material IO runs outside that transaction; observed absence is not an atomic
//! cross-store receipt or certification of an arbitrary SecretStore.

use super::{
    CredentialMaterialMutationPhase, CredentialMutationIntent, PendingManagedCredentialMutation,
};
use crate::{CredentialError, CredentialSource, catalog::ManagedVaultCredential};

/// Read only the candidate delta before Ready publication. Source and Managed
/// recovery share this port decision; each still owns its publication/abort CAS.
/// Missing candidate material selects abort, while an unavailable store retains
/// the WAL for retry. Retained references belong to the prior durable revision.
pub(super) async fn candidate_material_complete(
    before: Option<&CredentialSource>,
    after: &CredentialSource,
    store: &dyn crate::SecretStore,
) -> Result<bool, CredentialError> {
    let mut complete = true;
    for reference in super::new_material_refs(before, after) {
        match store.get(reference).await {
            Ok(_) => {}
            Err(CredentialError::SecretNotFound(_)) => complete = false,
            Err(error) => return Err(error),
        }
    }
    Ok(complete)
}

// One reference-membership decision for both publication envelopes. Shared
// refs are never reclaimed; a distinct replacement keeps its predecessor live.
const fn reclamation_selected(
    phase: CredentialMaterialMutationPhase,
    distinct: bool,
    before: bool,
    after: bool,
) -> Option<bool> {
    match phase {
        CredentialMaterialMutationPhase::Writing | CredentialMaterialMutationPhase::Ready => None,
        CredentialMaterialMutationPhase::Reclaiming => Some(!distinct && before && !after),
        CredentialMaterialMutationPhase::ReclaimingAbort => Some(after && !before),
    }
}

async fn reclaim_material(
    phase: CredentialMaterialMutationPhase,
    distinct: bool,
    before: Option<&CredentialSource>,
    after: &CredentialSource,
    store: &dyn crate::SecretStore,
) -> Result<(), CredentialError> {
    use std::collections::BTreeSet;
    let before = before
        .into_iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<BTreeSet<_>>();
    let after = after.material_refs().collect::<BTreeSet<_>>();
    // Check phase even when both sets are empty; empty IO is not authority to
    // skip Writing/Ready and retire a pending envelope.
    reclamation_selected(phase, distinct, false, false).ok_or_else(|| {
        CredentialError::MutationConflict(
            "credential material cleanup requires a terminal cleanup phase".into(),
        )
    })?;
    let references = before.union(&after).copied().filter(|reference| {
        reclamation_selected(
            phase,
            distinct,
            before.contains(reference),
            after.contains(reference),
        ) == Some(true)
    });
    for reference in references {
        store.delete(reference).await?;
        match store.get(reference).await {
            Err(CredentialError::SecretNotFound(missing)) if missing == reference.0 => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(CredentialError::Storage(
                    "credential material remains after acknowledged deletion".into(),
                ));
            }
        }
    }
    Ok(())
}

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
    /// Execute only this envelope's phase-selected material delta. A returned
    /// success records observed absence, not atomicity with a later WAL CAS or
    /// protection against an arbitrary backend's future write to the same ref.
    pub(super) async fn reclaim_material(
        &self,
        store: &dyn crate::SecretStore,
    ) -> Result<(), CredentialError> {
        self.validate()?;
        reclaim_material(
            self.material_fence.phase,
            self.is_distinct_replacement(),
            self.before.as_ref(),
            &self.after,
            store,
        )
        .await
    }

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
    pub(super) async fn reclaim_material(
        &self,
        store: &dyn crate::SecretStore,
    ) -> Result<(), CredentialError> {
        self.validate()
            .map_err(|error| CredentialError::MutationConflict(error.to_string()))?;
        reclaim_material(
            self.material_fence.phase,
            false,
            self.before_source.as_ref(),
            &self.after_source,
            store,
        )
        .await
    }
    /// Compare the complete pre-publication pair under the caller's repository
    /// lock or transaction. Shared by begin, commit, abort and completion; this
    /// value comparison neither acquires authority nor performs storage IO.
    #[must_use]
    pub fn is_unpublished_at(
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
fn credential_reclamation_never_deletes_shared_or_live_predecessor_material() {
    // C1 all phases, C2 distinct replacement, C3 independent membership in
    // before/after. R1 nonterminal => reject; R2 published => only retired refs,
    // and never a live distinct predecessor; R3 aborted => only candidate refs;
    // R4 shared or unrelated => never delete. No bound is assumed between C2/C3.
    use CredentialMaterialMutationPhase::*;
    let phase = match kani::any::<u8>() % 4 {
        0 => Writing,
        1 => Ready,
        2 => Reclaiming,
        _ => ReclaimingAbort,
    };
    let distinct = kani::any::<bool>();
    let before = kani::any::<bool>();
    let after = kani::any::<bool>();
    let selected = reclamation_selected(phase, distinct, before, after);
    match phase {
        Writing | Ready => assert_eq!(selected, None),
        Reclaiming => assert_eq!(selected, Some(before && !after && !distinct)),
        ReclaimingAbort => assert_eq!(selected, Some(after && !before)),
    }
    if before == after {
        assert_ne!(selected, Some(true));
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
pub(super) mod tests {

    use crate::{InMemorySecretStore, SecretStore};
    use awaken_agent_contract::RedactedString;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    pub(crate) struct FaultyDeleteStore {
        pub(crate) inner: InMemorySecretStore,
        pub(crate) delete_attempts: std::sync::Mutex<Vec<crate::SecretRef>>,
        pub(crate) fail_before_delete: AtomicBool,
        pub(crate) lose_first_response: AtomicBool,
        pub(crate) acknowledge_without_delete: AtomicBool,
        pub(crate) readback_unavailable: AtomicBool,
        pub(crate) wrong_missing_identity: AtomicBool,
    }
    #[async_trait::async_trait]
    impl SecretStore for FaultyDeleteStore {
        async fn put(
            &self,
            r: &crate::SecretRef,
            secret: RedactedString,
        ) -> Result<(), CredentialError> {
            self.inner.put(r, secret).await
        }

        async fn get(&self, r: &crate::SecretRef) -> Result<RedactedString, CredentialError> {
            if self.wrong_missing_identity.load(Ordering::SeqCst) {
                return Err(CredentialError::SecretNotFound("foreign-reference".into()));
            }
            if self.readback_unavailable.load(Ordering::SeqCst) {
                return Err(CredentialError::Storage(
                    "cleanup readback unavailable".into(),
                ));
            }
            self.inner.get(r).await
        }

        async fn delete(&self, r: &crate::SecretRef) -> Result<(), CredentialError> {
            self.delete_attempts.lock().unwrap().push(r.clone());
            if self.fail_before_delete.load(Ordering::SeqCst) {
                return Err(CredentialError::Storage("delete timeout".into()));
            }
            if self.acknowledge_without_delete.load(Ordering::SeqCst) {
                return Ok(());
            }
            self.inner.delete(r).await?;
            if self.lose_first_response.swap(false, Ordering::SeqCst) {
                return Err(CredentialError::Storage("delete response lost".into()));
            }
            Ok(())
        }
    }

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

    #[tokio::test]
    async fn candidate_readiness_uses_only_new_refs_and_retains_unknown_outcomes() {
        // Causes: retained primary moved to aux, new material present/missing,
        // store unavailable, no new refs. R1 only retained => true with no IO;
        // R2 candidate present => true even if retained material is absent;
        // R3 candidate missing => false (abort); R4 unavailable => error (retry).
        // Existing Source/Managed recovery tables own the resulting WAL/CAS
        // outcomes; this table isolates their shared external-read decision.
        for new_ref in [false, true] {
            for present in [false, true] {
                for unavailable in [false, true] {
                    let store = FaultyDeleteStore::default();
                    let before = source();
                    let mut after = before.clone();
                    after.material_ref = None;
                    after
                        .auxiliary_material_refs
                        .insert("retained".into(), before.material_ref.clone().unwrap());
                    let candidate = SecretRef("candidate".into());
                    if new_ref {
                        after.material_ref = Some(candidate.clone());
                    }
                    if present {
                        store
                            .inner
                            .put(&candidate, RedactedString::new("fixture"))
                            .await
                            .unwrap();
                    }
                    store
                        .readback_unavailable
                        .store(unavailable, Ordering::SeqCst);
                    let result = candidate_material_complete(Some(&before), &after, &store).await;
                    if new_ref && unavailable {
                        assert!(matches!(result, Err(CredentialError::Storage(_))), "R4");
                    } else {
                        assert_eq!(result.unwrap(), !new_ref || present, "R1-R3");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn material_delta_covers_phase_membership_and_distinct_predecessors() {
        // Decision table: 8 before sets x 8 after sets x 4 phases x 2 lineage
        // shapes. Include a duplicated primary/aux ref and an unrelated ref.
        // R1 Writing/Ready => zero deletes; R2 published => before-minus-after
        // except distinct predecessor; R3 abort => after-minus-before. Every
        // surviving physical ref is checked, not only the returned status.
        // R4 selected refs are deleted once each in byte-lexical identity order;
        // primary/aux aliases never create a second physical delete.
        fn with_refs(mask: u8) -> CredentialSource {
            let mut value = source();
            value.material_ref = (mask & 1 != 0).then(|| SecretRef("ref-0".into()));
            for index in 0..3 {
                if mask & (1 << index) != 0 {
                    value
                        .auxiliary_material_refs
                        .insert(index.to_string(), SecretRef(format!("ref-{index}")));
                }
            }
            value
        }
        for before in 0u8..8 {
            for after in 0u8..8 {
                for phase in PHASES {
                    for distinct in [false, true] {
                        let store = FaultyDeleteStore::default();
                        for index in 0..4 {
                            store
                                .put(
                                    &SecretRef(format!("ref-{index}")),
                                    RedactedString::new("fixture"),
                                )
                                .await
                                .unwrap();
                        }
                        let result = reclaim_material(
                            phase,
                            distinct,
                            Some(&with_refs(before)),
                            &with_refs(after),
                            &store,
                        )
                        .await;
                        let terminal = matches!(
                            phase,
                            CredentialMaterialMutationPhase::Reclaiming
                                | CredentialMaterialMutationPhase::ReclaimingAbort
                        );
                        assert_eq!(result.is_ok(), terminal);
                        let removed = match phase {
                            CredentialMaterialMutationPhase::Reclaiming if !distinct => {
                                before & !after
                            }
                            CredentialMaterialMutationPhase::ReclaimingAbort => after & !before,
                            _ => 0,
                        };
                        let expected_order = (0..4)
                            .filter(|index| removed & (1 << index) != 0)
                            .map(|index| SecretRef(format!("ref-{index}")))
                            .collect::<Vec<_>>();
                        assert_eq!(*store.delete_attempts.lock().unwrap(), expected_order, "R4");
                        for index in 0..4 {
                            assert_eq!(
                                store.get(&SecretRef(format!("ref-{index}"))).await.is_ok(),
                                removed & (1 << index) == 0
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn unconfirmed_deletion_retains_source_intent_until_exact_absence() {
        use crate::repo::{CredentialRepo, InMemoryCredentialRepo, recover_credential_mutations};
        // C1 acknowledged delete which does nothing; C2 failed readback after
        // actual deletion or absence naming another ref; C3 adapter recovered.
        // Cross both published retirement and abort. R1 C1|C2 => error + same WAL;
        // R2 C3 + exact absence => retire once; R3 retry after retirement => 0.
        // Real source recovery, not a hand-implemented completion, is exercised.
        for published in [false, true] {
            for fault in 0..3 {
                let store = FaultyDeleteStore::default();
                let repo = InMemoryCredentialRepo::new();
                let mut after = source();
                let before = published.then(|| after.clone());
                if let Some(before) = &before {
                    repo.put(before.clone()).await.unwrap();
                    after.version = 2;
                    after.material_ref = None;
                }
                let intent = CredentialMutationIntent::prepare(before, after).unwrap();
                repo.begin_mutation(intent.clone()).await.unwrap();
                let abort = if published {
                    repo.apply_mutation(&intent).await.unwrap()
                } else {
                    repo.abort_mutation(&intent).await.unwrap()
                };
                let reference = if published {
                    abort.before.as_ref().unwrap().material_ref.as_ref()
                } else {
                    abort.after.material_ref.as_ref()
                }
                .unwrap();
                store
                    .inner
                    .put(reference, RedactedString::new("cleanup-fixture"))
                    .await
                    .unwrap();
                store
                    .acknowledge_without_delete
                    .store(fault == 0, Ordering::SeqCst);
                store
                    .readback_unavailable
                    .store(fault == 1, Ordering::SeqCst);
                store
                    .wrong_missing_identity
                    .store(fault == 2, Ordering::SeqCst);
                assert!(
                    recover_credential_mutations(&store, &repo).await.is_err(),
                    "R1"
                );
                assert_eq!(
                    repo.pending_mutations().await.unwrap(),
                    vec![abort],
                    "R1 retained"
                );
                store
                    .acknowledge_without_delete
                    .store(false, Ordering::SeqCst);
                store.readback_unavailable.store(false, Ordering::SeqCst);
                store.wrong_missing_identity.store(false, Ordering::SeqCst);
                assert_eq!(
                    recover_credential_mutations(&store, &repo).await.unwrap(),
                    1,
                    "R2"
                );
                assert_eq!(
                    recover_credential_mutations(&store, &repo).await.unwrap(),
                    0,
                    "R3"
                );
            }
        }
    }

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
