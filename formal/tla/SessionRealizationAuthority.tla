-------------------- MODULE SessionRealizationAuthority --------------------
EXTENDS Naturals, FiniteSets

(* Complements SessionRealizationMutex's phase/fairness model: this module
   models the slot's exact authority CAS across I/O and async owner-lock waits.
   One detailed Session plus one peer, two cleanup callers, two Control-admitted generations, three
   expiry versions. All actions are the SAME synchronous slot mutex boundary.
   Two slot allocations model removal/recreation with an identical CLOSED
   lease. Worker-wide close selects the existing cohort before drain; later
   admission/new Sessions, Worker death, OS execution, physical retirement and
   unbounded generations are outside this model; LocalProcessLifecycle and
   LocalProcessRecovery own execution/receipt composition. No Rust refinement
   or cleanup liveness under unknown physical evidence is asserted. *)
Callers == {1, 2}
Snapshots == (1..2) \X (1..3)
NoSnapshot == <<0, 0>>
VARIABLES projection, lease, request, cleanup, ownerLocked, closedGenerations,
          badGrant, badCleanup, granted, cleaned
VARIABLES slotIdentity, selectedSlot, peerLive, peerCleaned, workerClosed
selectionVars == <<slotIdentity, selectedSlot, peerLive, peerCleaned, workerClosed>>
vars == <<projection, lease, request, cleanup, ownerLocked, closedGenerations,
          badGrant, badCleanup, granted, cleaned, selectionVars>>

Init == /\ projection = "Live" /\ lease = <<1, 1>> /\ request = NoSnapshot
        /\ cleanup = [c \in Callers |-> NoSnapshot] /\ ownerLocked = TRUE
        /\ closedGenerations = {} /\ badGrant = FALSE /\ badCleanup = FALSE
        /\ granted = FALSE /\ cleaned = FALSE
        /\ slotIdentity = 1 /\ selectedSlot = [c \in Callers |-> 0]
        /\ peerLive = TRUE /\ peerCleaned = FALSE /\ workerClosed = FALSE

SendRenewal ==
  /\ UNCHANGED selectionVars
  /\ projection = "Live" /\ request = NoSnapshot /\ lease[2] < 3
  /\ request' = lease
  /\ UNCHANGED <<projection, lease, cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

ApplyRenewal ==
  /\ UNCHANGED selectionVars
  /\ request # NoSnapshot
  /\ IF projection = "Live" /\ lease = request
        THEN /\ lease' = <<request[1], request[2] + 1>>
             /\ projection' = "Live"
        ELSE UNCHANGED <<lease, projection>>
  /\ request' = NoSnapshot
  /\ UNCHANGED <<cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

BeginRevoke(c) ==
  /\ selectedSlot' = [selectedSlot EXCEPT ![c] = slotIdentity]
  /\ UNCHANGED <<slotIdentity, peerLive, peerCleaned, workerClosed>>
  /\ cleanup[c] = NoSnapshot
  /\ cleanup' = [cleanup EXCEPT ![c] = lease]
  /\ projection' = "Closed"
  /\ closedGenerations' = closedGenerations \cup {lease[1]}
  /\ UNCHANGED <<lease, request, ownerLocked, badGrant, badCleanup, granted, cleaned>>

(* Cancellation drops the waiter only; it does not restore local authority. *)
CancelCleanup(c) ==
  /\ UNCHANGED selectionVars
  /\ cleanup[c] # NoSnapshot /\ cleanup' = [cleanup EXCEPT ![c] = NoSnapshot]
  /\ UNCHANGED <<projection, lease, request, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

FinishCleanup(c) ==
  /\ UNCHANGED selectionVars
  /\ ~ownerLocked /\ cleanup[c] # NoSnapshot
  /\ LET admitted == projection = "Closed" /\ lease = cleanup[c]
                      /\ slotIdentity = selectedSlot[c]
     IN /\ badCleanup' = (badCleanup \/ (admitted /\
             (lease # cleanup[c] \/ slotIdentity # selectedSlot[c])))
        /\ cleaned' = (cleaned \/ admitted)
  /\ cleanup' = [cleanup EXCEPT ![c] = NoSnapshot]
  /\ UNCHANGED <<projection, lease, request, ownerLocked, closedGenerations,
                  badGrant, granted>>

(* A successful fresh Control claim installs generation 2. Replaying any
   expiry of the currently closed generation stutters, as does a rejected
   claim. The model does not invent local authority to claim generation 1. *)
