---------------------------- MODULE LiveInbox ----------------------------
EXTENDS Naturals, FiniteSets, Sequences, TLC

CONSTANT MaxId, MaxVersion

VARIABLE queue, retired, nextId, version, closed

vars == <<queue, retired, nextId, version, closed>>

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

Offer == /\ ~closed
         /\ nextId < MaxId
         /\ version < MaxVersion
         /\ nextId' = nextId + 1
         /\ queue' = Append(queue, nextId + 1)
         /\ version' = version + 1
         /\ UNCHANGED <<retired, closed>>

Wake == /\ ~closed
        /\ version < MaxVersion
        /\ version' = version + 1
        /\ UNCHANGED <<queue, retired, nextId, closed>>

Remove(id) == /\ ~closed
              /\ id \in QueueIds(queue)
              /\ version < MaxVersion
              /\ queue' = SelectSeq(queue, LAMBDA queued: queued # id)
              /\ retired' = retired \cup {id}
              /\ version' = version + 1
              /\ UNCHANGED <<nextId, closed>>

Replace(id) == /\ ~closed
               /\ id \in QueueIds(queue)
               /\ version < MaxVersion
               /\ version' = version + 1
               /\ UNCHANGED <<queue, retired, nextId, closed>>

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
               /\ UNCHANGED <<retired, nextId, closed>>
          ELSE UNCHANGED vars

Drain == /\ queue # <<>>
         /\ retired' = retired \cup QueueIds(queue)
         /\ queue' = <<>>
         /\ UNCHANGED <<nextId, version, closed>>

Close == /\ ~closed
         /\ version < MaxVersion
         /\ closed' = TRUE
         /\ retired' = retired \cup QueueIds(queue)
         /\ queue' = <<>>
         /\ version' = version + 1
         /\ UNCHANGED nextId

BoundedStutter == /\ closed \/ version = MaxVersion \/ nextId = MaxId
                  /\ UNCHANGED vars

Next == Offer \/ Wake \/ (\E id \in 1..MaxId: Remove(id) \/ Replace(id))
     \/ (\E candidate \in BoundedOrders: Reorder(candidate))
     \/ Drain \/ Close \/ BoundedStutter

TypeOK == /\ queue \in BoundedOrders
          /\ retired \subseteq 1..MaxId
          /\ nextId \in 0..MaxId
          /\ version \in 0..MaxVersion
          /\ closed \in BOOLEAN

IdentityPartition == /\ QueueIds(queue) \cap retired = {}
                     /\ QueueIds(queue) \cup retired = 1..nextId
QueueIdentityIsUnique == Cardinality(QueueIds(queue)) = Len(queue)
ClosedIsEmpty == closed => queue = <<>>
NoFutureIdentityQueued == \A id \in QueueIds(queue): id <= nextId
RetiredNeverQueued == retired \cap QueueIds(queue) = {}

Spec == Init /\ [][Next]_vars
=============================================================================
