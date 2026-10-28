-------------------- MODULE SessionRunBudgetAdmission --------------------
EXTENDS Naturals, FiniteSets

\* A finite I/O-order composition, not another activity or CAS state machine.
\* The root already exists and is ready, source identities are exact, and both
\* callers request fresh Run activity (continue_committed_await = FALSE).
\* InitialRootRevision is a fixed coordinate offset, NOT an independent clock:
\* every root write advances the one AggregateCAS generation. OpenAt consumes
\* that same committed root coordinate; policy/rejection/settlement may leave
\* gaps between issued activity epochs. SQL/Rust refinement is not asserted.
CONSTANTS Operations, Callers, FastCaller, PolicyWriter, SettlementWriter,
          NoWriter, NoOperation, InitialRootRevision, MaxRootChanges,
          MaxActivityEpoch, ReceiptFault, FaultedOperation

Writers == Callers \cup {PolicyWriter, SettlementWriter}
VARIABLES generation, value, expected, result, resultExpected, resultGeneration,
          lastApplied, lastBefore, lastAfter, lastValueBefore, lastValueAfter,
          activityEpoch, activeEpochs, settled, nextEpoch,
          budgetAdmits, negativeReceipts, calls, policyPhase,
          settlementPhase, settlementOperation, witnessed

CAS == INSTANCE AggregateCAS WITH
    Writers <- Writers, MaxGeneration <- MaxRootChanges, NoWriter <- NoWriter,
    generation <- generation, value <- value, expected <- expected,
    result <- result, resultExpected <- resultExpected,
    resultGeneration <- resultGeneration, lastApplied <- lastApplied,
    lastBefore <- lastBefore, lastAfter <- lastAfter,
    lastValueBefore <- lastValueBefore, lastValueAfter <- lastValueAfter
Activity == INSTANCE SessionActivityKernel WITH
    ActivityIds <- Operations, MaxActivityEpoch <- MaxActivityEpoch,
    kActivityEpoch <- activityEpoch, kActiveActivityEpochs <- activeEpochs,
    kSettledActivities <- settled, kNextActivityEpoch <- nextEpoch

casVars == <<generation, value, expected, result, resultExpected, resultGeneration,
             lastApplied, lastBefore, lastAfter, lastValueBefore, lastValueAfter>>
activityVars == <<activityEpoch, activeEpochs, settled, nextEpoch>>
vars == <<casVars, activityVars, budgetAdmits, negativeReceipts, calls,
          policyPhase, settlementPhase, settlementOperation, witnessed>>
RootRevision == generation + InitialRootRevision
ReadRevision(caller) == expected[caller] + InitialRootRevision

Phases == {"Start", "ObservePositive", "SelectPositive", "ReplayRoot",
           "ObserveNegative", "SelectNegative", "ReadRoot", "Decide",
           "LateRoot", "LateDecide", "Commit", "Ack", "Done"}
Outcomes == {"None", "Accepted", "Rejected", "Pending"}
Causes == {"None", "Receipt", "Raised", "Conflict", "Outage", "LostAck", "Hash"}
Kinds == {"None", "Positive", "Negative"}
Classes == {"None", "Exact", "Mismatch"}
Routes == {"Initial", "PostRoot", "Late"}
Tags == {"OpenCommitted", "SettleCommitted", "SettledReplay",
         "AcceptedAfterStaleBudget", "RaisedPending", "NegativeCommitted",
         "NegativeReplayAfterRaise", "NegativeCASLostToRaise",
         "NegativeCASLostToAccepted", "BeforeWriteOutage", "NegativeAckLost",
         "PositiveAckLost", "AcceptedAfterLostAck", "RejectedAfterLostAck",
         "PositiveHashRefused", "NegativeHashRefused",
         "PositiveStoreReplay", "NegativeStoreReplay"}

ASSUME /\ Cardinality(Callers) = 2 /\ FastCaller \in Callers
       /\ PolicyWriter \notin Callers /\ SettlementWriter \notin Callers
       /\ PolicyWriter # SettlementWriter /\ NoWriter \notin Writers
       /\ NoOperation \notin Operations /\ FaultedOperation \in Operations
       /\ InitialRootRevision \in Nat \ {0}
       /\ MaxRootChanges \in Nat /\ MaxActivityEpoch \in Nat
       /\ ReceiptFault \in Kinds

