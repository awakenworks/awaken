--------------------------- MODULE WorkQueueProof ---------------------------
EXTENDS WorkQueue, TLAPS

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
    \A item \in WorkItems:
      KernelAssumptions /\ Safety /\ Ack(item) => Safety'
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
    \A item \in WorkItems:
      KernelAssumptions /\ Safety /\ Stop(item) => Safety'
BY Z3T(30) DEF Stop, Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, WorkStates,
   TerminalStates, KernelAssumptions

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

LEMMA SafetyStutter == Safety /\ UNCHANGED vars => Safety'
BY Z3T(30) DEF Safety, TypeOK, SingleActive, ActiveHasOneOwner,
   ActiveEpochIsPositive, TerminalHasNoLeaseAuthority, vars

InductiveSafety == KernelAssumptions /\ Safety

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_vars => InductiveSafety'
BY NextSafety, SafetyStutter
   DEF InductiveSafety, KernelAssumptions, vars

THEOREM WorkQueueSafety ==
    KernelAssumptions /\ Spec => []Safety
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

=============================================================================
