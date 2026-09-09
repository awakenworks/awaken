----------------------- MODULE ToolResultProtocol -----------------------
EXTENDS Naturals, FiniteSets, TLC

\* Calls are public tool-request occurrences. Operations are immutable delivery
\* attempts retained by the Session Event batch and ResumeApplied audit fact.
CONSTANT Calls, PendingCommits, Operations, Keys, Fingerprints, None,
         MaxRejected, MaxConflicts
ASSUME /\ Calls # {} /\ PendingCommits # {} /\ Operations # {}
       /\ Keys # {} /\ Fingerprints # {}
       /\ MaxRejected \in Nat /\ MaxConflicts \in Nat

VARIABLE awaiting, awaitingCommit, completed, receiptCall, receiptCommit,
         accepted, rejected, lastAccepted, lastExpected, lastAcceptedCall, lastExpectedCall,
         lastAcceptedCommit, lastExpectedCommit, terminal,
         batchFingerprint, batchAppends, batchConflicts

vars == <<awaiting, awaitingCommit, completed, receiptCall, receiptCommit,
          accepted, rejected, lastAccepted, lastExpected, lastAcceptedCall, lastExpectedCall,
          lastAcceptedCommit, lastExpectedCommit, terminal,
          batchFingerprint, batchAppends, batchConflicts>>

Init == /\ awaiting = None
        /\ awaitingCommit = None
        /\ completed = {}
        /\ receiptCall = [op \in Operations |-> None]
        /\ receiptCommit = [op \in Operations |-> None]
        /\ accepted = [op \in Operations |-> 0]
        /\ rejected = 0
        /\ lastAccepted = None
        /\ lastExpected = None
        /\ lastAcceptedCall = None
        /\ lastExpectedCall = None
        /\ lastAcceptedCommit = None
        /\ lastExpectedCommit = None
        /\ terminal = FALSE
        /\ batchFingerprint = [key \in Keys |-> None]
        /\ batchAppends = [key \in Keys |-> 0]
        /\ batchConflicts = 0

ConsumedPairs == {<<receiptCall[op], receiptCommit[op]>> : op \in completed}

Await(call, pendingCommit) ==
    /\ call \in Calls
    /\ pendingCommit \in PendingCommits
    /\ ~terminal
    /\ awaiting = None
    /\ <<call, pendingCommit>> \notin ConsumedPairs
    /\ awaiting' = call
    /\ awaitingCommit' = pendingCommit
    /\ UNCHANGED <<completed, receiptCall, receiptCommit, accepted, rejected,
                    lastAccepted, lastExpected, lastAcceptedCall, lastExpectedCall, lastAcceptedCommit,
                    lastExpectedCommit, terminal,
                    batchFingerprint, batchAppends, batchConflicts>>

\* Ticket consumption and the durable operation receipt are one transition.
\* The retained pending commit is part of the identity: a later Awaiting
\* occurrence may reuse the Runtime call id but cannot consume this receipt.
Deliver(call, pendingCommit, op) ==
    /\ call \in Calls
    /\ pendingCommit \in PendingCommits
    /\ op \in Operations
    /\ ~terminal
    /\ awaiting = call
    /\ awaitingCommit = pendingCommit
    /\ op \notin completed
    /\ awaiting' = None
    /\ awaitingCommit' = None
    /\ completed' = completed \cup {op}
    /\ receiptCall' = [receiptCall EXCEPT ![op] = call]
    /\ receiptCommit' = [receiptCommit EXCEPT ![op] = pendingCommit]
    /\ accepted' = [accepted EXCEPT ![op] = @ + 1]
    /\ lastAccepted' = op
    /\ lastExpected' = op
    /\ lastAcceptedCall' = call
    /\ lastExpectedCall' = awaiting
    /\ lastAcceptedCommit' = pendingCommit
    /\ lastExpectedCommit' = awaitingCommit
    /\ UNCHANGED <<rejected, terminal, batchFingerprint,
                    batchAppends, batchConflicts>>

\* A response-loss retry observes the exact durable receipt and stutters. It
\* neither needs nor recreates the consumed Awaiting ticket.
Replay(call, pendingCommit, op) ==
    /\ call \in Calls
    /\ pendingCommit \in PendingCommits
    /\ op \in completed
    /\ receiptCall[op] = call
    /\ receiptCommit[op] = pendingCommit
    /\ UNCHANGED vars

