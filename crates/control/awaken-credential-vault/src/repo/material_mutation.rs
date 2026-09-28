use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{CredentialError, CredentialSource};

/// Durable process phase around the external SecretStore participant. This is
/// the sole phase vocabulary for source-only and Managed pair mutations.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMaterialMutationPhase {
    #[default]
    Writing,
    Ready,
    Reclaiming,
    ReclaimingAbort,
}

/// Observation supplied to the one material-recovery planner. The command
/// owner and the supervisor use the same vocabulary, so a failed/unknown
/// external effect cannot be reinterpreted as successful cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialMaterialRecoverySignal {
    SupervisorTick,
    OwnerWritesComplete,
    OwnerWriteFailed,
    UnpublishedConflict,
    ReadyMaterialComplete,
    ReadyMaterialIncomplete,
    ReadyMaterialUnknown,
    CleanupConfirmedAbsent,
    CleanupUnknown,
}

/// Sole phase/effect-to-next-work plan for source-only and Managed envelopes.
/// `RetainResponsibility` is an intentional durable outcome, not successful
/// completion: only `RetireIntent` permits removal of the WAL row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialMaterialRecoveryPlan {
    RetainResponsibility,
    MarkReady,
    InspectReadyMaterial,
    ClaimExpiredWriterForAbort,
    PublishReady,
    AbortUnpublished,
    CleanupPublished,
    CleanupAborted,
    RetireIntent,
}

/// A material writer has this long to finish its external write before a
/// reconciler may atomically fence it and take ownership.
pub const CREDENTIAL_MATERIAL_WRITER_LEASE_MS: u64 = 120_000;

const CREDENTIAL_MATERIAL_MUTATION_FORMAT_VERSION: u8 = 1;

/// Sole durable writer/lease/attempt/phase authority shared by both credential
/// publication envelopes. It is flattened into each envelope's existing JSON
/// row, so their distinct local transactions do not become distinct external-
/// effect state machines.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CredentialMaterialMutationFence {
    #[serde(default)]
    pub(super) format_version: u8,
    #[serde(default)]
    pub phase: CredentialMaterialMutationPhase,
    #[serde(default)]
    pub(super) attempt_id: String,
    #[serde(default)]
    pub writer_token: String,
    #[serde(default)]
    pub writer_epoch: u64,
    #[serde(default)]
    pub writer_lease_expires_at_unix_ms: u64,
}

impl Default for CredentialMaterialMutationFence {
    fn default() -> Self {
        Self {
            format_version: 0,
            phase: CredentialMaterialMutationPhase::Writing,
            attempt_id: String::new(),
            writer_token: String::new(),
            writer_epoch: 0,
            writer_lease_expires_at_unix_ms: 0,
        }
    }
}

impl CredentialMaterialMutationFence {
    pub(super) fn fresh(writes_new_material: bool) -> Result<Self, CredentialError> {
        if !writes_new_material {
            return Ok(Self {
                format_version: CREDENTIAL_MATERIAL_MUTATION_FORMAT_VERSION,
                phase: CredentialMaterialMutationPhase::Ready,
                ..Self::default()
            });
        }
        let now_unix_ms = credential_material_now_unix_ms()?;
        let writer_token = uuid::Uuid::new_v4().simple().to_string();
        Ok(Self {
            format_version: CREDENTIAL_MATERIAL_MUTATION_FORMAT_VERSION,
            phase: CredentialMaterialMutationPhase::Writing,
            attempt_id: writer_token.clone(),
            writer_token,
            writer_epoch: 1,
            writer_lease_expires_at_unix_ms: now_unix_ms
                .checked_add(CREDENTIAL_MATERIAL_WRITER_LEASE_MS)
                .ok_or_else(|| {
                    CredentialError::MutationConflict(
                        "credential material writer lease deadline overflowed".into(),
                    )
                })?,
        })
    }

    #[must_use]
    pub fn is_current_format(&self) -> bool {
        self.format_version == CREDENTIAL_MATERIAL_MUTATION_FORMAT_VERSION
    }

    #[must_use]
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    #[must_use]
    fn has_valid_writer_owner(&self) -> bool {
        !self.writer_token.is_empty()
            && self.writer_epoch > 0
            && self.writer_lease_expires_at_unix_ms > 0
    }

    pub fn claim_after_expiry(
        &self,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
    ) -> Result<Option<Self>, CredentialError> {
        self.claim_after_expiry_with_token(now_unix_ms, lease_expires_at_unix_ms, || {
            uuid::Uuid::new_v4().simple().to_string()
        })
    }

    // Keep entropy at the edge; the production wrapper and bounded proof use
    // this same transition. The token factory remains lazy, including error
    // precedence and the pre-existing epoch-exhaustion allocation behavior.
    fn claim_after_expiry_with_token(
        &self,
        now_unix_ms: u64,
        lease_expires_at_unix_ms: u64,
        new_writer_token: impl FnOnce() -> String,
    ) -> Result<Option<Self>, CredentialError> {
        if self.phase != CredentialMaterialMutationPhase::Writing
            || self.writer_lease_expires_at_unix_ms > now_unix_ms
        {
            return Ok(None);
        }
        if lease_expires_at_unix_ms <= now_unix_ms {
            return Err(CredentialError::MutationConflict(
                "credential material recovery lease must expire after claim time".into(),
            ));
        }
        let mut claimed = self.clone();
        claimed.writer_token = new_writer_token();
        // Epoch one is reserved for the original writer. A legacy zero epoch
        // must skip it; recovery safety cannot depend on UUID non-collision.
        claimed.writer_epoch = self.writer_epoch.max(1).checked_add(1).ok_or_else(|| {
            CredentialError::MutationConflict(
                "credential material writer epoch is exhausted".into(),
            )
        })?;
        claimed.writer_lease_expires_at_unix_ms = lease_expires_at_unix_ms;
        Ok(Some(claimed))
    }

