---------------------------- MODULE LiveInbox ----------------------------
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANT MaxId, MaxVersion

VARIABLE queue, retired, nextId, version, closed,
         pauseRequested, drained, decision,
         decisionSawPause, decisionHadInput

vars == <<queue, retired, nextId, version, closed,
          pauseRequested, drained, decision,
          decisionSawPause, decisionHadInput>>

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
        /\ closed = FALSE
        /\ pauseRequested = FALSE
        /\ drained = <<>>
        /\ decision = "none"
        /\ decisionSawPause = FALSE
        /\ decisionHadInput = FALSE

Offer == /\ ~closed
         /\ nextId < MaxId
         /\ version < MaxVersion
         /\ nextId' = nextId + 1
         /\ queue' = Append(queue, nextId + 1)
         /\ version' = version + 1
         /\ UNCHANGED <<retired, closed, pauseRequested, drained, decision,
                         decisionSawPause, decisionHadInput>>

Wake == /\ ~closed
        /\ version < MaxVersion
        /\ version' = version + 1
        /\ UNCHANGED <<queue, retired, nextId, closed, pauseRequested, drained,
                        decision, decisionSawPause, decisionHadInput>>

Remove(id) == /\ ~closed
              /\ id \in QueueIds(queue)
              /\ version < MaxVersion
              /\ queue' = SelectSeq(queue, LAMBDA queued: queued # id)
              /\ retired' = retired \cup {id}
              /\ version' = version + 1
              /\ UNCHANGED <<nextId, closed, pauseRequested, drained, decision,
                              decisionSawPause, decisionHadInput>>

Replace(id) == /\ ~closed
               /\ id \in QueueIds(queue)
               /\ version < MaxVersion
               /\ version' = version + 1
               /\ UNCHANGED <<queue, retired, nextId, closed, pauseRequested,
                               drained, decision, decisionSawPause,
                               decisionHadInput>>

\* An accepted reorder is an exact permutation.  Every stale request is an
\* explicit stuttering step: validation happens before the production queue is
\* mutated, so a valid-looking prefix followed by an unknown/duplicate id
\* cannot partially reorder the inbox.
Reorder(candidate) ==
    /\ ~closed
    /\ candidate \in BoundedOrders
    /\ IF ExactPermutation(candidate, queue) /\ version < MaxVersion
          THEN /\ queue' = candidate
               /\ version' = version + 1
               /\ UNCHANGED <<retired, nextId, closed, pauseRequested, drained,
                               decision, decisionSawPause, decisionHadInput>>
          ELSE UNCHANGED vars

RequestPause == /\ ~closed
                /\ ~pauseRequested
                /\ pauseRequested' = TRUE
                /\ UNCHANGED <<queue, retired, nextId, version, closed, drained,
                                decision, decisionSawPause, decisionHadInput>>

\* The production boundary observes pause and the complete queue under one
\* process-local drain. The drained entries are already retired from the
\* editable inbox; only the caller's later ThreadCommit can make their content
\* durable. That best-effort/durable distinction is intentional.
EvaluateBoundary ==
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
    /\ UNCHANGED <<nextId, version, closed, pauseRequested>>

\* Success and caller failure both retire this process-local decision. Durable
\* commit behavior is verified by ThreadCommit; this model deliberately does
\* not turn LiveInbox into a second persistent ingress.
FinishBoundary ==
    /\ decision # "none"
    /\ drained' = <<>>
    /\ decision' = "none"
    /\ UNCHANGED <<queue, retired, nextId, version, closed, pauseRequested,
                    decisionSawPause, decisionHadInput>>

Close == /\ ~closed
         /\ version < MaxVersion
         /\ closed' = TRUE
         /\ retired' = retired \cup QueueIds(queue)
         /\ queue' = <<>>
         /\ version' = version + 1
         /\ UNCHANGED <<nextId, pauseRequested, drained, decision,
                         decisionSawPause, decisionHadInput>>

BoundedStutter == /\ closed \/ version = MaxVersion \/ nextId = MaxId
                  /\ UNCHANGED vars

Next == Offer \/ Wake \/ (\E id \in 1..MaxId: Remove(id) \/ Replace(id))
     \/ (\E candidate \in BoundedOrders: Reorder(candidate))
     \/ RequestPause \/ EvaluateBoundary \/ FinishBoundary \/ Close
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

Spec == Init /\ [][Next]_vars
=============================================================================
