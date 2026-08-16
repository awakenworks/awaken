---------------------- MODULE RolloutEventIdentity ----------------------
EXTENDS TLC

\* Two different payloads deliberately share one stable event id. This small
\* model covers the outbox repository decision only; transport and Session
\* adoption remain in ManagedCredentialRollout.
ExactEvent == [id |-> "event-1", payload |-> "exact"]
CollisionEvent == [id |-> "event-1", payload |-> "collision"]
NoEvent == [id |-> "none", payload |-> "none"]
Events == {ExactEvent, CollisionEvent}

VARIABLE pairCommitted, durableEvent, acknowledged,
         exactReplaySeen, collisionRejected, staleAckRejected

vars == <<pairCommitted, durableEvent, acknowledged,
          exactReplaySeen, collisionRejected, staleAckRejected>>

Init == /\ pairCommitted = FALSE /\ durableEvent = NoEvent
        /\ acknowledged = FALSE /\ exactReplaySeen = FALSE
        /\ collisionRejected = FALSE /\ staleAckRejected = FALSE

CommitExact ==
    /\ durableEvent = NoEvent /\ ~acknowledged
    /\ pairCommitted' = TRUE /\ durableEvent' = ExactEvent
    /\ UNCHANGED <<acknowledged, exactReplaySeen, collisionRejected,
                    staleAckRejected>>

ReplayExact ==
    /\ durableEvent = ExactEvent
    /\ exactReplaySeen' = TRUE
    /\ UNCHANGED <<pairCommitted, durableEvent, acknowledged,
                    collisionRejected, staleAckRejected>>

RejectCollidingInsert ==
    /\ durableEvent = ExactEvent /\ CollisionEvent.id = durableEvent.id
    /\ collisionRejected' = TRUE
    /\ UNCHANGED <<pairCommitted, durableEvent, acknowledged,
                    exactReplaySeen, staleAckRejected>>

RejectCollidingAck ==
    /\ durableEvent = ExactEvent /\ CollisionEvent.id = durableEvent.id
    /\ staleAckRejected' = TRUE
    /\ UNCHANGED <<pairCommitted, durableEvent, acknowledged,
                    exactReplaySeen, collisionRejected>>

AckExact ==
    /\ durableEvent = ExactEvent
    /\ acknowledged' = TRUE /\ durableEvent' = NoEvent
    /\ UNCHANGED <<pairCommitted, exactReplaySeen, collisionRejected,
                    staleAckRejected>>

Quiesce == /\ acknowledged /\ UNCHANGED vars

Next == CommitExact \/ ReplayExact \/ RejectCollidingInsert
        \/ RejectCollidingAck \/ AckExact \/ Quiesce

TypeOK == /\ pairCommitted \in BOOLEAN
          /\ durableEvent \in Events \cup {NoEvent}
          /\ acknowledged \in BOOLEAN /\ exactReplaySeen \in BOOLEAN
          /\ collisionRejected \in BOOLEAN /\ staleAckRejected \in BOOLEAN
OutboxRequiresCommittedPair == durableEvent # NoEvent => pairCommitted
CollisionNeverReplacesDurableEvent ==
    collisionRejected /\ ~acknowledged => durableEvent = ExactEvent
CollidingAckNeverDeletesDurableEvent ==
    staleAckRejected /\ ~acknowledged => durableEvent = ExactEvent
AcknowledgementRequiresExactRemoval == acknowledged => durableEvent = NoEvent

Spec == Init /\ [][Next]_vars
=========================================================================