    /// Project both envelopes through one material-membership/attempt rule.
    /// A retained reference is not a new write, regardless of its slot or suffix.
    /// Empty deltas are bound only when the attempt itself is nonempty; fresh
    /// and recovery validation consume these facts with their existing rules.
    pub(super) fn material_input_facts(
        &self,
        before: Option<&CredentialSource>,
        after: &CredentialSource,
    ) -> (bool, bool) {
        let mut new_refs = new_material_refs(before, after);
        let Some(first_new) = new_refs.next() else {
            return (false, !self.attempt_id().is_empty());
        };
        // Keep the first delta item instead of traversing its prefix again to
        // derive the second fact. Both facts consume one membership traversal.
        // Check the exact suffix without allocating a formatted copy of the
        // attempt. strip_suffix keeps near-matching attempt prefixes distinct.
        let bound_to_attempt = |reference: &crate::SecretRef| {
            reference
                .0
                .strip_suffix(self.attempt_id())
                .is_some_and(|prefix| prefix.ends_with(":attempt:"))
        };
        let new_refs_bound_to_attempt = !self.attempt_id().is_empty()
            && bound_to_attempt(first_new)
            && new_refs.all(bound_to_attempt);
        (true, new_refs_bound_to_attempt)
    }

    pub(super) fn validate(
        &self,
        writes_new_material: bool,
        new_refs_bound_to_attempt: bool,
    ) -> Result<(), CredentialError> {
        if self.format_version > CREDENTIAL_MATERIAL_MUTATION_FORMAT_VERSION
            || !credential_material_attempt_admitted(
                self.format_version != 0,
                self.phase == CredentialMaterialMutationPhase::Writing,
                writes_new_material,
                self.has_valid_writer_owner(),
                new_refs_bound_to_attempt,
            )
        {
            return Err(CredentialError::MutationConflict(
                "invalid credential material mutation fence".into(),
            ));
        }
        Ok(())
    }

    /// Admit only the one canonical shape produced by `fresh`. Serde fields
    /// are recovery transport, not authority: a caller cannot start a new
    /// material-writing command in `Ready`, forge a takeover epoch, or attach
    /// a writer to a material-free command.
    pub(super) fn validate_fresh_for_begin(
        &self,
        writes_new_material: bool,
        new_refs_bound_to_attempt: bool,
    ) -> Result<(), CredentialError> {
        self.validate(writes_new_material, new_refs_bound_to_attempt)?;
        let fresh_shape = if writes_new_material {
            self.phase == CredentialMaterialMutationPhase::Writing
                && !self.attempt_id.is_empty()
                && self.writer_token == self.attempt_id
                && self.writer_epoch == 1
                && self.writer_lease_expires_at_unix_ms > credential_material_now_unix_ms()?
        } else {
            self.phase == CredentialMaterialMutationPhase::Ready
                && self.attempt_id.is_empty()
                && self.writer_token.is_empty()
                && self.writer_epoch == 0
                && self.writer_lease_expires_at_unix_ms == 0
        };
        if self.is_current_format() && fresh_shape {
            Ok(())
        } else {
            Err(CredentialError::MutationConflict(
                "credential material mutation does not have canonical fresh command shape".into(),
            ))
        }
    }

    pub(super) fn ready(&self) -> Result<Self, CredentialError> {
        if self.phase != CredentialMaterialMutationPhase::Writing
            || !self.has_valid_writer_owner()
            || self.writer_epoch != 1
            || self.writer_token != self.attempt_id
        {
            return Err(CredentialError::MutationConflict(
                "credential material mutation is not the original owned Writing work".into(),
            ));
        }
        let mut ready = self.clone();
        ready.phase = CredentialMaterialMutationPhase::Ready;
        Ok(ready)
    }

    pub(super) fn reclaiming(&self) -> Result<Self, CredentialError> {
        if self.phase != CredentialMaterialMutationPhase::Ready {
            return Err(CredentialError::MutationConflict(
                "credential material mutation is not ready to publish".into(),
            ));
        }
        let mut reclaiming = self.clone();
        reclaiming.phase = CredentialMaterialMutationPhase::Reclaiming;
        Ok(reclaiming)
    }

    pub(super) fn reclaiming_abort(&self) -> Result<Self, CredentialError> {
        if self.phase == CredentialMaterialMutationPhase::ReclaimingAbort {
            return Ok(self.clone());
        }
        if !matches!(
            self.phase,
            CredentialMaterialMutationPhase::Writing | CredentialMaterialMutationPhase::Ready
        ) {
            return Err(CredentialError::MutationConflict(
                "credential material mutation cannot enter abort cleanup from this phase".into(),
            ));
        }
        let mut reclaiming = self.clone();
        reclaiming.phase = CredentialMaterialMutationPhase::ReclaimingAbort;
        Ok(reclaiming)
    }
}

