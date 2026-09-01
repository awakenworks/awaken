---------------------------- MODULE LiveInbox ----------------------------
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANT MaxId, MaxVersion, MaxAttempt

VARIABLE queue, retired, nextId, version, closed,
         pauseRequested, drained, decision,
         decisionSawPause, decisionHadInput,
         attemptGeneration, attemptActive, attemptLocal,
         ownershipCurrent, supportsLive

vars == <<queue, retired, nextId, version, closed,
          pauseRequested, drained, decision,
          decisionSawPause, decisionHadInput,
          attemptGeneration, attemptActive, attemptLocal,
          ownershipCurrent, supportsLive>>

InboxOpen == attemptActive /\ attemptLocal /\ ownershipCurrent /\ supportsLive
             /\ ~closed

QueueIds(entries) == {entries[index] : index \in 1..Len(entries)}

BoundedOrders == UNION {[1..length -> 1..MaxId] : length \in 0..MaxId}

ExactPermutation(candidate, current) ==
    /\ Len(candidate) = Len(current)
    /\ QueueIds(candidate) = QueueIds(current)
    /\ Cardinality(QueueIds(candidate)) = Len(candidate)
    /\ Cardinality(QueueIds(current)) = Len(current)

Init == /\ queue = <<>>
        /\ retired = {}
        /\ nextId = 0
        /\ version = 0
        /\ closed = TRUE
        /\ pauseRequested = FALSE
        /\ drained = <<>>
        /\ decision = "none"
        /\ decisionSawPause = FALSE
        /\ decisionHadInput = FALSE
        /\ attemptGeneration = 0
        /\ attemptActive = FALSE
        /\ attemptLocal = FALSE
        /\ ownershipCurrent = FALSE
        /\ supportsLive = FALSE

\* A physical attempt owns the only process-local inbox. Remote executors and
\* executors without a declared safe input boundary still open an attempt
\* scope, but never advertise an inbox.
OpenAttempt(isLocal, canAcceptLive) ==
    /\ ~attemptActive
    /\ attemptGeneration < MaxAttempt
    /\ queue = <<>>
    /\ drained = <<>>
    /\ decision = "none"
    /\ attemptGeneration' = attemptGeneration + 1
    /\ attemptActive' = TRUE
    /\ attemptLocal' = isLocal
    /\ ownershipCurrent' = TRUE
    /\ supportsLive' = canAcceptLive
    /\ closed' = ~(isLocal /\ canAcceptLive)
    /\ pauseRequested' = FALSE
    /\ UNCHANGED <<queue, retired, nextId, version, drained, decision,
                    decisionSawPause, decisionHadInput>>

Offer == /\ InboxOpen
         /\ nextId < MaxId
         /\ version < MaxVersion
         /\ nextId' = nextId + 1
         /\ queue' = Append(queue, nextId + 1)
         /\ version' = version + 1
         /\ UNCHANGED <<retired, closed, pauseRequested, drained, decision,
                         decisionSawPause, decisionHadInput, attemptGeneration,
                         attemptActive, attemptLocal, ownershipCurrent,
                         supportsLive>>

Wake == /\ InboxOpen
        /\ version < MaxVersion
        /\ version' = version + 1
        /\ UNCHANGED <<queue, retired, nextId, closed, pauseRequested, drained,
                        decision, decisionSawPause, decisionHadInput,
                        attemptGeneration, attemptActive, attemptLocal,
                        ownershipCurrent, supportsLive>>

Remove(id) == /\ InboxOpen
              /\ id \in QueueIds(queue)
              /\ version < MaxVersion
              /\ queue' = SelectSeq(queue, LAMBDA queued: queued # id)
              /\ retired' = retired \cup {id}
              /\ version' = version + 1
              /\ UNCHANGED <<nextId, closed, pauseRequested, drained, decision,
                              decisionSawPause, decisionHadInput,
                              attemptGeneration, attemptActive, attemptLocal,
                              ownershipCurrent, supportsLive>>

Replace(id) == /\ InboxOpen
               /\ id \in QueueIds(queue)
               /\ version < MaxVersion
               /\ version' = version + 1
               /\ UNCHANGED <<queue, retired, nextId, closed, pauseRequested,
                               drained, decision, decisionSawPause,
                               decisionHadInput, attemptGeneration,
                               attemptActive, attemptLocal, ownershipCurrent,
                               supportsLive>>

\* An accepted reorder is an exact permutation.  Every stale request is an
\* explicit stuttering step: validation happens before the production queue is
\* mutated, so a valid-looking prefix followed by an unknown/duplicate id
\* cannot partially reorder the inbox.
Reorder(candidate) ==
    /\ InboxOpen
    /\ candidate \in BoundedOrders
    /\ IF ExactPermutation(candidate, queue) /\ version < MaxVersion
          THEN /\ queue' = candidate
               /\ version' = version + 1
               /\ UNCHANGED <<retired, nextId, closed, pauseRequested, drained,
                               decision, decisionSawPause, decisionHadInput,
                               attemptGeneration, attemptActive, attemptLocal,
                               ownershipCurrent, supportsLive>>
          ELSE UNCHANGED vars

RequestPause == /\ InboxOpen
                /\ ~pauseRequested
                /\ pauseRequested' = TRUE
                /\ UNCHANGED <<queue, retired, nextId, version, closed, drained,
                                decision, decisionSawPause, decisionHadInput,
                                attemptGeneration, attemptActive, attemptLocal,
                                ownershipCurrent, supportsLive>>

