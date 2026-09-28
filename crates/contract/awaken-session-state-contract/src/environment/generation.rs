// @formal entrypoint session_environment.checkpoint_receipt_requires_every_immutable_generation_axis
// @formal entrypoint session_environment.quiescence_receipt_requires_exact_operation_epoch_mcp_set_and_zero_live_effects
// @formal entrypoint session_environment.restore_receipt_requires_exact_checkpoint_and_nonempty_binding
// @formal entrypoint session_environment.source_disposal_requires_a_verified_checkpoint
//! Immutable identity of one live Sandbox generation.

use crate::SessionId;
/// Stable identity and immutable compatibility facts for one live Sandbox.
///
/// **Not an optimistic-concurrency witness**, and deliberately not an
/// [`awaken_resource_contract::CompleteGeneration`]. It has no monotonic
/// version because it never fences a conditional write against a moving head:
///
/// * It is assigned exactly once. [`SessionEnvironmentState::assign_generation`]
///   refuses to replace a generation that is already present ("adoption never
///   rotates it"), so there is no sequence of values to be behind.
/// * Its [`SandboxGeneration::id`] is itself the content digest of every other
///   coordinate — the session, both timestamps, and both fingerprints are
///   hashed into it by [`SandboxGeneration::new`]. Two Sandboxes that differ in
///   any coordinate have different ids, and the same id can never be reached
///   twice by a different Sandbox, so the `A -> B -> A` cycle a version
///   coordinate exists to catch is unrepresentable here.
/// * Every consumer compares it as effect identity, not as a precondition:
///   `QuiescenceReceipt`, `CheckpointReceipt`, `SourceDisposedReceipt`, and
///   `RestoreReceipt` check `receipt.generation_id == generation.id` to confirm
///   a receipt describes *this* physical effect, and
///   [`SessionEnvironmentOperation::new`] folds the whole value into the
///   effect-id preimage. None of those reads a durable head and writes it back.
///
/// Adding a version here would therefore be a coordinate with no consumer. If
/// a conditional mutation of Sandbox state is ever introduced, that is the
/// moment to give this type a version and implement `CompleteGeneration`.
///
/// [`SessionEnvironmentState::assign_generation`]: super::SessionEnvironmentState::assign_generation
/// [`SessionEnvironmentOperation::new`]: super::SessionEnvironmentOperation::new
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxGeneration {
    pub id: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub environment_fingerprint: String,
    pub base_image_fingerprint: String,
}

impl SandboxGeneration {
    #[must_use]
    pub fn new(
        session_id: &SessionId,
        created_at_unix_ms: u64,
        expires_at_unix_ms: u64,
        environment_fingerprint: impl Into<String>,
        base_image_fingerprint: impl Into<String>,
    ) -> Self {
        let environment_fingerprint = environment_fingerprint.into();
        let base_image_fingerprint = base_image_fingerprint.into();
        Self {
            id: crate::stable_fingerprint(&(
                "sandbox-generation-v1",
                session_id,
                created_at_unix_ms,
                expires_at_unix_ms,
                environment_fingerprint.as_str(),
                base_image_fingerprint.as_str(),
            )),
            created_at_unix_ms,
            expires_at_unix_ms,
            environment_fingerprint,
            base_image_fingerprint,
        }
    }

    #[must_use]
    pub const fn expired_at(&self, now_unix_ms: u64) -> bool {
        now_unix_ms >= self.expires_at_unix_ms
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendPhase {
    Quiescing,
    Uploading,
    ReadyToDispose,
    Disposing,
}

/// Representation-free source-durability preparation gate shared by production
/// and the bounded proof harness.
#[must_use]
pub const fn checkpoint_source_preparation_authorized(
    phase: SuspendPhase,
    has_checkpoint: bool,
    has_preparation: bool,
) -> bool {
    matches!(phase, SuspendPhase::ReadyToDispose) && has_checkpoint && !has_preparation
}

/// Representation-free irreversible physical-effect gate shared by production
/// and the bounded proof harness. Preparation must be durably admitted before
/// this phase, so physical absence is never inferred from provider state alone.
#[must_use]
pub const fn checkpoint_source_disposal_authorized(
    phase: SuspendPhase,
    has_checkpoint: bool,
    has_preparation: bool,
) -> bool {
    matches!(phase, SuspendPhase::Disposing) && has_checkpoint && has_preparation
}

/// Closed admission rule for evidence that every live Environment effect has
/// quiesced for the exact suspend operation. String identity comparisons stay
/// at the typed receipt boundary; this heap-free kernel owns their conjunction.
#[must_use]
pub(crate) const fn quiescence_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    activity_epoch_matches: bool,
    mcp_generations_match: bool,
    live_environment_effects: u32,
) -> bool {
    effect_matches
        && generation_matches
        && activity_epoch_matches
        && mcp_generations_match
        && live_environment_effects == 0
}

/// Closed admission rule for a checkpoint created by the exact operation over
/// the exact immutable Environment and base-image generation.
#[must_use]
pub(crate) const fn checkpoint_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    checkpoint_effect_matches: bool,
    environment_fingerprint_matches: bool,
    base_image_fingerprint_matches: bool,
) -> bool {
    effect_matches
        && generation_matches
        && checkpoint_effect_matches
        && environment_fingerprint_matches
        && base_image_fingerprint_matches
}

/// Closed admission rule for the canonical receipt proving every live
/// source-durability participant completed before physical deletion.
#[must_use]
pub(crate) const fn source_release_preparation_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    source_binding_matches: bool,
    canonical_receipt_matches: bool,
) -> bool {
    effect_matches && generation_matches && source_binding_matches && canonical_receipt_matches
}

/// Closed admission rule for the irreversible source-disposal receipt.
#[must_use]
pub(crate) const fn source_disposal_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    source_binding_matches: bool,
    terminated: bool,
) -> bool {
    effect_matches && generation_matches && source_binding_matches && terminated
}

/// Closed admission rule for restoring the exact live generation and
/// checkpoint to a non-empty substrate binding.
#[must_use]
pub(crate) const fn restore_receipt_admitted(
    effect_matches: bool,
    generation_matches: bool,
    checkpoint_matches: bool,
    binding_present: bool,
) -> bool {
    effect_matches && generation_matches && checkpoint_matches && binding_present
}