#[must_use]
pub(super) const fn credential_material_recovery_plan(
    fence: &CredentialMaterialMutationFence,
    now_unix_ms: u64,
    signal: CredentialMaterialRecoverySignal,
) -> CredentialMaterialRecoveryPlan {
    use CredentialMaterialRecoveryPlan::*;
    use CredentialMaterialRecoverySignal::*;
    match fence.phase {
        CredentialMaterialMutationPhase::Writing => match signal {
            OwnerWritesComplete => MarkReady,
            OwnerWriteFailed | UnpublishedConflict => AbortUnpublished,
            SupervisorTick if fence.writer_lease_expires_at_unix_ms <= now_unix_ms => {
                ClaimExpiredWriterForAbort
            }
            _ => RetainResponsibility,
        },
        CredentialMaterialMutationPhase::Ready => match signal {
            SupervisorTick => InspectReadyMaterial,
            OwnerWritesComplete | ReadyMaterialComplete => PublishReady,
            OwnerWriteFailed | UnpublishedConflict | ReadyMaterialIncomplete => AbortUnpublished,
            _ => RetainResponsibility,
        },
        CredentialMaterialMutationPhase::Reclaiming => match signal {
            CleanupConfirmedAbsent => RetireIntent,
            CleanupUnknown => RetainResponsibility,
            _ => CleanupPublished,
        },
        CredentialMaterialMutationPhase::ReclaimingAbort => match signal {
            CleanupConfirmedAbsent => RetireIntent,
            CleanupUnknown => RetainResponsibility,
            _ => CleanupAborted,
        },
    }
}

#[must_use]
const fn credential_material_attempt_admitted(
    current_format: bool,
    writing: bool,
    writes_new_material: bool,
    valid_owner: bool,
    new_refs_bound_to_attempt: bool,
) -> bool {
    !current_format
        || (((!writing && !writes_new_material) || valid_owner)
            && (!writes_new_material || new_refs_bound_to_attempt))
}

/// One borrowed candidate delta for preparation, admission and Ready recovery.
/// Retention is reference identity, independent of slot. Candidate order and
/// aliases are preserved because read recovery observes each candidate in order.
pub(super) fn new_material_refs<'a>(
    before: Option<&'a CredentialSource>,
    after: &'a CredentialSource,
) -> impl Iterator<Item = &'a crate::SecretRef> {
    // Extend avoids the temporary Vec/sort in BTreeSet::from_iter.
    let mut before_refs = BTreeSet::new();
    before_refs.extend(before.into_iter().flat_map(CredentialSource::material_refs));
    after
        .material_refs()
        .filter(move |reference| !before_refs.contains(reference))
}

pub(super) fn namespace_new_material_refs(
    before: Option<&CredentialSource>,
    after: &mut CredentialSource,
    attempt_id: &str,
) {
    let before_refs = before
        .into_iter()
        .flat_map(CredentialSource::material_refs)
        .cloned()
        .collect::<BTreeSet<_>>();
    let suffix = format!(":attempt:{attempt_id}");
    if let Some(reference) = after.material_ref.as_mut()
        && !before_refs.contains(reference)
        && !reference.0.ends_with(&suffix)
    {
        reference.0.push_str(&suffix);
    }
    for reference in after.auxiliary_material_refs.values_mut() {
        if !before_refs.contains(reference) && !reference.0.ends_with(&suffix) {
            reference.0.push_str(&suffix);
        }
    }
}

pub(super) fn fence_material_writes<T>(
    material_fence: &CredentialMaterialMutationFence,
    before: Option<&CredentialSource>,
    after: &CredentialSource,
    materials: &mut [(crate::SecretRef, T)],
) -> Result<(), CredentialError> {
    if materials.is_empty() {
        return Ok(());
    }
    if !material_fence.has_valid_writer_owner() || material_fence.attempt_id().is_empty() {
        return Err(CredentialError::MutationConflict(
            "credential material writes require one exact Writing owner".into(),
        ));
    }
    let before_refs = before
        .into_iter()
        .flat_map(CredentialSource::material_refs)
        .collect::<BTreeSet<_>>();
    let suffix = format!(":attempt:{}", material_fence.attempt_id());
    for (reference, _) in materials {
        let fenced = if reference.0.ends_with(&suffix) {
            reference.clone()
        } else {
            crate::SecretRef(format!("{}{suffix}", reference.0))
        };
        if before_refs.contains(&fenced)
            || !after.material_refs().any(|candidate| candidate == &fenced)
        {
            return Err(CredentialError::MutationConflict(
                "credential material write is not bound to its durable attempt".into(),
            ));
        }
        *reference = fenced;
    }
    Ok(())
}

