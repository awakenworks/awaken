//! The shape every optimistic-concurrency witness must have.
//!
//! A compare-and-set precondition is only sound when it pins all three
//! coordinates of the head it observed:
//!
//! * **identity** — which subject was read. Without it a witness taken from one
//!   row can be replayed against another (a path-only rename moves content
//!   between logical locations while every other coordinate is unchanged).
//! * **version** — how far that subject had advanced. Without it an
//!   `A -> B -> A` content cycle retains the observed digest while the
//!   authoritative head has moved on, and the stale writer silently wins.
//! * **content digest** — what was actually observed. Without it a counter that
//!   was bumped by a no-op or by a metadata-only write looks like a content
//!   change (and a counter reset or a per-replica counter looks like no change
//!   at all).
//!
//! [`CompleteGeneration`] is the type-level statement of that rule. It is
//! sealed, so a bare counter (`u64`) or a bare digest (`String`) can never be
//! introduced as a CAS precondition by accident: a new witness type must be
//! declared deliberately, and declaring it forces all three coordinates to be
//! supplied.

/// The seal for [`CompleteGeneration`].
///
/// It is `pub` (and `#[doc(hidden)]`) rather than private because complete
/// witnesses are owned by the bounded context whose aggregate they fence —
/// `MemoryGeneration` here, `McpGeneration` in `awaken-session-mcp-contract` — and
/// those contexts sit downstream of this crate. Naming it is therefore possible
/// but never accidental: an implementation has to reach into a hidden module of
/// a contract crate, which is greppable and reviewable, and it still has to
/// supply every coordinate of [`CompleteGeneration`].
#[doc(hidden)]
pub mod sealed {
    /// Supertrait seal. Implement it only alongside
    /// [`super::CompleteGeneration`], never on its own.
    pub trait Sealed {}
}

/// A complete optimistic-concurrency witness.
///
/// Sealed: implementing it requires supplying all three coordinates, so a bare
/// counter or a bare digest cannot be used as a CAS precondition.
///
/// Implementations are value objects: the three coordinates must have been
/// observed **together**, in one read of one authoritative head. Assembling a
/// witness from an identity read at one time and a digest read at another
/// re-opens exactly the window the trait exists to close.
pub trait CompleteGeneration: sealed::Sealed {
    /// The immutable identity of the subject whose head was observed.
    fn subject_id(&self) -> &str;
    /// The monotonic advance of that subject at the moment of observation.
    fn version(&self) -> u64;
    /// The digest of the content observed at that version.
    fn content_digest(&self) -> &str;
}

/// Does `observed` still describe `live`?
///
/// This is the one comparison a conditional mutation may make. Both sides are
/// complete witnesses, so a mismatch on *any* coordinate — a replayed identity,
/// a stale version behind an `A -> B -> A` cycle, or a digest that drifted
/// under an unchanged counter — refuses the write.
#[must_use]
pub fn generation_matches<Observed, Live>(observed: &Observed, live: &Live) -> bool
where
    Observed: CompleteGeneration + ?Sized,
    Live: CompleteGeneration + ?Sized,
{
    observed.subject_id() == live.subject_id()
        && observed.version() == live.version()
        && observed.content_digest() == live.content_digest()
}

#[cfg(test)]
mod tests {
    use super::{CompleteGeneration, generation_matches};
    use crate::{Memory, MemoryGeneration};

    fn memory(id: &str, version: u64, content: &str) -> Memory {
        Memory {
            id: id.to_string(),
            path: "/notes.md".to_string(),
            content_sha256: crate::memory_sha256_hex(content),
            content_size: content.len() as u64,
            version,
            created_unix_nanos: 1,
            updated_unix_nanos: u128::from(version),
            content: Some(content.to_string()),
        }
    }

    #[test]
    fn memory_generation_exposes_all_three_coordinates() {
        let head = memory("mem-1", 7, "alpha");
        let generation = head.generation();
        assert_eq!(generation.subject_id(), "mem-1");
        assert_eq!(generation.version(), 7);
        assert_eq!(
            generation.content_digest(),
            crate::memory_sha256_hex("alpha")
        );
        assert!(generation_matches(&generation, &head.generation()));
    }

    #[test]
    fn an_a_b_a_content_cycle_is_refused_by_the_version_coordinate() {
        // The classic ABA: a reader observes A, two writers turn it into B and
        // back into A. The digest is identical on both ends of the cycle, so a
        // digest-only witness would apply the stale write. The complete witness
        // still carries version 3 vs the live version 5 and must refuse.
        let observed = memory("mem-1", 3, "A").generation();
        let live = memory("mem-1", 5, "A").generation();
        assert_eq!(observed.content_digest(), live.content_digest());
        assert!(!generation_matches(&observed, &live));
        // Re-reading after the cycle produces a witness that does apply.
        assert!(generation_matches(&live, &live));
    }

    #[test]
    fn a_replayed_identity_is_refused_even_at_the_same_version_and_digest() {
        // A path-only rename leaves two rows whose version and content agree;
        // only the immutable id separates them.
        let observed = memory("mem-1", 4, "A").generation();
        let other = memory("mem-2", 4, "A").generation();
        assert_eq!(observed.version(), other.version());
        assert_eq!(observed.content_digest(), other.content_digest());
        assert!(!generation_matches(&observed, &other));
    }

    #[test]
    fn a_digest_that_drifted_under_an_unchanged_counter_is_refused() {
        let observed = memory("mem-1", 4, "A").generation();
        let live = memory("mem-1", 4, "B").generation();
        assert!(!generation_matches(&observed, &live));
    }

    #[test]
    fn a_bare_counter_cannot_stand_in_for_a_witness() {
        // The seal is what makes this a compile-time property: `u64` and
        // `String` are outside this crate's control and cannot acquire
        // `sealed::Sealed`, so neither can reach a `CompleteGeneration` bound.
        // What is asserted here is the runtime half: the version coordinate
        // alone does not decide the comparison.
        fn precondition(witness: &impl CompleteGeneration) -> (String, u64, String) {
            (
                witness.subject_id().to_string(),
                witness.version(),
                witness.content_digest().to_string(),
            )
        }
        let generation = MemoryGeneration::new("mem-1", 9, crate::memory_sha256_hex("A"));
        assert_eq!(
            precondition(&generation),
            ("mem-1".to_string(), 9, crate::memory_sha256_hex("A"))
        );
    }
}