Replace ==
  /\ UNCHANGED selectionVars
  /\ lease[1] = 1 /\ lease' = <<2, 1>> /\ projection' = "Live"
  /\ UNCHANGED <<request, cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

ReleaseOwner ==
  /\ UNCHANGED selectionVars
  /\ ownerLocked /\ ownerLocked' = FALSE
  /\ UNCHANGED <<projection, lease, request, cleanup, closedGenerations,
                  badGrant, badCleanup, granted, cleaned>>

Grant ==
  /\ UNCHANGED selectionVars
  /\ projection = "Live"
  /\ badGrant' = (badGrant \/ (lease[1] \in closedGenerations))
  /\ granted' = TRUE
  /\ UNCHANGED <<projection, lease, request, cleanup, ownerLocked,
                  closedGenerations, badCleanup, cleaned>>

(* Same closed lease bytes, but an independently allocated slot/owner lock.
   Old waiters keep their original locks; they never lock the replacement. *)
RecreateClosedSlot ==
  /\ slotIdentity = 1 /\ projection = "Closed" /\ slotIdentity' = 2
  /\ UNCHANGED <<projection, lease, request, cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned, selectedSlot,
                  peerLive, peerCleaned, workerClosed>>

(* The Worker captures all current selections under ONE mutex before any
   drain/async owner-lock wait. Peer cleanup has no dependency on ownerLocked. *)
CloseWorker ==
  /\ ~workerClosed /\ cleanup[1] = NoSnapshot
  /\ workerClosed' = TRUE /\ peerLive' = FALSE
  /\ projection' = "Closed"
  /\ cleanup' = [cleanup EXCEPT ![1] = lease]
  /\ selectedSlot' = [selectedSlot EXCEPT ![1] = slotIdentity]
  /\ closedGenerations' = closedGenerations \cup {lease[1]}
  /\ UNCHANGED <<lease, request, ownerLocked, badGrant, badCleanup, granted, cleaned,
                  slotIdentity, peerCleaned>>

FinishPeer ==
  /\ workerClosed /\ ~peerLive /\ ~peerCleaned
  /\ peerCleaned' = TRUE
  /\ UNCHANGED <<projection, lease, request, cleanup, ownerLocked, closedGenerations,
                  badGrant, badCleanup, granted, cleaned, slotIdentity, selectedSlot,
                  peerLive, workerClosed>>

SessionAndCleanupNext == SendRenewal \/ ApplyRenewal \/ Replace \/ ReleaseOwner \/ Grant
        \/ RecreateClosedSlot \/ FinishPeer
        \/ (\E c \in Callers : BeginRevoke(c) \/ CancelCleanup(c) \/ FinishCleanup(c))
Next == SessionAndCleanupNext \/ CloseWorker
Spec == Init /\ [][Next]_vars /\ WF_vars(FinishPeer)

TypeOK == /\ projection \in {"Live", "Closed"} /\ lease \in Snapshots
          /\ request \in Snapshots \cup {NoSnapshot}
          /\ cleanup \in [Callers -> Snapshots \cup {NoSnapshot}]
          /\ ownerLocked \in BOOLEAN /\ closedGenerations \subseteq 1..2
          /\ badGrant \in BOOLEAN /\ badCleanup \in BOOLEAN
          /\ granted \in BOOLEAN /\ cleaned \in BOOLEAN
          /\ slotIdentity \in 1..2 /\ selectedSlot \in [Callers -> 0..2]
          /\ peerLive \in BOOLEAN /\ peerCleaned \in BOOLEAN /\ workerClosed \in BOOLEAN
ClosedGenerationCannotRenew == projection = "Live" => lease[1] \notin closedGenerations
NoGrantAfterClose == ~badGrant
OldCleanupCannotTouchReplacement == ~badCleanup
WorkerCloseIncludesPeer == workerClosed => ~peerLive
(* The caller continues driving a ready abstract peer under weak fairness.
   Abandoning the entire batch is outside this liveness assumption; executable
   cancellation tests require retained closed owners, not successful cleanup.
   This is independent progress, not a bound on unknown physical cleanup time. *)
PeerEventuallyCleans == workerClosed ~> peerCleaned
(* Mutation witnesses prevent an inert model from satisfying safety vacuously. *)
NoGrantReached == ~granted
NoCleanupReached == ~cleaned
=============================================================================