fn attempt_namespace_base(reference: &crate::SecretRef) -> Option<&str> {
    let (base, attempt) = reference.0.rsplit_once(":attempt:")?;
    (attempt.len() == 32 && attempt.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(base)
}

/// Compare one logical material slot while treating the randomly generated
/// attempt suffix as physical publication state. This is the sole comparison
/// used by pending-command attachment and published idempotency replay.
pub(super) fn logical_material_ref_matches(
    actual: Option<&crate::SecretRef>,
    expected: Option<&crate::SecretRef>,
) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(expected)) => {
            let actual_base = attempt_namespace_base(actual);
            let expected_base = attempt_namespace_base(expected);
            actual == expected
                || actual_base == Some(expected.0.as_str())
                || expected_base == Some(actual.0.as_str())
                || (actual_base.is_some() && actual_base == expected_base)
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

/// Compare the complete logical Source projection while ignoring only the
/// attempt-specific physical material namespace. Clearing material fields from
/// cloned rows keeps every current and future non-material field in the derived
/// equality check instead of maintaining a second hand-written field list.
pub(super) fn logical_credential_source_matches(
    actual: &CredentialSource,
    expected: &CredentialSource,
) -> bool {
    let primary_matches =
        logical_material_ref_matches(actual.material_ref.as_ref(), expected.material_ref.as_ref());
    let auxiliary_matches = actual.auxiliary_material_refs.len()
        == expected.auxiliary_material_refs.len()
        && expected
            .auxiliary_material_refs
            .iter()
            .all(|(slot, reference)| {
                actual
                    .auxiliary_material_refs
                    .get(slot)
                    .is_some_and(|actual| {
                        logical_material_ref_matches(Some(actual), Some(reference))
                    })
            });
    let mut actual_projection = actual.clone();
    actual_projection.material_ref = None;
    actual_projection.auxiliary_material_refs.clear();
    let mut expected_projection = expected.clone();
    expected_projection.material_ref = None;
    expected_projection.auxiliary_material_refs.clear();
    primary_matches && auxiliary_matches && actual_projection == expected_projection
}

/// Compare the stable identity of an idempotent create after later lifecycle
/// transitions may have advanced the durable revision. Material references keep
/// their logical comparison above; every other source field remains exact.
pub(super) fn idempotent_credential_source_matches(
    actual: &CredentialSource,
    expected: &CredentialSource,
) -> bool {
    let mut actual_identity = actual.clone();
    actual_identity.status = expected.status;
    actual_identity.version = expected.version;
    logical_credential_source_matches(&actual_identity, expected)
}

pub(super) fn credential_material_now_unix_ms() -> Result<u64, CredentialError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CredentialError::Storage(format!("credential clock: {error}")))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| CredentialError::Storage("credential clock overflowed u64".into()))
}

#[cfg(any(test, kani))]
mod material_input_proofs {
    use super::*;

    // Indices are a reference oracle, not another production membership path.
    // 0 absent; 1 unbound; 2/3 bound to a; 4 foreign attempt; 5 prefix-only match.
    fn reference(index: u8) -> Option<crate::SecretRef> {
        // Allocate each literal before merging the branches: the same six
        // classes avoid symbolic allocation lengths in the bounded heap proof.
        let value = match index {
            0 => return None,
            1 => String::from("r"),
            2 => String::from("r:attempt:a"),
            3 => String::from("s:attempt:a"),
            4 => String::from("r:attempt:b"),
            5 => String::from("r:attempt:aa"),
            _ => unreachable!("reference index outside the decision table"),
        };
        Some(crate::SecretRef(value))
    }

    fn source_with_slots(slots: [u8; 2]) -> CredentialSource {
        let mut source = super::super::publication_proofs::source();
        source.material_ref = reference(slots[0]);
        if let Some(reference) = reference(slots[1]) {
            source.auxiliary_material_refs.insert("k".into(), reference);
        }
        source
    }

    fn check_case(slots: [u8; 4], has_before: bool, has_attempt: bool) {
        // Causes: independent primary/auxiliary slots on both rows, absent
        // predecessor, absent attempt, retained/moved/duplicate references,
        // bound/foreign/prefix-only suffix. Effects: new-write and exact-bound
        // facts. R1 only after-minus-before is new, regardless of slot; R2
        // all new refs must bind to the nonempty attempt; R3 empty delta is
        // vacuously bound only with a nonempty attempt. The indexed oracle
        // never calls BTreeSet, material_refs, strip_suffix or ends_with from production.
        // R4 candidate order and repeated new aliases survive filtering; Ready
        // recovery must retain its missing-then-unavailable observation order.
        let before = has_before.then(|| source_with_slots([slots[0], slots[1]]));
        let after = source_with_slots([slots[2], slots[3]]);
        let fence = CredentialMaterialMutationFence {
            attempt_id: if has_attempt {
                "a".into()
            } else {
                String::new()
            },
            ..Default::default()
        };
        let mut expected_new = false;
        let mut expected_bound = has_attempt;
        #[cfg(test)]
        let mut expected_delta = Vec::new();
        for index in [slots[2], slots[3]] {
            let is_new = index != 0 && (!has_before || (index != slots[0] && index != slots[1]));
            expected_new |= is_new;
            #[cfg(test)]
            if is_new {
                expected_delta.push(reference(index).unwrap());
            }
            if is_new && index != 2 && index != 3 {
                expected_bound = false;
            }
        }
        #[cfg(test)]
        assert_eq!(
            new_material_refs(before.as_ref(), &after)
                .cloned()
                .collect::<Vec<_>>(),
            expected_delta,
            "R4: exact ordered candidate delta"
        );
        assert_eq!(
            fence.material_input_facts(before.as_ref(), &after),
            (expected_new, expected_bound)
        );
    }

