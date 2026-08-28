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
         accepted, rejected, lastAccepted, lastExpected,
         lastAcceptedCommit, lastExpectedCommit, terminal,
         batchFingerprint, batchAppends, batchConflicts

vars == <<awaiting, awaitingCommit, completed, receiptCall, receiptCommit,
          accepted, rejected, lastAccepted, lastExpected,
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
                    lastAccepted, lastExpected, lastAcceptedCommit,
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
                    receiptCommit, accepted, lastAccepted, lastExpected,
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
                    lastExpected, lastAcceptedCommit, lastExpectedCommit,
                    terminal, batchConflicts>>

ReplayBatch(key, fingerprint) ==
    /\ key \in Keys
    /\ fingerprint \in Fingerprints
    /\ batchFingerprint[key] = fingerprint
    /\ UNCHANGED vars

ConflictBatch(key, fingerprint) ==
    /\ key \in Keys
    /\ fingerprint \in Fingerprints
    /\ batchFingerprint[key] # None
    /\ batchFingerprint[key] # fingerprint
    /\ batchConflicts < MaxConflicts
    /\ batchConflicts' = batchConflicts + 1
    /\ UNCHANGED <<awaiting, awaitingCommit, completed, receiptCall,
                    receiptCommit, accepted, rejected, lastAccepted,
                    lastExpected, lastAcceptedCommit, lastExpectedCommit, terminal,
                    batchFingerprint, batchAppends>>

End == /\ ~terminal
       /\ terminal' = TRUE
       /\ awaiting' = None
       /\ awaitingCommit' = None
       /\ UNCHANGED <<completed, receiptCall, receiptCommit, accepted, rejected,
                       lastAccepted, lastExpected, lastAcceptedCommit,
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
        \/ ConflictBatch(key, fingerprint))
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
    /\ lastAcceptedCommit \in PendingCommits \cup {None}
    /\ lastExpectedCommit \in PendingCommits \cup {None}
    /\ terminal \in BOOLEAN
    /\ batchFingerprint \in [Keys -> Fingerprints \cup {None}]
    /\ batchAppends \in [Keys -> 0..1]
    /\ batchConflicts \in 0..MaxConflicts

OnlyMatchingResultIsAccepted ==
    lastAccepted # None =>
        lastAccepted = lastExpected /\ lastAcceptedCommit = lastExpectedCommit

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

BatchKeyExistsExactlyForOneAppend ==
    \A key \in Keys: (batchFingerprint[key] # None) \equiv (batchAppends[key] = 1)

EveryBatchKeyAppendsAtMostOnce ==
    \A key \in Keys: batchAppends[key] <= 1

Spec == Init /\ [][Next]_vars
=============================================================================