Reject(call, pendingCommit, op) ==
    /\ call \in Calls
    /\ pendingCommit \in PendingCommits
    /\ op \in Operations
    /\ rejected < MaxRejected
    /\ \/ terminal
       \/ /\ op \in completed
          /\ \/ receiptCall[op] # call
             \/ receiptCommit[op] # pendingCommit
       \/ /\ op \notin completed
          /\ \/ awaiting # call
             \/ awaitingCommit # pendingCommit
    /\ rejected' = rejected + 1
    /\ UNCHANGED <<awaiting, awaitingCommit, completed, receiptCall,
                    receiptCommit, accepted, lastAccepted, lastExpected, lastAcceptedCall, lastExpectedCall,
                    lastAcceptedCommit, lastExpectedCommit, terminal,
                    batchFingerprint, batchAppends, batchConflicts>>

\* Session Event ingress uses the same root-CAS principle as ToolReply. A new
\* key durably binds one request fingerprint in the same transition that
\* appends its batch. Exact transport retries stutter; changed retries only
\* record a bounded rejected observation and can never append another batch.
AppendBatch(key, fingerprint) ==
    /\ key \in Keys
    /\ fingerprint \in Fingerprints
    /\ batchFingerprint[key] = None
    /\ batchFingerprint' = [batchFingerprint EXCEPT ![key] = fingerprint]
    /\ batchAppends' = [batchAppends EXCEPT ![key] = @ + 1]
    /\ UNCHANGED <<awaiting, awaitingCommit, completed, receiptCall,
                    receiptCommit, accepted, rejected, lastAccepted,
                    lastExpected, lastAcceptedCall, lastExpectedCall, lastAcceptedCommit, lastExpectedCommit,
                    terminal, batchConflicts>>

\* One retry decision is consumed by the original batch actions and the
\* production observation relation. Closed is retained non-admission, never
\* an executable receipt. An absent post-failure read preserves that failure.
BatchRetryDecision(present, matchesFingerprint, closed) ==
    IF ~present THEN "unavailable"
    ELSE IF ~matchesFingerprint THEN "conflict"
    ELSE IF closed THEN "closed" ELSE "exact"

BatchRetryObservation(o) ==
    /\ o.result = BatchRetryDecision(o.present, o.fingerprint = o.requested, o.closed)
    /\ o.before = o.after /\ o.cache_unchanged
    /\ IF o.result = "exact"
          THEN o.receipt = o.expected_receipt /\ o.receipt # "null" /\ o.error = "none"
          ELSE /\ o.receipt = "null"
               /\ o.error = CASE o.result = "conflict" -> "idempotency_conflict"
                                  [] o.result = "closed" -> "event_admission_closed"
                                  [] OTHER -> o.failure