    #[test]
    fn material_input_facts_cover_slots_retention_and_attempt_binding() {
        for encoded in 0..1296u16 {
            let slots = [
                (encoded % 6) as u8,
                ((encoded / 6) % 6) as u8,
                ((encoded / 36) % 6) as u8,
                ((encoded / 216) % 6) as u8,
            ];
            for has_before in [false, true] {
                for has_attempt in [false, true] {
                    check_case(slots, has_before, has_attempt);
                }
            }
        }
    }

    #[test]
    fn borrowed_binding_matches_the_physical_namespace_for_variable_tokens() {
        // Causes: empty, short, UUID-shaped, separator-bearing and Unicode
        // attempt ids; exact/missing/near suffix; candidate first or after a
        // valid reference. R1 any new reference => writes; R2 empty attempt =>
        // unbound; R3 every new ref must match the exact physical namespace.
        // The oracle uses the existing namespace wire grammar and ends_with,
        // independently of the production strip_suffix + separator decision.
        for attempt in [
            "",
            "a",
            "aa",
            "0123456789abcdef0123456789abcdef",
            ":attempt:",
            "é",
            "🗝",
        ] {
            let suffix = format!(":attempt:{attempt}");
            let canonical = format!("valid{suffix}");
            for candidate in [
                format!("candidate{suffix}"),
                format!("candidate{suffix}x"),
                format!("candidate:attemptX:{attempt}"),
                format!("candidate{suffix}{suffix}"),
                attempt.to_owned(),
                String::new(),
            ] {
                let expected = !attempt.is_empty() && candidate.ends_with(&suffix);
                for candidate_first in [false, true] {
                    let mut after = source_with_slots([0, 0]);
                    let (primary, auxiliary) = if candidate_first {
                        (&candidate, &canonical)
                    } else {
                        (&canonical, &candidate)
                    };
                    after.material_ref = Some(crate::SecretRef(primary.clone()));
                    after
                        .auxiliary_material_refs
                        .insert("k".into(), crate::SecretRef(auxiliary.clone()));
                    let fence = CredentialMaterialMutationFence {
                        attempt_id: attempt.into(),
                        ..Default::default()
                    };
                    assert_eq!(fence.material_input_facts(None, &after), (true, expected));
                }
            }
        }
    }

    #[cfg(kani)]
    fn check_partition<const HAS_BEFORE: bool>() {
        // The no-predecessor proof retains all six classes in both candidate
        // slots and both attempt states. The shared executable table also
        // covers predecessor presence; that heap proof remains outstanding.
        // The proof calls the actual production implementation without stubs.
        // Arbitrary strings/collection sizes and external material IO remain
        // outside this bounded heap proof. Collection loops use unwind 3; CI
        // gives builtin memcmp its own bound 16 for the longer attempt suffix.
        // Every loop retains its unwinding assertion.
        let slots: [u8; 4] = kani::any();
        // Direct tuple constraints avoid increasing every collection loop's
        // bound merely to iterate over the four independent input indices.
        kani::assume(slots[0] < 6 && slots[1] < 6 && slots[2] < 6 && slots[3] < 6);
        check_case(slots, HAS_BEFORE, kani::any());
    }

