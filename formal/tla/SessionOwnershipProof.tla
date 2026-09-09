--------------------- MODULE SessionOwnershipProof ---------------------
EXTENDS SessionOwnership, SessionRootProof

\* Cause/effect partitions reuse the root proof unchanged: writes preserve the
\* snapshot; reads preserve the root and capture its tuple; crashes stutter.
LEMMA EmptyRowIsCoherent == KernelAssumptions => RowIsOwned(NoRow)
BY Z3T(10) DEF KernelAssumptions, RowIsOwned, NoRow

LEMMA RootSafetyImpliesOwnedRow ==
    KernelAssumptions /\ Safety => VisibleRowAlwaysHasOwner
BY Z3T(20) DEF Safety, TypeOK, VisibleRootIsComplete,
   VisibleRowAlwaysHasOwner, RowIsOwned, RootRow, NoRow, KernelAssumptions

LEMMA OwnershipInitSafety ==
    KernelAssumptions /\ OwnershipInit => OwnershipSafety
BY InitSafety, EmptyRowIsCoherent, RootSafetyImpliesOwnedRow
   DEF OwnershipInit, OwnershipSafety, ReadSnapshotIsCoherent

LEMMA RootWritePreservesOwnership ==
    KernelAssumptions /\ OwnershipSafety /\ Next /\
    UNCHANGED <<readSnapshot, readCompleted>> => OwnershipSafety'
BY Z3T(30), NextSafety DEF OwnershipSafety, ReadSnapshotIsCoherent,
   VisibleRowAlwaysHasOwner, RowIsOwned, RootRow, NoRow, KernelAssumptions,
   Safety, TypeOK, VisibleRootIsComplete

LEMMA ReadPreservesOwnership ==
    KernelAssumptions /\ OwnershipSafety /\ Read => OwnershipSafety'
BY Z3T(30), SafetyStutter, RootSafetyImpliesOwnedRow
   DEF Read, OwnershipSafety, ReadSnapshotIsCoherent, VisibleRowAlwaysHasOwner,
       KernelAssumptions, RowIsOwned, RootRow, kVars

LEMMA OwnershipStutter ==
    OwnershipSafety /\ UNCHANGED OwnershipVars => OwnershipSafety'
BY Z3T(30), SafetyStutter DEF OwnershipSafety, OwnershipVars,
   VisibleRowAlwaysHasOwner, ReadSnapshotIsCoherent, RootRow, RowIsOwned, kVars

OwnershipInductiveSafety == KernelAssumptions /\ OwnershipSafety

LEMMA OwnershipStepSafety ==
    OwnershipInductiveSafety /\ [OwnershipNext]_OwnershipVars =>
    OwnershipInductiveSafety'
BY RootWritePreservesOwnership, ReadPreservesOwnership, OwnershipStutter
   DEF OwnershipInductiveSafety, OwnershipNext, KernelAssumptions

THEOREM SessionOwnershipSafety ==
    KernelAssumptions /\ OwnershipSpec => []OwnershipSafety
BY OwnershipInitSafety, OwnershipStepSafety, PTL
   DEF OwnershipSpec, OwnershipInductiveSafety

LEMMA EachReadCapturesOneRootFact ==
    [OwnershipNext]_OwnershipVars =>
      [readCompleted' # readCompleted => readSnapshot' = RootRow]_OwnershipVars
BY DEF OwnershipNext, Read, OwnershipVars

THEOREM SessionOwnershipReadCoherence ==
    OwnershipSpec => ReadCapturesOneRootFact
BY EachReadCapturesOneRootFact, PTL
   DEF OwnershipSpec, ReadCapturesOneRootFact

\* Writes can choose an owner only for Absent; replay/conflict, realization,
\* terminal operations and observation preserve the bound identity.
LEMMA KernelNeverRebindsOwner ==
    Next /\ kExistence # "Absent" => kOwner' = kOwner
BY Z3T(30) DEF Next, Create, ExactCreateReplay, RejectConflictingCreate,
   ReplayTombstonedCreate, BeginRealization, StageRealization,
   ActivateRealization, CompleteRealization, CloseRunningInterval,
   RetryableRealizationFailure, PermanentRealizationFailure,
   ProjectWork, LoseWorkProjection, Terminate, Tombstone

LEMMA OwnershipNeverRebindsOwnerStep ==
    [OwnershipNext]_OwnershipVars =>
      [kExistence # "Absent" => kOwner' = kOwner]_OwnershipVars
BY KernelNeverRebindsOwner DEF OwnershipNext, OwnershipVars, Read, kVars

THEOREM SessionOwnerNeverRebinds ==
    OwnershipSpec => OwnerNeverRebinds
BY OwnershipNeverRebindsOwnerStep, PTL DEF OwnershipSpec, OwnerNeverRebinds
=============================================================================