ReplayBatch(key, fingerprint) ==
    /\ key \in Keys
    /\ fingerprint \in Fingerprints
    /\ BatchRetryDecision(batchFingerprint[key] # None, batchFingerprint[key] = fingerprint, batchAppends[key] = 0) \in {"exact", "closed"}
    /\ UNCHANGED vars

\* Final non-admission occupies the SAME key/fingerprint slot. batchAppends
\* counts executable acceptance, not the empty root receipt used to close it.
\* A delayed AppendBatch is disabled after this competing root CAS wins.
CloseBatch(key, fingerprint) ==
    /\ key \in Keys
    /\ fingerprint \in Fingerprints
    /\ batchFingerprint[key] = None
    /\ batchFingerprint' = [batchFingerprint EXCEPT ![key] = fingerprint]
    /\ UNCHANGED <<awaiting, awaitingCommit, completed, receiptCall,
                    receiptCommit, accepted, rejected, lastAccepted,
                    lastExpected, lastAcceptedCall, lastExpectedCall, lastAcceptedCommit, lastExpectedCommit,
                    terminal, batchAppends, batchConflicts>>

ConflictBatch(key, fingerprint) ==
    /\ key \in Keys
    /\ fingerprint \in Fingerprints
    /\ BatchRetryDecision(batchFingerprint[key] # None, batchFingerprint[key] = fingerprint, batchAppends[key] = 0) = "conflict"
    /\ batchConflicts < MaxConflicts
    /\ batchConflicts' = batchConflicts + 1
    /\ UNCHANGED <<awaiting, awaitingCommit, completed, receiptCall,
                    receiptCommit, accepted, rejected, lastAccepted,
                    lastExpected, lastAcceptedCall, lastExpectedCall, lastAcceptedCommit, lastExpectedCommit, terminal,
                    batchFingerprint, batchAppends>>

End == /\ ~terminal
       /\ terminal' = TRUE
       /\ awaiting' = None
       /\ awaitingCommit' = None
       /\ UNCHANGED <<completed, receiptCall, receiptCommit, accepted, rejected,
                       lastAccepted, lastExpected, lastAcceptedCall, lastExpectedCall, lastAcceptedCommit,
                       lastExpectedCommit, batchFingerprint,
                       batchAppends, batchConflicts>>

Next ==
    (\E call \in Calls, pendingCommit \in PendingCommits, op \in Operations:
        Deliver(call, pendingCommit, op) \/ Replay(call, pendingCommit, op)
        \/ Reject(call, pendingCommit, op))
    \/ (\E call \in Calls, pendingCommit \in PendingCommits:
        Await(call, pendingCommit))
    \/ (\E key \in Keys, fingerprint \in Fingerprints:
        AppendBatch(key, fingerprint) \/ ReplayBatch(key, fingerprint)
        \/ ConflictBatch(key, fingerprint) \/ CloseBatch(key, fingerprint))
    \/ End

TypeOK ==
    /\ awaiting \in Calls \cup {None}
    /\ awaitingCommit \in PendingCommits \cup {None}
    /\ completed \subseteq Operations
    /\ receiptCall \in [Operations -> Calls \cup {None}]
    /\ receiptCommit \in [Operations -> PendingCommits \cup {None}]
    /\ accepted \in [Operations -> 0..1]
    /\ rejected \in 0..MaxRejected
    /\ lastAccepted \in Operations \cup {None}
    /\ lastExpected \in Operations \cup {None}
    /\ lastAcceptedCall \in Calls \cup {None}
    /\ lastExpectedCall \in Calls \cup {None}
    /\ lastAcceptedCommit \in PendingCommits \cup {None}
    /\ lastExpectedCommit \in PendingCommits \cup {None}
    /\ terminal \in BOOLEAN
    /\ batchFingerprint \in [Keys -> Fingerprints \cup {None}]
    /\ batchAppends \in [Keys -> 0..1]
    /\ batchConflicts \in 0..MaxConflicts

\* Observe the delivered call and PRE-transition awaiting call independently.
\* Equating operation ids assigned from the same input cannot detect a removed
\* call guard; exact call and pending-commit coordinates must both agree.
OnlyMatchingResultIsAccepted ==
    lastAccepted # None =>
        /\ lastAccepted = lastExpected
        /\ lastAcceptedCall = lastExpectedCall
        /\ lastAcceptedCommit = lastExpectedCommit

EveryOperationIsConsumedAtMostOnce ==
    \A op \in Operations: accepted[op] <= 1

ReceiptExistsExactlyForConsumedOperation ==
    \A op \in Operations:
        (op \in completed) \equiv
            (receiptCall[op] # None /\ receiptCommit[op] # None)

OneReceiptNeverNamesTwoCalls ==
    \A op \in completed:
        receiptCall[op] \in Calls /\ receiptCommit[op] \in PendingCommits

AwaitingTicketHasNoPriorReceipt ==
    awaiting # None => <<awaiting, awaitingCommit>> \notin ConsumedPairs

TerminalRejectsLateResults == terminal => awaiting = None

AcceptedBatchHasIdentity ==
    \A key \in Keys: batchAppends[key] = 1 => batchFingerprint[key] # None

BatchIsClosed(key) == batchFingerprint[key] # None /\ batchAppends[key] = 0
ClosedBatchNeverAppends == \A key \in Keys: [](BatchIsClosed(key) => []BatchIsClosed(key))
AcceptedBatchNeverCloses == \A key \in Keys: [](batchAppends[key] = 1 => [](batchAppends[key] = 1))

\* Once either Append or Close binds a key, every retry keeps its identity.
BatchFingerprintNeverChanges ==
    [][\A key \in Keys: (batchFingerprint[key] # None) => (batchFingerprint'[key] = batchFingerprint[key])]_vars

EveryBatchKeyAppendsAtMostOnce ==
    \A key \in Keys: batchAppends[key] <= 1

Spec == Init /\ [][Next]_vars
=============================================================================
