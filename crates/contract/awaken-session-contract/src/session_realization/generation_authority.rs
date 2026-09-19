//! Authority of one exact MCP generation fence across lease renewal.
//!
//! Split from the realization control flow: this module owns only the
//! predicate that decides whether completed physical evidence still belongs to
//! the live fence, never a state transition.

use super::McpGenerationRef;

/// Whether the current exact-generation fence is the asserted fence or a
/// monotonic lease extension of it. Logical generation, owner incarnation, and
/// epoch remain immutable; only expiry may advance. Under a live root lease,
/// Control uses this relation to retain completed predecessor evidence without
/// repeating a physical effect merely because authority duration increased.
///
/// The attachment identity is not a separate equality: it is one of the three
/// coordinates the complete generation witness already compares.
#[must_use]
pub fn realization_generation_authorizes(
    current: &McpGenerationRef,
    asserted: &McpGenerationRef,
) -> bool {
    use awaken_provisioning_contract::{FenceSuccession, fence_succession};
    // Same Session and generation, and the current lease is a renewal of the
    // asserted one under the one fence succession law.
    current.session_id == asserted.session_id
        && awaken_resource_contract::generation_matches(&current.generation, &asserted.generation)
        && matches!(
            fence_succession(
                asserted.lease_epoch,
                asserted.lease_expires_at_unix_ms,
                current.lease_epoch,
                current.lease_expires_at_unix_ms,
                current.runtime_incarnation == asserted.runtime_incarnation
                    && current.lease_epoch == asserted.lease_epoch,
            ),
            Some(FenceSuccession::Renewal)
        )
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn realization_generation_authority_requires_every_axis() {
    // G1-G64: all combinations of the immutable identity equalities and expiry
    // ordering. Strings and the content digest use two one-byte representatives
    // of equality; expiry,
    // epoch, and the generation's version/digest retain arbitrary values. The
    // attachment identity is now a coordinate of the complete generation
    // witness, so it varies inside `generation` rather than beside it. Live
    // root authority is a separate precondition, not a claim of this predicate.
    let same_session: bool = kani::any();
    let same_attachment: bool = kani::any();
    let same_digest: bool = kani::any();
    let same_incarnation: bool = kani::any();
    let current = McpGenerationRef {
        session_id: "a".into(),
        generation: crate::McpGeneration::new(
            crate::McpAttachmentId("a".into()),
            kani::any(),
            crate::McpAttachmentContentDigest::proof_representative(true),
        ),
        runtime_incarnation: "a".into(),
        lease_epoch: kani::any(),
        lease_expires_at_unix_ms: kani::any(),
    };
    let asserted = McpGenerationRef {
        session_id: if same_session { "a" } else { "b" }.into(),
        generation: crate::McpGeneration::new(
            crate::McpAttachmentId(if same_attachment { "a" } else { "b" }.into()),
            kani::any(),
            crate::McpAttachmentContentDigest::proof_representative(same_digest),
        ),
        runtime_incarnation: if same_incarnation { "a" } else { "b" }.into(),
        lease_epoch: kani::any(),
        lease_expires_at_unix_ms: kani::any(),
    };
    assert_eq!(
        realization_generation_authorizes(&current, &asserted),
        same_session
            && same_attachment
            && same_digest
            && same_incarnation
            && current.generation.version() == asserted.generation.version()
            && current.lease_epoch == asserted.lease_epoch
            && current.lease_expires_at_unix_ms >= asserted.lease_expires_at_unix_ms
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpGenerationRef;

    /// Generation-fence cause/effect graph: C1 logical generation identity,
    /// owner incarnation, and epoch match; C2 current expiry is equal/later.
    /// E1 retains completed evidence under separately verified live authority;
    /// E2 rejects replacement, regression, or cross-generation input.
    ///
    /// | Rule | C1 | C2 | Effect |
    /// |---|---|---|---|
    /// | G1 exact | yes | equal | E1 |
    /// | G2 renewed | yes | later | E1 |
    /// | G3 regression | yes | earlier | E2 |
    /// | G4 replacement | no | any | E2 |
    #[test]
    fn realization_generation_authorization_decision_table() {
        let asserted = McpGenerationRef {
            session_id: "session-a".into(),
            generation: crate::mcp_attachment::test_generation(
                "browser",
                1,
                "https://browser.test/mcp",
            ),
            runtime_incarnation: "worker-a/boot-1".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: 10,
        };
        // G5 every feasible equality combination (64 rows) independently
        // binds all six axes; G1-G4 below retain readable boundary examples.
        for mask in 0_u8..64 {
            let mut candidate = asserted.clone();
            if mask & 1 != 0 {
                candidate.session_id = "foreign".into();
            }
            if mask & 2 != 0 {
                candidate.generation = crate::mcp_attachment::test_generation(
                    "foreign",
                    candidate.generation.version(),
                    "https://browser.test/mcp",
                );
            }
            if mask & 4 != 0 {
                candidate.generation =
                    crate::mcp_attachment::test_generation_at_version(&candidate.generation, 2);
            }
            if mask & 8 != 0 {
                candidate.runtime_incarnation = "foreign".into();
            }
            if mask & 16 != 0 {
                candidate.lease_epoch += 1;
            }
            if mask & 32 != 0 {
                candidate.lease_expires_at_unix_ms -= 1;
            }
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
                McpGenerationRef {
                    lease_expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
                true,
            ),
            (
                "G3",
                McpGenerationRef {
                    lease_expires_at_unix_ms: 9,
                    ..asserted.clone()
                },
                false,
            ),
            (
                "G4",
                McpGenerationRef {
                    lease_epoch: 4,
                    lease_expires_at_unix_ms: 20,
                    ..asserted.clone()
                },
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
        let asserted = McpGenerationRef {
            session_id: "session-a".into(),
            generation: crate::mcp_attachment::test_generation(
                "browser",
                1,
                "https://browser.test/mcp",
            ),
            runtime_incarnation: "worker-a/boot-1".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: 10,
        };
        for (epoch, expiry, expected, rule) in [
            (3, 9, false, "R2 shortened"),
            (3, 10, true, "R1 equal"),
            (3, 11, true, "R1 lengthened"),
            (4, 11, false, "R3 reassignment"),
            (4, 10, false, "R3 reassignment equal expiry"),
        ] {
            let mut current = asserted.clone();
            current.lease_epoch = epoch;
            current.lease_expires_at_unix_ms = expiry;
            assert_eq!(
                realization_generation_authorizes(&current, &asserted),
                expected,
                "{rule}"
            );
        }
    }
}
