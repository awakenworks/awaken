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
VARIABLES owner, phase, completed, remainingRenewals
vars == <<owner, phase, completed, remainingRenewals>>

ActiveSessions == {session \in Sessions : owner[session] # NoDriver}

Init == /\ owner = [session \in Sessions |-> NoDriver]
        /\ phase = [session \in Sessions |-> "Idle"]
        /\ completed = [session \in Sessions |-> 0]
        /\ remainingRenewals = [session \in Sessions |-> 0]

Acquire(session, d) ==
  /\ session \in Sessions
  /\ d \in Drivers
  /\ owner[session] = NoDriver
  /\ completed[session] < MaxCompleted
  /\ Cardinality(ActiveSessions) < MaxConcurrent
  /\ owner' = [owner EXCEPT ![session] = d]
  /\ phase' = [phase EXCEPT ![session] = "WaitingForControl"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = MaxRenewals]
  /\ UNCHANGED completed

ControlRespond(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ phase' = [phase EXCEPT ![session] = "Driving"]
  /\ UNCHANGED <<owner, completed, remainingRenewals>>

ControlTimeout(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "WaitingForControl"
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]
  /\ UNCHANGED completed

Renew(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Driving"
  /\ remainingRenewals[session] > 0
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = @ - 1]
  /\ UNCHANGED <<owner, phase, completed>>

Advance(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Driving"
  /\ phase' = [phase EXCEPT ![session] = "Committing"]
  /\ UNCHANGED <<owner, completed, remainingRenewals>>

Release(session, d) ==
  /\ session \in Sessions
  /\ owner[session] = d
  /\ phase[session] = "Committing"
  /\ completed[session] < MaxCompleted
  /\ owner' = [owner EXCEPT ![session] = NoDriver]
  /\ phase' = [phase EXCEPT ![session] = "Idle"]
  /\ completed' = [completed EXCEPT ![session] = @ + 1]
  /\ remainingRenewals' = [remainingRenewals EXCEPT ![session] = 0]

ControlSettles(session, d) ==
  ControlRespond(session, d) \/ ControlTimeout(session, d)

Next == \E session \in Sessions, d \in Drivers :
          Acquire(session, d)
          \/ ControlRespond(session, d)
          \/ ControlTimeout(session, d)
          \/ Renew(session, d)
          \/ Advance(session, d)
          \/ Release(session, d)

Fairness ==
  /\ \A session \in Sessions, d \in Drivers : WF_vars(ControlSettles(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Advance(session, d))
  /\ \A session \in Sessions, d \in Drivers : WF_vars(Release(session, d))

Spec == Init /\ [][Next]_vars /\ Fairness

TypeOK == /\ owner \in [Sessions -> Drivers \cup {NoDriver}]
          /\ phase \in [Sessions -> {"Idle", "WaitingForControl", "Driving", "Committing"}]
          /\ completed \in [Sessions -> 0..MaxCompleted]
          /\ remainingRenewals \in [Sessions -> 0..MaxRenewals]
OneDriverPerSession ==
  \A session \in Sessions :
    phase[session] = "Idle" <=> owner[session] = NoDriver
BoundedCrossSessionConcurrency ==
  Cardinality(ActiveSessions) <= MaxConcurrent
Safety == TypeOK /\ OneDriverPerSession /\ BoundedCrossSessionConcurrency

(* A bounded Control request cannot retain scheduler capacity forever. Under
   weak process fairness, each acquired slot either times out or completes its
   phase driver and returns to Idle. *)
EveryAcquiredSlotEventuallyReleases ==
  \A session \in Sessions : [](owner[session] # NoDriver => <>(owner[session] = NoDriver))
=============================================================================
