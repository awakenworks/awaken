------------------------- MODULE SessionRealizationMutex -------------------------
EXTENDS Naturals, FiniteSets

CONSTANTS DriverA, DriverB, NoDriver, Sessions, MaxCompleted, MaxConcurrent, MaxRenewals
ASSUME /\ DriverA # DriverB
       /\ NoDriver \notin {DriverA, DriverB}
       /\ Sessions # {}
       /\ IsFiniteSet(Sessions)
       /\ MaxCompleted \in Nat \ {0}
       /\ MaxConcurrent \in Nat \ {0}
       /\ MaxConcurrent <= Cardinality(Sessions)
       /\ MaxRenewals \in Nat

Drivers == {DriverA, DriverB}
VARIABLES owner, phase, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet
vars == <<owner, phase, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet>>

ActiveSessions == {session \in Sessions : owner[session] # NoDriver}

Init == /\ owner = [session \in Sessions |-> NoDriver]
        /\ phase = [session \in Sessions |-> "Idle"]
        /\ completed = [session \in Sessions |-> 0]
        /\ remainingRenewals = [session \in Sessions |-> 0]
        /\ leaseVisible = [session \in Sessions |-> FALSE]
        /\ receiptCurrent = [session \in Sessions |-> FALSE]
        /\ publishSet = [session \in Sessions |-> {}]
        /\ drainSet = [session \in Sessions |-> {}]

Acquire(session, d) ==
  /\ session \in Sessions
  /\ d \in Drivers
  /\ owner[session] = NoDriver
  /\ completed[session] < MaxCompleted
  /\ Cardinality(ActiveSessions) < MaxConcurrent
  /\ owner' = [owner EXCEPT ![session] = d]
  /\ phase' = [phase EXCEPT ![session] = "WaitingForControl"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = MaxRenewals]
  /\ UNCHANGED <<completed, leaseVisible, receiptCurrent, publishSet, drainSet>>

ControlRespond(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ phase' = [phase EXCEPT ![session] = "PreparingResources"]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = TRUE]
  /\ UNCHANGED <<owner, completed, remainingRenewals, receiptCurrent, publishSet, drainSet>>

ControlTimeout(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ UNCHANGED <<completed, leaseVisible, receiptCurrent, publishSet, drainSet>>

(* Reads hold the existing driver lock but not renewal capacity. A temporary
   failure is stuttering; explicit loss/expiry cancels before lock-wait cleanup.
   Owner/epoch/expiry CAS arithmetic is proved by the contract kernels, not by
   this scheduling abstraction. *)
PrepareResources(session, d) ==
  /\ owner[session] = d
  /\ phase[session] = "PreparingResources"
  /\ leaseVisible[session]
  /\ phase' = [phase EXCEPT ![session] = "Driving"]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet>>

LosePreparationAuthority(session, d) ==
  /\ owner[session] = d
  /\ phase[session] = "PreparingResources"
  /\ phase' = [phase EXCEPT ![session] = "CancellingResources"]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<owner, completed, remainingRenewals, receiptCurrent, publishSet, drainSet>>

AbortPreparation(session, d) ==
  /\ owner[session] = d
  /\ phase[session] \in {"PreparingResources", "CancellingResources"}
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<completed, receiptCurrent, publishSet, drainSet>>

Renew(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] \in {"PreparingResources", "Driving", "Publishing"}
  /\ leaseVisible[session]
  /\ MaxRenewals > 0
  (* Cyclic finite abstraction permits infinitely many successful renewals;
     it never assumes that heartbeats eventually stop. receiptCurrent records
     only equality versus an authorized older expiry, not epoch arithmetic. *)
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = IF @ = 0 THEN MaxRenewals ELSE @ - 1]
  /\ receiptCurrent' = [receiptCurrent EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<owner, phase, completed, leaseVisible, publishSet, drainSet>>

PublicationCandidates(staged, active) == staged \cap active

PublicationObservation(staged, active, published, drained) ==
    /\ published = PublicationCandidates(staged, active)
    /\ published \cap drained = {}

Advance(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Driving"
  /\ phase' = [phase EXCEPT ![session] = "Publishing"]
  /\ receiptCurrent' = [receiptCurrent EXCEPT ![session] = TRUE]
  \* The regression witness: Stage returned both old and new generations.
  \* Activate durably retires old; Publish partitions those exact receipts.
  /\ publishSet' = [publishSet EXCEPT ![session] = PublicationCandidates({"old", "new"}, {"new"})]
  /\ drainSet' = [drainSet EXCEPT ![session] = {"old"}]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible>>

(* Completed physical evidence remains valid under live same-generation
   authority. Renewal does not send this phase back to Driving. *)
Acknowledge(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Publishing"
  /\ phase' = [phase EXCEPT ![session] = "Committing"]
  /\ UNCHANGED <<owner, completed, remainingRenewals, leaseVisible, receiptCurrent, publishSet, drainSet>>

Release(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Committing"
  /\ completed[session] < MaxCompleted
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ completed' = [completed EXCEPT ![session] = @ + 1]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ leaseVisible' = [leaseVisible EXCEPT ![session] = FALSE]
  /\ UNCHANGED <<receiptCurrent, publishSet, drainSet>>

ControlSettles(session, d) ==
  ControlRespond(session, d) \/ ControlTimeout(session, d)

Next == \E session \in Sessions, d \in Drivers :
          Acquire(session, d)
          \/ ControlRespond(session, d)
          \/ ControlTimeout(session, d)
          \/ PrepareResources(session, d)
          \/ LosePreparationAuthority(session, d)
          \/ AbortPreparation(session, d)
          \/ Renew(session, d)
          \/ Advance(session, d)
          \/ Acknowledge(session, d)
          \/ Release(session, d)

Fairness ==
  /\ \A session \in Sessions, d \in Drivers : WF_vars(ControlSettles(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(PrepareResources(session, d) \/ AbortPreparation(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Advance(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Acknowledge(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Release(session, d))

Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ owner \in [Sessions -> Drivers \cup {NoDriver}]
          /\ phase \in [Sessions -> {"Idle", "WaitingForControl", "PreparingResources", "CancellingResources", "Driving", "Publishing", "Committing"}]
          /\ completed \in [Sessions -> 0..MaxCompleted]
          /\ remainingRenewals \in [Sessions -> 0..MaxRenewals]
          /\ leaseVisible \in [Sessions -> BOOLEAN]
          /\ receiptCurrent \in [Sessions -> BOOLEAN]
OneDriverPerSession ==
  \A session \in Sessions :
    phase[session] = "Idle" <=> owner[session] = NoDriver
BoundedCrossSessionConcurrency ==
  Cardinality(ActiveSessions) <= MaxConcurrent
PublicationExcludesRetiredGenerations ==
  \A session \in Sessions :
    /\ publishSet[session] \subseteq {"new"}
    /\ publishSet[session] \cap drainSet[session] = {}
    /\ (phase[session] \in {"Publishing", "Committing"} => publishSet[session] = {"new"})
Safety == TypeOK /\ OneDriverPerSession /\ BoundedCrossSessionConcurrency
          /\ PublicationExcludesRetiredGenerations

ResourcesHaveRenewableAuthority ==
  \A session \in Sessions :
    (phase[session] \in {"PreparingResources", "Driving", "Publishing", "Committing"}) = leaseVisible[session]

(* A bounded Control request cannot retain scheduler capacity forever. Under
   weak process fairness, each acquired slot either times out or completes its
   phase driver and returns to Idle. *)
EveryAcquiredSlotEventuallyReleases ==
  \A session \in Sessions : [](owner[session] # NoDriver => <>(owner[session] = NoDriver))
=============================================================================
