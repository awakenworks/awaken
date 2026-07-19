---------------------------- MODULE LiveInbox ----------------------------
EXTENDS Naturals, FiniteSets, TLC

CONSTANT MaxId, MaxVersion

VARIABLE queue, retired, nextId, version, closed

vars == <<queue, retired, nextId, version, closed>>

Init == /\ queue = {}
        /\ retired = {}
        /\ nextId = 0
        /\ version = 0
        /\ closed = FALSE

Offer == /\ ~closed
         /\ nextId < MaxId
         /\ version < MaxVersion
         /\ nextId' = nextId + 1
         /\ queue' = queue \cup {nextId + 1}
         /\ version' = version + 1
         /\ UNCHANGED <<retired, closed>>

Wake == /\ ~closed
        /\ version < MaxVersion
        /\ version' = version + 1
        /\ UNCHANGED <<queue, retired, nextId, closed>>

Remove(id) == /\ ~closed
              /\ id \in queue
              /\ version < MaxVersion
              /\ queue' = queue \ {id}
              /\ retired' = retired \cup {id}
              /\ version' = version + 1
              /\ UNCHANGED <<nextId, closed>>

Drain == /\ queue # {}
         /\ retired' = retired \cup queue
         /\ queue' = {}
         /\ UNCHANGED <<nextId, version, closed>>

Close == /\ ~closed
         /\ version < MaxVersion
         /\ closed' = TRUE
         /\ retired' = retired \cup queue
         /\ queue' = {}
         /\ version' = version + 1
         /\ UNCHANGED nextId

BoundedStutter == /\ closed \/ version = MaxVersion \/ nextId = MaxId
                  /\ UNCHANGED vars

Next == Offer \/ Wake \/ (\E id \in 1..MaxId: Remove(id))
     \/ Drain \/ Close \/ BoundedStutter

TypeOK == /\ queue \subseteq 1..MaxId
          /\ retired \subseteq 1..MaxId
          /\ nextId \in 0..MaxId
          /\ version \in 0..MaxVersion
          /\ closed \in BOOLEAN

IdentityPartition == /\ queue \cap retired = {}
                     /\ queue \cup retired = 1..nextId
ClosedIsEmpty == closed => queue = {}
NoFutureIdentityQueued == \A id \in queue: id <= nextId
RetiredNeverQueued == retired \cap queue = {}

Spec == Init /\ [][Next]_vars
=============================================================================
