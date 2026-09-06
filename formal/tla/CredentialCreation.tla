----------------------- MODULE CredentialCreation -----------------------
EXTENDS ManagedCredentialCreation

\* Source-only projection of the canonical material-attempt protocol. The
\* absent Managed child is a ghost publication bit, not a second runtime store.
\* Both envelopes share Writing/Ready/ReclaimingAbort and exact writer epochs.
\* This replaces the obsolete atomic delete-and-retire transition, not Rust
\* refinement. Unreachable late-writer orphans remain an explicit exclusion.
VisibleRowAlwaysHasMaterial == sourcePublished => material = "Complete"
MaterialIsNeverUntracked == UnpublishedMaterialCleanupHasDurableFact
RetiredUnpublishedIntentHasNoMaterial == AbortedCreationIsClean
=============================================================================