\* Mismatch represents a present key with a foreign payload, not absence. The
\* concrete hash compiler and insertion race remain the native H1/H2 boundary.
PositiveClass(operation) ==
    IF ReceiptFault = "Positive" /\ operation = FaultedOperation THEN "Mismatch"
    ELSE IF activityEpoch[operation] > 0 THEN "Exact" ELSE "None"
NegativeClass(operation) ==
    IF ReceiptFault = "Negative" /\ operation = FaultedOperation THEN "Mismatch"
    ELSE IF operation \in negativeReceipts THEN "Exact" ELSE "None"
IncomingClass(caller) ==
    IF calls[caller].kind = "Positive"
    THEN PositiveClass(calls[caller].operation)
    ELSE NegativeClass(calls[caller].operation)

EmptyCall == [operation |-> NoOperation, phase |-> "Start", route |-> "Initial",
              positive |-> "None", negative |-> "None", seenEpoch |-> 0,
              readBudget |-> FALSE, projected |-> 0, mustReplay |-> 0,
              staleBudget |-> FALSE, kind |-> "None", outcome |-> "None",
              answerEpoch |-> 0, cause |-> "None", lostAck |-> "None"]
CallStep(caller, after) == calls' = [calls EXCEPT ![caller] = after]
LocalStep(caller, after, tags) ==
    /\ CallStep(caller, after)
    /\ witnessed' = tags
    /\ UNCHANGED <<casVars, activityVars, budgetAdmits, negativeReceipts,
                   policyPhase, settlementPhase, settlementOperation>>
Finish(caller, answer, cause, tags) ==
    LocalStep(caller, [calls[caller] EXCEPT !.phase = "Done",
        !.outcome = answer, !.cause = cause], tags)
Init ==
    /\ CAS!Init /\ Activity!Init
    /\ budgetAdmits = FALSE /\ negativeReceipts = {}
    /\ calls = [caller \in Callers |-> EmptyCall]
    /\ policyPhase = "Read"
    /\ settlementPhase = "Read" /\ settlementOperation = NoOperation
    /\ witnessed = {}

Start(caller, operation) ==
    /\ calls[caller].phase = "Start" /\ operation \in Operations
    /\ LocalStep(caller, [EmptyCall EXCEPT !.operation = operation,
         !.phase = "ObservePositive",
         !.projected = IF caller = FastCaller THEN RootRevision ELSE 0], {})

ObservePositive(caller) ==
    /\ calls[caller].phase = "ObservePositive"
    /\ LocalStep(caller, [calls[caller] EXCEPT !.phase = "SelectPositive",
         !.positive = PositiveClass(calls[caller].operation),
         !.seenEpoch = activityEpoch[calls[caller].operation]], {})

\* Both admission entrances and retries consume this one closed observation.
\* The accepted test uses the durable receipt, never current active membership.
SelectPositive(caller) ==
    /\ calls[caller].phase = "SelectPositive"
    /\ CASE calls[caller].positive = "Mismatch" ->
              Finish(caller, "Pending", "Hash", {"PositiveHashRefused"})
         [] calls[caller].positive = "Exact" ->
              LocalStep(caller, [calls[caller] EXCEPT !.phase = "ReplayRoot"], {})
         [] OTHER ->
              LocalStep(caller, [calls[caller] EXCEPT !.phase = "ObserveNegative"], {})

\* Separate fresh root observation/validation after the exact positive lookup;
\* a settled receipt still returns its original epoch and never opens again.
ReplayRoot(caller) ==
    /\ calls[caller].phase = "ReplayRoot"
    /\ Activity!ReplayOpen(calls[caller].operation)
    /\ CallStep(caller, [calls[caller] EXCEPT !.phase = "Done",
         !.outcome = "Accepted", !.answerEpoch = calls[caller].seenEpoch, !.cause = "Receipt"])
    /\ witnessed' = {}
         \cup (IF calls[caller].operation \in settled THEN {"SettledReplay"} ELSE {})
         \cup (IF calls[caller].staleBudget THEN {"AcceptedAfterStaleBudget"} ELSE {})
         \cup (IF calls[caller].lostAck = "Positive" THEN {"AcceptedAfterLostAck"} ELSE {})
    /\ UNCHANGED <<casVars, budgetAdmits, negativeReceipts, policyPhase,
                   settlementPhase, settlementOperation>>