    #[cfg(kani)]
    /// @obligation credential.managed_abort_cleanup_remains_durable_until_unpublished_material_is_removed
    /// @obligation credential.managed_material_admission_predicate_requires_owner_and_namespace_match
    /// @kani profile=minisat-unwind-memcmp-16-v1
    #[kani::proof]
    #[kani::unwind(3)]
    fn credential_material_input_facts_without_predecessor() {
        check_partition::<false>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(
        phase: CredentialMaterialMutationPhase,
        writer_lease_expires_at_unix_ms: u64,
    ) -> CredentialMaterialMutationFence {
        CredentialMaterialMutationFence {
            format_version: CREDENTIAL_MATERIAL_MUTATION_FORMAT_VERSION,
            phase,
            attempt_id: "attempt".into(),
            writer_token: "writer".into(),
            writer_epoch: 1,
            writer_lease_expires_at_unix_ms,
        }
    }

    #[test]
    fn recovery_plan_decision_table_is_shared_by_both_envelopes() {
        /* Causes: C1 phase; C2 Writing lease live; C3 command/supervisor
         * observation. Effects: R1 live Writing tick retains its WAL; R2 expired
         * Writing is claimed for abort; R3 successful owner writes mark Ready;
         * R4 only confirmed-complete Ready publishes; R5 incomplete Ready
         * aborts; R6 unknown Ready retains; R7 terminal phases clean; R8 only
         * confirmed absence retires the WAL. Both envelope services consume
         * this table; SecretStore/SQL truth remains outside the classifier. */
        let now = 10;
        let cases = [
            (
                fence(CredentialMaterialMutationPhase::Writing, now + 1),
                CredentialMaterialRecoverySignal::SupervisorTick,
                CredentialMaterialRecoveryPlan::RetainResponsibility,
            ),
            (
                fence(CredentialMaterialMutationPhase::Writing, now),
                CredentialMaterialRecoverySignal::SupervisorTick,
                CredentialMaterialRecoveryPlan::ClaimExpiredWriterForAbort,
            ),
            (
                fence(CredentialMaterialMutationPhase::Writing, now + 1),
                CredentialMaterialRecoverySignal::OwnerWritesComplete,
                CredentialMaterialRecoveryPlan::MarkReady,
            ),
            (
                fence(CredentialMaterialMutationPhase::Ready, 0),
                CredentialMaterialRecoverySignal::ReadyMaterialComplete,
                CredentialMaterialRecoveryPlan::PublishReady,
            ),
            (
                fence(CredentialMaterialMutationPhase::Ready, 0),
                CredentialMaterialRecoverySignal::ReadyMaterialIncomplete,
                CredentialMaterialRecoveryPlan::AbortUnpublished,
            ),
            (
                fence(CredentialMaterialMutationPhase::Ready, 0),
                CredentialMaterialRecoverySignal::ReadyMaterialUnknown,
                CredentialMaterialRecoveryPlan::RetainResponsibility,
            ),
            (
                fence(CredentialMaterialMutationPhase::Reclaiming, 0),
                CredentialMaterialRecoverySignal::SupervisorTick,
                CredentialMaterialRecoveryPlan::CleanupPublished,
            ),
            (
                fence(CredentialMaterialMutationPhase::ReclaimingAbort, 0),
                CredentialMaterialRecoverySignal::SupervisorTick,
                CredentialMaterialRecoveryPlan::CleanupAborted,
            ),
            (
                fence(CredentialMaterialMutationPhase::ReclaimingAbort, 0),
                CredentialMaterialRecoverySignal::CleanupConfirmedAbsent,
                CredentialMaterialRecoveryPlan::RetireIntent,
            ),
            (
                fence(CredentialMaterialMutationPhase::Reclaiming, 0),
                CredentialMaterialRecoverySignal::CleanupUnknown,
                CredentialMaterialRecoveryPlan::RetainResponsibility,
            ),
        ];
        for (fence, signal, expected) in cases {
            assert_eq!(
                credential_material_recovery_plan(&fence, now, signal),
                expected
            );
        }
    }

    #[test]
    fn expired_writer_takeover_can_abort_but_never_publish_original_material() {
        // C1 before/at expiry; C2 valid/expired successor lease; C3 epoch
        // available/exhausted. R1 live => no claim; R2 invalid lease or epoch
        // overflow => error; R3 exact expiry => successor may abort, never
        // Ready/publish. The original attempt namespace survives takeover;
        // repository CAS, not this value operation, retires the old authority.
        let mut original = fence(CredentialMaterialMutationPhase::Writing, 10);
        original.writer_token = original.attempt_id.clone();
        assert!(original.claim_after_expiry(9, 20).unwrap().is_none());
        assert!(original.claim_after_expiry(10, 10).is_err());
        let claimed = original.claim_after_expiry(10, 20).unwrap().unwrap();
        assert_eq!(claimed.writer_epoch, 2);
        assert_eq!(claimed.attempt_id, original.attempt_id);
        assert_eq!(claimed.writer_lease_expires_at_unix_ms, 20);
        assert!(claimed.ready().is_err());
        assert!(claimed.reclaiming().is_err());
        let aborted = claimed.reclaiming_abort().unwrap();
        assert_eq!(aborted.reclaiming_abort().unwrap(), aborted);
        assert!(aborted.ready().is_err());
        assert!(aborted.reclaiming().is_err());
        let published = original.ready().unwrap().reclaiming().unwrap();
        assert!(published.reclaiming_abort().is_err());
        original.writer_epoch = u64::MAX;
        assert!(original.claim_after_expiry(10, 20).is_err());

        // R4: a valid legacy epoch-zero fence may retain its attempt ID.
        // Force a repeated (valid UUID) token rather than trusting random
        // uniqueness: recovery must not regain the reserved epoch-one writer.
        let legacy = CredentialMaterialMutationFence {
            format_version: 0,
            phase: CredentialMaterialMutationPhase::Writing,
            attempt_id: "0000000000004000800000000000000a".into(),
            ..Default::default()
        };
        legacy.validate(true, true).unwrap();
        assert!(legacy.ready().is_err());
        let claimed = legacy
            .claim_after_expiry_with_token(0, 1, || legacy.attempt_id.clone())
            .unwrap()
            .unwrap();
        assert!(
            claimed.ready().is_err(),
            "R4 legacy takeover cannot become the original writer"
        );
        assert_eq!(claimed.writer_epoch, 2);
        assert_eq!(claimed.attempt_id, legacy.attempt_id);
        assert_eq!(claimed.writer_token, legacy.attempt_id);
        claimed.validate(true, true).unwrap();
        assert!(claimed.reclaiming().is_err());
        assert_eq!(
            claimed.reclaiming_abort().unwrap().phase,
            CredentialMaterialMutationPhase::ReclaimingAbort
        );
    }
}

#[cfg(kani)]
/// @obligation credential.managed_material_admission_predicate_requires_owner_and_namespace_match
/// @obligation credential.material_is_always_published_or_intent_tracked
/// @obligation credential.visible_row_always_has_material
#[kani::proof]
fn credential_material_attempt_requires_owner_and_exact_physical_namespace() {
    let writing = kani::any::<bool>();
    let writes_new_material = kani::any::<bool>();
    let valid_owner = kani::any::<bool>();
    let new_refs_bound_to_attempt = kani::any::<bool>();
    let admitted = credential_material_attempt_admitted(
        true,
        writing,
        writes_new_material,
        valid_owner,
        new_refs_bound_to_attempt,
    );
    if admitted && (writing || writes_new_material) {
        assert!(valid_owner);
    }
    if admitted && writes_new_material {
        assert!(new_refs_bound_to_attempt);
    }
}

#[cfg(kani)]
mod phase_proofs {
    use super::*;

