// @formal entrypoint session_realization.generation_authority_requires_every_immutable_axis
//! Authority of one exact MCP generation fence across lease renewal.
//!
//! Split from the realization control flow: this module owns only the
//! predicate that decides whether completed physical evidence still belongs to
//! the live fence, never a state transition.

use super::McpGenerationRef;
#[cfg(kani)]
use awaken_session_configuration_contract::mcp::{
    McpAttachmentContentDigest, McpAttachmentId, McpGeneration,
};

/// Whether the current exact-generation fence is the asserted fence or a
/// monotonic lease extension of it. Session, logical generation, owner
/// incarnation, and epoch remain immutable; only expiry may advance. Under a
/// live root lease, Control uses this relation to retain completed predecessor
/// evidence without repeating a physical effect merely because authority
/// duration increased.
///
/// This is exactly a renewal under the one fence law: the Session and the
/// complete generation witness are the lease's subject, so a fence for another
/// Session or generation is never a successor, and a higher epoch is a
/// reassignment that does not authorize an assertion made under its
/// predecessor.
#[must_use]
pub fn realization_generation_authorizes(
    current: &McpGenerationRef,
    asserted: &McpGenerationRef,
) -> bool {
    asserted.succession(current) == Some(awaken_lease_contract::fence::FenceSuccession::Renewal)
}

#[cfg(kani)]
/// @obligation session_realization.generation_authority_requires_every_immutable_axis
/// @obligation session_realization.publication_excludes_staged_retired_generation
#[kani::proof]
#[kani::unwind(3)]
fn realization_generation_authority_requires_every_axis() {
    // G1-G64: all combinations of the immutable identity equalities and expiry
    // ordering. Strings and the content digest use two one-byte representatives
    // of equality; expiry, epoch, and the generation's version retain
    // arbitrary values (epochs are nonzero because no lease grants zero). The
    // attachment identity is a coordinate of the complete generation witness,
    // so it varies inside `generation` rather than beside it. Live root
    // authority is a separate precondition, not a claim of this predicate.
    let same_session: bool = kani::any();
    let same_attachment: bool = kani::any();
    let same_digest: bool = kani::any();
    let same_incarnation: bool = kani::any();
    let current_version: u64 = kani::any();
    let asserted_version: u64 = kani::any();
    let current_epoch: u64 = kani::any();
    let asserted_epoch: u64 = kani::any();
    kani::assume(current_epoch != 0 && asserted_epoch != 0);
    let current_expiry: u64 = kani::any();
    let asserted_expiry: u64 = kani::any();
    let current = McpGenerationRef::from_persisted(
        "a".into(),
        McpGeneration::new(
            McpAttachmentId("a".into()),
            current_version,
            McpAttachmentContentDigest::proof_representative(true),
        ),
        "a".into(),
        current_epoch,
        current_expiry,
    )
    .unwrap();
    let asserted = McpGenerationRef::from_persisted(
        if same_session { "a" } else { "b" }.into(),
        McpGeneration::new(
            McpAttachmentId(if same_attachment { "a" } else { "b" }.into()),
            asserted_version,
            McpAttachmentContentDigest::proof_representative(same_digest),
        ),
        if same_incarnation { "a" } else { "b" }.into(),
        asserted_epoch,
        asserted_expiry,
    )
    .unwrap();
    assert_eq!(
        realization_generation_authorizes(&current, &asserted),
        same_session
            && same_attachment
            && same_digest
            && same_incarnation
            && current_version == asserted_version
            && current_epoch == asserted_epoch
            && current_expiry >= asserted_expiry
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpGenerationRef;

    fn fence(
        session: &str,
        attachment: &str,
        version: u64,
        incarnation: &str,
        epoch: u64,
        expiry: u64,
    ) -> McpGenerationRef {
        McpGenerationRef::from_persisted(
            session.into(),
            crate::mcp_attachment::test_generation(attachment, version, "https://browser.test/mcp"),
            incarnation.into(),
            epoch,
            expiry,
        )
        .expect("nonzero epoch")
    }

    /// Generation-fence cause/effect graph: C1 Session, logical generation
    /// identity, owner incarnation, and epoch match; C2 current expiry is
    /// equal/later. E1 retains completed evidence under separately verified
    /// live authority; E2 rejects replacement, regression, or cross-generation
    /// input.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | G1 exact | yes | equal | E1 |
    /// | G2 renewed | yes | later | E1 |
    /// | G3 regression | yes | earlier | E2 |
    /// | G4 replacement | no | any | E2 |
    #[test]
    fn realization_generation_authorization_decision_table() {
        let asserted = fence("session-a", "browser", 1, "worker-a/boot-1", 3, 10);
        // G5 every feasible equality combination (64 rows) independently
        // binds all six axes; G1-G4 below retain readable boundary examples.
        for mask in 0_u8..64 {
            let candidate = fence(
                if mask & 1 != 0 {
                    "foreign"
                } else {
                    "session-a"
                },
                if mask & 2 != 0 { "foreign" } else { "browser" },
                if mask & 4 != 0 { 2 } else { 1 },
                if mask & 8 != 0 {
                    "foreign"
                } else {
                    "worker-a/boot-1"
                },
                if mask & 16 != 0 { 4 } else { 3 },
                if mask & 32 != 0 { 9 } else { 10 },
            );
            assert_eq!(
                realization_generation_authorizes(&candidate, &asserted),
                mask == 0,
                "G5 mask={mask}"
            );
        }
        for (rule, current, expected) in [
            ("G1", asserted.clone(), true),
            (
                "G2",
                fence("session-a", "browser", 1, "worker-a/boot-1", 3, 20),
                true,
            ),
            (
                "G3",
                fence("session-a", "browser", 1, "worker-a/boot-1", 3, 9),
                false,
            ),
            (
                "G4",
                fence("session-a", "browser", 1, "worker-a/boot-1", 4, 20),
                false,
            ),
        ] {
            assert_eq!(
                realization_generation_authorizes(&current, &asserted),
                expected,
                "{rule}"
            );
        }
    }

    #[test]
    fn generation_authorization_is_renewal_only_under_the_fence_succession_law() {
        // C1 current lease expiry shorter/equal/longer than asserted; C2 epoch
        // equal/higher. R1 equal epoch with expiry not shortened => authorized
        // (a renewal); R2 shortened expiry => refused; R3 a higher epoch is a
        // *reassignment* under the shared succession law and must not
        // authorize an assertion made under the previous generation.
        let asserted = fence("session-a", "browser", 1, "worker-a/boot-1", 3, 10);
        for (epoch, expiry, expected, rule) in [
            (3, 9, false, "R2 shortened"),
            (3, 10, true, "R1 equal"),
            (3, 11, true, "R1 lengthened"),
            (4, 11, false, "R3 reassignment"),
            (4, 10, false, "R3 reassignment equal expiry"),
        ] {
            let current = fence("session-a", "browser", 1, "worker-a/boot-1", epoch, expiry);
            assert_eq!(
                realization_generation_authorizes(&current, &asserted),
                expected,
                "{rule}"
            );
        }
    }
}
