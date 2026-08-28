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

\* Proof design follows the causal effects of Settle: it changes only active
\* membership and durable settlement membership.  Keep each invariant as an
\* independently checked obligation so a future state-axis change cannot hide
\* behind one large, timeout-prone SMT query.
LEMMA SettlePreservesActivityEpochType ==
    \A activity \in ActivityIds:
      TypeOK /\ Settle(activity) =>
        kActivityEpoch' \in [ActivityIds -> 0..MaxActivityEpoch]
BY Z3T(30) DEF Settle, TypeOK

LEMMA RemovingOneElementPreservesSubset ==
    \A elements, range, removed:
      elements \subseteq range => elements \ {removed} \subseteq range
BY Z3T(30)

LEMMA SettlePreservesActiveEpochType ==
    \A activity \in ActivityIds:
      TypeOK /\ Settle(activity) =>
        kActiveActivityEpochs' \subseteq 1..MaxActivityEpoch
BY RemovingOneElementPreservesSubset DEF Settle, TypeOK

LEMMA SettlePreservesSettledActivityType ==
    \A activity \in ActivityIds:
      TypeOK /\ Settle(activity) =>
        kSettledActivities' \subseteq ActivityIds
BY Z3T(30) DEF Settle, TypeOK

LEMMA SettlePreservesNextEpochType ==
    \A activity \in ActivityIds:
      TypeOK /\ Settle(activity) =>
        kNextActivityEpoch' \in 1..(MaxActivityEpoch + 1)
BY Z3T(30) DEF Settle, TypeOK

LEMMA SettleTypeOK ==
    \A activity \in ActivityIds:
      TypeOK /\ Settle(activity) => TypeOK'
BY SettlePreservesActivityEpochType, SettlePreservesActiveEpochType,
   SettlePreservesSettledActivityType, SettlePreservesNextEpochType DEF TypeOK

LEMMA SettlePreservesReceiptInjectivity ==
    \A activity \in ActivityIds:
      ReceiptsAreInjective /\ Settle(activity) => ReceiptsAreInjective'
BY Z3T(30) DEF Settle, ReceiptsAreInjective

LEMMA SettlePreservesActiveReceipt ==
    \A activity \in ActivityIds:
      ActiveEpochHasExactReceipt /\ Settle(activity) =>
        ActiveEpochHasExactReceipt'
BY Z3T(30) DEF Settle, ActiveEpochHasExactReceipt

LEMMA SettlePublishesDurableReceipt ==
    \A activity \in ActivityIds:
      TypeOK /\ SettledActivityHasDurableReceipt /\ Settle(activity) =>
        SettledActivityHasDurableReceipt'
BY Z3T(30) DEF Settle, TypeOK, SettledActivityHasDurableReceipt

LEMMA SettlePreservesIssuedEpochMonotonicity ==
    \A activity \in ActivityIds:
      IssuedEpochsAreMonotonic /\ Settle(activity) =>
        IssuedEpochsAreMonotonic'
BY Z3T(30) DEF Settle, IssuedEpochsAreMonotonic

LEMMA SettleSafety ==
    \A activity \in ActivityIds:
      KernelAssumptions /\ Safety /\ Settle(activity) => Safety'
BY SettleTypeOK, SettlePreservesReceiptInjectivity,
   SettlePreservesActiveReceipt, SettlePublishesDurableReceipt,
   SettlePreservesIssuedEpochMonotonicity DEF Safety

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
