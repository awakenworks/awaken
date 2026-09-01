------------------------- MODULE AgentMessageProtocol -------------------------
EXTENDS Naturals, TLC

CONSTANTS Owners, NoOwner

VARIABLES requestDurable, activityOpen, dispatchState, claimOwner,
          targetMessageCommitted, sourceReceiptCommitted, threadClosed,
          previousRequestDurable, previousActivityOpen, previousDispatchState,
          previousClaimOwner, previousTargetMessageCommitted,
          previousSourceReceiptCommitted, previousThreadClosed, lastOutcome

vars == <<requestDurable, activityOpen, dispatchState, claimOwner,
          targetMessageCommitted, sourceReceiptCommitted, threadClosed,
          previousRequestDurable, previousActivityOpen, previousDispatchState,
          previousClaimOwner, previousTargetMessageCommitted,
          previousSourceReceiptCommitted, previousThreadClosed, lastOutcome>>

SnapshotPrevious ==
    /\ previousRequestDurable' = requestDurable
    /\ previousActivityOpen' = activityOpen
    /\ previousDispatchState' = dispatchState
    /\ previousClaimOwner' = claimOwner
    /\ previousTargetMessageCommitted' = targetMessageCommitted
    /\ previousSourceReceiptCommitted' = sourceReceiptCommitted
    /\ previousThreadClosed' = threadClosed

Init ==
    /\ requestDurable = FALSE
    /\ activityOpen = FALSE
    /\ dispatchState = "absent"
    /\ claimOwner = NoOwner
    /\ targetMessageCommitted = FALSE
    /\ sourceReceiptCommitted = FALSE
    /\ threadClosed = FALSE
    /\ previousRequestDurable = FALSE
    /\ previousActivityOpen = FALSE
    /\ previousDispatchState = "absent"
    /\ previousClaimOwner = NoOwner
    /\ previousTargetMessageCommitted = FALSE
    /\ previousSourceReceiptCommitted = FALSE
    /\ previousThreadClosed = FALSE
    /\ lastOutcome = "none"

PersistRequest ==
    /\ ~requestDurable
    /\ SnapshotPrevious
    /\ requestDurable' = TRUE
    /\ lastOutcome' = "request_committed"
    /\ UNCHANGED <<activityOpen, dispatchState, claimOwner,
                    targetMessageCommitted, sourceReceiptCommitted, threadClosed>>

AdmitExact ==
    /\ requestDurable
    /\ SnapshotPrevious
    /\ IF dispatchState = "absent" /\ ~threadClosed
       THEN /\ activityOpen' = TRUE
            /\ dispatchState' = "pending"
            /\ claimOwner' = NoOwner
            /\ lastOutcome' = "admitted"
       ELSE /\ UNCHANGED <<activityOpen, dispatchState, claimOwner>>
            /\ lastOutcome' = IF dispatchState = "absent"
                               THEN "closed_rejection"
                               ELSE "exact_replay"
    /\ UNCHANGED <<requestDurable, targetMessageCommitted,
                    sourceReceiptCommitted, threadClosed>>

AdmitConflict ==
    /\ requestDurable
    /\ SnapshotPrevious
    /\ lastOutcome' = "payload_conflict"
    /\ UNCHANGED <<requestDurable, activityOpen, dispatchState, claimOwner,
                    targetMessageCommitted, sourceReceiptCommitted, threadClosed>>

Claim(owner) ==
    /\ owner \in Owners
    /\ dispatchState = "pending"
    /\ ~threadClosed
    /\ SnapshotPrevious
    /\ dispatchState' = "claimed"
    /\ claimOwner' = owner
    /\ lastOutcome' = "claimed"
    /\ UNCHANGED <<requestDurable, activityOpen, targetMessageCommitted,
                    sourceReceiptCommitted, threadClosed>>

LeaseExpires ==
    /\ dispatchState = "claimed"
    /\ ~targetMessageCommitted
    /\ SnapshotPrevious
    /\ dispatchState' = "pending"
    /\ claimOwner' = NoOwner
    /\ lastOutcome' = "lease_expired"
    /\ UNCHANGED <<requestDurable, activityOpen, targetMessageCommitted,
                    sourceReceiptCommitted, threadClosed>>

CommitTarget(owner) ==
    /\ owner \in Owners
    /\ dispatchState = "claimed"
    /\ claimOwner = owner
    /\ SnapshotPrevious
    /\ targetMessageCommitted' = TRUE
    /\ lastOutcome' = "target_committed"
    /\ UNCHANGED <<requestDurable, activityOpen, dispatchState, claimOwner,
                    sourceReceiptCommitted, threadClosed>>

