----------------------- MODULE SessionActivityProof -----------------------
EXTENDS SessionActivityKernel, TLAPS

LEMMA InitSafety == KernelAssumptions /\ Init => Safety
BY Z3T(30) DEF Init, Safety, TypeOK, ReceiptsAreInjective,
   ActiveEpochHasExactReceipt, SettledActivityHasDurableReceipt,
   IssuedEpochsAreMonotonic, KernelAssumptions

LEMMA OpenSafety ==
    \A activity \in ActivityIds:
      KernelAssumptions /\ Safety /\ Open(activity) => Safety'
BY Z3T(30) DEF Open, Safety, TypeOK, ReceiptsAreInjective,
   ActiveEpochHasExactReceipt, SettledActivityHasDurableReceipt,
   IssuedEpochsAreMonotonic, KernelAssumptions

LEMMA ReplayOpenSafety ==
    \A activity \in ActivityIds:
      KernelAssumptions /\ Safety /\ ReplayOpen(activity) => Safety'
BY Z3T(30) DEF ReplayOpen, Safety, TypeOK, ReceiptsAreInjective,
   ActiveEpochHasExactReceipt, SettledActivityHasDurableReceipt,
   IssuedEpochsAreMonotonic, KernelAssumptions, kVars

LEMMA SettleSafety ==
    \A activity \in ActivityIds:
      KernelAssumptions /\ Safety /\ Settle(activity) => Safety'
BY Z3T(30) DEF Settle, Safety, TypeOK, ReceiptsAreInjective,
   ActiveEpochHasExactReceipt, SettledActivityHasDurableReceipt,
   IssuedEpochsAreMonotonic, KernelAssumptions

LEMMA ReplaySettleSafety ==
    \A activity \in ActivityIds:
      KernelAssumptions /\ Safety /\ ReplaySettle(activity) => Safety'
BY Z3T(30) DEF ReplaySettle, Safety, TypeOK, ReceiptsAreInjective,
   ActiveEpochHasExactReceipt, SettledActivityHasDurableReceipt,
   IssuedEpochsAreMonotonic, KernelAssumptions, kVars

LEMMA NextSafety == KernelAssumptions /\ Safety /\ Next => Safety'
BY OpenSafety, ReplayOpenSafety, SettleSafety, ReplaySettleSafety DEF Next

LEMMA SafetyStutter == Safety /\ UNCHANGED kVars => Safety'
BY Z3T(30) DEF Safety, TypeOK, ReceiptsAreInjective,
   ActiveEpochHasExactReceipt, SettledActivityHasDurableReceipt,
   IssuedEpochsAreMonotonic, kVars

InductiveSafety == KernelAssumptions /\ Safety

LEMMA InitInductiveSafety == KernelAssumptions /\ Init => InductiveSafety
BY InitSafety DEF InductiveSafety

LEMMA StepInductiveSafety ==
    InductiveSafety /\ [Next]_kVars => InductiveSafety'
BY NextSafety, SafetyStutter
   DEF InductiveSafety, KernelAssumptions, kVars

THEOREM SessionActivitySafety ==
    KernelAssumptions /\ Spec => []Safety
BY InitInductiveSafety, StepInductiveSafety, PTL
   DEF Spec, InductiveSafety

=============================================================================