\* The production boundary observes pause and the complete queue under one
\* process-local drain. The drained entries are already retired from the
\* editable inbox; only the caller's later ThreadCommit can make their content
\* durable. That best-effort/durable distinction is intentional.
EvaluateBoundary ==
    /\ InboxOpen
    /\ decision = "none"
    /\ drained = <<>>
    /\ drained' = queue
    /\ retired' = retired \cup QueueIds(queue)
    /\ queue' = <<>>
    /\ decisionSawPause' = pauseRequested
    /\ decisionHadInput' = (queue # <<>>)
    /\ decision' = IF pauseRequested
                      THEN "await"
                      ELSE IF (queue # <<>>) THEN "continue" ELSE "idle"
    /\ UNCHANGED <<nextId, version, closed, pauseRequested,
                    attemptGeneration, attemptActive, attemptLocal,
                    ownershipCurrent, supportsLive>>

\* Success and caller failure both retire this process-local decision. Durable
\* commit behavior is verified by ThreadCommit; this model deliberately does
\* not turn LiveInbox into a second persistent ingress.
FinishBoundary ==
    /\ decision # "none"
    /\ drained' = <<>>
    /\ decision' = "none"
    /\ UNCHANGED <<queue, retired, nextId, version, closed, pauseRequested,
                    decisionSawPause, decisionHadInput, attemptGeneration,
                    attemptActive, attemptLocal, ownershipCurrent, supportsLive>>

\* Closing an attempt or losing its claim discards every uncommitted live item.
\* The next attempt can open only after this state is empty; no Session-level
\* slot carries the queue across generations.
CloseAttempt == /\ attemptActive
         /\ attemptActive' = FALSE
         /\ attemptLocal' = FALSE
         /\ ownershipCurrent' = FALSE
         /\ supportsLive' = FALSE
         /\ closed' = TRUE
         /\ retired' = retired \cup QueueIds(queue)
         /\ queue' = <<>>
         /\ pauseRequested' = FALSE
         /\ drained' = <<>>
         /\ decision' = "none"
         /\ UNCHANGED <<nextId, version, decisionSawPause,
                         decisionHadInput, attemptGeneration>>

LoseOwnership == /\ attemptActive
                 /\ ownershipCurrent
                 /\ ownershipCurrent' = FALSE
                 /\ closed' = TRUE
                 /\ retired' = retired \cup QueueIds(queue)
                 /\ queue' = <<>>
                 /\ pauseRequested' = FALSE
                 /\ drained' = <<>>
                 /\ decision' = "none"
                 /\ UNCHANGED <<nextId, version, decisionSawPause,
                                 decisionHadInput, attemptGeneration,
                                 attemptActive, attemptLocal, supportsLive>>

BoundedStutter == /\ ~InboxOpen \/ version = MaxVersion \/ nextId = MaxId
                  /\ UNCHANGED vars

Next == (\E isLocal, canAcceptLive \in BOOLEAN:
            OpenAttempt(isLocal, canAcceptLive))
     \/ Offer \/ Wake \/ (\E id \in 1..MaxId: Remove(id) \/ Replace(id))
     \/ (\E candidate \in BoundedOrders: Reorder(candidate))
     \/ RequestPause \/ EvaluateBoundary \/ FinishBoundary
     \/ CloseAttempt \/ LoseOwnership
     \/ BoundedStutter

TypeOK == /\ queue \in BoundedOrders
          /\ retired \subseteq 1..MaxId
          /\ nextId \in 0..MaxId
          /\ version \in 0..MaxVersion
          /\ closed \in BOOLEAN
          /\ pauseRequested \in BOOLEAN
          /\ drained \in BoundedOrders
          /\ decision \in {"none", "continue", "await", "idle"}
          /\ decisionSawPause \in BOOLEAN
          /\ decisionHadInput \in BOOLEAN
          /\ attemptGeneration \in 0..MaxAttempt
          /\ attemptActive \in BOOLEAN
          /\ attemptLocal \in BOOLEAN
          /\ ownershipCurrent \in BOOLEAN
          /\ supportsLive \in BOOLEAN

IdentityPartition == /\ QueueIds(queue) \cap retired = {}
                     /\ QueueIds(queue) \cup retired = 1..nextId
QueueIdentityIsUnique == Cardinality(QueueIds(queue)) = Len(queue)
ClosedIsEmpty == closed => queue = <<>>
NoFutureIdentityQueued == \A id \in QueueIds(queue): id <= nextId
RetiredNeverQueued == retired \cap QueueIds(queue) = {}
DrainedEntriesAreRetired == QueueIds(drained) \subseteq retired
NoInactiveDrain == decision = "none" => drained = <<>>
BoundaryDecisionIsExact ==
    decision # "none" =>
      /\ decisionHadInput = (drained # <<>>)
      /\ decision = IF decisionSawPause
                       THEN "await"
                       ELSE IF decisionHadInput THEN "continue" ELSE "idle"
InboxRequiresExactAttempt ==
    InboxOpen => attemptActive /\ attemptLocal /\ ownershipCurrent /\ supportsLive
ClosedMatchesCapability == closed = ~(
    attemptActive /\ attemptLocal /\ ownershipCurrent /\ supportsLive)
NoQueueOutsideCurrentSupportedLocalAttempt == ~InboxOpen => queue = <<>>
InactiveAttemptIsEmpty == ~attemptActive =>
    /\ queue = <<>>
    /\ drained = <<>>
    /\ decision = "none"

Spec == Init /\ [][Next]_vars
=============================================================================