CommitSourceReceipt ==
    /\ dispatchState # "absent"
    /\ SnapshotPrevious
    /\ sourceReceiptCommitted' = TRUE
    /\ lastOutcome' = "source_receipt_committed"
    /\ UNCHANGED <<requestDurable, activityOpen, dispatchState, claimOwner,
                    targetMessageCommitted, threadClosed>>

SettleDispatch ==
    /\ dispatchState = "claimed"
    /\ targetMessageCommitted
    /\ SnapshotPrevious
    /\ dispatchState' = "done"
    /\ claimOwner' = NoOwner
    /\ activityOpen' = FALSE
    /\ lastOutcome' = "settled"
    /\ UNCHANGED <<requestDurable, targetMessageCommitted,
                    sourceReceiptCommitted, threadClosed>>

CloseThread ==
    /\ ~threadClosed
    /\ SnapshotPrevious
    /\ threadClosed' = TRUE
    /\ IF dispatchState \in {"pending", "claimed"}
       THEN /\ dispatchState' = "cancelled"
            /\ claimOwner' = NoOwner
            /\ activityOpen' = FALSE
       ELSE /\ UNCHANGED <<dispatchState, claimOwner, activityOpen>>
    /\ lastOutcome' = "thread_closed"
    /\ UNCHANGED <<requestDurable, targetMessageCommitted,
                    sourceReceiptCommitted>>

Next ==
    \/ PersistRequest
    \/ AdmitExact
    \/ AdmitConflict
    \/ \E owner \in Owners: Claim(owner)
    \/ LeaseExpires
    \/ \E owner \in Owners: CommitTarget(owner)
    \/ CommitSourceReceipt
    \/ SettleDispatch
    \/ CloseThread

TypeOK ==
    /\ requestDurable \in BOOLEAN
    /\ activityOpen \in BOOLEAN
    /\ dispatchState \in {"absent", "pending", "claimed", "done", "cancelled"}
    /\ claimOwner \in Owners \cup {NoOwner}
    /\ targetMessageCommitted \in BOOLEAN
    /\ sourceReceiptCommitted \in BOOLEAN
    /\ threadClosed \in BOOLEAN
    /\ previousRequestDurable \in BOOLEAN
    /\ previousActivityOpen \in BOOLEAN
    /\ previousDispatchState \in {"absent", "pending", "claimed", "done", "cancelled"}
    /\ previousClaimOwner \in Owners \cup {NoOwner}
    /\ previousTargetMessageCommitted \in BOOLEAN
    /\ previousSourceReceiptCommitted \in BOOLEAN
    /\ previousThreadClosed \in BOOLEAN
    /\ lastOutcome \in {"none", "request_committed", "admitted",
                         "closed_rejection", "exact_replay", "payload_conflict",
                         "claimed", "lease_expired", "target_committed",
                         "source_receipt_committed", "settled", "thread_closed"}

RequestBeforeDispatch == dispatchState # "absent" => requestDurable
ReceiptImpliesDurableDispatch == sourceReceiptCommitted => dispatchState # "absent"
TargetCommitRequiresDurableRequest == targetMessageCommitted => requestDurable
OneLiveClaim == (dispatchState = "claimed") = (claimOwner \in Owners)
ActivityNotSettledEarly == dispatchState \in {"pending", "claimed"} => activityOpen
ClosedThreadHasNoRunnableDispatch ==
    threadClosed => dispatchState \notin {"pending", "claimed"}
ExactReplayStutters ==
    lastOutcome = "exact_replay"
      => /\ requestDurable = previousRequestDurable
         /\ activityOpen = previousActivityOpen
         /\ dispatchState = previousDispatchState
         /\ claimOwner = previousClaimOwner
         /\ targetMessageCommitted = previousTargetMessageCommitted
         /\ sourceReceiptCommitted = previousSourceReceiptCommitted
         /\ threadClosed = previousThreadClosed
PayloadConflictStutters ==
    lastOutcome = "payload_conflict"
      => /\ requestDurable = previousRequestDurable
         /\ activityOpen = previousActivityOpen
         /\ dispatchState = previousDispatchState
         /\ claimOwner = previousClaimOwner
         /\ targetMessageCommitted = previousTargetMessageCommitted
         /\ sourceReceiptCommitted = previousSourceReceiptCommitted
         /\ threadClosed = previousThreadClosed

Safety ==
    /\ TypeOK
    /\ RequestBeforeDispatch
    /\ ReceiptImpliesDurableDispatch
    /\ TargetCommitRequiresDurableRequest
    /\ OneLiveClaim
    /\ ActivityNotSettledEarly
    /\ ClosedThreadHasNoRunnableDispatch
    /\ ExactReplayStutters
    /\ PayloadConflictStutters

Spec == Init /\ [][Next]_vars
=============================================================================