ObserveNegative(caller) ==
    /\ calls[caller].phase = "ObserveNegative"
    /\ LocalStep(caller, [calls[caller] EXCEPT !.phase = "SelectNegative",
         !.negative = NegativeClass(calls[caller].operation)], {})
SelectNegative(caller) ==
    /\ calls[caller].phase = "SelectNegative"
    /\ CASE calls[caller].negative = "Mismatch" ->
              Finish(caller, "Pending", "Hash", {"NegativeHashRefused"})
         [] calls[caller].negative = "Exact" ->
              Finish(caller, "Rejected", "Receipt",
                (IF budgetAdmits THEN {"NegativeReplayAfterRaise"} ELSE {})
                \cup (IF calls[caller].lostAck = "Negative" THEN {"RejectedAfterLostAck"} ELSE {}))
         [] OTHER ->
              LocalStep(caller, [calls[caller] EXCEPT !.phase =
                CASE calls[caller].route = "Initial" -> "ReadRoot"
                  [] calls[caller].route = "PostRoot" -> "Decide"
                  [] OTHER -> "LateDecide"], {})

ReadRoot(caller) ==
    /\ calls[caller].phase = "ReadRoot"
    /\ CAS!Read(caller)
    /\ calls' = [calls EXCEPT ![caller] = [calls[caller] EXCEPT
         !.phase = "ObservePositive", !.route = "PostRoot",
         !.positive = "None", !.negative = "None", !.seenEpoch = 0,
         !.readBudget = budgetAdmits, !.mustReplay = activityEpoch[calls[caller].operation]]]
    /\ UNCHANGED <<activityVars, budgetAdmits, negativeReceipts, policyPhase,
                   settlementPhase, settlementOperation, witnessed>>

Decide(caller) ==
    /\ calls[caller].phase = "Decide"
    /\ IF caller = FastCaller /\ calls[caller].projected # ReadRevision(caller)
       THEN Finish(caller, "Pending", "Conflict", {})
       ELSE IF calls[caller].readBudget
            THEN LocalStep(caller, [calls[caller] EXCEPT !.phase = "Commit", !.kind = "Positive"], {})
            ELSE LocalStep(caller, [calls[caller] EXCEPT !.phase = "LateRoot", !.staleBudget = TRUE], {})

\* Do not merge this root read with either receipt read or the later CAS.
LateRoot(caller) ==
    /\ calls[caller].phase = "LateRoot"
    /\ CAS!Read(caller)
    /\ calls' = [calls EXCEPT ![caller] = [calls[caller] EXCEPT
         !.phase = "ObservePositive", !.route = "Late",
         !.positive = "None", !.negative = "None", !.seenEpoch = 0,
         !.readBudget = budgetAdmits, !.mustReplay = activityEpoch[calls[caller].operation]]]
    /\ UNCHANGED <<activityVars, budgetAdmits, negativeReceipts, policyPhase,
                   settlementPhase, settlementOperation, witnessed>>
LateDecide(caller) ==
    /\ calls[caller].phase = "LateDecide"
    /\ IF calls[caller].readBudget
       THEN Finish(caller, "Pending", "Raised", {"RaisedPending"})
       ELSE LocalStep(caller, [calls[caller] EXCEPT !.phase = "Commit", !.kind = "Negative"], {})

\* Repository replay checks ONLY the incoming kind's key before the CAS. A
\* positive key must not block a negative commit (or the old bug is hidden).
\* Cross-kind ordering belongs to the application observations above.
StoreReplay(caller) ==
    /\ calls[caller].phase = "Commit" /\ IncomingClass(caller) = "Exact"
    /\ IF calls[caller].kind = "Positive"
       THEN LocalStep(caller, [calls[caller] EXCEPT !.phase = "ReplayRoot",
              !.positive = "Exact", !.seenEpoch = activityEpoch[calls[caller].operation]], {"PositiveStoreReplay"})
       ELSE Finish(caller, "Rejected", "Receipt", {"NegativeStoreReplay"})
