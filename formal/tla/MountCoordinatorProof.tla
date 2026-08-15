-------------------- MODULE MountCoordinatorProof --------------------
EXTENDS MountCoordinator, TLAPS

LEMMA InitSafety == KernelAssumptions /\ Init => Safety
BY Z3T(30) DEF KernelAssumptions, Init, Safety, TypeOK,
   MountedIffReferenced, AtMostOneLiveMount

LEMMA AcquireNewSafety ==
    \A store \in Stores:
      KernelAssumptions /\ Safety /\ AcquireNew(store) => Safety'
BY Z3T(30) DEF KernelAssumptions, AcquireNew, Safety, TypeOK,
   MountedIffReferenced, AtMostOneLiveMount

LEMMA AcquireSharedSafety ==
    \A store \in Stores:
      KernelAssumptions /\ Safety /\ AcquireShared(store) => Safety'
BY Z3T(30) DEF KernelAssumptions, AcquireShared, Safety, TypeOK,
   MountedIffReferenced, AtMostOneLiveMount

LEMMA ReleaseSomeSafety ==
    \A store \in Stores:
      KernelAssumptions /\ Safety /\ ReleaseSome(store) => Safety'
BY Z3T(30) DEF KernelAssumptions, ReleaseSome, Safety, TypeOK,
   MountedIffReferenced, AtMostOneLiveMount

LEMMA ReleaseLastSafety ==
    \A store \in Stores:
      KernelAssumptions /\ Safety /\ ReleaseLast(store) => Safety'
BY Z3T(30) DEF KernelAssumptions, ReleaseLast, Safety, TypeOK,
   MountedIffReferenced, AtMostOneLiveMount

LEMMA RejectionsPreserveSafety ==
    \A store \in Stores:
      Safety /\ (AcquireMountFailure(store) \/ RejectOverflow(store)
                  \/ ReleaseUnknown(store)) => Safety'
BY Z3T(30) DEF AcquireMountFailure, RejectOverflow, ReleaseUnknown,
   Safety, TypeOK, MountedIffReferenced, AtMostOneLiveMount, vars

LEMMA NextSafety == KernelAssumptions /\ Safety /\ Next => Safety'
BY AcquireNewSafety, AcquireSharedSafety, ReleaseSomeSafety,
   ReleaseLastSafety, RejectionsPreserveSafety DEF Next

LEMMA SafetyStutter == Safety /\ UNCHANGED vars => Safety'
BY Z3T(30) DEF Safety, TypeOK, MountedIffReferenced,
   AtMostOneLiveMount, vars

InductiveSafety == KernelAssumptions /\ Safety

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_vars => InductiveSafety'
BY NextSafety, SafetyStutter
   DEF InductiveSafety, KernelAssumptions, vars

THEOREM MountCoordinatorSafety ==
    KernelAssumptions /\ Spec => []Safety
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

=======================================================================
