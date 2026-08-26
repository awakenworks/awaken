use std::collections::HashSet;
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

/// Sole phase-to-recovery action classifier for both publication envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialMaterialRecoveryAction {
    SkipLiveWriting,
    AbortExpiredWriting,
    PublishReady,
    CleanupPublished,
    CleanupAborted,
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
        claimed.writer_token = uuid::Uuid::new_v4().simple().to_string();
        claimed.writer_epoch = self.writer_epoch.checked_add(1).ok_or_else(|| {
            CredentialError::MutationConflict(
                "credential material writer epoch is exhausted".into(),
            )
        })?;
        claimed.writer_lease_expires_at_unix_ms = lease_expires_at_unix_ms;
        Ok(Some(claimed))
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
pub(super) const fn credential_material_recovery_action(
    fence: &CredentialMaterialMutationFence,
    now_unix_ms: u64,
) -> CredentialMaterialRecoveryAction {
    match fence.phase {
        CredentialMaterialMutationPhase::Writing
            if fence.writer_lease_expires_at_unix_ms > now_unix_ms =>
        {
            CredentialMaterialRecoveryAction::SkipLiveWriting
        }
        CredentialMaterialMutationPhase::Writing => {
            CredentialMaterialRecoveryAction::AbortExpiredWriting
        }
        CredentialMaterialMutationPhase::Ready => CredentialMaterialRecoveryAction::PublishReady,
        CredentialMaterialMutationPhase::Reclaiming => {
            CredentialMaterialRecoveryAction::CleanupPublished
        }
        CredentialMaterialMutationPhase::ReclaimingAbort => {
            CredentialMaterialRecoveryAction::CleanupAborted
        }
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

pub(super) fn namespace_new_material_refs(
    before: Option<&CredentialSource>,
    after: &mut CredentialSource,
    attempt_id: &str,
) {
    let before_refs = before
        .into_iter()
        .flat_map(CredentialSource::material_refs)
        .cloned()
        .collect::<HashSet<_>>();
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
        .collect::<HashSet<_>>();
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
    fn recovery_action_decision_table_is_shared_by_both_envelopes() {
        /* Cause/effect table: C1=phase, C2=Writing lease live. Effects are the
         * sole external action. R1 Writing+live=>skip; R2 Writing+expired=>abort;
         * R3 Ready=>publish; R4 Reclaiming=>published cleanup; R5
         * ReclaimingAbort=>unpublished cleanup. Source and Managed recovery call
         * this classifier, so one table covers both publication envelopes. */
        let now = 10;
        let cases = [
            (
                fence(CredentialMaterialMutationPhase::Writing, now + 1),
                CredentialMaterialRecoveryAction::SkipLiveWriting,
            ),
            (
                fence(CredentialMaterialMutationPhase::Writing, now),
                CredentialMaterialRecoveryAction::AbortExpiredWriting,
            ),
            (
                fence(CredentialMaterialMutationPhase::Ready, 0),
                CredentialMaterialRecoveryAction::PublishReady,
            ),
            (
                fence(CredentialMaterialMutationPhase::Reclaiming, 0),
                CredentialMaterialRecoveryAction::CleanupPublished,
            ),
            (
                fence(CredentialMaterialMutationPhase::ReclaimingAbort, 0),
                CredentialMaterialRecoveryAction::CleanupAborted,
            ),
        ];
        for (fence, expected) in cases {
            assert_eq!(credential_material_recovery_action(&fence, now), expected);
        }
    }
}

#[cfg(kani)]
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
