----------------- MODULE ObservationReconcileProof -----------------
EXTENDS ObservationReconcile, TLAPS

LEMMA InitTypeOK == KernelAssumptions /\ Init => TypeOK
<1>1 ASSUME KernelAssumptions, Init
     PROVE TypeOK
  <2>1 evidence \in EvidenceValues /\
        sourceEpoch \in 0..MaxEpoch /\
        publishedEvidence \in EvidenceValues /\
        publishedEpoch \in 0..MaxEpoch /\
        hasFence \in BOOLEAN /\
        lastFence \in 0..MaxEpoch
    BY <1>1 DEF KernelAssumptions, Init
  <2>2 captured \in [Callers -> 0..MaxEpoch]
    BY <1>1 DEF KernelAssumptions, Init
  <2>3 phase \in [Callers -> Phases]
    BY <1>1 DEF Init, Phases
  <2>4 lockOwner \in Callers \cup {NoCaller}
    BY <1>1 DEF Init
  <2>5 QED
    BY <2>1, <2>2, <2>3, <2>4 DEF TypeOK
<1>2 QED
  BY <1>1

LEMMA InitCapturedNeverFuture ==
    KernelAssumptions /\ Init => CapturedNeverFuture
BY DEF KernelAssumptions, Init, CapturedNeverFuture

LEMMA InitPublishedNeverFuture ==
    KernelAssumptions /\ Init => PublishedNeverFuture
BY Z3T(30) DEF KernelAssumptions, Init, PublishedNeverFuture

LEMMA InitPublishedCurrent ==
    KernelAssumptions /\ Init => PublishedCurrentWhenCaughtUp
BY Z3T(30) DEF KernelAssumptions, Init, PublishedCurrentWhenCaughtUp

LEMMA InitFence == KernelAssumptions /\ Init => FenceNeverAheadOfPublication
BY Z3T(30) DEF KernelAssumptions, Init, FenceNeverAheadOfPublication

LEMMA InitMutex == KernelAssumptions /\ Init => MutexCoherent
BY DEF KernelAssumptions, Init, MutexCoherent

LEMMA InitSafety == KernelAssumptions /\ Init => Safety
BY InitTypeOK, InitCapturedNeverFuture, InitPublishedNeverFuture,
   InitPublishedCurrent, InitFence, InitMutex DEF Safety

LEMMA EvidenceChangesSafety ==
    \A nextEvidence \in EvidenceValues:
      KernelAssumptions /\ Safety /\ EvidenceChanges(nextEvidence) => Safety'
BY Z3T(30) DEF KernelAssumptions, EvidenceChanges, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases

LEMMA CaptureSafety ==
    \A caller \in Callers:
      KernelAssumptions /\ Safety /\ Capture(caller) => Safety'
BY Z3T(30) DEF KernelAssumptions, Capture, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases

LEMMA SkipSafety ==
    \A caller \in Callers:
      KernelAssumptions /\ Safety /\ Skip(caller) => Safety'
BY Z3T(30) DEF KernelAssumptions, Skip, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases

LEMMA StartReconcileSafety ==
    \A caller \in Callers:
      KernelAssumptions /\ Safety /\ StartReconcile(caller) => Safety'
BY Z3T(30) DEF KernelAssumptions, StartReconcile, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases

LEMMA ReconcileSucceedsSafety ==
    \A caller \in Callers:
      KernelAssumptions /\ Safety /\ ReconcileSucceeds(caller) => Safety'
BY Z3T(30) DEF KernelAssumptions, ReconcileSucceeds, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases

LEMMA ReconcileFailsSafety ==
    \A caller \in Callers:
      KernelAssumptions /\ Safety /\ ReconcileFails(caller) => Safety'
BY Z3T(30) DEF KernelAssumptions, ReconcileFails, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases

LEMMA ResetSafety ==
    \A caller \in Callers:
      KernelAssumptions /\ Safety /\ Reset(caller) => Safety'
BY Z3T(30) DEF KernelAssumptions, Reset, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases

LEMMA UnchangedHeartbeatSafety == Safety /\ UnchangedHeartbeat => Safety'
BY Z3T(30) DEF UnchangedHeartbeat, Safety, TypeOK,
   CapturedNeverFuture, PublishedNeverFuture,
   PublishedCurrentWhenCaughtUp, FenceNeverAheadOfPublication,
   MutexCoherent, Phases, vars

LEMMA NextSafety == KernelAssumptions /\ Safety /\ Next => Safety'
BY EvidenceChangesSafety, CaptureSafety, SkipSafety, StartReconcileSafety,
   ReconcileSucceedsSafety, ReconcileFailsSafety, ResetSafety,
   UnchangedHeartbeatSafety DEF Next

LEMMA SafetyStutter == Safety /\ UNCHANGED vars => Safety'
BY Z3T(30) DEF Safety, TypeOK, CapturedNeverFuture,
   PublishedNeverFuture, PublishedCurrentWhenCaughtUp,
   FenceNeverAheadOfPublication, MutexCoherent, Phases, vars

InductiveSafety == KernelAssumptions /\ Safety

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_vars => InductiveSafety'
BY NextSafety, SafetyStutter
   DEF InductiveSafety, KernelAssumptions, vars

THEOREM ObservationReconcileSafety ==
    KernelAssumptions /\ Spec => []Safety
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

\* The operational meaning of FenceNeverAheadOfPublication: every coalesced
\* caller names an observation epoch already published or superseded.
THEOREM SkipRequiresCoveredObservation ==
    \A caller \in Callers:
      Safety /\ Skip(caller) => captured[caller] <= publishedEpoch
BY Z3T(30) DEF Safety, FenceNeverAheadOfPublication, Skip

THEOREM FailedReconcileDoesNotAdvanceFence ==
    \A caller \in Callers:
      ReconcileFails(caller) =>
        UNCHANGED <<publishedEvidence, publishedEpoch, hasFence, lastFence>>
BY DEF ReconcileFails

======================================================================
