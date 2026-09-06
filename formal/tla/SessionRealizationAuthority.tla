-------------------- MODULE SessionRealizationAuthority --------------------
EXTENDS Naturals, FiniteSets

(* Complements SessionRealizationMutex's phase/fairness model: this module
   models the slot's exact authority CAS across I/O and async owner-lock waits.
   One Session, two cleanup callers, two Control-admitted generations, three
   expiry versions. All actions are the SAME synchronous slot mutex boundary.
   Worker death, removed/recreated slots, OS execution, physical retirement and
   unbounded generations are outside this model; LocalProcessLifecycle and
   LocalProcessRecovery own execution/receipt composition. No Rust refinement
   or cleanup liveness under unknown physical evidence is asserted. *)
Callers == {1, 2}
Snapshots == (1..2) \X (1..3)
NoSnapshot == <<0, 0>>
VARIABLES projection, lease, request, cleanup, ownerLocked, closedGenerations,
          badGrant, badCleanup, granted, cleaned
vars == <<projection, lease, request, cleanup, ownerLocked, closedGenerations,
          badGrant, badCleanup, granted, cleaned>>

Init == /\ projection = "Live" /\ lease = <<1, 1>> /\ request = NoSnapshot
        /\ cleanup = [c \in Callers |-> NoSnapshot] /\ ownerLocked = TRUE
        /\ closedGenerations = {} /\ badGrant = FALSE /\ badCleanup = FALSE
        /\ granted = FALSE /\ cleaned = FALSE

SendRenewal ==
  /\ projection = "Live" /\ request = NoSnapshot /\ lease[2] < 3
  /\ request' = lease
  /\ UNCHANGED <<projection, lease, cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

ApplyRenewal ==
  /\ request # NoSnapshot
  /\ IF projection = "Live" /\ lease = request
        THEN /\ lease' = <<request[1], request[2] + 1>>
             /\ projection' = "Live"
        ELSE UNCHANGED <<lease, projection>>
  /\ request' = NoSnapshot
  /\ UNCHANGED <<cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

BeginRevoke(c) ==
  /\ cleanup[c] = NoSnapshot
  /\ cleanup' = [cleanup EXCEPT ![c] = lease]
  /\ projection' = "Closed"
  /\ closedGenerations' = closedGenerations \cup {lease[1]}
  /\ UNCHANGED <<lease, request, ownerLocked, badGrant, badCleanup, granted, cleaned>>

(* Cancellation drops the waiter only; it does not restore local authority. *)
CancelCleanup(c) ==
  /\ cleanup[c] # NoSnapshot /\ cleanup' = [cleanup EXCEPT ![c] = NoSnapshot]
  /\ UNCHANGED <<projection, lease, request, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

FinishCleanup(c) ==
  /\ ~ownerLocked /\ cleanup[c] # NoSnapshot
  /\ LET admitted == projection = "Closed" /\ lease = cleanup[c]
     IN /\ badCleanup' = (badCleanup \/ (admitted /\ lease # cleanup[c]))
        /\ cleaned' = (cleaned \/ admitted)
  /\ cleanup' = [cleanup EXCEPT ![c] = NoSnapshot]
  /\ UNCHANGED <<projection, lease, request, ownerLocked, closedGenerations,
                  badGrant, granted>>

(* A successful fresh Control claim installs generation 2. Replaying any
   expiry of the currently closed generation stutters, as does a rejected
   claim. The model does not invent local authority to claim generation 1. *)
Replace ==
  /\ lease[1] = 1 /\ lease' = <<2, 1>> /\ projection' = "Live"
  /\ UNCHANGED <<request, cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

ReleaseOwner ==
  /\ ownerLocked /\ ownerLocked' = FALSE
  /\ UNCHANGED <<projection, lease, request, cleanup, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

Grant ==
  /\ projection = "Live"
  /\ badGrant' = (badGrant \/ (lease[1] \in closedGenerations))
  /\ granted' = TRUE
  /\ UNCHANGED <<projection, lease, request, cleanup, ownerLocked,
                  closedGenerations, badCleanup, cleaned>>

Next == SendRenewal \/ ApplyRenewal \/ Replace \/ ReleaseOwner \/ Grant
        \/ (\E c \in Callers : BeginRevoke(c) \/ CancelCleanup(c) \/ FinishCleanup(c))
Spec == Init /\ [][Next]_vars

TypeOK == /\ projection \in {"Live", "Closed"} /\ lease \in Snapshots
          /\ request \in Snapshots \cup {NoSnapshot}
          /\ cleanup \in [Callers -> Snapshots \cup {NoSnapshot}]
          /\ ownerLocked \in BOOLEAN /\ closedGenerations \subseteq 1..2
          /\ badGrant \in BOOLEAN /\ badCleanup \in BOOLEAN
          /\ granted \in BOOLEAN /\ cleaned \in BOOLEAN
ClosedGenerationCannotRenew == projection = "Live" => lease[1] \notin closedGenerations
NoGrantAfterClose == ~badGrant
OldCleanupCannotTouchReplacement == ~badCleanup
(* Mutation witnesses prevent an inert model from satisfying safety vacuously. *)
NoGrantReached == ~granted
NoCleanupReached == ~cleaned
=============================================================================
