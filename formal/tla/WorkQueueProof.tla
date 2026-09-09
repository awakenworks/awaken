--------------------------- MODULE WorkQueueProof ---------------------------
EXTENDS WorkQueueKernel, TLAPS

LEMMA InitSafety == KernelAssumptions /\ Init => Safety
BY Z3T(30) DEF Init, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, NoActive, KernelAssumptions

LEMMA ClaimSafety ==
    \A worker \in Workers, item \in WorkItems:
      KernelAssumptions /\ Safety /\ Claim(worker, item) => Safety'
BY Z3T(30) DEF Claim, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, NoActive, KernelAssumptions

LEMMA ReclaimSafety ==
    \A worker \in Workers, item \in WorkItems:
      KernelAssumptions /\ Safety /\ Reclaim(worker, item) => Safety'
BY Z3T(30) DEF Reclaim, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, KernelAssumptions

LEMMA AckSafety ==
    \A worker \in Workers, item \in WorkItems:
      KernelAssumptions /\ Safety /\ Ack(worker, item) => Safety'
BY Z3T(30) DEF Ack, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, KernelAssumptions

LEMMA HeartbeatSafety ==
    \A worker \in Workers, item \in WorkItems, expected \in 0..MaxHeartbeat:
      KernelAssumptions /\ Safety /\ Heartbeat(worker, item, expected) => Safety'
BY Z3T(30) DEF Heartbeat, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, KernelAssumptions

LEMMA StopSafety ==
    \A worker \in Workers, item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
      KernelAssumptions /\ Safety /\ Stop(worker, item, expectedEpoch) => Safety'
BY Z3T(30) DEF Stop, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, KernelAssumptions

\* Cause/effect rows S1..S4 quantify over request identity separately from
\* state invariants. Deleting an admission guard must refute its binding or
\* noninterference theorem even if StopSafety continues to hold.
LEMMA StopEffectBindsExactRequest ==
    \A worker \in Workers, item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
      Stop(worker, item, expectedEpoch) => ExactStopRequest(worker, item, expectedEpoch)
BY Z3T(30) DEF Stop, ExactStopRequest

LEMMA WrongOwnerStopCannotChangeAuthority ==
    \A worker \in Workers, item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
      kOwner[item] # worker /\ [Stop(worker, item, expectedEpoch)]_kVars =>
        UNCHANGED kVars
BY Z3T(30) DEF Stop, kVars

LEMMA WrongEpochStopCannotChangeAuthority ==
    \A worker \in Workers, item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
      kEpoch[item] # expectedEpoch /\ [Stop(worker, item, expectedEpoch)]_kVars =>
        UNCHANGED kVars
BY Z3T(30) DEF Stop, kVars

LEMMA ExactStopClearsOnlyAddressedAuthority ==
    \A worker \in Workers, item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
      TypeOK /\ Stop(worker, item, expectedEpoch) =>
        /\ kState'[item] = "Stopped"
        /\ kOwner'[item] = NoWorker
        /\ kEpoch'[item] = expectedEpoch
        /\ kHeartbeat'[item] = 0
        /\ kExpires'[item] = 0
        /\ StopFramesOtherRows(item)
        /\ UNCHANGED kNow
BY Z3T(30) DEF Stop, StopFramesOtherRows, TypeOK

LEMMA StoppedCannotApplyAnotherStop ==
    \A worker \in Workers, item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
      KernelAssumptions /\ Safety /\ kState[item] = "Stopped" =>
        ~Stop(worker, item, expectedEpoch)
BY Z3T(30) DEF Stop, KernelAssumptions, Safety, TerminalHasNoLeaseAuthority,
   TerminalStates

LEMMA StoppedStutterKeepsItsEpoch ==
    \A item \in WorkItems, expectedEpoch \in 0..MaxEpoch:
      kState[item] = "Stopped" /\ kEpoch[item] = expectedEpoch /\ UNCHANGED kVars =>
        /\ [Next]_kVars
        /\ kState'[item] = "Stopped"
        /\ kEpoch'[item] = expectedEpoch
BY Z3T(30) DEF kVars

THEOREM EveryStopEffectHasExactRequest == StopRequestsAreBound
BY StopEffectBindsExactRequest, PTL
   DEF StopRequestsAreBound, StopRequestBinding, kVars

LEMMA RemoveEnvironmentSafety ==
    KernelAssumptions /\ Safety /\ RemoveEnvironment => Safety'
BY Z3T(30) DEF RemoveEnvironment, Safety, TypeOK, SingleActive,
   ActiveHasOneOwner, ActiveEpochIsPositive, TerminalHasNoLeaseAuthority,
   WorkStates, TerminalStates, KernelAssumptions

LEMMA AdvanceTimeSafety ==
    KernelAssumptions /\ Safety /\ AdvanceTime => Safety'
BY Z3T(30) DEF AdvanceTime, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, KernelAssumptions

LEMMA NextSafety == KernelAssumptions /\ Safety /\ Next => Safety'
BY ClaimSafety, ReclaimSafety, AckSafety, HeartbeatSafety, StopSafety,
   RemoveEnvironmentSafety, AdvanceTimeSafety DEF Next

LEMMA SafetyStutter == Safety /\ UNCHANGED kVars => Safety'
BY Z3T(30) DEF Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, kVars

InductiveSafety == KernelAssumptions /\ Safety

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_kVars => InductiveSafety'
BY NextSafety, SafetyStutter
   DEF InductiveSafety, KernelAssumptions, kVars

THEOREM WorkQueueSafety ==
    KernelAssumptions /\ Spec => []Safety
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

=============================================================================