    fn phase() -> CredentialMaterialMutationPhase {
        match kani::any::<u8>() % 4 {
            0 => CredentialMaterialMutationPhase::Writing,
            1 => CredentialMaterialMutationPhase::Ready,
            2 => CredentialMaterialMutationPhase::Reclaiming,
            _ => CredentialMaterialMutationPhase::ReclaimingAbort,
        }
    }

    fn identity() -> String {
        match kani::any::<u8>() % 3 {
            0 => String::new(),
            1 => "a".into(),
            _ => "b".into(),
        }
    }

    /// @obligation credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
    #[kani::proof]
    #[kani::unwind(4)]
    fn credential_material_phase_transitions_preserve_exact_writer_fence() {
        // Causes: all phases, independent empty/equal/foreign token and
        // attempt identities, arbitrary u64 epoch/deadline and u8 format.
        // R1 Ready iff original owned Writing; R2 publication iff Ready;
        // R3 abort iff not already publishing. Every accepted transition
        // changes phase alone; every rejected transition leaves input intact.
        // This calls the real String-bearing methods. Identity strings are
        // bounded to {empty,a,b}; neither arbitrary String refinement nor
        // repository transaction admission is asserted by this harness.
        use CredentialMaterialMutationPhase::*;
        let input = CredentialMaterialMutationFence {
            format_version: kani::any(),
            phase: phase(),
            attempt_id: identity(),
            writer_token: identity(),
            writer_epoch: kani::any(),
            writer_lease_expires_at_unix_ms: kani::any(),
        };
        let snapshot = input.clone();
        let ready = input.ready();
        let original_writer = input.writer_epoch == 1
            && !input.writer_token.is_empty()
            && input.writer_token == input.attempt_id
            && input.writer_lease_expires_at_unix_ms != 0;
        assert_eq!(ready.is_ok(), input.phase == Writing && original_writer);
        let published = input.reclaiming();
        assert_eq!(published.is_ok(), input.phase == Ready);
        let aborted = input.reclaiming_abort();
        assert_eq!(aborted.is_ok(), input.phase != Reclaiming);
        for (result, expected_phase) in [
            (ready, Ready),
            (published, Reclaiming),
            (aborted, ReclaimingAbort),
        ] {
            if let Ok(mut result) = result {
                assert_eq!(result.phase, expected_phase);
                // Normalize only the permitted state coordinate, then require
                // equality of every remaining field (including future fields).
                result.phase = input.phase;
                assert_eq!(result, snapshot);
            }
        }
        assert_eq!(input, snapshot);
    }

    /// @obligation credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
    /// @obligation credential.restart_recovery_compensates_unpublished_material
    #[kani::proof]
    #[kani::unwind(36)]
    fn credential_material_takeover_preserves_attempt_and_cannot_restore_original_writer() {
        // C1 all phases and full-width expiry/now/successor deadlines; C2 all
        // epochs including zero/MAX; C3 independent empty/two valid 32-byte UUID attempt, old and
        // replacement tokens. T1 non-Writing or live => None before any token
        // call; T2 expired Writing + invalid new lease => error without entropy;
        // T3 exhausted epoch => error; T4 otherwise exactly one successor.
        // T4 preserves every field except token/epoch/deadline. Zero and one
        // both advance to two; higher epochs advance exactly once. Every
        // successor is abort-only even if entropy repeats the attempt token.
        // This is the production transition; UUID entropy, arbitrary strings,
        // envelope validation and repository CAS remain separate boundaries.
        use CredentialMaterialMutationPhase::*;
        let identity = || match kani::any::<u8>() % 3 {
            0 => String::new(),
            1 => "0000000000004000800000000000000a".into(),
            _ => "0000000000004000800000000000000b".into(),
        };
        let original = CredentialMaterialMutationFence {
            format_version: kani::any(),
            phase: phase(),
            attempt_id: identity(),
            writer_token: identity(),
            writer_epoch: kani::any(),
            writer_lease_expires_at_unix_ms: kani::any(),
        };
        let snapshot = original.clone();
        let now = kani::any::<u64>();
        let deadline = kani::any::<u64>();
        let token = identity();
        let mut calls = 0u8;
        let result = original.claim_after_expiry_with_token(now, deadline, || {
            calls += 1;
            token.clone()
        });
        assert_eq!(original, snapshot);
        if original.phase != Writing || original.writer_lease_expires_at_unix_ms > now {
            assert!(matches!(result, Ok(None)));
            assert_eq!(calls, 0);
        } else if deadline <= now {
            assert!(result.is_err());
            assert_eq!(calls, 0);
        } else if original.writer_epoch == u64::MAX {
            assert!(result.is_err());
            assert_eq!(calls, 1);
        } else {
            let mut claimed = result.unwrap().unwrap();
            assert_eq!(calls, 1);
            assert_eq!(claimed.phase, Writing);
            assert_eq!(claimed.writer_epoch, original.writer_epoch.max(1) + 1);
            assert_eq!(claimed.writer_token, token);
            assert_eq!(claimed.writer_lease_expires_at_unix_ms, deadline);
            assert!(claimed.reclaiming().is_err());
            assert!(claimed.writer_epoch >= 2);
            assert!(claimed.ready().is_err());
            assert_eq!(claimed.reclaiming_abort().unwrap().phase, ReclaimingAbort);
            claimed.writer_epoch = original.writer_epoch;
            claimed.writer_token = original.writer_token.clone();
            claimed.writer_lease_expires_at_unix_ms = original.writer_lease_expires_at_unix_ms;
            assert_eq!(claimed, snapshot);
        }
    }