StoreMismatch(caller) ==
    /\ calls[caller].phase = "Commit" /\ IncomingClass(caller) = "Mismatch"
    /\ Finish(caller, "Pending", "Hash", {})

StoreWrite(caller) ==
    /\ calls[caller].phase = "Commit" /\ IncomingClass(caller) = "None"
    /\ CAS!Write(caller)
    /\ IF lastApplied'
       THEN /\ IF calls[caller].kind = "Positive"
               THEN /\ Activity!OpenAt(calls[caller].operation,
                                        generation' + InitialRootRevision)
                    /\ UNCHANGED negativeReceipts
               ELSE /\ negativeReceipts' = negativeReceipts \cup {calls[caller].operation}
                    /\ UNCHANGED activityVars
            /\ CallStep(caller, [calls[caller] EXCEPT !.phase = "Ack",
                 !.answerEpoch = IF calls[caller].kind = "Positive" THEN generation' + InitialRootRevision ELSE 0])
            /\ witnessed' =
                 (IF calls[caller].kind = "Positive" THEN {"OpenCommitted"} ELSE {"NegativeCommitted"})
       ELSE /\ UNCHANGED <<activityVars, negativeReceipts>>
            /\ CallStep(caller, [calls[caller] EXCEPT !.phase = "Done", !.outcome = "Pending", !.cause = "Conflict"])
            /\ witnessed' =
                 (IF calls[caller].kind = "Negative" /\ budgetAdmits
                  THEN IF activityEpoch[calls[caller].operation] > 0
                       THEN {"NegativeCASLostToAccepted"} ELSE {"NegativeCASLostToRaise"}
                  ELSE {})
    /\ UNCHANGED <<budgetAdmits, policyPhase, settlementPhase, settlementOperation>>

Ack(caller) ==
    /\ calls[caller].phase = "Ack"
    /\ Finish(caller, IF calls[caller].kind = "Positive" THEN "Accepted" ELSE "Rejected", "Receipt", {})
LoseAck(caller) ==
    /\ calls[caller].phase = "Ack"
    /\ LocalStep(caller, [calls[caller] EXCEPT !.phase = "Done",
         !.outcome = "Pending", !.cause = "LostAck", !.lostAck = calls[caller].kind],
         IF calls[caller].kind = "Positive" THEN {"PositiveAckLost"} ELSE {"NegativeAckLost"})
Outage(caller) ==
    /\ calls[caller].phase \in {"ObservePositive", "ReplayRoot", "ObserveNegative",
                                "ReadRoot", "LateRoot", "Commit"}
    /\ Finish(caller, "Pending", "Outage",
         IF calls[caller].phase = "Commit" THEN {"BeforeWriteOutage"} ELSE {})
Retry(caller) ==
    /\ calls[caller].phase = "Done"
    /\ LocalStep(caller, [EmptyCall EXCEPT !.phase = "ObservePositive",
         !.operation = calls[caller].operation, !.lostAck = calls[caller].lostAck,
         !.projected = IF caller = FastCaller THEN RootRevision ELSE 0], {})

ReadPolicy ==
    /\ policyPhase = "Read" /\ CAS!Read(PolicyWriter)
    /\ policyPhase' = "Commit"
    /\ UNCHANGED <<activityVars, budgetAdmits, negativeReceipts, calls,
                   settlementPhase, settlementOperation, witnessed>>
RaiseBudget ==
    /\ policyPhase = "Commit" /\ CAS!Write(PolicyWriter)
    /\ budgetAdmits' = IF lastApplied' THEN TRUE ELSE budgetAdmits
    /\ policyPhase' = "Done"
    /\ UNCHANGED <<activityVars, negativeReceipts, calls,
                   settlementPhase, settlementOperation, witnessed>>

ReadSettlement(operation) ==
    /\ settlementPhase = "Read" /\ activityEpoch[operation] \in activeEpochs
    /\ CAS!Read(SettlementWriter)
    /\ settlementPhase' = "Commit" /\ settlementOperation' = operation
    /\ UNCHANGED <<activityVars, budgetAdmits, negativeReceipts, calls, policyPhase, witnessed>>
CommitSettlement ==
    /\ settlementPhase = "Commit" /\ CAS!Write(SettlementWriter)
    /\ IF lastApplied'
       THEN /\ Activity!Settle(settlementOperation)
            /\ witnessed' = {"SettleCommitted"}
       ELSE /\ UNCHANGED <<activityVars, witnessed>>
    /\ settlementPhase' = "Done"
    /\ UNCHANGED <<budgetAdmits, negativeReceipts, calls, policyPhase, settlementOperation>>

Next ==
    \/ \E caller \in Callers, operation \in Operations: Start(caller, operation)
    \/ \E caller \in Callers:
         ObservePositive(caller) \/ SelectPositive(caller) \/ ReplayRoot(caller)
         \/ ObserveNegative(caller) \/ SelectNegative(caller) \/ ReadRoot(caller)
         \/ Decide(caller) \/ LateRoot(caller) \/ LateDecide(caller)
         \/ StoreReplay(caller) \/ StoreMismatch(caller) \/ StoreWrite(caller)
         \/ Ack(caller) \/ LoseAck(caller) \/ Outage(caller) \/ Retry(caller)
    \/ ReadPolicy \/ RaiseBudget
    \/ \E operation \in Operations: ReadSettlement(operation)
    \/ CommitSettlement
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ Activity!TypeOK /\ CAS!TypeOK
    /\ budgetAdmits \in BOOLEAN /\ negativeReceipts \subseteq Operations
    /\ policyPhase \in {"Read", "Commit", "Done"}
    /\ settlementPhase \in {"Read", "Commit", "Done"}
    /\ settlementOperation \in Operations \cup {NoOperation}
    /\ witnessed \subseteq Tags
    /\ calls \in [Callers -> [operation : Operations \cup {NoOperation}, phase : Phases,
         route : Routes, positive : Classes, negative : Classes, seenEpoch : 0..MaxActivityEpoch,
         readBudget : BOOLEAN, projected : 0..(InitialRootRevision + MaxRootChanges),
         mustReplay : 0..MaxActivityEpoch, staleBudget : BOOLEAN, kind : Kinds,
         outcome : Outcomes, answerEpoch : 0..MaxActivityEpoch, cause : Causes, lostAck : Kinds]]
NoAcceptedNegativePair == \A operation \in negativeReceipts: activityEpoch[operation] = 0
ReceiptEpochUsesRootCoordinate ==
    /\ \A operation \in Operations: activityEpoch[operation] <= RootRevision
    /\ nextEpoch <= RootRevision + 1
RejectedAnswerHasDurableReceipt ==
    \A caller \in Callers: calls[caller].outcome = "Rejected" =>
        calls[caller].operation \in negativeReceipts
AcceptedAnswerHasExactReceipt ==
    \A caller \in Callers: calls[caller].outcome = "Accepted" =>
        /\ calls[caller].answerEpoch > 0
        /\ calls[caller].answerEpoch = activityEpoch[calls[caller].operation]
AcceptedObservationIsNotDiscarded ==
    \A caller \in Callers: calls[caller].positive = "Exact" =>
        calls[caller].phase \in {"SelectPositive", "ReplayRoot", "Done"}
HealthyClosedReadReplaysAccepted ==
    \A caller \in Callers:
        calls[caller].phase = "Done" /\ calls[caller].mustReplay > 0
          /\ calls[caller].cause \notin {"Outage", "Hash"} =>
            /\ calls[caller].outcome = "Accepted"
            /\ calls[caller].answerEpoch = calls[caller].mustReplay
RaisedDecisionCannotCloseRefusal ==
    \A caller \in Callers:
        calls[caller].phase = "Done" /\ calls[caller].cause = "Raised" =>
            /\ calls[caller].outcome = "Pending" /\ calls[caller].answerEpoch = 0
UncertaintyCannotReportRefusal ==
    \A caller \in Callers: calls[caller].cause \in {"Conflict", "Outage", "LostAck", "Hash"} =>
        calls[caller].outcome = "Pending"
CorruptReceiptNeverBecomesAbsence ==
    ReceiptFault # "None" =>
        /\ activityEpoch[FaultedOperation] = 0
        /\ FaultedOperation \notin negativeReceipts

\* This state/action frame checks the actual joint result, not just enabledness.
ActivityCommitUsesExactRootRevision == [][
    \A operation \in Operations:
        activityEpoch[operation] = 0 /\ activityEpoch'[operation] > 0 =>
            activityEpoch'[operation] = generation' + InitialRootRevision
]_vars
NegativeReceiptIsAbsorbing == [][negativeReceipts \subseteq negativeReceipts']_vars
RejectedCASPreservesDurableState == [][
    generation' = generation =>
        UNCHANGED <<activityVars, budgetAdmits, negativeReceipts>>
]_vars
NewRefusalWasBudgetRejected == [][
    \A operation \in negativeReceipts' \ negativeReceipts:
        \E caller \in Callers:
            /\ calls[caller].phase = "Commit" /\ calls[caller].kind = "Negative"
            /\ calls[caller].operation = operation /\ ~calls[caller].readBudget
]_vars
Safety == /\ TypeOK /\ Activity!Safety /\ CAS!GenerationZeroHasNoValue
          /\ CAS!AppliedMatchedGeneration /\ CAS!LastWriteIsAtomicCAS
          /\ CAS!RejectedWritePreservesValue /\ NoAcceptedNegativePair
          /\ ReceiptEpochUsesRootCoordinate /\ RejectedAnswerHasDurableReceipt
          /\ AcceptedAnswerHasExactReceipt /\ AcceptedObservationIsNotDiscarded
          /\ HealthyClosedReadReplaysAccepted /\ RaisedDecisionCannotCloseRefusal
          /\ UncertaintyCannotReportRefusal /\ CorruptReceiptNeverBecomesAbsence

\* Tags describe the latest tracked event, not an accumulating parallel log.
\* Inverse invariants are expected to FAIL on the unmodified model. They are
\* positive reachability controls, never proof that a real provider progresses.
NoOpenCommit == "OpenCommitted" \notin witnessed
NoSettleCommit == "SettleCommitted" \notin witnessed
NoSettledReplay == "SettledReplay" \notin witnessed
NoAcceptedAfterStaleBudget == "AcceptedAfterStaleBudget" \notin witnessed
NoRaisedPending == "RaisedPending" \notin witnessed
NoNegativeAfterRaise == "NegativeReplayAfterRaise" \notin witnessed
NoNegativeCASLossToRaise == "NegativeCASLostToRaise" \notin witnessed
NoNegativeCASLossToAccepted == "NegativeCASLostToAccepted" \notin witnessed
NoBeforeWriteOutage == "BeforeWriteOutage" \notin witnessed
NoNegativeLostAckRecovery == "RejectedAfterLostAck" \notin witnessed
NoPositiveLostAckRecovery == "AcceptedAfterLostAck" \notin witnessed
NoPositiveHashRefusal == "PositiveHashRefused" \notin witnessed
NoNegativeHashRefusal == "NegativeHashRefused" \notin witnessed
NoPositiveStoreReplay == "PositiveStoreReplay" \notin witnessed
NoNegativeStoreReplay == "NegativeStoreReplay" \notin witnessed
NoFastOpenCommit == ~(calls[FastCaller].phase = "Ack" /\ calls[FastCaller].kind = "Positive")
NoWorkerOpenCommit == \A caller \in Callers \ {FastCaller}:
    ~(calls[caller].phase = "Ack" /\ calls[caller].kind = "Positive")
NoFastNegativeCommit == ~(calls[FastCaller].phase = "Ack" /\ calls[FastCaller].kind = "Negative")
NoWorkerNegativeCommit == \A caller \in Callers \ {FastCaller}:
    ~(calls[caller].phase = "Ack" /\ calls[caller].kind = "Negative")
=============================================================================