    /// @obligation credential.failed_create_compensates_material
    /// @obligation credential.managed_creation_expired_owner_takeover_fences_abstract_transitions_and_conditionally_converges
    /// @obligation credential.material_is_always_published_or_intent_tracked
    /// @obligation credential.restart_recovery_compensates_unpublished_material
    /// @obligation credential.visible_row_always_has_material
    #[kani::proof]
    fn credential_material_recovery_action_is_total_and_expiry_exact() {
        // Causes: C1 all four durable phases; C2 all eight signals; C3 arbitrary
        // full-width now/deadline. Effects: only confirmed Ready material may
        // publish; owner failure/incomplete material aborts; unknown retains;
        // terminal cleanup remains tracked until confirmed absence; only then
        // may the WAL retire. Empty owner fields deliberately avoid assuming
        // envelope validation: this is planning, not repository/SecretStore IO.
        use CredentialMaterialMutationPhase::*;
        use CredentialMaterialRecoveryPlan::*;
        use CredentialMaterialRecoverySignal::*;
        let input = CredentialMaterialMutationFence {
            phase: phase(),
            writer_lease_expires_at_unix_ms: kani::any(),
            ..Default::default()
        };
        let now = kani::any::<u64>();
        let signal = match kani::any::<u8>() % 9 {
            0 => SupervisorTick,
            1 => OwnerWritesComplete,
            2 => OwnerWriteFailed,
            3 => UnpublishedConflict,
            4 => ReadyMaterialComplete,
            5 => ReadyMaterialIncomplete,
            6 => ReadyMaterialUnknown,
            7 => CleanupConfirmedAbsent,
            _ => CleanupUnknown,
        };
        let plan = credential_material_recovery_plan(&input, now, signal);
        assert_eq!(
            plan == PublishReady,
            input.phase == Ready && matches!(signal, OwnerWritesComplete | ReadyMaterialComplete)
        );
        assert_eq!(
            plan == RetireIntent,
            matches!(input.phase, Reclaiming | ReclaimingAbort) && signal == CleanupConfirmedAbsent
        );
        if matches!(signal, ReadyMaterialUnknown | CleanupUnknown) {
            assert_ne!(plan, PublishReady);
            assert_ne!(plan, RetireIntent);
        }
        if signal == OwnerWriteFailed {
            assert_eq!(
                plan == AbortUnpublished,
                matches!(input.phase, Writing | Ready)
            );
        }
        check_recovery_plan_conditionally_reaches_cleanup();
    }

    fn check_recovery_plan_conditionally_reaches_cleanup() {
        // Causes: C1 every durable material phase; C2 the Writing lease is at
        // or past expiry; C3 each selected repository/external action returns
        // success. Effects: E1 expired Writing advances only to abort cleanup;
        // E2 Ready advances only to published cleanup; E3 both cleanup phases
        // are terminal recovery actions. The phase rules are exhaustive, so
        // under C2+C3 every non-cleanup phase reaches cleanup in one transition.
        // SQL CAS, SecretStore completion and scheduler fairness remain TCB;
        // this proves only the production decision and phase methods they call.
        use CredentialMaterialMutationPhase::*;
        use CredentialMaterialRecoveryPlan::*;
        use CredentialMaterialRecoverySignal::*;

        let phase = phase();
        let now = kani::any::<u64>();
        let input = CredentialMaterialMutationFence {
            format_version: CREDENTIAL_MATERIAL_MUTATION_FORMAT_VERSION,
            phase,
            attempt_id: "a".into(),
            writer_token: "a".into(),
            writer_epoch: 1,
            writer_lease_expires_at_unix_ms: now,
        };
        let action = credential_material_recovery_plan(&input, now, SupervisorTick);
        assert_ne!(
            action, RetainResponsibility,
            "C2 excludes the only stutter action"
        );
        match action {
            ClaimExpiredWriterForAbort => {
                let next = input.reclaiming_abort().expect("expired Writing aborts");
                assert_eq!(
                    credential_material_recovery_plan(&next, now, SupervisorTick),
                    CleanupAborted,
                    "E1"
                );
            }
            InspectReadyMaterial => {
                assert_eq!(
                    credential_material_recovery_plan(&input, now, ReadyMaterialComplete),
                    PublishReady
                );
                let next = input.reclaiming().expect("complete Ready publishes");
                assert_eq!(
                    credential_material_recovery_plan(&next, now, SupervisorTick),
                    CleanupPublished,
                    "E2"
                );
            }
            CleanupPublished | CleanupAborted => {
                assert!(matches!(phase, Reclaiming | ReclaimingAbort), "E3")
            }
            _ => unreachable!("the expired supervisor plan is closed"),
        }
    }
}
